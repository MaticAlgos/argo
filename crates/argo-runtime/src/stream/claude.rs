//! Claude `stream-json` parser.
//!
//! Claude emits one JSON object per line. Argo cares about a small subset and
//! must ignore the rest forward-compatibly: a new record type in a future build
//! should not fail a turn.
//!
//! Two behaviors here are load-bearing and both come from OpenDesign's
//! experience with this stream:
//!
//! - The `session_id` on the init record is what makes resume possible later.
//! - Frames carrying a non-null `parent_tool_use_id` belong to a *nested* agent.
//!   Their content is surfaced, but their completion must not finish the parent
//!   run, or a subagent's exit would truncate the real turn.

use super::{truncate, StreamSink, TerminalOutcome};
use argo_core::event::{RunEventKind, RunStatus, TokenUsage};
use argo_core::ids::{AgentId, RunId, SessionId};
use std::collections::HashMap;

/// Incremental parser for Claude's stream-json.
#[derive(Debug, Default)]
pub struct ClaudeStreamParser {
    outcome: Option<TerminalOutcome>,
    /// True once parent assistant text was streamed from a message block.
    streamed_text: bool,
    /// Native subagent tool metadata keyed by Claude's parent tool-use id.
    native_tools: HashMap<String, (AgentId, String)>,
    /// Native children already announced, with whether they emitted prose.
    native_children: HashMap<String, bool>,
}

