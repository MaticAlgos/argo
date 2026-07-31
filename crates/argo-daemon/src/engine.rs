//! Turn orchestration.
//!
//! One function owns the full lifecycle of a turn, in the order that keeps the
//! store consistent even if the process dies partway:
//!
//! 1. Append the user message.
//! 2. Resolve the agent and model from the conversation's pending selection.
//! 3. Pin an assistant placeholder so streamed events always have an owner.
//! 4. Evaluate the resume guard against the conversation cursor.
//! 5. Compose the turn body — only the new prompt when resuming, otherwise the
//!    remaining context plus the prompt.
//! 6. Execute, persisting every normalized event as it arrives.
//! 7. If the upstream session turned out to be dead, clear the handle and retry
//!    once within the same turn, reseeded with full context.
//! 8. Finalize the assistant message and persist the session handle.

use argo_context::{
    budget::{fallback_summary, plan_budget, ContextBudget},
    compose_turn, ContextPackage, WorkspaceFacts,
};
use argo_core::error::{ArgoError, Result};
use argo_core::event::{RunEventKind, RunStatus};
use argo_core::ids::{AgentId, ConversationId, MessageId, RunId, SessionId};
use argo_core::message::{ContentBlock, Message, ToolCall, ToolStatus};
use argo_core::runtime::PromptDelivery;
use argo_core::session::{
    evaluate_resume, AgentSessionRecord, InvalidationReason, ResumeInputs, ResumePlan,
};
use argo_core::{sha256_hex, ArgoPaths};
use argo_runtime::{
    exec::{execute, CancelToken, ExecRequest},
    InvocationContext, StagedPrompt, StreamSink, TerminalOutcome,
};
use argo_store::{NewMessage, NewRun, Store};
use std::sync::{Arc, Mutex};

/// A store shared between the socket handlers and spawned turn tasks.
///
/// Locked briefly per operation and never across an `await`: SQLite calls are
/// microseconds, while holding the guard across a child process's lifetime would
/// serialize the whole daemon behind one turn.
pub type SharedStore = Arc<Mutex<Store>>;

/// Locks the shared store for one operation.
///
/// A poisoned lock means another thread panicked mid-write, which Argo cannot
/// reason about safely, so it is surfaced rather than ignored.
fn lock(store: &SharedStore) -> Result<std::sync::MutexGuard<'_, Store>> {
    store
        .lock()
        .map_err(|_| ArgoError::Store("store lock poisoned by a previous panic".into()))
}

/// Instructions prepended to every fresh session.
///
/// Kept short and stable: it is hashed to decide whether a resumed session needs
/// it re-sent, so churn here costs cache hits.
pub const STABLE_INSTRUCTIONS: &str = "\
You are working inside Argo, which orchestrates multiple coding-agent CLIs over one shared conversation. \
The transcript below may include turns produced by a different agent or model; treat it as authoritative \
history of this conversation. Continue the work rather than restarting it, and do not re-answer earlier turns. \
Only describe reasoning, tools, or child activity that Argo or the underlying CLI actually emits.";

/// Per-turn fallback available even when the upstream session is resumed.
const DELEGATION_DIRECTIVE: &str = "Argo delegation is available for exploratory work or a second opinion: run \"$ARGO_BIN\" delegate <agent> <self-contained task>, wait for its report, and incorporate the result.";

/// How many recent messages are considered before budgeting trims further.
const MAX_RECENT_MESSAGES: usize = 200;

/// Everything the engine needs to run one turn.
pub struct TurnRequest {
    /// Conversation receiving the turn.
    pub conversation_id: ConversationId,
    /// Host run that spawned this turn, for Argo-managed delegation.
    pub parent_run_id: Option<RunId>,
    /// Whether this turn is below the bounded delegation depth.
    pub delegation_allowed: bool,
    /// The user's message.
    pub prompt: String,
    /// Resolved agent.
    pub agent_id: AgentId,
    /// Resolved model, if any.
    pub model: Option<String>,
    /// Reasoning effort, if any.
    pub reasoning: Option<String>,
    /// Executable resolved by detection.
    pub bin: String,
    /// Help flags observed on the installed binary.
    pub help_flags: Vec<String>,
    /// Skills staged for this turn.
    pub active_skills: Vec<String>,
    /// MCP servers exposed for this turn.
    pub active_mcp_servers: Vec<String>,
    /// Rendered project instruction files.
    pub project_instructions: Option<String>,
    /// MCP descriptors for protocol adapters.
    pub mcp_descriptors: Vec<serde_json::Value>,
    /// Generated MCP config path, for adapters that take a file.
    pub mcp_config: Option<String>,
    /// Hard ceiling on the turn.
    pub timeout_ms: Option<u64>,
    /// Execution mode for this turn.
    pub mode: argo_core::mode::AgentMode,
}

