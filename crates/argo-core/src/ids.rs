//! Strongly typed identifiers.
//!
//! Argo keys native agent sessions by `(conversation, agent)` and keys runs by
//! conversation. Mixing those id spaces would silently corrupt history, so each
//! one is a distinct newtype rather than a bare `String`.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Wraps an existing identifier string.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Mints a fresh random v4 identifier.
            pub fn generate() -> Self {
                Self(uuid::Uuid::new_v4().to_string())
            }

            /// Borrows the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// True when the identifier carries no characters.
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_string())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

string_id!(
    WorkspaceId,
    "Identifies a workspace (one canonical project root)."
);
string_id!(ConversationId, "Identifies a conversation thread.");
string_id!(MessageId, "Identifies a single message row.");
string_id!(RunId, "Identifies one agent invocation (one turn).");
string_id!(
    AgentId,
    "Identifies a runtime adapter, for example `claude`, `codex`, `kiro`, `grok`."
);
string_id!(
    SessionId,
    "An upstream CLI's own session/thread handle, opaque to Argo."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique() {
        assert_ne!(ConversationId::generate(), ConversationId::generate());
    }

    #[test]
    fn ids_serialize_transparently_as_strings() {
        // Transparent representation keeps the IPC payloads and SQLite columns
        // human-readable instead of nesting `{"0": "..."}`.
        let id = AgentId::new("codex");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, "\"codex\"");
        let back: AgentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn distinct_id_types_do_not_interchange() {
        // Compile-time guarantee documented as a test: the only way to cross id
        // spaces is an explicit string round-trip.
        let conversation = ConversationId::new("abc");
        let agent = AgentId::new("abc");
        assert_eq!(conversation.as_str(), agent.as_str());
    }
}
