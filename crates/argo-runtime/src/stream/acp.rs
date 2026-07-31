//! ACP (Agent Client Protocol) JSON-RPC over stdio.
//!
//! Verified against the ACP v1 specification and Kiro's documented support. The
//! lifecycle Argo drives is:
//!
//! 1. `initialize` — negotiate protocol version and exchange capabilities.
//! 2. `session/new` or `session/load` — create, or resume when Argo holds a
//!    handle and the agent advertises `loadSession`.
//! 3. `session/set_model` — only when a concrete model was selected.
//! 4. `session/prompt` — send the composed turn; the response ends the turn.
//! 5. `session/cancel` — notification, on user cancellation.
//!
//! The agent streams `session/update` notifications in between. Kiro also sends
//! `_kiro.dev/*` extension messages, which are optional by spec and ignored here
//! rather than treated as protocol errors.

use super::{truncate, StreamSink, TerminalOutcome};
use argo_core::event::{RunEventKind, RunStatus, TokenUsage};
use argo_core::ids::SessionId;
use serde_json::json;

/// ACP protocol version Argo speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// A JSON-RPC message Argo needs to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// Serialized JSON-RPC frame, newline-delimited on the wire.
    pub json: String,
}

/// What the parser wants the transport to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpAction {
    /// Send this frame to the agent.
    Send(OutgoingMessage),
    /// Nothing to do.
    Idle,
}

/// Drives one ACP turn.
///
/// Kept transport-agnostic so it can be unit-tested against a scripted peer
/// without spawning a process.
#[derive(Debug)]
pub struct AcpSession {
    next_id: i64,
    state: State,
    session_id: Option<String>,
    resume_target: Option<String>,
    model: Option<String>,
    prompt: String,
    mcp_servers: Vec<serde_json::Value>,
    cwd: String,
    load_supported: bool,
    outcome: Option<TerminalOutcome>,
    initialize_id: Option<i64>,
    session_id_request: Option<i64>,
    set_model_id: Option<i64>,
    prompt_id: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Start,
    AwaitingInitialize,
    AwaitingSession,
    AwaitingSetModel,
    AwaitingPrompt,
    Done,
}

impl AcpSession {
    /// Creates a session driver for one turn.
    pub fn new(
        cwd: impl Into<String>,
        prompt: impl Into<String>,
        resume_target: Option<String>,
        model: Option<String>,
        mcp_servers: Vec<serde_json::Value>,
    ) -> Self {
        Self {
            next_id: 0,
            state: State::Start,
            session_id: None,
            resume_target,
            model,
            prompt: prompt.into(),
            mcp_servers,
            cwd: cwd.into(),
            load_supported: false,
            outcome: None,
            initialize_id: None,
            session_id_request: None,
            set_model_id: None,
            prompt_id: None,
        }
    }

