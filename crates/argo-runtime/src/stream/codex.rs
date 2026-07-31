//! Codex JSONL event-stream parser.
//!
//! Verified against OpenAI's documented `codex exec --json` output: each stdout
//! line is a JSON object with a `type` such as `thread.started`, `turn.started`,
//! `turn.completed`, `turn.failed`, `item.started`, `item.completed`, or `error`.
//! Item types cover agent messages, reasoning, command execution, file changes,
//! MCP tool calls, web searches, and plan updates.

use super::{truncate, StreamSink, TerminalOutcome};
use argo_core::event::{RunEventKind, RunStatus, TokenUsage};
use argo_core::ids::SessionId;

/// Incremental parser for Codex's JSONL stream.
#[derive(Debug, Default)]
pub struct CodexStreamParser {
    outcome: Option<TerminalOutcome>,
    /// True once a session id has been captured, so it is emitted only once.
    session_seen: bool,
    /// Cumulative text already emitted for message items, keyed by item id.
    agent_items: std::collections::HashMap<String, String>,
    /// Cumulative reasoning already emitted for reasoning items.
    reasoning_items: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemPhase {
    Started,
    Updated,
    Completed,
}

impl CodexStreamParser {
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
            sink.emit(RunEventKind::Diagnostic {
                code: "UNPARSEABLE_LINE".into(),
                detail: truncate(trimmed, 200),
            });
            return;
        };

        // OpenCode stamps its own session id on every event; capture it once so a
        // later turn can continue with `--session`.
        if let Some(session) = value.get("sessionID").and_then(|v| v.as_str()) {
            if !self.session_seen {
                self.session_seen = true;
                sink.emit(RunEventKind::SessionCaptured {
                    session_id: SessionId::new(session),
                });
            }
        }

