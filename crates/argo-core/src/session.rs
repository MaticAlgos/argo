//! Native-session continuation policy.
//!
//! Ports OpenDesign's resume-identity guard. A stored upstream session is only
//! safe to continue when the conversation has not changed shape underneath it.
//! Otherwise Argo mints a fresh session and reseeds it from the canonical
//! transcript, which is what makes switching CLI mid-conversation lossless.

use crate::ids::{AgentId, MessageId, SessionId};
use serde::{Deserialize, Serialize};

/// Why a stored session was rejected for reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidationReason {
    /// The session was built under a different model.
    ModelChanged,
    /// The session was built against a different workspace root.
    CwdChanged,
    /// Another agent completed a turn since this session last spoke.
    ConversationAdvanced,
    /// The stored row predates cursor tracking and cannot be verified.
    MissingCursor,
    /// The adapter cannot resume sessions at all.
    Unsupported,
}

impl InvalidationReason {
    /// Short human-readable explanation for the TUI.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::ModelChanged => "model changed since this session was created",
            Self::CwdChanged => "workspace changed since this session was created",
            Self::ConversationAdvanced => "another agent completed a turn in between",
            Self::MissingCursor => "stored session has no verifiable cursor",
            Self::Unsupported => "this agent does not support native session resume",
        }
    }
}

/// A persisted upstream session handle for one `(conversation, agent)` pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSessionRecord {
    /// Adapter that owns the handle.
    pub agent_id: AgentId,
    /// Opaque upstream handle.
    pub session_id: SessionId,
    /// Model in effect when the session was created.
    pub model: Option<String>,
    /// Canonical workspace root in effect when the session was created.
    pub cwd: Option<String>,
    /// Hash of the stable instruction block last sent on this session.
    pub stable_hash: Option<String>,
    /// Id of the last completed assistant message this session produced.
    pub last_message_id: Option<MessageId>,
    /// Last update time in epoch millis.
    pub updated_at: i64,
}

/// What the engine decided to do for the next turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeDecision {
    /// Continue the upstream session and send only the new user turn.
    Resume,
    /// Start a new upstream session and seed it with the canonical transcript.
    FreshWithContext,
}

/// The concrete plan for the next turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResumePlan {
    /// Chosen strategy.
    pub decision: ResumeDecision,
    /// Handle to resume, when resuming.
    pub resume_session_id: Option<SessionId>,
    /// Stored handle even when rejected, for diagnostics.
    pub stored_session_id: Option<SessionId>,
    /// Why a stored handle was rejected, when applicable.
    pub invalidation: Option<InvalidationReason>,
    /// Stable-instruction hash carried by the resumed session.
    pub stored_stable_hash: Option<String>,
}

impl ResumePlan {
    /// True when only the newest user turn should be sent.
    ///
    /// Equivalent to OpenDesign's `skipTranscript`: the upstream session already
    /// holds the history, so replaying it would duplicate context and can make
    /// the model re-answer an earlier turn.
    pub fn skip_transcript(&self) -> bool {
        matches!(self.decision, ResumeDecision::Resume)
    }

    /// Plan that starts fresh for the stated reason.
    pub fn fresh(reason: Option<InvalidationReason>, stored: Option<SessionId>) -> Self {
        Self {
            decision: ResumeDecision::FreshWithContext,
            resume_session_id: None,
            stored_session_id: stored,
            invalidation: reason,
            stored_stable_hash: None,
        }
    }
}

/// Inputs to the guard, gathered at the moment a turn is submitted.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeInputs<'a> {
    /// Stored record, if any.
    pub stored: Option<&'a AgentSessionRecord>,
    /// Whether the adapter can resume at all.
    pub supports_resume: bool,
    /// Model selected for this turn.
    pub current_model: Option<&'a str>,
    /// Canonical workspace root for this turn.
    pub current_cwd: Option<&'a str>,
    /// Newest completed assistant message in the conversation, excluding this
    /// turn's in-flight placeholder.
    pub latest_completed_assistant: Option<&'a MessageId>,
}

/// Applies the resume-identity guard.
///
/// Rejects reuse when the model changed, the workspace changed, the stored
/// cursor is unverifiable, or the conversation advanced under a different agent.
pub fn evaluate_resume(inputs: ResumeInputs<'_>) -> ResumePlan {
    if !inputs.supports_resume {
        return ResumePlan::fresh(Some(InvalidationReason::Unsupported), None);
    }

    let Some(stored) = inputs.stored else {
        // No prior session for this agent: first turn, or the handle was cleared
        // after a failed resume.
        return ResumePlan::fresh(None, None);
    };

    let stored_id = Some(stored.session_id.clone());

    if stored.model.as_deref() != inputs.current_model {
        return ResumePlan::fresh(Some(InvalidationReason::ModelChanged), stored_id);
    }
    if stored.cwd.as_deref() != inputs.current_cwd {
        return ResumePlan::fresh(Some(InvalidationReason::CwdChanged), stored_id);
    }
    let Some(cursor) = stored.last_message_id.as_ref() else {
        return ResumePlan::fresh(Some(InvalidationReason::MissingCursor), stored_id);
    };
    if inputs.latest_completed_assistant != Some(cursor) {
        return ResumePlan::fresh(Some(InvalidationReason::ConversationAdvanced), stored_id);
    }

    ResumePlan {
        decision: ResumeDecision::Resume,
        resume_session_id: stored_id.clone(),
        stored_session_id: stored_id,
        invalidation: None,
        stored_stable_hash: stored.stable_hash.clone(),
    }
}

