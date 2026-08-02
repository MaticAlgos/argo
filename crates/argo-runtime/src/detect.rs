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
//!
//! **Lightweight discovery** (`discover_one`/`discover_all`) resolves executables
//! purely from PATH without spawning any child processes. This is used for daemon
//! startup and agent list refresh, deferring the expensive deep probe until a
//! specific adapter is actually needed for a turn.

use crate::def::{AgentInfo, RuntimeDef};
use crate::registry::ADAPTERS;
use argo_core::runtime::ModelOption;
use std::time::Duration;
use tokio::process::Command;

/// Default timeout for the version probe.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Default timeout for the help probe.
const HELP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Lightweight filesystem-only discovery (no child processes)
// ---------------------------------------------------------------------------

/// Resolves an executable name to an absolute path by searching PATH.
///
/// On Unix, additionally verifies executable bits. Never spawns a child process.
fn resolve_path(bin: &str) -> Option<String> {
    if std::path::Path::new(bin).is_absolute() {
        return executable_path(std::path::Path::new(bin));
    }
    let path = std::env::var_os("PATH")?;
    resolve_path_in(bin, std::env::split_paths(&path))
}

/// Resolves `bin` against explicit directories, which also makes no-spawn tests
/// hermetic without mutating process-global PATH.
fn resolve_path_in(
    bin: &str,
    directories: impl IntoIterator<Item = std::path::PathBuf>,
) -> Option<String> {
    directories
        .into_iter()
        .filter_map(|directory| executable_path(&directory.join(bin)))
        .next()
}

fn executable_path(path: &std::path::Path) -> Option<String> {
    if !path.is_file() || !is_executable(path) {
        return None;
    }
    std::fs::canonicalize(path)
        .ok()
        .map(|resolved| resolved.to_string_lossy().into_owned())
}

/// Checks executable permission bits on Unix. On non-Unix, returns true if file exists.
#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Lightweight discovery for one adapter — filesystem only, no subprocess.
///
/// Returns an `AgentInfo` with `available = true` and `probed = false` when the
/// executable is found on PATH. No version, models, or auth state is resolved.
pub fn discover_one(def: &'static RuntimeDef) -> AgentInfo {
    for candidate in def.candidate_bins() {
        if let Some(abs) = resolve_path(candidate) {
            return AgentInfo {
                id: def.id.to_string(),
                name: def.name.to_string(),
                available: true,
                probed: false,
                path: Some(abs),
                version: None,
                authenticated: None,
                models: def.fallback_model_options(),
                models_live: false,
                reasoning: def.reasoning_option_values(),
                model_reasoning: Vec::new(),
                capabilities: def.capabilities.clone(),
                diagnostics: {
                    let mut diagnostics = vec![
                        "installed; version, authentication, and live models load only when selected"
                            .to_string(),
                    ];
                    diagnostics.extend(crate::def::capability_diagnostics(def));
                    diagnostics
                },
                install_url: def.install_url.to_string(),
                help_flags: Vec::new(),
            };
        }
    }
    AgentInfo::unavailable(
        def,
        format!(
            "{} not found on PATH. Install it from {}",
            def.bin, def.install_url
        ),
    )
}

/// Lightweight discovery for all registered adapters — filesystem only.
pub fn discover_all_lightweight() -> Vec<AgentInfo> {
    ADAPTERS.iter().map(discover_one).collect()
}

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

