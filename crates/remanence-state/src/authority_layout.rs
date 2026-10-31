//! Admission fence for the single per-tape authority layout. Only an empty
//! state or the declared, fenced reset may install the marker, and only under
//! the exclusive state lock. Read-only validation takes no lock and installs
//! nothing. Projection rebuilds validate this marker and never initialize or
//! reset authority.
use crate::{StateError, StatePaths};
use remanence_library::durable_file::{self, File};
use std::{fs, io::Write, path::Path};

/// Active host authority layout identifier.
pub const FORMAT: &str = "remanence-tape-log-v1";
/// Exact operator guidance for the declared clean break.
pub const RESET_REQUIRED: &str = "This state directory uses the two-journal authority layout.\nStep 2 requires a declared state reset and reinitialization of its test tapes.\nRun rem reset-catalog --config <config> with the existing explicit reset\nconfirmation, then reinitialize the approved scratch tapes. No importer is available.";

fn invalid(message: impl Into<String>) -> StateError {
    StateError::ConfigInvalid(message.into())
}
fn old_authority(path: &Path, archive: &Path) -> Result<bool, StateError> {
    if !path.exists() {
        return Ok(false);
    }
    for entry in
        fs::read_dir(path).map_err(|e| StateError::io_at("inspect authority layout", path, e))?
    {
        let entry = entry.map_err(|e| StateError::io_at("inspect authority entry", path, e))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.path() == archive {
            continue;
        }
        let ty = entry
            .file_type()
            .map_err(|e| StateError::io_at("inspect authority type", entry.path(), e))?;
        if name.contains(".remjournal")
            || name.contains(".remcheckpoint")
            || name.contains(".finalizing")
            || (path.file_name().is_some_and(|n| n == "checkpoints")
                && (name.ends_with(".init") || name.ends_with(".lock")))
        {
            return Ok(true);
        }
        if ty.is_symlink() {
            return Err(invalid(format!(
                "authority layout contains a symlink: {}",
                entry.path().display()
            )));
        }
        if ty.is_dir() && old_authority(&entry.path(), archive)? {
            return Ok(true);
        }
    }
    Ok(false)
}
fn contents(paths: &StatePaths) -> Result<Vec<u8>, StateError> {
    let root = paths
        .journal_dir
        .canonicalize()
        .map_err(|e| StateError::io_at("resolve journal root", &paths.journal_dir, e))?;
    let root = root
        .to_str()
        .ok_or_else(|| invalid("journal root is not UTF-8"))?;
    Ok(format!(
        "{FORMAT}\n{}\n",
        serde_json::to_string(root).map_err(|e| invalid(e.to_string()))?
    )
    .into_bytes())
}
/// Validate admission under the caller's exclusive state lock. A genuinely
/// empty layout is provisioned atomically; any existing authority needs reset.
pub fn admit(paths: &StatePaths) -> Result<(), StateError> {
    if validate_layout(paths)? == Layout::Marked {
        return Ok(());
    }
    install(paths, false)
}

#[derive(Debug, PartialEq, Eq)]
enum Layout {
    /// The marker matches and no old authority remains.
    Marked,
    /// No marker and no state at all: only an owner may install one.
    Empty,
}

/// Read-only layout check: creates, locks and installs nothing.
fn validate_layout(paths: &StatePaths) -> Result<Layout, StateError> {
    crate::state::ensure_no_catalog_reset_fence(paths)?;
    let marker = paths.state_dir.join("authority-format");
    let old = old_authority(&paths.journal_dir, &paths.state_dir.join("reset-archives"))?;
    if marker.exists() {
        if fs::read(&marker).map_err(|e| StateError::io_at("read authority format", &marker, e))?
            != contents(paths)?
        {
            return Err(invalid("authority-format does not match remanence-tape-log-v1 and the configured journal root"));
        }
        if old {
            return Err(invalid("marked tape-log layout contains old authority files in active paths; declared state reset required"));
        }
        return Ok(Layout::Marked);
    }
    if old
        || paths.sqlite_path.exists()
        || directory_nonempty(&paths.audit_dir)?
        || directory_nonempty(&paths.journal_dir.join("tapes"))?
    {
        return Err(invalid(RESET_REQUIRED));
    }
    Ok(Layout::Empty)
}
fn directory_nonempty(path: &Path) -> Result<bool, StateError> {
    match fs::read_dir(path) {
        Ok(mut entries) => entries
            .next()
            .transpose()
            .map(|v| v.is_some())
            .map_err(|e| StateError::io_at("inspect authority directory", path, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(StateError::io_at("inspect authority directory", path, e)),
    }
}
/// Install after the reset has archived and cleared active inputs, while its
/// durable fence and exclusive state lock remain held.
pub(crate) fn install(paths: &StatePaths, reset: bool) -> Result<(), StateError> {
    let tapes = paths.journal_dir.join("tapes");
    fs::create_dir_all(&tapes)
        .map_err(|e| StateError::io_at("create tape-log directory", &tapes, e))?;
    let marker = paths.state_dir.join("authority-format");
    let temporary = paths
        .state_dir
        .join(format!(".authority-format-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        File::open(&tapes)?.sync_all()?;
        File::open(&paths.journal_dir)?.sync_all()?;
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&contents(paths).map_err(std::io::Error::other)?)?;
        file.sync_all()?;
        if reset {
            fs::rename(&temporary, &marker)?;
        } else {
            durable_file::rename_noreplace(&temporary, &marker)?;
        }
        File::open(&paths.state_dir)?.sync_all()
    })();
    let _ = fs::remove_file(&temporary);
    result.map_err(|e| StateError::io_at("install authority format", &marker, e))
}

/// Admit a configured runtime while its process-lifetime state lock is held.
/// This must precede opening a new catalog and all device discovery.
pub fn admit_owned_config(config: &crate::RemConfig) -> Result<(), StateError> {
    let paths = StatePaths::from_config(Path::new(""), config);
    fs::create_dir_all(&paths.journal_dir)
        .map_err(|e| StateError::io_at("create journal namespace", &paths.journal_dir, e))?;
    admit(&paths)?;
    if paths.sqlite_path.exists() {
        crate::CatalogIndex::open_read_only(&paths.sqlite_path)?;
    }
    Ok(())
}

/// Gate configured runtime entry before media or drive-configuration commands.
/// Read-only: it takes no state lock, so read-only and daemon-delegating
/// commands keep working while the daemon owns the state, and it creates
/// nothing. An unmarked empty state passes without a marker; only an owner
/// that holds the exclusive state lock may install one ([`admit`]).
pub fn validate_config_layout(config: &crate::RemConfig) -> Result<(), StateError> {
    let paths = StatePaths::from_config(Path::new(""), config);
    validate_layout(&paths)?;
    if paths.sqlite_path.exists() {
        crate::CatalogIndex::open_read_only(&paths.sqlite_path)?;
    }
    Ok(())
}

/// The same read-only validation, for daemon constructors that already own
/// the process-lifetime state lock.
pub fn validate_owned_config_layout(config: &crate::RemConfig) -> Result<(), StateError> {
    validate_config_layout(config)
}

/// Load configuration for runtime commands, validating the layout read-only.
/// Declared reset uses the ungated parser so that predecessor state can be
/// archived without opening its journals.
pub fn load_runtime_config(path: impl AsRef<Path>) -> Result<crate::RemConfig, StateError> {
    let config = crate::load_config(path)?;
    validate_config_layout(&config)?;
    Ok(config)
}
