//! Normalized run events.
//!
//! Every adapter — Claude stream-json, Codex JSONL, Kiro ACP, Grok plain text —
//! is reduced to this one event vocabulary. The TUI and the store never learn
//! which CLI produced a turn, which is what keeps adding an agent a one-file
//! change instead of a UI change.

use crate::ids::{AgentId, RunId, SessionId};
use serde::{Deserialize, Serialize};

/// Monotonic per-run event sequence number.
///
/// Clients reconnect by asking for events after a known sequence, so ordering
/// must be dense and stable.
pub type EventSeq = i64;

/// Terminal and in-flight run states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Created but the child process has not been spawned yet.
    Pending,
    /// Child process is running.
    Running,
    /// Completed successfully.
    Succeeded,
    /// Completed with an error.
    Failed,
    /// Cancelled by the user or shutdown.
    Cancelled,
}

impl RunStatus {
    /// True when no further events can arrive for this run.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// True when this run counts as a completed assistant turn for the purposes
    /// of the resume cursor.
    ///
    /// Failed and cancelled turns deliberately do not advance the cursor: an
    /// agent whose last good turn is still the newest completed turn stays
    /// resumable instead of paying a needless cold reseed.
    pub fn advances_cursor(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Token accounting when a CLI reports it.
///
/// Argo never invents these numbers; absent fields stay `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt tokens.
    pub input: Option<u64>,
    /// Completion tokens.
    pub output: Option<u64>,
    /// Cached prompt tokens.
    pub cached_input: Option<u64>,
    /// Reasoning tokens.
    pub reasoning: Option<u64>,
}

impl TokenUsage {
    /// True when no field carries a value.
    pub fn is_empty(&self) -> bool {
        self.input.is_none()
            && self.output.is_none()
            && self.cached_input.is_none()
            && self.reasoning.is_none()
    }
}

/// The normalized event vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunEventKind {
    /// The run was accepted and a child process is starting.
    RunStarted {
        /// Adapter handling the run.
        agent_id: AgentId,
        /// Resolved model, when the adapter selects one.
        model: Option<String>,
        /// True when Argo continued the CLI's own session.
        resumed: bool,
    },
    /// Incremental assistant prose.
    TextDelta {
        /// Text fragment.
        text: String,
    },
    /// Incremental model reasoning.
    ThinkingDelta {
        /// Reasoning fragment.
        text: String,
    },
    /// A tool call started.
    ToolStarted {
        /// Correlation id.
        id: String,
        /// Tool name.
        name: String,
        /// Bounded arguments rendering.
        input: Option<String>,
    },
    /// A tool call finished.
    ToolCompleted {
        /// Correlation id.
        id: String,
        /// Bounded result rendering.
        output: Option<String>,
        /// False when the tool reported an error.
        ok: bool,
    },
    /// The agent wrote a file.
    FileWritten {
        /// Workspace-relative path.
        path: String,
    },
    /// The agent published or updated a plan or todo list.
    PlanUpdated {
        /// Rendered plan steps.
        steps: Vec<String>,
    },
    /// The upstream CLI disclosed its durable session handle.
    ///
    /// Argo persists this so the next turn on the same agent can resume rather
    /// than replay the transcript.
    SessionCaptured {
        /// Opaque upstream handle.
        session_id: SessionId,
    },
    /// Argo replaced a dead upstream session and reseeded within the same turn.
    ///
    /// Surfaced as a diagnostic rather than an error because the user's turn
    /// still succeeds.
    SessionReseeded {
        /// Why the resume was abandoned.
        reason: String,
    },
    /// A child agent run was spawned by the active agent.
    ChildSpawned {
        /// Child run id.
        child_run_id: RunId,
        /// Adapter chosen for the child.
        child_agent_id: AgentId,
        /// Task handed to the child.
        task: String,
        /// True for a CLI-native child that has no independent Argo run stream.
        #[serde(default)]
        native: bool,
    },
    /// One explicitly emitted event attributed to a child agent.
    ChildEvent {
        /// Child identity announced by [`RunEventKind::ChildSpawned`].
        child_run_id: RunId,
        /// Event emitted by that child. Hidden reasoning is never synthesized.
        event: Box<RunEventKind>,
    },
    /// A child agent run finished.
    ChildCompleted {
        /// Child run id.
        child_run_id: RunId,
        /// Terminal status.
        status: RunStatus,
    },
    /// The primary exhausted its allowance before acting and the same run moved
    /// to its snapshotted standby route.
    BackupFailover {
        /// Exhausted primary adapter.
        from_agent_id: AgentId,
        /// Primary model used for the failed attempt.
        from_model: Option<String>,
        /// Primary reasoning effort used for the failed attempt.
        from_reasoning: Option<String>,
        /// Standby adapter now executing the run.
        to_agent_id: AgentId,
        /// Standby model selected at turn start.
        to_model: Option<String>,
        /// Standby reasoning effort selected at turn start.
        to_reasoning: Option<String>,
        /// Human-readable explanation for transcript surfaces.
        detail: String,
    },
    /// Non-fatal diagnostic information.
    Diagnostic {
        /// Stable code.
        code: String,
        /// Human-readable detail.
        detail: String,
    },
    /// The run failed.
    Error {
        /// Stable code.
        code: String,
        /// Human-readable message.
        message: String,
        /// True when the same request may succeed on retry.
        retryable: bool,
    },
    /// The run reached a terminal state.
    RunFinished {
        /// Terminal status.
        status: RunStatus,
        /// Token accounting when reported.
        usage: TokenUsage,
    },
}