/// What the engine decided and produced.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    /// The run that was created.
    pub run_id: RunId,
    /// Whether the upstream session was resumed.
    pub resumed: bool,
    /// Why a fresh session was started, when one was.
    pub invalidation: Option<InvalidationReason>,
    /// Terminal status.
    pub status: RunStatus,
    /// True when a dead session forced an in-turn reseed.
    pub reseeded: bool,
}

/// Sink that persists events as they stream and mirrors them to a listener.
struct PersistingSink<'a> {
    store: SharedStore,
    run_id: RunId,
    blocks: Vec<ContentBlock>,
    session_id: Option<String>,
    listener: Option<&'a (dyn Fn(argo_core::event::RunEvent) + Send + Sync)>,
}

impl StreamSink for PersistingSink<'_> {
    fn emit(&mut self, event: RunEventKind) {
        // Accumulate the assistant message as it forms, so a crash mid-turn still
        // leaves the partial reply recoverable from run_events.
        match &event {
            RunEventKind::TextDelta { text } if !text.is_empty() => match self.blocks.last_mut() {
                Some(ContentBlock::Text { text: accumulated }) => accumulated.push_str(text),
                _ => self.blocks.push(ContentBlock::text(text)),
            },
            RunEventKind::ThinkingDelta { text } if !text.is_empty() => {
                match self.blocks.last_mut() {
                    Some(ContentBlock::Thinking { text: accumulated }) => {
                        accumulated.push_str(text)
                    }
                    _ => self
                        .blocks
                        .push(ContentBlock::Thinking { text: text.clone() }),
                }
            }
            RunEventKind::FileWritten { path } => {
                if !self.blocks.iter().any(
                    |block| matches!(block, ContentBlock::FileWrite { path: known } if known == path),
                ) {
                    self.blocks
                        .push(ContentBlock::FileWrite { path: path.clone() });
                }
            }
            RunEventKind::ToolStarted { id, name, input } => {
                self.blocks.push(ContentBlock::Tool {
                    call: ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        output: None,
                        status: ToolStatus::Pending,
                    },
                });
            }
            RunEventKind::ToolCompleted { id, output, ok } => {
                let call = self.blocks.iter_mut().rev().find_map(|block| match block {
                    ContentBlock::Tool { call } if call.id == *id => Some(call),
                    _ => None,
                });
                if let Some(call) = call {
                    call.output = output.clone();
                    call.status = if *ok {
                        ToolStatus::Completed
                    } else {
                        ToolStatus::Failed
                    };
                } else {
                    self.blocks.push(ContentBlock::Tool {
                        call: ToolCall {
                            id: id.clone(),
                            name: "tool".to_string(),
                            input: None,
                            output: output.clone(),
                            status: if *ok {
                                ToolStatus::Completed
                            } else {
                                ToolStatus::Failed
                            },
                        },
                    });
                }
            }
            RunEventKind::SessionCaptured { session_id } => {
                self.session_id = Some(session_id.to_string());
            }
            _ => {}
        }

        let stored = lock(&self.store).and_then(|store| store.append_event(&self.run_id, event));
        match stored {
            Ok(stored) => {
                if let Some(listener) = self.listener {
                    listener(stored);
                }
            }
            Err(error) => {
                // Losing an event must not abort a turn the user is watching.
                tracing::warn!(%error, run_id = %self.run_id, "failed to persist run event");
            }
        }
    }
}

impl PersistingSink<'_> {
    /// Builds the final content blocks for the assistant message.
    fn blocks(&self) -> Vec<ContentBlock> {
        self.blocks
            .iter()
            .filter(|block| match block {
                ContentBlock::Text { text } | ContentBlock::Thinking { text } => {
                    !text.trim().is_empty()
                }
                _ => true,
            })
            .cloned()
            .collect()
    }
}

