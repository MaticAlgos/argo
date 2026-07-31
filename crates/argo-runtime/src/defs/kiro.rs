//! Kiro CLI adapter (ACP over stdio).
//!
//! Verified against Kiro's official documentation: `kiro-cli acp` speaks ACP
//! JSON-RPC 2.0 over stdin/stdout and implements `initialize`, `session/new`,
//! `session/load`, `session/prompt`, `session/cancel`, `session/set_mode`, and
//! `session/set_model`, advertising `loadSession: true`.
//!
//! Because the session lifecycle is protocol-level, argv is nearly empty: the
//! model, the prompt, the MCP descriptors, and the resume target are all carried
//! in JSON-RPC messages rather than command-line flags.

use crate::def::{InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::ModeSupport;
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, StreamFormat,
};

fn build_args(_ctx: &InvocationContext) -> Vec<String> {
    // Everything else is negotiated over the protocol; see the ACP transport.
    vec!["acp".into()]
}

/// Parses `kiro-cli chat --list-models --format json`.
///
/// The list lives under the `chat` subcommand, which is why the top-level help
/// reveals nothing. Each entry carries a context window and a credit multiplier,
/// both worth surfacing: the multiplier is the difference between a cheap model
/// and one that costs twenty times more.
fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return vec![ModelOption::labeled("default", "default (CLI configured)")];
    };
    let Some(models) = value.get("models").and_then(|m| m.as_array()) else {
        return vec![ModelOption::labeled("default", "default (CLI configured)")];
    };

    let default_model = value
        .get("default_model")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    let mut out = vec![ModelOption::labeled("default", "default (CLI configured)")];
    for model in models {
        let Some(id) = model
            .get("model_id")
            .or_else(|| model.get("model_name"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };

        let mut parts: Vec<String> = Vec::new();
        if let Some(window) = model.get("context_window_tokens").and_then(|v| v.as_u64()) {
            parts.push(if window >= 1_000_000 {
                format!("{}M ctx", window / 1_000_000)
            } else {
                format!("{}k ctx", window / 1_000)
            });
        }
        if let Some(rate) = model.get("rate_multiplier").and_then(|v| v.as_f64()) {
            parts.push(format!("{rate}x credits"));
        }
        if id == default_model {
            parts.push("default".to_string());
        }

        let label = if parts.is_empty() {
            id.to_string()
        } else {
            format!("{id} — {}", parts.join(" · "))
        };
        out.push(ModelOption::labeled(id, label));
    }
    out
}

/// Kiro CLI adapter definition.
pub const KIRO: RuntimeDef = RuntimeDef {
    id: "kiro",
    name: "Kiro CLI",
    bin: "kiro-cli",
    fallback_bins: &["kiro"],
    version_args: &["--version"],
    help_args: &["--help"],
    // The model list lives under the `chat` subcommand, so the top-level help
    // shows nothing — which is why this was missed initially.
    model_probe: Some(ModelProbe {
        args: &["chat", "--list-models", "--format", "json"],
        timeout_ms: 15_000,
        parse: parse_models,
        parse_reasoning: None,
    }),
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("auto", "auto"),
        ("claude-sonnet-5", "claude-sonnet-5"),
        ("gpt-5.6-sol", "gpt-5.6-sol"),
    ],
    reasoning_options: &[],
    auth_probe: None,
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::AcpJsonRpc,
        prompt_encoding: PromptEncoding::Raw,
        prompt_delivery: PromptDelivery::Protocol,
        native_resume: true,
        captures_session: true,
        mcp_injection: McpInjection::AcpSessionNew,
        supports_images: true,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport {
            plan: false,
            accept_edits: false,
            read_only: false,
        },
    },
    install_url: "https://kiro.dev/docs/cli/",
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_the_acp_subcommand_only() {
        let args = KIRO.args_for(&InvocationContext {
            cwd: "/repo".into(),
            // Even with a model and a resume target present, argv stays minimal:
            // both are protocol concerns for this adapter.
            model: Some("auto".into()),
            resume_session: Some("sess_abc".into()),
            ..Default::default()
        });
        assert_eq!(args, vec!["acp".to_string()]);
    }

    /// A trimmed copy of the real `--list-models --format json` output.
    const MODELS_JSON: &str = r#"{"models":[
        {"model_name":"auto","model_id":"auto","description":"Chosen by task",
         "context_window_tokens":1000000,"rate_multiplier":1.0},
        {"model_name":"claude-sonnet-5","model_id":"claude-sonnet-5","description":"Sonnet 5",
         "context_window_tokens":1000000,"rate_multiplier":1.3},
        {"model_name":"gpt-5.6-sol","model_id":"gpt-5.6-sol","description":"Sol",
         "context_window_tokens":272000,"rate_multiplier":2.4}
    ],"default_model":"auto"}"#;

    #[test]
    fn models_are_discovered_from_the_chat_subcommand() {
        let models = parse_models(MODELS_JSON);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["default", "auto", "claude-sonnet-5", "gpt-5.6-sol"]
        );
    }

    #[test]
    fn labels_surface_context_window_and_cost() {
        // The credit multiplier is the difference between a cheap model and one
        // costing twenty times more, so it belongs in the picker.
        let models = parse_models(MODELS_JSON);
        let auto = models.iter().find(|m| m.id == "auto").expect("auto");
        assert!(auto.label.contains("1M ctx"));
        assert!(auto.label.contains("1x credits"));
        assert!(auto.label.contains("default"));

        let sol = models.iter().find(|m| m.id == "gpt-5.6-sol").expect("sol");
        assert!(sol.label.contains("272k ctx"));
        assert!(sol.label.contains("2.4x credits"));
    }

    #[test]
    fn malformed_output_falls_back_to_the_default_only() {
        assert_eq!(parse_models("not json").len(), 1);
        assert_eq!(parse_models("{}").len(), 1);
    }

    #[test]
    fn prompt_is_delivered_over_the_protocol() {
        assert_eq!(KIRO.capabilities.prompt_delivery, PromptDelivery::Protocol);
        assert_eq!(KIRO.capabilities.stream_format, StreamFormat::AcpJsonRpc);
    }

    #[test]
    fn falls_back_to_the_short_binary_name() {
        // Installations expose `kiro-cli`; some setups symlink `kiro`.
        assert_eq!(KIRO.candidate_bins(), vec!["kiro-cli", "kiro"]);
    }

    #[test]
    fn supports_resume_and_delegation() {
        const {
            assert!(KIRO.capabilities.native_resume);
        }
        const {
            assert!(KIRO.capabilities.can_delegate());
        }
    }
}
