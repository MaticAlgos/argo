//! Adapter detection.
//!
//! Every registered adapter is probed concurrently and independently. Isolation
//! matters: one CLI that hangs or crashes on `--version` must not empty the whole
//! picker, which is exactly what a sequential probe loop would do.
//!
//! Probing order per adapter:
//! 1. Resolve the executable, trying fallback names.
//! 2. Run the version probe. A binary that launches but rejects `--version` is
//!    still available, just without a version string.
//! 3. Only then run help, model, and auth probes in parallel.

use crate::def::{AgentInfo, RuntimeDef};
use crate::registry::ADAPTERS;
use argo_core::runtime::ModelOption;
use std::time::Duration;
use tokio::process::Command;

/// Default timeout for the version probe.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for the help probe.
const HELP_TIMEOUT: Duration = Duration::from_secs(10);

/// Output of one probe.
struct ProbeOutput {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs `bin args...`, returning `None` if it could not be executed at all.
async fn run(bin: &str, args: &[&str], timeout: Duration) -> Option<ProbeOutput> {
    let mut command = Command::new(bin);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Detach from the terminal: a probe must never inherit the TUI's stdin or
    // print into the rendered frame.
    command.kill_on_drop(true);

    let child = command.output();
    match tokio::time::timeout(timeout, child).await {
        Ok(Ok(output)) => Some(ProbeOutput {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }),
        // Launched but timed out: the binary exists, so report it as executable
        // with no version rather than hiding the agent entirely.
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Resolves which candidate binary is executable, preferring the primary name.
async fn resolve_bin(def: &RuntimeDef) -> Option<(String, Option<ProbeOutput>)> {
    for candidate in def.candidate_bins() {
        if let Some(output) = run(candidate, def.version_args, VERSION_TIMEOUT).await {
            return Some((candidate.to_string(), Some(output)));
        }
    }
    None
}

/// Extracts a version string from probe output.
///
/// CLIs disagree about which stream carries it and how much decoration they add,
/// so the first non-empty line is taken from stdout, then stderr.
fn extract_version(output: &ProbeOutput) -> Option<String> {
    for stream in [&output.stdout, &output.stderr] {
        if let Some(line) = stream.lines().map(str::trim).find(|l| !l.is_empty()) {
            return Some(line.to_string());
        }
    }
    None
}

/// Collects the long flags an installed build advertises.
///
/// Arg builders consult these so Argo never passes an option an older binary
/// would reject.
fn extract_flags(help: &str) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    for token in help.split(|c: char| c.is_whitespace() || matches!(c, ',' | '=' | '[' | ']' | '<'))
    {
        if token.starts_with("--") && token.len() > 2 {
            let flag = token
                .trim_end_matches(['.', ')', ':', '"', '\''])
                .to_string();
            if flag.len() > 2 && !flags.contains(&flag) {
                flags.push(flag);
            }
        }
    }
    flags
}

/// Detects one adapter.
pub async fn detect_one(def: &'static RuntimeDef) -> AgentInfo {
    let Some((bin, version_output)) = resolve_bin(def).await else {
        return AgentInfo::unavailable(
            def,
            format!(
                "{} not found on PATH. Install it from {}",
                def.bin, def.install_url
            ),
        );
    };

    let mut diagnostics: Vec<String> = Vec::new();
    let version = version_output.as_ref().and_then(extract_version);
    if version.is_none() {
        // Not fatal: the binary launched, which is what availability means.
        diagnostics.push(format!(
            "{bin} did not report a version; capability detection may be limited"
        ));
    }

    // Availability is established; remaining probes run concurrently.
    let help_future = run(&bin, def.help_args, HELP_TIMEOUT);
    let models_future = async {
        match &def.model_probe {
            Some(probe) => run(&bin, probe.args, Duration::from_millis(probe.timeout_ms))
                .await
                .filter(|o| o.ok)
                .map(|o| {
                    let models = (probe.parse)(&o.stdout);
                    // The same output carries per-model reasoning levels, so parse it
                    // once rather than probing twice.
                    let reasoning = probe
                        .parse_reasoning
                        .map(|parse| parse(&o.stdout))
                        .unwrap_or_default();
                    (models, reasoning)
                }),
            None => None,
        }
    };
    let auth_future = async {
        match &def.auth_probe {
            Some(probe) => run(&bin, probe.args, Duration::from_millis(probe.timeout_ms))
                .await
                .map(|o| o.ok),
            // Adapters without a probe leave auth unknown rather than guessing
            // from the presence of a config directory.
            None => None,
        }
    };

    let (help, models, authenticated) = futures::join!(help_future, models_future, auth_future);

    let help_flags = help
        .as_ref()
        .map(|o| extract_flags(&format!("{}\n{}", o.stdout, o.stderr)))
        .unwrap_or_default();

    let (models, model_reasoning, models_live) = match models {
        Some((list, reasoning)) if list.len() > 1 => (list, reasoning, true),
        _ => (def.fallback_model_options(), Vec::new(), false),
    };

    if authenticated == Some(false) {
        diagnostics.push(format!(
            "{bin} appears not to be logged in; run its login command before starting a run"
        ));
    }

    diagnostics.extend(crate::def::capability_diagnostics(def));

    AgentInfo {
        id: def.id.to_string(),
        name: def.name.to_string(),
        available: true,
        path: Some(bin),
        version,
        authenticated,
        models,
        models_live,
        reasoning: def.reasoning_option_values(),
        model_reasoning,
        capabilities: def.capabilities.clone(),
        diagnostics,
        install_url: def.install_url.to_string(),
    }
    .with_help_flags(help_flags)
}

impl AgentInfo {
    /// Attaches observed help flags, kept out of the serialized surface.
    fn with_help_flags(self, flags: Vec<String>) -> Self {
        HELP_FLAGS.with(|cell| {
            cell.borrow_mut().retain(|(id, _)| id != &self.id);
            cell.borrow_mut().push((self.id.clone(), flags));
        });
        self
    }
}

thread_local! {
    /// Per-thread cache of the flags each installed binary advertises.
    ///
    /// Detection and invocation happen on the daemon's runtime, so this stays
    /// process-local rather than being persisted; a stale cache would otherwise
    /// outlive a CLI upgrade.
    static HELP_FLAGS: std::cell::RefCell<Vec<(String, Vec<String>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Returns the flags last observed for `agent_id`.
pub fn observed_flags(agent_id: &str) -> Vec<String> {
    HELP_FLAGS.with(|cell| {
        cell.borrow()
            .iter()
            .find(|(id, _)| id == agent_id)
            .map(|(_, flags)| flags.clone())
            .unwrap_or_default()
    })
}

/// Detects every registered adapter concurrently.
pub async fn detect_all() -> Vec<AgentInfo> {
    let futures: Vec<_> = ADAPTERS.iter().map(detect_one).collect();
    futures::future::join_all(futures).await
}

/// Models to offer for `agent_id`, preferring live discovery.
pub fn models_for(info: &AgentInfo) -> &[ModelOption] {
    &info.models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_taken_from_stdout_then_stderr() {
        let stdout_only = ProbeOutput {
            ok: true,
            stdout: "\n  2.1.178 \n".into(),
            stderr: String::new(),
        };
        assert_eq!(extract_version(&stdout_only).as_deref(), Some("2.1.178"));

        let stderr_only = ProbeOutput {
            ok: true,
            stdout: "   ".into(),
            stderr: "grok 0.1.212".into(),
        };
        assert_eq!(
            extract_version(&stderr_only).as_deref(),
            Some("grok 0.1.212")
        );

        let silent = ProbeOutput {
            ok: true,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(extract_version(&silent), None);
    }

    #[test]
    fn help_flag_extraction_finds_long_options() {
        let help = "Usage: claude [options]\n  \
                    --add-dir <path>  Add a directory\n  \
                    --model=<name>    Choose a model\n  \
                    --verbose,        Verbose output.\n  \
                    -p                Print mode\n";
        let flags = extract_flags(help);
        assert!(flags.contains(&"--add-dir".to_string()));
        assert!(flags.contains(&"--model".to_string()));
        assert!(flags.contains(&"--verbose".to_string()));
        // Short flags are not tracked; arg builders only gate on long options.
        assert!(!flags.iter().any(|f| f == "-p"));
    }

    #[test]
    fn help_flag_extraction_deduplicates() {
        let flags = extract_flags("--json --json --verbose");
        assert_eq!(flags.iter().filter(|f| *f == "--json").count(), 1);
    }

    #[tokio::test]
    async fn a_missing_binary_is_reported_as_unavailable_with_install_guidance() {
        // Detection must degrade to an actionable card, never an error that hides
        // the other agents.
        static MISSING: RuntimeDef = RuntimeDef {
            id: "definitely-not-installed",
            name: "Ghost CLI",
            bin: "argo-nonexistent-binary-xyz",
            fallback_bins: &[],
            version_args: &["--version"],
            help_args: &["--help"],
            model_probe: None,
            fallback_models: &[("default", "default")],
            reasoning_options: &[],
            auth_probe: None,
            build_args: |_| vec![],
            capture_session: None,
            capabilities: argo_core::runtime::AgentCapabilities {
                stream_format: argo_core::runtime::StreamFormat::Plain,
                prompt_delivery: argo_core::runtime::PromptDelivery::Stdin,
                prompt_encoding: argo_core::runtime::PromptEncoding::Raw,
                native_resume: false,
                captures_session: false,
                mcp_injection: argo_core::runtime::McpInjection::None,
                supports_images: false,
                permission: argo_core::runtime::PermissionPosture::FullBypass,
                modes: argo_core::mode::ModeSupport::NONE,
            },
            install_url: "https://example.invalid/install",
        };

        let info = detect_one(&MISSING).await;
        assert!(!info.available);
        assert!(info.path.is_none());
        assert!(info.diagnostics[0].contains("not found on PATH"));
        assert!(info.diagnostics[0].contains("https://example.invalid/install"));
        // Even unavailable adapters offer fallback models so the picker renders.
        assert!(!info.models.is_empty());
    }

    #[tokio::test]
    async fn detection_probes_every_adapter_without_one_failure_hiding_others() {
        let all = detect_all().await;
        assert_eq!(all.len(), ADAPTERS.len());
        let ids: Vec<&str> = all.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude",
                "codex",
                "opencode",
                "kiro",
                "cmd",
                "antigravity",
                "grok"
            ]
        );
        // Whatever is installed on this machine, every adapter produced a result.
        for info in &all {
            assert!(!info.models.is_empty(), "{} has no models", info.id);
            assert!(!info.install_url.is_empty());
        }
    }

    #[tokio::test]
    async fn a_real_binary_is_detected_with_a_version() {
        // `sh` exists everywhere Argo runs, so this exercises the success path
        // without depending on a coding CLI being installed.
        static SH: RuntimeDef = RuntimeDef {
            id: "test-sh",
            name: "Shell",
            bin: "sh",
            fallback_bins: &[],
            version_args: &["-c", "echo 1.2.3"],
            help_args: &["-c", "echo --demo-flag"],
            model_probe: None,
            fallback_models: &[("default", "default")],
            reasoning_options: &[],
            auth_probe: None,
            build_args: |_| vec![],
            capture_session: None,
            capabilities: argo_core::runtime::AgentCapabilities {
                stream_format: argo_core::runtime::StreamFormat::ClaudeStreamJson,
                prompt_delivery: argo_core::runtime::PromptDelivery::Stdin,
                prompt_encoding: argo_core::runtime::PromptEncoding::Raw,
                native_resume: true,
                captures_session: true,
                mcp_injection: argo_core::runtime::McpInjection::ClaudeMcpJson,
                supports_images: false,
                permission: argo_core::runtime::PermissionPosture::FullBypass,
                modes: argo_core::mode::ModeSupport::NONE,
            },
            install_url: "https://example.invalid",
        };

        let info = detect_one(&SH).await;
        assert!(info.available);
        assert_eq!(info.path.as_deref(), Some("sh"));
        assert_eq!(info.version.as_deref(), Some("1.2.3"));
        assert!(observed_flags("test-sh").contains(&"--demo-flag".to_string()));
    }

    #[tokio::test]
    async fn capability_limitations_are_surfaced_as_diagnostics() {
        let all = detect_all().await;
        let grok = all.iter().find(|i| i.id == "grok").expect("grok");
        let joined = grok.diagnostics.join(" ");
        // The user should learn why Grok behaves differently before they hit it.
        assert!(joined.contains("reseeded"));
        assert!(joined.contains("plain text"));
        assert!(joined.contains("separate reasoning channel"));
        assert!(joined.contains("no stable native-subagent lifecycle"));
        assert!(joined.contains("command fallback"));
    }
}
