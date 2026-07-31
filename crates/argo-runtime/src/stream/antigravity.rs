//! Antigravity `--output-format stream-json` parser.
//!
//! The installed `agy` emits JSONL records shaped as:
//!
//! - `{event:"init", conversation_id, init:{tools:[...]}}`
//! - `{event:"step_update", step_update:{step_index,step_type,state,...}}`
//! - `{event:"result", result:{status,response,usage,...}}`
//!
//! Using this format fixes three problems the old plain adapter could not solve:
//! the durable conversation id is captured for resume, tool calls/results are
//! visible in the TUI, and text is streamed rather than appearing only at exit.

use super::{truncate, StreamSink, TerminalOutcome};
use argo_core::event::{RunEventKind, RunStatus, TokenUsage};
use argo_core::ids::SessionId;

/// Incremental Antigravity JSONL parser.
#[derive(Debug, Default)]
pub struct AntigravityStreamParser {
    outcome: Option<TerminalOutcome>,
    streamed_text: bool,
}

impl AntigravityStreamParser {
    /// Creates a parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Consumes one JSONL record.
    pub fn push_line(&mut self, line: &str, sink: &mut dyn StreamSink) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            sink.emit(RunEventKind::Diagnostic {
                code: "UNPARSEABLE_LINE".into(),
                detail: truncate(trimmed, 200),
            });
            return;
        };

        match value
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "init" => {
                if let Some(id) = value.get("conversation_id").and_then(|v| v.as_str()) {
                    sink.emit(RunEventKind::SessionCaptured {
                        session_id: SessionId::new(id),
                    });
                }
            }
            "step_update" => self.handle_step(value.get("step_update"), sink),
            "result" => self.handle_result(value.get("result"), sink),
            _ => {}
        }
    }

    fn handle_step(&mut self, step: Option<&serde_json::Value>, sink: &mut dyn StreamSink) {
        let Some(step) = step else { return };
        let kind = step
            .get("step_type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let state = step
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let id = step
            .get("step_index")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "unknown".into());

        match kind {
            "agent_response" | "assistant_message" | "message" | "model_response"
            | "final_response" => {
                if let Some(text) = step_text(step) {
                    if !text.is_empty() {
                        self.streamed_text = true;
                        sink.emit(RunEventKind::TextDelta { text });
                    }
                }
            }
            // Future/current builds may name explicitly emitted reasoning in
            // several ways. These are wire labels, not inferred chain-of-thought.
            "thinking" | "reasoning" | "analysis" | "agent_thought" | "thought" => {
                if let Some(text) = step_text(step) {
                    if !text.is_empty() {
                        sink.emit(RunEventKind::ThinkingDelta { text });
                    }
                }
            }
            "tool" => {
                let info = step.get("tool_info");
                let name = step
                    .get("tool_name")
                    .or_else(|| info.and_then(|v| v.get("name")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                if state == "ACTIVE" {
                    let input = info
                        .and_then(|v| v.get("parameters"))
                        .map(|v| truncate(&v.to_string(), 2_000));
                    if let Some(path) = written_path(&name, info.and_then(|v| v.get("parameters")))
                    {
                        sink.emit(RunEventKind::FileWritten { path });
                    }
                    sink.emit(RunEventKind::ToolStarted { id, name, input });
                } else if state == "DONE" || state == "ERROR" || state == "FAILED" {
                    let output = info
                        .and_then(|v| v.get("output"))
                        .map(render_value)
                        .map(|v| truncate(&v, 4_000));
                    sink.emit(RunEventKind::ToolCompleted {
                        id,
                        output,
                        ok: state == "DONE",
                    });
                }
            }
            "plan" | "task_boundary" => {
                if let Some(text) = step
                    .get("text")
                    .or_else(|| step.get("description"))
                    .and_then(|v| v.as_str())
                {
                    sink.emit(RunEventKind::PlanUpdated {
                        steps: vec![text.to_string()],
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_result(&mut self, result: Option<&serde_json::Value>, sink: &mut dyn StreamSink) {
        let Some(result) = result else { return };
        let status_text = result
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("ERROR");
        let status = if status_text == "SUCCESS" {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        };

        // `response` repeats streamed agent_response deltas. Use it only if no text
        // arrived incrementally, preserving result-only turns without duplication.
        if !self.streamed_text {
            if let Some(response) = result.get("response").and_then(|v| v.as_str()) {
                if !response.is_empty() {
                    sink.emit(RunEventKind::TextDelta {
                        text: response.to_string(),
                    });
                }
            }
        }

        self.outcome = Some(TerminalOutcome {
            status,
            usage: parse_usage(result.get("usage")),
            resume_target_missing: status == RunStatus::Failed
                && result
                    .get("error")
                    .and_then(|v| v.as_str())
                    .map(is_missing_conversation)
                    .unwrap_or(false),
            message: (status == RunStatus::Failed).then(|| {
                result
                    .get("error")
                    .or_else(|| result.get("response"))
                    .map(render_value)
                    .unwrap_or_else(|| format!("antigravity ended with status {status_text}"))
            }),
        });
    }

    /// Terminal outcome once the result record arrives.
    pub fn outcome(&self) -> Option<&TerminalOutcome> {
        self.outcome.as_ref()
    }
}

fn step_text(step: &serde_json::Value) -> Option<String> {
    for key in ["text_delta", "text", "message", "description"] {
        if let Some(text) = step.get(key).and_then(|value| value.as_str()) {
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }
    }
    step.get("content")
        .and_then(|content| {
            content
                .as_str()
                .or_else(|| content.get("text").and_then(|value| value.as_str()))
        })
        .filter(|text| !text.is_empty())
        .map(String::from)
}

fn parse_usage(value: Option<&serde_json::Value>) -> TokenUsage {
    let get = |key| value.and_then(|v| v.get(key)).and_then(|v| v.as_u64());
    TokenUsage {
        input: get("input_tokens"),
        output: get("output_tokens"),
        cached_input: get("cache_read_tokens"),
        reasoning: get("thinking_tokens"),
    }
}

fn render_value(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| value.to_string())
}

fn is_missing_conversation(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("conversation")
        && (lower.contains("not found")
            || lower.contains("does not exist")
            || lower.contains("unknown"))
}

fn written_path(name: &str, parameters: Option<&serde_json::Value>) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    if !lower.contains("write") && !lower.contains("edit") && !lower.contains("patch") {
        return None;
    }
    let parameters = parameters?.as_object()?;
    for key in ["file_path", "path", "FilePath", "Path", "TargetFile"] {
        if let Some(path) = parameters.get(key).and_then(|v| v.as_str()) {
            return Some(path.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectingSink;

    #[test]
    fn a_real_init_record_captures_the_conversation_id() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"init","conversation_id":"21e361a6-0425-4823-a255-319b79c7d4ad","init":{"tools":["run_command"]}}"#,
            &mut sink,
        );
        assert!(sink.events.iter().any(|event| matches!(
            event,
            RunEventKind::SessionCaptured { session_id }
                if session_id.as_str() == "21e361a6-0425-4823-a255-319b79c7d4ad"
        )));
    }

    #[test]
    fn tool_lifecycle_from_a_real_probe_is_normalized() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"pwd"}}}}"#,
            &mut sink,
        );
        parser.push_line(
            r#"{"event":"step_update","step_update":{"step_index":3,"state":"DONE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"pwd"},"output":"/repo\n"}}}"#,
            &mut sink,
        );
        assert!(matches!(
            &sink.events[0],
            RunEventKind::ToolStarted { name, input: Some(input), .. }
                if name == "run_command" && input.contains("pwd")
        ));
        assert!(matches!(
            &sink.events[1],
            RunEventKind::ToolCompleted { ok: true, output: Some(output), .. }
                if output.contains("/repo")
        ));
    }

    #[test]
    fn explicit_thinking_and_message_aliases_are_not_dropped() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"step_update","step_update":{"step_index":1,"state":"ACTIVE","step_type":"analysis","content":{"text":"checking context"}}}"#,
            &mut sink,
        );
        parser.push_line(
            r#"{"event":"step_update","step_update":{"step_index":2,"state":"ACTIVE","step_type":"assistant_message","message":"answer fragment"}}"#,
            &mut sink,
        );

        assert!(sink.events.contains(&RunEventKind::ThinkingDelta {
            text: "checking context".into()
        }));
        assert!(sink.events.contains(&RunEventKind::TextDelta {
            text: "answer fragment".into()
        }));
    }

    #[test]
    fn streamed_text_is_not_repeated_by_the_result() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"step_update","step_update":{"step_index":2,"state":"ACTIVE","step_type":"agent_response","text_delta":"AGY-PROBE"}}"#,
            &mut sink,
        );
        parser.push_line(
            r#"{"event":"result","result":{"status":"SUCCESS","response":"AGY-PROBE\n","usage":{"input_tokens":10,"output_tokens":2,"thinking_tokens":1,"cache_read_tokens":3}}}"#,
            &mut sink,
        );
        let text = sink
            .events
            .iter()
            .filter(|event| matches!(event, RunEventKind::TextDelta { .. }))
            .count();
        assert_eq!(text, 1);
        let outcome = parser.outcome().expect("outcome");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.usage.input, Some(10));
        assert_eq!(outcome.usage.reasoning, Some(1));
    }

    #[test]
    fn result_only_turns_still_emit_a_reply() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"result","result":{"status":"SUCCESS","response":"done"}}"#,
            &mut sink,
        );
        assert!(sink.events.iter().any(|event| matches!(
            event,
            RunEventKind::TextDelta { text } if text == "done"
        )));
    }

    #[test]
    fn a_missing_resume_target_is_retryable() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"result","result":{"status":"ERROR","error":"conversation not found"}}"#,
            &mut sink,
        );
        assert!(parser.outcome().expect("outcome").resume_target_missing);
    }

    #[test]
    fn write_tools_surface_the_file_path() {
        let mut parser = AntigravityStreamParser::new();
        let mut sink = CollectingSink::default();
        parser.push_line(
            r#"{"event":"step_update","step_update":{"step_index":4,"state":"ACTIVE","step_type":"tool","tool_name":"write_file","tool_info":{"parameters":{"FilePath":"src/main.rs"}}}}"#,
            &mut sink,
        );
        assert!(sink.events.iter().any(|event| matches!(
            event,
            RunEventKind::FileWritten { path } if path == "src/main.rs"
        )));
    }
}