/// Runs one turn to completion.
pub async fn run_turn(
    store: &SharedStore,
    paths: &ArgoPaths,
    request: TurnRequest,
    cancel: &CancelToken,
    listener: Option<&(dyn Fn(argo_core::event::RunEvent) + Send + Sync)>,
) -> Result<TurnOutcome> {
    let def = argo_runtime::require(request.agent_id.as_str())?;
    let (conversation, cwd) = {
        let store = lock(store)?;
        let conversation = store.get_conversation(&request.conversation_id)?;
        let cwd = store.workspace_root(&conversation.workspace_id)?;
        (conversation, cwd)
    };

    // 1. The user's message becomes canonical history immediately, so it survives

    // A TUI-created conversation starts untitled. Persist a deterministic title
    // from the first real request before spawning, so even a failed turn remains
    // recognizable in `/resume`. Explicit `/new <title>` values are never replaced.
    if conversation
        .title
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
        .is_empty()
    {
        let title = argo_core::conversation_title(&request.prompt);
        lock(store)?.set_title(&request.conversation_id, &title)?;
    }
    //    even if the agent never responds.
    lock(store)?.append_message(
        &request.conversation_id,
        NewMessage::user(request.prompt.clone()),
    )?;

    // 2. Pin the assistant placeholder before spawning.
    let assistant_id = MessageId::generate();

    // 3. Decide resume vs fresh. The guard is confined to this block: holding it
    //    across the child process's lifetime would serialize the whole daemon
    //    behind one turn.
    let (stored_session, cursor) = {
        let store = lock(store)?;
        let stored_session =
            store.get_agent_session(&request.conversation_id, &request.agent_id)?;
        let cursor = store.latest_completed_assistant_message_id(
            &request.conversation_id,
            Some(&assistant_id),
            stored_session
                .as_ref()
                .and_then(|s| s.last_message_id.as_ref()),
        )?;
        (stored_session, cursor)
    };
    let plan = evaluate_resume(ResumeInputs {
        stored: stored_session.as_ref(),
        supports_resume: def.capabilities.native_resume,
        current_model: request.model.as_deref(),
        current_cwd: Some(&cwd),
        latest_completed_assistant: cursor.as_ref(),
    });

    // 4. Create the run and attach the placeholder.
    let run_id = lock(store)?.create_run(NewRun {
        conversation_id: request.conversation_id.clone(),
        workspace_id: conversation.workspace_id.clone(),
        agent_id: request.agent_id.clone(),
        model: request.model.clone(),
        resumed: plan.skip_transcript(),
        invalidation_reason: plan.invalidation,
        parent_run_id: request.parent_run_id.clone(),
    })?;
    lock(store)?.append_message_with_id(
        &request.conversation_id,
        assistant_id.clone(),
        NewMessage::assistant(
            vec![],
            request.agent_id.clone(),
            request.model.clone(),
            run_id.clone(),
        ),
    )?;
    {
        let store = lock(store)?;
        store.attach_run_message(&run_id, &assistant_id)?;
        store.mark_run_running(&run_id)?;
    }

    let mut sink = PersistingSink {
        store: Arc::clone(store),
        run_id: run_id.clone(),
        blocks: Vec::new(),
        session_id: None,
        listener,
    };

    sink.emit(RunEventKind::RunStarted {
        agent_id: request.agent_id.clone(),
        model: request.model.clone(),
        resumed: plan.skip_transcript(),
    });
    if thinking_stream_unavailable(def.id, request.reasoning.as_deref(), &request.help_flags) {
        sink.emit(RunEventKind::Diagnostic {
            code: "THINKING_UNAVAILABLE".into(),
            detail: "this Claude build does not advertise partial-message streaming; Argo can show only reasoning the CLI emits"
                .into(),
        });
    }

    // 5. Execute, with one transparent reseed if the handle turns out dead.
    let mut attempt_plan = plan.clone();
    let mut reseeded = false;
    let mut transient_retried = false;
    // Retained so a successful fresh turn can store the id it was told to use.
    let mut minted_session;
    let outcome = loop {
        let body = compose_body(store, &request, &attempt_plan, &assistant_id, &cwd)?;
        let staged = match def.capabilities.prompt_delivery {
            PromptDelivery::File => Some(StagedPrompt::create(
                paths.staging(),
                run_id.as_str(),
                &body,
            )?),
            _ => None,
        };

        let context = InvocationContext {
            prompt: (def.capabilities.prompt_delivery == PromptDelivery::Argument)
                .then(|| body.clone()),
            model: request.model.clone(),
            reasoning: request.reasoning.clone(),
            prompt_file: staged.as_ref().map(|s| s.path_string()),
            resume_session: attempt_plan
                .resume_session_id
                .as_ref()
                .map(|s| s.to_string()),
            // Minted per attempt so a reseed after a dead handle gets a new id
            // rather than colliding with the one that just failed.
            new_session: {
                let fresh = attempt_plan.resume_session_id.is_none().then(uuid_v4);
                minted_session = fresh.clone();
                fresh
            },
            cwd: cwd.clone(),
            extra_dirs: vec![],
            mcp_config: request.mcp_config.clone(),
            help_flags: request.help_flags.clone(),
            mode: request.mode,
        };

        let exec = execute(
            def,
            ExecRequest {
                bin: request.bin.clone(),
                prompt: body,
                context,
                env: vec![
                    (
                        crate::mcp::CONVERSATION_ENV.to_string(),
                        request.conversation_id.to_string(),
                    ),
                    (crate::mcp::RUN_ENV.to_string(), run_id.to_string()),
                    (
                        crate::mcp::BINARY_ENV.to_string(),
                        std::env::current_exe()
                            .map(|path| path.to_string_lossy().to_string())
                            .unwrap_or_else(|_| "argo".to_string()),
                    ),
                ],
                mcp_servers: request.mcp_descriptors.clone(),
                timeout_ms: request.timeout_ms,
            },
            cancel,
            &mut sink,
        )
        .await;

        // The staged prompt is deleted here, when `staged` drops.
        drop(staged);

        match exec {
            Ok(exec) => {
                let dead_session = exec.outcome.resume_target_missing
                    && attempt_plan.skip_transcript()
                    && !reseeded;
                if dead_session {
                    // The stored handle is gone. Clear it and retry this same turn
                    // with full context so the user still gets an answer.
                    lock(store)?
                        .clear_agent_session(&request.conversation_id, &request.agent_id)?;
                    sink.emit(RunEventKind::SessionReseeded {
                        reason: "the agent's saved session no longer exists".into(),
                    });
                    attempt_plan = ResumePlan::fresh(None, attempt_plan.stored_session_id.clone());
                    reseeded = true;
                    continue;
                }

                let has_output = !sink.blocks().is_empty();
                // Retry once only before the CLI emitted meaningful output. A
                // retry after tools or prose could duplicate side effects; that
                // case remains visible and available for explicit user retry.
                if should_retry_transient(&exec.outcome, transient_retried, has_output) {
                    let detail = exec
                        .outcome
                        .message
                        .as_deref()
                        .unwrap_or("transient network failure");
                    sink.emit(RunEventKind::Diagnostic {
                        code: "TRANSIENT_RETRY".into(),
                        detail: format!("{detail}; retrying once"),
                    });
                    transient_retried = true;
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                break exec;
            }
            Err(error) => {
                sink.emit(RunEventKind::Error {
                    code: error.code().to_string(),
                    message: error.to_string(),
                    retryable: error.is_retryable(),
                });
                let status = if matches!(error, ArgoError::Cancelled) {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                };
                finalize(store, &sink, &assistant_id)?;
                lock(store)?.finish_run(
                    &run_id,
                    status,
                    Some((error.code(), &error.to_string())),
                )?;
                // Terminal is the commit barrier for clients and queues: all
                // canonical state must be durable before this is broadcast.
                sink.emit(RunEventKind::RunFinished {
                    status,
                    usage: Default::default(),
                });
                return Ok(TurnOutcome {
                    run_id,
                    resumed: false,
                    invalidation: attempt_plan.invalidation,
                    status,
                    reseeded,
                });
            }
        }
    };

    // 6. Finalize canonical state before publishing the terminal event. Queued
    // clients treat RunFinished as a commit barrier and may send the next turn
    // immediately.
    if let Some(error) = terminal_failure_event(&outcome.outcome) {
        sink.emit(error);
    }
    finalize(store, &sink, &assistant_id)?;

    let error_detail = outcome
        .outcome
        .message
        .clone()
        .filter(|_| outcome.outcome.status == RunStatus::Failed);
    lock(store)?.finish_run(
        &run_id,
        outcome.outcome.status,
        error_detail
            .as_deref()
            .map(|message| ("AGENT_ERROR", message)),
    )?;

    // 7. Persist the upstream handle, but only for a turn that actually completed.
    //    Storing a handle from a failed turn would resume into a broken state.
    if def.capabilities.captures_session && outcome.outcome.status == RunStatus::Succeeded {
        match outcome
            .session_id
            .clone()
            .or_else(|| {
                attempt_plan
                    .resume_session_id
                    .as_ref()
                    .map(|s| s.to_string())
            })
            .or_else(|| minted_session.clone())
        {
            Some(session) => lock(store)?.upsert_agent_session(
                &request.conversation_id,
                &AgentSessionRecord {
                    agent_id: request.agent_id.clone(),
                    session_id: SessionId::new(session),
                    model: request.model.clone(),
                    cwd: Some(cwd.clone()),
                    stable_hash: Some(sha256_hex(STABLE_INSTRUCTIONS)),
                    last_message_id: Some(assistant_id.clone()),
                    updated_at: 0,
                },
            )?,
            None => {
                // The adapter claimed it captures sessions but produced no handle;
                // clearing prevents resuming into an unidentified session.
                lock(store)?.clear_agent_session(&request.conversation_id, &request.agent_id)?;
            }
        }
    }

    // Published last: at this point the assistant message, terminal run status,
    // and resumable session cursor are all visible in one committed store state.
    sink.emit(RunEventKind::RunFinished {
        status: outcome.outcome.status,
        usage: outcome.outcome.usage,
    });

    Ok(TurnOutcome {
        run_id,
        resumed: attempt_plan.skip_transcript(),
        invalidation: attempt_plan.invalidation,
        status: outcome.outcome.status,
        reseeded,
    })
}

/// Generates a fresh session identifier.
///
/// Claude accepts `--session-id <uuid>`, which makes the next turn's resume
/// reliable without depending on the stream disclosing an id.
fn uuid_v4() -> String {
    argo_core::ids::SessionId::generate().to_string()
}

fn thinking_stream_unavailable(
    agent_id: &str,
    reasoning: Option<&str>,
    help_flags: &[String],
) -> bool {
    agent_id == "claude"
        && reasoning.is_some()
        && !help_flags
            .iter()
            .any(|flag| flag == "--include-partial-messages")
}

fn should_retry_transient(
    outcome: &TerminalOutcome,
    already_retried: bool,
    has_output: bool,
) -> bool {
    outcome.status == RunStatus::Failed
        && !already_retried
        && !has_output
        && outcome
            .message
            .as_deref()
            .is_some_and(argo_runtime::stream::is_retryable_failure)
}

fn terminal_failure_event(outcome: &TerminalOutcome) -> Option<RunEventKind> {
    if outcome.status != RunStatus::Failed {
        return None;
    }
    let message = outcome
        .message
        .clone()
        .unwrap_or_else(|| "the agent ended the turn without a response".into());
    Some(RunEventKind::Error {
        code: "AGENT_ERROR".into(),
        retryable: argo_runtime::stream::is_retryable_failure(&message),
        message,
    })
}

/// Writes the accumulated assistant content into its pinned message.
fn finalize(
    store: &SharedStore,
    sink: &PersistingSink<'_>,
    assistant_id: &MessageId,
) -> Result<()> {
    let blocks = sink.blocks();
    if blocks.is_empty() {
        return Ok(());
    }
    lock(store)?.set_message_blocks(assistant_id, &blocks)
}

/// Composes the body for one attempt.
pub fn compose_body(
    store: &SharedStore,
    request: &TurnRequest,
    plan: &ResumePlan,
    assistant_id: &MessageId,
    cwd: &str,
) -> Result<String> {
    // A resumed session already holds the history; sending it again would
    // duplicate context and invite the model to re-answer an earlier turn.
    let body = if plan.skip_transcript() {
        compose_turn(plan, &ContextPackage::default(), &request.prompt)
    } else {
        let package = build_context_package(store, request, assistant_id, cwd)?;
        compose_turn(plan, &package, &request.prompt)
    };

    // Per-turn directives lead the turn: restrictions and delegation availability
    // must also reach resumed native sessions that skip canonical context.
    let mut directives = Vec::new();
    if let Some(mode) = request.mode.directive() {
        directives.push(mode);
    }
    if request.delegation_allowed {
        directives.push(DELEGATION_DIRECTIVE);
    }
    if directives.is_empty() {
        Ok(body)
    } else {
        Ok(format!("{}\n\n{body}", directives.join("\n\n")))
    }
}

/// Assembles the remaining context for a fresh session.
pub fn build_context_package(
    store: &SharedStore,
    request: &TurnRequest,
    assistant_id: &MessageId,
    cwd: &str,
) -> Result<ContextPackage> {
    let history: Vec<Message> = lock(store)?
        .list_recent_messages(&request.conversation_id, MAX_RECENT_MESSAGES)?
        .into_iter()
        // Exclude this turn's own rows: the placeholder is empty, and the prompt
        // is delivered separately as the live request.
        .filter(|m| &m.id != assistant_id)
        .collect();
    let history = drop_last_user_prompt(history, &request.prompt);

    let files_touched: Vec<String> = history
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::FileWrite { path } => Some(path.clone()),
            _ => None,
        })
        .fold(Vec::new(), |mut acc, path| {
            if !acc.contains(&path) {
                acc.push(path);
            }
            acc
        });

    // Trim to what the target model can accept, summarizing the remainder rather
    // than silently dropping it.
    let budget = ContextBudget::conservative();
    let plan = plan_budget(&history, budget);
    let (older, recent) = history.split_at(plan.verbatim_from.min(history.len()));
    let compacted_summary = plan
        .needs_compaction
        .then(|| fallback_summary(older))
        .filter(|s| !s.is_empty());

    Ok(ContextPackage {
        stable_instructions: STABLE_INSTRUCTIONS.to_string(),
        workspace: WorkspaceFacts {
            root: cwd.to_string(),
            git_branch: None,
            git_dirty: false,
            files_touched,
        },
        active_skills: request.active_skills.clone(),
        active_mcp_servers: request.active_mcp_servers.clone(),
        project_instructions: request.project_instructions.clone(),
        compacted_summary,
        recent_messages: recent.to_vec(),
        open_tasks: vec![],
        child_outcomes: vec![],
    })
}

