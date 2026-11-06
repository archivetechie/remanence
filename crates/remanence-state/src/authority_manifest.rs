//! Durable binding of the state lease, native logs and library authority.
//! The manifest is filesystem identity, never imported application authority.
//! Admission inspects it before touching locks, SQL or existing log bytes.
use crate::{StateError, StatePaths};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const NAME: &str = "authority-manifest.json";
const RESET: &str = "Legacy or unbound authority requires the declared state reset (`rem reset-catalog --config <config>`); no authority importer is available";

/// Validated, canonical authority roots and the library log's immutable UUID.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuthorityManifest {
    version: u32,
    state_root: PathBuf,
    tape_root: PathBuf,
    library_root: PathBuf,
    tape_format: u32,
    library_format: u32,
    library_uuid: [u8; 16],
}
fn invalid(message: impl Into<String>) -> StateError {
    StateError::ConfigInvalid(message.into())
}
/// Inspect every ancestor without following symlinks, including dangling links.
pub(crate) fn no_symlinks(path: &Path) -> Result<(), StateError> {
    for p in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "authority path contains a symlink: {}",
                    p.display()
                )))
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(StateError::io_at("inspect authority path", p, e)),
        }
    }
    Ok(())
}
fn canonical(path: &Path) -> Result<PathBuf, StateError> {
    no_symlinks(path)?;
    fs::canonicalize(path).map_err(|e| StateError::io_at("resolve authority root", path, e))
}
fn has_authority(path: &Path) -> Result<bool, StateError> {
    no_symlinks(path)?;
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(StateError::io_at("inspect authority namespace", path, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| StateError::io_at("inspect authority entry", path, e))?;
        let kind = entry
            .file_type()
            .map_err(|e| StateError::io_at("inspect authority type", entry.path(), e))?;
        if kind.is_symlink() {
            return Err(invalid("symlink in authority namespace"));
        }
        if kind.is_dir() {
            if has_authority(&entry.path())? {
                return Ok(true);
            }
        } else if entry.file_name() != "library-log.lock" {
            // Unknown files and ambiguity markers are authority, too. Never
            // initialize over bytes whose provenance cannot be established.
            return Ok(true);
        }
    }
    Ok(false)
}
/// Persist every new directory name, including configured external ancestors.
pub(crate) fn create_durable_directories(path: &Path) -> Result<(), StateError> {
    let mut missing = Vec::new();
    for directory in path.ancestors() {
        if directory
            .try_exists()
            .map_err(|e| StateError::io_at("inspect authority root", directory, e))?
        {
            break;
        }
        missing.push(directory);
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| StateError::io_at("create authority root", path, e))?;
    for directory in missing.into_iter().rev() {
        if let Some(parent) = directory.parent() {
            remanence_library::durable_file::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| StateError::io_at("sync authority parent", parent, e))?;
        }
    }
    Ok(())
}
impl AuthorityManifest {
    /// Read-only startup check. Missing manifests are accepted only when both
    /// authority namespaces are empty; SQL contents and versions are irrelevant.
    pub fn preflight(paths: &StatePaths) -> Result<Option<Self>, StateError> {
        crate::state::ensure_no_catalog_reset_fence(paths)?;
        Self::inspect_for_reset(paths)
    }