        match value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            // The thread id is the handle `codex exec resume <id>` needs.
            "thread.started" => {
                if let Some(id) = value.get("thread_id").and_then(|v| v.as_str()) {
                    sink.emit(RunEventKind::SessionCaptured {
                        session_id: SessionId::new(id),
                    });
                }
            }
            "item.started" => self.handle_item(&value, ItemPhase::Started, sink),
            "item.updated" => self.handle_item(&value, ItemPhase::Updated, sink),
            "item.completed" => self.handle_item(&value, ItemPhase::Completed, sink),
            // OpenCode's vocabulary. `text` carries assistant output, and
            // `step_finish` terminates the turn with token accounting.
            "text" => {
                if let Some(text) = value
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|v| v.as_str())
                {
                    if !text.is_empty() {
                        sink.emit(RunEventKind::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
            "reasoning" => {
                if let Some(text) = value
                    .get("part")
                    .and_then(|p| p.get("text"))
                    .and_then(|v| v.as_str())
                {
                    if !text.is_empty() {
                        sink.emit(RunEventKind::ThinkingDelta {
                            text: text.to_string(),
                        });
                    }
                }
            }
            "tool" => {
                let part = value.get("part");
                let name = part
                    .and_then(|p| p.get("tool"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let id = part
                    .and_then(|p| p.get("callID"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let state = part
                    .and_then(|p| p.get("state"))
                    .and_then(|s| s.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                match state {
                    "completed" | "error" => sink.emit(RunEventKind::ToolCompleted {
                        id,
                        output: None,
                        ok: state == "completed",
                    }),
                    _ => sink.emit(RunEventKind::ToolStarted {
                        id,
                        name,
                        input: None,
                    }),
                }
            }
            "step_finish" => {
                let usage = value.get("part").and_then(|p| p.get("tokens"));
                self.outcome = Some(TerminalOutcome {
                    status: RunStatus::Succeeded,
                    usage: parse_opencode_usage(usage),
                    resume_target_missing: false,
                    message: None,
                });
            }
            "turn.completed" => {
                self.outcome = Some(TerminalOutcome {
                    status: RunStatus::Succeeded,
                    usage: parse_usage(value.get("usage")),
                    resume_target_missing: false,
                    message: None,
                });
            }
            "turn.failed" => {
                let message = value
                    .get("error")
                    .and_then(|e| e.get("message").and_then(|m| m.as_str()))
                    .map(|s| truncate(s, 500));
                self.outcome = Some(TerminalOutcome {
                    status: RunStatus::Failed,
                    usage: parse_usage(value.get("usage")),
                    resume_target_missing: message
                        .as_deref()
                        .map(is_missing_thread)
                        .unwrap_or(false),
                    message,
                });
            }
            "error" => {
                // Vendors nest the text differently and OpenCode does not put it at
                // the top level, so a single lookup silently lost the real cause and
                // reported a generic string instead. Never discard the payload.
                let extracted = extract_error_message(&value);
                let message = extracted
                    .as_deref()
                    .unwrap_or("the agent reported an error");
                // A missing rollout means the thread Argo asked to resume is gone;
                // the engine clears the handle and reseeds inside the same turn.
                let missing = is_missing_thread(message);
                sink.emit(RunEventKind::Error {
                    code: if missing {
                        "RESUME_TARGET_MISSING".into()
                    } else {
                        "AGENT_ERROR".into()
                    },
                    message: truncate(message, 500),
                    retryable: missing,
                });
                self.outcome = Some(TerminalOutcome {
                    status: RunStatus::Failed,
                    usage: TokenUsage::default(),
                    resume_target_missing: missing,
                    message: Some(truncate(message, 500)),
                });
            }
            _ => {}
        }
    }

    fn handle_item(
        &mut self,
        value: &serde_json::Value,
        phase: ItemPhase,
        sink: &mut dyn StreamSink,
    ) {
        let Some(item) = value.get("item") else {
            return;
        };
        let item_type = item
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("item")
            .to_string();
        let completed = phase == ItemPhase::Completed;

        match item_type {
            "agent_message" => {
                // Newer Codex builds may send cumulative item.updated records.
                // Emit only the unseen suffix, then suppress the identical final
                // item.completed payload.
                if phase != ItemPhase::Started {
                    if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                        if let Some(delta) = unseen_suffix(&mut self.agent_items, &id, text) {
                            sink.emit(RunEventKind::TextDelta { text: delta });
                        }
                    }
                }
            }
            "reasoning" => {
                if phase != ItemPhase::Started {
                    if let Some(text) = item
                        .get("text")
                        .or_else(|| item.get("summary"))
                        .and_then(|v| v.as_str())
                    {
                        if let Some(delta) = unseen_suffix(&mut self.reasoning_items, &id, text) {
                            sink.emit(RunEventKind::ThinkingDelta { text: delta });
                        }
                    }
                }
            }
            "command_execution" => {
                let command = item
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if completed {
                    let ok = item
                        .get("exit_code")
                        .and_then(|v| v.as_i64())
                        .map(|c| c == 0)
                        .unwrap_or(true);
                    let output = item
                        .get("aggregated_output")
                        .or_else(|| item.get("output"))
                        .and_then(|v| v.as_str())
                        .map(|s| truncate(s, 4_000));
                    sink.emit(RunEventKind::ToolCompleted { id, output, ok });
                } else if phase == ItemPhase::Started {
                    sink.emit(RunEventKind::ToolStarted {
                        id,
                        name: "shell".into(),
                        input: Some(truncate(&command, 2_000)),
                    });
                }
            }
            "file_change" => {
                if completed {
                    for path in file_paths(item) {
                        sink.emit(RunEventKind::FileWritten { path });
                    }
                }
            }
            "mcp_tool_call" => {
                let name = item
                    .get("tool")
                    .or_else(|| item.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("mcp")
                    .to_string();
                if completed {
                    let ok = item
                        .get("status")
                        .and_then(|v| v.as_str())
                        .map(|s| s != "failed")
                        .unwrap_or(true);
                    sink.emit(RunEventKind::ToolCompleted {
                        id,
                        output: item.get("result").map(|v| truncate(&v.to_string(), 4_000)),
                        ok,
                    });
                } else if phase == ItemPhase::Started {
                    sink.emit(RunEventKind::ToolStarted {
                        id,
                        name,
                        input: item
                            .get("arguments")
                            .map(|v| truncate(&v.to_string(), 2_000)),
                    });
                }
            }
            "web_search" => {
                if phase == ItemPhase::Started {
                    sink.emit(RunEventKind::ToolStarted {
                        id,
                        name: "web_search".into(),
                        input: item
                            .get("query")
                            .and_then(|v| v.as_str())
                            .map(|s| truncate(s, 500)),
                    });
                }
            }
            "todo_list" | "plan_update" => {
                if let Some(steps) = plan_steps(item) {
                    sink.emit(RunEventKind::PlanUpdated { steps });
                }
            }
            _ => {}
        }
    }

    /// The terminal outcome, once the stream reported one.
    pub fn outcome(&self) -> Option<&TerminalOutcome> {
        self.outcome.as_ref()
    }
}

fn unseen_suffix(
    seen: &mut std::collections::HashMap<String, String>,
    id: &str,
    text: &str,
) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let delta = match seen.get(id) {
        Some(previous) if previous == text => None,
        Some(previous) if text.starts_with(previous) => Some(text[previous.len()..].to_string()),
        // A replacement record is not cumulative. Preserve it rather than
        // silently dropping the final agent message.
        Some(_) | None => Some(text.to_string()),
    };
    seen.insert(id.to_string(), text.to_string());
    delta.filter(|delta| !delta.is_empty())
}

/// Reads OpenCode's nested token shape.
fn parse_opencode_usage(tokens: Option<&serde_json::Value>) -> TokenUsage {
    let Some(tokens) = tokens else {
        return TokenUsage::default();
    };
    TokenUsage {
        input: tokens.get("input").and_then(|v| v.as_u64()),
        output: tokens.get("output").and_then(|v| v.as_u64()),
        cached_input: tokens
            .get("cache")
            .and_then(|c| c.get("read"))
            .and_then(|v| v.as_u64()),
        reasoning: tokens.get("reasoning").and_then(|v| v.as_u64()),
    }
}

/// Pulls a human-readable error out of an event, whatever shape it arrived in.
///
/// Checks the common nestings in order, then falls back to serialising the whole
/// payload: an ugly message that says what happened beats a tidy one that does not.
fn extract_error_message(value: &serde_json::Value) -> Option<String> {
    const DIRECT: &[&str] = &["message", "error", "detail", "reason", "text"];
    for key in DIRECT {
        if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
            if !text.trim().is_empty() {
                return Some(text.to_string());
            }
        }
    }

    // Nested objects: {"error":{"message":...}}, {"data":{"message":...}}, and
    // OpenCode's {"properties":{...}} envelope.
    const NESTED: &[&str] = &["error", "data", "properties", "payload", "result", "info"];
    for key in NESTED {
        if let Some(child) = value.get(key) {
            if child.is_object() {
                if let Some(found) = extract_error_message(child) {
                    return Some(found);
                }
            }
        }
    }

    // Nothing recognisable; serialise so the cause is at least visible.
    let raw = value.to_string();
    (raw.len() > 2).then_some(raw)
}

/// Signatures Codex prints when a resume target's rollout is gone.
fn is_missing_thread(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("no rollout found for thread id")
        || lower.contains("thread/resume failed")
        // OpenCode's wording when `--session` targets a session that is gone.
        || lower.contains("session not found")
        || lower.contains("notfounderror")
}

/// Collects changed paths from a `file_change` item.
fn file_paths(item: &serde_json::Value) -> Vec<String> {
    if let Some(changes) = item.get("changes").and_then(|v| v.as_array()) {
        return changes
            .iter()
            .filter_map(|c| {
                c.get("path")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
    }
    item.get("path")
        .and_then(|v| v.as_str())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default()
}

/// Renders plan or todo items.
fn plan_steps(item: &serde_json::Value) -> Option<Vec<String>> {
    let items = item
        .get("items")
        .or_else(|| item.get("todos"))
        .or_else(|| item.get("steps"))?
        .as_array()?;
    let steps: Vec<String> = items
        .iter()
        .filter_map(|entry| {
            if let Some(text) = entry.as_str() {
                return Some(text.to_string());
            }
            let text = entry
                .get("text")
                .or_else(|| entry.get("content"))
                .or_else(|| entry.get("title"))
                .and_then(|v| v.as_str())?;
            let status = entry
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            Some(format!("[{status}] {text}"))
        })
        .collect();
    (!steps.is_empty()).then_some(steps)
}

/// Reads Codex's usage shape.
fn parse_usage(usage: Option<&serde_json::Value>) -> TokenUsage {
    let Some(usage) = usage else {
        return TokenUsage::default();
    };
    let read = |key: &str| usage.get(key).and_then(|v| v.as_u64());
    TokenUsage {
        input: read("input_tokens"),
        output: read("output_tokens"),
        cached_input: read("cached_input_tokens"),
        reasoning: read("reasoning_output_tokens"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::CollectingSink;

    fn parse(lines: &[&str]) -> (Vec<RunEventKind>, Option<TerminalOutcome>) {
        let mut parser = CodexStreamParser::new();
        let mut sink = CollectingSink::default();
        for line in lines {
            parser.push_line(line, &mut sink);
        }
        (sink.events, parser.outcome().cloned())
    }

    #[test]
    fn opencode_events_are_understood_by_the_shared_parser() {
        // OpenCode uses `sessionID` plus text/tool/step_finish rather than Codex's
        // thread/turn/item vocabulary.
        let (events, outcome) = parse(&[
            r#"{"type":"step_start","sessionID":"ses_abc","part":{"type":"step-start"}}"#,
            r#"{"type":"text","sessionID":"ses_abc","part":{"type":"text","text":"hello from opencode"}}"#,
            r#"{"type":"tool","sessionID":"ses_abc","part":{"callID":"c1","tool":"read","state":{"status":"running"}}}"#,
            r#"{"type":"tool","sessionID":"ses_abc","part":{"callID":"c1","tool":"read","state":{"status":"completed"}}}"#,
            r#"{"type":"step_finish","sessionID":"ses_abc","part":{"tokens":{"input":11,"output":7,"cache":{"read":5}}}}"#,
        ]);

        assert!(events.contains(&RunEventKind::SessionCaptured {
            session_id: SessionId::new("ses_abc")
        }));
        assert!(events.contains(&RunEventKind::TextDelta {
            text: "hello from opencode".into()
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            RunEventKind::ToolStarted { name, .. } if name == "read"
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, RunEventKind::ToolCompleted { ok: true, .. })));

        let outcome = outcome.expect("terminal outcome");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.usage.input, Some(11));
        assert_eq!(outcome.usage.cached_input, Some(5));
    }

    #[test]
    fn the_session_id_is_captured_only_once() {
        // It appears on every OpenCode event; emitting it repeatedly would spam.
        let (events, _) = parse(&[
            r#"{"type":"text","sessionID":"ses_x","part":{"type":"text","text":"a"}}"#,
            r#"{"type":"text","sessionID":"ses_x","part":{"type":"text","text":"b"}}"#,
        ]);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, RunEventKind::SessionCaptured { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_missing_opencode_session_is_flagged_as_a_dead_resume_target() {
        let (_, outcome) = parse(&[r#"{"type":"error","message":"Session not found"}"#]);
        assert!(outcome.expect("outcome").resume_target_missing);
    }

    #[test]
    fn captures_the_thread_id_for_resume() {
        let (events, _) = parse(&[
            r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}"#,
        ]);
        assert_eq!(
            events,
            vec![RunEventKind::SessionCaptured {
                session_id: SessionId::new("0199a213-81c0-7800-8aa1-bbab2a035a53")
            }]
        );
    }

    #[test]
    fn parses_the_documented_happy_path_stream() {
        // This is the exact sample shape from OpenAI's non-interactive docs.
        let (events, outcome) = parse(&[
            r#"{"type":"thread.started","thread_id":"t-1"}"#,
            r#"{"type":"turn.started"}"#,
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc ls","exit_code":0,"aggregated_output":"src\ndocs"}}"#,
            r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Repo contains docs, sdk, and examples directories."}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":24763,"cached_input_tokens":24448,"output_tokens":122,"reasoning_output_tokens":0}}"#,
        ]);

        assert!(events.iter().any(|e| matches!(
            e,
            RunEventKind::ToolStarted { name, .. } if name == "shell"
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, RunEventKind::ToolCompleted { ok: true, .. })));
        assert!(events.contains(&RunEventKind::TextDelta {
            text: "Repo contains docs, sdk, and examples directories.".into()
        }));

        let outcome = outcome.expect("terminal outcome");
        assert_eq!(outcome.status, RunStatus::Succeeded);
        assert_eq!(outcome.usage.input, Some(24763));
        assert_eq!(outcome.usage.cached_input, Some(24448));
        assert_eq!(outcome.usage.output, Some(122));
    }

    #[test]
    fn agent_text_is_emitted_once_not_twice() {
        // `item.started` for a message carries no final text; emitting on both
        // frames would duplicate the reply in the transcript.
        let (events, _) = parse(&[
            r#"{"type":"item.started","item":{"id":"m1","type":"agent_message","text":"partial"}}"#,
            r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"final text"}}"#,
        ]);
        let texts: Vec<&RunEventKind> = events
            .iter()
            .filter(|e| matches!(e, RunEventKind::TextDelta { .. }))
            .collect();
        assert_eq!(texts.len(), 1);
        assert_eq!(
            texts[0],
            &RunEventKind::TextDelta {
                text: "final text".into()
            }
        );
    }

    #[test]
    fn cumulative_message_and_reasoning_updates_emit_only_unseen_suffixes() {
        let (events, _) = parse(&[
            r#"{"type":"item.updated","item":{"id":"m1","type":"agent_message","text":"Hel"}}"#,
            r#"{"type":"item.updated","item":{"id":"m1","type":"agent_message","text":"Hello"}}"#,
            r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"Hello"}}"#,
            r#"{"type":"item.updated","item":{"id":"r1","type":"reasoning","text":"check"}}"#,
            r#"{"type":"item.completed","item":{"id":"r1","type":"reasoning","text":"checking"}}"#,
        ]);
        let text = events
            .iter()
            .filter_map(|event| match event {
                RunEventKind::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let thinking = events
            .iter()
            .filter_map(|event| match event {
                RunEventKind::ThinkingDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "Hello");
        assert_eq!(thinking, "checking");
    }

    #[test]
    fn item_updates_do_not_duplicate_tool_start_or_complete_events() {
        let (events, _) = parse(&[
            r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"cargo test"}}"#,
            r#"{"type":"item.updated","item":{"id":"c1","type":"command_execution","command":"cargo test","aggregated_output":"running"}}"#,
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"cargo test","exit_code":0,"aggregated_output":"ok"}}"#,
        ]);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEventKind::ToolStarted { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RunEventKind::ToolCompleted { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn failing_commands_report_not_ok() {
        let (events, _) = parse(&[
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"false","exit_code":1,"aggregated_output":""}}"#,
        ]);
        assert!(events
            .iter()
            .any(|e| matches!(e, RunEventKind::ToolCompleted { ok: false, .. })));
    }

    #[test]
    fn file_changes_become_file_events() {
        let (events, _) = parse(&[
            r#"{"type":"item.completed","item":{"id":"f1","type":"file_change","changes":[{"path":"src/a.rs","kind":"modified"},{"path":"src/b.rs","kind":"added"}]}}"#,
        ]);
        assert!(events.contains(&RunEventKind::FileWritten {
            path: "src/a.rs".into()
        }));
        assert!(events.contains(&RunEventKind::FileWritten {
            path: "src/b.rs".into()
        }));
    }

    #[test]
    fn mcp_tool_calls_are_normalized_like_any_other_tool() {
        let (events, _) = parse(&[
            r#"{"type":"item.started","item":{"id":"m1","type":"mcp_tool_call","tool":"argo_spawn_agent","arguments":{"agent":"claude"}}}"#,
            r#"{"type":"item.completed","item":{"id":"m1","type":"mcp_tool_call","status":"completed","result":{"ok":true}}}"#,
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            RunEventKind::ToolStarted { name, .. } if name == "argo_spawn_agent"
        )));
        assert!(events
            .iter()
            .any(|e| matches!(e, RunEventKind::ToolCompleted { ok: true, .. })));
    }

    #[test]
    fn plan_updates_are_surfaced() {
        let (events, _) = parse(&[
            r#"{"type":"item.completed","item":{"id":"p1","type":"todo_list","items":[{"text":"read code","status":"completed"},{"text":"write fix","status":"pending"}]}}"#,
        ]);
        assert!(events.contains(&RunEventKind::PlanUpdated {
            steps: vec!["[completed] read code".into(), "[pending] write fix".into()]
        }));
    }

    #[test]
    fn a_missing_rollout_is_flagged_as_a_dead_resume_target() {
        let (events, outcome) = parse(&[
            r#"{"type":"error","message":"thread/resume: thread/resume failed: no rollout found for thread id abc"}"#,
        ]);
        assert!(events.iter().any(|e| matches!(
            e,
            RunEventKind::Error { code, retryable: true, .. } if code == "RESUME_TARGET_MISSING"
        )));
        assert!(outcome.expect("outcome").resume_target_missing);
    }

    #[test]
    fn an_ordinary_error_is_not_treated_as_a_dead_session() {
        let (events, outcome) = parse(&[r#"{"type":"error","message":"rate limited"}"#]);
        assert!(events.iter().any(|e| matches!(
            e,
            RunEventKind::Error { code, retryable: false, .. } if code == "AGENT_ERROR"
        )));
        assert!(!outcome.expect("outcome").resume_target_missing);
    }

    #[test]
    fn turn_failed_records_the_reason() {
        let (_, outcome) =
            parse(&[r#"{"type":"turn.failed","error":{"message":"sandbox denied write"}}"#]);
        let outcome = outcome.expect("outcome");
        assert_eq!(outcome.status, RunStatus::Failed);
        assert_eq!(outcome.message.as_deref(), Some("sandbox denied write"));
    }

    #[test]
    fn partial_and_malformed_lines_do_not_fail_the_turn() {
        let (events, outcome) = parse(&["{\"type\":\"thread.st", "", "plain text noise"]);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, RunEventKind::Diagnostic { .. }))
                .count(),
            2
        );
        assert!(outcome.is_none());
    }

    #[test]
    fn unknown_item_types_are_ignored() {
        let (events, _) =
            parse(&[r#"{"type":"item.completed","item":{"id":"x","type":"some_future_item"}}"#]);
        assert!(events.is_empty());
    }
}
