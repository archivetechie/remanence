//! Writer diagnostics captured at bootstrap write time or once before terminal planning.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

use crate::{diagnostic_text, ParityError};

/// Software identity and a shared clock, preserved across sink detach/reattach.
#[derive(Clone)]
pub struct WriterIdentity {
    software: String,
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl std::fmt::Debug for WriterIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterIdentity")
            .field("software", &self.software)
            .finish_non_exhaustive()
    }
}

impl WriterIdentity {
    /// Construct an identity with a system UTC clock.
    pub fn system(software: String) -> Self {
        Self::with_clock(software, OffsetDateTime::now_utc)
    }

    /// Construct an identity with a fixed or scripted clock.
    ///
    /// Panics if software is not at most 128 printable ASCII bytes.
    pub fn with_clock(
        software: String,
        clock: impl Fn() -> OffsetDateTime + Send + Sync + 'static,
    ) -> Self {
        assert!(diagnostic_text::validate_writer_version(&software).is_ok());
        Self {
            software,
            clock: Arc::new(clock),
        }
    }

    /// Construct a fixed identity for deterministic tests and generators.
    pub fn fixed(software: String, instant: OffsetDateTime) -> Self {
        Self::with_clock(software, move || instant)
    }

    /// Software string for bootstrap key 3.
    pub fn software(&self) -> &str {
        &self.software
    }

    /// Read the clock for bootstrap key 4, as RFC 3339 UTC (at most 64 bytes).
    pub fn written_at(&self) -> Result<String, time::error::Format> {
        (self.clock)().to_offset(UtcOffset::UTC).format(&Rfc3339)
    }

    /// Capture keys 6/7 once, before a terminal-prefix planner fixes its bytes.
    pub fn capture(&self) -> Result<ParityMapDiagnostics, ParityError> {
        Ok(ParityMapDiagnostics {
            writer_version: self.software.clone(),
            write_timestamp: self.written_at().map_err(|_| {
                ParityError::Invariant("writer clock cannot be formatted as RFC3339")
            })?,
        })
    }
}

/// Immutable diagnostic pair used by both the terminal ParityMap and edition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParityMapDiagnostics {
    /// Planning software, at most 128 printable ASCII bytes.
    pub writer_version: String,
    /// Planning time, RFC 3339 UTC, at most 64 bytes.
    pub write_timestamp: String,
}

impl ParityMapDiagnostics {
    /// Validate diagnostics restored from a persisted terminal plan.
    pub fn validate(&self) -> Result<(), ParityError> {
        diagnostic_text::validate_writer_version(&self.writer_version)
            .map_err(ParityError::Invariant)?;
        diagnostic_text::validate_write_timestamp(&self.write_timestamp)
            .map_err(ParityError::Invariant)?;
        let instant = OffsetDateTime::parse(&self.write_timestamp, &Rfc3339)
            .map_err(|_| ParityError::Invariant("planning timestamp is not RFC3339"))?;
        if instant.offset() != UtcOffset::UTC {
            return Err(ParityError::Invariant("planning timestamp is not UTC"));
        }
        Ok(())
    }
}

#[cfg(test)]
include!("advancing_test_identity.rs");
