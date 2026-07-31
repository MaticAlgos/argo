//! Process execution.
//!
//! Spawns one child per turn, feeds it the composed prompt, drives the adapter's
//! parser over its output, and guarantees the process is reaped. Everything
//! agent-specific comes from the [`RuntimeDef`]; this module is shared.
//!
//! Cancellation is the subtle part. A coding agent spawns its own children
//! (compilers, test runners, language servers), so killing just the direct child
//! would orphan those. On Unix the child is therefore made a process-group leader
//! and the whole group is signalled, escalating from `SIGTERM` to `SIGKILL`.

use crate::def::{InvocationContext, RuntimeDef};
use crate::stream::{
    acp::{AcpAction, AcpSession},
    antigravity::AntigravityStreamParser,
    claude::ClaudeStreamParser,
    codex::CodexStreamParser,
    plain::PlainStreamParser,
    StreamSink, TerminalOutcome,
};
use argo_core::error::{ArgoError, Result};
use argo_core::runtime::{PromptDelivery, PromptEncoding, StreamFormat};
use argo_core::RunStatus;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Notify;

/// How long to spend collecting an ACP agent's stderr once its turn is over.
///
/// Only diagnostics live there, so a bound is safe — and necessary, because the
/// agent is a long-lived server whose stderr never closes on its own.
const ACP_STDERR_DRAIN_MS: u64 = 300;

/// Grace period between `SIGTERM` and `SIGKILL`.
const TERM_GRACE_MS: u64 = 2_000;

/// Cooperative cancellation handle for a running turn.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    notify: Arc<Notify>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl CancelToken {
    /// Creates a token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// True once cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Resolves when cancellation is requested.
    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.notify.notified().await;
    }
}

/// What one turn needs in order to run.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// Resolved executable path or name.
    pub bin: String,
    /// Composed body sent to the agent.
    pub prompt: String,
    /// Adapter invocation inputs.
    pub context: InvocationContext,
    /// Environment overrides applied to the child.
    pub env: Vec<(String, String)>,
    /// MCP server descriptors, for protocol adapters that pass them inline.
    pub mcp_servers: Vec<serde_json::Value>,
    /// Hard ceiling on the turn, independent of user cancellation.
    pub timeout_ms: Option<u64>,
}

/// Result of one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    /// Terminal outcome derived from the stream and exit status.
    pub outcome: TerminalOutcome,
    /// Upstream session handle observed during the turn, when any.
    pub session_id: Option<String>,
}

/// Runs one turn.
pub async fn execute(
    def: &RuntimeDef,
    request: ExecRequest,
    cancel: &CancelToken,
    sink: &mut dyn StreamSink,
) -> Result<ExecOutcome> {
    let started_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let args = def.args_for(&request.context);
    let mut command = Command::new(&request.bin);
    command
        .args(&args)
        .current_dir(&request.context.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    for (key, value) in &request.env {
        command.env(key, value);
    }

    // Own the whole process tree so cancellation cannot leave orphans behind.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            // New process group with this child as leader.
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command
        .spawn()
        .map_err(|e| ArgoError::Process(format!("spawn {}: {e}", request.bin)))?;

    let pid = child.id();

    let mut result = match def.capabilities.stream_format {
        StreamFormat::AcpJsonRpc => drive_acp(&mut child, &request, cancel, sink).await,
        format => {
            drive_line_stream(
                format,
                def.capabilities.prompt_encoding,
                &mut child,
                &request,
                cancel,
                sink,
            )
            .await
        }
    };

    // Whatever happened above, the process must not survive this function.
    terminate(&mut child, pid).await;

    if let (Ok(outcome), Some(capture)) = (&mut result, def.capture_session) {
        if outcome.outcome.status == RunStatus::Succeeded && outcome.session_id.is_none() {
            outcome.session_id = capture(&request.context, started_at_ms);
        }
    }

    result
}

/// Frames the composed body for the wire.
///
/// A CLI reading stdin as a protocol stream rejects bare text, so the framing is
/// applied here from the adapter's declared encoding.
fn frame_prompt(body: &str, encoding: PromptEncoding) -> String {
    match encoding {
        PromptEncoding::Raw => body.to_string(),
        PromptEncoding::StreamJsonUserMessage => serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": body }]
            }
        })
        .to_string(),
    }
}

