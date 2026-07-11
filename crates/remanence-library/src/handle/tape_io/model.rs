//! Layer 3a value types — `TapePosition`, position proofs,
//! `BlockSize`, `SpaceKind`, `SpaceResult`, write/read outcomes, and
//! `TapeConfig`.
//!
//! These structs carry enough information that callers do not need a
//! follow-up CDB to learn what just happened. Position-bearing results
//! also expose whether a position came from READ POSITION or from
//! arithmetic over a previously proven cursor.

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

/// Position proven by a successful READ POSITION response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePositionProof {
    position: TapePosition,
}

impl DevicePositionProof {
    pub(crate) fn from_device_position(position: TapePosition) -> Self {
        Self { position }
    }

    /// The proven tape position.
    pub fn position(self) -> TapePosition {
        self.position
    }

    /// Logical block address from the proven position.
    pub fn lba(self) -> u64 {
        self.position.lba
    }
}

/// Position computed by arithmetic from an earlier device proof.
///
/// ```compile_fail
/// use remanence_library::{ComputedPosition, DevicePositionProof};
///
/// fn commit_boundary(_: DevicePositionProof) {}
///
/// fn cannot_commit_with_computed(position: ComputedPosition) {
///     commit_boundary(position);
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComputedPosition {
    position: TapePosition,
}

impl ComputedPosition {
    pub(crate) fn from_position(position: TapePosition) -> Self {
        Self { position }
    }

    /// The computed tape position.
    pub fn position(self) -> TapePosition {
        self.position
    }

    /// Logical block address from the computed position.
    pub fn lba(self) -> u64 {
        self.position.lba
    }
}

/// Evidence behind a position-bearing operation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionAfter {
    /// Position came from READ POSITION.
    Device(DevicePositionProof),
    /// Position was advanced arithmetically.
    Computed(ComputedPosition),
}

impl PositionAfter {
    /// Return the tape position regardless of evidence kind.
    pub fn position(self) -> TapePosition {
        match self {
            Self::Device(proof) => proof.position(),
            Self::Computed(position) => position.position(),
        }
    }

    /// Logical block address regardless of evidence kind.
    pub fn lba(self) -> u64 {
        self.position().lba
    }

    /// Return a device proof when this result has one.
    pub fn device_proof(self) -> Option<DevicePositionProof> {
        match self {
            Self::Device(proof) => Some(proof),
            Self::Computed(_) => None,
        }
    }
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
    position_evidence: PositionAfter,
}

impl SpaceResult {
    /// Construct a SPACE result whose post-position came from READ
    /// POSITION.
    pub fn from_device_position(
        units_traversed: i64,
        stopped_at_boundary: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            units_traversed,
            stopped_at_boundary,
            position_after,
            position_evidence: PositionAfter::Device(DevicePositionProof::from_device_position(
                position_after,
            )),
        }
    }

    /// Evidence behind `position_after`.
    pub fn position_evidence(&self) -> PositionAfter {
        self.position_evidence
    }
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
    position_evidence: PositionAfter,
}

impl WriteOutcome {
    /// Construct a write outcome whose post-position came from READ
    /// POSITION.
    pub fn from_device_position(
        bytes_written: u32,
        early_warning: bool,
        end_of_medium: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            bytes_written,
            early_warning,
            end_of_medium,
            position_after,
            position_evidence: PositionAfter::Device(DevicePositionProof::from_device_position(
                position_after,
            )),
        }
    }

    /// Construct a write outcome whose post-position was tracked
    /// arithmetically from a prior device-proven boundary.
    pub fn from_computed_position(
        bytes_written: u32,
        early_warning: bool,
        end_of_medium: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            bytes_written,
            early_warning,
            end_of_medium,
            position_after,
            position_evidence: PositionAfter::Computed(ComputedPosition::from_position(
                position_after,
            )),
        }
    }

    /// Evidence behind `position_after`.
    pub fn position_evidence(&self) -> PositionAfter {
        self.position_evidence
    }

    /// Device proof carried by the legacy single-block write path.
    pub fn device_position_proof(&self) -> Option<DevicePositionProof> {
        self.position_evidence.device_proof()
    }
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
    position_evidence: PositionAfter,
}

