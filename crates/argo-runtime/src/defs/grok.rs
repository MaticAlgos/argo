//! Grok CLI adapter (xAI).
//!
//! Ported from OpenDesign's shipped `grok-build` definition. Three constraints
//! shape this adapter, and all three are load-bearing:
//!
//! - Recent builds require the prompt as an argv value (`-p/--single`), but Argo's
//!   composed prompts routinely exceed safe argv limits. The prompt is therefore
//!   staged in a file and passed with `--prompt-file`.
//! - Headless runs need `--no-plan --always-approve`; without auto-approval a
//!   write is permission-cancelled while the CLI still exits successfully, which
//!   would look like a silent no-op.
//! - Output is plain text. Argo gets no tool or file events from this CLI and
//!   reconciles filesystem changes after the run instead.
//!
//! Grok also has no durable session, so every turn is reseeded from Argo's
//! canonical transcript. That is the clearest demonstration that Argo's store,
//! not the CLI's, is the authority.

use crate::def::{InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::{AgentMode, ModeSupport};
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, StreamFormat,
};

/// True when a model id denotes a reasoning-capable variant.
///
/// `--effort` is only valid for those; passing it otherwise is rejected.
fn supports_effort(model: &str) -> bool {
    model.to_ascii_lowercase().contains("reasoning")
}

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    // The staged prompt file is mandatory for this adapter. An empty path here
    // would silently send no prompt, so the engine must always supply one.
    if let Some(path) = &ctx.prompt_file {
        args.push("--prompt-file".into());
        args.push(path.clone());
    }

    // Grok plans by default; Argo suppresses that unless plan mode is requested,
    // and withholds auto-approval so the plan is not immediately executed.
    if ctx.mode == AgentMode::Plan {
        args.push("--plan".into());
    } else {
        args.push("--no-plan".into());
        args.push("--always-approve".into());
    }

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
        if let Some(effort) = &ctx.reasoning {
            if supports_effort(model) {
                args.push("--effort".into());
                args.push(effort.clone());
            }
        }
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
    help_args: &["-p", "--help"],
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
    reasoning_options: &[
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("xhigh", "xhigh"),
        ("max", "max"),
    ],
    // Grok owns its own OAuth flow; Argo injects no credentials and does not
    // probe login state.
    auth_probe: None,
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::Plain,
        prompt_encoding: PromptEncoding::Raw,
        prompt_delivery: PromptDelivery::File,
        native_resume: false,
        captures_session: false,
        mcp_injection: McpInjection::None,
        supports_images: false,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport {
            plan: true,
            accept_edits: false,
            read_only: false,
        },
    },
    install_url: "https://x.ai/cli",
};

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> InvocationContext {
        InvocationContext {
            cwd: "/repo".into(),
            prompt_file: Some("/data/argo/staging/prompt.txt".into()),
            ..Default::default()
        }
    }

    #[test]
    fn prompt_is_delivered_by_file_with_headless_flags() {
        let args = GROK.args_for(&ctx());
        assert!(args
            .windows(2)
            .any(|w| w == ["--prompt-file", "/data/argo/staging/prompt.txt"]));
        // Without these, a write is permission-cancelled while the CLI still
        // exits 0 — a silent no-op.
        assert!(args.contains(&"--no-plan".to_string()));
        assert!(args.contains(&"--always-approve".to_string()));
    }

    #[test]
    fn plan_mode_asks_for_a_plan_and_withholds_approval() {
        let args = GROK.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            ..ctx()
        });
        assert!(args.contains(&"--plan".to_string()));
        assert!(!args.contains(&"--always-approve".to_string()));
        assert!(!args.contains(&"--no-plan".to_string()));
    }

    #[test]
    fn effort_is_only_sent_for_reasoning_models() {
        let non_reasoning = GROK.args_for(&InvocationContext {
            model: Some("grok-4.3".into()),
            reasoning: Some("high".into()),
            ..ctx()
        });
        assert!(!non_reasoning.contains(&"--effort".to_string()));

        let reasoning = GROK.args_for(&InvocationContext {
            model: Some("grok-4.20-reasoning".into()),
            reasoning: Some("high".into()),
            ..ctx()
        });
        assert!(reasoning.windows(2).any(|w| w == ["--effort", "high"]));
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
        // No MCP injection path, so Grok cannot host Argo's delegation tools and
        // therefore cannot spawn children — it can only be a delegation target.
        const {
            assert!(!GROK.capabilities.can_delegate());
        }
        // Plain output means no per-tool granularity.
        const {
            assert!(!GROK.capabilities.stream_format.has_structured_tool_events());
        }
    }

    #[test]
    fn missing_prompt_file_produces_no_prompt_flag() {
        // The engine must always stage a file; this documents the failure shape
        // if it ever does not.
        let args = GROK.args_for(&InvocationContext {
            prompt_file: None,
            ..ctx()
        });
        assert!(!args.contains(&"--prompt-file".to_string()));
    }
}