/// Writes the prompt to stdin when the adapter expects it there, then closes it.
async fn deliver_prompt(
    child: &mut Child,
    request: &ExecRequest,
    delivery: PromptDelivery,
    encoding: PromptEncoding,
) {
    if delivery != PromptDelivery::Stdin {
        // File and protocol delivery are handled by the caller and the driver.
        if let Some(stdin) = child.stdin.take() {
            drop(stdin);
        }
        return;
    }
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    // A broken pipe here means the CLI exited early; the stream and exit status
    // will explain why, so this failure is not surfaced separately.
    let framed = frame_prompt(&request.prompt, encoding);
    let _ = stdin.write_all(framed.as_bytes()).await;
    let _ = stdin.write_all(b"\n").await;
    let _ = stdin.flush().await;
    drop(stdin);
}

/// Drives a line-oriented stream: Claude, Codex, or plain text.
async fn drive_line_stream(
    format: StreamFormat,
    encoding: PromptEncoding,
    child: &mut Child,
    request: &ExecRequest,
    cancel: &CancelToken,
    sink: &mut dyn StreamSink,
) -> Result<ExecOutcome> {
    deliver_prompt(child, request, request.context_delivery(), encoding).await;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ArgoError::Process("child has no stdout".into()))?;
    let stderr = child.stderr.take();

    let mut reader = BufReader::new(stdout).lines();
    let mut claude = ClaudeStreamParser::new();
    let mut antigravity = AntigravityStreamParser::new();
    let mut codex = CodexStreamParser::new();
    let mut plain = PlainStreamParser::new();
    let mut session_id: Option<String> = None;

    let deadline = request
        .timeout_ms
        .map(|ms| tokio::time::Instant::now() + std::time::Duration::from_millis(ms));

    loop {
        let next_line = reader.next_line();
        let line = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Ok(ExecOutcome {
                    outcome: TerminalOutcome {
                        status: argo_core::event::RunStatus::Cancelled,
                        usage: Default::default(),
                        resume_target_missing: false,
                        message: None,
                    },
                    session_id,
                });
            }
            _ = sleep_until(deadline) => {
                return Err(ArgoError::Timeout(request.timeout_ms.unwrap_or(0)));
            }
            line = next_line => line.map_err(|e| ArgoError::Process(format!("read stdout: {e}")))?,
        };

        let Some(line) = line else { break };

        let mut capture = SessionCapturingSink {
            inner: sink,
            session_id: &mut session_id,
        };
        match format {
            StreamFormat::ClaudeStreamJson => claude.push_line(&line, &mut capture),
            StreamFormat::AntigravityStreamJson => antigravity.push_line(&line, &mut capture),
            StreamFormat::JsonEventStream => codex.push_line(&line, &mut capture),
            StreamFormat::Plain => plain.push_line(&line, &mut capture),
            StreamFormat::AcpJsonRpc => unreachable!("handled by drive_acp"),
        }
    }

    let stderr_text = read_stderr(stderr).await;
    let status = child
        .wait()
        .await
        .map_err(|e| ArgoError::Process(format!("wait: {e}")))?;

    let outcome = match format {
        StreamFormat::ClaudeStreamJson => claude
            .outcome()
            .cloned()
            .unwrap_or_else(|| outcome_from_exit(status.success(), &stderr_text)),
        StreamFormat::AntigravityStreamJson => antigravity
            .outcome()
            .cloned()
            .unwrap_or_else(|| outcome_from_exit(status.success(), &stderr_text)),
        StreamFormat::JsonEventStream => codex
            .outcome()
            .cloned()
            .unwrap_or_else(|| outcome_from_exit(status.success(), &stderr_text)),
        // Plain adapters have no terminal record in the stream, so the exit status
        // and the presence of output are the only signals available.
        StreamFormat::Plain => plain.finish(status.success(), &stderr_text),
        StreamFormat::AcpJsonRpc => unreachable!(),
    };

    Ok(ExecOutcome {
        outcome,
        session_id,
    })
}

