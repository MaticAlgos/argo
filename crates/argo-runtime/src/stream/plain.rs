//! Plain-text stream handling (Grok).
//!
//! A plain adapter gives Argo no structured events: stdout is the assistant's
//! reply and nothing else. Two consequences are handled here.
//!
//! Argo cannot know which files changed from the stream, so the engine
//! reconciles the workspace after the run. And because a plain CLI can exit `0`
//! while having produced nothing useful, an empty reply is reported as a failure
//! rather than a silent success.

use super::{truncate, StreamSink, TerminalOutcome};
use argo_core::event::{RunEventKind, RunStatus, TokenUsage};

/// Accumulates plain stdout as assistant text.
#[derive(Debug, Default)]
pub struct PlainStreamParser {
    bytes: usize,
    saw_content: bool,
}

/// Upper bound on captured plain output, to bound memory on a runaway CLI.
const MAX_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

impl PlainStreamParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one line of stdout.
    pub fn push_line(&mut self, line: &str, sink: &mut dyn StreamSink) {
        if self.bytes >= MAX_OUTPUT_BYTES {
            return;
        }
        // Blank lines are preserved inside a reply but never start one, so a CLI
        // that pads its output does not read as content.
        if !self.saw_content && line.trim().is_empty() {
            return;
        }
        self.saw_content = true;
        self.bytes += line.len() + 1;
        sink.emit(RunEventKind::TextDelta {
            text: format!("{line}\n"),
        });
    }

    /// True when any content was observed.
    pub fn has_content(&self) -> bool {
        self.saw_content
    }

    /// Derives the terminal outcome from the process result.
    ///
    /// A plain CLI can exit successfully having printed nothing — for example when
    /// a write was silently declined. Treating that as success would show the user
    /// an empty assistant turn with no explanation.
    pub fn finish(&self, exit_ok: bool, stderr: &str) -> TerminalOutcome {
        if !exit_ok {
            return TerminalOutcome {
                status: RunStatus::Failed,
                usage: TokenUsage::default(),
                resume_target_missing: false,
                message: Some(if stderr.trim().is_empty() {
                    "the CLI exited with a non-zero status and no diagnostics".to_string()
                } else {
                    truncate(stderr.trim(), 500)
                }),
            };
        }
        if !self.saw_content {
            return TerminalOutcome {
                status: RunStatus::Failed,
                usage: TokenUsage::default(),
                resume_target_missing: false,
                message: Some(
                    "the CLI exited successfully but produced no output; check its permissions and plan settings"
                        .to_string(),
                ),
            };
        }
        TerminalOutcome::succeeded()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectingSink;

    #[test]
    fn stdout_becomes_assistant_text() {
        let mut parser = PlainStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line("Here is the answer.", &mut sink);
        parser.push_line("Second line.", &mut sink);
        assert_eq!(
            sink.events,
            vec![
                RunEventKind::TextDelta {
                    text: "Here is the answer.\n".into()
                },
                RunEventKind::TextDelta {
                    text: "Second line.\n".into()
                },
            ]
        );
        assert!(parser.has_content());
    }

    #[test]
    fn leading_blank_lines_do_not_count_as_content() {
        let mut parser = PlainStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line("", &mut sink);
        parser.push_line("   ", &mut sink);
        assert!(!parser.has_content());
        assert!(sink.events.is_empty());
    }

    #[test]
    fn blank_lines_inside_a_reply_are_preserved() {
        let mut parser = PlainStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line("para one", &mut sink);
        parser.push_line("", &mut sink);
        parser.push_line("para two", &mut sink);
        assert_eq!(sink.events.len(), 3);
    }

    #[test]
    fn successful_exit_with_output_succeeds() {
        let mut parser = PlainStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line("done", &mut sink);
        let outcome = parser.finish(true, "");
        assert_eq!(outcome.status, RunStatus::Succeeded);
    }

    #[test]
    fn a_silent_success_is_reported_as_a_failure_with_guidance() {
        // Grok exits 0 when a write is permission-cancelled; without this the user
        // would see an empty turn and no reason.
        let parser = PlainStreamParser::new();
        let outcome = parser.finish(true, "");
        assert_eq!(outcome.status, RunStatus::Failed);
        let message = outcome.message.expect("message");
        assert!(message.contains("produced no output"));
        assert!(message.contains("permissions"));
    }

    #[test]
    fn a_nonzero_exit_surfaces_stderr() {
        let parser = PlainStreamParser::new();
        let outcome = parser.finish(false, "error: not authenticated\n");
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(outcome.message.as_deref(), Some("error: not authenticated"));
    }

    #[test]
    fn a_nonzero_exit_without_stderr_still_explains_itself() {
        let parser = PlainStreamParser::new();
        let outcome = parser.finish(false, "   ");
        assert!(outcome
            .message
            .expect("message")
            .contains("non-zero status"));
    }

    #[test]
    fn output_is_bounded_to_protect_memory() {
        let mut parser = PlainStreamParser::new();
        let mut sink = CollectingSink::default();
        let line = "x".repeat(1024);
        for _ in 0..4096 {
            parser.push_line(&line, &mut sink);
        }
        // Emission stops once the cap is reached rather than growing without bound.
        assert!(sink.events.len() < 4096);
        assert!(parser.has_content());
    }

    #[test]
    fn plain_streams_never_claim_a_dead_resume_target() {
        // Grok has no session to resume, so this must never be set.
        let parser = PlainStreamParser::new();
        assert!(!parser.finish(false, "boom").resume_target_missing);
        assert!(!parser.finish(true, "").resume_target_missing);
    }
}
