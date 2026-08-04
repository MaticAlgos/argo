//! Turning Argo state into chat messages.
//!
//! Two jobs. The reply bubble accumulates streamed prose and is re-rendered on
//! every edit, so rendering must be cheap and must never produce text Telegram
//! rejects. The recap card is what makes switching sessions comprehensible: a
//! Telegram DM is one linear chat, so moving between conversations shows nothing
//! at all unless the switch carries its own context.

use crate::markdown_v2::{escape, to_markdown_v2};

/// How much of a recalled message is quoted in a recap.
const RECAP_EXCERPT_CHARS: usize = 600;

/// A tool call in flight, shown in its own status bubble.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProgress {
    /// Tool name.
    pub name: String,
    /// True once the call finished.
    pub done: bool,
    /// False when the tool reported an error.
    pub ok: bool,
}

/// Everything needed to draw the recap shown when the active session changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recap {
    /// Workspace directory name.
    pub workspace: Option<String>,
    /// Conversation title.
    pub title: Option<String>,
    /// Agent answering next.
    pub agent: Option<String>,
    /// Model in effect.
    pub model: Option<String>,
    /// Execution mode.
    pub mode: Option<String>,
    /// Standby agent, when failover is configured.
    pub backup: Option<String>,
    /// Total messages in the conversation.
    pub message_count: usize,
    /// The daemon's rolling "started with / current focus" description.
    pub description: Option<String>,
    /// Last user prompt, for orientation.
    pub last_user: Option<String>,
    /// Last assistant reply, for orientation.
    pub last_assistant: Option<String>,
}

/// Renders the card posted when the active workspace or conversation changes.
///
/// Everything above the divider comes from metadata the daemon already keeps, so
/// this costs one summary lookup rather than a history replay.
pub fn recap_card(recap: &Recap) -> String {
    let mut out = String::new();

    let heading = match (&recap.workspace, &recap.title) {
        (Some(workspace), Some(title)) => format!("📁 {workspace} · 💬 {title}"),
        (Some(workspace), None) => format!("📁 {workspace} · 💬 new conversation"),
        (None, Some(title)) => format!("💬 {title}"),
        (None, None) => "💬 new conversation".to_string(),
    };
    out.push_str(&format!("*{}*\n", escape(&heading)));

    let mut selection = Vec::new();
    match (&recap.agent, &recap.model) {
        (Some(agent), Some(model)) => selection.push(format!("{agent}/{model}")),
        (Some(agent), None) => selection.push(agent.clone()),
        _ => selection.push("no CLI selected".to_string()),
    }
    if let Some(mode) = &recap.mode {
        selection.push(mode.clone());
    }
    if let Some(backup) = &recap.backup {
        selection.push(format!("⇄ {backup}"));
    }
    out.push_str(&escape(&selection.join(" · ")));
    out.push('\n');

    out.push_str(&escape(&match recap.message_count {
        0 => "no messages yet".to_string(),
        1 => "1 message".to_string(),
        count => format!("{count} messages"),
    }));
    out.push('\n');

    if let Some(description) = &recap.description {
        out.push('\n');
        out.push_str(&escape(description));
        out.push('\n');
    }

    if recap.last_user.is_some() || recap.last_assistant.is_some() {
        out.push('\n');
        out.push_str(&escape("── recent ──"));
        out.push('\n');
        if let Some(text) = &recap.last_user {
            out.push_str(&format!("🧑 {}\n", escape(&excerpt(text))));
        }
        if let Some(text) = &recap.last_assistant {
            out.push_str(&format!("🤖 {}\n", escape(&excerpt(text))));
        }
    }

    out.trim_end().to_string()
}

/// Trims a recalled message to a quotable length.
fn excerpt(text: &str) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= RECAP_EXCERPT_CHARS {
        return flattened;
    }
    let clipped: String = flattened.chars().take(RECAP_EXCERPT_CHARS).collect();
    // Back off to a word boundary so the excerpt does not end mid-token.
    match clipped.rfind(' ') {
        Some(index) if index > RECAP_EXCERPT_CHARS / 2 => format!("{}…", &clipped[..index]),
        _ => format!("{clipped}…"),
    }
}

/// Renders the reply bubble for a turn in progress or just finished.
///
/// `thinking` is shown only while nothing else has arrived, so an agent that
/// reasons for a while still looks alive.
pub fn reply_bubble(text: &str, finished: bool, failed: Option<&str>) -> String {
    let body = to_markdown_v2(text.trim());
    let mut out = if body.is_empty() {
        if finished {
            escape("(no output)")
        } else {
            escape("…")
        }
    } else {
        body
    };
    if let Some(error) = failed {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("⚠️ {}", escape(error)));
    } else if !finished {
        out.push_str(&escape(" ▌"));
    }
    out
}

/// Renders the status bubble that tracks tool activity for one turn.
///
/// Edited in place rather than posted per call: a long agentic turn can make
/// dozens of calls, and one message per call buries the actual answer.
pub fn tool_bubble(tools: &[ToolProgress]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for tool in tools {
        let marker = match (tool.done, tool.ok) {
            (false, _) => "⏳",
            (true, true) => "✓",
            (true, false) => "✗",
        };
        out.push_str(&format!("{marker} {}\n", escape(&tool.name)));
    }
    out.trim_end().to_string()
}

