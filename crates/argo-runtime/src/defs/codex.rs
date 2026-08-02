//! Codex CLI adapter.
//!
//! Verified against OpenAI's official documentation: `codex exec --json` turns
//! stdout into a JSONL event stream (`thread.started`, `turn.*`, `item.*`,
//! `error`) and `codex exec resume <SESSION_ID>` continues a persisted thread.
//!
//! Create and resume accept different flags. `-C`, `--add-dir`, and `--sandbox`
//! are create-only; a resume turn must express sandbox policy through `-c
//! sandbox_mode=...` instead, or Codex rejects the invocation.

use crate::def::{AuthProbe, InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::{AgentMode, ModeSupport};
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, ReasoningOption, StreamFormat,
};

/// Sandbox mode Argo requests, matching the selected full-bypass posture.
const SANDBOX_MODE: &str = "danger-full-access";

/// Sandbox mode for the requested execution mode.
///
/// Codex expresses authority through its sandbox rather than a permission flag,
/// so Argo's plan directive is enforced with Codex's read-only sandbox.
fn sandbox_for(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::ReadOnly | AgentMode::Plan => "read-only",
        AgentMode::AcceptEdits => "workspace-write",
        AgentMode::Full => SANDBOX_MODE,
    }
}

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    let resuming = ctx.resume_session.is_some();
    let sandbox = sandbox_for(ctx.mode);
    let mut args: Vec<String> = vec!["exec".into()];

    if resuming {
        args.push("resume".into());
    }

    args.push("--json".into());
    // Codex refuses to run outside a git repository; Argo workspaces are not
    // always repositories, so the check is skipped explicitly.
    args.push("--skip-git-repo-check".into());

    if resuming {
        // Create-only flags are invalid here; sandbox policy moves to -c.
        args.push("-c".into());
        args.push(format!("sandbox_mode={sandbox}"));
    } else {
        args.push("--sandbox".into());
        args.push(sandbox.into());
        args.push("-C".into());
        args.push(ctx.cwd.clone());
        for dir in &ctx.extra_dirs {
            args.push("--add-dir".into());
            args.push(dir.clone());
        }
    }

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    // Codex takes reasoning effort as a config override rather than a flag; the key
    // is the same one `~/.codex/config.toml` uses.
    if let Some(effort) = &ctx.reasoning {
        args.push("-c".into());
        args.push(format!("model_reasoning_effort=\"{effort}\""));
    }

    // Codex accepts MCP servers only through its normal TOML configuration
    // hierarchy. Argo supplies one inline-table override so nothing is
    // persisted into the user's ~/.codex/config.toml.
    for config in &ctx.mcp_overrides {
        args.push("-c".into());
        args.push(config.clone());
    }

    // The resume target is a positional argument and must come last.
    if let Some(session) = &ctx.resume_session {
        args.push(session.clone());
    }

    args
}

/// Parses `codex debug models` output.
///
/// The command emits one large JSON document — not one model per line — so an
/// earlier line-based parser silently produced nothing and fell back to
/// hardcoded names. The shape is:
///
/// ```json
/// { "models": [ { "slug": "...", "display_name": "...", "visibility": "list",
///                 "supported_reasoning_levels": [ { "effort": "low" } ] } ] }
/// ```
///
/// Models marked `visibility: "hide"` are internal (for example the auto-review
/// model) and must not appear in a picker.
fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let mut out = vec![ModelOption::labeled("default", "default (CLI configured)")];

    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return out;
    };
    let Some(models) = value.get("models").and_then(|m| m.as_array()) else {
        return out;
    };

    for model in models {
        let Some(slug) = model.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        if model.get("visibility").and_then(|v| v.as_str()) == Some("hide") {
            continue;
        }
        let label = model
            .get("display_name")
            .and_then(|v| v.as_str())
            .filter(|name| !name.is_empty())
            .map(|name| format!("{name} ({slug})"))
            .unwrap_or_else(|| slug.to_string());
        out.push(ModelOption::labeled(slug, label));
    }
    out
}