impl WriteFilemarksOutcome {
    /// Construct a filemark outcome whose post-position came from
    /// READ POSITION.
    pub fn from_device_position(
        early_warning: bool,
        end_of_medium: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            early_warning,
            end_of_medium,
            position_after,
            position_evidence: PositionAfter::Device(DevicePositionProof::from_device_position(
                position_after,
            )),
        }
    }

    /// Evidence behind `position_after`.
    pub fn position_evidence(&self) -> PositionAfter {
        self.position_evidence
    }

    /// Device proof for the post-filemark boundary.
    pub fn device_position_proof(&self) -> DevicePositionProof {
        match self.position_evidence {
            PositionAfter::Device(proof) => proof,
            PositionAfter::Computed(_) => unreachable!("filemark outcome must be device-proven"),
        }
    }
}

/// Outcome of a fixed-mode multi-record WRITE(6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteBatchOutcome {
    /// Records accepted by the CDB.
    pub records_written: u32,
    /// Bytes represented by `records_written`.
    pub bytes_written: u32,
    /// True iff sense indicated approaching end-of-medium.
    pub early_warning: bool,
    /// True iff the drive reported hard end-of-medium.
    pub end_of_medium: bool,
    /// Position immediately after the accepted records.
    pub position_after: TapePosition,
    position_evidence: PositionAfter,
}

/// Allocation-free write-submitter telemetry snapshot. Percentiles are
/// derived from fixed microsecond buckets; maxima preserve exact samples.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PipelinedWriteDiagnostics {
    /// Completion-to-submit gap samples retained in the histogram.
    pub gap_samples: u64,
    /// SG_IO duration samples retained in the histogram.
    pub ioctl_samples: u64,
    /// Clean data commands recorded before any following tripwire.
    pub good_commands: u64,
    /// Records carried by clean data commands.
    pub good_records: u64,
    /// Bytes carried by clean data commands.
    pub good_bytes: u64,
    /// Residual/transport disagreements observed on position-arbitrated EOM/EW.
    pub residual_claim_mismatches: u64,
    /// Median completion-to-next-submit gap in microseconds.
    pub gap_p50_us: u64,
    /// 95th-percentile completion-to-next-submit gap in microseconds.
    pub gap_p95_us: u64,
    /// Maximum completion-to-next-submit gap in microseconds.
    pub gap_max_us: u64,
    /// Median SG_IO duration in microseconds.
    pub ioctl_p50_us: u64,
    /// 95th-percentile SG_IO duration in microseconds.
    pub ioctl_p95_us: u64,
    /// Maximum SG_IO duration in microseconds.
    pub ioctl_max_us: u64,
    /// Mean submit-to-submit cadence in microseconds.
    pub cadence_us: u64,
    /// Effective bytes fed per second over the observed command span.
    pub effective_feed_bytes_per_second: u64,
}

/// Read-submitter telemetry has the same allocation-free cadence schema as
/// write submission. The alias keeps field tooling identical while restore
/// diagnostics name the direction explicitly.
pub type PipelinedReadDiagnostics = PipelinedWriteDiagnostics;

impl WriteBatchOutcome {
    /// Construct a batch outcome backed by an arithmetic cursor.
    pub fn from_computed_position(
        records_written: u32,
        bytes_written: u32,
        early_warning: bool,
        end_of_medium: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            records_written,
            bytes_written,
            early_warning,
            end_of_medium,
            position_after,
            position_evidence: PositionAfter::Computed(ComputedPosition::from_position(
                position_after,
            )),
        }
    }

    pub(crate) fn from_device_position(
        records_written: u32,
        bytes_written: u32,
        early_warning: bool,
        end_of_medium: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            records_written,
            bytes_written,
            early_warning,
            end_of_medium,
            position_after,
            position_evidence: PositionAfter::Device(DevicePositionProof::from_device_position(
                position_after,
            )),
        }
    }

    /// Evidence behind `position_after`.
    pub fn position_evidence(&self) -> PositionAfter {
        self.position_evidence
    }
}

