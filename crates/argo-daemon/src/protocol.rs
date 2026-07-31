//! The daemon IPC contract.
//!
//! Newline-delimited JSON over a user-private Unix socket. Every connection
//! begins with a handshake carrying [`IPC_PROTOCOL_VERSION`]; the daemon refuses
//! a mismatched client rather than risk a stale TUI misreading the event stream.
//!
//! The daemon is local-only and unauthenticated by design, protected by
//! filesystem permissions on the socket rather than by tokens. That is why the
//! socket must live in a directory only the user can reach.

use argo_core::event::{EventSeq, RunEvent};
use argo_core::ids::{AgentId, ConversationId, RunId};
use argo_core::message::ContentBlock;
use argo_core::runtime::{ModelOption, ReasoningOption};
use argo_core::session::SelectionChange;
use argo_runtime::AgentInfo;
use serde::{Deserialize, Serialize};

/// A request from a client to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Negotiate the protocol version. Must be the first message.
    Hello {
        /// Client's protocol version.
        protocol: u32,
        /// Client name, for logs.
        client: String,
    },
    /// Liveness and version check.
    Ping,
    /// List detected agents with models and diagnostics.
    ListAgents {
        /// Re-probe instead of returning the cached inventory.
        refresh: bool,
    },
    /// Open or create the workspace for a directory.
    OpenWorkspace {
        /// Directory to use as the workspace root.
        root: String,
    },
    /// List conversations in a workspace.
    ListConversations {
        /// Workspace root.
        root: String,
    },
    /// Create a conversation.
    NewConversation {
        /// Workspace root.
        root: String,
        /// Optional title.
        title: Option<String>,
    },
    /// Load a conversation's messages.
    GetConversation {
        /// Conversation to load.
        conversation_id: ConversationId,
    },
    /// Record a pending agent/model/reasoning selection.
    ///
    /// Applied at the next turn boundary; never rebinds a running child.
    Select {
        /// Conversation to update.
        conversation_id: ConversationId,
        /// Requested change.
        change: SelectionChange,
    },
    /// Set the execution mode applied at the next turn.
    SetMode {
        /// Conversation to update.
        conversation_id: ConversationId,
        /// Mode identifier, or `None` to return to full access.
        mode: Option<String>,
    },
    /// Preview the exact body the next turn would send.
    ///
    /// Backs `/context`, so a user can see what a switched agent will receive
    /// before committing to the turn.
    PreviewContext {
        /// Conversation to inspect.
        conversation_id: ConversationId,
        /// Prompt the user is about to send.
        prompt: String,
    },
    /// Submit a turn.
    SendMessage {
        /// Conversation to append to.
        conversation_id: ConversationId,
        /// User's message.
        prompt: String,
    },
    /// Cancel a run.
    Cancel {
        /// Run to stop.
        run_id: RunId,
    },
    /// Stream events for a run after a cursor.
    Subscribe {
        /// Run to follow.
        run_id: RunId,
        /// Last sequence the client already has.
        after_seq: EventSeq,
    },
    /// List child conversations spawned by delegation.
    ListChildren {
        /// Parent conversation.
        conversation_id: ConversationId,
    },
    /// Run a task on another CLI as a subagent and wait for its result.
    ///
    /// This is what Argo's delegation MCP tool calls. The child gets its own
    /// conversation, its own upstream session, and a bounded capsule of the
    /// parent's context — so a Claude turn can hand work to Codex without either
    /// one inheriting the other's session state.
    Delegate {
        /// Conversation whose agent is delegating.
        parent_conversation_id: ConversationId,
        /// Adapter to run the child on.
        agent_id: AgentId,
        /// Model for the child, when specified.
        model: Option<String>,
        /// Task handed to the child.
        task: String,
        /// How long to wait before giving up on the child.
        timeout_ms: Option<u64>,
    },
    /// Ask the daemon to shut down.
    Shutdown,
}