/// Removes the just-appended user prompt from replayed history.
///
/// It is delivered as the live request, so including it in the transcript would
/// show the model the same instruction twice.
fn drop_last_user_prompt(mut history: Vec<Message>, prompt: &str) -> Vec<Message> {
    if let Some(position) = history
        .iter()
        .rposition(|m| m.role == argo_core::message::Role::User && m.transferable_text() == prompt)
    {
        history.remove(position);
    }
    history
}

#[cfg(test)]
mod tests {
    use super::*;
    use argo_core::ids::WorkspaceId;

    fn setup() -> (
        SharedStore,
        ArgoPaths,
        ConversationId,
        WorkspaceId,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(dir.path().join("argo.sqlite")).expect("store");
        let paths = ArgoPaths::with_root(dir.path().join("data"));
        paths.ensure_dirs().expect("dirs");
        let ws = store.ensure_workspace(dir.path()).expect("workspace");
        let conv = store.create_conversation(&ws, None).expect("conversation");
        (Arc::new(Mutex::new(store)), paths, conv, ws, dir)
    }

    /// Appends a message through the shared store.
    fn append(store: &SharedStore, conv: &ConversationId, message: NewMessage) {
        lock(store)
            .expect("lock")
            .append_message(conv, message)
            .expect("append");
    }

