//! Command Code adapter.
//! Verified against the installed CLI (0.41.1). `cmd -p "<query>"` runs one
//! non-interactive turn, `--yolo` bypasses permission prompts, `-m` selects a
//! model, and `--list-models` reports provider-qualified models.
//!
//! Command Code has no structured stdout format, so tool events are not
//! available. It does, however, persist each session under
//! `~/.commandcode/projects/<workspace-slug>/<session-id>.jsonl`. Argo discovers
//! the file created by a successful fresh turn and uses its id with `--resume`.

use crate::def::{AuthProbe, InvocationContext, ModelProbe, RuntimeDef};
use argo_core::mode::{AgentMode, ModeSupport};
use argo_core::runtime::{
    AgentCapabilities, McpInjection, ModelOption, PermissionPosture, PromptDelivery,
    PromptEncoding, StreamFormat,
};
use serde_json::Value;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    // `-p` enables one non-interactive turn; the engine supplies the prompt.
    let mut args: Vec<String> = vec!["-p".into()];

    if let Some(session_id) = &ctx.resume_session {
        args.push("--resume".into());
        args.push(session_id.clone());
    }

    // Onboarding and trust prompts would hang a headless turn in every mode.
    args.push("--skip-onboarding".into());
    args.push("--trust".into());

    match ctx.mode {
        AgentMode::Plan => {
            // Withholding --yolo is what makes plan mode real.
            args.push("--permission-mode".into());
            args.push("plan".into());
        }
        AgentMode::AcceptEdits => {
            args.push("--permission-mode".into());
            args.push("auto-accept".into());
        }
        AgentMode::Full | AgentMode::ReadOnly => args.push("--yolo".into()),
    }

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    for dir in &ctx.extra_dirs {
        args.push("--add-dir".into());
        args.push(dir.clone());
    }

    args
}

fn workspace_slug(cwd: &str) -> String {
    Path::new(cwd)
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy()),
            _ => None,
        })
        .flat_map(|part| {
            part.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() {
                        c.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .chain(std::iter::once('-'))
                .collect::<Vec<_>>()
        })
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

fn session_id_from_file(path: &Path) -> Option<String> {
    let first = fs::read_to_string(path).ok()?.lines().next()?.to_string();
    serde_json::from_str::<Value>(&first)
        .ok()?
        .get("sessionId")?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn command_code_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".commandcode/projects"))
}

/// Finds the Command Code session created for this workspace and fresh turn.
///
/// This is model-independent: all models exposed by `cmd --list-models` use the
/// same local session store and resume flag.
fn capture_session(ctx: &InvocationContext, started_at_ms: i64) -> Option<String> {
    let project = command_code_root()?.join(workspace_slug(&ctx.cwd));
    let mut newest: Option<(u128, PathBuf)> = None;
    for entry in fs::read_dir(project).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            || path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.ends_with(".checkpoints.jsonl"))
        {
            continue;
        }
        let modified = entry
            .metadata()
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis();
        if modified + 1_000 < started_at_ms.max(0) as u128 {
            continue;
        }
        if newest.as_ref().is_none_or(|(known, _)| modified > *known) {
            newest = Some((modified, path));
        }
    }
    session_id_from_file(&newest?.1)
}

/// Parses `cmd --list-models` output.
///
/// The listing is grouped under provider headings, with each model line holding a
/// `provider/model` identifier followed by a prose description. Only the leading
/// identifier is kept, so headings and descriptions cannot become selectable.
fn parse_models(stdout: &str) -> Vec<ModelOption> {
    let mut out = vec![ModelOption::labeled("default", "default (CLI configured)")];
    let mut seen: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(id) = parts.next() else { continue };

        // Identifiers are provider-qualified; headings such as "Open Source" and
        // the "Available models · 35 models" banner are not.
        let valid = id.contains('/')
            && id.len() <= 128
            && !id.starts_with('/')
            && !id.ends_with('/')
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/'));
        if !valid || seen.contains(&id.to_string()) {
            continue;
        }

        let description = parts.collect::<Vec<_>>().join(" ");
        // Mark the CLI's own default so the picker does not look arbitrary.
        let label = if description.contains("(default)") {
            format!("{id} (CLI default)")
        } else if description.is_empty() {
            id.to_string()
        } else {
            format!("{id} — {description}")
        };

        seen.push(id.to_string());
        out.push(ModelOption::labeled(id, label));
    }
    out
}

