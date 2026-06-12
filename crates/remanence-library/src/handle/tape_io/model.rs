//! Layer 3a value types — `TapePosition`, `BlockSize`, `SpaceKind`,
//! `SpaceResult`, `WriteOutcome`, `TapeConfig`.
//!
//! All public, all `#[derive]`-only (no manual impls). Each carries
//! enough that the caller doesn't need a follow-up CDB to learn what
//! just happened — see `docs/layer3a-design.md` §4 for the rationale
//! behind each field.

/// Output of `READ POSITION` (long-form) — where the head sits right
/// now. Returned by every Layer 3a method that moves the tape so the
/// caller doesn't need a follow-up `position()` round-trip.
///
/// LBA is canonical. Physical position is intentionally not exposed
/// — physical addresses aren't portable across drives, generations,
/// or even read passes within the same cartridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapePosition {
    /// Logical block address. `READ POSITION` long-form returns 8
    /// bytes (64-bit); short-form would return 4 bytes that get
    /// zero-extended here.
    pub lba: u64,
    /// Partition number. Decoded from `READ POSITION` long-form
    /// bytes 4..8 (4-byte big-endian, per IBM LTO SCSI Reference
    /// §5.2.22.3 / Table 99) — codex 19:57 caught the earlier
    /// byte-1 parse. rem operates only in partition 0 in
    /// production (LTFS uses partition 1 for its index, one
    /// reason rem does not use LTFS), but the wider type
    /// reflects the on-wire field accurately.
    pub partition: u32,
    /// True iff the head is at the beginning of the partition (BOP).
    pub beginning_of_partition: bool,
    /// True iff the head is at logical end-of-partition (past the
    /// last written block).
    pub end_of_partition: bool,
    /// True iff the drive set **BPEW** (Beyond Programmable
    /// Early Warning; IBM LTO SCSI Reference §5.2.16 / Table 99,
    /// READ POSITION long-form) in the response — head is past
    /// the programmable early-warning point near EOM. Surfaces
    /// the near-EOM signal for orchestrator handling; rem
    /// itself does not auto-retire near-EOM tapes (Layer 5
    /// policy).
    pub block_position_end_of_warning: bool,
}

/// How Layer 3a addresses the variable-vs-fixed block choice.
///
/// rem-chunked-v1 uses [`Self::Variable`] (the LTO factory default
/// and POSIX tar convention). [`Self::Fixed`] is opt-in via
/// `DriveHandle::write_config` for formats that need uniform-size
/// block enforcement (e.g. streaming uncompressed video at a known
/// frame size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSize {
    /// Variable-block mode. Each WRITE accepts a buffer of any size
    /// (subject to the drive's hardware cap); each READ returns one
    /// block of whatever size is on tape, up to the host buffer.
    Variable,
    /// Fixed-block mode. Every read and write is a multiple of
    /// `size_bytes`. The drive enforces it; mismatched buffers
    /// return CHECK CONDITION.
    Fixed {
        /// Block size in bytes. Zero is invalid; multi-MiB values
        /// must fit in the drive's documented maximum (queryable
        /// via `read_config()` → `TapeConfig::max_block_size_bytes`).
        size_bytes: u32,
    },
}

/// Motion-type code for `SPACE`. Maps 1-to-1 onto
/// [`remanence_scsi::space::SpaceCode`] but exposed separately at the
/// Layer 3a surface so callers don't depend on the SCSI crate.
///
/// **IBM LTO support note**: per the IBM LTO SCSI Reference SPACE
/// table, only CODEs 0 (Blocks), 1 (Filemarks), and 3 (End of
/// Data) are implemented. CODE 2 (SequentialFilemarks) is
/// Reserved and the drive returns INVALID FIELD IN CDB.
/// [`DriveHandle::space`](super::super::DriveHandle::space)
/// rejects `SequentialFilemarks` at the API boundary so it never
/// reaches the wire — but the variant remains in the enum for
/// SSC-vocabulary parity. Codex 20:00 catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceKind {
    /// Skip N blocks (positive = forward, negative = backward).
    Blocks,
    /// Skip N file marks.
    Filemarks,
    /// Reserved on IBM LTO — `DriveHandle::space` returns
    /// `TapeIoError::InvalidRequest(InvalidInput)` and does not issue
    /// a CDB. Callers wanting "advance to next file mark" should use
    /// `space(1, Filemarks)` instead.
    SequentialFilemarks,
    /// Move to End-of-Data. Count is ignored by the drive.
    EndOfData,
}

