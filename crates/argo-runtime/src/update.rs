//! Version discovery and self-update support.
//!
//! Argo's public installer builds the current `main` branch, so the workspace
//! package version is the authoritative update signal. Releases must bump that
//! version; installed builds can then compare without downloading or executing
//! code. Installation is a separate, explicit operation.

use argo_core::error::{ArgoError, Result};
use semver::Version;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Published workspace manifest used for the lightweight update check.
pub const MANIFEST_URL: &str = "https://raw.githubusercontent.com/MaticAlgos/argo/main/Cargo.toml";
/// Published installer used only after the user explicitly requests an update.
pub const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/MaticAlgos/argo/main/install.sh";

/// Result of comparing this build with the version published on GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus {
    pub current: Version,
    pub latest: Version,
}

impl UpdateStatus {
    /// True only when GitHub advertises a strictly newer semantic version.
    pub fn available(&self) -> bool {
        self.latest > self.current
    }
}

/// Checks GitHub for a newer Argo version without running remote code.
pub async fn check() -> Result<UpdateStatus> {
    let manifest = fetch_text(MANIFEST_URL).await?;
    status_from_manifest(env!("CARGO_PKG_VERSION"), &manifest)
}

/// Downloads and runs Argo's public installer into the directory containing the
/// current executable. This is intentionally separate from [`check`]: merely
/// starting Argo never executes downloaded code.
pub async fn install_latest() -> Result<()> {
    let install_dir = update_install_dir()?;
    let temporary = std::env::temp_dir().join(format!(
        "argo-self-update-{}-{}",
        std::process::id(),
        argo_core::now_millis()
    ));
    tokio::fs::create_dir_all(&temporary)
        .await
        .map_err(|error| ArgoError::Io(format!("create update directory: {error}")))?;
    let script = temporary.join("install.sh");

    let download = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--retry",
            "3",
            "--output",
        ])
        .arg(&script)
        .arg(INSTALL_SCRIPT_URL)
        .status()
        .await;
    let download = match download {
        Ok(status) => status,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&temporary).await;
            return Err(ArgoError::Process(format!(
                "start curl for update: {error}"
            )));
        }
    };
    if !download.success() {
        let _ = tokio::fs::remove_dir_all(&temporary).await;
        return Err(ArgoError::Process(format!(
            "download installer failed with {download}"
        )));
    }

    let installed = Command::new("bash")
        .arg(&script)
        .env("ARGO_INSTALL_DIR", &install_dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await;
    let _ = tokio::fs::remove_dir_all(&temporary).await;
    let installed =
        installed.map_err(|error| ArgoError::Process(format!("start Argo installer: {error}")))?;
    if !installed.success() {
        return Err(ArgoError::Process(format!(
            "Argo installer failed with {installed}"
        )));
    }
    Ok(())
}

/// Removes the currently running installed Argo executable.
///
/// Conversation data and configuration live outside the executable and are
/// deliberately preserved. Development binaries are rejected so running a test
/// or a workspace build can never delete Cargo output unexpectedly.
pub fn uninstall_current_executable() -> Result<PathBuf> {
    let executable = std::env::current_exe()
        .map_err(|error| ArgoError::Io(format!("locate current Argo executable: {error}")))?;
    uninstall_executable_at(&executable)?;
    Ok(executable)
}

fn uninstall_executable_at(executable: &Path) -> Result<()> {
    if is_development_binary(executable) {
        return Err(ArgoError::Invalid(
            "self-uninstall is disabled for a target/debug or target/release build; run the installed Argo binary instead".into(),
        ));
    }
    std::fs::remove_file(executable).map_err(|error| {
        ArgoError::Io(format!(
            "remove installed executable {}: {error}",
            executable.display()
        ))
    })?;
    Ok(())
}

