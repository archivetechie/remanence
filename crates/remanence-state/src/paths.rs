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
        }
    }

    /// Return the Layer 3c journal path for a tape UUID.
    pub fn journal_path(&self, tape_uuid: [u8; 16]) -> PathBuf {
        self.journal_dir
            .join(format!("{}.remjournal", hex_uuid(tape_uuid)))
    }
}

fn hex_uuid(tape_uuid: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in tape_uuid {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_path_uses_lowercase_hex_uuid() {
        let paths = StatePaths {
            config_path: "/tmp/rem/config.toml".into(),
            state_dir: "/tmp/rem".into(),
            audit_dir: "/tmp/rem/audit".into(),
            journal_dir: "/tmp/rem/journals".into(),
            sqlite_path: "/tmp/rem/index/rem-state.sqlite".into(),
            tape_cache_dir: "/tmp/rem/cache/tapes".into(),
        };

        assert_eq!(
            paths.journal_path([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
            PathBuf::from("/tmp/rem/journals/000102030405060708090a0b0c0d0e0f.remjournal")
        );
    }
}
