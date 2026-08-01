//! Adapter capability vocabulary.
//!
//! Adapters are declarative data, not subclasses. These types describe *how to
//! talk to* a CLI; the shared engine performs detection, launching, streaming,
//! and cancellation uniformly. Adding an agent is a new data value.

use serde::{Deserialize, Serialize};

/// Wire format an adapter speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StreamFormat {
    /// Anthropic-style `stream-json` JSONL (Claude).
    ClaudeStreamJson,
    /// Typed JSONL event stream (Codex `exec --json`).
    JsonEventStream,
    /// Antigravity `--output-format stream-json` records.
    AntigravityStreamJson,
    /// Agent Client Protocol JSON-RPC over stdio (Kiro).
    AcpJsonRpc,
    /// Unstructured text on stdout (Grok).
    Plain,
}

impl StreamFormat {
    /// True when the format exposes per-tool and per-file events.
    ///
    /// Plain adapters give Argo no tool granularity, so the engine reconciles
    /// filesystem changes after the run instead of trusting the stream.
    pub const fn has_structured_tool_events(&self) -> bool {
        !matches!(self, Self::Plain)
    }
}

/// How the composed prompt reaches the child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptDelivery {
    /// Written to the child's stdin.
    Stdin,
    /// Staged in a temp file whose path is passed as an argument.
    ///
    /// Available for adapters that cannot accept a prompt through stdin or a
    /// bounded command-line argument.
    File,
    /// Passed as the value of a command-line prompt flag.
    ///
    /// Used only when the CLI offers no stdin or prompt-file form. The context
    /// budget remains conservative because operating systems cap argv size.
    Argument,
    /// Sent inside a JSON-RPC request body.
    Protocol,
}

/// How the prompt bytes must be framed for the CLI to accept them.
///
/// Claude's `--input-format stream-json` reads stdin as JSONL protocol frames, so
/// raw text is rejected with a parse error. Declaring the framing per adapter
/// keeps that knowledge in the definition instead of the shared executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptEncoding {
    /// Send the composed body verbatim.
    Raw,
    /// Wrap the body in one `stream-json` user message.
    StreamJsonUserMessage,
}

/// Permission posture Argo requests from the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPosture {
    /// Pass the CLI's own non-interactive bypass flags.
    ///
    /// Argo runs children without a TTY, so an interactive approval prompt would
    /// hang the turn. This grants the agent the authority the flags imply.
    FullBypass,
}

/// A selectable model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// Identifier passed to the CLI.
    pub id: String,
    /// Display label.
    pub label: String,
}

impl ModelOption {
    /// Builds an option whose label matches its id.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            label: id.clone(),
            id,
        }
    }

    /// Builds an option with a distinct display label.
    pub fn labeled(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

/// A selectable reasoning effort level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningOption {
    /// Identifier passed to the CLI.
    pub id: String,
    /// Display label.
    pub label: String,
}

/// How Argo hands its own MCP servers to a CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpInjection {
    /// Generated Claude-style `mcp.json` passed per run.
    ClaudeMcpJson,
    /// Codex config override flags.
    CodexConfig,
    /// ACP `session/new.mcpServers` descriptors.
    AcpSessionNew,
    /// Antigravity's shared `~/.gemini/config/mcp_config.json`.
    ///
    /// Unlike the others this file is global rather than per-run, because the CLI
    /// offers no way to point at one. Argo therefore merges into it and leaves
    /// entries it did not add untouched.
    GeminiSharedConfig,
    /// OpenCode's shared `~/.config/opencode/opencode.jsonc`.
    OpenCodeSharedConfig,
    /// Command Code's shared `~/.commandcode/mcp.json`.
    CommandCodeSharedConfig,
    /// No supported injection path.
    ///
    /// The agent can still be a delegation target, but cannot host Argo's
    /// delegation tools and so cannot itself spawn children.
    None,
}