/// Renders a diagnostic Argo emits mid-turn, such as a failover notice.
pub fn notice(text: &str) -> String {
    format!("ℹ️ {}", escape(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recap() -> Recap {
        Recap {
            workspace: Some("agentmux".into()),
            title: Some("Kiro usage panel fix".into()),
            agent: Some("claude".into()),
            model: Some("sonnet".into()),
            mode: Some("full access".into()),
            backup: Some("codex".into()),
            message_count: 14,
            description: Some(
                "Started with: fix the Kiro /usage panel · Current focus: bump version".into(),
            ),
            last_user: Some("it works properly, update the version number".into()),
            last_assistant: Some("Committed as f41cf9d.".into()),
        }
    }

    #[test]
    fn the_recap_states_where_you_are_and_what_was_happening() {
        // Switching sessions in a linear chat shows nothing otherwise, so this
        // card is the only orientation the user gets.
        let card = recap_card(&recap());
        assert!(card.contains("agentmux"), "{card}");
        assert!(card.contains("Kiro usage panel fix"), "{card}");
        assert!(card.contains("claude/sonnet"), "{card}");
        assert!(card.contains("⇄ codex"), "{card}");
        assert!(card.contains("14 messages"), "{card}");
        assert!(card.contains("Current focus"), "{card}");
        assert!(card.contains("🧑"), "{card}");
        assert!(card.contains("🤖"), "{card}");
    }

    #[test]
    fn recap_text_is_fully_escaped_for_markdown_v2() {
        // Titles and prompts routinely carry dots, dashes and parentheses; one
        // unescaped character means the card fails to send at all. Every field
        // here is attacker-ish text in the sense that Argo does not control it.
        let card = recap_card(&Recap {
            workspace: Some("argo-v2".into()),
            title: Some("fix run.rs (again)".into()),
            agent: Some("claude".into()),
            message_count: 3,
            description: Some("Started with: bump 0.1.6 -> 0.2.0!".into()),
            last_user: Some("see crates/argo-tui/src/run.rs".into()),
            ..Default::default()
        });
        for fragment in [
            "argo\\-v2",
            "run\\.rs",
            "\\(again\\)",
            "0\\.1\\.6",
            "\\-\\> 0\\.2\\.0\\!",
            "argo\\-tui",
        ] {
            assert!(card.contains(fragment), "missing {fragment} in {card}");
        }
        // Only Argo's own emphasis markers may remain unescaped.
        let stray_dots = card
            .match_indices('.')
            .filter(|(index, _)| *index == 0 || card.as_bytes()[index - 1] != b'\\');
        assert_eq!(stray_dots.count(), 0, "unescaped '.' in {card}");
    }

    #[test]
    fn a_brand_new_conversation_still_renders() {
        let card = recap_card(&Recap {
            workspace: Some("agentmux".into()),
            ..Default::default()
        });
        assert!(card.contains("new conversation"), "{card}");
        assert!(card.contains("no messages yet"), "{card}");
        assert!(!card.contains("── recent ──"), "{card}");
    }

    #[test]
    fn a_long_excerpt_is_clipped_at_a_word_boundary() {
        let long = "word ".repeat(400);
        let clipped = excerpt(&long);
        assert!(clipped.chars().count() <= RECAP_EXCERPT_CHARS + 1);
        assert!(clipped.ends_with('…'));
        assert!(!clipped.contains("  "), "whitespace should be flattened");
    }

    #[test]
    fn a_streaming_bubble_shows_a_cursor_until_it_finishes() {
        let streaming = reply_bubble("partial answ", false, None);
        assert!(streaming.contains('▌'), "{streaming}");
        let done = reply_bubble("partial answer", true, None);
        assert!(!done.contains('▌'), "{done}");
    }

    #[test]
    fn an_empty_turn_says_so_rather_than_sending_nothing() {
        // Telegram rejects an empty message, so a silent agent must still
        // produce sendable text.
        assert!(!reply_bubble("", true, None).is_empty());
        assert!(!reply_bubble("", false, None).is_empty());
    }

    #[test]
    fn a_failure_is_appended_to_whatever_was_already_streamed() {
        let bubble = reply_bubble("started work", true, Some("usage limit reached"));
        assert!(bubble.contains("started work"), "{bubble}");
        assert!(bubble.contains("⚠️"), "{bubble}");
        assert!(bubble.contains("usage limit reached"), "{bubble}");
    }

    #[test]
    fn the_tool_bubble_marks_running_done_and_failed_calls() {
        let bubble = tool_bubble(&[
            ToolProgress {
                name: "Read".into(),
                done: true,
                ok: true,
            },
            ToolProgress {
                name: "Bash".into(),
                done: true,
                ok: false,
            },
            ToolProgress {
                name: "Edit".into(),
                done: false,
                ok: true,
            },
        ]);
        assert!(bubble.contains("✓ Read"), "{bubble}");
        assert!(bubble.contains("✗ Bash"), "{bubble}");
        assert!(bubble.contains("⏳ Edit"), "{bubble}");
        assert!(tool_bubble(&[]).is_empty());
    }
}
