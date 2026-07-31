//! OpenCode adapter.
//!
//! Verified against the installed CLI (1.18.9): `opencode run --format json`
//! emits a JSON event stream, `-s <id>` continues a session, `-m provider/model`
//! selects a model, and `--auto` auto-approves permissions for headless use.
//!
//! OpenCode also mints its own session id and stamps it on stream events, so Argo
//! captures that rather than specifying one.

use crate::def::{InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::{AgentMode, ModeSupport};
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, StreamFormat,
};

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into()];

    // Plan is one of OpenCode's built-in agents rather than a flag, and it must
    // not be combined with auto-approval or the restriction is meaningless.
    if ctx.mode == AgentMode::Plan {
        args.push("--agent".into());
        args.push("plan".into());
    } else {
        // Argo runs children without a TTY, so an approval prompt would hang.
        args.push("--auto".into());
    }

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    // Continue OpenCode's own session when Argo holds a valid handle; otherwise
    // this turn is a fresh session seeded from the canonical transcript.
    if let Some(session) = &ctx.resume_session {
        args.push("--session".into());
        args.push(session.clone());
    }

    args
}

/// Parses `opencode models` output.
///
/// Every line is a `provider/model` identifier, so the shape is simple — but the
/// list runs to hundreds of entries, and a picker of 475 rows is unusable. Only
/// well-formed ids are kept; the caller is responsible for presenting them.
fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let mut out = vec![ModelOption::labeled("default", "default (CLI configured)")];
    let mut seen: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let id = line.trim();
        let valid = id.contains('/')
            && !id.contains(char::is_whitespace)
            && id.len() <= 128
            && !id.starts_with('/')
            && !id.ends_with('/');
        if valid && !seen.contains(&id.to_string()) {
            seen.push(id.to_string());
            out.push(ModelOption::new(id));
        }
    }
    out
}

/// OpenCode adapter definition.
pub const OPENCODE: RuntimeDef = RuntimeDef {
    id: "opencode",
    name: "OpenCode",
    bin: "opencode",
    fallback_bins: &["opencode-cli"],
    version_args: &["--version"],
    help_args: &["run", "--help"],
    model_probe: Some(ModelProbe {
        args: &["models"],
        // The list is long; allow a little more time than the other probes.
        timeout_ms: 15_000,
        parse: parse_models,
        parse_reasoning: None,
    }),
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("anthropic/claude-sonnet-4-5", "anthropic/claude-sonnet-4-5"),
        ("openai/gpt-5.6", "openai/gpt-5.6"),
    ],
    reasoning_options: &[],
    auth_probe: None,
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::JsonEventStream,
        prompt_delivery: PromptDelivery::Stdin,
        prompt_encoding: PromptEncoding::Raw,
        native_resume: true,
        // OpenCode mints the id itself and stamps it on stream events.
        captures_session: true,
        mcp_injection: McpInjection::OpenCodeSharedConfig,
        supports_images: true,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport {
            plan: true,
            accept_edits: false,
            read_only: false,
        },
    },
    install_url: "https://opencode.ai/docs/",
};

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> InvocationContext {
        InvocationContext {
            cwd: "/repo".into(),
            ..Default::default()
        }
    }

    #[test]
    fn create_turn_requests_json_events_and_auto_approval() {
        let args = OPENCODE.args_for(&ctx());
        assert_eq!(args[0], "run");
        assert!(args.windows(2).any(|w| w == ["--format", "json"]));
        // Without --auto a permission prompt would hang the headless turn.
        assert!(args.contains(&"--auto".to_string()));
        assert!(!args.contains(&"--session".to_string()));
    }

    #[test]
    fn plan_mode_selects_the_plan_agent_instead_of_auto_approving() {
        let args = OPENCODE.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--agent", "plan"]));
        assert!(!args.contains(&"--auto".to_string()));
    }

    #[test]
    fn resume_turn_continues_the_captured_session() {
        let args = OPENCODE.args_for(&InvocationContext {
            resume_session: Some("ses_abc123".into()),
            ..ctx()
        });
        let index = args
            .iter()
            .position(|a| a == "--session")
            .expect("--session");
        assert_eq!(args[index + 1], "ses_abc123");
    }

    #[test]
    fn model_is_forwarded_only_when_concrete() {
        assert!(!OPENCODE
            .args_for(&InvocationContext {
                model: Some("default".into()),
                ..ctx()
            })
            .contains(&"--model".to_string()));

        let args = OPENCODE.args_for(&InvocationContext {
            model: Some("anthropic/claude-sonnet-4-5".into()),
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|w| w == ["--model", "anthropic/claude-sonnet-4-5"]));
    }

    #[test]
    fn model_discovery_keeps_provider_qualified_ids() {
        // Matches the real output shape: one `provider/model` per line.
        let stdout = "opencode/big-pickle\n\
                      amazon-bedrock/amazon.nova-2-lite-v1:0\n\
                      anthropic/claude-sonnet-4-5\n";
        let models = parse_models(stdout);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids[0], "default");
        assert!(ids.contains(&"opencode/big-pickle"));
        assert!(ids.contains(&"amazon-bedrock/amazon.nova-2-lite-v1:0"));
    }

    #[test]
    fn model_discovery_rejects_prose_and_malformed_ids() {
        let stdout = "Available models:\n\
                      no-slash-here\n\
                      /leading\n\
                      trailing/\n\
                      has a space/model\n\
                      good/one\n";
        let ids: Vec<String> = parse_models(stdout).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["default".to_string(), "good/one".to_string()]);
    }

    #[test]
    fn model_discovery_deduplicates() {
        let models = parse_models("a/b\na/b\n");
        assert_eq!(models.iter().filter(|m| m.id == "a/b").count(), 1);
    }

    #[test]
    fn capabilities_match_the_verified_interface() {
        const { assert!(OPENCODE.capabilities.native_resume) };
        const { assert!(OPENCODE.capabilities.captures_session) };
        // No MCP injection path is wired yet, so OpenCode can be a delegation
        // target but cannot host Argo's delegation tools.
        const { assert!(!OPENCODE.capabilities.can_delegate()) };
        const {
            assert!(OPENCODE
                .capabilities
                .stream_format
                .has_structured_tool_events())
        };
    }
}
