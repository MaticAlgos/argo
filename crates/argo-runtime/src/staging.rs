//! Prompt-file staging.
//!
//! Adapters with [`PromptDelivery::File`] receive their composed prompt as a
//! path. That prompt contains whatever the conversation contains — source
//! excerpts, file contents, sometimes credentials the agent has already seen — so
//! the staged file is created inside Argo's own data directory with owner-only
//! permissions rather than in shared `/tmp`, and is removed when the turn ends.
//!
//! [`PromptDelivery::File`]: argo_core::runtime::PromptDelivery::File

use argo_core::error::{ArgoError, Result};
use std::path::{Path, PathBuf};

/// A staged prompt file that deletes itself when dropped.
///
/// Tying removal to the guard's lifetime means an early return or a panic mid-run
/// cannot leave conversation content behind on disk.
#[derive(Debug)]
pub struct StagedPrompt {
    path: PathBuf,
}

impl StagedPrompt {
    /// Writes `body` into `dir` with owner-only permissions.
    pub fn create(dir: impl AsRef<Path>, run_id: &str, body: &str) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("prompt-{run_id}.txt"));

        write_private(&path, body)?;

        Ok(Self { path })
    }

    /// Absolute path passed to the CLI.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path as a string, for argv construction.
    pub fn path_string(&self) -> String {
        self.path.to_string_lossy().to_string()
    }
}

impl Drop for StagedPrompt {
    fn drop(&mut self) {
        // Best effort: a failure to clean up must not mask the run's own outcome.
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "failed to remove staged prompt file"
                );
            }
        }
    }
}

/// Writes `body` to `path`, readable and writable only by the current user.
///
/// On Unix the mode is applied at creation time rather than afterwards, so the
/// content is never briefly world-readable.
fn write_private(path: &Path, body: &str) -> Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options
        .open(path)
        .map_err(|e| ArgoError::Io(format!("create {}: {e}", path.display())))?;
    file.write_all(body.as_bytes())
        .map_err(|e| ArgoError::Io(format!("write {}: {e}", path.display())))?;
    file.flush()
        .map_err(|e| ArgoError::Io(format!("flush {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_the_prompt_and_reports_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = StagedPrompt::create(dir.path(), "run-1", "hello agent").expect("stage");
        assert!(staged.path().exists());
        assert_eq!(
            std::fs::read_to_string(staged.path()).expect("read"),
            "hello agent"
        );
        assert!(staged.path_string().contains("run-1"));
    }

    #[cfg(unix)]
    #[test]
    fn staged_prompt_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let staged = StagedPrompt::create(dir.path(), "run-2", "secret context").expect("stage");
        let mode = std::fs::metadata(staged.path())
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "prompt content must not be group/world readable"
        );
    }

    #[test]
    fn staged_prompt_is_removed_when_the_turn_ends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = {
            let staged = StagedPrompt::create(dir.path(), "run-3", "transient").expect("stage");
            staged.path().to_path_buf()
        };
        assert!(!path.exists(), "prompt file must not outlive the run");
    }

    #[test]
    fn creates_the_staging_directory_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("staging").join("prompts");
        let staged = StagedPrompt::create(&nested, "run-4", "body").expect("stage");
        assert!(staged.path().exists());
    }

    #[test]
    fn rewriting_the_same_run_truncates_rather_than_appends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _first = StagedPrompt::create(dir.path(), "run-5", "old content").expect("first");
        let second = StagedPrompt::create(dir.path(), "run-5", "new").expect("second");
        assert_eq!(std::fs::read_to_string(second.path()).expect("read"), "new");
    }
}
