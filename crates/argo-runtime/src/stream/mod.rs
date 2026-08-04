//! Stream parsing.
//!
//! Each adapter's wire format is reduced to Argo's normalized [`RunEventKind`]
//! vocabulary here. The daemon owns process lifecycle; these parsers are pure
//! state machines over lines, which makes them testable against recorded output
//! without spawning anything.

pub mod acp;
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod plain;

use argo_core::event::{RunEventKind, RunStatus, TokenUsage};

/// Receives normalized events as a parser consumes input.
///
/// `Send` is required because a turn runs on a spawned task.
pub trait StreamSink: Send {
    /// Records one event.
    fn emit(&mut self, event: RunEventKind);
}

/// Collects events in memory. Used by tests and by the delegation bridge.
#[derive(Debug, Default)]
pub struct CollectingSink {
    /// Events in emission order.
    pub events: Vec<RunEventKind>,
}

impl StreamSink for CollectingSink {
    fn emit(&mut self, event: RunEventKind) {
        self.events.push(event);
    }
}

/// How a turn ended, as reported by the CLI's own stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOutcome {
    /// Terminal status.
    pub status: RunStatus,
    /// Token accounting, when reported.
    pub usage: TokenUsage,
    /// True when the failure was specifically "the session I asked to resume is
    /// gone".
    ///
    /// The engine reacts by clearing the stored handle and transparently retrying
    /// the same turn with a full context reseed, so the user still gets an answer.
    pub resume_target_missing: bool,
    /// Bounded failure detail.
    pub message: Option<String>,
}

impl TerminalOutcome {
    /// A successful outcome with no usage reported.
    pub fn succeeded() -> Self {
        Self {
            status: RunStatus::Succeeded,
            usage: TokenUsage::default(),
            resume_target_missing: false,
            message: None,
        }
    }

    /// A failed outcome.
    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed,
            usage: TokenUsage::default(),
            resume_target_missing: false,
            message: Some(message.into()),
        }
    }
}

/// True when a CLI-reported failure is likely transient and safe to retry.
///
/// Coding CLIs frequently encode transport failures as an ordinary failed result
/// rather than a process error. Keep this deliberately conservative: auth
/// rejection, invalid requests, and model errors require user action, while DNS,
/// connection, timeout, rate-limit, and service-unavailable failures may recover.
pub fn is_retryable_failure(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "timed out",
        "timeout",
        "no such host",
        "temporary failure",
        "network is unreachable",
        "connection reset",
        "connection refused",
        "connection closed",
        "broken pipe",
        "econnreset",
        "eai_again",
        "dns",
        "rate limit",
        "too many requests",
        "status 429",
        "service unavailable",
        "status 502",
        "status 503",
        "status 504",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// True when a failure means this CLI's plan or credit is spent.
///
/// Narrower than [`is_retryable_failure`] on purpose, and the two are not
/// interchangeable: a transient rate limit clears on its own and is already
/// handled by the engine's one-shot retry, whereas an exhausted plan will not
/// recover within the conversation. Only the latter justifies handing the turn to
/// a different CLI, so anything ambiguous is deliberately excluded — a false
/// positive silently moves a user onto another vendor's quota.
pub fn is_limit_exhausted(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "usage limit",
        "usage limits",
        "out of credit",
        "insufficient credit",
        "insufficient_quota",
        "quota exceeded",
        "exceeded your quota",
        "exceeded your current quota",
        "weekly limit",
        "monthly limit",
        "upgrade your plan",
        "plan limit",
        "credit balance is too low",
        "no credits remaining",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Truncates `text` to `max` bytes, respecting char boundaries.
///
/// Tool output and error text from a CLI is unbounded in principle, so every
/// parser clamps what it persists and shows.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collecting_sink_preserves_order() {
        let mut sink = CollectingSink::default();
        sink.emit(RunEventKind::TextDelta { text: "a".into() });
        sink.emit(RunEventKind::TextDelta { text: "b".into() });
        assert_eq!(sink.events.len(), 2);
        assert_eq!(sink.events[0], RunEventKind::TextDelta { text: "a".into() });
    }

    #[test]
    fn outcome_constructors_set_expected_defaults() {
        assert_eq!(TerminalOutcome::succeeded().status, RunStatus::Succeeded);
        assert!(!TerminalOutcome::succeeded().resume_target_missing);
        let failed = TerminalOutcome::failed("boom");
        assert_eq!(failed.status, RunStatus::Failed);
        assert_eq!(failed.message.as_deref(), Some("boom"));
    }

    #[test]
    fn retryability_is_limited_to_transient_transport_failures() {
        assert!(is_retryable_failure(
            "dial tcp: lookup api.example.com: no such host"
        ));
        assert!(is_retryable_failure("authentication request timed out"));
        assert!(is_retryable_failure("HTTP status 503 service unavailable"));
        assert!(!is_retryable_failure("invalid API key"));
        assert!(!is_retryable_failure("model does not exist"));
    }

    #[test]
    fn exhaustion_is_recognized_from_how_the_clis_actually_word_it() {
        assert!(is_limit_exhausted(
            "Claude usage limit reached. Your limit will reset at 3pm."
        ));
        assert!(is_limit_exhausted(
            "You exceeded your current quota, please check your plan and billing details"
        ));
        assert!(is_limit_exhausted(
            "Your credit balance is too low to access the Anthropic API"
        ));
        assert!(is_limit_exhausted("You've hit your weekly limit for Opus"));
    }

    #[test]
    fn generic_non_billing_limits_are_not_exhaustion() {
        assert!(!is_limit_exhausted("tool call limit reached for this turn"));
        assert!(!is_limit_exhausted("recursion limit reached"));
        assert!(!is_limit_exhausted(
            "you have reached your limit of 100 files per request"
        ));
        assert!(!is_limit_exhausted("maximum token limit reached"));
    }

    #[test]
    fn transient_failures_are_never_treated_as_exhaustion() {
        // These recover on their own and are already covered by the engine's
        // one-shot retry. Failing over on them would move the user onto another
        // vendor's quota for what was a blip.
        assert!(!is_limit_exhausted("connection reset by peer"));
        assert!(!is_limit_exhausted("HTTP status 429 too many requests"));
        assert!(!is_limit_exhausted("rate limit exceeded, retry after 20s"));
        assert!(!is_limit_exhausted("HTTP status 503 service unavailable"));
        // Neither is an ordinary error the user must act on.
        assert!(!is_limit_exhausted("invalid API key"));
    }

    #[test]
    fn truncate_leaves_short_text_untouched() {
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn truncate_marks_clipped_text() {
        let out = truncate(&"x".repeat(50), 10);
        assert!(out.starts_with("xxxxxxxxxx"));
        assert!(out.ends_with("… [truncated]"));
    }

    #[test]
    fn truncate_respects_multibyte_boundaries() {
        // Slicing mid-codepoint would panic; this must not.
        let out = truncate(&"é".repeat(100), 51);
        assert!(out.ends_with("… [truncated]"));
    }
}
