//! Claude Code adapter.
//!
//! Reference implementation for the structured-stream path. Verified surface:
//! `claude -p` with `--input-format stream-json --output-format stream-json`
//! emits JSONL that carries text, reasoning, tool calls, file writes, usage, and
//! the session id Argo persists for resume.

use crate::def::{AuthProbe, InvocationContext, RuntimeDef};
use argo_core::mode::{AgentMode, ModeSupport};
use argo_core::runtime::{
    AgentCapabilities, McpInjection, PermissionPosture, PromptDelivery, PromptEncoding,
    StreamFormat,
};

fn build_args(ctx: &InvocationContext) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        // Required for stream-json to emit incremental events rather than one
        // trailing blob.
        "--verbose".into(),
    ];

    // Richer streaming, but only in newer builds: an older install rejects the
    // unknown option and exits, killing the turn.
    if ctx.supports_flag("--include-partial-messages") {
        args.push("--include-partial-messages".into());
    }

    if let Some(model) = ctx.concrete_model() {
        args.push("--model".into());
        args.push(model.to_string());
    }

    // `--effort` is session-wide and independent of the model, so it is passed
    // whenever the user selected one.
    if let Some(effort) = &ctx.reasoning {
        args.push("--effort".into());
        args.push(effort.clone());
    }

    // Continue the CLI's own session when Argo holds a valid handle. Otherwise
    // mint the id here rather than hoping the stream discloses one: `--session-id`
    // makes the next turn's resume reliable even if no id is ever reported.
    match (&ctx.resume_session, &ctx.new_session) {
        (Some(session), _) => {
            args.push("--resume".into());
            args.push(session.clone());
        }
        (None, Some(fresh)) => {
            args.push("--session-id".into());
            args.push(fresh.clone());
        }
        (None, None) => {}
    }

    // Argo runs children without a TTY, so an interactive approval prompt would
    // hang the turn. The mode chooses how much authority that grants.
    args.push("--permission-mode".into());
    args.push(
        match ctx.mode {
            AgentMode::Plan => "plan",
            AgentMode::AcceptEdits => "acceptEdits",
            // Full and ReadOnly both fall back to the bypass: Claude has no
            // read-only permission mode, and ModeSupport declares that.
            AgentMode::Full | AgentMode::ReadOnly => "bypassPermissions",
        }
        .to_string(),
    );

    // Only pass extra roots when the installed build advertises the flag.
    if ctx.supports_flag("--add-dir") {
        for dir in &ctx.extra_dirs {
            args.push("--add-dir".into());
            args.push(dir.clone());
        }
    }

    if let Some(config) = &ctx.mcp_config {
        args.push("--mcp-config".into());
        args.push(config.clone());
        // Deliberately NOT `--strict-mcp-config`. That would make Claude use only
        // Argo's servers and silently drop the user's own — including OAuth-backed
        // ones Argo cannot authenticate and therefore cannot replace. Argo's job is
        // to add servers to every agent, not to take working ones away.
    }

    args
}