/// Extracts each model's supported reasoning levels from `codex debug models`.
///
/// Reasoning levels are a property of the model, not of the CLI: `gpt-5.6-sol`
/// advertises six levels including `ultra`, which a hardcoded low/medium/high
/// list would hide.
pub fn parse_model_reasoning(stdout: &str) -> Vec<(String, Vec<ReasoningOption>)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return Vec::new();
    };
    let Some(models) = value.get("models").and_then(|m| m.as_array()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for model in models {
        let Some(slug) = model.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        if model.get("visibility").and_then(|v| v.as_str()) == Some("hide") {
            continue;
        }
        let levels: Vec<ReasoningOption> = model
            .get("supported_reasoning_levels")
            .and_then(|l| l.as_array())
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| {
                        let effort = level.get("effort").and_then(|v| v.as_str())?;
                        let description = level
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        Some(ReasoningOption {
                            id: effort.to_string(),
                            label: if description.is_empty() {
                                effort.to_string()
                            } else {
                                format!("{effort} — {description}")
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !levels.is_empty() {
            out.push((slug.to_string(), levels));
        }
    }
    out
}

/// Codex CLI adapter definition.
pub const CODEX: RuntimeDef = RuntimeDef {
    id: "codex",
    name: "Codex CLI",
    bin: "codex",
    fallback_bins: &[],
    version_args: &["--version"],
    help_args: &["exec", "--help"],
    model_probe: Some(ModelProbe {
        args: &["debug", "models"],
        timeout_ms: 10_000,
        parse: parse_models,
        parse_reasoning: Some(parse_model_reasoning),
    }),
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("gpt-5.6-sol", "GPT-5.6-Sol (gpt-5.6-sol)"),
        ("gpt-5.6-terra", "GPT-5.6-Terra (gpt-5.6-terra)"),
    ],
    // Superseded by per-model levels from discovery; used only when the probe
    // fails. Verified against `codex debug models`.
    reasoning_options: &[
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("xhigh", "xhigh"),
        ("max", "max"),
        ("ultra", "ultra"),
    ],
    auth_probe: Some(AuthProbe {
        args: &["login", "status"],
        timeout_ms: 10_000,
    }),
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::JsonEventStream,
        prompt_encoding: PromptEncoding::Raw,
        prompt_delivery: PromptDelivery::Stdin,
        native_resume: true,
        captures_session: true,
        mcp_injection: McpInjection::CodexConfig,
        supports_images: true,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport {
            plan: true,
            accept_edits: true,
            read_only: true,
        },
    },
    install_url: "https://developers.openai.com/codex/cli",
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
    fn create_turn_uses_create_only_flags() {
        let args = CODEX.args_for(&ctx());
        assert_eq!(args[0], "exec");
        assert!(!args.contains(&"resume".to_string()));
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"--skip-git-repo-check".to_string()));
        assert!(args.windows(2).any(|w| w == ["--sandbox", SANDBOX_MODE]));
        assert!(args.windows(2).any(|w| w == ["-C", "/repo"]));
    }

    #[test]
    fn resume_turn_drops_create_only_flags_and_moves_sandbox_to_config() {
        // Codex rejects --sandbox, -C, and --add-dir on `exec resume`.
        let args = CODEX.args_for(&InvocationContext {
            resume_session: Some("thread-1".into()),
            extra_dirs: vec!["/skills".into()],
            ..ctx()
        });
        assert_eq!(args[0], "exec");
        assert_eq!(args[1], "resume");
        assert!(!args.contains(&"--sandbox".to_string()));
        assert!(!args.contains(&"-C".to_string()));
        assert!(!args.contains(&"--add-dir".to_string()));
        assert!(args.contains(&format!("sandbox_mode={SANDBOX_MODE}")));
        // The thread id is positional and last.
        assert_eq!(args.last().expect("last"), "thread-1");
    }

    #[test]
    fn the_mode_selects_the_sandbox() {
        // Codex expresses authority through the sandbox, not a permission flag.
        assert_eq!(sandbox_for(AgentMode::Full), SANDBOX_MODE);
        assert_eq!(sandbox_for(AgentMode::Plan), "read-only");
        assert_eq!(sandbox_for(AgentMode::AcceptEdits), "workspace-write");
        assert_eq!(sandbox_for(AgentMode::ReadOnly), "read-only");

        let args = CODEX.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
    }

    #[test]
    fn the_mode_survives_a_resume_turn_via_config() {
        // Create-only flags are invalid on resume, so the sandbox moves to -c.
        let args = CODEX.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            resume_session: Some("t-1".into()),
            ..ctx()
        });
        assert!(args.contains(&"sandbox_mode=read-only".to_string()));
        assert!(!args.contains(&"--sandbox".to_string()));
    }

    #[test]
    fn shift_tab_cycle_can_enter_codex_plan_mode() {
        let support = CODEX.capabilities.modes;
        assert!(support.plan);
        assert_eq!(support.next_supported(AgentMode::Full), AgentMode::Plan);
    }

    #[test]
    fn model_is_forwarded_only_when_concrete() {
        assert!(!CODEX
            .args_for(&InvocationContext {
                model: Some("default".into()),
                ..ctx()
            })
            .contains(&"--model".to_string()));
        let args = CODEX.args_for(&InvocationContext {
            model: Some("gpt-5.6-codex".into()),
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5.6-codex"]));
    }

    #[test]
    fn reasoning_effort_is_passed_as_a_config_override() {
        // Codex has no --effort flag; it reads model_reasoning_effort from config.
        // The adapter previously declared levels but dropped the value entirely.
        let args = CODEX.args_for(&InvocationContext {
            reasoning: Some("xhigh".into()),
            ..ctx()
        });
        assert!(args.contains(&"model_reasoning_effort=\"xhigh\"".to_string()));
    }

    #[test]
    fn reasoning_survives_a_resume_turn() {
        let args = CODEX.args_for(&InvocationContext {
            reasoning: Some("max".into()),
            resume_session: Some("t-1".into()),
            ..ctx()
        });
        assert!(args.contains(&"model_reasoning_effort=\"max\"".to_string()));
        // The thread id must remain the last positional argument.
        assert_eq!(args.last().expect("last"), "t-1");
    }

    #[test]
    fn no_config_override_when_no_effort_is_selected() {
        let args = CODEX.args_for(&ctx());
        assert!(!args.iter().any(|a| a.contains("model_reasoning_effort")));
    }

    #[test]
    fn native_mcp_overrides_are_forwarded_to_codex() {
        let override_value =
            "mcp_servers={\"argo\"={command=\"/usr/bin/argo\",args=[\"mcp-server\"]}}";
        let args = CODEX.args_for(&InvocationContext {
            mcp_overrides: vec![override_value.into()],
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|window| window == ["-c", override_value]));
        assert!(!args.iter().any(|arg| arg.contains("mcp_servers_file")));
    }

    #[test]
    fn extra_dirs_are_passed_on_create_turns() {
        let args = CODEX.args_for(&InvocationContext {
            extra_dirs: vec!["/skills".into(), "/shared".into()],
            ..ctx()
        });
        assert_eq!(args.iter().filter(|a| *a == "--add-dir").count(), 2);
    }

    /// A trimmed copy of the real `codex debug models` shape.
    const MODELS_JSON: &str = r#"{"models":[
        {"slug":"gpt-5.6-sol","display_name":"GPT-5.6-Sol","visibility":"list",
         "default_reasoning_level":"low",
         "supported_reasoning_levels":[
            {"effort":"low","description":"Fast responses with lighter reasoning"},
            {"effort":"medium","description":"Balances speed and reasoning depth"},
            {"effort":"high","description":"Greater reasoning depth"},
            {"effort":"xhigh","description":"Extra high reasoning depth"},
            {"effort":"max","description":"Maximum reasoning depth"},
            {"effort":"ultra","description":"Maximum reasoning with delegation"}]},
        {"slug":"gpt-5.6-terra","display_name":"GPT-5.6-Terra","visibility":"list",
         "supported_reasoning_levels":[{"effort":"medium","description":"Balanced"}]},
        {"slug":"codex-auto-review","display_name":"Codex Auto Review","visibility":"hide",
         "supported_reasoning_levels":[{"effort":"medium","description":"Balanced"}]}
    ]}"#;

    #[test]
    fn model_discovery_parses_the_real_json_document() {
        // The command emits one large JSON object, not one model per line. A
        // line-based parser silently produced nothing and fell back to hardcoded
        // names, which is what made the picker wrong.
        let models = parse_models(MODELS_JSON);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["default", "gpt-5.6-sol", "gpt-5.6-terra"]);
        // Display names make the picker readable.
        assert_eq!(models[1].label, "GPT-5.6-Sol (gpt-5.6-sol)");
    }

    #[test]
    fn reasoning_levels_are_read_per_model() {
        // Levels belong to the model: sol advertises six, terra advertises one.
        // A single hardcoded list would have hidden `xhigh`, `max`, and `ultra`.
        let mapping = parse_model_reasoning(MODELS_JSON);
        let sol = mapping
            .iter()
            .find(|(slug, _)| slug == "gpt-5.6-sol")
            .expect("sol");
        let ids: Vec<&str> = sol.1.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["low", "medium", "high", "xhigh", "max", "ultra"]);
        assert!(sol.1[0].label.contains("Fast responses"));

        let terra = mapping
            .iter()
            .find(|(slug, _)| slug == "gpt-5.6-terra")
            .expect("terra");
        assert_eq!(terra.1.len(), 1);

        // Hidden models contribute no levels.
        assert!(!mapping.iter().any(|(slug, _)| slug == "codex-auto-review"));
    }

    #[test]
    fn malformed_discovery_output_falls_back_to_the_default_only() {
        assert_eq!(parse_models("not json at all").len(), 1);
        assert!(parse_model_reasoning("not json at all").is_empty());
        assert_eq!(parse_models("{\"unexpected\":true}").len(), 1);
    }

    #[test]
    fn empty_discovery_output_still_offers_the_default() {
        let models = parse_models("");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "default");
    }
}
