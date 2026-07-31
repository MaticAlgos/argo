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