    fn request(conv: &ConversationId, agent: &str, prompt: &str) -> TurnRequest {
        TurnRequest {
            conversation_id: conv.clone(),
            parent_run_id: None,
            delegation_allowed: true,
            prompt: prompt.into(),
            agent_id: AgentId::new(agent),
            model: Some("m1".into()),
            reasoning: None,
            bin: "sh".into(),
            help_flags: vec![],
            active_skills: vec![],
            active_mcp_servers: vec![],
            mcp_descriptors: vec![],
            mcp_config: None,
            project_instructions: None,
            timeout_ms: Some(5_000),
            mode: argo_core::mode::AgentMode::Full,
        }
    }

    #[test]
    fn stable_instructions_hash_is_deterministic() {
        // The hash decides whether a resumed session must re-receive the block, so
        // it must not vary run to run.
        assert_eq!(
            sha256_hex(STABLE_INSTRUCTIONS),
            sha256_hex(STABLE_INSTRUCTIONS)
        );
    }

    #[test]
    fn the_live_prompt_is_not_duplicated_in_the_replayed_transcript() {
        let (store, _paths, conv, _ws, _dir) = setup();
        append(&store, &conv, NewMessage::user("earlier question"));
        append(&store, &conv, NewMessage::user("current prompt"));

        let assistant_id = MessageId::new("pending");
        let package = build_context_package(
            &store,
            &request(&conv, "claude", "current prompt"),
            &assistant_id,
            "/repo",
        )
        .expect("package");

        let rendered = package.render();
        assert!(rendered.contains("earlier question"));
        assert!(
            !rendered.contains("current prompt"),
            "the live request must not also appear in the transcript"
        );
    }