/// Outcome of a fixed-mode multi-record READ(6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadBatchOutcome {
    /// Data records read before completion or a filemark backstop.
    pub records_read: u32,
    /// Bytes represented by `records_read`.
    pub bytes_read: u32,
    /// True iff READ stopped on a filemark.
    pub filemark: bool,
    /// Position after the read. Filemark outcomes include the
    /// consumed filemark in this arithmetic position.
    pub position_after: TapePosition,
    position_evidence: PositionAfter,
}

/// A fixed-read handoff together with the position evidence produced by the
/// same command. Keeping this wrapper outside [`ReadBufferHandoff`] preserves
/// the handoff's length-typed data surface while preventing evidence loss.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadHandoffOutcome {
    /// Position immediately after the fixed READ.
    pub position_after: TapePosition,
    /// Whether that position was computed or proven by READ POSITION.
    pub evidence: PositionAfter,
    /// The unchanged, stale-tail-safe data handoff.
    pub handoff: ReadBufferHandoff,
}

/// One ordered data-buffer delivery from the read submitter.
#[derive(Debug, PartialEq, Eq)]
pub struct SequencedHandoff {
    /// Strictly monotonic command sequence within the read window.
    pub seq: u64,
    /// Cumulative planned records completed through this command.
    pub plan_records_end: u64,
    /// Position immediately after this command.
    pub position_after: TapePosition,
    /// Position evidence carried by this command's funnel outcome.
    pub evidence: PositionAfter,
    /// The unchanged, stale-tail-safe data handoff.
    pub handoff: ReadBufferHandoff,
}

/// In-band delivery protocol between the read submitter and decoder.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadDelivery {
    /// A data-bearing fixed-read completion.
    Handoff(SequencedHandoff),
    /// A standalone READ POSITION proof naming the exact completed frontier.
    ProofFrontier {
        /// Highest command sequence covered by the proof.
        through_seq: u64,
        /// Cumulative planned records covered by the proof.
        plan_records_end: u64,
        /// Device position proof for that frontier.
        proof: DevicePositionProof,
    },
}

/// Terminal conditions attached to a successfully delivered fixed-read buffer.
/// Fail-closed errors never produce a handoff, so `error` remains false for a
/// value returned through the safe read API and exists to keep the relay
/// contract explicit and forward-compatible.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadTerminalFlags {
    /// The READ consumed a filemark after the delivered records.
    pub filemark: bool,
    /// The READ reached logical end-of-data.
    pub end_of_data: bool,
    /// A terminal error accompanied this buffer.
    pub error: bool,
}

/// Owned, length-typed handoff from the tape submitter to a read consumer.
/// The backing slab's visible region is shortened to `valid_bytes` before
/// construction, so stale bytes from a reused ring slot cannot be observed
/// through safe APIs.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadBufferHandoff {
    buffer: ReadBuffer,
    /// Bytes proven complete by the READ completion and residual decode.
    pub valid_bytes: usize,
    /// Complete fixed records represented by `valid_bytes`.
    pub records_read: u32,
    /// Boundary conditions decoded from the completion.
    pub terminal_flags: ReadTerminalFlags,
}

