//! Argo context engine.
//!
//! Argo's SQLite transcript is authoritative; upstream CLI session stores are
//! treated as caches that may be reused when safe. This crate turns canonical
//! history into the body a specific agent receives for one turn:
//!
//! - [`transcript`] flattens messages into role-marked text and neutralizes
//!   delimiters planted in untrusted content.
//! - [`package`] assembles the remaining-context bundle and composes the final
//!   turn body, skipping the transcript when the upstream session is resumed.
//! - [`budget`] decides when history must be compacted to fit a target model.

pub mod budget;
pub mod package;
pub mod transcript;

pub use budget::{plan_budget, BudgetPlan, ContextBudget};
pub use package::{compose_turn, ChildOutcome, ContextPackage, WorkspaceFacts};
pub use transcript::{flatten_transcript, guard_delimiters, TRANSCRIPT_HEADING};