/// Drives an ACP JSON-RPC exchange over the child's stdio.
async fn drive_acp(
    child: &mut Child,
    request: &ExecRequest,
    cancel: &CancelToken,
    sink: &mut dyn StreamSink,
) -> Result<ExecOutcome> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ArgoError::Process("child has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ArgoError::Process("child has no stdout".into()))?;
    let stderr = child.stderr.take();

    let mut session = AcpSession::new(
        request.context.cwd.clone(),
        request.prompt.clone(),
        request.context.resume_session.clone(),
        request.context.concrete_model().map(|m| m.to_string()),
        request.mcp_servers.clone(),
    );

    // Kick off the handshake.
    write_action(&mut stdin, session.start()).await?;

    let mut reader = BufReader::new(stdout).lines();
    let deadline = request
        .timeout_ms
        .map(|ms| tokio::time::Instant::now() + std::time::Duration::from_millis(ms));

    loop {
        let next_line = reader.next_line();
        let line = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Ask the agent to stop cleanly; the caller then reaps the process.
                if let Some(frame) = session.cancel() {
                    let _ = stdin.write_all(frame.json.as_bytes()).await;
                    let _ = stdin.write_all(b"\n").await;
                    let _ = stdin.flush().await;
                }
                return Ok(ExecOutcome {
                    outcome: TerminalOutcome {
                        status: argo_core::event::RunStatus::Cancelled,
                        usage: Default::default(),
                        resume_target_missing: false,
                        message: None,
                    },
                    session_id: session.session_id().map(|s| s.to_string()),
                });
            }
            _ = sleep_until(deadline) => {
                return Err(ArgoError::Timeout(request.timeout_ms.unwrap_or(0)));
            }
            line = next_line => line.map_err(|e| ArgoError::Process(format!("read stdout: {e}")))?,
        };

        let Some(line) = line else { break };

        let action = session.handle_line(&line, sink);
        write_action(&mut stdin, action).await?;

        if session.is_done() {
            break;
        }
    }

    // An ACP agent is a persistent server: it answers `session/prompt` and then
    // keeps running, waiting for the next one. Its stdout and stderr therefore
    // never reach EOF, so waiting for them would hang the turn *after* it had
    // already succeeded. End the process first, then drain stderr under a bound.
    if session.is_done() {
        let _ = child.start_kill();
    }
    let stderr_text = read_stderr_bounded(stderr, ACP_STDERR_DRAIN_MS).await;
    let session_id = session.session_id().map(|s| s.to_string());
    let outcome = session.outcome().cloned().unwrap_or_else(|| {
        // The transport closed without a prompt response: the agent died mid-turn.
        TerminalOutcome::failed(if stderr_text.trim().is_empty() {
            "the ACP agent exited before completing the turn".to_string()
        } else {
            crate::stream::truncate(stderr_text.trim(), 500)
        })
    });

    Ok(ExecOutcome {
        outcome,
        session_id,
    })
}

/// Reads stderr, giving up after `budget_ms` rather than waiting for EOF.
///
/// Returns whatever arrived in time; a truncated diagnostic is strictly better
/// than a turn that never finishes.
async fn read_stderr_bounded(
    stderr: Option<tokio::process::ChildStderr>,
    budget_ms: u64,
) -> String {
    tokio::time::timeout(
        std::time::Duration::from_millis(budget_ms),
        read_stderr(stderr),
    )
    .await
    .unwrap_or_default()
}

