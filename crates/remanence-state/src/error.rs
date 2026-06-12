//! Error types for Layer 4 local state.

use std::fmt;
use std::path::{Path, PathBuf};

/// Layer 4 state-management failures.
#[derive(Debug, thiserror::Error)]
pub enum StateError {
    /// A filesystem operation failed.
    #[error("{context}{}: {source}", io_path_suffix(path))]
    Io {
        /// Human-readable operation context.
        context: String,
        /// Optional path involved in the failure.
        path: Option<PathBuf>,
        /// Underlying I/O failure.
        #[source]
        source: std::io::Error,
    },
    /// The operator configuration is not valid.
    #[error("invalid config: {0}")]
    ConfigInvalid(String),
    /// Another daemon owns the local state directory lock.
    #[error("state lock is already held: {0}")]
    StateLockHeld(PathBuf),
    /// The audit log is corrupt before the trailing record boundary.
    #[error("audit log corrupt: {0}")]
    AuditCorrupt(String),
    /// Replay observed and ignored a torn trailing audit record.
    #[error("audit log has a torn trailing record: {0}")]
    AuditTornTrailingRecord(String),
    /// Audit append failed.
    #[error("audit write failed: {0}")]
    AuditWriteFailed(String),
    /// The filesystem reported that durable space is exhausted.
    #[error("disk full: {0}")]
    DiskFull(String),
    /// Layer 3c journal replay failed while rebuilding projections.
    #[error("journal replay failed: {0}")]
    JournalReplayFailed(String),
    /// SQLite projection migration failed.
    #[error("index migration failed: {0}")]
    IndexMigrationFailed(String),
    /// SQLite projection I/O/query/update failed outside migration invariants.
    #[error("index {context}: {source}")]
    Index {
        /// Human-readable database operation context.
        context: String,
        /// Underlying SQLite failure.
        #[source]
        source: rusqlite::Error,
    },
    /// SQLite projection is corrupt and must be rebuilt.
    #[error("index corrupt: {0}")]
    IndexCorrupt(String),
    /// A projection rebuild is already in progress.
    #[error("index rebuild in progress")]
    IndexRebuildInProgress,
    /// An idempotency key was reused for a different request.
    #[error("idempotency conflict: {0}")]
    IdempotencyConflict(String),
    /// A tape with committed data was assigned to a conflicting pool.
    #[error("tape pool assignment conflict: {0}")]
    TapePoolAssignmentConflict(String),
    /// A catalog tape provisioning request conflicts with committed tape state.
    #[error("tape provisioning conflict: {0}")]
    TapeProvisionConflict(String),
    /// A catalog cache digest does not match its authoritative source.
    #[error("catalog cache digest mismatch: {0}")]
    CatalogCacheDigestMismatch(String),
    /// The lock file looked stale to an operator, but kernel lock state is authoritative.
    #[error("state lock stale diagnostic: {0}")]
    StateLockStale(String),
    /// State-changing work is disabled by config or degraded mode.
    #[error("read-only mode: {0}")]
    ReadOnlyMode(String),
    /// The configured state volume is not trusted for durable fsync semantics.
    #[error("untrusted state volume: {0}")]
    UntrustedStateVolume(String),
    /// Startup lacks permission to create or protect state files.
    #[error("permission denied: {0}")]
    PermissionDenied(String),
}

struct IoPathSuffix<'a>(Option<&'a Path>);

impl fmt::Display for IoPathSuffix<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = self.0 {
            write!(f, " at {}", path.display())?;
        }
        Ok(())
    }
}

fn io_path_suffix(path: &Option<PathBuf>) -> IoPathSuffix<'_> {
    IoPathSuffix(path.as_deref())
}

impl StateError {
    /// Build an I/O error with operation context and a path.
    pub fn io_at(
        context: impl Into<String>,
        path: impl Into<PathBuf>,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            context: context.into(),
            path: Some(path.into()),
            source,
        }
    }

    /// Build an I/O error with operation context and no specific path.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            path: None,
            source,
        }
    }

    /// True when the failure represents a held state lock.
    pub fn is_state_lock_held(&self) -> bool {
        matches!(self, Self::StateLockHeld(_))
    }
}

#[cfg(test)]
mod tests {
    use super::StateError;
    use std::io;

    #[test]
    fn io_at_display_includes_path() {
        let error = StateError::io_at(
            "read config",
            "/var/lib/rem/config.toml",
            io::Error::new(io::ErrorKind::NotFound, "missing"),
        );

        let rendered = error.to_string();
        assert!(rendered.contains("read config at /var/lib/rem/config.toml"));
        assert!(rendered.contains("missing"));
    }

    #[test]
    fn io_display_without_path_omits_empty_placeholder() {
        let error = StateError::io("append audit", io::Error::other("disk stalled"));

        assert_eq!(error.to_string(), "append audit: disk stalled");
    }
}