    #[test]
    fn a_restrictive_mode_leads_the_turn() {
        // Stated first so the boundary cannot be lost behind a long transcript.
        let (store, _paths, conv, _ws, _dir) = setup();
        append(&store, &conv, NewMessage::user("history"));
        let mut request = request(&conv, "claude", "add a feature");
        request.mode = argo_core::mode::AgentMode::Plan;
        let body = compose_body(
            &store,
            &request,
            &ResumePlan::fresh(None, None),
            &MessageId::new("pending"),
            "/repo",
        )
        .expect("body");
        assert!(body.starts_with("## Mode: PLAN"));
        assert!(body.contains("add a feature"));
    }

    #[test]
    fn full_mode_adds_no_directive() {
        let (store, _paths, conv, _ws, _dir) = setup();
        let body = compose_body(
            &store,
            &request(&conv, "claude", "go"),
            &ResumePlan::fresh(None, None),
            &MessageId::new("pending"),
            "/repo",
        )
        .expect("body");
        assert!(!body.contains("## Mode:"));
    }

    #[test]
    fn the_directive_survives_a_resumed_turn() {
        // A resumed session skips the transcript but must still carry the mode.
        let (store, _paths, conv, _ws, _dir) = setup();
        let mut request = request(&conv, "claude", "keep planning");
        request.mode = argo_core::mode::AgentMode::Plan;
        let plan = ResumePlan {
            decision: argo_core::session::ResumeDecision::Resume,
            resume_session_id: Some(SessionId::new("s1")),
            stored_session_id: Some(SessionId::new("s1")),
            invalidation: None,
            stored_stable_hash: None,
        };
        let body =
            compose_body(&store, &request, &plan, &MessageId::new("p"), "/repo").expect("body");
        assert!(body.starts_with("## Mode: PLAN"));
        assert!(body.contains("keep planning"));
    }