/// A response or streamed event from the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Handshake accepted.
    Welcome {
        /// Daemon protocol version.
        protocol: u32,
        /// Daemon build version.
        version: String,
        /// Absolute path of the active database.
        database: String,
    },
    /// Generic acknowledgement.
    Ok,
    /// Agent inventory.
    Agents {
        /// Detected adapters.
        agents: Vec<AgentInfo>,
    },
    /// Workspace opened.
    Workspace {
        /// Canonical root.
        root: String,
        /// Conversations in the workspace, newest first.
        conversations: Vec<ConversationSummary>,
    },
    /// Conversation list.
    Conversations {
        /// Summaries, newest first.
        conversations: Vec<ConversationSummary>,
    },
    /// Conversation contents.
    Conversation {
        /// Conversation metadata.
        summary: ConversationSummary,
        /// Messages in order.
        messages: Vec<MessageView>,
    },
    /// A composed turn body, for `/context`.
    ContextPreview {
        /// Whether the upstream session would be resumed.
        resuming: bool,
        /// Why a fresh session would be started, when applicable.
        reason: Option<String>,
        /// The exact body that would be sent.
        body: String,
    },
    /// A turn was accepted.
    RunStarted {
        /// The new run.
        run_id: RunId,
        /// Agent handling it.
        agent_id: String,
        /// Model in effect.
        model: Option<String>,
        /// True when the upstream session was resumed.
        resumed: bool,
        /// Why canonical context was transferred into a fresh session.
        #[serde(default)]
        context_transfer_reason: Option<String>,
        /// Authoritative metadata after the user turn and assistant placeholder were persisted.
        #[serde(default)]
        conversation: Option<ConversationSummary>,
    },
    /// One streamed run event.
    Event {
        /// The event.
        event: RunEvent,
    },
    /// The subscription reached the run's terminal event.
    StreamEnd {
        /// Run that finished.
        run_id: RunId,
    },
    /// Child conversations.
    Children {
        /// Direct children.
        children: Vec<ConversationSummary>,
    },
    /// A delegated child finished.
    DelegateResult {
        /// The child's conversation.
        conversation_id: ConversationId,
        /// The child's run.
        run_id: RunId,
        /// Adapter that ran it.
        agent_id: String,
        /// True when the child completed successfully.
        ok: bool,
        /// The child's reply, or the failure reason.
        output: String,
    },
    /// A request failed.
    Error {
        /// Stable code.
        code: String,
        /// Human-readable message.
        message: String,
        /// True when retrying may succeed.
        retryable: bool,
    },
}

/// Conversation metadata shown in lists and headers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationSummary {
    /// Conversation id.
    pub id: ConversationId,
    /// Title, when set.
    pub title: Option<String>,
    /// Selected agent for the next turn.
    pub selected_agent_id: Option<String>,
    /// Selected model for the next turn.
    pub selected_model: Option<String>,
    /// Selected reasoning level for the next turn.
    pub selected_reasoning: Option<String>,
    /// Execution mode selected for the next turn.
    pub selected_mode: Option<String>,
    /// Number of messages.
    pub message_count: usize,
    /// Agents that hold a live upstream session here.
    pub agents_with_sessions: Vec<String>,
    /// Parent conversation, for delegated children.
    pub parent_conversation_id: Option<ConversationId>,
    /// Last activity in epoch millis.
    pub updated_at: i64,
}

/// A message rendered for display.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageView {
    /// Message id.
    pub id: String,
    /// Author role.
    pub role: String,
    /// Legacy flattened text for simple clients.
    pub text: String,
    /// Canonical structured blocks for native transcript reconstruction.
    #[serde(default)]
    pub blocks: Vec<ContentBlock>,
    /// Producing agent, for assistant turns.
    pub agent_id: Option<String>,
    /// Producing model, for assistant turns.
    pub model: Option<String>,
    /// Creation time in epoch millis.
    pub created_at: i64,
}

/// Models and reasoning levels offered for an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentChoices {
    /// Selectable models.
    pub models: Vec<ModelOption>,
    /// Selectable reasoning levels.
    pub reasoning: Vec<ReasoningOption>,
}

impl Response {
    /// Builds an error response from an Argo error.
    pub fn from_error(error: &argo_core::error::ArgoError) -> Self {
        Self::Error {
            code: error.code().to_string(),
            message: error.to_string(),
            retryable: error.is_retryable(),
        }
    }

    /// Encodes the response as one newline-terminated JSON line.
    pub fn encode(&self) -> String {
        // Serialization of these types cannot fail; a panic here would mean a
        // non-serializable field was introduced, which is a programming error.
        match serde_json::to_string(self) {
            Ok(json) => format!("{json}\n"),
            Err(error) => format!(
                "{{\"type\":\"error\",\"code\":\"SERDE_ERROR\",\"message\":\"{error}\",\"retryable\":false}}\n"
            ),
        }
    }
}

