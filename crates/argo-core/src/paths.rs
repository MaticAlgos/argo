//! Data-directory and socket-path resolution.
//!
//! One resolved root owns the database, logs, staged resources, and the daemon
//! socket. `ARGO_DATA_DIR` overrides it so tests never touch a real user's data.

use crate::error::{ArgoError, Result};
use std::path::{Path, PathBuf};

/// Environment variable that overrides the data root.
pub const DATA_DIR_ENV: &str = "ARGO_DATA_DIR";

/// Resolved filesystem layout for one Argo installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgoPaths {
    root: PathBuf,
}

impl ArgoPaths {
    /// Resolves the layout, honoring `ARGO_DATA_DIR` and falling back to the
    /// platform data directory.
    pub fn resolve() -> Result<Self> {
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV) {
            let raw = PathBuf::from(dir);
            if raw.as_os_str().is_empty() {
                return Err(ArgoError::Invalid(format!(
                    "{DATA_DIR_ENV} is set but empty"
                )));
            }
            return Ok(Self::with_root(raw));
        }
        let dirs = directories::ProjectDirs::from("dev", "argo", "argo")
            .ok_or_else(|| ArgoError::Io("cannot determine platform data directory".into()))?;
        Ok(Self::with_root(dirs.data_dir().to_path_buf()))
    }

    /// Builds a layout rooted at an explicit directory.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The data root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Canonical SQLite database file.
    pub fn database(&self) -> PathBuf {
        self.root.join("argo.sqlite")
    }

    /// Daemon log file.
    pub fn log_file(&self) -> PathBuf {
        self.root.join("logs").join("argo-daemon.log")
    }

    /// Directory holding staged skill copies and other run inputs.
    pub fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// Argo-managed user skills root.
    pub fn user_skills(&self) -> PathBuf {
        self.root.join("skills")
    }

    /// Installed plugin bundles.
    pub fn plugins(&self) -> PathBuf {
        self.root.join("plugins")
    }

    /// Daemon socket path.
    ///
    /// Unix domain sockets have a short path limit (roughly 104 bytes on macOS),
    /// so a deep data root would make `bind` fail with a confusing error. The
    /// socket therefore lives under the runtime dir when the resolved path would
    /// be too long.
    pub fn socket(&self) -> PathBuf {
        let preferred = self.root.join("argo.sock");
        if preferred.as_os_str().len() <= 100 {
            return preferred;
        }
        let hash = crate::sha256_hex(&self.root.to_string_lossy());
        std::env::temp_dir().join(format!("argo-{}.sock", &hash[..16]))
    }

    /// Daemon lock file used to enforce a single instance.
    pub fn lock_file(&self) -> PathBuf {
        self.root.join("argo.lock")
    }

    /// Creates the directories Argo writes to.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(self.root.join("logs"))?;
        std::fs::create_dir_all(self.staging())?;
        std::fs::create_dir_all(self.user_skills())?;
        std::fs::create_dir_all(self.plugins())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_root_places_all_artifacts_under_it() {
        let p = ArgoPaths::with_root("/tmp/argo-test");
        assert_eq!(p.database(), PathBuf::from("/tmp/argo-test/argo.sqlite"));
        assert_eq!(p.socket(), PathBuf::from("/tmp/argo-test/argo.sock"));
        assert!(p.log_file().starts_with("/tmp/argo-test"));
        assert!(p.staging().starts_with("/tmp/argo-test"));
    }

    #[test]
    fn overly_long_roots_fall_back_to_a_short_socket_path() {
        // Guards a real failure mode: bind() rejects long sun_path, and the
        // resulting errno is not self-explanatory.
        let deep = format!("/tmp/{}", "d".repeat(200));
        let p = ArgoPaths::with_root(&deep);
        let socket = p.socket();
        assert!(!socket.starts_with(&deep));
        assert!(socket.to_string_lossy().len() < 120);
        assert!(socket.to_string_lossy().contains("argo-"));
    }

    #[test]
    fn socket_fallback_is_deterministic_for_a_given_root() {
        let deep = format!("/tmp/{}", "x".repeat(200));
        assert_eq!(
            ArgoPaths::with_root(&deep).socket(),
            ArgoPaths::with_root(&deep).socket()
        );
    }

    #[test]
    fn ensure_dirs_creates_the_layout() {
        let tmp = std::env::temp_dir().join(format!("argo-paths-{}", uuid::Uuid::new_v4()));
        let p = ArgoPaths::with_root(&tmp);
        p.ensure_dirs().expect("create dirs");
        assert!(tmp.join("logs").is_dir());
        assert!(p.staging().is_dir());
        assert!(p.user_skills().is_dir());
        assert!(p.plugins().is_dir());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