    #[test]
    fn resumed_turns_compose_only_the_prompt() {
        let (store, _paths, conv, _ws, _dir) = setup();
        append(&store, &conv, NewMessage::user("history"));
        let plan = ResumePlan {
            decision: argo_core::session::ResumeDecision::Resume,
            resume_session_id: Some(SessionId::new("s1")),
            stored_session_id: Some(SessionId::new("s1")),
            invalidation: None,
            stored_stable_hash: None,
        };
        let body = compose_body(
            &store,
            &request(&conv, "claude", "next thing"),
            &plan,
            &MessageId::new("pending"),
            "/repo",
        )
        .expect("body");
        assert!(body.starts_with(DELEGATION_DIRECTIVE));
        assert!(body.ends_with("next thing"));
        assert!(!body.contains(argo_context::TRANSCRIPT_HEADING));
    }

    #[test]
    fn fresh_turns_compose_context_plus_the_prompt() {
        let (store, _paths, conv, _ws, _dir) = setup();
        append(&store, &conv, NewMessage::user("earlier"));
        let plan = ResumePlan::fresh(Some(InvalidationReason::ModelChanged), None);
        let body = compose_body(
            &store,
            &request(&conv, "claude", "next thing"),
            &plan,
            &MessageId::new("pending"),
            "/repo",
        )
        .expect("body");
        assert!(body.contains(argo_context::TRANSCRIPT_HEADING));
        assert!(body.contains("earlier"));
        assert!(body.contains("## Current request\nnext thing"));
        assert!(body.contains(STABLE_INSTRUCTIONS));
    }

    #[test]
    fn completed_tool_outputs_are_preserved_for_cross_agent_context() {
        let (store, _paths, conv, ws, _dir) = setup();
        let run = lock(&store)
            .expect("lock")
            .create_run(NewRun {
                conversation_id: conv,
                workspace_id: ws,
                agent_id: AgentId::new("antigravity"),
                model: None,
                resumed: false,
                invalidation_reason: None,
                parent_run_id: None,
            })
            .expect("run");
        let mut sink = PersistingSink {
            store,
            run_id: run,
            blocks: Vec::new(),
            session_id: None,
            listener: None,
        };
        sink.emit(RunEventKind::TextDelta {
            text: "Submitting. ".into(),
        });
        sink.emit(RunEventKind::ToolStarted {
            id: "17".into(),
            name: "call_mcp_tool".into(),
            input: Some("run_backtest".into()),
        });
        sink.emit(RunEventKind::ToolCompleted {
            id: "17".into(),
            output: Some("{\"runID\":\"backtest-123\"}".into()),
            ok: true,
        });
        sink.emit(RunEventKind::TextDelta {
            text: "Done.".into(),
        });

        let blocks = sink.blocks();
        assert!(matches!(
            &blocks[0],
            ContentBlock::Text { text } if text == "Submitting. "
        ));
        let ContentBlock::Tool { call } = &blocks[1] else {
            panic!("expected durable tool block");
        };
        assert_eq!(call.name, "call_mcp_tool");
        assert_eq!(call.status, ToolStatus::Completed);
        assert!(call
            .output
            .as_deref()
            .is_some_and(|out| out.contains("backtest-123")));
        assert!(matches!(
            &blocks[2],
            ContentBlock::Text { text } if text == "Done."
        ));
    }