impl Request {
    /// Parses one line into a request.
    pub fn decode(line: &str) -> argo_core::error::Result<Self> {
        serde_json::from_str(line.trim())
            .map_err(|e| argo_core::error::ArgoError::Invalid(format!("malformed request: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use argo_core::event::{RunEventKind, RunStatus, TokenUsage};

    #[test]
    fn requests_round_trip_with_a_tagged_op() {
        let request = Request::SendMessage {
            conversation_id: ConversationId::new("c1"),
            prompt: "hello".into(),
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert!(json.contains("\"op\":\"send_message\""));
        assert_eq!(Request::decode(&json).expect("decode"), request);
    }

    #[test]
    fn responses_encode_as_one_newline_terminated_line() {
        let encoded = Response::Ok.encode();
        assert!(encoded.ends_with('\n'));
        assert_eq!(encoded.matches('\n').count(), 1);
        assert!(encoded.contains("\"type\":\"ok\""));
    }

    #[test]
    fn run_started_carries_immediate_authoritative_conversation_metadata() {
        let summary = ConversationSummary {
            id: ConversationId::new("c1"),
            title: Some("first task".into()),
            selected_agent_id: Some("codex".into()),
            selected_model: Some("gpt-5".into()),
            selected_reasoning: None,
            selected_mode: None,
            message_count: 2,
            agents_with_sessions: vec![],
            parent_conversation_id: None,
            updated_at: 42,
        };
        let response = Response::RunStarted {
            run_id: RunId::new("r1"),
            agent_id: "codex".into(),
            model: Some("gpt-5".into()),
            resumed: false,
            context_transfer_reason: None,
            conversation: Some(summary.clone()),
        };
        let encoded = response.encode();
        let decoded: Response = serde_json::from_str(encoded.trim()).expect("decode");
        assert_eq!(decoded, response);
        let Response::RunStarted {
            conversation: Some(actual),
            ..
        } = decoded
        else {
            panic!("missing authoritative summary");
        };
        assert_eq!(actual.title, summary.title);
        assert_eq!(actual.message_count, 2);
    }

    #[test]
    fn events_survive_the_wire_intact() {
        let event = RunEvent::new(
            RunId::new("r1"),
            3,
            RunEventKind::RunFinished {
                status: RunStatus::Succeeded,
                usage: TokenUsage {
                    input: Some(10),
                    ..Default::default()
                },
            },
        );
        let response = Response::Event {
            event: event.clone(),
        };
        let line = response.encode();
        let back: Response = serde_json::from_str(line.trim()).expect("decode");
        assert_eq!(back, Response::Event { event });
    }

    #[test]
    fn malformed_requests_are_rejected_with_a_clear_error() {
        let err = Request::decode("{not json}").expect_err("must fail");
        assert_eq!(err.code(), "INVALID_REQUEST");
        assert!(err.to_string().contains("malformed request"));
    }

    #[test]
    fn unknown_ops_are_rejected_rather_than_silently_ignored() {
        let err = Request::decode(r#"{"op":"launch_missiles"}"#).expect_err("must fail");
        assert_eq!(err.code(), "INVALID_REQUEST");
    }

    #[test]
    fn errors_carry_stable_codes_and_retryability() {
        let response = Response::from_error(&argo_core::error::ArgoError::Timeout(500));
        match response {
            Response::Error {
                code, retryable, ..
            } => {
                assert_eq!(code, "TIMEOUT");
                assert!(retryable);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[test]
    fn selection_changes_survive_the_wire() {
        let request = Request::Select {
            conversation_id: ConversationId::new("c1"),
            change: SelectionChange {
                agent_id: Some(argo_core::ids::AgentId::new("codex")),
                model: Some("gpt-5.6".into()),
                reasoning: None,
            },
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert_eq!(Request::decode(&json).expect("decode"), request);
    }

    #[test]
    fn delegation_requests_round_trip() {
        let request = Request::Delegate {
            parent_conversation_id: ConversationId::new("parent"),
            agent_id: AgentId::new("codex"),
            model: Some("gpt-5.6-sol".into()),
            task: "review the diff".into(),
            timeout_ms: Some(600_000),
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert!(json.contains("\"op\":\"delegate\""));
        assert_eq!(Request::decode(&json).expect("decode"), request);
    }

    #[test]
    fn delegation_results_survive_multiline_output() {
        // A child's reply routinely spans lines; the framing must hold.
        let response = Response::DelegateResult {
            conversation_id: ConversationId::new("child"),
            run_id: RunId::new("r1"),
            agent_id: "codex".into(),
            ok: true,
            output: "line one\nline two".into(),
        };
        let encoded = response.encode();
        assert_eq!(encoded.matches('\n').count(), 1);
        let back: Response = serde_json::from_str(encoded.trim()).expect("decode");
        assert_eq!(back, response);
    }

    #[test]
    fn a_response_containing_newlines_in_text_stays_one_line() {
        // Assistant text routinely contains newlines; the framing must survive it.
        let response = Response::ContextPreview {
            resuming: false,
            reason: Some("model_changed".into()),
            body: "line one\nline two\n## user\nline three".into(),
        };
        let encoded = response.encode();
        assert_eq!(encoded.matches('\n').count(), 1, "framing must not break");
        let back: Response = serde_json::from_str(encoded.trim()).expect("decode");
        assert_eq!(back, response);
    }
}
