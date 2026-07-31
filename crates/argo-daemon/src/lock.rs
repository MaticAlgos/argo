//! Single-instance enforcement.
//!
//! Two daemons sharing one database would each own child processes and each
//! believe they own the write path. An advisory `flock` on a lock file keeps that
//! from happening, and because the lock is released by the kernel when the
//! process dies, a crashed daemon does not leave a stale lock behind — which is
//! the failure mode a plain pidfile has.

use argo_core::error::{ArgoError, Result};
use std::fs::File;
use std::path::{Path, PathBuf};

/// Holds the daemon's exclusive lock for its lifetime.
#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
    // Retained so the descriptor — and therefore the lock — lives as long as this
    // value. Dropping the file releases the lock.
    _file: File,
}

impl InstanceLock {
    /// Acquires the lock, or reports who holds it.
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = File::options()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| ArgoError::Io(format!("open lock {}: {e}", path.display())))?;

        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            // Non-blocking: report the conflict instead of hanging a second launch.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let existing = std::fs::read_to_string(&path).unwrap_or_default();
                let owner = existing.trim();
                return Err(ArgoError::Invalid(if owner.is_empty() {
                    "another Argo daemon is already running".to_string()
                } else {
                    format!("another Argo daemon is already running (pid {owner})")
                }));
            }
        }

        // Record the pid for diagnostics only; correctness rests on the flock.
        std::fs::write(&path, std::process::id().to_string())
            .map_err(|e| ArgoError::Io(format!("write lock {}: {e}", path.display())))?;

        Ok(Self { path, _file: file })
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // The kernel releases the flock when the descriptor closes; removing the
        // file is cosmetic, so a failure here is not worth surfacing.
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_holder_acquires_the_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lock = InstanceLock::acquire(dir.path().join("argo.lock")).expect("acquire");
        assert!(lock.path().exists());
        let recorded = std::fs::read_to_string(lock.path()).expect("read");
        assert_eq!(recorded.trim(), std::process::id().to_string());
    }

    #[cfg(unix)]
    #[test]
    fn a_second_holder_is_refused_with_the_owning_pid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("argo.lock");
        let _first = InstanceLock::acquire(&path).expect("first");
        let err = InstanceLock::acquire(&path).expect_err("second must fail");
        assert_eq!(err.code(), "INVALID_REQUEST");
        assert!(err.to_string().contains("already running"));
        assert!(err.to_string().contains(&std::process::id().to_string()));
    }

    #[test]
    fn releasing_allows_a_later_acquire() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("argo.lock");
        {
            let _lock = InstanceLock::acquire(&path).expect("first");
        }
        // A cleanly exited daemon leaves the lock available.
        let _again = InstanceLock::acquire(&path).expect("second");
    }

    #[test]
    fn a_stale_lock_file_without_a_live_holder_is_reusable() {
        // This is the pidfile failure mode the flock avoids: a leftover file from
        // a crashed daemon must not block startup forever.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("argo.lock");
        std::fs::write(&path, "999999").expect("write stale pid");
        let lock = InstanceLock::acquire(&path).expect("must reuse stale file");
        assert_eq!(
            std::fs::read_to_string(lock.path()).expect("read").trim(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn missing_parent_directories_are_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("deeper").join("argo.lock");
        let lock = InstanceLock::acquire(&path).expect("acquire");
        assert!(lock.path().exists());
    }
}