    fn allocate_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// First frame to send: `initialize`.
    pub fn start(&mut self) -> AcpAction {
        let id = self.allocate_id();
        self.initialize_id = Some(id);
        self.state = State::AwaitingInitialize;
        // Argo declares the client capabilities it can actually service. Claiming
        // a capability it cannot honor would strand the agent waiting on a reply.
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": { "readTextFile": false, "writeTextFile": false },
                    "terminal": false
                },
                "clientInfo": { "name": "argo", "version": env!("CARGO_PKG_VERSION") }
            }
        }))
    }

    fn send(&self, value: serde_json::Value) -> AcpAction {
        // ACP failures are otherwise almost undiagnosable: the agent simply stops
        // and the turn hangs, with no clue which request went unanswered.
        tracing::debug!(target: "argo::acp", direction = "out", json = %truncate(&value.to_string(), 400));
        AcpAction::Send(OutgoingMessage {
            json: value.to_string(),
        })
    }

    /// Handles one inbound line, returning the next action.
    pub fn handle_line(&mut self, line: &str, sink: &mut dyn StreamSink) -> AcpAction {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return AcpAction::Idle;
        }
        tracing::debug!(target: "argo::acp", direction = "in", json = %truncate(trimmed, 400));
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // stderr carries logs by spec; unparseable stdout is still only a
            // diagnostic, never a turn failure.
            sink.emit(RunEventKind::Diagnostic {
                code: "UNPARSEABLE_LINE".into(),
                detail: truncate(trimmed, 200),
            });
            return AcpAction::Idle;
        };

        if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
            return self.handle_inbound_method(method, &value, sink);
        }
        self.handle_response(&value, sink)
    }

    fn handle_inbound_method(
        &mut self,
        method: &str,
        value: &serde_json::Value,
        sink: &mut dyn StreamSink,
    ) -> AcpAction {
        match method {
            "session/update" => {
                // `session/load` replays the whole stored transcript as updates
                // before the new turn begins. Those are history Argo already has, so
                // recording them would append the previous reply to this one — the
                // canonical message would end up holding both.
                if self.prompt_id.is_none() {
                    tracing::debug!(
                        target: "argo::acp",
                        "ignoring replayed session update received before the prompt"
                    );
                    return AcpAction::Idle;
                }
                self.handle_update(value.get("params"), sink);
                AcpAction::Idle
            }
            "session/request_permission" => {
                // Argo runs with the full-bypass posture the user selected, and
                // there is no TTY to prompt on, so approve the first allow-style
                // option rather than deadlocking the turn.
                let id = value.get("id").cloned().unwrap_or(json!(null));
                let option = self
                    .first_allow_option(value)
                    .unwrap_or_else(|| "allow".to_string());
                sink.emit(RunEventKind::Diagnostic {
                    code: "PERMISSION_AUTO_APPROVED".into(),
                    detail: format!("auto-approved permission request with '{option}'"),
                });
                self.send(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "outcome": { "outcome": "selected", "optionId": option } }
                }))
            }
            "_kiro.dev/subagent/list_update" => {
                sink.emit(RunEventKind::Diagnostic {
                    code: "NATIVE_SUBAGENT_UNAVAILABLE".into(),
                    detail: "Kiro emitted a native-subagent update, but its vendor extension does not expose a verified child event schema; use Argo delegation for inspectable child activity".into(),
                });
                AcpAction::Idle
            }
            // Anything else is unimplemented. The distinction that matters is not
            // the method name but whether the peer expects an answer: a request
            // carries an `id` and blocks until it gets one, so silently ignoring it
            // strands the agent until it gives up and exits. Notifications have no
            // `id` and are safely dropped.
            //
            // This covers vendor extensions (`_kiro.dev/...`) and filesystem or
            // terminal callbacks Argo declined in its client capabilities but that
            // an agent may still attempt.
            other => match value.get("id") {
                Some(id) if !id.is_null() => {
                    sink.emit(RunEventKind::Diagnostic {
                        code: "ACP_METHOD_UNSUPPORTED".into(),
                        detail: format!("declined unsupported request '{other}'"),
                    });
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "id": id.clone(),
                        "error": {
                            "code": -32601,
                            "message": format!("{other} is not supported by this client")
                        }
                    }))
                }
                _ => AcpAction::Idle,
            },
        }
    }

    fn first_allow_option(&self, value: &serde_json::Value) -> Option<String> {
        let options = value.get("params")?.get("options")?.as_array()?;
        options
            .iter()
            .find(|o| {
                let kind = o.get("kind").and_then(|v| v.as_str()).unwrap_or_default();
                kind.contains("allow")
            })
            .or_else(|| options.first())
            .and_then(|o| o.get("optionId").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
    }

    fn handle_response(
        &mut self,
        value: &serde_json::Value,
        sink: &mut dyn StreamSink,
    ) -> AcpAction {
        let id = value.get("id").and_then(|v| v.as_i64());

        if let Some(error) = value.get("error") {
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("acp error");
            // A failed `session/load` means the stored handle is dead. Report it
            // so the engine can clear the handle and reseed in the same turn.
            let loading =
                id.is_some() && id == self.session_id_request && self.resume_target.is_some();
            let kind = error
                .get("data")
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let resume_dead = loading || kind == "resume_failed";
            sink.emit(RunEventKind::Error {
                code: if resume_dead {
                    "RESUME_TARGET_MISSING".into()
                } else {
                    "AGENT_ERROR".into()
                },
                message: truncate(message, 500),
                retryable: resume_dead,
            });
            self.outcome = Some(TerminalOutcome {
                status: RunStatus::Failed,
                usage: TokenUsage::default(),
                resume_target_missing: resume_dead,
                message: Some(truncate(message, 500)),
            });
            self.state = State::Done;
            return AcpAction::Idle;
        }

        let result = value.get("result");

        if id == self.initialize_id {
            self.load_supported = result
                .and_then(|r| r.get("agentCapabilities"))
                .and_then(|c| c.get("loadSession"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            return self.open_session();
        }

        if id == self.session_id_request {
            if let Some(session) = result
                .and_then(|r| r.get("sessionId"))
                .and_then(|v| v.as_str())
            {
                self.session_id = Some(session.to_string());
                sink.emit(RunEventKind::SessionCaptured {
                    session_id: SessionId::new(session),
                });
            } else if let Some(existing) = &self.resume_target {
                // `session/load` legitimately returns no id; the handle we sent is
                // still the live session.
                self.session_id = Some(existing.clone());
            }
            return self.select_model_or_prompt();
        }

        if id == self.set_model_id {
            // A rejected model selection is not fatal; the CLI keeps its default.
            return self.send_prompt();
        }

        if id == self.prompt_id {
            let stop_reason = result
                .and_then(|r| r.get("stopReason"))
                .and_then(|v| v.as_str())
                .unwrap_or("end_turn");
            let status = if stop_reason == "refusal" {
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            };
            self.outcome = Some(TerminalOutcome {
                status,
                usage: parse_usage(result.and_then(|r| r.get("usage"))),
                resume_target_missing: false,
                message: (status == RunStatus::Failed).then(|| stop_reason.to_string()),
            });
            self.state = State::Done;
            return AcpAction::Idle;
        }

        AcpAction::Idle
    }

    fn open_session(&mut self) -> AcpAction {
        let id = self.allocate_id();
        self.session_id_request = Some(id);
        self.state = State::AwaitingSession;

        // Resume only when Argo holds a handle *and* the agent advertised support.
        match (&self.resume_target, self.load_supported) {
            (Some(session), true) => self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/load",
                "params": { "sessionId": session, "cwd": self.cwd, "mcpServers": self.mcp_servers }
            })),
            _ => self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/new",
                "params": { "cwd": self.cwd, "mcpServers": self.mcp_servers }
            })),
        }
    }

    fn select_model_or_prompt(&mut self) -> AcpAction {
        let Some(model) = self.model.clone() else {
            return self.send_prompt();
        };
        let id = self.allocate_id();
        self.set_model_id = Some(id);
        self.state = State::AwaitingSetModel;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/set_model",
            "params": { "sessionId": self.session_id, "modelId": model }
        }))
    }

    fn send_prompt(&mut self) -> AcpAction {
        let id = self.allocate_id();
        self.prompt_id = Some(id);
        self.state = State::AwaitingPrompt;
        // The ACP v1 schema names this field `prompt`, but Kiro's documented
        // example uses `content`, and it exits rather than answering when only
        // `prompt` is present. Sending both satisfies either reading; an agent
        // that knows one key ignores the other.
        let blocks = json!([{ "type": "text", "text": self.prompt }]);
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": self.session_id,
                "prompt": blocks,
                "content": blocks
            }
        }))
    }

    /// Frame that cancels the active turn.
    pub fn cancel(&self) -> Option<OutgoingMessage> {
        let session = self.session_id.as_ref()?;
        Some(OutgoingMessage {
            json: json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": session }
            })
            .to_string(),
        })
    }

    fn handle_update(&mut self, params: Option<&serde_json::Value>, sink: &mut dyn StreamSink) {
        let Some(update) = params.and_then(|p| p.get("update")) else {
            return;
        };
        let kind = update
            .get("sessionUpdate")
            .or_else(|| update.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        match kind {
            "agent_message_chunk" | "AgentMessageChunk" => {
                if let Some(text) = chunk_text(update) {
                    sink.emit(RunEventKind::TextDelta { text });
                }
            }
            "agent_thought_chunk" | "AgentThoughtChunk" => {
                if let Some(text) = chunk_text(update) {
                    sink.emit(RunEventKind::ThinkingDelta { text });
                }
            }
            "tool_call" | "ToolCall" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let name = update
                    .get("title")
                    .or_else(|| update.get("kind"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                sink.emit(RunEventKind::ToolStarted {
                    id,
                    name,
                    input: update
                        .get("rawInput")
                        .map(|v| truncate(&v.to_string(), 2_000)),
                });
            }
            "tool_call_update" | "ToolCallUpdate" => {
                let id = update
                    .get("toolCallId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let status = update
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                for path in acp_paths(update) {
                    sink.emit(RunEventKind::FileWritten { path });
                }
                if matches!(status, "completed" | "failed") {
                    sink.emit(RunEventKind::ToolCompleted {
                        id,
                        output: update
                            .get("rawOutput")
                            .map(|v| truncate(&v.to_string(), 4_000)),
                        ok: status == "completed",
                    });
                }
            }
            "plan" | "Plan" => {
                if let Some(entries) = update.get("entries").and_then(|v| v.as_array()) {
                    let steps: Vec<String> = entries
                        .iter()
                        .filter_map(|e| {
                            let content = e.get("content").and_then(|v| v.as_str())?;
                            let status = e
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("pending");
                            Some(format!("[{status}] {content}"))
                        })
                        .collect();
                    if !steps.is_empty() {
                        sink.emit(RunEventKind::PlanUpdated { steps });
                    }
                }
            }
            other if !other.is_empty() => {
                let detail = chunk_text(update)
                    .map(|text| format!("{other}: {text}"))
                    .unwrap_or_else(|| other.to_string());
                sink.emit(RunEventKind::Diagnostic {
                    code: "ACP_UPDATE".into(),
                    detail: truncate(&detail, 200),
                });
            }
            _ => {}
        }
    }

    /// The terminal outcome, once the turn ended.
    pub fn outcome(&self) -> Option<&TerminalOutcome> {
        self.outcome.as_ref()
    }

    /// The live session handle, once known.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// True when the turn has reached a terminal state.
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }
}

/// Extracts text from an ACP content chunk.
fn chunk_text(update: &serde_json::Value) -> Option<String> {
    let content = update.get("content")?;
    if let Some(text) = content.get("text").and_then(|v| v.as_str()) {
        return (!text.is_empty()).then(|| text.to_string());
    }
    if let Some(items) = content.as_array() {
        let joined: String = items
            .iter()
            .filter_map(|i| i.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("");
        return (!joined.is_empty()).then_some(joined);
    }
    None
}

/// Collects absolute paths a tool-call update reports touching.
fn acp_paths(update: &serde_json::Value) -> Vec<String> {
    let Some(locations) = update.get("locations").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    locations
        .iter()
        .filter_map(|l| l.get("path").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

fn parse_usage(usage: Option<&serde_json::Value>) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage::default();
    };
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64());
    TokenUsage {
        input: read("inputTokens").or_else(|| read("input_tokens")),
        output: read("outputTokens").or_else(|| read("output_tokens")),
        cached_input: read("cachedInputTokens"),
        reasoning: read("reasoningTokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectingSink;

    fn method_of(action: &AcpAction) -> String {
        match action {
            AcpAction::Send(msg) => {
                let value: serde_json::Value = serde_json::from_str(&msg.json).expect("json");
                value
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string()
            }
            AcpAction::Idle => String::new(),
        }
    }

    fn params_of(action: &AcpAction) -> serde_json::Value {
        match action {
            AcpAction::Send(msg) => {
                let value: serde_json::Value = serde_json::from_str(&msg.json).expect("json");
                value.get("params").cloned().unwrap_or(json!(null))
            }
            AcpAction::Idle => json!(null),
        }
    }

    fn id_of(action: &AcpAction) -> i64 {
        match action {
            AcpAction::Send(msg) => {
                let value: serde_json::Value = serde_json::from_str(&msg.json).expect("json");
                value.get("id").and_then(|v| v.as_i64()).unwrap_or(-1)
            }
            AcpAction::Idle => -1,
        }
    }

    fn session(resume: Option<&str>, model: Option<&str>) -> AcpSession {
        AcpSession::new(
            "/repo",
            "do the thing",
            resume.map(String::from),
            model.map(String::from),
            vec![],
        )
    }

    #[test]
    fn initialize_declares_only_capabilities_argo_can_service() {
        let mut s = session(None, None);
        let action = s.start();
        assert_eq!(method_of(&action), "initialize");
        let params = params_of(&action);
        assert_eq!(params["protocolVersion"], json!(PROTOCOL_VERSION));
        // Argo does not service fs or terminal requests, so it must not claim them.
        assert_eq!(params["clientCapabilities"]["terminal"], json!(false));
        assert_eq!(
            params["clientCapabilities"]["fs"]["writeTextFile"],
            json!(false)
        );
    }

    #[test]
    fn a_fresh_turn_creates_a_session_then_prompts() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let init = s.start();

        let next = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}})
                .to_string(),
            &mut sink,
        );
        assert_eq!(method_of(&next), "session/new");
        assert_eq!(params_of(&next)["cwd"], json!("/repo"));

        let prompt = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&next),"result":{"sessionId":"sess_abc"}})
                .to_string(),
            &mut sink,
        );
        assert_eq!(method_of(&prompt), "session/prompt");
        assert_eq!(params_of(&prompt)["sessionId"], json!("sess_abc"));
        assert_eq!(
            params_of(&prompt)["prompt"][0]["text"],
            json!("do the thing")
        );
        // Both spellings are sent; Kiro reads `content`, the spec says `prompt`.
        assert_eq!(
            params_of(&prompt)["content"][0]["text"],
            json!("do the thing")
        );

        // The handle is captured so the next turn can resume.
        assert!(sink.events.contains(&RunEventKind::SessionCaptured {
            session_id: SessionId::new("sess_abc")
        }));
    }

    #[test]
    fn a_resumed_turn_loads_the_stored_handle() {
        let mut sink = CollectingSink::default();
        let mut s = session(Some("sess_prev"), None);
        let init = s.start();
        let next = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{"agentCapabilities":{"loadSession":true}}})
                .to_string(),
            &mut sink,
        );
        assert_eq!(method_of(&next), "session/load");
        assert_eq!(params_of(&next)["sessionId"], json!("sess_prev"));
    }

    #[test]
    fn resume_is_skipped_when_the_agent_does_not_advertise_load_session() {
        // Sending session/load to an agent that cannot honor it would fail the
        // turn; falling back to a fresh session keeps the conversation alive.
        let mut sink = CollectingSink::default();
        let mut s = session(Some("sess_prev"), None);
        let init = s.start();
        let next = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{"agentCapabilities":{"loadSession":false}}})
                .to_string(),
            &mut sink,
        );
        assert_eq!(method_of(&next), "session/new");
    }

    #[test]
    fn a_concrete_model_is_selected_before_prompting() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, Some("auto"));
        let init = s.start();
        let new_session = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{"agentCapabilities":{}}})
                .to_string(),
            &mut sink,
        );
        let set_model = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&new_session),"result":{"sessionId":"s1"}})
                .to_string(),
            &mut sink,
        );
        assert_eq!(method_of(&set_model), "session/set_model");
        assert_eq!(params_of(&set_model)["modelId"], json!("auto"));

        let prompt = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&set_model),"result":{}}).to_string(),
            &mut sink,
        );
        assert_eq!(method_of(&prompt), "session/prompt");
    }

    #[test]
    fn streaming_updates_become_normalized_events() {
        let mut sink = CollectingSink::default();
        let mut s = prompting_session();

        s.handle_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"thinking"}}}}).to_string(),
            &mut sink,
        );
        s.handle_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello"}}}}).to_string(),
            &mut sink,
        );
        s.handle_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"fs_write"}}}).to_string(),
            &mut sink,
        );
        s.handle_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","locations":[{"path":"/repo/src/a.rs"}]}}}).to_string(),
            &mut sink,
        );

        assert!(sink.events.contains(&RunEventKind::ThinkingDelta {
            text: "thinking".into()
        }));
        assert!(sink.events.contains(&RunEventKind::TextDelta {
            text: "hello".into()
        }));
        assert!(sink.events.contains(&RunEventKind::FileWritten {
            path: "/repo/src/a.rs".into()
        }));
        assert!(sink
            .events
            .iter()
            .any(|e| matches!(e, RunEventKind::ToolCompleted { ok: true, .. })));
    }

    #[test]
    fn unknown_textual_updates_remain_visible_as_diagnostics() {
        let mut sink = CollectingSink::default();
        let mut s = prompting_session();
        s.handle_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_progress_message","content":{"type":"text","text":"still checking"}}}}).to_string(),
            &mut sink,
        );
        assert!(sink.events.iter().any(|event| matches!(
            event,
            RunEventKind::Diagnostic { code, detail }
                if code == "ACP_UPDATE" && detail.contains("still checking")
        )));
    }

    #[test]
    fn plans_are_surfaced() {
        let mut sink = CollectingSink::default();
        let mut s = prompting_session();
        s.handle_line(
            &json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"plan","entries":[{"content":"read","status":"completed"},{"content":"write","status":"pending"}]}}}).to_string(),
            &mut sink,
        );
        assert!(sink.events.contains(&RunEventKind::PlanUpdated {
            steps: vec!["[completed] read".into(), "[pending] write".into()]
        }));
    }

    /// Drives a session through the handshake so it is awaiting prompt output.
    ///
    /// Updates arriving before the prompt are replayed history and deliberately
    /// ignored, so a test about live streaming has to get past the handshake.
    fn prompting_session() -> AcpSession {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let init = s.start();
        let new_session = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{}}).to_string(),
            &mut sink,
        );
        let after = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&new_session),"result":{"sessionId":"s1"}})
                .to_string(),
            &mut sink,
        );
        // With no concrete model selected this is already the prompt.
        assert_eq!(method_of(&after), "session/prompt");
        s
    }

    #[test]
    fn an_unsupported_request_is_declined_rather_than_ignored() {
        // A request carries an id and blocks until answered; ignoring one strands
        // the agent until it gives up and exits, which is exactly how Kiro failed.
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let action = s.handle_line(
            &json!({"jsonrpc":"2.0","id":7,"method":"_kiro.dev/subagent/create","params":{}})
                .to_string(),
            &mut sink,
        );
        match action {
            AcpAction::Send(msg) => {
                let value: serde_json::Value = serde_json::from_str(&msg.json).expect("json");
                assert_eq!(value["id"], json!(7));
                assert_eq!(value["error"]["code"], json!(-32601));
            }
            AcpAction::Idle => panic!("a request with an id must be answered"),
        }
    }

    #[test]
    fn an_unsupported_notification_is_dropped_silently() {
        // No id means no answer is expected; replying would itself be an error.
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let action = s.handle_line(
            &json!({"jsonrpc":"2.0","method":"_kiro.dev/subagent/list_update","params":{}})
                .to_string(),
            &mut sink,
        );
        assert!(matches!(action, AcpAction::Idle));
    }

    #[test]
    fn permission_requests_are_auto_approved_because_there_is_no_tty() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let action = s.handle_line(
            &json!({"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{"options":[{"optionId":"reject","kind":"reject_once"},{"optionId":"yes","kind":"allow_always"}]}}).to_string(),
            &mut sink,
        );
        match action {
            AcpAction::Send(msg) => {
                let value: serde_json::Value = serde_json::from_str(&msg.json).expect("json");
                assert_eq!(value["id"], json!(42));
                assert_eq!(value["result"]["outcome"]["optionId"], json!("yes"));
            }
            AcpAction::Idle => panic!("must answer the permission request"),
        }
        assert!(sink.events.iter().any(|e| matches!(
            e,
            RunEventKind::Diagnostic { code, .. } if code == "PERMISSION_AUTO_APPROVED"
        )));
    }

    #[test]
    fn prompt_response_ends_the_turn_with_usage() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let init = s.start();
        let new_session = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{}}).to_string(),
            &mut sink,
        );
        let prompt = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&new_session),"result":{"sessionId":"s1"}})
                .to_string(),
            &mut sink,
        );
        s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&prompt),"result":{"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":5}}}).to_string(),
            &mut sink,
        );

        let outcome = s.outcome().expect("outcome");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.usage.input, Some(10));
        assert!(s.is_done());
    }

    #[test]
    fn a_failed_session_load_is_reported_as_a_dead_resume_target() {
        let mut sink = CollectingSink::default();
        let mut s = session(Some("gone"), None);
        let init = s.start();
        let load = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{"agentCapabilities":{"loadSession":true}}}).to_string(),
            &mut sink,
        );
        s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&load),"error":{"code":-32603,"message":"session not found"}}).to_string(),
            &mut sink,
        );
        let outcome = s.outcome().expect("outcome");
        assert!(outcome.resume_target_missing);
        assert!(sink.events.iter().any(|e| matches!(
            e,
            RunEventKind::Error { code, retryable: true, .. } if code == "RESUME_TARGET_MISSING"
        )));
    }

    #[test]
    fn structured_resume_failed_kind_is_recognized() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let init = s.start();
        s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"error":{"code":-1,"message":"nope","data":{"kind":"resume_failed"}}}).to_string(),
            &mut sink,
        );
        assert!(s.outcome().expect("outcome").resume_target_missing);
    }

    #[test]
    fn kiro_native_subagent_updates_report_the_unverified_schema() {
        let mut sink = CollectingSink::default();
        let mut session = session(None, None);
        let action = session.handle_line(
            &json!({
                "jsonrpc": "2.0",
                "method": "_kiro.dev/subagent/list_update",
                "params": {"agents": [{"id": "opaque"}]}
            })
            .to_string(),
            &mut sink,
        );
        assert_eq!(action, AcpAction::Idle);
        assert!(sink.events.iter().any(|event| matches!(
            event,
            RunEventKind::Diagnostic { code, .. }
                if code == "NATIVE_SUBAGENT_UNAVAILABLE"
        )));
        assert!(!sink
            .events
            .iter()
            .any(|event| matches!(event, RunEventKind::ChildSpawned { .. })));
    }

    #[test]
    fn vendor_extension_methods_are_ignored_not_errors() {
        // Kiro sends _kiro.dev/* notifications; they are optional by spec.
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let action = s.handle_line(
            &json!({"jsonrpc":"2.0","method":"_kiro.dev/mcp/server_initialized","params":{}})
                .to_string(),
            &mut sink,
        );
        assert_eq!(action, AcpAction::Idle);
        assert!(sink.events.is_empty());
        assert!(s.outcome().is_none());
    }

    #[test]
    fn cancel_is_only_possible_once_a_session_exists() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        assert!(s.cancel().is_none());

        let init = s.start();
        let new_session = s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&init),"result":{}}).to_string(),
            &mut sink,
        );
        s.handle_line(
            &json!({"jsonrpc":"2.0","id":id_of(&new_session),"result":{"sessionId":"s1"}})
                .to_string(),
            &mut sink,
        );
        let cancel = s.cancel().expect("cancel frame");
        assert!(cancel.json.contains("session/cancel"));
        assert!(cancel.json.contains("s1"));
    }

    #[test]
    fn malformed_stdout_is_a_diagnostic_only() {
        let mut sink = CollectingSink::default();
        let mut s = session(None, None);
        let action = s.handle_line("this is a log line, not json", &mut sink);
        assert_eq!(action, AcpAction::Idle);
        assert!(sink.events.iter().any(|e| matches!(
            e,
            RunEventKind::Diagnostic { code, .. } if code == "UNPARSEABLE_LINE"
        )));
        assert!(s.outcome().is_none());
    }
}