async fn write_action(stdin: &mut tokio::process::ChildStdin, action: AcpAction) -> Result<()> {
    if let AcpAction::Send(message) = action {
        stdin
            .write_all(message.json.as_bytes())
            .await
            .map_err(|e| ArgoError::Process(format!("write acp frame: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| ArgoError::Process(format!("write acp newline: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| ArgoError::Process(format!("flush acp frame: {e}")))?;
    }
    Ok(())
}

/// Sleeps until `deadline`, or forever when there is none.
async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

/// Drains stderr without letting a chatty CLI grow unbounded.
async fn read_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    const MAX: usize = 64 * 1024;
    let Some(stderr) = stderr else {
        return String::new();
    };
    let mut lines = BufReader::new(stderr).lines();
    let mut out = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        if out.len() >= MAX {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Fallback outcome when a structured stream ended without a terminal record.
fn outcome_from_exit(success: bool, stderr: &str) -> TerminalOutcome {
    if success {
        // Exited cleanly but never reported a result: treat as success with a
        // note rather than inventing a failure.
        TerminalOutcome::succeeded()
    } else {
        TerminalOutcome::failed(if stderr.trim().is_empty() {
            "the agent exited with a non-zero status and no diagnostics".to_string()
        } else {
            crate::stream::truncate(stderr.trim(), 500)
        })
    }
}

/// Terminates the child and its process group, escalating if needed.
async fn terminate(child: &mut Child, pid: Option<u32>) {
    // Already gone?
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }

    #[cfg(unix)]
    if let Some(pid) = pid {
        // Negative pid targets the whole group, so the agent's own children
        // (compilers, test runners) are cleaned up too.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        let grace = tokio::time::timeout(
            std::time::Duration::from_millis(TERM_GRACE_MS),
            child.wait(),
        )
        .await;
        if grace.is_ok() {
            return;
        }
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }

    let _ = child.start_kill();
    let _ = child.wait().await;
}

impl ExecRequest {
    /// Prompt delivery implied by the staged file's presence.
    ///
    /// A file-delivery adapter has already had its prompt written to disk, so
    /// stdin must be closed rather than fed.
    fn context_delivery(&self) -> PromptDelivery {
        if self.context.prompt_file.is_some() {
            PromptDelivery::File
        } else {
            PromptDelivery::Stdin
        }
    }
}

/// Wraps a sink to record any session handle the stream discloses.
struct SessionCapturingSink<'a> {
    inner: &'a mut dyn StreamSink,
    session_id: &'a mut Option<String>,
}

