//! Antigravity adapter.
//!
//! Verified against the installed CLI (`agy` 1.1.9). `agy -p` runs one prompt
//! non-interactively, `--conversation <id>` resumes a previous conversation,
//! `--mode` selects `plan` or `accept-edits`, and `--dangerously-skip-permissions`
//! bypasses approval prompts for headless use.
//!
//! Output uses `--output-format stream-json`, whose init record exposes the
//! conversation id and whose step records expose text/tool/file activity. Argo
//! persists that id and resumes with `--conversation <id>` on the next unchanged
//! turn.

use crate::def::{InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::{AgentMode, ModeSupport};
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, ReasoningOption, StreamFormat,
};

const EFFORT_LEVELS: &[(&str, &str)] = &[("low", "low"), ("medium", "medium"), ("high", "high")];

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    // `--print` is a string flag whose *next value is the prompt*. It must be
    // appended last; putting flags after it makes the model answer the flag text
    // (for example `--dangerously-skip-permissions`) instead of the user's message.
    let mut args: Vec<String> = Vec::new();

    // Argo runs children without a TTY, so an approval prompt would hang the turn.
    // Plan mode is the exception: withholding the bypass is what makes it real.
    if !ctx.mode.is_read_only() {
        args.push("--dangerously-skip-permissions".into());
    }

    // `--mode` accepts only these two values; Full is the CLI's own default.
    match ctx.mode {
        AgentMode::Plan => {
            args.push("--mode".into());
            args.push("plan".into());
        }
        AgentMode::AcceptEdits => {
            args.push("--mode".into());
            args.push("accept-edits".into());
        }
        AgentMode::Full | AgentMode::ReadOnly => {}
    }

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    if let Some(effort) = &ctx.reasoning {
        args.push("--effort".into());
        args.push(effort.clone());
    }

    if let Some(conversation) = &ctx.resume_session {
        args.push("--conversation".into());
        args.push(conversation.clone());
    }

    for dir in &ctx.extra_dirs {
        args.push("--add-dir".into());
        args.push(dir.clone());
    }

    args.push("--output-format".into());
    args.push("stream-json".into());
    args.push("--print".into());
    args.push(ctx.prompt.clone().unwrap_or_default());

    args
}

/// Parses `agy models` output.
///
/// Each line is a human-readable model name such as `Gemini 3.6 Flash (High)`,
/// which is also the value `--model` expects, so names are kept verbatim.
fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let mut out = vec![ModelOption::labeled("default", "default (CLI configured)")];
    let mut seen: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let name = line.trim();
        // Reject banners and blank lines: a real entry names a model and is not a
        // sentence.
        let valid = !name.is_empty()
            && name.len() <= 96
            && !name.ends_with(':')
            && name.split_whitespace().count() <= 6
            && name
                .chars()
                .next()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false);
        if valid && !seen.contains(&name.to_string()) {
            seen.push(name.to_string());
            out.push(ModelOption::new(name));
        }
    }
    out
}

/// Returns effort controls only for Antigravity models with adjustable effort.
///
/// The current Gemini and GPT-OSS inventory publishes separate model ids ending
/// in `-low`, `-medium`, or `-high`; choosing one already fixes the effort. The
/// Claude entries instead use the CLI's independent `--effort` session control.
fn parse_model_reasoning(stdout: &str) -> Vec<(String, Vec<ReasoningOption>)> {
    parse_models(stdout)
        .into_iter()
        .filter(|model| model.id.to_ascii_lowercase().starts_with("claude-"))
        .map(|model| {
            let levels = EFFORT_LEVELS
                .iter()
                .map(|(id, label)| ReasoningOption {
                    id: (*id).to_string(),
                    label: (*label).to_string(),
                })
                .collect();
            (model.id, levels)
        })
        .collect()
}

