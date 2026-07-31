//! Error taxonomy shared across Argo crates.

use thiserror::Error;

/// Convenience alias for fallible Argo operations.
pub type Result<T> = std::result::Result<T, ArgoError>;

/// Every failure Argo surfaces to a client is one of these variants.
///
/// The daemon maps these onto stable machine-readable codes so the TUI can show
/// an actionable fix rather than a raw string.
#[derive(Debug, Error)]
pub enum ArgoError {
    /// The on-disk store could not be opened, migrated, or queried.
    #[error("store error: {0}")]
    Store(String),

    /// A requested entity does not exist.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// Entity kind, for example `conversation`.
        kind: &'static str,
        /// Identifier that was looked up.
        id: String,
    },

    /// The request violated an invariant, such as using a conversation that
    /// belongs to a different workspace.
    #[error("invalid request: {0}")]
    Invalid(String),

    /// A runtime adapter is not installed or failed its detection probe.
    #[error("agent unavailable: {agent}: {reason}")]
    AgentUnavailable {
        /// Adapter id.
        agent: String,
        /// Human-readable reason with a suggested fix.
        reason: String,
    },

    /// Spawning or supervising a child process failed.
    #[error("process error: {0}")]
    Process(String),

    /// A structured stream or JSON-RPC transport produced unusable output.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The run was cancelled by the user or by shutdown.
    #[error("cancelled")]
    Cancelled,

    /// The operation exceeded its deadline.
    #[error("timed out after {0}ms")]
    Timeout(u64),

    /// Underlying filesystem failure.
    #[error("io error: {0}")]
    Io(String),

    /// Serialization or deserialization failure.
    #[error("serde error: {0}")]
    Serde(String),

    /// An error already formatted by the daemon.
    ///
    /// Displayed verbatim so a client does not prefix a message that already
    /// describes itself.
    #[error("{message}")]
    Remote {
        /// Stable code reported by the daemon.
        code: String,
        /// Human-readable message, already formatted.
        message: String,
        /// Whether the daemon considered it retryable.
        retryable: bool,
    },
}

impl ArgoError {
    /// Stable code for telemetry, logs, and TUI diagnostics.
    ///
    /// Codes are part of Argo's observable surface; renaming one is a breaking
    /// change for anything matching on daemon output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Store(_) => "STORE_ERROR",
            Self::NotFound { .. } => "NOT_FOUND",
            Self::Invalid(_) => "INVALID_REQUEST",
            Self::AgentUnavailable { .. } => "AGENT_UNAVAILABLE",
            Self::Process(_) => "PROCESS_ERROR",
            Self::Protocol(_) => "PROTOCOL_ERROR",
            Self::Cancelled => "CANCELLED",
            Self::Timeout(_) => "TIMEOUT",
            Self::Io(_) => "IO_ERROR",
            Self::Serde(_) => "SERDE_ERROR",
            Self::Remote { code, .. } => match code.as_str() {
                "NOT_FOUND" => "NOT_FOUND",
                "AGENT_UNAVAILABLE" => "AGENT_UNAVAILABLE",
                "CANCELLED" => "CANCELLED",
                "TIMEOUT" => "TIMEOUT",
                _ => "REMOTE_ERROR",
            },
        }
    }

    /// True when retrying the same request may succeed without user action.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout(_) | Self::Process(_) | Self::Protocol(_) => true,
            Self::Remote { retryable, .. } => *retryable,
            _ => false,
        }
    }

    /// Wraps a daemon error response without re-formatting its message.
    pub fn remote(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self::Remote {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    /// Builds a `NotFound` error.
    pub fn not_found(kind: &'static str, id: impl Into<String>) -> Self {
        Self::NotFound {
            kind,
            id: id.into(),
        }
    }
}

impl From<std::io::Error> for ArgoError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value.to_string())
    }
}

impl From<serde_json::Error> for ArgoError {
    fn from(value: serde_json::Error) -> Self {
        Self::Serde(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_per_variant() {
        assert_eq!(ArgoError::Cancelled.code(), "CANCELLED");
        assert_eq!(
            ArgoError::not_found("conversation", "x").code(),
            "NOT_FOUND"
        );
        assert_eq!(ArgoError::Timeout(500).code(), "TIMEOUT");
    }

    #[test]
    fn only_transient_failures_are_retryable() {
        assert!(ArgoError::Timeout(1).is_retryable());
        assert!(ArgoError::Process("spawn".into()).is_retryable());
        // A missing binary or a bad request will not fix itself on retry.
        assert!(!ArgoError::Invalid("bad".into()).is_retryable());
        assert!(!ArgoError::AgentUnavailable {
            agent: "codex".into(),
            reason: "not installed".into()
        }
        .is_retryable());
    }

    #[test]
    fn remote_errors_are_displayed_verbatim() {
        // The daemon already formatted this; prefixing again produced
        // "invalid request: invalid request: ..." in the CLI.
        let error = ArgoError::remote("INVALID_REQUEST", "invalid request: bad model", false);
        assert_eq!(error.to_string(), "invalid request: bad model");
        assert!(!error.is_retryable());
        assert!(ArgoError::remote("TIMEOUT", "timed out", true).is_retryable());
    }

    #[test]
    fn not_found_message_names_kind_and_id() {
        let err = ArgoError::not_found("conversation", "conv-1");
        assert_eq!(err.to_string(), "conversation not found: conv-1");
    }
}