/// A pending agent or model change requested through a slash command.
///
/// Selection changes are recorded immediately but applied at the next turn
/// boundary: they must never rebind a child process that is already running.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectionChange {
    /// Newly selected adapter, when the user switched agents.
    pub agent_id: Option<AgentId>,
    /// Newly selected model, when the user switched models.
    pub model: Option<String>,
    /// Newly selected reasoning effort, when supported.
    pub reasoning: Option<String>,
}

impl SelectionChange {
    /// True when nothing was actually changed.
    pub fn is_empty(&self) -> bool {
        self.agent_id.is_none() && self.model.is_none() && self.reasoning.is_none()
    }
}

/// Decides whether the stable instruction block must be re-sent.
///
/// Always required on a fresh session. On a resumed session it is skipped only
/// when the composed block is byte-identical to what that session already
/// received, so a legacy row with no stored hash re-sends once.
pub fn include_stable_instructions(
    is_resuming: bool,
    stored_hash: Option<&str>,
    current_hash: &str,
) -> bool {
    !is_resuming || stored_hash != Some(current_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> AgentSessionRecord {
        AgentSessionRecord {
            agent_id: AgentId::new("claude"),
            session_id: SessionId::new("sess-A"),
            model: Some("sonnet".into()),
            cwd: Some("/repo".into()),
            stable_hash: Some("h1".into()),
            last_message_id: Some(MessageId::new("m-1")),
            updated_at: 0,
        }
    }

    fn inputs<'a>(
        stored: Option<&'a AgentSessionRecord>,
        cursor: Option<&'a MessageId>,
    ) -> ResumeInputs<'a> {
        ResumeInputs {
            stored,
            supports_resume: true,
            current_model: Some("sonnet"),
            current_cwd: Some("/repo"),
            latest_completed_assistant: cursor,
        }
    }

    #[test]
    fn resumes_when_identity_is_unchanged() {
        let rec = record();
        let cursor = MessageId::new("m-1");
        let plan = evaluate_resume(inputs(Some(&rec), Some(&cursor)));
        assert_eq!(plan.decision, ResumeDecision::Resume);
        assert!(plan.skip_transcript());
        assert_eq!(plan.resume_session_id, Some(SessionId::new("sess-A")));
        assert_eq!(plan.stored_stable_hash.as_deref(), Some("h1"));
    }

    #[test]
    fn model_change_forces_fresh_session_with_context() {
        // This is the behavior the user asked for explicitly: switching model
        // means the next message carries the remaining context.
        let rec = record();
        let cursor = MessageId::new("m-1");
        let mut i = inputs(Some(&rec), Some(&cursor));
        i.current_model = Some("opus");
        let plan = evaluate_resume(i);
        assert_eq!(plan.decision, ResumeDecision::FreshWithContext);
        assert!(!plan.skip_transcript());
        assert_eq!(plan.invalidation, Some(InvalidationReason::ModelChanged));
        // The rejected handle is still reported for diagnostics.
        assert_eq!(plan.stored_session_id, Some(SessionId::new("sess-A")));
    }

    #[test]
    fn workspace_change_forces_fresh_session() {
        let rec = record();
        let cursor = MessageId::new("m-1");
        let mut i = inputs(Some(&rec), Some(&cursor));
        i.current_cwd = Some("/other");
        let plan = evaluate_resume(i);
        assert_eq!(plan.invalidation, Some(InvalidationReason::CwdChanged));
    }

    #[test]
    fn cross_agent_turn_invalidates_the_stored_session() {
        // Another agent completed a turn, so the cursor moved past m-1.
        let rec = record();
        let moved = MessageId::new("m-4");
        let plan = evaluate_resume(inputs(Some(&rec), Some(&moved)));
        assert_eq!(
            plan.invalidation,
            Some(InvalidationReason::ConversationAdvanced)
        );
        assert!(!plan.skip_transcript());
    }

    #[test]
    fn missing_cursor_reseeds_once() {
        let mut rec = record();
        rec.last_message_id = None;
        let cursor = MessageId::new("m-1");
        let plan = evaluate_resume(inputs(Some(&rec), Some(&cursor)));
        assert_eq!(plan.invalidation, Some(InvalidationReason::MissingCursor));
    }

    #[test]
    fn no_stored_session_starts_fresh_without_blaming_the_user() {
        let plan = evaluate_resume(inputs(None, None));
        assert_eq!(plan.decision, ResumeDecision::FreshWithContext);
        assert_eq!(plan.invalidation, None);
    }

    #[test]
    fn resume_incapable_adapter_always_reseeds() {
        // Grok has no durable session, so every turn is a fresh seed.
        let rec = record();
        let cursor = MessageId::new("m-1");
        let mut i = inputs(Some(&rec), Some(&cursor));
        i.supports_resume = false;
        let plan = evaluate_resume(i);
        assert_eq!(plan.invalidation, Some(InvalidationReason::Unsupported));
        assert!(!plan.skip_transcript());
    }

    #[test]
    fn stable_instructions_resent_only_when_changed() {
        assert!(include_stable_instructions(false, Some("h1"), "h1"));
        assert!(!include_stable_instructions(true, Some("h1"), "h1"));
        assert!(include_stable_instructions(true, Some("h1"), "h2"));
        assert!(include_stable_instructions(true, None, "h1"));
    }

    #[test]
    fn selection_change_emptiness() {
        assert!(SelectionChange::default().is_empty());
        assert!(!SelectionChange {
            model: Some("opus".into()),
            ..Default::default()
        }
        .is_empty());
    }
}
