//! Exclusive state-directory locking.

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::error::StateError;

const STATE_LOCK_FILE: &str = "state.lock";

/// Guard that owns the exclusive Layer 4 state lock.
#[derive(Debug)]
pub struct StateLockGuard {
    _file: Flock<File>,
    path: PathBuf,
}

impl StateLockGuard {
    /// Acquire the nonblocking exclusive state lock.
    pub fn acquire(state_dir: impl AsRef<Path>) -> Result<Self, StateError> {
        let state_dir = state_dir.as_ref();
        // Reset/startup must refuse aliases before chmod, mkdir or diagnostic
        // writes. Inspect ancestors too, including dangling symlinks.
        reject_symlink_ancestors(state_dir)?;
        let path = state_dir.join(STATE_LOCK_FILE);
        reject_symlink_ancestors(&path)?;
        create_private_dir(state_dir)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        options.custom_flags(nix::libc::O_NOFOLLOW);
        let file = options
            .open(&path)
            .map_err(|err| StateError::io_at("open state lock", &path, err))?;
        let mut file =
            Flock::lock(file, FlockArg::LockExclusiveNonblock).map_err(|(_, errno)| {
                let err = std::io::Error::from(errno);
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    StateError::StateLockHeld(path.clone())
                } else if err.kind() == std::io::ErrorKind::PermissionDenied {
                    StateError::PermissionDenied(path.display().to_string())
                } else {
                    StateError::io_at("lock state file", &path, err)
                }
            })?;

        write_diagnostics(&mut file, &path)?;
        sync_directory(state_dir)?;

        Ok(Self { _file: file, path })
    }

    /// Refuse motion recovery while a declared reset is unresolved. The held
    /// state lock excludes a reset beginning between this check and dispatch;
    /// no catalog, authority log or reset receipt is opened or changed.
    pub fn ensure_no_catalog_reset(&self) -> Result<(), StateError> {
        crate::state::ensure_no_catalog_reset_at(
            self.path
                .parent()
                .expect("state lock has a parent directory"),
        )
    }

    /// Path of the lock file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Return the current diagnostic contents.
    pub fn diagnostics(&self) -> Result<String, StateError> {
        fs::read_to_string(&self.path)
            .map_err(|err| StateError::io_at("read state lock diagnostics", &self.path, err))
    }
}

/// Refuse every existing symlink component without following it or mutating
/// its target. Missing components are allowed for first startup under a real
/// parent; failures to inspect an existing path are never treated as absence.
fn reject_symlink_ancestors(path: &Path) -> Result<(), StateError> {
    for component in path.ancestors().filter(|path| !path.as_os_str().is_empty()) {
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(StateError::ConfigInvalid(format!(
                    "state lock path contains a symlink: {}",
                    component.display(),
                )));
            }
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => {
                return Err(StateError::io_at(
                    "inspect state lock path",
                    component,
                    error,
                ))
            }
        }
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), StateError> {
    #[cfg(unix)]
    {
        crate::authority_manifest::create_durable_directories(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|err| StateError::io_at("chmod state directory", path, err))?;
    }

    #[cfg(not(unix))]
    {
        crate::authority_manifest::create_durable_directories(path)?;
    }

    Ok(())
}

fn write_diagnostics(file: &mut Flock<File>, path: &Path) -> Result<(), StateError> {
    let started_at_utc = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| StateError::ConfigInvalid(format!("format lock timestamp: {err}")))?;
    let body = format!(
        "pid={}\nhost_id={}\nstarted_at_utc={started_at_utc}\nbinary_version={}\n",
        std::process::id(),
        host_id(),
        env!("CARGO_PKG_VERSION")
    );

    file.set_len(0)
        .map_err(|err| StateError::io_at("truncate state lock", path, err))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|err| StateError::io_at("seek state lock", path, err))?;
    file.write_all(body.as_bytes())
        .map_err(|err| StateError::io_at("write state lock diagnostics", path, err))?;
    file.sync_all()
        .map_err(|err| StateError::io_at("fsync state lock", path, err))?;
    Ok(())
}

fn host_id() -> String {
    fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn sync_directory(path: &Path) -> Result<(), StateError> {
    let dir =
        File::open(path).map_err(|err| StateError::io_at("open state directory", path, err))?;
    dir.sync_all()
        .map_err(|err| StateError::io_at("fsync state directory", path, err))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// The reset lease may not change a symlink target, even before its reset
    /// path validation starts. Existing and dangling aliases both refuse.
    #[cfg(unix)]
    #[test]
    fn state_lock_refuses_symlinks_before_any_target_write() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for target_exists in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("target");
            if target_exists {
                fs::create_dir(&target).unwrap();
                fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
                fs::write(target.join(STATE_LOCK_FILE), b"unchanged").unwrap();
            }
            let alias = root.path().join("alias");
            symlink(&target, &alias).unwrap();
            for path in [alias.clone(), alias.join("new-child")] {
                assert!(StateLockGuard::acquire(&path).is_err());
            }
            assert!(!target.join("new-child").exists());
            if target_exists {
                assert_eq!(
                    fs::read(target.join(STATE_LOCK_FILE)).unwrap(),
                    b"unchanged"
                );
                assert_eq!(
                    fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            } else {
                assert!(!target.exists());
            }
        }
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("other-file");
        fs::write(&target, b"unchanged").unwrap();
        symlink(&target, root.path().join(STATE_LOCK_FILE)).unwrap();
        assert!(StateLockGuard::acquire(root.path()).is_err());
        assert_eq!(fs::read(target).unwrap(), b"unchanged");
    }

    #[test]
    fn second_state_lock_is_rejected() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-lock")
            .tempdir()
            .expect("temp dir");
        let first = StateLockGuard::acquire(temp.path()).expect("first lock");
        let err = StateLockGuard::acquire(temp.path()).expect_err("second lock must fail");

        assert!(err.is_state_lock_held(), "{err}");
        drop(first);
    }

    #[test]
    fn stale_looking_file_contents_do_not_block_free_kernel_lock() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-lock")
            .tempdir()
            .expect("temp dir");
        let lock_path = temp.path().join(STATE_LOCK_FILE);
        fs::write(&lock_path, "pid=1\nhost_id=old\n").expect("write stale lock file");

        let guard = StateLockGuard::acquire(temp.path()).expect("lock should be acquirable");
        let diagnostics = guard.diagnostics().expect("diagnostics");

        assert!(diagnostics.contains("pid="));
        assert!(diagnostics.contains("started_at_utc="));
        assert!(!diagnostics.contains("host_id=old"));
    }
}