/// A sequenced event belonging to one run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunEvent {
    /// Owning run.
    pub run_id: RunId,
    /// Monotonic position within the run.
    pub seq: EventSeq,
    /// Emission time in epoch millis.
    pub at: i64,
    /// Payload.
    pub kind: RunEventKind,
}

impl RunEvent {
    /// Builds an event.
    pub fn new(run_id: RunId, seq: EventSeq, kind: RunEventKind) -> Self {
        Self {
            run_id,
            seq,
            at: crate::now_millis(),
            kind,
        }
    }

    /// True when this event terminates its run.
    pub fn is_terminal(&self) -> bool {
        matches!(&self.kind, RunEventKind::RunFinished { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_succeeded_advances_the_resume_cursor() {
        // This is the crux of OpenDesign's resume-identity guard: an intervening
        // failed or cancelled turn must not force a cold reseed.
        assert!(RunStatus::Succeeded.advances_cursor());
        assert!(!RunStatus::Failed.advances_cursor());
        assert!(!RunStatus::Cancelled.advances_cursor());
        assert!(!RunStatus::Running.advances_cursor());
    }

    #[test]
    fn terminal_states_are_classified() {
        assert!(RunStatus::Succeeded.is_terminal());
        assert!(RunStatus::Failed.is_terminal());
        assert!(RunStatus::Cancelled.is_terminal());
        assert!(!RunStatus::Pending.is_terminal());
        assert!(!RunStatus::Running.is_terminal());
    }

    #[test]
    fn events_round_trip_through_json_with_tagged_kind() {
        let event = RunEvent::new(
            RunId::new("r1"),
            7,
            RunEventKind::TextDelta { text: "hi".into() },
        );
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"kind\":\"text_delta\""));
        let back: RunEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, event);
    }

    #[test]
    fn attributed_child_events_round_trip_without_becoming_terminal() {
        let event = RunEvent::new(
            RunId::new("parent"),
            3,
            RunEventKind::ChildEvent {
                child_run_id: RunId::new("child"),
                event: Box::new(RunEventKind::ThinkingDelta {
                    text: "emitted reasoning".into(),
                }),
            },
        );
        let json = serde_json::to_string(&event).expect("serialize");
        let back: RunEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, event);
        assert!(!back.is_terminal());
    }

    #[test]
    fn run_finished_is_the_only_terminal_event() {
        let finished = RunEvent::new(
            RunId::new("r1"),
            1,
            RunEventKind::RunFinished {
                status: RunStatus::Succeeded,
                usage: TokenUsage::default(),
            },
        );
        assert!(finished.is_terminal());
        let err = RunEvent::new(
            RunId::new("r1"),
            2,
            RunEventKind::Error {
                code: "X".into(),
                message: "boom".into(),
                retryable: false,
            },
        );
        // An error still requires an explicit RunFinished so the store always
        // records one terminal transition per run.
        assert!(!err.is_terminal());
    }

    #[test]
    fn empty_usage_is_detected() {
        assert!(TokenUsage::default().is_empty());
        assert!(!TokenUsage {
            input: Some(10),
            ..Default::default()
        }
        .is_empty());
    }
}
