//! Canonical conversation messages.
//!
//! Argo's transcript is the authority, not any upstream CLI's store. That is
//! what makes switching agents mid-conversation possible: a fresh session on a
//! different CLI is seeded from these rows.

use crate::event::RunStatus;
use crate::ids::{AgentId, MessageId, RunId};
use serde::{Deserialize, Serialize};

/// Who produced a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A human turn.
    User,
    /// An agent turn.
    Assistant,
    /// A durable instruction recording newly effective context.
    ///
    /// Mirrors OpenCode's mid-conversation system message: when the active
    /// skills, model, or workspace facts change, the change is appended to
    /// history instead of silently rewriting the baseline.
    System,
}

impl Role {
    /// Marker used when flattening a transcript for a fresh session.
    pub fn marker(&self) -> &'static str {
        match self {
            Self::User => "## user",
            Self::Assistant => "## assistant",
            Self::System => "## system",
        }
    }
}

/// Lifecycle state of a tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Started but not finished.
    Pending,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
}

/// One tool invocation observed on a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Adapter-scoped call id, used to correlate start and completion.
    pub id: String,
    /// Tool name as reported by the CLI.
    pub name: String,
    /// Bounded rendering of the arguments.
    pub input: Option<String>,
    /// Bounded rendering of the result.
    pub output: Option<String>,
    /// Current status.
    pub status: ToolStatus,
}

/// A typed fragment of message content.
///
/// Keeping content structured (rather than one flat string) lets Argo replay a
/// transcript to a different CLI while deciding per-block what to include:
/// reasoning is dropped across model families, tool results are summarized, and
/// text is preserved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Ordinary visible prose.
    Text {
        /// The text body.
        text: String,
    },
    /// Model reasoning. Never replayed verbatim across model families.
    Thinking {
        /// The reasoning body.
        text: String,
    },
    /// A tool invocation and its bounded result.
    Tool {
        /// The call.
        call: ToolCall,
    },
    /// A file the agent created or modified.
    FileWrite {
        /// Workspace-relative path.
        path: String,
    },
    /// An Argo-managed child run linked to this assistant turn.
    ChildActivity {
        /// Durable child run id; its full transcript lives in the child conversation.
        run_id: RunId,
        /// CLI adapter that handled the child.
        agent_id: AgentId,
        /// Self-contained task handed to the child.
        task: String,
        /// Terminal status once the child's commit barrier was observed.
        status: Option<RunStatus>,
        /// Ordered content explicitly emitted by a CLI-native child.
        #[serde(default)]
        blocks: Vec<ContentBlock>,
    },
}

impl ContentBlock {
    /// Convenience constructor for a text block.
    pub fn text(body: impl Into<String>) -> Self {
        Self::Text { text: body.into() }
    }

    /// True when this block should be replayed when seeding a different agent.
    ///
    /// Reasoning is excluded: providers reject or mis-handle foreign reasoning
    /// signatures, and OpenCode's context contract likewise only replays opaque
    /// continuation metadata on an exact provider/model match.
    pub fn is_transferable(&self) -> bool {
        !matches!(self, Self::Thinking { .. })
    }
}

/// One persisted conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Stable row id.
    pub id: MessageId,
    /// Author.
    pub role: Role,
    /// Ordered content blocks.
    pub blocks: Vec<ContentBlock>,
    /// Agent that produced an assistant message, when known.
    pub agent_id: Option<AgentId>,
    /// Model that produced an assistant message, when known.
    pub model: Option<String>,
    /// Run that produced the message, when it came from an agent.
    pub run_id: Option<RunId>,
    /// Monotonic position within the conversation.
    pub seq: i64,
    /// Creation time in epoch millis.
    pub created_at: i64,
}

impl Message {
    /// Concatenates the transferable text of this message.
    ///
    /// Tool and file blocks are rendered as compact annotations so a receiving
    /// agent learns what already happened without being handed a foreign tool
    /// schema it cannot execute.
    pub fn transferable_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for block in self.blocks.iter().filter(|b| b.is_transferable()) {
            match block {
                ContentBlock::Text { text } => {
                    if !text.trim().is_empty() {
                        parts.push(text.trim().to_string());
                    }
                }
                ContentBlock::Thinking { .. } => {}
                ContentBlock::Tool { call } => {
                    let status = match call.status {
                        ToolStatus::Pending => "pending",
                        ToolStatus::Completed => "ok",
                        ToolStatus::Failed => "failed",
                    };
                    let mut rendered = format!("[tool {} -> {}]", call.name, status);
                    if let Some(output) = call.output.as_deref().map(str::trim) {
                        if !output.is_empty() {
                            rendered.push('\n');
                            rendered.push_str(output);
                        }
                    }
                    parts.push(rendered);
                }
                ContentBlock::FileWrite { path } => {
                    parts.push(format!("[wrote {path}]"));
                }
                ContentBlock::ChildActivity {
                    run_id,
                    agent_id,
                    task,
                    status,
                    blocks: _,
                } => {
                    let state = status
                        .map(|value| format!("{value:?}").to_ascii_lowercase())
                        .unwrap_or_else(|| "running".to_string());
                    parts.push(format!(
                        "[subagent {agent_id} · run {run_id} · {state}]\n{task}"
                    ));
                }
            }
        }
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(blocks: Vec<ContentBlock>) -> Message {
        Message {
            id: MessageId::new("m1"),
            role: Role::Assistant,
            blocks,
            agent_id: Some(AgentId::new("claude")),
            model: Some("sonnet".into()),
            run_id: None,
            seq: 1,
            created_at: 0,
        }
    }

    #[test]
    fn role_markers_are_distinct() {
        assert_eq!(Role::User.marker(), "## user");
        assert_eq!(Role::Assistant.marker(), "## assistant");
        assert_eq!(Role::System.marker(), "## system");
    }

    #[test]
    fn thinking_is_not_transferable_across_agents() {
        assert!(!ContentBlock::Thinking { text: "hmm".into() }.is_transferable());
        assert!(ContentBlock::text("hello").is_transferable());
    }

    #[test]
    fn transferable_text_drops_reasoning_but_keeps_annotations() {
        let m = msg(vec![
            ContentBlock::Thinking {
                text: "secret chain of thought".into(),
            },
            ContentBlock::text("Here is the fix."),
            ContentBlock::Tool {
                call: ToolCall {
                    id: "t1".into(),
                    name: "edit".into(),
                    input: None,
                    output: Some("runID=backtest-123".into()),
                    status: ToolStatus::Completed,
                },
            },
            ContentBlock::FileWrite {
                path: "src/main.rs".into(),
            },
        ]);
        let text = m.transferable_text();
        assert!(!text.contains("secret chain of thought"));
        assert!(text.contains("Here is the fix."));
        assert!(text.contains("[tool edit -> ok]"));
        assert!(text.contains("runID=backtest-123"));
        assert!(text.contains("[wrote src/main.rs]"));
    }

    #[test]
    fn empty_and_whitespace_text_blocks_are_skipped() {
        let m = msg(vec![ContentBlock::text("   "), ContentBlock::text("real")]);
        assert_eq!(m.transferable_text(), "real");
    }
}
