//! Writer commit check values for a range of fixed data blocks.
//!
//! For committed ordinals `[W, T)`, feed each whole-block `data_shard_crc64`
//! in ascending ordinal order. The check is CRC-64/XZ over their eight-byte
//! little-endian encodings; the empty range has value zero. This detects
//! accidental differences in position, range or bytes on the host path. It
//! does not defend against deliberate tampering. This writer mechanism belongs
//! to the Implementation Guide's writing chapter, not the REM-PARITY format;
//! readers do not need it.

use remanence_crc::crc64_xz_update_slice;
use serde::{Deserialize, Serialize};

/// CRC-64/XZ over the ordered little-endian whole-block CRCs of a committed range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckValue(pub u64);

impl CheckValue {
    /// Check value for an empty range.
    pub const EMPTY: CheckValue = CheckValue(0);
}

/// Incremental writer check with a pre-final-XOR CRC register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckValueBuilder {
    register: u64,
}

impl CheckValueBuilder {
    /// Start an empty range.
    pub fn new() -> Self {
        Self { register: u64::MAX }
    }

    /// Extend the range by the next whole-block CRC in ascending ordinal order.
    pub fn push(&mut self, block_crc: u64) {
        self.register = crc64_xz_update_slice(self.register, &block_crc.to_le_bytes());
    }

    /// Read the current check without consuming or changing the running register.
    pub fn value(&self) -> CheckValue {
        CheckValue(self.register ^ u64::MAX)
    }
}

impl Default for CheckValueBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the writer check from whole-block CRCs in ascending ordinal order.
pub fn check_value(block_crcs: impl IntoIterator<Item = u64>) -> CheckValue {
    let mut builder = CheckValueBuilder::new();
    for block_crc in block_crcs {
        builder.push(block_crc);
    }
    builder.value()
}

/// Source of the uncovered-range check; neither nonempty variant proves readback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckProvenance {
    /// W=T and check zero.
    Empty,
    /// Computed from original write bytes.
    OriginalWrite,
    /// Includes reread bytes not compared to prior authority (until step 3).
    UnverifiedRebuild,
}

/// Barrier-proved physical append coordinate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitCursor {
    /// SCSI partition.
    pub partition: u32,
    /// Logical block/filemark coordinate.
    pub lba: u64,
}