async fn fetch_text(url: &str) -> Result<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--max-time",
                "8",
            ])
            .arg(url)
            .output(),
    )
    .await
    .map_err(|_| ArgoError::Timeout(10_000))?
    .map_err(|error| ArgoError::Process(format!("start curl for update check: {error}")))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(ArgoError::Process(if detail.is_empty() {
            format!("update check failed with {}", output.status)
        } else {
            format!("update check failed: {detail}")
        }));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| ArgoError::Invalid(format!("update response was not UTF-8: {error}")))
}

fn status_from_manifest(current: &str, manifest: &str) -> Result<UpdateStatus> {
    let current = Version::parse(current)
        .map_err(|error| ArgoError::Invalid(format!("invalid current version: {error}")))?;
    let latest = workspace_version(manifest)?;
    Ok(UpdateStatus { current, latest })
}

fn workspace_version(manifest: &str) -> Result<Version> {
    let mut in_workspace_package = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_workspace_package = line == "[workspace.package]";
            continue;
        }
        if !in_workspace_package || !line.starts_with("version") {
            continue;
        }
        let value = line
            .split_once('=')
            .map(|(_, value)| value.trim().trim_matches(['\'', '"']))
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ArgoError::Invalid("published workspace version is malformed".into()))?;
        return Version::parse(value).map_err(|error| {
            ArgoError::Invalid(format!("published workspace version is invalid: {error}"))
        });
    }
    Err(ArgoError::Invalid(
        "published manifest has no [workspace.package] version".into(),
    ))
}

fn update_install_dir() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("ARGO_INSTALL_DIR") {
        let path = PathBuf::from(explicit);
        if path.as_os_str().is_empty() {
            return Err(ArgoError::Invalid("ARGO_INSTALL_DIR is empty".into()));
        }
        return Ok(path);
    }

    let executable = std::env::current_exe()
        .map_err(|error| ArgoError::Io(format!("locate current Argo executable: {error}")))?;
    if is_development_binary(&executable) {
        return Err(ArgoError::Invalid(
            "self-update is disabled for a target/debug or target/release build; install Argo first or set ARGO_INSTALL_DIR explicitly".into(),
        ));
    }
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| ArgoError::Io("current Argo executable has no parent directory".into()))
}

fn is_development_binary(path: &Path) -> bool {
    let components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair[0] == "target" && matches!(pair[1].as_ref(), "debug" | "release"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
[workspace]
members = []

[workspace.package]
version = "0.4.2"
edition = "2021"
"#;

    #[test]
    fn manifest_version_is_parsed_only_from_workspace_package() {
        assert_eq!(workspace_version(MANIFEST).unwrap(), Version::new(0, 4, 2));
        assert!(workspace_version("[package]\nversion = \"9.0.0\"").is_err());
    }

    #[test]
    fn only_a_strictly_newer_version_is_an_update() {
        assert!(status_from_manifest("0.4.1", MANIFEST).unwrap().available());
        assert!(!status_from_manifest("0.4.2", MANIFEST).unwrap().available());
        assert!(!status_from_manifest("0.5.0", MANIFEST).unwrap().available());
    }

    #[test]
    fn development_builds_are_not_overwritten_implicitly() {
        assert!(is_development_binary(Path::new("/repo/target/debug/argo")));
        assert!(is_development_binary(Path::new(
            "/repo/target/release/argo"
        )));
        assert!(!is_development_binary(Path::new(
            "/Users/me/.local/bin/argo"
        )));
    }

    #[test]
    fn uninstall_removes_only_the_selected_executable() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let executable = temporary.path().join("bin").join("argo");
        std::fs::create_dir_all(executable.parent().expect("bin parent")).expect("create bin");
        std::fs::write(&executable, b"test binary").expect("write executable");
        let state = temporary.path().join("state.sqlite");
        std::fs::write(&state, b"conversation").expect("write state");

        uninstall_executable_at(&executable).expect("uninstall executable");

        assert!(!executable.exists());
        assert!(state.exists(), "uninstall must preserve unrelated state");
    }
}