/// Claude Code adapter definition.
pub const CLAUDE: RuntimeDef = RuntimeDef {
    id: "claude",
    name: "Claude Code",
    bin: "claude",
    // Argv-compatible drop-in forks, so a single-binary install is still detected.
    fallback_bins: &["openclaude"],
    version_args: &["--version"],
    // `--add-dir` and `--include-partial-messages` appear only under `claude -p`,
    // so the global help would never reveal them.
    help_args: &["-p", "--help"],
    model_probe: None,
    // Claude Code exposes no model-listing command, so these are the aliases its
    // own `--help` documents. A full name such as `claude-fable-5` also works and
    // can be typed directly with `/model <name>`.
    fallback_models: &[
        ("default", "default (CLI configured)"),
        ("fable", "fable (latest)"),
        ("opus", "opus"),
        ("sonnet", "sonnet"),
        ("haiku", "haiku"),
        ("claude-opus-4-5", "claude-opus-4-5"),
        ("claude-sonnet-4-5", "claude-sonnet-4-5"),
        ("claude-haiku-4-5", "claude-haiku-4-5"),
    ],
    // Verified against `claude --help`: --effort <low|medium|high|xhigh|max>.
    reasoning_options: &[
        ("low", "low"),
        ("medium", "medium"),
        ("high", "high"),
        ("xhigh", "xhigh"),
        ("max", "max"),
    ],
    auth_probe: Some(AuthProbe {
        args: &["auth", "status"],
        timeout_ms: 8_000,
    }),
    build_args,
    capture_session: None,
    capabilities: AgentCapabilities {
        stream_format: StreamFormat::ClaudeStreamJson,
        prompt_encoding: PromptEncoding::StreamJsonUserMessage,
        prompt_delivery: PromptDelivery::Stdin,
        native_resume: true,
        captures_session: true,
        mcp_injection: McpInjection::ClaudeMcpJson,
        supports_images: true,
        permission: PermissionPosture::FullBypass,
        modes: ModeSupport {
            plan: true,
            accept_edits: true,
            read_only: false,
        },
    },
    install_url: "https://docs.anthropic.com/en/docs/claude-code",
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
    fn create_turn_uses_structured_streaming_and_bypasses_prompts() {
        let args = CLAUDE.args_for(&ctx());
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-format", "stream-json"]));
        assert!(args.contains(&"--verbose".to_string()));
        assert!(args
            .windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"]));
        // A create turn must not carry a resume handle.
        assert!(!args.contains(&"--resume".to_string()));
    }

    #[test]
    fn the_mode_selects_the_permission_mode() {
        let plan = CLAUDE.args_for(&InvocationContext {
            mode: AgentMode::Plan,
            ..ctx()
        });
        assert!(plan.windows(2).any(|w| w == ["--permission-mode", "plan"]));

        let edits = CLAUDE.args_for(&InvocationContext {
            mode: AgentMode::AcceptEdits,
            ..ctx()
        });
        assert!(edits
            .windows(2)
            .any(|w| w == ["--permission-mode", "acceptEdits"]));
    }

    #[test]
    fn a_fresh_turn_specifies_the_session_id_rather_than_hoping_for_one() {
        // Waiting to capture an id means resume is impossible when the stream never
        // reports one; specifying it removes that dependency.
        let args = CLAUDE.args_for(&InvocationContext {
            new_session: Some("11111111-2222-3333-4444-555555555555".into()),
            ..ctx()
        });
        let index = args
            .iter()
            .position(|a| a == "--session-id")
            .expect("--session-id");
        assert_eq!(args[index + 1], "11111111-2222-3333-4444-555555555555");
        assert!(!args.contains(&"--resume".to_string()));
    }

    #[test]
    fn resume_takes_priority_over_a_minted_id() {
        let args = CLAUDE.args_for(&InvocationContext {
            resume_session: Some("existing".into()),
            new_session: Some("fresh".into()),
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--resume", "existing"]));
        assert!(!args.contains(&"--session-id".to_string()));
    }

    #[test]
    fn partial_messages_are_gated_on_the_installed_build() {
        assert!(!CLAUDE
            .args_for(&ctx())
            .contains(&"--include-partial-messages".to_string()));
        let args = CLAUDE.args_for(&InvocationContext {
            help_flags: vec!["--include-partial-messages".into()],
            ..ctx()
        });
        assert!(args.contains(&"--include-partial-messages".to_string()));
    }

    #[test]
    fn mcp_injection_adds_servers_without_removing_the_users_own() {
        // `--strict-mcp-config` would discard servers configured in the CLI itself,
        // including OAuth-backed ones Argo has no way to authenticate.
        let args = CLAUDE.args_for(&InvocationContext {
            mcp_config: Some("/tmp/argo-mcp.json".into()),
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|w| w == ["--mcp-config", "/tmp/argo-mcp.json"]));
        assert!(!args.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn argv_compatible_forks_are_detected() {
        assert_eq!(CLAUDE.candidate_bins(), vec!["claude", "openclaude"]);
    }

    #[test]
    fn resume_turn_passes_the_stored_handle() {
        let args = CLAUDE.args_for(&InvocationContext {
            resume_session: Some("sess-A".into()),
            ..ctx()
        });
        let idx = args.iter().position(|a| a == "--resume").expect("--resume");
        assert_eq!(args[idx + 1], "sess-A");
    }

    #[test]
    fn default_model_is_not_forwarded() {
        let args = CLAUDE.args_for(&InvocationContext {
            model: Some("default".into()),
            ..ctx()
        });
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn concrete_model_is_forwarded() {
        let args = CLAUDE.args_for(&InvocationContext {
            model: Some("opus".into()),
            ..ctx()
        });
        let idx = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[idx + 1], "opus");
    }

    #[test]
    fn effort_is_forwarded_when_selected() {
        // Previously the adapter declared no effort levels and dropped the value,
        // so choosing one silently did nothing.
        let args = CLAUDE.args_for(&InvocationContext {
            reasoning: Some("xhigh".into()),
            ..ctx()
        });
        assert!(args.windows(2).any(|w| w == ["--effort", "xhigh"]));
    }

    #[test]
    fn no_effort_flag_when_none_is_selected() {
        assert!(!CLAUDE.args_for(&ctx()).contains(&"--effort".to_string()));
    }

    #[test]
    fn the_documented_effort_levels_are_offered() {
        let levels: Vec<&str> = CLAUDE.reasoning_options.iter().map(|(id, _)| *id).collect();
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn the_documented_model_aliases_are_offered() {
        let ids: Vec<&str> = CLAUDE.fallback_models.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&"fable"));
        assert!(ids.contains(&"opus"));
        assert!(ids.contains(&"sonnet"));
        assert_eq!(ids[0], "default");
    }

    #[test]
    fn extra_dirs_are_gated_on_the_installed_builds_help_output() {
        // An older build that does not know --add-dir would fail to launch.
        let without = CLAUDE.args_for(&InvocationContext {
            extra_dirs: vec!["/skills".into()],
            ..ctx()
        });
        assert!(!without.contains(&"--add-dir".to_string()));

        let with = CLAUDE.args_for(&InvocationContext {
            extra_dirs: vec!["/skills".into()],
            help_flags: vec!["--add-dir".into()],
            ..ctx()
        });
        assert!(with.windows(2).any(|w| w == ["--add-dir", "/skills"]));
    }

    #[test]
    fn mcp_config_is_passed_when_generated() {
        let args = CLAUDE.args_for(&InvocationContext {
            mcp_config: Some("/tmp/argo-mcp.json".into()),
            ..ctx()
        });
        assert!(args
            .windows(2)
            .any(|w| w == ["--mcp-config", "/tmp/argo-mcp.json"]));
    }

    #[test]
    fn capabilities_allow_resume_and_delegation() {
        const {
            assert!(CLAUDE.capabilities.native_resume);
        }
        const {
            assert!(CLAUDE.capabilities.can_delegate());
        }
        const {
            assert!(!CLAUDE.capabilities.always_reseeds());
        }
    }
}