/// Antigravity adapter definition.
pub const ANTIGRAVITY: RuntimeDef = RuntimeDef {
    id: "antigravity",
    name: "Antigravity",
    bin: "agy",
    fallback_bins: &["antigravity"],
    version_args: &["--version"],
    help_args: &["--help"],
    model_probe: Some(ModelProbe {
        args: &["models"],
        timeout_ms: 15_000,
        parse: parse_models,
        parse_reasoning: Some(parse_model_reasoning),
    }),
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("gemini-3.6-flash-high", "gemini-3.6-flash-high"),
        ("gemini-3.6-flash-medium", "gemini-3.6-flash-medium"),
        ("gemini-3.6-flash-low", "gemini-3.6-flash-low"),
        ("gemini-3.5-flash-high", "gemini-3.5-flash-high"),
        ("gemini-3.5-flash-medium", "gemini-3.5-flash-medium"),
        ("gemini-3.5-flash-low", "gemini-3.5-flash-low"),
        ("gemini-3.1-pro-high", "gemini-3.1-pro-high"),
        ("gemini-3.1-pro-low", "gemini-3.1-pro-low"),
        ("claude-sonnet-4-6", "claude-sonnet-4-6"),
        ("claude-opus-4-6-thinking", "claude-opus-4-6-thinking"),
        ("gpt-oss-120b-medium", "gpt-oss-120b-medium"),
    ],
    // Antigravity effort is model-specific; see `parse_model_reasoning`.
    reasoning_options: &[],
    auth_probe: None,
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::AntigravityStreamJson,
        prompt_delivery: PromptDelivery::Argument,
        prompt_encoding: PromptEncoding::Raw,
        // The init record exposes `conversation_id`, and `--conversation <id>`
        // continues it exactly.
        native_resume: true,
        captures_session: true,
        mcp_injection: McpInjection::GeminiSharedConfig,
        supports_images: false,
        permission: PermissionPosture::FullBypass,
        // `--mode` offers exactly plan and accept-edits.
        modes: ModeSupport::PLAN_AND_EDITS,
    },
    install_url: "https://antigravity.google",
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
    fn a_default_turn_runs_non_interactively_with_the_bypass() {
        let args = ANTIGRAVITY.args_for(&ctx());
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-format", "stream-json"]));
        // --print consumes the following value, so it and the prompt must be last.
        assert_eq!(&args[args.len() - 2..], ["--print", "do the thing"]);
        // Full is the CLI's own default, so no --mode is passed.
        assert!(!args.contains(&"--mode".to_string()));
    }

    #[test]
    fn plan_mode_withholds_the_permission_bypass() {
        // Passing plan mode while also bypassing permissions would make the
        // restriction meaningless.
        let args = ANTIGRAVITY.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--mode", "plan"]));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn accept_edits_mode_keeps_the_bypass() {
        let args = ANTIGRAVITY.args_for(&InvocationContext {
            mode: AgentMode::AcceptEdits,
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--mode", "accept-edits"]));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn model_and_conversation_are_forwarded() {
        let args = ANTIGRAVITY.args_for(&InvocationContext {
            model: Some("gemini-3.1-pro-high".into()),
            resume_session: Some("conv-42".into()),
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|w| w == ["--model", "gemini-3.1-pro-high"]));
        assert!(args.windows(2).any(|w| w == ["--conversation", "conv-42"]));
    }

    #[test]
    fn the_default_model_sentinel_is_not_forwarded() {
        let args = ANTIGRAVITY.args_for(&InvocationContext {
            model: Some("default".into()),
            ..ctx()
        });
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn model_discovery_keeps_ids_verbatim() {
        let stdout = "gemini-3.6-flash-high\n\
                      gemini-3.1-pro-low\n\
                      claude-sonnet-4-6\n";
        let ids: Vec<String> = parse_models(stdout).into_iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![
                "default".to_string(),
                "gemini-3.6-flash-high".to_string(),
                "gemini-3.1-pro-low".to_string(),
                "claude-sonnet-4-6".to_string(),
            ]
        );
    }

    #[test]
    fn model_discovery_rejects_banners_and_prose() {
        let stdout = "Available models:\n\
                      gemini-3.1-pro-high\n\
                      this line is a long sentence that is clearly not a model name\n";
        let ids: Vec<String> = parse_models(stdout).into_iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec!["default".to_string(), "gemini-3.1-pro-high".to_string()]
        );
    }

    #[test]
    fn effort_is_offered_only_for_adjustable_antigravity_models() {
        let stdout = "gemini-3.6-flash-high\n\
                      claude-sonnet-4-6\n\
                      claude-opus-4-6-thinking\n\
                      gpt-oss-120b-medium\n";
        let reasoning = parse_model_reasoning(stdout);
        let models: Vec<&str> = reasoning.iter().map(|(model, _)| model.as_str()).collect();
        assert_eq!(
            models,
            vec!["claude-sonnet-4-6", "claude-opus-4-6-thinking"]
        );
        assert!(reasoning.iter().all(|(_, levels)| {
            levels
                .iter()
                .map(|level| level.id.as_str())
                .collect::<Vec<_>>()
                == ["low", "medium", "high"]
        }));
    }

    #[test]
    fn capabilities_reflect_the_verified_interface() {
        // stream-json exposes conversation ids, text deltas, tools, and files.
        const {
            assert!(ANTIGRAVITY
                .capabilities
                .stream_format
                .has_structured_tool_events())
        };
        const { assert!(ANTIGRAVITY.capabilities.native_resume) };
        const { assert!(ANTIGRAVITY.capabilities.captures_session) };
        // But it does honor plan and accept-edits.
        const { assert!(ANTIGRAVITY.capabilities.modes.plan) };
        const { assert!(ANTIGRAVITY.capabilities.modes.accept_edits) };
        const { assert!(!ANTIGRAVITY.capabilities.modes.read_only) };
    }
}
