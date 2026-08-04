//! Argo domain model.
//!
//! This crate is pure: it performs no I/O and holds no runtime state. Every
//! other Argo crate depends on the vocabulary defined here so that the daemon,
//! the store, the adapters, and the TUI all agree on one shape for
//! conversations, runs, normalized events, and session-continuation policy.

pub mod error;
pub mod event;
pub mod ids;
pub mod message;
pub mod mode;
pub mod paths;
pub mod runtime;
pub mod session;
pub mod title;

pub use error::{ArgoError, Result};
pub use event::{EventSeq, RunEvent, RunEventKind, RunStatus, TokenUsage};
pub use ids::{AgentId, ConversationId, MessageId, RunId, SessionId, WorkspaceId};
pub use message::{ContentBlock, Message, Role, ToolCall, ToolStatus};
pub use mode::{AgentMode, ModeSupport};
pub use paths::ArgoPaths;
pub use runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, ReasoningOption, StreamFormat,
};
pub use session::{
    AgentSessionRecord, InvalidationReason, ResumeDecision, ResumePlan, SelectionChange,
};
pub use title::{conversation_description, conversation_title};

/// Protocol version for the daemon <-> client IPC contract.
///
/// The daemon refuses connections from clients advertising a different major
/// version so that an upgraded binary can never be driven by a stale TUI that
/// would misinterpret the event stream.
///
/// Bumped to 3 for the Telegram bridge and `/compact` requests: a v2 daemon has
/// no variant for either, so it would fail to decode them rather than being
/// recognized as stale and replaced.
pub const IPC_PROTOCOL_VERSION: u32 = 3;

/// Name of the per-project directory Argo owns inside a user workspace.
pub const ARGO_WORKSPACE_DIR: &str = ".argo";

/// Returns the lowercase hex sha256 of `input`.
///
/// Used for content-addressed staging paths and for the stable-context hash
/// that gates native-session reuse.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Milliseconds since the Unix epoch.
///
/// Argo stores all timestamps as `i64` epoch millis so that SQLite ordering,
/// JSON transport, and event sequencing share one representation.
pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_is_stable_and_lowercase() {
        // Known vector: sha256("") is well-defined, so a regression in the hex
        // encoding (padding, case, byte order) fails loudly here.
        assert_eq!(
            sha256_hex(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex("argo"), sha256_hex("argo"));
        assert_ne!(sha256_hex("argo"), sha256_hex("Argo"));
    }

    #[test]
    fn now_millis_is_positive_and_monotonic_enough() {
        let a = now_millis();
        assert!(a > 1_600_000_000_000, "expected a plausible epoch-ms value");
        let b = now_millis();
        assert!(b >= a);
    }
}
