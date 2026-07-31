//! Context budgeting.
//!
//! Different CLIs and models accept very different amounts of context, so the
//! same conversation cannot always be replayed verbatim when the user switches.
//! This module decides how much recent history survives intact and how much must
//! be folded into a compacted summary.
//!
//! Argo never deletes history to satisfy a budget: the canonical rows stay in
//! SQLite, and only the projection sent to the agent is reduced.

use argo_core::message::{Message, Role};
use serde::{Deserialize, Serialize};

/// Rough bytes-per-token ratio used to convert a token budget into a byte
/// budget. Deliberately conservative: over-estimating tokens is safe, while
/// under-estimating produces a rejected request.
const BYTES_PER_TOKEN: usize = 3;

/// Share of the model's window Argo is willing to spend on replayed history.
///
/// The remainder is left for the system prompt, the live request, tool schemas,
/// and the response itself.
const HISTORY_FRACTION: f64 = 0.5;

/// A target model's capacity as far as Argo is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBudget {
    /// Maximum bytes of flattened history to replay.
    pub max_history_bytes: usize,
}

impl ContextBudget {
    /// Derives a byte budget from a model's advertised token window.
    pub fn from_token_window(tokens: usize) -> Self {
        let usable = (tokens as f64 * HISTORY_FRACTION) as usize;
        Self {
            max_history_bytes: usable.saturating_mul(BYTES_PER_TOKEN),
        }
    }

    /// Budget used when a model's window is unknown.
    ///
    /// Sized for a small window so an unknown model is never handed more than it
    /// can accept; the cost is extra compaction, not a failed turn.
    pub fn conservative() -> Self {
        Self::from_token_window(32_000)
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::conservative()
    }
}

/// How history should be projected for one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetPlan {
    /// Index into the input slice; messages from here on are replayed verbatim.
    pub verbatim_from: usize,
    /// Number of older messages that must be represented by a summary instead.
    pub compact_count: usize,
    /// True when a summary is required.
    pub needs_compaction: bool,
    /// Estimated bytes of the verbatim portion.
    pub verbatim_bytes: usize,
}

/// Chooses how much history fits inside `budget`.
///
/// Walks backwards from the newest message so the most relevant turns are kept,
/// and always retains at least the final message even when it alone exceeds the
/// budget — dropping the live context entirely would be worse than a large
/// request that the CLI can still reject explicitly.
pub fn plan_budget(messages: &[Message], budget: ContextBudget) -> BudgetPlan {
    if messages.is_empty() {
        return BudgetPlan {
            verbatim_from: 0,
            compact_count: 0,
            needs_compaction: false,
            verbatim_bytes: 0,
        };
    }

    let mut used = 0usize;
    let mut first_kept = messages.len();

    for (idx, message) in messages.iter().enumerate().rev() {
        // Role marker plus separators; small and constant, but counted so the
        // estimate does not drift on long conversations of short turns.
        let cost = message.transferable_text().len() + message.role.marker().len() + 4;
        if used + cost > budget.max_history_bytes && idx != messages.len() - 1 {
            break;
        }
        used += cost;
        first_kept = idx;
    }

    BudgetPlan {
        verbatim_from: first_kept,
        compact_count: first_kept,
        needs_compaction: first_kept > 0,
        verbatim_bytes: used,
    }
}