impl McpInjection {
    /// True when Argo can expose its MCP servers to this adapter.
    pub const fn is_supported(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// True when this adapter can host Argo's own delegation tools.
    ///
    /// Delegation carries the parent conversation id in the server's environment,
    /// which a *shared* config file cannot express: it is global, so two
    /// conversations would overwrite each other's parent and a subagent could
    /// attach to the wrong one. Such adapters still receive the user's servers.
    pub const fn hosts_delegation(&self) -> bool {
        match self {
            Self::ClaudeMcpJson | Self::CodexConfig | Self::AcpSessionNew => true,
            Self::GeminiSharedConfig
            | Self::OpenCodeSharedConfig
            | Self::CommandCodeSharedConfig
            | Self::None => false,
        }
    }
}

/// Capability summary the engine and TUI branch on.
///
/// Capabilities are declared per adapter so the engine degrades honestly rather
/// than assuming every CLI behaves like Claude.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    /// Wire format.
    pub stream_format: StreamFormat,
    /// Prompt transport.
    pub prompt_delivery: PromptDelivery,
    /// Prompt framing required by the CLI.
    pub prompt_encoding: PromptEncoding,
    /// True when the CLI can continue its own session by handle.
    pub native_resume: bool,
    /// True when the CLI discloses a durable handle Argo can store.
    pub captures_session: bool,
    /// MCP injection strategy.
    pub mcp_injection: McpInjection,
    /// True when the adapter accepts image attachments.
    pub supports_images: bool,
    /// Permission posture used for headless runs.
    pub permission: PermissionPosture,
    /// Execution modes this CLI can actually enforce.
    pub modes: crate::mode::ModeSupport,
}

impl AgentCapabilities {
    /// True because every Argo-managed turn receives the daemon-backed command
    /// fallback. Adapters with safe per-run MCP injection also receive native
    /// `argo_delegate` tools; shared-config and non-MCP adapters use `$ARGO_BIN
    /// delegate ...` through their shell tool instead.
    pub const fn can_delegate(&self) -> bool {
        true
    }

    /// True when delegation is exposed as an MCP tool in addition to the command fallback.
    pub const fn delegates_via_mcp(&self) -> bool {
        self.mcp_injection.hosts_delegation()
    }

    /// True when every turn must be seeded with canonical context.
    pub const fn always_reseeds(&self) -> bool {
        !self.native_resume
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(stream: StreamFormat, resume: bool, mcp: McpInjection) -> AgentCapabilities {
        AgentCapabilities {
            stream_format: stream,
            prompt_delivery: PromptDelivery::Stdin,
            prompt_encoding: PromptEncoding::Raw,
            native_resume: resume,
            captures_session: resume,
            mcp_injection: mcp,
            supports_images: false,
            permission: PermissionPosture::FullBypass,
            modes: crate::mode::ModeSupport::NONE,
        }
    }

    #[test]
    fn plain_format_has_no_structured_tool_events() {
        assert!(!StreamFormat::Plain.has_structured_tool_events());
        assert!(StreamFormat::ClaudeStreamJson.has_structured_tool_events());
        assert!(StreamFormat::AcpJsonRpc.has_structured_tool_events());
        assert!(StreamFormat::JsonEventStream.has_structured_tool_events());
    }

    #[test]
    fn grok_shaped_adapter_always_reseeds_and_cannot_delegate() {
        // Grok: plain stream, no native resume, no MCP injection path.
        let grok = caps(StreamFormat::Plain, false, McpInjection::None);
        assert!(grok.always_reseeds());
        assert!(grok.can_delegate());
        assert!(!grok.delegates_via_mcp());
    }

    #[test]
    fn claude_shaped_adapter_resumes_and_delegates() {
        let claude = caps(
            StreamFormat::ClaudeStreamJson,
            true,
            McpInjection::ClaudeMcpJson,
        );
        assert!(!claude.always_reseeds());
        assert!(claude.can_delegate());
    }

    #[test]
    fn model_option_helpers() {
        assert_eq!(ModelOption::new("grok-4.3").label, "grok-4.3");
        assert_eq!(ModelOption::labeled("a", "A").label, "A");
    }
}
