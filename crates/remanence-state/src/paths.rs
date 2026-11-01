//! Layer 4 state path handling.

use std::path::{Path, PathBuf};

use crate::config::RemConfig;

/// Concrete local-state paths used by a daemon instance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePaths {
    /// Path to the operator config file.
    pub config_path: PathBuf,
    /// Root directory for mutable daemon state.
    pub state_dir: PathBuf,
    /// Directory containing daily audit-log segments.
    pub audit_dir: PathBuf,
    /// Directory containing per-tape 3c journals.
    pub journal_dir: PathBuf,
    /// Path to the rebuildable SQLite projection.
    pub sqlite_path: PathBuf,
    /// Directory containing per-tape catalog caches.
    pub tape_cache_dir: PathBuf,
    /// Directory holding the durable calibration-control store. Deliberately derived from
    /// `state_dir` and **not** cleared by catalog reset: the
    /// `calibration_generation` allocator inside must never restart,
    /// or a generation could be re-issued and resurrect a cached
    /// negative.
    pub calibration_dir: PathBuf,
}

impl StatePaths {
    /// Build paths from the parsed operator config.
    pub fn from_config(config_path: impl AsRef<Path>, config: &RemConfig) -> Self {
        Self {
            config_path: config_path.as_ref().to_path_buf(),
            state_dir: config.daemon.state_dir.clone(),
            audit_dir: config.audit.dir.clone(),
            journal_dir: config.journal.dir.clone(),
            sqlite_path: config.index.sqlite_path.clone(),
            tape_cache_dir: config.cache.tape_catalog_dir.clone(),
            calibration_dir: config.daemon.state_dir.join("calibration"),
        }
    }
}