    #[test]
    fn files_touched_are_collected_for_the_receiving_agent() {
        let (store, _paths, conv, ws, _dir) = setup();
        let run = lock(&store)
            .expect("lock")
            .create_run(NewRun {
                conversation_id: conv.clone(),
                workspace_id: ws,
                agent_id: AgentId::new("claude"),
                model: None,
                resumed: false,
                invalidation_reason: None,
                parent_run_id: None,
            })
            .expect("run");
        append(
            &store,
            &conv,
            NewMessage::assistant(
                vec![
                    ContentBlock::text("edited"),
                    ContentBlock::FileWrite {
                        path: "src/a.rs".into(),
                    },
                ],
                AgentId::new("claude"),
                None,
                run,
            ),
        );

        let package = build_context_package(
            &store,
            &request(&conv, "codex", "continue"),
            &MessageId::new("pending"),
            "/repo",
        )
        .expect("package");
        assert_eq!(
            package.workspace.files_touched,
            vec!["src/a.rs".to_string()]
        );
        assert!(package.render().contains("files changed so far"));
    }

    #[test]
    fn long_histories_are_compacted_rather_than_dropped() {
        let (store, _paths, conv, _ws, _dir) = setup();
        for i in 0..80 {
            append(
                &store,
                &conv,
                NewMessage::user(format!("{} {}", i, "detail ".repeat(200))),
            );
        }
        let package = build_context_package(
            &store,
            &request(&conv, "claude", "summarize"),
            &MessageId::new("pending"),
            "/repo",
        )
        .expect("package");

        let summary = package
            .compacted_summary
            .as_ref()
            .expect("expected a compaction summary");
        assert!(summary.contains("earlier message(s) omitted"));
        // Nothing was silently lost: the summary states what it stands in for.
        assert!(summary.contains("full history is retained"));
        assert!(package.recent_messages.len() < 80);
    }

    #[test]
    fn missing_partial_message_support_is_reported_only_for_claude_reasoning() {
        assert!(thinking_stream_unavailable("claude", Some("high"), &[]));
        assert!(!thinking_stream_unavailable(
            "claude",
            Some("high"),
            &["--include-partial-messages".into()]
        ));
        assert!(!thinking_stream_unavailable("codex", Some("high"), &[]));
        assert!(!thinking_stream_unavailable("claude", None, &[]));
    }

    #[test]
    fn failed_cli_outcomes_become_visible_retryable_error_events() {
        let network = TerminalOutcome::failed(
            "dial tcp: lookup daily-cloudcode-pa.googleapis.com: no such host",
        );
        assert!(should_retry_transient(&network, false, false));
        assert!(!should_retry_transient(&network, true, false));
        assert!(!should_retry_transient(&network, false, true));
        assert!(matches!(
            terminal_failure_event(&network),
            Some(RunEventKind::Error {
                code,
                retryable: true,
                message,
            }) if code == "AGENT_ERROR" && message.contains("no such host")
        ));

        let invalid = TerminalOutcome::failed("invalid model name");
        assert!(matches!(
            terminal_failure_event(&invalid),
            Some(RunEventKind::Error {
                retryable: false,
                ..
            })
        ));
        assert!(terminal_failure_event(&TerminalOutcome::succeeded()).is_none());
    }

    #[tokio::test]
    async fn an_unknown_agent_is_rejected_before_anything_is_written() {
        let (store, paths, conv, _ws, _dir) = setup();
        let before = lock(&store)
            .expect("lock")
            .list_messages(&conv)
            .expect("messages")
            .len();
        let err = run_turn(
            &store,
            &paths,
            request(&conv, "not-a-real-agent", "hi"),
            &CancelToken::new(),
            None,
        )
        .await
        .expect_err("must reject");
        assert_eq!(err.code(), "INVALID_REQUEST");
        assert_eq!(
            lock(&store)
                .expect("lock")
                .list_messages(&conv)
                .expect("messages")
                .len(),
            before,
            "a rejected turn must not append rows"
        );
    }
}