/// Outcome of a `space()` call. `SPACE` can stop short of the
/// requested count if it hits a file mark / EOD / BOP / EOM —
/// callers need to know whether their request actually completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceResult {
    /// Signed number of units the tape actually moved. Negative
    /// means backward. Always between `-count` and `+count`.
    pub units_traversed: i64,
    /// True iff `SPACE` stopped because it hit a file mark or
    /// EOD mid-traversal. Callers often want to know — e.g., a
    /// backward block-skip that hit BOP returns `true` here, and
    /// the caller knows position is at BOP without a second CDB.
    pub stopped_at_boundary: bool,
    /// Position immediately after the SPACE, queried via an inline
    /// READ POSITION. Lets the caller turn a relative skip into an
    /// absolute LBA for the next read without a second round-trip.
    pub position_after: TapePosition,
}

/// Outcome of a `write_block` call. `WRITE` can stop short of the
/// requested buffer if the drive hits EOM (end-of-medium) or
/// early-warning. Callers need byte-accurate accounting plus the
/// signals to handle near-EOM gracefully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Bytes actually committed to media. May be less than the
    /// buffer length when the drive reports an early-warning state
    /// in sense data and stops writing.
    pub bytes_written: u32,
    /// True iff sense data indicated approaching end-of-medium —
    /// the drive is past the early-warning point. The orchestrator
    /// should plan to close the tape soon.
    pub early_warning: bool,
    /// True iff the drive reported end-of-medium reached. Further
    /// writes will fail.
    pub end_of_medium: bool,
    /// Position immediately after the write (from an inline READ
    /// POSITION). Lets the caller learn the LBA of the block it
    /// just wrote without a second round-trip — useful for recording
    /// the per-chunk LBA in the catalog.
    pub position_after: TapePosition,
}

/// Outcome of a `write_block_unpositioned` call. This is the hot-path
/// shape for callers that already track sequential position and do not
/// need a READ POSITION after every clean block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteUnpositionedOutcome {
    /// Bytes actually committed to media. May be less than the
    /// buffer length when the drive reports an early-warning state
    /// in sense data and stops writing.
    pub bytes_written: u32,
    /// True iff sense data indicated approaching end-of-medium.
    pub early_warning: bool,
    /// True iff the drive reported end-of-medium reached.
    pub end_of_medium: bool,
}

/// Outcome of a `write_filemarks` call. WRITE FILEMARKS(6) on
/// LTO can cross the programmable early-warning point (PEWZ) or
/// hit hard EOM — per IBM LTO SCSI Reference §4.8, the drive
/// surfaces this as CHECK CONDITION with NO SENSE + EOM bit set
/// (NO SENSE 0x0 = informational; VOLUME OVERFLOW 0x0D = hard).
/// The filemark is committed; the caller learns the post-write
/// position plus the EW / EOM flags from this struct rather than
/// having to re-decode sense or re-position. Mirrors
/// [`WriteOutcome`] for symmetry.
///
/// Codex 20:17 (idref=6e9b56d9 High) caught the earlier shape
/// where `write_filemarks` returned only `TapePosition` and
/// silently mapped EW to Err, leading to caller retries that
/// would double-write filemarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteFilemarksOutcome {
    /// True iff sense indicated approaching end-of-medium —
    /// further filemarks/writes may still succeed but the drive
    /// is past the EW point.
    pub early_warning: bool,
    /// True iff the drive reported VOLUME OVERFLOW — further
    /// writes will fail.
    pub end_of_medium: bool,
    /// Position immediately after the marks were committed (via
    /// inline READ POSITION). The caller can record the LBA of
    /// the marks without a second round-trip.
    pub position_after: TapePosition,
}

/// WORM media state decoded from drive-reported mode data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WormMediaState {
    /// The loaded medium is not reported as WORM.
    NotWorm,
    /// The loaded medium is WORM.
    Worm,
    /// The drive did not report a recognized loaded-medium type.
    Unknown,
}