    /// Read the configured binding while the declared reset retains its fence.
    /// This never installs or clears anything.
    pub(crate) fn inspect_for_reset(paths: &StatePaths) -> Result<Option<Self>, StateError> {
        for p in [&paths.state_dir, &paths.audit_dir, &paths.journal_dir] {
            no_symlinks(p)?;
        }
        let path = paths.state_dir.join(NAME);
        no_symlinks(&path)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if has_authority(&paths.audit_dir)? || has_authority(&paths.journal_dir)? {
                    return Err(invalid(RESET));
                }
                return Ok(None);
            }
            Err(e) => return Err(StateError::io_at("read authority manifest", path, e)),
        };
        let value: Self = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("invalid authority manifest: {e}")))?;
        if value.version != 1
            || value.tape_format != 2
            || value.library_format != 1
            || value.library_uuid == [0; 16]
            || value.state_root != canonical(&paths.state_dir)?
            || value.tape_root != canonical(&paths.journal_dir.join("tapes"))?
            || value.library_root != canonical(&paths.audit_dir)?
        {
            return Err(invalid("authority manifest format or configured root binding differs; declared reset required"));
        }
        Ok(Some(value))
    }

    /// Install once under the encompassing state lease. Publication is synced
    /// before opening the library log; a retry retains the same UUID.
    pub fn admit(paths: &StatePaths, lock: &crate::StateLockGuard) -> Result<Self, StateError> {
        if canonical(
            lock.path()
                .parent()
                .ok_or_else(|| invalid("state lock has no root"))?,
        )? != canonical(&paths.state_dir)?
        {
            return Err(invalid("authority manifest has an unrelated state lease"));
        }
        if let Some(value) = Self::preflight(paths)? {
            return Ok(value);
        }
        Self::publish(paths, *uuid::Uuid::new_v4().as_bytes())
    }

    /// Install the receipt-pinned fresh binding only within a fenced reset.
    /// The caller retains the encompassing lease and has archived old inputs.
    pub(crate) fn install_reset(
        paths: &StatePaths,
        lock: &crate::StateLockGuard,
        uuid: [u8; 16],
    ) -> Result<Self, StateError> {
        if crate::state::catalog_reset_fence_evidence(paths)?.is_none() {
            return Err(invalid("reset manifest requires the durable reset fence"));
        }
        if canonical(
            lock.path()
                .parent()
                .ok_or_else(|| invalid("state lease has no parent"))?,
        )? != canonical(&paths.state_dir)?
        {
            return Err(invalid("reset manifest has an unrelated state lease"));
        }
        if paths.state_dir.join(NAME).exists() {
            let value =
                Self::inspect_for_reset(paths)?.ok_or_else(|| invalid("missing reset manifest"))?;
            if value.library_uuid != uuid {
                return Err(invalid("reset receipt and manifest identities differ"));
            }
            return Ok(value);
        }
        if has_authority(&paths.audit_dir)? || has_authority(&paths.journal_dir)? {
            return Err(invalid("reset manifest cannot cover uncleared authority"));
        }
        Self::publish(paths, uuid)
    }

    fn publish(paths: &StatePaths, uuid: [u8; 16]) -> Result<Self, StateError> {
        if uuid == [0; 16] {
            return Err(invalid("authority manifest UUID must be nonzero"));
        }
        for path in [&paths.audit_dir, &paths.journal_dir.join("tapes")] {
            create_durable_directories(path)?;
        }
        let value = Self {
            version: 1,
            state_root: canonical(&paths.state_dir)?,
            tape_root: canonical(&paths.journal_dir.join("tapes"))?,
            library_root: canonical(&paths.audit_dir)?,
            tape_format: 2,
            library_format: 1,
            library_uuid: uuid,
        };
        if value.tape_root.starts_with(&value.library_root)
            || value.library_root.starts_with(&value.tape_root)
            || value.state_root.starts_with(&value.library_root)
            || value.state_root.starts_with(&value.tape_root)
        {
            return Err(invalid("authority roots must be distinct"));
        }
        let body = serde_json::to_vec_pretty(&value).map_err(|e| invalid(e.to_string()))?;
        let mut temporary = crate::log_frame::publication::Temporary::new(
            &value.state_root,
            "authority",
            "manifest",
        )
        .map_err(|e| StateError::io_at("stage authority manifest", &value.state_root, e))?;
        temporary
            .file
            .write_all(&body)
            .map_err(|e| StateError::io_at("write authority manifest", &temporary.path, e))?;
        for directory in [&value.library_root, &value.tape_root, &paths.journal_dir] {
            remanence_library::durable_file::File::open(directory)
                .and_then(|f| f.sync_all())
                .map_err(|e| StateError::io_at("sync authority directory", directory, e))?;
        }
        crate::log_frame::publication::publish_new(
            &temporary.file,
            &temporary.path,
            &value.state_root.join(NAME),
            &value.state_root,
        )
        .map_err(|e| StateError::io_at("publish authority manifest", &value.state_root, e))?;
        Ok(value)
    }
    /// Canonical library-log namespace, including configurations outside state_root.
    pub fn library_root(&self) -> &Path {
        &self.library_root
    }
    /// Canonical native-log namespace.
    pub fn tape_root(&self) -> &Path {
        &self.tape_root
    }
    /// Immutable identity read from the manifest, never selected from SQL.
    pub fn library_uuid(&self) -> [u8; 16] {
        self.library_uuid
    }
}