impl StreamSink for SessionCapturingSink<'_> {
    fn emit(&mut self, event: argo_core::event::RunEventKind) {
        if let argo_core::event::RunEventKind::SessionCaptured { session_id } = &event {
            *self.session_id = Some(session_id.to_string());
        }
        self.inner.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectingSink;
    use argo_core::event::{RunEventKind, RunStatus};
    use argo_core::runtime::{
        AgentCapabilities, McpInjection, PermissionPosture, PromptDelivery, PromptEncoding,
        StreamFormat,
    };

    /// Builds a def that runs an arbitrary shell script with a chosen format.
    fn script_def(format: StreamFormat, delivery: PromptDelivery) -> RuntimeDef {
        RuntimeDef {
            id: "test-script",
            name: "Test Script",
            bin: "sh",
            fallback_bins: &[],
            version_args: &["-c", "echo 0"],
            help_args: &["-c", "echo"],
            model_probe: None,
            fallback_models: &[("default", "default")],
            reasoning_options: &[],
            auth_probe: None,
            // The script body is passed through extra_dirs[0] so each test can
            // supply its own without needing a distinct static def.
            build_args: |ctx| vec!["-c".into(), ctx.extra_dirs[0].clone()],
            capture_session: None,
            capabilities: AgentCapabilities {
                stream_format: format,
                prompt_delivery: delivery,
                prompt_encoding: PromptEncoding::Raw,
                native_resume: true,
                captures_session: true,
                mcp_injection: McpInjection::None,
                supports_images: false,
                permission: PermissionPosture::FullBypass,
                modes: argo_core::mode::ModeSupport::NONE,
            },
            install_url: "https://example.invalid",
        }
    }

    fn request(script: &str) -> ExecRequest {
        ExecRequest {
            bin: "sh".into(),
            prompt: "hello".into(),
            context: InvocationContext {
                cwd: std::env::temp_dir().to_string_lossy().to_string(),
                extra_dirs: vec![script.to_string()],
                ..Default::default()
            },
            env: vec![],
            mcp_servers: vec![],
            timeout_ms: Some(10_000),
        }
    }

    #[tokio::test]
    async fn runs_a_claude_style_stream_and_captures_the_session() {
        let def = script_def(StreamFormat::ClaudeStreamJson, PromptDelivery::Stdin);
        let script = r#"
            echo '{"type":"system","session_id":"sess-xyz"}'
            echo '{"type":"assistant","message":{"content":[{"type":"text","text":"hi there"}]}}'
            echo '{"type":"result","is_error":false,"result":"","num_turns":1,"duration_api_ms":50}'
        "#;
        let mut sink = CollectingSink::default();
        let cancel = CancelToken::new();
        let out = execute(&def, request(script), &cancel, &mut sink)
            .await
            .expect("execute");

        assert_eq!(out.outcome.status, RunStatus::Succeeded);
        assert_eq!(out.session_id.as_deref(), Some("sess-xyz"));
        assert!(sink.events.contains(&RunEventKind::TextDelta {
            text: "hi there".into()
        }));
    }

    #[tokio::test]
    async fn runs_a_codex_style_stream() {
        let def = script_def(StreamFormat::JsonEventStream, PromptDelivery::Stdin);
        let script = r#"
            echo '{"type":"thread.started","thread_id":"t-42"}'
            echo '{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"done"}}'
            echo '{"type":"turn.completed","usage":{"input_tokens":7}}'
        "#;
        let mut sink = CollectingSink::default();
        let out = execute(&def, request(script), &CancelToken::new(), &mut sink)
            .await
            .expect("execute");
        assert_eq!(out.outcome.status, RunStatus::Succeeded);
        assert_eq!(out.session_id.as_deref(), Some("t-42"));
        assert_eq!(out.outcome.usage.input, Some(7));
    }

    #[tokio::test]
    async fn the_prompt_reaches_the_child_on_stdin() {
        let def = script_def(StreamFormat::Plain, PromptDelivery::Stdin);
        // Echo stdin back so the test can observe what was delivered.
        let mut sink = CollectingSink::default();
        let out = execute(&def, request("cat"), &CancelToken::new(), &mut sink)
            .await
            .expect("execute");
        assert_eq!(out.outcome.status, RunStatus::Succeeded);
        let text: String = sink
            .events
            .iter()
            .filter_map(|e| match e {
                RunEventKind::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("hello"));
    }

    #[tokio::test]
    async fn a_plain_cli_that_prints_nothing_is_reported_as_failed() {
        // Exit 0 with no output is Grok's silent-decline shape.
        let def = script_def(StreamFormat::Plain, PromptDelivery::File);
        let mut sink = CollectingSink::default();
        let mut req = request("exit 0");
        req.context.prompt_file = Some("/dev/null".into());
        let out = execute(&def, req, &CancelToken::new(), &mut sink)
            .await
            .expect("execute");
        assert_eq!(out.outcome.status, RunStatus::Failed);
        assert!(out
            .outcome
            .message
            .expect("message")
            .contains("produced no output"));
    }

    #[tokio::test]
    async fn a_nonzero_exit_surfaces_stderr_as_the_failure_reason() {
        let def = script_def(StreamFormat::Plain, PromptDelivery::Stdin);
        let mut sink = CollectingSink::default();
        let out = execute(
            &def,
            request("echo 'not authenticated' >&2; exit 3"),
            &CancelToken::new(),
            &mut sink,
        )
        .await
        .expect("execute");
        assert_eq!(out.outcome.status, RunStatus::Failed);
        assert!(out
            .outcome
            .message
            .expect("message")
            .contains("not authenticated"));
    }

    #[tokio::test]
    async fn cancellation_stops_the_turn_promptly() {
        let def = script_def(StreamFormat::Plain, PromptDelivery::Stdin);
        let cancel = CancelToken::new();
        let token = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            token.cancel();
        });

        let mut sink = CollectingSink::default();
        let started = std::time::Instant::now();
        // A script that would otherwise run far longer than the test.
        let out = execute(
            &def,
            request("echo start; sleep 30; echo end"),
            &cancel,
            &mut sink,
        )
        .await
        .expect("execute");

        assert_eq!(out.outcome.status, RunStatus::Cancelled);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "cancellation must not wait for the child to finish naturally"
        );
    }

    #[tokio::test]
    async fn a_hung_child_hits_the_timeout() {
        let def = script_def(StreamFormat::Plain, PromptDelivery::Stdin);
        let mut sink = CollectingSink::default();
        let mut req = request("sleep 30");
        req.timeout_ms = Some(150);
        let err = execute(&def, req, &CancelToken::new(), &mut sink)
            .await
            .expect_err("must time out");
        assert_eq!(err.code(), "TIMEOUT");
    }

    #[tokio::test]
    async fn a_missing_binary_is_a_process_error_not_a_panic() {
        let def = script_def(StreamFormat::Plain, PromptDelivery::Stdin);
        let mut sink = CollectingSink::default();
        let mut req = request("true");
        req.bin = "argo-definitely-missing-binary".into();
        let err = execute(&def, req, &CancelToken::new(), &mut sink)
            .await
            .expect_err("must fail");
        assert_eq!(err.code(), "PROCESS_ERROR");
    }

    #[tokio::test]
    async fn environment_overrides_reach_the_child() {
        let def = script_def(StreamFormat::Plain, PromptDelivery::Stdin);
        let mut sink = CollectingSink::default();
        let mut req = request("printf '%s\\n' \"$ARGO_TEST_VAR\"");
        req.env = vec![("ARGO_TEST_VAR".into(), "injected".into())];
        execute(&def, req, &CancelToken::new(), &mut sink)
            .await
            .expect("execute");
        let text: String = sink
            .events
            .iter()
            .filter_map(|e| match e {
                RunEventKind::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(text.contains("injected"));
    }

    #[tokio::test]
    async fn an_acp_agent_completes_a_full_handshake() {
        // A scripted ACP peer: replies to initialize, session/new, and prompt in
        // order, exercising the real driver rather than a mock.
        let def = script_def(StreamFormat::AcpJsonRpc, PromptDelivery::Protocol);
        // Output follows the prompt, as a live agent's does: updates sent before it
        // are replayed history and are deliberately ignored.
        let script = r#"
            read -r line
            echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}'
            read -r line
            echo '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"sess_acp"}}'
            read -r line
            echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}}}'
            echo '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn","usage":{"inputTokens":11}}}'
        "#;
        let mut sink = CollectingSink::default();
        let out = execute(&def, request(script), &CancelToken::new(), &mut sink)
            .await
            .expect("execute");

        assert_eq!(out.outcome.status, RunStatus::Succeeded);
        assert_eq!(out.session_id.as_deref(), Some("sess_acp"));
        assert_eq!(out.outcome.usage.input, Some(11));
        assert!(sink.events.contains(&RunEventKind::TextDelta {
            text: "working".into()
        }));
    }

    #[tokio::test]
    async fn replayed_history_from_session_load_is_not_recorded_again() {
        // `session/load` replays the stored transcript before the new turn. Recording
        // it appended the previous reply to the new one, corrupting stored history.
        let def = script_def(StreamFormat::AcpJsonRpc, PromptDelivery::Protocol);
        let script = r#"
            read -r line
            echo '{"jsonrpc":"2.0","id":0,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}'
            read -r line
            echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OLD-REPLY"}}}}'
            echo '{"jsonrpc":"2.0","id":1,"result":{"sessionId":"sess_acp"}}'
            read -r line
            echo '{"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"NEW-REPLY"}}}}'
            echo '{"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}}'
        "#;
        let mut sink = CollectingSink::default();
        let out = execute(&def, request(script), &CancelToken::new(), &mut sink)
            .await
            .expect("execute");
        assert_eq!(out.outcome.status, RunStatus::Succeeded);

        let text: String = sink
            .events
            .iter()
            .filter_map(|e| match e {
                RunEventKind::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "NEW-REPLY", "replayed history must not be recorded");
    }

    #[tokio::test]
    async fn an_acp_agent_that_dies_mid_turn_fails_with_a_reason() {
        let def = script_def(StreamFormat::AcpJsonRpc, PromptDelivery::Protocol);
        let mut sink = CollectingSink::default();
        let out = execute(
            &def,
            request("echo 'fatal: acp bootstrap failed' >&2; exit 1"),
            &CancelToken::new(),
            &mut sink,
        )
        .await
        .expect("execute");
        assert_eq!(out.outcome.status, RunStatus::Failed);
        assert!(out
            .outcome
            .message
            .expect("message")
            .contains("acp bootstrap failed"));
    }

    #[tokio::test]
    async fn a_structured_stream_without_a_result_record_still_terminates() {
        let def = script_def(StreamFormat::ClaudeStreamJson, PromptDelivery::Stdin);
        let mut sink = CollectingSink::default();
        let out = execute(
            &def,
            request(r#"echo '{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}'"#),
            &CancelToken::new(),
            &mut sink,
        )
        .await
        .expect("execute");
        // Exited cleanly, so this is a success rather than an invented failure.
        assert_eq!(out.outcome.status, RunStatus::Succeeded);
    }

    #[test]
    fn cancel_token_is_observable_before_awaiting() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }
}