/// Builds a deterministic fallback summary for the compacted prefix.
///
/// Used when no model-generated summary is available — for example when
/// compaction itself failed. It is intentionally mechanical: losing history
/// silently would be worse than replaying a terse, accurate outline.
pub fn fallback_summary(messages: &[Message]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut user_turns = 0usize;
    let mut agents: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    for message in messages {
        if message.role == Role::User {
            user_turns += 1;
        }
        if let Some(agent) = &message.agent_id {
            let name = agent.to_string();
            if !agents.contains(&name) {
                agents.push(name);
            }
        }
        for block in &message.blocks {
            if let argo_core::message::ContentBlock::FileWrite { path } = block {
                if !files.contains(path) {
                    files.push(path.clone());
                }
            }
        }
    }

    let mut lines = vec![format!(
        "{} earlier message(s) omitted, including {user_turns} user request(s).",
        messages.len()
    )];
    if !agents.is_empty() {
        lines.push(format!("Agents involved: {}.", agents.join(", ")));
    }
    if !files.is_empty() {
        lines.push(format!("Files previously modified: {}.", files.join(", ")));
    }
    lines.push(
        "Ask before assuming details of the omitted turns; the full history is retained by Argo."
            .to_string(),
    );
    lines.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use argo_core::ids::{AgentId, MessageId};
    use argo_core::message::ContentBlock;

    fn msg(seq: i64, role: Role, text: &str) -> Message {
        Message {
            id: MessageId::new(format!("m{seq}")),
            role,
            blocks: vec![ContentBlock::text(text)],
            agent_id: if role == Role::Assistant {
                Some(AgentId::new("claude"))
            } else {
                None
            },
            model: None,
            run_id: None,
            seq,
            created_at: 0,
        }
    }

    #[test]
    fn small_history_needs_no_compaction() {
        let messages = vec![
            msg(1, Role::User, "hello"),
            msg(2, Role::Assistant, "hi there"),
        ];
        let plan = plan_budget(&messages, ContextBudget::conservative());
        assert!(!plan.needs_compaction);
        assert_eq!(plan.verbatim_from, 0);
        assert_eq!(plan.compact_count, 0);
    }

    #[test]
    fn tight_budget_keeps_the_newest_turns_and_compacts_the_rest() {
        let messages: Vec<Message> = (0..20)
            .map(|i| msg(i, Role::User, &"x".repeat(100)))
            .collect();
        let plan = plan_budget(
            &messages,
            ContextBudget {
                max_history_bytes: 400,
            },
        );
        assert!(plan.needs_compaction);
        assert!(plan.verbatim_from > 0);
        // Newest message is always retained.
        assert!(plan.verbatim_from < messages.len());
        assert!(plan.verbatim_bytes <= 400 + 200);
    }

    #[test]
    fn final_message_survives_even_when_it_alone_exceeds_the_budget() {
        // Better to send one oversized turn the CLI can reject explicitly than to
        // send a turn with no live context at all.
        let messages = vec![
            msg(1, Role::User, &"a".repeat(50)),
            msg(2, Role::User, &"b".repeat(5_000)),
        ];
        let plan = plan_budget(
            &messages,
            ContextBudget {
                max_history_bytes: 100,
            },
        );
        assert_eq!(plan.verbatim_from, 1);
        assert!(plan.needs_compaction);
    }

    #[test]
    fn empty_history_is_handled() {
        let plan = plan_budget(&[], ContextBudget::conservative());
        assert!(!plan.needs_compaction);
        assert_eq!(plan.verbatim_bytes, 0);
    }

    #[test]
    fn budgets_scale_with_the_model_window() {
        let small = ContextBudget::from_token_window(32_000);
        let large = ContextBudget::from_token_window(1_000_000);
        assert!(large.max_history_bytes > small.max_history_bytes * 10);
    }

    #[test]
    fn fallback_summary_states_what_was_omitted_without_inventing_detail() {
        let messages = vec![
            msg(1, Role::User, "first"),
            Message {
                blocks: vec![
                    ContentBlock::text("done"),
                    ContentBlock::FileWrite {
                        path: "src/a.rs".into(),
                    },
                ],
                ..msg(2, Role::Assistant, "done")
            },
        ];
        let summary = fallback_summary(&messages);
        assert!(summary.contains("2 earlier message(s) omitted"));
        assert!(summary.contains("1 user request"));
        assert!(summary.contains("Agents involved: claude."));
        assert!(summary.contains("src/a.rs"));
        assert!(summary.contains("Ask before assuming"));
    }

    #[test]
    fn fallback_summary_of_nothing_is_empty() {
        assert_eq!(fallback_summary(&[]), "");
    }
}