/// Command Code adapter definition.
pub const COMMAND_CODE: RuntimeDef = RuntimeDef {
    id: "cmd",
    name: "Command Code",
    bin: "cmd",
    fallback_bins: &["commandcode"],
    version_args: &["--version"],
    help_args: &["--help"],
    model_probe: Some(ModelProbe {
        args: &["--list-models"],
        timeout_ms: 15_000,
        parse: parse_models,
        parse_reasoning: None,
    }),
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("deepseek/deepseek-v4-flash", "deepseek/deepseek-v4-flash"),
        ("zai-org/GLM-5.2", "zai-org/GLM-5.2"),
    ],
    reasoning_options: &[],
    auth_probe: Some(AuthProbe {
        args: &["status"],
        timeout_ms: 10_000,
    }),
    build_args,
    capture_session: Some(capture_session),
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::Plain,
        // `-p` enables one non-interactive turn; the prompt is written to stdin.
        prompt_delivery: PromptDelivery::Stdin,
        prompt_encoding: PromptEncoding::Raw,
        // Every Command Code model uses the same sidecar-backed session protocol.
        native_resume: true,
        captures_session: true,
        mcp_injection: McpInjection::CommandCodeSharedConfig,
        supports_images: false,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport::PLAN_AND_EDITS,
    },
    install_url: "https://commandcode.ai",
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
    fn a_turn_runs_non_interactively_without_prompts() {
        let args = COMMAND_CODE.args_for(&ctx());
        assert_eq!(args[0], "-p");
        // Any of these prompts would hang a headless turn.
        assert!(args.contains(&"--yolo".to_string()));
        assert!(args.contains(&"--skip-onboarding".to_string()));
        assert!(args.contains(&"--trust".to_string()));
    }

    #[test]
    fn plan_mode_withholds_the_permission_bypass() {
        let args = COMMAND_CODE.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--permission-mode", "plan"]));
        assert!(!args.contains(&"--yolo".to_string()));
        // Onboarding must still be skipped or the turn hangs.
        assert!(args.contains(&"--skip-onboarding".to_string()));
    }

    #[test]
    fn accept_edits_mode_uses_auto_accept() {
        let args = COMMAND_CODE.args_for(&InvocationContext {
            mode: AgentMode::AcceptEdits,
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|w| w == ["--permission-mode", "auto-accept"]));
    }

    #[test]
    fn model_is_forwarded_only_when_concrete() {
        assert!(!COMMAND_CODE
            .args_for(&InvocationContext {
                model: Some("default".into()),
                ..ctx()
            })
            .contains(&"--model".to_string()));

        let args = COMMAND_CODE.args_for(&InvocationContext {
            model: Some("zai-org/GLM-5.2".into()),
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--model", "zai-org/GLM-5.2"]));
    }

    #[test]
    fn extra_directories_are_passed_through() {
        let args = COMMAND_CODE.args_for(&InvocationContext {
            extra_dirs: vec!["/skills".into(), "/shared".into()],
            ..ctx()
        });
        assert_eq!(args.iter().filter(|a| *a == "--add-dir").count(), 2);
    }

    #[test]
    fn model_discovery_keeps_identifiers_and_drops_headings() {
        // Matches the real grouped output shape.
        let stdout = "Available models  ·  35 models\n\n\
                      Open Source\n\n\
                      deepseek/deepseek-v4-pro             hybrid-attention long-context reasoning\n\
                      deepseek/deepseek-v4-flash           fast hybrid-attention reasoning (default)\n\
                      zai-org/GLM-5.2                      powerful coding with 1M context\n";
        let models = parse_models(stdout);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "default",
                "deepseek/deepseek-v4-pro",
                "deepseek/deepseek-v4-flash",
                "zai-org/GLM-5.2"
            ]
        );
        // Headings must never become selectable models.
        assert!(!ids.iter().any(|id| id.contains("Available")));
        assert!(!ids.contains(&"Open"));
    }

    #[test]
    fn descriptions_are_kept_as_labels_and_the_cli_default_is_marked() {
        let stdout = "deepseek/deepseek-v4-flash  fast reasoning (default)\n\
                      zai-org/GLM-5.2             powerful coding\n";
        let models = parse_models(stdout);
        let flash = models
            .iter()
            .find(|m| m.id == "deepseek/deepseek-v4-flash")
            .expect("flash");
        assert!(flash.label.contains("CLI default"));
        let glm = models
            .iter()
            .find(|m| m.id == "zai-org/GLM-5.2")
            .expect("glm");
        assert!(glm.label.contains("powerful coding"));
    }

    #[test]
    fn model_discovery_deduplicates() {
        let models = parse_models("a/b desc\na/b desc\n");
        assert_eq!(models.iter().filter(|m| m.id == "a/b").count(), 1);
    }

    #[test]
    fn resume_is_forwarded_for_every_model() {
        for model in [
            "MiniMaxAI/MiniMax-M3",
            "deepseek/deepseek-v4-flash",
            "zai-org/GLM-5.2",
        ] {
            let args = COMMAND_CODE.args_for(&InvocationContext {
                model: Some(model.into()),
                resume_session: Some("0de9c73a-c8f5-4e0b-9469-f4add37dc600".into()),
                ..ctx()
            });
            assert!(args
                .windows(2)
                .any(|pair| { pair == ["--resume", "0de9c73a-c8f5-4e0b-9469-f4add37dc600"] }));
        }
    }

    #[test]
    fn workspace_path_maps_to_command_code_project_slug() {
        assert_eq!(
            workspace_slug("/Users/matic/WORK/agentmux"),
            "users-matic-work-agentmux"
        );
    }

    #[test]
    fn reads_session_id_from_first_jsonl_record() {
        let path = std::env::temp_dir().join(format!(
            "argo-command-code-session-{}.jsonl",
            std::process::id()
        ));
        fs::write(
            &path,
            "{\"sessionId\":\"0de9c73a-c8f5-4e0b-9469-f4add37dc600\",\"role\":\"user\"}\n",
        )
        .expect("write fixture");
        assert_eq!(
            session_id_from_file(&path).as_deref(),
            Some("0de9c73a-c8f5-4e0b-9469-f4add37dc600")
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn capabilities_reflect_sidecar_resume() {
        // Plain stdout still has no per-tool events.
        const {
            assert!(!COMMAND_CODE
                .capabilities
                .stream_format
                .has_structured_tool_events())
        };
        // Session capture and resume apply uniformly to every selectable model.
        const { assert!(COMMAND_CODE.capabilities.native_resume) };
        const { assert!(COMMAND_CODE.capabilities.captures_session) };
        const { assert!(COMMAND_CODE.capabilities.can_delegate()) };
        const { assert!(!COMMAND_CODE.capabilities.delegates_via_mcp()) };
    }
}