impl ReadBufferHandoff {
    pub(crate) fn from_outcome(
        mut buffer: ReadBuffer,
        outcome: ReadBatchOutcome,
    ) -> Result<Self, &'static str> {
        let valid_bytes = outcome.bytes_read as usize;
        if valid_bytes > buffer.len() {
            return Err("read outcome exceeds supplied buffer");
        }
        buffer.resize(valid_bytes)?;
        Ok(Self {
            buffer,
            valid_bytes,
            records_read: outcome.records_read,
            terminal_flags: ReadTerminalFlags {
                filemark: outcome.filemark,
                end_of_data: false,
                error: false,
            },
        })
    }

    /// Return only bytes proven complete by the READ outcome.
    pub fn valid_data(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    /// Reclaim the allocation for a later ring refill. Its visible length is
    /// still exactly `valid_bytes`; callers must resize before the next READ.
    pub fn into_reusable_buffer(self) -> ReadBuffer {
        self.buffer
    }
}

/// Page-aligned owned storage reused by the read reservoir.
#[derive(Debug, PartialEq, Eq)]
pub struct ReadBuffer {
    storage: Vec<u8>,
    start: usize,
    len: usize,
    capacity: usize,
}

impl ReadBuffer {
    /// Allocate `capacity` usable bytes whose first byte is page-aligned.
    pub fn try_new_page_aligned(capacity: usize) -> Result<Self, String> {
        let page_size = system_page_size();
        let allocation = capacity
            .checked_add(page_size - 1)
            .ok_or_else(|| "read reservoir allocation size overflow".to_string())?;
        let mut storage = Vec::new();
        storage
            .try_reserve_exact(allocation)
            .map_err(|err| format!("allocate page-aligned read buffer: {err}"))?;
        storage.resize(allocation, 0);
        let address = storage.as_ptr() as usize;
        let start = (page_size - (address % page_size)) % page_size;
        debug_assert_eq!((address + start) % page_size, 0);
        debug_assert!(start + capacity <= storage.len());
        Ok(Self {
            storage,
            start,
            len: capacity,
            capacity,
        })
    }

    /// Visible byte length.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the visible region is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Fixed usable capacity of this slab.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Resize the visible region without reallocating the aligned slab.
    pub fn resize(&mut self, len: usize) -> Result<(), &'static str> {
        if len > self.capacity {
            return Err("read buffer resize exceeds aligned slab capacity");
        }
        self.len = len;
        Ok(())
    }

    /// Visible bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.len]
    }

    /// Mutable visible bytes.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.storage[self.start..self.start + self.len]
    }

    /// True when the first visible byte is aligned to the system page size.
    pub fn is_page_aligned(&self) -> bool {
        (self.as_slice().as_ptr() as usize) % system_page_size() == 0
    }
}

impl From<Vec<u8>> for ReadBuffer {
    fn from(storage: Vec<u8>) -> Self {
        let len = storage.len();
        Self {
            storage,
            start: 0,
            len,
            capacity: len,
        }
    }
}

fn system_page_size() -> usize {
    // SAFETY: `sysconf(_SC_PAGESIZE)` takes no pointers and has no memory side
    // effects. A non-positive result falls back to 4 KiB.
    let reported = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(reported)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(4096)
}

impl ReadBatchOutcome {
    pub(crate) fn from_computed_position(
        records_read: u32,
        bytes_read: u32,
        filemark: bool,
        position_after: TapePosition,
    ) -> Self {
        Self {
            records_read,
            bytes_read,
            filemark,
            position_after,
            position_evidence: PositionAfter::Computed(ComputedPosition::from_position(
                position_after,
            )),
        }
    }

    pub(crate) fn from_position_evidence(
        records_read: u32,
        bytes_read: u32,
        filemark: bool,
        position_evidence: PositionAfter,
    ) -> Self {
        Self {
            records_read,
            bytes_read,
            filemark,
            position_after: position_evidence.position(),
            position_evidence,
        }
    }

    /// Evidence behind `position_after`.
    pub fn position_evidence(&self) -> PositionAfter {
        self.position_evidence
    }
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
        let r = SpaceResult::from_device_position(-5, false, p);
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
        let o = WriteOutcome::from_device_position(1024 * 1024, false, false, p);
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
        let o = WriteOutcome::from_device_position(512 * 1024, true, false, p);
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