/// Current block-size + compression configuration of a loaded tape.
/// Returned by `DriveHandle::read_config` (queried via MODE SENSE
/// pages 0x10 + 0x0F) and consumed by `write_config` (issued as a
/// MODE SELECT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TapeConfig {
    /// Block-size mode: [`BlockSize::Variable`] or [`BlockSize::Fixed`].
    pub block_size: BlockSize,
    /// Whether the drive's hardware compression is enabled. `false`
    /// is the rem-chunked-v1 default — pre-compressed data
    /// (zstd-seekable) doesn't compress further and the wasted CPU
    /// matters at LTO line rate. See `docs/pfr-reference.md` §6.3.
    pub compression: bool,
    /// Drive-reported maximum logical block size in bytes. Set by
    /// `read_config()` from the device's **READ BLOCK LIMITS**
    /// response (IBM LTO SCSI Reference §5.2.17.1 / Table 78 —
    /// `MAXIMUM BLOCK LENGTH LIMIT` field), NOT from MODE SENSE.
    /// Ignored by `write_config()` (the drive's own cap always
    /// wins). Useful for variable-block READ buffer sizing.
    ///
    /// Note (codex 19:57 follow-up): the **reported** RBL value
    /// is not always the same as the **supported** maximum. IBM
    /// Table 78 documents the field value as `0x80_0000` (8 MiB)
    /// on LTO-9 hardware, and Note 15 says larger no-encryption
    /// block lengths *may* be accepted but are not reported. §4.11
    /// gives `0xFF_FFFF` (16 MiB - 1) as the *supported* unencrypted
    /// maximum. Layer 3a stores the actual RBL response value;
    /// callers wanting the supported cap should not infer it
    /// from `max_block_size_bytes` alone.
    pub max_block_size_bytes: u32,
    /// Write-protect bit from the MODE SENSE parameter header.
    pub write_protected: bool,
    /// WORM/non-WORM media state inferred from the MODE SENSE medium type.
    pub worm: WormMediaState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tape_position_at_bot_is_all_zero() {
        let p = TapePosition {
            lba: 0,
            partition: 0,
            beginning_of_partition: true,
            end_of_partition: false,
            block_position_end_of_warning: false,
        };
        assert_eq!(p.lba, 0);
        assert!(p.beginning_of_partition);
    }

    #[test]
    fn block_size_variants_equal_themselves() {
        assert_eq!(BlockSize::Variable, BlockSize::Variable);
        assert_eq!(
            BlockSize::Fixed { size_bytes: 1024 },
            BlockSize::Fixed { size_bytes: 1024 }
        );
        assert_ne!(BlockSize::Variable, BlockSize::Fixed { size_bytes: 1024 });
        assert_ne!(
            BlockSize::Fixed { size_bytes: 1024 },
            BlockSize::Fixed { size_bytes: 2048 }
        );
    }

    #[test]
    fn space_kind_distinct_variants() {
        assert_ne!(SpaceKind::Blocks, SpaceKind::Filemarks);
        assert_ne!(SpaceKind::Filemarks, SpaceKind::SequentialFilemarks);
        assert_ne!(SpaceKind::SequentialFilemarks, SpaceKind::EndOfData);
    }

    #[test]
    fn space_result_signed_units_round_trip() {
        // Backward skip case: -5 blocks reported as -5 (twos-complement
        // happy path; the SCSI builder takes care of CDB byte encoding).
        let p = TapePosition {
            lba: 42,
            partition: 0,
            beginning_of_partition: false,
            end_of_partition: false,
            block_position_end_of_warning: false,
        };
        let r = SpaceResult {
            units_traversed: -5,
            stopped_at_boundary: false,
            position_after: p,
        };
        assert_eq!(r.units_traversed, -5);
        assert_eq!(r.position_after.lba, 42);
    }

    #[test]
    fn write_outcome_happy_path_no_warnings() {
        let p = TapePosition {
            lba: 1001,
            partition: 0,
            beginning_of_partition: false,
            end_of_partition: false,
            block_position_end_of_warning: false,
        };
        let o = WriteOutcome {
            bytes_written: 1024 * 1024,
            early_warning: false,
            end_of_medium: false,
            position_after: p,
        };
        assert!(!o.early_warning);
        assert!(!o.end_of_medium);
        assert_eq!(o.bytes_written, 1024 * 1024);
    }

    #[test]
    fn write_outcome_near_eom() {
        let p = TapePosition {
            lba: u64::MAX - 1,
            partition: 0,
            beginning_of_partition: false,
            end_of_partition: false,
            block_position_end_of_warning: true,
        };
        let o = WriteOutcome {
            bytes_written: 512 * 1024,
            early_warning: true,
            end_of_medium: false,
            position_after: p,
        };
        assert!(o.early_warning);
        assert!(o.position_after.block_position_end_of_warning);
    }

    #[test]
    fn tape_config_chunked_v1_default_shape() {
        // The rem-chunked-v1 default per pfr-reference.md §6.3:
        // variable-block, compression off. max_block_size_bytes is
        // a drive-reported value; tests construct a representative
        // unencrypted-cartridge LTO-9 cap of 0xFFFFFF per IBM
        // LTO SCSI Reference §4.11 (codex 03706ad5 caught the
        // earlier off-by-one 16 MiB literal).
        let c = TapeConfig {
            block_size: BlockSize::Variable,
            compression: false,
            max_block_size_bytes: 0xFF_FFFF,
            write_protected: false,
            worm: WormMediaState::NotWorm,
        };
        assert_eq!(c.block_size, BlockSize::Variable);
        assert!(!c.compression);
        assert_eq!(c.max_block_size_bytes, 0xFF_FFFF);
        assert!(!c.write_protected);
        assert_eq!(c.worm, WormMediaState::NotWorm);
    }
}