impl ClaudeStreamParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one line, emitting normalized events into `sink`.
    pub fn push_line(&mut self, line: &str, sink: &mut dyn StreamSink) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            // Non-JSON output on stdout is diagnostic noise, not a turn failure.
            sink.emit(RunEventKind::Diagnostic {
                code: "UNPARSEABLE_LINE".into(),
                detail: truncate(trimmed, 200),
            });
            return;
        };

        let record_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let nested_id = value
            .get("parent_tool_use_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        match record_type {
            "system" => {
                if let Some(session) = value.get("session_id").and_then(|v| v.as_str()) {
                    sink.emit(RunEventKind::SessionCaptured {
                        session_id: SessionId::new(session),
                    });
                }
            }
            "assistant" | "user" => match nested_id.as_deref() {
                Some(child_id) => self.handle_nested_message(&value, child_id, sink),
                None => self.handle_message(&value, false, sink),
            },
            "result" => match nested_id.as_deref() {
                Some(child_id) => self.handle_nested_result(&value, child_id, sink),
                None => self.handle_result(&value, sink),
            },
            _ => {}
        }
    }

    fn handle_message(
        &mut self,
        value: &serde_json::Value,
        nested: bool,
        sink: &mut dyn StreamSink,
    ) {
        let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            return;
        };

        for block in content {
            let kind = block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            match kind {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            self.streamed_text = true;
                            sink.emit(RunEventKind::TextDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "thinking" => {
                    if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            sink.emit(RunEventKind::ThinkingDelta {
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let input = block.get("input").map(|v| truncate(&v.to_string(), 2_000));

                    if matches!(name.as_str(), "Agent" | "Task") {
                        let raw = block.get("input");
                        let agent = raw
                            .and_then(|value| value.get("subagent_type"))
                            .and_then(|value| value.as_str())
                            .filter(|value| !value.trim().is_empty())
                            .map(|value| AgentId::new(format!("claude/{value}")))
                            .unwrap_or_else(|| AgentId::new("claude-native"));
                        let task = raw
                            .and_then(|value| {
                                value
                                    .get("description")
                                    .or_else(|| value.get("prompt"))
                                    .or_else(|| value.get("task"))
                            })
                            .and_then(|value| value.as_str())
                            .filter(|value| !value.trim().is_empty())
                            .unwrap_or("task unavailable from Claude's emitted stream")
                            .to_string();
                        self.native_tools.insert(id.clone(), (agent, task));
                    }

                    // File-producing tools also surface as explicit file events so
                    // the TUI and the store agree on what changed.
                    if let Some(path) = file_path_from_tool(&name, block.get("input")) {
                        sink.emit(RunEventKind::FileWritten { path });
                    }
                    if name == "TodoWrite" {
                        if let Some(steps) = todo_steps(block.get("input")) {
                            sink.emit(RunEventKind::PlanUpdated { steps });
                        }
                    }
                    sink.emit(RunEventKind::ToolStarted { id, name, input });
                }
                "tool_result" => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let ok = !block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let output = block
                        .get("content")
                        .map(|v| truncate(&render_content(v), 4_000));
                    sink.emit(RunEventKind::ToolCompleted { id, output, ok });
                }
                _ => {}
            }
        }

        if nested {
            sink.emit(RunEventKind::Diagnostic {
                code: "NESTED_AGENT_ACTIVITY".into(),
                detail: "output from a nested agent invoked by this turn".into(),
            });
        }
    }

    fn ensure_native_child(&mut self, child_id: &str, sink: &mut dyn StreamSink) -> RunId {
        let run_id = RunId::new(format!("claude-native-{child_id}"));
        if !self.native_children.contains_key(child_id) {
            let (agent_id, task) = self.native_tools.get(child_id).cloned().unwrap_or_else(|| {
                (
                    AgentId::new("claude-native"),
                    "task unavailable from Claude's emitted stream".to_string(),
                )
            });
            self.native_children.insert(child_id.to_string(), false);
            sink.emit(RunEventKind::ChildSpawned {
                child_run_id: run_id.clone(),
                child_agent_id: agent_id,
                task,
                native: true,
            });
        }
        run_id
    }

    fn emit_native_child(sink: &mut dyn StreamSink, child_run_id: &RunId, event: RunEventKind) {
        sink.emit(RunEventKind::ChildEvent {
            child_run_id: child_run_id.clone(),
            event: Box::new(event),
        });
    }

    fn handle_nested_message(
        &mut self,
        value: &serde_json::Value,
        child_id: &str,
        sink: &mut dyn StreamSink,
    ) {
        let child_run_id = self.ensure_native_child(child_id, sink);
        let Some(content) = value
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_array())
        else {
            return;
        };

        for block in content {
            let kind = block
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            match kind {
                "text" => {
                    if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                        if !text.is_empty() {
                            self.native_children.insert(child_id.to_string(), true);
                            Self::emit_native_child(
                                sink,
                                &child_run_id,
                                RunEventKind::TextDelta {
                                    text: text.to_string(),
                                },
                            );
                        }
                    }
                }
                "thinking" => {
                    if let Some(text) = block.get("thinking").and_then(|value| value.as_str()) {
                        if !text.is_empty() {
                            Self::emit_native_child(
                                sink,
                                &child_run_id,
                                RunEventKind::ThinkingDelta {
                                    text: text.to_string(),
                                },
                            );
                        }
                    }
                }
                "tool_use" => {
                    let id = block
                        .get("id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let input = block
                        .get("input")
                        .map(|value| truncate(&value.to_string(), 2_000));
                    if let Some(path) = file_path_from_tool(&name, block.get("input")) {
                        Self::emit_native_child(
                            sink,
                            &child_run_id,
                            RunEventKind::FileWritten { path },
                        );
                    }
                    if name == "TodoWrite" {
                        if let Some(steps) = todo_steps(block.get("input")) {
                            Self::emit_native_child(
                                sink,
                                &child_run_id,
                                RunEventKind::PlanUpdated { steps },
                            );
                        }
                    }
                    Self::emit_native_child(
                        sink,
                        &child_run_id,
                        RunEventKind::ToolStarted { id, name, input },
                    );
                }
                "tool_result" => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let ok = !block
                        .get("is_error")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false);
                    let output = block
                        .get("content")
                        .map(|value| truncate(&render_content(value), 4_000));
                    Self::emit_native_child(
                        sink,
                        &child_run_id,
                        RunEventKind::ToolCompleted { id, output, ok },
                    );
                }
                _ => {}
            }
        }
    }

    fn handle_nested_result(
        &mut self,
        value: &serde_json::Value,
        child_id: &str,
        sink: &mut dyn StreamSink,
    ) {
        let child_run_id = self.ensure_native_child(child_id, sink);
        let streamed = self.native_children.get(child_id).copied().unwrap_or(false);
        if !streamed {
            if let Some(text) = value.get("result").and_then(|value| value.as_str()) {
                if !text.is_empty() {
                    Self::emit_native_child(
                        sink,
                        &child_run_id,
                        RunEventKind::TextDelta {
                            text: text.to_string(),
                        },
                    );
                }
            }
        }
        let status = if value
            .get("is_error")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            RunStatus::Failed
        } else {
            RunStatus::Succeeded
        };
        sink.emit(RunEventKind::ChildCompleted {
            child_run_id,
            status,
        });
    }

    fn handle_result(&mut self, value: &serde_json::Value, sink: &mut dyn StreamSink) {
        let is_error = value
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let usage = parse_usage(value.get("usage"));

        if is_error {
            let turns = value
                .get("num_turns")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            let api_ms = value
                .get("duration_api_ms")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            // A resume whose target session is gone fails locally, before any API
            // call: zero turns and zero API time. That signature is stable across
            // builds, whereas the human-readable message is not, so it is the
            // primary detector for a dead handle.
            let resume_failure = turns == 0 && api_ms == 0;
            self.outcome = Some(TerminalOutcome {
                status: RunStatus::Failed,
                usage,
                resume_target_missing: resume_failure,
                message: value
                    .get("result")
                    .and_then(|v| v.as_str())
                    .map(|s| truncate(s, 500)),
            });
        } else {
            // Only use the result text when nothing was streamed, so a normal turn
            // is not duplicated but a result-only turn still produces a reply.
            if !self.streamed_text {
                if let Some(text) = value.get("result").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        sink.emit(RunEventKind::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
            self.outcome = Some(TerminalOutcome {
                status: RunStatus::Succeeded,
                usage,
                resume_target_missing: false,
                message: None,
            });
        }
    }

    /// The terminal outcome, once the stream reported one.
    pub fn outcome(&self) -> Option<&TerminalOutcome> {
        self.outcome.as_ref()
    }
}

/// Extracts a written path from a file-producing tool call.
fn file_path_from_tool(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    if !matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
        return None;
    }
    input?
        .get("file_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Extracts todo text from a TodoWrite call.
fn todo_steps(input: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let todos = input?.get("todos")?.as_array()?;
    let steps: Vec<String> = todos
        .iter()
        .filter_map(|t| {
            let content = t.get("content").and_then(|v| v.as_str())?;
            let status = t
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            Some(format!("[{status}] {content}"))
        })
        .collect();
    (!steps.is_empty()).then_some(steps)
}

/// Renders tool-result content, which may be a string or an array of blocks.
fn render_content(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Reads token accounting, leaving absent fields as `None`.
pub(super) fn parse_usage(usage: Option<&serde_json::Value>) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage::default();
    };
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64());
    TokenUsage {
        input: read("input_tokens"),
        output: read("output_tokens"),
        cached_input: read("cache_read_input_tokens").or_else(|| read("cached_input_tokens")),
        reasoning: read("reasoning_output_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectingSink;

    fn parse(lines: &[&str]) -> (Vec<RunEventKind>, Option<TerminalOutcome>) {
        let mut parser = ClaudeStreamParser::new();
        let mut sink = CollectingSink::default();
        for line in lines {
            parser.push_line(line, &mut sink);
        }
        (sink.events, parser.outcome().cloned())
    }

    #[test]
    fn captures_the_session_id_so_the_next_turn_can_resume() {
        let (events, _) =
            parse(&[r#"{"type":"system","subtype":"init","session_id":"sess-abc","tools":[]}"#]);
        assert_eq!(
            events,
            vec![RunEventKind::SessionCaptured {
                session_id: SessionId::new("sess-abc")
            }]
        );
    }

    #[test]
    fn emits_text_and_thinking_separately() {
        let (events, _) = parse(&[
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"considering"},{"type":"text","text":"Here it is."}]}}"#,
        ]);
        assert_eq!(
            events,
            vec![
                RunEventKind::ThinkingDelta {
                    text: "considering".into()
                },
                RunEventKind::TextDelta {
                    text: "Here it is.".into()
                },
            ]
        );
    }

    #[test]
    fn tool_use_produces_a_tool_event_and_a_file_event() {
        let (events, _) = parse(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"src/main.rs","content":"fn main(){}"}}]}}"#,
        ]);
        assert!(events.contains(&RunEventKind::FileWritten {
            path: "src/main.rs".into()
        }));
        assert!(matches!(
            &events[1],
            RunEventKind::ToolStarted { id, name, .. } if id == "t1" && name == "Write"
        ));
    }

    #[test]
    fn tool_results_report_success_and_failure() {
        let (events, _) = parse(&[
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"done"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":[{"type":"text","text":"boom"}]}]}}"#,
        ]);
        assert_eq!(
            events[0],
            RunEventKind::ToolCompleted {
                id: "t1".into(),
                output: Some("done".into()),
                ok: true
            }
        );
        assert_eq!(
            events[1],
            RunEventKind::ToolCompleted {
                id: "t2".into(),
                output: Some("boom".into()),
                ok: false
            }
        );
    }

    #[test]
    fn todo_writes_become_plan_updates() {
        let (events, _) = parse(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"TodoWrite","input":{"todos":[{"content":"write tests","status":"in_progress"},{"content":"ship","status":"pending"}]}}]}}"#,
        ]);
        assert_eq!(
            events[0],
            RunEventKind::PlanUpdated {
                steps: vec!["[in_progress] write tests".into(), "[pending] ship".into()]
            }
        );
    }

    #[test]
    fn successful_result_completes_the_run_with_usage() {
        let (_, outcome) = parse(&[
            r#"{"type":"result","is_error":false,"result":"All done.","num_turns":3,"duration_api_ms":1200,"usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":80}}"#,
        ]);
        let outcome = outcome.expect("terminal outcome");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.usage.input, Some(100));
        assert_eq!(outcome.usage.cached_input, Some(80));
        assert!(!outcome.resume_target_missing);
    }

    #[test]
    fn streamed_text_is_not_duplicated_by_the_result_record() {
        // Claude repeats the final reply in `result`. Emitting both produced a
        // visibly doubled answer in the first real run against the CLI.
        let (events, outcome) = parse(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"the answer"}]}}"#,
            r#"{"type":"result","is_error":false,"result":"the answer","num_turns":1,"duration_api_ms":10}"#,
        ]);
        let texts: Vec<&RunEventKind> = events
            .iter()
            .filter(|e| matches!(e, RunEventKind::TextDelta { .. }))
            .collect();
        assert_eq!(texts.len(), 1, "reply must appear exactly once");
        assert_eq!(outcome.expect("outcome").status, RunStatus::Succeeded);
    }

    #[test]
    fn a_result_only_turn_still_yields_its_text() {
        // Some invocations emit no assistant block at all; the reply must survive.
        let (events, _) = parse(&[
            r#"{"type":"result","is_error":false,"result":"only here","num_turns":1,"duration_api_ms":10}"#,
        ]);
        assert!(events.contains(&RunEventKind::TextDelta {
            text: "only here".into()
        }));
    }

    #[test]
    fn a_dead_resume_target_is_detected_structurally_not_by_prose() {
        // Zero turns and zero API time means it failed locally before any model
        // call: the session could not be loaded. Prose wording drifts between
        // builds, so this shape is the reliable signal.
        let (_, outcome) = parse(&[
            r#"{"type":"result","is_error":true,"num_turns":0,"duration_api_ms":0,"result":"No conversation found with session ID: x"}"#,
        ]);
        let outcome = outcome.expect("terminal outcome");
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(outcome.resume_target_missing);
    }

    #[test]
    fn a_transient_api_failure_is_not_mistaken_for_a_dead_session() {
        // Real API time was spent, so the handle is probably fine; dropping it
        // would throw away a valid session over a network blip.
        let (_, outcome) = parse(&[
            r#"{"type":"result","is_error":true,"num_turns":2,"duration_api_ms":5000,"result":"overloaded"}"#,
        ]);
        let outcome = outcome.expect("terminal outcome");
        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(!outcome.resume_target_missing);
    }

    #[test]
    fn nested_agent_output_does_not_terminate_the_parent_run() {
        // A subagent's own `result` frame must not end the real turn.
        let (events, outcome) = parse(&[
            r#"{"type":"result","parent_tool_use_id":"t1","is_error":false,"result":"child done"}"#,
        ]);
        assert!(
            outcome.is_none(),
            "nested result must not finish the parent"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            RunEventKind::ChildCompleted {
                status: RunStatus::Succeeded,
                ..
            }
        )));
    }

    #[test]
    fn nested_agent_output_is_attributed_without_becoming_parent_prose() {
        let (events, _) = parse(&[
            r#"{"type":"assistant","parent_tool_use_id":"t1","message":{"content":[{"type":"text","text":"child text"}]}}"#,
        ]);
        assert!(events
            .iter()
            .any(|event| matches!(event, RunEventKind::ChildSpawned { native: true, .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            RunEventKind::ChildEvent { event, .. }
                if matches!(event.as_ref(), RunEventKind::TextDelta { text } if text == "child text")
        )));
        assert!(!events
            .iter()
            .any(|event| matches!(event, RunEventKind::TextDelta { .. })));
    }

    #[test]
    fn malformed_lines_become_diagnostics_rather_than_failures() {
        let (events, outcome) = parse(&["not json at all", "", "   "]);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            RunEventKind::Diagnostic { code, .. } if code == "UNPARSEABLE_LINE"
        ));
        assert!(outcome.is_none());
    }

    #[test]
    fn unknown_record_types_are_ignored_for_forward_compatibility() {
        let (events, outcome) = parse(&[r#"{"type":"some_future_record","payload":{}}"#]);
        assert!(events.is_empty());
        assert!(outcome.is_none());
    }

    #[test]
    fn oversized_tool_output_is_truncated() {
        let big = "x".repeat(10_000);
        let line = format!(
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1","content":"{big}"}}]}}}}"#
        );
        let (events, _) = parse(&[&line]);
        match &events[0] {
            RunEventKind::ToolCompleted { output, .. } => {
                let output = output.as_ref().expect("output");
                assert!(output.len() < 5_000);
                assert!(output.ends_with("… [truncated]"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