/// Detects one adapter (deep probe: spawns subprocess for version, help, models, auth).
///
/// First resolves the path without executing, then probes. A version timeout or
/// failure does not make a filesystem-present executable unavailable.
pub async fn detect_one(def: &'static RuntimeDef) -> AgentInfo {
    // Phase 1: resolve without executing. If found on PATH, the adapter is
    // considered available even if version/help probes fail or timeout.
    let resolved_path = {
        let mut found = None;
        for candidate in def.candidate_bins() {
            if let Some(abs) = resolve_path(candidate) {
                found = Some(abs);
                break;
            }
        }
        found
    };

    let Some(resolved) = resolved_path else {
        let mut unavailable = AgentInfo::unavailable(
            def,
            format!(
                "{} not found on PATH. Install it from {}",
                def.bin, def.install_url
            ),
        );
        unavailable.probed = true;
        return unavailable;
    };

    // Phase 2: deep probe (version, help, models, auth). Failures here do NOT
    // make the adapter unavailable — the binary exists on disk.
    let bin = &resolved;
    let mut diagnostics: Vec<String> = Vec::new();

    let version = match run(bin, def.version_args, VERSION_TIMEOUT).await {
        Some(output) => {
            let v = extract_version(&output);
            if v.is_none() {
                diagnostics.push(format!(
                    "{bin} did not report a version; capability detection may be limited"
                ));
            }
            v
        }
        None => {
            diagnostics.push(format!(
                "{bin} version probe timed out; capability detection may be limited"
            ));
            None
        }
    };

    // Availability is established; remaining probes run concurrently.
    let help_future = run(bin, def.help_args, HELP_TIMEOUT);
    let models_future = async {
        match &def.model_probe {
            Some(probe) => run(bin, probe.args, Duration::from_millis(probe.timeout_ms))
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
            Some(probe) => run(bin, probe.args, Duration::from_millis(probe.timeout_ms))
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
        probed: true,
        path: Some(resolved),
        version,
        authenticated,
        models,
        models_live,
        reasoning: def.reasoning_option_values(),
        model_reasoning,
        capabilities: def.capabilities.clone(),
        diagnostics,
        install_url: def.install_url.to_string(),
        help_flags,
    }
}

/// Detects every registered adapter concurrently (deep probe).
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

    #[test]
    fn lightweight_discovery_lists_every_adapter_without_launching_them() {
        let all = discover_all_lightweight();
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
        // Canonicalization may resolve `/bin/sh` to the implementation binary
        // (for example `/usr/bin/dash` on Ubuntu), but the result must be an
        // absolute executable path on every platform.
        assert!(
            info.path
                .as_deref()
                .is_some_and(|path| std::path::Path::new(path).is_absolute()),
            "path should be absolute: {:?}",
            info.path
        );
        assert_eq!(info.version.as_deref(), Some("1.2.3"));
        assert!(info.help_flags.contains(&"--demo-flag".to_string()));
        assert!(info.probed);
    }

    #[test]
    fn capability_limitations_are_surfaced_as_diagnostics() {
        let grok = discover_one(crate::registry::find("grok").expect("grok"));
        let joined = grok.diagnostics.join(" ");
        // The user should learn why Grok behaves differently before they hit it.
        assert!(joined.contains("reseeded"));
        assert!(joined.contains("plain text"));
        assert!(joined.contains("separate reasoning channel"));
        assert!(joined.contains("no stable native-subagent lifecycle"));
        assert!(joined.contains("command fallback"));
    }

    #[test]
    fn lightweight_path_resolution_never_executes_the_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir");
        let executable = directory.path().join("fake-agent");
        let marker = directory.path().join("executed");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
        )
        .expect("script");
        let mut permissions = std::fs::metadata(&executable)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).expect("chmod");

        let resolved = resolve_path_in("fake-agent", [directory.path().to_path_buf()]);
        let expected = std::fs::canonicalize(&executable).expect("canonical executable");
        assert_eq!(resolved.as_deref(), expected.to_str());
        assert!(
            !marker.exists(),
            "discovery must never launch the candidate"
        );
    }

    #[test]
    fn lightweight_discovery_missing_binary_is_unavailable() {
        static MISSING_DISCOVER: RuntimeDef = RuntimeDef {
            id: "test-missing-discover",
            name: "Ghost",
            bin: "argo-nonexistent-binary-test-xyz",
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
            install_url: "https://example.invalid",
        };

        let info = discover_one(&MISSING_DISCOVER);
        assert!(!info.available);
        assert!(!info.probed);
    }

    #[test]
    fn discover_all_lightweight_never_deep_probes() {
        let all = discover_all_lightweight();
        assert_eq!(all.len(), ADAPTERS.len());
        for info in &all {
            assert!(
                !info.probed,
                "{} was deep-probed during lightweight discovery",
                info.id
            );
        }
    }
}
