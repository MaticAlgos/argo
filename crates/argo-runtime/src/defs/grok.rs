//! Grok CLI adapter (xAI).
//!
//! Verified against Grok CLI 1.1.7. The current headless interface accepts the
//! composed prompt through `--prompt`, emits plain text with `--format text`, and
//! uses `--no-sandbox` for Argo's full-authority mode. Earlier plan, approval,
//! prompt-file, and effort flags no longer exist, so Argo does not advertise them.
//!
//! The CLI accepts a session id but does not emit a stable new-session handle in
//! plain output. Argo therefore reseeds every turn from canonical history rather
//! than pretending native resume is reliable.

use crate::def::{InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::ModeSupport;
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, StreamFormat,
};

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--format".into(),
        "text".into(),
        // Argo's full mode is intentionally unsandboxed; restrictive modes are
        // not advertised because this Grok build exposes no enforceable analogue.
        "--no-sandbox".into(),
    ];

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    if let Some(prompt) = &ctx.prompt {
        args.push("--prompt".into());
        args.push(prompt.clone());
    }

    args
}

/// Parses `grok models` output, keeping only concrete `grok-*` ids.
///
/// The command prints status lines such as "You are logged in with grok.com"
/// alongside bullet-prefixed ids; without filtering, that prose would appear as
/// selectable models.
fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let mut out = vec![ModelOption::labeled("default", "default (CLI configured)")];
    let mut seen: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let token = line
            .trim()
            .trim_start_matches(['*', '-', '•'])
            .split_whitespace()
            .next()
            .unwrap_or_default();
        let id = token.trim_end_matches(':');
        let valid = id.starts_with("grok-")
            && id.len() > 5
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
        if valid && !seen.contains(&id.to_string()) {
            seen.push(id.to_string());
            out.push(ModelOption::new(id));
        }
    }
    out
}

/// Grok CLI adapter definition.
pub const GROK: RuntimeDef = RuntimeDef {
    id: "grok",
    name: "Grok CLI",
    bin: "grok",
    fallback_bins: &[],
    version_args: &["--version"],
    help_args: &["--help"],
    model_probe: Some(ModelProbe {
        args: &["models"],
        timeout_ms: 10_000,
        parse: parse_models,
        parse_reasoning: None,
    }),
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("grok-4.3", "grok-4.3"),
        ("grok-4.20-reasoning", "grok-4.20-reasoning (deep)"),
        ("grok-4.20-non-reasoning", "grok-4.20-non-reasoning (fast)"),
    ],
    reasoning_options: &[],
    // Grok owns its own OAuth flow; Argo injects no credentials and does not
    // probe login state.
    auth_probe: None,
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::Plain,
        prompt_encoding: PromptEncoding::Raw,
        prompt_delivery: PromptDelivery::Argument,
        native_resume: false,
        captures_session: false,
        mcp_injection: McpInjection::None,
        supports_images: false,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport::NONE,
    },
    install_url: "https://x.ai/cli",
};

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> InvocationContext {
        InvocationContext {
            cwd: "/repo".into(),
            prompt: Some("do the thing".into()),
            ..Default::default()
        }
    }

    #[test]
    fn prompt_uses_the_current_headless_interface() {
        let args = GROK.args_for(&ctx());
        assert!(args.windows(2).any(|w| w == ["--format", "text"]));
        assert!(args.windows(2).any(|w| w == ["--prompt", "do the thing"]));
        assert!(args.contains(&"--no-sandbox".to_string()));
        for obsolete in [
            "--prompt-file",
            "--plan",
            "--no-plan",
            "--always-approve",
            "--effort",
        ] {
            assert!(!args.contains(&obsolete.to_string()), "{args:?}");
        }
    }

    #[test]
    fn concrete_model_is_forwarded() {
        let args = GROK.args_for(&InvocationContext {
            model: Some("grok-4.20-reasoning".into()),
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|w| w == ["--model", "grok-4.20-reasoning"]));
    }

    #[test]
    fn default_model_is_not_forwarded() {
        let args = GROK.args_for(&InvocationContext {
            model: Some("default".into()),
            ..ctx()
        });
        assert!(!args.contains(&"--model".to_string()));
        assert!(!args.contains(&"--effort".to_string()));
    }

    #[test]
    fn model_discovery_keeps_only_grok_ids() {
        let stdout = "You are logged in with grok.com\n\
                      Available models:\n\
                      * grok-4.3 (default)\n\
                      - grok-4.20-reasoning\n\
                      some other prose\n";
        let models = parse_models(stdout);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "grok-4.3", "grok-4.20-reasoning"]);
    }

    #[test]
    fn capabilities_reflect_the_real_limitations() {
        // No durable session: every turn must be reseeded from Argo's transcript.
        const {
            assert!(GROK.capabilities.always_reseeds());
        }
        const {
            assert!(!GROK.capabilities.native_resume);
        }
        const {
            assert!(!GROK.capabilities.captures_session);
        }
        assert!(!GROK.capabilities.modes.has_any());
        assert_eq!(GROK.capabilities.prompt_delivery, PromptDelivery::Argument);
        // No MCP injection path, so Grok cannot host Argo's delegation tools and
        // therefore cannot spawn children — it can only be a delegation target.
        const {
            assert!(GROK.capabilities.can_delegate());
            assert!(!GROK.capabilities.delegates_via_mcp());
        }
        // Plain output means no per-tool granularity.
        const {
            assert!(!GROK.capabilities.stream_format.has_structured_tool_events());
        }
    }

    #[test]
    fn missing_prompt_produces_no_prompt_flag() {
        let args = GROK.args_for(&InvocationContext {
            prompt: None,
            ..ctx()
        });
        assert!(!args.contains(&"--prompt".to_string()));
    }
}
