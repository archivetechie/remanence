//! [`ParitySink`] — wraps a raw physical tape sink and emits Layer 3c v0.4.4
//! parity sidecar tape files.
//!
//! Object tape files contain only body-format fixed blocks. Parity is
//! accumulated in memory and committed as sidecar tape files after object
//! filemarks or at final finish. Completed-epoch sidecars are currently a
//! volatile RAM spool bounded by the object-start capacity reserve; the planned
//! local-disk parity spool is not wired yet. The legacy v0.2 inline parity
//! frame path is not part of the active writer surface.
//!
//! ### Filemark ownership
//!
//! Layer 3c owns every physical filemark. Body-format writers feed only fixed
//! object data blocks through the body-facing [`BlockSink`] surface; object
//! filemarks are emitted by [`ParitySink::finish_object`], sidecar filemarks by
//! the sidecar emitter, and bootstrap filemarks by the bootstrap writer. The
//! `BlockSink::write_filemarks` implementation on [`ParitySink`] therefore
//! rejects external calls so a body format cannot silently introduce an
//! untracked tape-file boundary.

use std::collections::BTreeMap;

use remanence_library::scsi::ScsiError;
use remanence_library::{
    BlockSink, TapeIoError, TapePosition, WriteFilemarksOutcome, WriteOutcome,
};

use crate::bootstrap::{
    validate_bootstrap_object_row, validate_bootstrap_object_rows, write_bootstrap_block,
    BootstrapObjectRow, BootstrapPayload, ParitySchemeRecord,
};
use crate::capacity::{CapacityReserveCause, CapacityReserveInput, CapacityReserveReport};
use crate::codec::ReedSolomonCodec;
use crate::durable::DurableBoundaryState;
use crate::error::ParityError;
use crate::filemark_map::{
    FilemarkMap, FilemarkMapBuilder, FilemarkMapDigest, TapeFileKind, TapeFileMapEntry,
};
use crate::journal::{CommittedBundle, CommittedBundleKind, TapeFileEntry, TapeFileJournal};
use crate::model::{FinalGeometry, ParityScheme};
use crate::parity_map::{
    encode_parity_map_tape_file, ParityMapPayload, ParityMapReference, SidecarEpochDirectory,
    SidecarEpochDirectoryEntry, SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
    SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
};
use crate::raw::{PhysicalPositionHint, RawTapeSink, RawWriteOutcome};
use crate::resume::{ResumeAppendResult, ResumeLiveEpochState};
use crate::sidecar::{data_shard_crc64, encode_sidecar_tape_file, SidecarDescriptor};

const INLINE_DIRECTORY_PRODUCTION_MARGIN_BYTES: usize = 4096;
const OBJECT_ROW_METADATA_FRAME_MAX_LEN: u64 = 16 * 1024 * 1024;

/// Representation class for pre-write bootstrap object-row admission.
///
/// The actual row is still recorded after object bytes are written, when
/// representation-specific anchors are known. This value lets RAO writers
/// reserve worst-case key-30 row space before the first object block reaches
/// tape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapObjectRowAdmission {
    /// A plaintext RAO row carrying manifest location, size, count, and digest.
    PlaintextRao,
    /// An encrypted RAO row carrying only envelope-visible key id and metadata length.
    EncryptedRao,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveObject {
    tape_file_number: u32,
    projected_size_blocks: u64,
    pending_sidecars_at_start: u64,
    pending_sidecar_limit: u64,
    written_blocks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EarlyWarningReserveEvent {
    ObjectDataBlock,
    ObjectFilemark,
    SidecarBlock,
    SidecarFilemark,
    BootstrapBlock,
    BootstrapFilemark,
}

impl EarlyWarningReserveEvent {
    fn reserve_cost_blocks(self, input: CapacityReserveInput) -> u64 {
        match self {
            Self::ObjectDataBlock => 0,
            Self::ObjectFilemark => filemark_reserve_cost_blocks(input.object_filemark_blocks),
            Self::SidecarBlock => 1,
            Self::SidecarFilemark => filemark_reserve_cost_blocks(input.sidecar_filemark_blocks),
            Self::BootstrapBlock => 1,
            Self::BootstrapFilemark => {
                filemark_reserve_cost_blocks(input.bootstrap_filemark_blocks)
            }
        }
    }
}

/// Count a successful filemark as at least one completed tape operation for
/// the runtime EW guard, even if the admission estimate was under-modeled.
///
/// Admission-time reserve math evaluates the caller's model as supplied, but
/// runtime accounting has seen the filemark actually land on tape. A zero
/// filemark estimate is therefore treated as a catalog/modeling bug and
/// charged as one consumed tape block for EW-only continuation decisions.
fn filemark_reserve_cost_blocks(model_blocks: u64) -> u64 {
    model_blocks.max(1)
}

fn worst_case_bootstrap_object_row(
    admission: BootstrapObjectRowAdmission,
    tape_file_number: u32,
    stored_block_count: u64,
    block_size_bytes: u32,
) -> Result<BootstrapObjectRow, ParityError> {
    if stored_block_count == 0 {
        return Err(ParityError::Invariant(
            "bootstrap object-row admission requires a positive projected object size",
        ));
    }
    match admission {
        BootstrapObjectRowAdmission::PlaintextRao => {
            let max_chunks_by_capacity = u64::MAX / u64::from(block_size_bytes);
            let manifest_chunk_count = stored_block_count.min(max_chunks_by_capacity);
            let manifest_first_chunk_lba =
                stored_block_count.checked_sub(manifest_chunk_count).ok_or(
                    ParityError::Invariant("worst-case manifest chunk range underflows"),
                )?;
            let manifest_size_bytes = manifest_chunk_count
                .checked_mul(u64::from(block_size_bytes))
                .ok_or(ParityError::Invariant(
                    "worst-case manifest byte capacity overflows",
                ))?;
            Ok(BootstrapObjectRow::plaintext(
                tape_file_number,
                stored_block_count,
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                [0xFF; 32],
            ))
        }
        BootstrapObjectRowAdmission::EncryptedRao => Ok(BootstrapObjectRow::encrypted(
            tape_file_number,
            stored_block_count,
            [0xFF; 16],
            OBJECT_ROW_METADATA_FRAME_MAX_LEN,
        )),
    }
}

fn worst_case_parity_map_reference() -> ParityMapReference {
    ParityMapReference {
        tape_file_number: u32::MAX,
        block_count: u64::MAX,
        directory_scope_tape_file_count: u32::MAX,
        directory_scope_total_data_ordinals: u64::MAX,
        directory_scope_highest_protected_ordinal: u64::MAX,
        is_final_directory: true,
        parity_map_payload_sha256: [0xFF; 32],
        canonical_map_digest: [0xFF; 32],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EarlyWarningReserveState {
    input: CapacityReserveInput,
    report: CapacityReserveReport,
    object_blocks_written: u64,
    reserve_blocks_consumed: u64,
}

impl EarlyWarningReserveState {
    fn new(input: CapacityReserveInput, report: CapacityReserveReport) -> Self {
        Self {
            input,
            report,
            object_blocks_written: 0,
            reserve_blocks_consumed: 0,
        }
    }

    fn record_successful_event(
        &mut self,
        event: EarlyWarningReserveEvent,
    ) -> Result<(), ParityError> {
        match event {
            EarlyWarningReserveEvent::ObjectDataBlock => {
                self.object_blocks_written =
                    self.object_blocks_written
                        .checked_add(1)
                        .ok_or(ParityError::Invariant(
                            "early-warning reserve object block count overflows",
                        ))?;
            }
            _ => {
                self.reserve_blocks_consumed = self
                    .reserve_blocks_consumed
                    .checked_add(event.reserve_cost_blocks(self.input))
                    .ok_or(ParityError::Invariant(
                        "early-warning reserve consumed block count overflows",
                    ))?;
            }
        }
        Ok(())
    }

    fn ensure_covers_outstanding_commitments(&self) -> Result<(), ParityError> {
        let remaining_projected_object_blocks = self
            .input
            .projected_object_blocks
            .checked_sub(self.object_blocks_written)
            .ok_or(ParityError::Invariant(
                "early-warning reserve object writes exceeded projection",
            ))?;
        let remaining_reserve_blocks = self
            .report
            .reserve_after_object_blocks
            .saturating_sub(self.reserve_blocks_consumed);
        let outstanding_commitment_blocks = remaining_projected_object_blocks
            .checked_add(remaining_reserve_blocks)
            .ok_or(ParityError::Invariant(
                "early-warning outstanding commitment count overflows",
            ))?;
        let consumed_blocks = self
            .object_blocks_written
            .checked_add(self.reserve_blocks_consumed)
            .ok_or(ParityError::Invariant(
                "early-warning reserve consumed block count overflows",
            ))?;
        let remaining_blocks = self
            .input
            .remaining_tape_blocks
            .checked_sub(consumed_blocks)
            .ok_or(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                projected_object_blocks: self.input.projected_object_blocks,
                remaining_blocks: Some(0),
                reserve_blocks: Some(remaining_reserve_blocks),
                remaining_spool_bytes: None,
                required_spool_bytes: None,
            })?;
        if remaining_blocks < outstanding_commitment_blocks {
            return Err(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                projected_object_blocks: self.input.projected_object_blocks,
                remaining_blocks: Some(remaining_blocks),
                reserve_blocks: Some(remaining_reserve_blocks),
                remaining_spool_bytes: None,
                required_spool_bytes: None,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingSidecar {
    epoch_id: u64,
    block_size: u32,
    protected_ordinal_start: u64,
    protected_ordinal_end_exclusive: u64,
    parity_shards: Vec<Vec<u8>>,
    data_shard_crc64s: Vec<u64>,
}

struct ParitySinkBackend<'a>(&'a mut dyn RawTapeSink);

impl ParitySinkBackend<'_> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let bytes_written = u32::try_from(buf.len())
            .map_err(|_| invalid_input("RawTapeSink fixed-block write length exceeds u32"))?;
        match self
            .0
            .write_fixed_block(buf)
            .map_err(parity_error_to_tape_io)?
        {
            RawWriteOutcome::WroteBlock {
                position_after,
                early_warning,
                end_of_medium,
            } => Ok(WriteOutcome::from_device_position(
                bytes_written,
                early_warning,
                end_of_medium,
                physical_to_tape_position(position_after),
            )),
            RawWriteOutcome::WroteFilemark { .. } => Err(invalid_input(
                "RawTapeSink::write_fixed_block returned a filemark outcome",
            )),
        }
    }

    fn write_one_filemark(&mut self) -> Result<WriteFilemarksOutcome, TapeIoError> {
        match self.0.write_filemark().map_err(parity_error_to_tape_io)? {
            RawWriteOutcome::WroteFilemark {
                position_after,
                early_warning,
                end_of_medium,
            } => Ok(WriteFilemarksOutcome::from_device_position(
                early_warning,
                end_of_medium,
                physical_to_tape_position(position_after),
            )),
            RawWriteOutcome::WroteBlock { .. } => Err(invalid_input(
                "RawTapeSink::write_filemark returned a block outcome",
            )),
        }
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(physical_to_tape_position(
            self.0.position().map_err(parity_error_to_tape_io)?,
        ))
    }
}

fn invalid_input(message: &'static str) -> TapeIoError {
    TapeIoError::CheckCondition(ScsiError::InvalidInput(message))
}

/// Convert raw-adapter parity errors back into the body-facing tape-I/O shape.
///
/// RawTapeSink implementations must return `ParityError::TapeIo` with
/// `TapeIoError::Transport` for completion-unknown SG_IO / driver failures.
/// String-only wrapper variants are preserved as diagnostics but cannot carry
/// the Layer 3a dirty-bit signal.
fn parity_error_to_tape_io(err: ParityError) -> TapeIoError {
    match err {
        ParityError::TapeIo(err) => err,
        ParityError::Invariant(message) => invalid_input(message),
        other => TapeIoError::OperationFailed(format!("RawTapeSink operation failed: {other}")),
    }
}

fn physical_to_tape_position(position: PhysicalPositionHint) -> TapePosition {
    TapePosition {
        lba: position.lba,
        partition: position.partition,
        beginning_of_partition: position.lba == 0,
        end_of_partition: false,
        block_position_end_of_warning: false,
    }
}

fn expected_resume_stripe_rows(data_blocks: u64, stripes: usize, stripe_index: usize) -> usize {
    let stripes = stripes as u64;
    let stripe_index = stripe_index as u64;
    if data_blocks <= stripe_index {
        0
    } else {
        ((data_blocks - 1 - stripe_index) / stripes + 1) as usize
    }
}

fn new_epoch_parity_accumulators(
    codec: &ReedSolomonCodec,
    stripes: usize,
    block_size: usize,
) -> Vec<Vec<Vec<u8>>> {
    (0..stripes)
        .map(|_| codec.new_parity_accumulators(block_size))
        .collect()
}

fn sidecar_summary_to_directory_entry(sidecar: &SidecarWriteSummary) -> SidecarEpochDirectoryEntry {
    let mut flags =
        SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD;
    if sidecar.final_partial_epoch {
        flags |= SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH;
    }
    SidecarEpochDirectoryEntry {
        tape_file_number: sidecar.tape_file_number,
        epoch_id: sidecar.epoch_id,
        protected_ordinal_start: sidecar.protected_ordinal_start,
        protected_ordinal_end_exclusive: sidecar.protected_ordinal_end_exclusive,
        sidecar_total_block_count: sidecar.block_count,
        sidecar_header_block_count: sidecar.sidecar_header_block_count,
        parity_shard_block_count: sidecar.parity_shard_block_count,
        canonical_metadata_hash: sidecar.canonical_metadata_hash,
        flags,
    }
}

/// Writer-visible metadata for one parity sidecar tape file emitted by the
/// sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarWriteSummary {
    /// Tape-file number assigned to the sidecar in the filemark map.
    pub tape_file_number: u32,
    /// Parity epoch protected by this sidecar.
    pub epoch_id: u64,
    /// Count of fixed-size records in the sidecar tape file, excluding the
    /// trailing filemark.
    pub block_count: u64,
    /// First protected object-data ordinal in the half-open sidecar range.
    pub protected_ordinal_start: u64,
    /// End-exclusive protected object-data ordinal in the sidecar range.
    pub protected_ordinal_end_exclusive: u64,
    /// Blocks in one replicated sidecar header/index copy.
    pub sidecar_header_block_count: u32,
    /// Raw parity-shard block count in the sidecar body.
    pub parity_shard_block_count: u32,
    /// Canonical metadata hash shared by the primary and tail sidecar copies.
    pub canonical_metadata_hash: [u8; 32],
    /// True when this sidecar protects a final partial epoch.
    pub final_partial_epoch: bool,
    /// Outcome of the sidecar's synchronous trailing filemark write.
    pub filemark_outcome: WriteFilemarksOutcome,
}

/// Result of closing the currently active object tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectWriteSummary {
    /// Tape-file number assigned by
    /// [`ParitySink::begin_object_with_capacity_reserve`].
    pub tape_file_number: u32,
    /// First `ParityDataOrdinal` assigned to this object tape file.
    pub first_parity_data_ordinal: u64,
    /// Conservative block-count bound supplied at object start.
    pub projected_size_blocks: u64,
    /// Actual object blocks written through the body-facing data path.
    pub data_block_count: u64,
    /// Outcome of the object's synchronous trailing filemark write.
    pub filemark_outcome: WriteFilemarksOutcome,
    /// Completed-epoch sidecars emitted immediately after the object filemark.
    pub sidecars_emitted: Vec<SidecarWriteSummary>,
    /// Highest protected object-data ordinal after any sidecars emitted at
    /// this object boundary.
    pub highest_protected_ordinal: u64,
    /// Bootstrap/parity_map control files emitted by
    /// [`BootstrapPlacementPolicy`] at this object boundary and folded into
    /// the same atomic Object journal bundle.
    pub control_tape_files_emitted: Vec<TapeFileEntry>,
    /// Higher-layer bootstrap object row attached to this object close, if
    /// supplied before [`ParitySink::finish_object`].
    pub bootstrap_object_row: Option<BootstrapObjectRow>,
}

impl ObjectWriteSummary {
    /// Build the generic v0.7.2 journal bundle for this object close.
    pub fn committed_bundle(&self) -> Result<CommittedBundle, ParityError> {
        let total_committed_ordinals_after = self
            .first_parity_data_ordinal
            .checked_add(self.data_block_count)
            .ok_or(ParityError::Invariant(
                "object commit bundle total ordinal count overflows",
            ))?;
        let mut entries = Vec::with_capacity(1 + self.sidecars_emitted.len());
        let mut object_entry = TapeFileEntry::from_map_entry(TapeFileMapEntry::object(
            self.tape_file_number,
            self.data_block_count,
            self.first_parity_data_ordinal,
        ));
        object_entry.bootstrap_object_row = self.bootstrap_object_row.clone();
        entries.push(object_entry);
        entries.extend(
            self.sidecars_emitted
                .iter()
                .map(SidecarWriteSummary::tape_file_entry),
        );
        entries.extend(self.control_tape_files_emitted.clone());
        Ok(CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries,
            highest_protected_ordinal: self.highest_protected_ordinal,
            total_committed_ordinals: total_committed_ordinals_after,
        })
    }
}

impl SidecarWriteSummary {
    /// Build the generic journal row for this emitted sidecar.
    pub fn tape_file_entry(&self) -> TapeFileEntry {
        TapeFileEntry {
            tape_file_number: self.tape_file_number,
            kind: TapeFileKind::ParitySidecar,
            block_count: self.block_count,
            physical_start_hint: None,
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: Some(self.epoch_id),
            protected_ordinal_start: Some(self.protected_ordinal_start),
            protected_ordinal_end_exclusive: Some(self.protected_ordinal_end_exclusive),
            canonical_metadata_hash: Some(self.canonical_metadata_hash),
            bootstrap_object_row: None,
        }
    }
}

/// Design-facing alias for the object-close result.
pub type ObjectCloseResult = ObjectWriteSummary;

/// Design-facing alias for one emitted sidecar tape file summary.
pub type SidecarTapeFile = SidecarWriteSummary;

/// Result of writing a resumable clean-session checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointResult {
    /// Non-final bootstrap tape-file number written by the checkpoint.
    pub bootstrap_tape_file_number: u32,
    /// Number of committed tape files attested by the checkpoint bootstrap.
    pub tape_file_count: u32,
    /// Protection watermark after the checkpoint bundle.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals after the checkpoint bundle.
    pub total_committed_ordinals: u64,
}

/// Content-driven bootstrap cadence from Layer 3c §7.3.
///
/// The policy is opt-in on the current writer API so existing callers can
/// continue choosing explicit bootstrap placements while Layer 5 tunes the
/// akash-specific thresholds. Once configured, the sink evaluates it only at
/// object boundaries and folds any emitted control files into that object's
/// journal bundle.
#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapPlacementPolicy {
    /// Emit after this many object commit bundles since the last bootstrap.
    pub bundles_per_bootstrap: u32,
    /// Emit after this many newly protected ordinals since the last bootstrap.
    pub ordinals_per_bootstrap: u64,
    /// `(remaining_fraction, divisor)` pairs that tighten the two floors near
    /// end of medium. The largest divisor whose fraction has been crossed is
    /// applied.
    pub eom_taper: Vec<(f64, u32)>,
    /// Minimum physical LBA distance from the previous bootstrap start.
    pub min_physical_separation_blocks: u64,
}

impl BootstrapPlacementPolicy {
    /// Validate that the policy can be evaluated without divide-by-zero,
    /// non-finite thresholds, or a cadence that can never be reasoned about.
    pub fn validate(&self) -> Result<(), ParityError> {
        if self.bundles_per_bootstrap == 0 || self.ordinals_per_bootstrap == 0 {
            return Err(ParityError::Invariant(
                "bootstrap placement policy floors must be non-zero",
            ));
        }
        let mut previous_taper: Option<(f64, u32)> = None;
        for (remaining_fraction, divisor) in &self.eom_taper {
            if !remaining_fraction.is_finite()
                || *remaining_fraction <= 0.0
                || *remaining_fraction > 1.0
            {
                return Err(ParityError::Invariant(
                    "bootstrap placement EOM taper fraction must be in (0, 1]",
                ));
            }
            if *divisor == 0 {
                return Err(ParityError::Invariant(
                    "bootstrap placement EOM taper divisor must be non-zero",
                ));
            }
            if let Some((previous_fraction, previous_divisor)) = previous_taper {
                if *remaining_fraction >= previous_fraction || *divisor <= previous_divisor {
                    return Err(ParityError::Invariant(
                        "bootstrap placement EOM taper entries must be ordered by descending remaining_fraction with strictly increasing divisors",
                    ));
                }
            }
            previous_taper = Some((*remaining_fraction, *divisor));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BootstrapPlacementState {
    bundles_since_last_bootstrap: u64,
    protected_ordinals_since_last_bootstrap: u64,
    last_bootstrap_start_lba: Option<u64>,
    estimated_total_tape_blocks: Option<u64>,
}

impl BootstrapPlacementState {
    fn reset_counters(&mut self) {
        self.bundles_since_last_bootstrap = 0;
        self.protected_ordinals_since_last_bootstrap = 0;
    }
}

/// Catalog and live-epoch state needed to create a sidecar-only writer after
/// a successful §7.8 resume operation.
#[derive(Debug)]
pub struct ResumeWriterSeed<'a> {
    /// Layer 5 catalog prefix after any resume-generated sidecars in
    /// [`Self::resume_result`] have committed.
    pub committed_prefix: &'a FilemarkMap,
    /// Header-derived sidecar directory entries for every parity sidecar in
    /// [`Self::committed_prefix`]. These rows are not part of the canonical
    /// filemark-map projection, but bootstraps and parity_map files need them
    /// as the recovery root of trust after resume.
    pub committed_prefix_sidecar_directory_entries: Vec<SidecarEpochDirectoryEntry>,
    /// Bootstrap object rows from the authoritative committed prefix. These
    /// rows are likewise outside the canonical filemark-map digest but must be
    /// carried forward so post-resume bootstraps remain authoritative for
    /// objects written before resume.
    pub committed_prefix_object_rows: Vec<BootstrapObjectRow>,
    /// Result returned by the resume sidecar commit path.
    pub resume_result: &'a ResumeAppendResult,
    /// Rebuilt partial epoch to load into the writer. `Some` means the
    /// resume rebuild left a non-empty partial epoch; `None` means
    /// `resume_result.live_epoch_start == resume_result.next_data_ordinal`
    /// and that ordinal is epoch-aligned. Consumed by value so the shard
    /// buffers are moved into the sink rather than cloned.
    pub live_epoch: Option<ResumeLiveEpochState>,
    /// Next sink-owned bootstrap sequence number to assign. Must be at least
    /// the number of bootstrap tape files already present in
    /// [`Self::committed_prefix`] so resumed writes cannot reuse an existing
    /// bootstrap sequence.
    pub next_bootstrap_sequence: u32,
}

/// Wraps an inner tape sink and inserts parity blocks or sidecar tape files at
/// the configured intervals.
///
/// The body format (Layer 3b) writes object data via `write_block`; the
/// parity sink forwards each fixed block to the raw tape sink while updating
/// per-stripe parity accumulators. Completed epochs are emitted later as
/// sidecar tape files, never as inline parity blocks in the object stream.
/// See `docs/layer3c-design.md` §5-§7.
#[allow(missing_debug_implementations)]
pub struct ParitySink<'a> {
    backend: ParitySinkBackend<'a>,
    journal: Option<&'a mut dyn TapeFileJournal>,
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    codec: ReedSolomonCodec,
    /// Tape block size in bytes the sink was constructed with.
    /// Pinned at construction so bootstrap writes (which happen
    /// before any data write) know how big the buffer must be.
    block_size_bytes: u32,

    /// Current neighborhood index (0 at BOT). Incremented when
    /// the writer finishes emitting parity for a full
    /// neighborhood (step 11.8).
    neighborhood_idx: u64,

    /// Count of data blocks the writer has handed us in the
    /// current neighborhood. Drives the row-major interleave
    /// math: `stripe_index = n % S`, `row = n / S`. Resets to
    /// 0 at every neighborhood boundary.
    data_blocks_in_neighborhood: u64,

    /// Per-stripe parity accumulators for the current epoch.
    ///
    /// Shape: `S` stripes, each containing `m` fixed-size parity shards. The
    /// sidecar-only writer updates the relevant stripe's accumulators with
    /// [`ReedSolomonCodec::accumulate`] for every object-data block. Missing
    /// final partial-epoch data shards are implicit zeros, so no data shard
    /// buffers are needed for final sidecar emission.
    parity_accumulators: Vec<Vec<Vec<u8>>>,

    /// CRC-64/XZ values for real object-data shards in the current epoch,
    /// in `ParityDataOrdinal` order. Logical zero padding used by `finish()`
    /// for RS math is deliberately excluded.
    current_epoch_data_crc64s: Vec<u64>,

    /// Completed-epoch sidecars whose parity has been computed but whose tape
    /// files cannot be emitted until the active object is closed, or until
    /// `finish()` closes the final partial epoch.
    pending_sidecars: Vec<PendingSidecar>,

    /// Highest object-data ordinal protected by sidecars actually emitted on
    /// tape and committed into the in-memory filemark map.
    highest_protected_ordinal: u64,

    /// First-write block size verified against
    /// `block_size_bytes`; once a `write_block` has landed,
    /// subsequent writes with a different length are rejected
    /// (parity encoder requires uniform shards). `None` until
    /// the first write (because tests / no-data-yet sessions
    /// shouldn't need to pin a size by going through the sink).
    block_size: Option<usize>,

    /// True after a non-recoverable transport error — the
    /// sink refuses further writes until a fresh session.
    poisoned: bool,

    /// LBA immediately after the last object data write (i.e. the
    /// next free LBA from the data-stream perspective, before any
    /// later sidecar or bootstrap control tape files). Updated on
    /// every successful body-facing `write_block`.
    /// Resume construction initializes this to the catalog-derived
    /// append position; the first resumed `write_block` overwrites it
    /// with the usual post-data-write position.
    ///
    /// Used by [`Self::finish`] to report `data_area_end_lba`
    /// truthfully when a neighborhood closes exactly (codex
    /// idref=30bf15c0 Medium): on an exact boundary the inner
    /// physical position is past the parity tail, but the
    /// logical end of user data is here.
    last_data_lba: u64,

    /// Active object bracketed by `begin_object`/`finish_object`.
    ///
    /// Only object blocks written while this is `Some` receive
    /// parity-data ordinals. Bootstrap and sidecar control tape files
    /// use internal bypass paths so non-object tape files never pollute
    /// the object-data epoch.
    active_object: Option<ActiveObject>,

    /// Object row supplied by the higher layer for the active object, if any.
    pending_bootstrap_object_row: Option<BootstrapObjectRow>,

    /// Structural map of tape files emitted by this sink.
    filemark_map: FilemarkMapBuilder,

    /// Sidecar-directory rows available for bootstrap/parity_map root of
    /// trust emission. The canonical filemark-map digest does not include
    /// these metadata fields; they are carried separately in bootstrap CBOR or
    /// a parity_map control file.
    sidecar_directory_entries: Vec<SidecarEpochDirectoryEntry>,

    /// RAO-binding object rows to include in subsequent checkpoint/final
    /// bootstrap payloads.
    bootstrap_object_rows: Vec<BootstrapObjectRow>,

    /// Catalog-visible commit-point state for object, sidecar, and bootstrap
    /// tape files.
    durable_boundary: DurableBoundaryState,

    /// Metadata hashes for newly emitted control tape files whose structural
    /// filemark-map rows do not carry the hash.
    control_metadata_hashes: BTreeMap<u32, [u8; 32]>,

    /// Runtime guard for EW-only outcomes. A pre-write capacity reserve admits
    /// the object; each successful raw operation consumes that model so every
    /// later EW signal can be handled by one "does the reserve still cover the
    /// not-yet-durable commitments?" predicate.
    early_warning_reserve: Option<EarlyWarningReserveState>,

    /// Next sink-owned bootstrap sequence number.
    next_bootstrap_sequence: u32,

    /// Next sink-owned parity_map sequence number.
    next_parity_map_sequence: u32,

    /// Optional §7.3 content-driven intermediate bootstrap policy.
    bootstrap_placement_policy: Option<BootstrapPlacementPolicy>,

    /// Runtime counters backing [`Self::bootstrap_placement_policy`].
    bootstrap_placement_state: BootstrapPlacementState,

    /// Physical cursor after the most recent successful raw operation. This
    /// avoids issuing extra POSITION probes solely for placement distance
    /// accounting.
    last_physical_lba: u64,
}

impl<'a> ParitySink<'a> {
    /// Construct a new sidecar-only parity sink wrapping `inner`.
    ///
    /// `block_size_bytes` is the tape's logical block size
    /// (from `DriveHandle::read_config()` or the format
    /// `WriteParams`). Pinned at construction because the
    /// bootstrap write (step 11.7) happens before any data
    /// write and needs to know how big the on-tape buffer is.
    /// Returns [`ParityError::InvalidScheme`] if the scheme
    /// fails validation.
    pub fn new(
        inner: &'a mut dyn RawTapeSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
    ) -> Result<Self, ParityError> {
        Self::new_with_backend(
            ParitySinkBackend(inner),
            None,
            scheme,
            tape_uuid,
            block_size_bytes,
        )
    }

    /// Construct a Layer 3c v0.7.2 parity sink with a durable journal.
    ///
    /// The journal is the write-side commit record. Object closes, standalone
    /// control bootstraps, and final close append `CommittedBundle` records
    /// only after their synchronous tape-file filemarks have completed.
    pub fn new_with_journal(
        inner: &'a mut dyn RawTapeSink,
        journal: &'a mut dyn TapeFileJournal,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
    ) -> Result<Self, ParityError> {
        if journal.tape_uuid() != tape_uuid {
            return Err(ParityError::SessionOpen(
                "journal tape UUID does not match parity sink tape UUID".into(),
            ));
        }
        Self::new_with_backend(
            ParitySinkBackend(inner),
            Some(journal),
            scheme,
            tape_uuid,
            block_size_bytes,
        )
    }

    /// Construct a Layer 3c v0.4.4 sidecar-only parity sink.
    ///
    /// Retained as a readable alias for [`Self::new`].
    pub fn new_sidecar_only(
        inner: &'a mut dyn RawTapeSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
    ) -> Result<Self, ParityError> {
        Self::new(inner, scheme, tape_uuid, block_size_bytes)
    }

    /// Construct a sidecar-only parity sink after a §7.8 resume operation.
    ///
    /// The seed must carry the Layer 5 catalog prefix after any
    /// resume-generated sidecars have committed. Its live epoch is consumed
    /// by value so the writer can continue accumulating the rebuilt open epoch
    /// without cloning its data shards.
    pub fn new_sidecar_only_from_resume(
        inner: &'a mut dyn RawTapeSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
        resume_seed: ResumeWriterSeed<'_>,
    ) -> Result<Self, ParityError> {
        let mut sink = Self::new_with_backend(
            ParitySinkBackend(inner),
            None,
            scheme,
            tape_uuid,
            block_size_bytes,
        )?;
        sink.validate_resume_prefix(
            resume_seed.committed_prefix,
            &resume_seed.committed_prefix_sidecar_directory_entries,
            &resume_seed.committed_prefix_object_rows,
            resume_seed.resume_result,
            resume_seed.next_bootstrap_sequence,
        )?;
        let expected_append_position = resume_seed
            .committed_prefix
            .append_position_after_prefix()
            .map_err(|err| match err {
                ParityError::FilemarkMapReconstruct(message) => ParityError::ResumeAppend(message),
                other => other,
            })?;
        let actual_tape_position = sink.backend.position().map_err(ParityError::TapeIo)?;
        let actual_append_position = PhysicalPositionHint {
            lba: actual_tape_position.lba,
            partition: actual_tape_position.partition,
        };
        if actual_append_position != expected_append_position {
            return Err(ParityError::ResumeAppend(format!(
                "raw sink is positioned at {:?}, expected append position {:?} after catalog-committed prefix",
                actual_append_position, expected_append_position
            )));
        }
        sink.filemark_map = FilemarkMapBuilder::from_committed_prefix(resume_seed.committed_prefix);
        sink.sidecar_directory_entries = resume_seed
            .committed_prefix_sidecar_directory_entries
            .clone();
        sink.bootstrap_object_rows = resume_seed.committed_prefix_object_rows.clone();
        sink.durable_boundary =
            DurableBoundaryState::from_committed_prefix(resume_seed.committed_prefix);
        sink.highest_protected_ordinal = resume_seed.resume_result.highest_protected_ordinal;
        sink.next_bootstrap_sequence = resume_seed.next_bootstrap_sequence;
        sink.next_parity_map_sequence = u32::try_from(
            resume_seed
                .committed_prefix
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::ParityMap)
                .count(),
        )
        .map_err(|_| ParityError::Invariant("resume parity_map sequence count exceeds u32"))?;
        sink.last_data_lba = actual_append_position.lba;
        sink.last_physical_lba = actual_append_position.lba;
        sink.load_resume_live_epoch(resume_seed.resume_result, resume_seed.live_epoch)?;
        Ok(sink)
    }

    /// Enable content-driven intermediate bootstrap placement for this
    /// session. Counters start from this call boundary; an already-written
    /// bootstrap, if any, still supplies the physical separation anchor.
    pub fn set_bootstrap_placement_policy(
        &mut self,
        policy: BootstrapPlacementPolicy,
    ) -> Result<(), ParityError> {
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "bootstrap placement policy cannot be changed while an object is active",
            ));
        }
        policy.validate()?;
        self.bootstrap_placement_policy = Some(policy);
        self.bootstrap_placement_state.reset_counters();
        Ok(())
    }

    /// Builder-style variant of [`Self::set_bootstrap_placement_policy`].
    pub fn with_bootstrap_placement_policy(
        mut self,
        policy: BootstrapPlacementPolicy,
    ) -> Result<Self, ParityError> {
        self.set_bootstrap_placement_policy(policy)?;
        Ok(self)
    }

    /// Disable automatic intermediate bootstraps for this session.
    pub fn clear_bootstrap_placement_policy(&mut self) -> Result<(), ParityError> {
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "bootstrap placement policy cannot be changed while an object is active",
            ));
        }
        self.bootstrap_placement_policy = None;
        self.bootstrap_placement_state.reset_counters();
        Ok(())
    }

    /// Current automatic bootstrap placement policy, if configured.
    pub fn bootstrap_placement_policy(&self) -> Option<&BootstrapPlacementPolicy> {
        self.bootstrap_placement_policy.as_ref()
    }

    fn new_with_backend(
        backend: ParitySinkBackend<'a>,
        journal: Option<&'a mut dyn TapeFileJournal>,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
    ) -> Result<Self, ParityError> {
        scheme.validate()?;
        if block_size_bytes == 0 {
            return Err(ParityError::InvalidScheme(
                "block_size_bytes = 0 — must be the tape's logical block size".into(),
            ));
        }
        let codec = ReedSolomonCodec::new(&scheme)?;
        let s = scheme.stripes_per_neighborhood as usize;
        let fixed_block_size = usize::try_from(block_size_bytes).map_err(|_| {
            ParityError::InvalidScheme("block_size_bytes does not fit usize".into())
        })?;
        let parity_accumulators = new_epoch_parity_accumulators(&codec, s, fixed_block_size);
        Ok(Self {
            backend,
            journal,
            scheme,
            tape_uuid,
            codec,
            block_size_bytes,
            neighborhood_idx: 0,
            data_blocks_in_neighborhood: 0,
            parity_accumulators,
            current_epoch_data_crc64s: Vec::new(),
            pending_sidecars: Vec::new(),
            highest_protected_ordinal: 0,
            block_size: None,
            poisoned: false,
            last_data_lba: 0,
            active_object: None,
            pending_bootstrap_object_row: None,
            filemark_map: FilemarkMapBuilder::new(),
            sidecar_directory_entries: Vec::new(),
            bootstrap_object_rows: Vec::new(),
            durable_boundary: DurableBoundaryState::new(),
            control_metadata_hashes: BTreeMap::new(),
            early_warning_reserve: None,
            next_bootstrap_sequence: 0,
            next_parity_map_sequence: 0,
            bootstrap_placement_policy: None,
            bootstrap_placement_state: BootstrapPlacementState::default(),
            last_physical_lba: 0,
        })
    }

    fn validate_resume_prefix(
        &self,
        committed_prefix: &FilemarkMap,
        committed_sidecar_directory_entries: &[SidecarEpochDirectoryEntry],
        committed_object_rows: &[BootstrapObjectRow],
        resume_result: &ResumeAppendResult,
        next_bootstrap_sequence: u32,
    ) -> Result<(), ParityError> {
        let sidecar_count = u32::try_from(resume_result.sidecars_emitted.len())
            .map_err(|_| ParityError::Invariant("resume sidecar count does not fit u32"))?;
        let committed_bootstrap_count = u32::try_from(
            committed_prefix
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::Bootstrap)
                .count(),
        )
        .map_err(|_| ParityError::Invariant("resume bootstrap count does not fit u32"))?;
        if next_bootstrap_sequence < committed_bootstrap_count {
            return Err(ParityError::Invariant(
                "resume bootstrap sequence precedes committed bootstrap count",
            ));
        }
        let expected_tape_files = resume_result
            .append_after_tape_file_number
            .checked_add(1)
            .and_then(|count| count.checked_add(sidecar_count))
            .ok_or(ParityError::Invariant(
                "resume committed prefix tape-file count overflows",
            ))?;
        if committed_prefix.tape_file_count() != expected_tape_files {
            return Err(ParityError::Invariant(
                "resume committed prefix does not include exactly the committed resume sidecars",
            ));
        }
        if committed_prefix.total_data_ordinals() != resume_result.next_data_ordinal {
            return Err(ParityError::Invariant(
                "resume committed prefix total ordinals do not match ResumeAppendResult",
            ));
        }
        if committed_prefix.max_sidecar_end_exclusive() != resume_result.highest_protected_ordinal {
            return Err(ParityError::Invariant(
                "resume committed prefix protection watermark does not match ResumeAppendResult",
            ));
        }
        self.validate_resume_sidecar_directory_entries(
            committed_prefix,
            committed_sidecar_directory_entries,
        )?;
        self.validate_resume_object_rows(committed_prefix, committed_object_rows)?;
        for (index, sidecar) in resume_result.sidecars_emitted.iter().enumerate() {
            let expected_tape_file_number = resume_result
                .append_after_tape_file_number
                .checked_add(1)
                .and_then(|value| value.checked_add(u32::try_from(index).ok()?))
                .ok_or(ParityError::Invariant(
                    "resume sidecar tape-file number overflows",
                ))?;
            if sidecar.tape_file_number != expected_tape_file_number {
                return Err(ParityError::Invariant(
                    "resume sidecar tape-file numbers are not contiguous after the append point",
                ));
            }
            let entry = committed_prefix
                .entries()
                .get(expected_tape_file_number as usize)
                .ok_or(ParityError::Invariant(
                    "resume committed prefix is missing a sidecar entry",
                ))?;
            if entry.kind != TapeFileKind::ParitySidecar
                || entry.block_count != sidecar.block_count
                || entry.epoch_id != Some(sidecar.epoch_id)
                || entry.protected_ordinal_start != Some(sidecar.protected_ordinal_start)
                || entry.protected_ordinal_end_exclusive
                    != Some(sidecar.protected_ordinal_end_exclusive)
            {
                return Err(ParityError::Invariant(
                    "resume committed prefix sidecar entry does not match ResumeAppendResult",
                ));
            }
        }
        Ok(())
    }

    fn validate_resume_object_rows(
        &self,
        committed_prefix: &FilemarkMap,
        committed_object_rows: &[BootstrapObjectRow],
    ) -> Result<(), ParityError> {
        validate_bootstrap_object_rows(committed_object_rows, Some(self.block_size_bytes))?;
        for row in committed_object_rows {
            let entry = committed_prefix
                .entries()
                .iter()
                .find(|entry| entry.tape_file_number == row.tape_file_number)
                .ok_or(ParityError::Invariant(
                    "resume object row references a tape file outside the committed prefix",
                ))?;
            if entry.kind != TapeFileKind::Object || entry.block_count != row.stored_block_count {
                return Err(ParityError::Invariant(
                    "resume object row does not match committed prefix object entry",
                ));
            }
        }
        self.validate_bootstrap_object_rows_fit(committed_object_rows)?;
        Ok(())
    }

    fn validate_resume_sidecar_directory_entries(
        &self,
        committed_prefix: &FilemarkMap,
        committed_sidecar_directory_entries: &[SidecarEpochDirectoryEntry],
    ) -> Result<(), ParityError> {
        let directory = SidecarEpochDirectory {
            directory_scope_tape_file_count: committed_prefix.tape_file_count(),
            directory_scope_total_data_ordinals: committed_prefix.total_data_ordinals(),
            directory_scope_highest_protected_ordinal: committed_prefix.max_sidecar_end_exclusive(),
            is_final_directory: false,
            entries: committed_sidecar_directory_entries.to_vec(),
        };
        directory.validate()?;

        let prefix_sidecars: Vec<&TapeFileMapEntry> = committed_prefix
            .entries()
            .iter()
            .filter(|entry| entry.kind == TapeFileKind::ParitySidecar)
            .collect();
        if prefix_sidecars.len() != committed_sidecar_directory_entries.len() {
            return Err(ParityError::Invariant(
                "resume sidecar directory does not cover every committed prefix sidecar",
            ));
        }

        for (entry, directory_entry) in prefix_sidecars
            .iter()
            .zip(committed_sidecar_directory_entries.iter())
        {
            if directory_entry.tape_file_number != entry.tape_file_number
                || directory_entry.sidecar_total_block_count != entry.block_count
                || Some(directory_entry.epoch_id) != entry.epoch_id
                || Some(directory_entry.protected_ordinal_start) != entry.protected_ordinal_start
                || Some(directory_entry.protected_ordinal_end_exclusive)
                    != entry.protected_ordinal_end_exclusive
            {
                return Err(ParityError::Invariant(
                    "resume sidecar directory entry does not match committed prefix",
                ));
            }
        }

        Ok(())
    }

    fn load_resume_live_epoch(
        &mut self,
        resume_result: &ResumeAppendResult,
        live_epoch: Option<ResumeLiveEpochState>,
    ) -> Result<(), ParityError> {
        let epoch_data_shards = self.epoch_data_shards()?;
        let Some(live) = live_epoch else {
            if resume_result.live_epoch_start != resume_result.next_data_ordinal {
                return Err(ParityError::Invariant(
                    "resume result without live epoch has a non-empty live range",
                ));
            }
            if resume_result.next_data_ordinal % epoch_data_shards != 0 {
                return Err(ParityError::Invariant(
                    "resume result without live epoch is not aligned to an epoch boundary",
                ));
            }
            self.neighborhood_idx = resume_result.next_data_ordinal / epoch_data_shards;
            return Ok(());
        };

        let ResumeLiveEpochState {
            epoch_id,
            protected_ordinal_start,
            next_data_ordinal,
            data_blocks_in_epoch,
            stripe_buffers,
            data_shard_crc64s,
        } = live;

        if protected_ordinal_start != resume_result.live_epoch_start
            || next_data_ordinal != resume_result.next_data_ordinal
        {
            return Err(ParityError::Invariant(
                "resume live epoch range does not match ResumeAppendResult",
            ));
        }
        if protected_ordinal_start % epoch_data_shards != 0 {
            return Err(ParityError::Invariant(
                "resume live epoch does not start on an epoch boundary",
            ));
        }
        let expected_epoch_id = protected_ordinal_start / epoch_data_shards;
        if epoch_id != expected_epoch_id {
            return Err(ParityError::Invariant(
                "resume live epoch id does not match its ordinal range",
            ));
        }
        let data_blocks = next_data_ordinal
            .checked_sub(protected_ordinal_start)
            .ok_or(ParityError::Invariant(
                "resume live epoch next ordinal precedes start",
            ))?;
        if data_blocks == 0 || data_blocks >= epoch_data_shards {
            return Err(ParityError::Invariant(
                "resume live epoch must contain a partial epoch",
            ));
        }
        if data_blocks_in_epoch != data_blocks {
            return Err(ParityError::Invariant(
                "resume live epoch data count does not match its ordinal range",
            ));
        }
        let data_blocks_usize = usize::try_from(data_blocks).map_err(|_| {
            ParityError::Invariant("resume live epoch data count does not fit usize")
        })?;
        if data_shard_crc64s.len() != data_blocks_usize {
            return Err(ParityError::Invariant(
                "resume live epoch CRC count does not match its data count",
            ));
        }

        let block_size = usize::try_from(self.block_size_bytes)
            .map_err(|_| ParityError::Invariant("fixed block size does not fit usize"))?;
        let stripes = self.scheme.stripes_per_neighborhood as usize;
        if stripe_buffers.len() != stripes {
            return Err(ParityError::Invariant(
                "resume live epoch stripe count does not match scheme",
            ));
        }
        for (stripe_index, stripe) in stripe_buffers.iter().enumerate() {
            let expected_rows = expected_resume_stripe_rows(data_blocks, stripes, stripe_index);
            if stripe.len() != expected_rows {
                return Err(ParityError::Invariant(
                    "resume live epoch stripe rows do not match row-major fill",
                ));
            }
            if stripe.iter().any(|block| block.len() != block_size) {
                return Err(ParityError::Invariant(
                    "resume live epoch block length does not match fixed block size",
                ));
            }
        }
        for (ordinal_offset, expected_crc) in data_shard_crc64s.iter().enumerate() {
            let stripe_index = ordinal_offset % stripes;
            let row_index = ordinal_offset / stripes;
            let block = &stripe_buffers[stripe_index][row_index];
            if data_shard_crc64(block) != *expected_crc {
                return Err(ParityError::Invariant(
                    "resume live epoch CRC does not match its shard bytes",
                ));
            }
        }

        self.reset_parity_accumulators()?;
        for (stripe_index, stripe) in stripe_buffers.iter().enumerate() {
            for (row_index, block) in stripe.iter().enumerate() {
                self.codec.accumulate(
                    row_index,
                    block,
                    &mut self.parity_accumulators[stripe_index],
                )?;
            }
        }

        self.neighborhood_idx = epoch_id;
        self.data_blocks_in_neighborhood = data_blocks_in_epoch;
        self.block_size = Some(block_size);
        self.current_epoch_data_crc64s = data_shard_crc64s;
        Ok(())
    }

    fn epoch_data_shards(&self) -> Result<u64, ParityError> {
        u64::from(self.scheme.stripes_per_neighborhood)
            .checked_mul(u64::from(self.scheme.data_blocks_per_stripe))
            .ok_or(ParityError::Invariant("epoch data-shard count overflows"))
    }

    /// Evaluate the Layer 3c v0.4.4 §7.5 reserve and begin the object only
    /// if the reserve succeeds.
    ///
    /// The caller supplies the tape/spool policy inputs that are not owned by
    /// this legacy sink yet. The epoch fields that *are* owned by the sink
    /// (`block_size_bytes`, current fill, data shards, and parity shards) are
    /// cross-checked so a caller cannot accidentally reserve against a
    /// different scheme state.
    pub fn begin_object_with_capacity_reserve(
        &mut self,
        input: CapacityReserveInput,
    ) -> Result<(u32, CapacityReserveReport), ParityError> {
        self.begin_object_after_admission(input, None)
    }

    /// Evaluate the capacity reserve and prove a worst-case bootstrap object
    /// row for this representation will still fit before beginning the object.
    ///
    /// RAO integrations that will later call
    /// [`Self::record_bootstrap_object_row`] use this pre-write gate to meet
    /// the key-30 bootstrap admission rule. The exact row is still checked at
    /// record time after the object body has been produced.
    pub fn begin_object_with_capacity_reserve_and_bootstrap_object_row(
        &mut self,
        input: CapacityReserveInput,
        object_row_admission: BootstrapObjectRowAdmission,
    ) -> Result<(u32, CapacityReserveReport), ParityError> {
        self.begin_object_after_admission(input, Some(object_row_admission))
    }

    fn begin_object_after_admission(
        &mut self,
        input: CapacityReserveInput,
        object_row_admission: Option<BootstrapObjectRowAdmission>,
    ) -> Result<(u32, CapacityReserveReport), ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "ParitySink poisoned after prior error",
            ));
        }
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "begin_object called while another object is active",
            ));
        }
        self.validate_capacity_reserve_input(&input)?;
        let report = input.evaluate()?;
        if let Some(admission) = object_row_admission {
            self.validate_bootstrap_object_row_admission_fit(admission, input)?;
        }
        let tape_file_number = self.start_object_after_reserve(input, report)?;
        Ok((tape_file_number, report))
    }

    /// Close the active object tape file by writing its trailing filemark.
    ///
    /// v0.4.4 makes filemarks a Layer 3c responsibility. This method gives
    /// object-bracketed callers an explicit delimiter, emits sidecars for
    /// completed full epochs accumulated during the object, and returns the
    /// exact filemark outcomes for catalog-commit ordering. It does not flush
    /// a partial parity epoch; that final-tail sidecar is emitted by
    /// [`Self::finish`].
    pub fn finish_object(&mut self) -> Result<ObjectWriteSummary, ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "ParitySink poisoned after prior error",
            ));
        }
        let object = self.active_object.ok_or(ParityError::Invariant(
            "finish_object called without an active object",
        ))?;
        let bundle_start = object.tape_file_number;
        if object.written_blocks == 0 {
            return Err(ParityError::Invariant(
                "finish_object called before any object blocks were written",
            ));
        }
        let filemark_outcome = match self.backend.write_one_filemark() {
            Ok(outcome) => outcome,
            Err(err) => {
                if err.is_completion_unknown() {
                    self.poisoned = true;
                    let boundary_err = self.abandon_tape_file_boundary_or(
                        TapeFileKind::Object,
                        object.tape_file_number,
                        ParityError::TapeIo(err),
                    );
                    return Err(boundary_err);
                }
                return Err(ParityError::TapeIo(err));
            }
        };
        self.record_physical_position(filemark_outcome.position_after.lba);
        if filemark_outcome.end_of_medium {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::Object,
                object.tape_file_number,
                ParityError::Invariant(
                    "object trailing filemark reached end of medium before catalog commit",
                ),
            ));
        }
        if let Err(err) = self.record_success_and_check_early_warning_reserve(
            EarlyWarningReserveEvent::ObjectFilemark,
            filemark_outcome.early_warning,
            filemark_outcome.end_of_medium,
        ) {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::Object,
                object.tape_file_number,
                err,
            ));
        }
        let entry = match self.filemark_map.push_object(object.written_blocks) {
            Ok(entry) => entry,
            Err(err) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::Object,
                    object.tape_file_number,
                    err,
                ));
            }
        };
        if entry.tape_file_number != object.tape_file_number {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::Object,
                object.tape_file_number,
                ParityError::Invariant("object tape-file number changed before finish_object"),
            ));
        }
        if let Err(err) =
            self.commit_tape_file_boundary(TapeFileKind::Object, object.tape_file_number)
        {
            self.poisoned = true;
            return Err(err);
        }
        self.active_object = None;
        let bootstrap_object_row = self.pending_bootstrap_object_row.take();
        if let Some(row) = bootstrap_object_row.as_ref() {
            self.bootstrap_object_rows.push(row.clone());
        }
        let first_parity_data_ordinal =
            entry
                .first_parity_data_ordinal
                .ok_or(ParityError::Invariant(
                    "object map entry missing first parity data ordinal",
                ))?;
        let previous_highest_protected_ordinal = self.highest_protected_ordinal;
        let sidecars_emitted = self.emit_pending_sidecars()?;
        if let Err(err) = self
            .validate_v1_post_object_bundle_bound(first_parity_data_ordinal, object.written_blocks)
        {
            self.poisoned = true;
            return Err(err);
        }
        self.record_object_bundle_for_bootstrap_policy(previous_highest_protected_ordinal)?;
        let control_start = self.filemark_map.next_tape_file_number()?;
        self.emit_policy_bootstrap_if_due()?;
        let control_tape_files_emitted = self.control_entries_from(control_start)?;
        let summary = ObjectWriteSummary {
            tape_file_number: object.tape_file_number,
            first_parity_data_ordinal,
            projected_size_blocks: object.projected_size_blocks,
            data_block_count: object.written_blocks,
            filemark_outcome,
            sidecars_emitted,
            highest_protected_ordinal: self.highest_protected_ordinal,
            control_tape_files_emitted,
            bootstrap_object_row,
        };
        if let Err(err) = self.commit_journal_map_range(
            CommittedBundleKind::Object,
            bundle_start,
            &summary.sidecars_emitted,
        ) {
            self.poisoned = true;
            return Err(err);
        }
        Ok(summary)
    }

    /// Tape-file number for the active object, if any.
    pub fn active_object_tape_file_number(&self) -> Option<u32> {
        self.active_object.map(|object| object.tape_file_number)
    }

    /// Object blocks written since the current `begin_object`.
    pub fn active_object_blocks_written(&self) -> Option<u64> {
        self.active_object.map(|object| object.written_blocks)
    }

    /// Attach the higher-layer bootstrap row for the active object.
    ///
    /// Layer 3c never parses object bytes, so RAO-specific manifest/envelope
    /// anchors must be supplied by the body-format layer after it has written
    /// the object bytes and before [`Self::finish_object`] emits any
    /// object-boundary checkpoint bootstrap.
    pub fn record_bootstrap_object_row(
        &mut self,
        row: BootstrapObjectRow,
    ) -> Result<(), ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "ParitySink poisoned after prior error",
            ));
        }
        let Some(object) = self.active_object else {
            return Err(ParityError::Invariant(
                "record_bootstrap_object_row called with no active object",
            ));
        };
        if row.tape_file_number != object.tape_file_number {
            return Err(ParityError::Invariant(
                "bootstrap object row tape-file number does not match active object",
            ));
        }
        if row.stored_block_count != object.written_blocks {
            return Err(ParityError::Invariant(
                "bootstrap object row block count does not match active object",
            ));
        }
        validate_bootstrap_object_row(&row, Some(self.block_size_bytes))?;
        if self.pending_bootstrap_object_row.is_some() {
            return Err(ParityError::Invariant(
                "bootstrap object row already recorded for active object",
            ));
        }
        let mut candidate_rows = self.bootstrap_object_rows.clone();
        candidate_rows.push(row.clone());
        self.validate_bootstrap_object_rows_fit(&candidate_rows)?;
        self.pending_bootstrap_object_row = Some(row);
        Ok(())
    }

    fn validate_bootstrap_object_rows_fit(
        &self,
        object_rows: &[BootstrapObjectRow],
    ) -> Result<(), ParityError> {
        let payload = BootstrapPayload {
            scheme: Some(ParitySchemeRecord {
                id: self.scheme.id.as_str().to_string(),
                data_blocks_per_stripe: self.scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: self.scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: self.scheme.stripes_per_neighborhood,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(FilemarkMapDigest {
                map_sha256: [0; 32],
                tape_file_count: u32::MAX,
                map_total_data_ordinals: u64::MAX,
                highest_protected_ordinal: u64::MAX,
                is_final_map: true,
            }),
            tape_uuid: self.tape_uuid,
            written_by_version: env!("CARGO_PKG_VERSION").to_string(),
            written_at: String::new(),
            sequence: u32::MAX,
            block_size_bytes: self.block_size_bytes,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: Some(worst_case_parity_map_reference()),
            object_rows: object_rows.to_vec(),
        };
        let mut block = vec![0u8; self.block_size_bytes as usize];
        write_bootstrap_block(&payload, &mut block).map(|_| ())
    }

    fn validate_bootstrap_object_row_admission_fit(
        &self,
        admission: BootstrapObjectRowAdmission,
        input: CapacityReserveInput,
    ) -> Result<(), ParityError> {
        let tape_file_number = self.filemark_map.next_tape_file_number()?;
        let row = worst_case_bootstrap_object_row(
            admission,
            tape_file_number,
            input.projected_object_blocks,
            self.block_size_bytes,
        )?;
        let mut candidate_rows = self.bootstrap_object_rows.clone();
        candidate_rows.push(row);
        self.validate_bootstrap_object_rows_fit(&candidate_rows)
    }

    /// Emit a non-final bootstrap tape file at the current position.
    ///
    /// Per Layer 3c v0.4.4 §7.3.1, the sink owns bootstrap sequence
    /// assignment and filemark-map digest construction. The bootstrap block
    /// bypasses the parity data path, is written directly to the inner sink,
    /// and is terminated by one filemark.
    pub fn write_bootstrap(&mut self) -> Result<u32, ParityError> {
        let bundle_start = self.filemark_map.next_tape_file_number()?;
        let tape_file_number = self.write_bootstrap_with_finality(false)?;
        if let Err(err) =
            self.commit_journal_map_range(CommittedBundleKind::Control, bundle_start, &[])
        {
            self.poisoned = true;
            return Err(err);
        }
        self.bootstrap_placement_state.reset_counters();
        Ok(tape_file_number)
    }

    /// Write a non-final bootstrap and journal it as a control checkpoint.
    ///
    /// This is the resumable clean-session boundary Layer 5 should call before
    /// unloading a tape that may later be appended to. It does not close a
    /// partial parity epoch; restart rebuilds that open epoch from the
    /// committed prefix.
    pub fn checkpoint(&mut self) -> Result<CheckpointResult, ParityError> {
        let bootstrap_tape_file_number = self.write_bootstrap()?;
        Ok(CheckpointResult {
            bootstrap_tape_file_number,
            tape_file_count: self.filemark_map.next_tape_file_number()?,
            highest_protected_ordinal: self.highest_protected_ordinal,
            total_committed_ordinals: self.filemark_map.total_data_ordinals()?,
        })
    }

    fn write_bootstrap_with_finality(&mut self, is_final_map: bool) -> Result<u32, ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "ParitySink poisoned after prior error",
            ));
        }
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "write_bootstrap called while an object is active",
            ));
        }
        let bootstrap_tape_file_number = self.filemark_map.next_tape_file_number()?;
        let bootstrap_entry = TapeFileMapEntry::bootstrap(bootstrap_tape_file_number, 1);
        let inline_digest = self
            .filemark_map
            .projected_digest(std::slice::from_ref(&bootstrap_entry), is_final_map)?;
        let inline_directory = self.sidecar_directory_for_digest(&inline_digest)?;
        let bootstrap_sequence = self.peek_bootstrap_sequence()?;
        let inline_payload = self.bootstrap_payload(
            inline_digest.clone(),
            bootstrap_sequence,
            Some(inline_directory),
            None,
        );
        if self.inline_directory_payload_fits(&inline_payload)? {
            return self.write_prepared_bootstrap(
                bootstrap_tape_file_number,
                inline_payload,
                is_final_map,
            );
        }

        let parity_map_tape_file_number = bootstrap_tape_file_number;
        let bootstrap_tape_file_number =
            parity_map_tape_file_number
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "parity_map bootstrap tape-file number overflows",
                ))?;
        let parity_map_sequence = self.peek_parity_map_sequence()?;
        let projected_directory = self.sidecar_directory_for_scope(
            bootstrap_tape_file_number
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "directory scope tape-file count overflows",
                ))?,
            inline_digest.map_total_data_ordinals,
            inline_digest.highest_protected_ordinal,
            is_final_map,
        )?;
        let provisional_payload = ParityMapPayload {
            tape_uuid: self.tape_uuid,
            sequence: parity_map_sequence,
            directory: projected_directory,
            canonical_map_digest: [0u8; 32],
            writer_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            write_timestamp: None,
        };
        let provisional_parity_map =
            encode_parity_map_tape_file(&provisional_payload, self.block_size_bytes)?;
        let parity_map_entry = TapeFileMapEntry::parity_map(
            parity_map_tape_file_number,
            provisional_parity_map.blocks.len() as u64,
        );
        let bootstrap_entry = TapeFileMapEntry::bootstrap(bootstrap_tape_file_number, 1);
        let external_digest = self.filemark_map.projected_digest(
            &[parity_map_entry.clone(), bootstrap_entry.clone()],
            is_final_map,
        )?;
        let external_directory = self.sidecar_directory_for_digest(&external_digest)?;
        let parity_map_payload = ParityMapPayload {
            tape_uuid: self.tape_uuid,
            sequence: parity_map_sequence,
            directory: external_directory,
            canonical_map_digest: external_digest.map_sha256,
            writer_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            write_timestamp: None,
        };
        let encoded_parity_map =
            encode_parity_map_tape_file(&parity_map_payload, self.block_size_bytes)?;
        if encoded_parity_map.blocks.len() as u64 != parity_map_entry.block_count {
            return Err(ParityError::Invariant(
                "parity_map block count changed after digest finalization",
            ));
        }
        self.write_prepared_parity_map(
            parity_map_tape_file_number,
            parity_map_entry.block_count,
            encoded_parity_map.header.payload_sha256,
            &encoded_parity_map.blocks,
        )?;
        self.next_parity_map_sequence = parity_map_sequence
            .checked_add(1)
            .ok_or(ParityError::Invariant("parity_map sequence overflow"))?;

        let reference = ParityMapReference {
            tape_file_number: parity_map_tape_file_number,
            block_count: parity_map_entry.block_count,
            directory_scope_tape_file_count: external_digest.tape_file_count,
            directory_scope_total_data_ordinals: external_digest.map_total_data_ordinals,
            directory_scope_highest_protected_ordinal: external_digest.highest_protected_ordinal,
            is_final_directory: external_digest.is_final_map,
            parity_map_payload_sha256: encoded_parity_map.header.payload_sha256,
            canonical_map_digest: external_digest.map_sha256,
        };
        reference.validate()?;
        let bootstrap_payload =
            self.bootstrap_payload(external_digest, bootstrap_sequence, None, Some(reference));
        self.write_prepared_bootstrap(bootstrap_tape_file_number, bootstrap_payload, is_final_map)
    }

    fn peek_bootstrap_sequence(&self) -> Result<u32, ParityError> {
        self.next_bootstrap_sequence
            .checked_add(1)
            .ok_or(ParityError::Invariant("bootstrap sequence overflow"))?;
        Ok(self.next_bootstrap_sequence)
    }

    fn peek_parity_map_sequence(&self) -> Result<u32, ParityError> {
        self.next_parity_map_sequence
            .checked_add(1)
            .ok_or(ParityError::Invariant("parity_map sequence overflow"))?;
        Ok(self.next_parity_map_sequence)
    }

    fn bootstrap_payload(
        &self,
        digest: FilemarkMapDigest,
        sequence: u32,
        sidecar_epoch_directory: Option<SidecarEpochDirectory>,
        parity_map_reference: Option<ParityMapReference>,
    ) -> BootstrapPayload {
        BootstrapPayload {
            scheme: Some(ParitySchemeRecord {
                id: self.scheme.id.as_str().to_string(),
                data_blocks_per_stripe: self.scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: self.scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: self.scheme.stripes_per_neighborhood,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(digest),
            tape_uuid: self.tape_uuid,
            written_by_version: env!("CARGO_PKG_VERSION").to_string(),
            written_at: String::new(),
            sequence,
            block_size_bytes: self.block_size_bytes,
            drive_compression: false,
            sidecar_epoch_directory,
            parity_map_reference,
            object_rows: self.bootstrap_object_rows.clone(),
        }
    }

    fn sidecar_directory_for_digest(
        &self,
        digest: &FilemarkMapDigest,
    ) -> Result<SidecarEpochDirectory, ParityError> {
        self.sidecar_directory_for_scope(
            digest.tape_file_count,
            digest.map_total_data_ordinals,
            digest.highest_protected_ordinal,
            digest.is_final_map,
        )
    }

    fn sidecar_directory_for_scope(
        &self,
        directory_scope_tape_file_count: u32,
        directory_scope_total_data_ordinals: u64,
        directory_scope_highest_protected_ordinal: u64,
        is_final_directory: bool,
    ) -> Result<SidecarEpochDirectory, ParityError> {
        let entries = self
            .sidecar_directory_entries
            .iter()
            .filter(|entry| entry.tape_file_number < directory_scope_tape_file_count)
            .cloned()
            .collect();
        let directory = SidecarEpochDirectory {
            directory_scope_tape_file_count,
            directory_scope_total_data_ordinals,
            directory_scope_highest_protected_ordinal,
            is_final_directory,
            entries,
        };
        directory.validate()?;
        Ok(directory)
    }

    fn inline_directory_payload_fits(
        &self,
        payload: &BootstrapPayload,
    ) -> Result<bool, ParityError> {
        let Some(directory) = payload.sidecar_epoch_directory.as_ref() else {
            return Ok(false);
        };
        let block_size = usize::try_from(self.block_size_bytes)
            .map_err(|_| ParityError::Invariant("bootstrap block size does not fit usize"))?;
        let base_payload = self.bootstrap_payload(
            payload
                .filemark_map_digest
                .clone()
                .ok_or(ParityError::Invariant("bootstrap payload missing digest"))?,
            payload.sequence,
            None,
            None,
        );
        let mut base_buf = vec![0u8; block_size];
        let base_framed_len = write_bootstrap_block(&base_payload, &mut base_buf)?;
        // Production-sized bootstrap blocks keep the addendum's slack for
        // future metadata growth; tiny test blocks fall back to exact fit.
        let margin = if block_size >= INLINE_DIRECTORY_PRODUCTION_MARGIN_BYTES * 2 {
            INLINE_DIRECTORY_PRODUCTION_MARGIN_BYTES
        } else {
            0
        };
        let inline_limit = block_size
            .saturating_sub(base_framed_len)
            .saturating_sub(margin);
        if directory.encoded_len()? > inline_limit {
            return Ok(false);
        }

        let mut buf = vec![0u8; block_size];
        match write_bootstrap_block(payload, &mut buf) {
            Ok(_) => Ok(true),
            Err(ParityError::BootstrapPayloadTooLarge { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn write_prepared_bootstrap(
        &mut self,
        tape_file_number: u32,
        payload: BootstrapPayload,
        is_final_map: bool,
    ) -> Result<u32, ParityError> {
        let bootstrap_start_lba = self.last_physical_lba;
        self.durable_boundary
            .begin_tape_file(TapeFileKind::Bootstrap, tape_file_number)?;
        let mut buf = vec![0u8; self.block_size_bytes as usize];
        if let Err(err) = write_bootstrap_block(&payload, &mut buf) {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::Bootstrap,
                tape_file_number,
                err,
            ));
        }
        self.write_control_block_and_filemark(TapeFileKind::Bootstrap, tape_file_number, &buf)?;
        let entry = match self.filemark_map.push_bootstrap() {
            Ok(entry) => entry,
            Err(err) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::Bootstrap,
                    tape_file_number,
                    err,
                ));
            }
        };
        if entry.tape_file_number != tape_file_number {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::Bootstrap,
                tape_file_number,
                ParityError::Invariant("bootstrap tape-file number changed before commit"),
            ));
        }
        let actual_digest = self
            .filemark_map
            .projected_digest(&[], is_final_map)
            .and_then(|digest| {
                payload
                    .filemark_map_digest
                    .as_ref()
                    .filter(|expected| **expected == digest)
                    .ok_or(ParityError::Invariant(
                        "prepared bootstrap digest does not match committed map",
                    ))
                    .map(|_| digest)
            });
        if let Err(err) = actual_digest {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::Bootstrap,
                tape_file_number,
                err,
            ));
        }
        if let Err(err) = self.commit_tape_file_boundary(TapeFileKind::Bootstrap, tape_file_number)
        {
            self.poisoned = true;
            return Err(err);
        }
        self.next_bootstrap_sequence = payload
            .sequence
            .checked_add(1)
            .ok_or(ParityError::Invariant("bootstrap sequence overflow"))?;
        self.bootstrap_placement_state.last_bootstrap_start_lba = Some(bootstrap_start_lba);
        Ok(tape_file_number)
    }

    fn write_prepared_parity_map(
        &mut self,
        tape_file_number: u32,
        block_count: u64,
        canonical_metadata_hash: [u8; 32],
        blocks: &[Vec<u8>],
    ) -> Result<(), ParityError> {
        if blocks.len() as u64 != block_count {
            return Err(ParityError::Invariant(
                "prepared parity_map block count does not match encoded blocks",
            ));
        }
        self.durable_boundary
            .begin_tape_file(TapeFileKind::ParityMap, tape_file_number)?;
        for block in blocks {
            self.write_control_block(TapeFileKind::ParityMap, tape_file_number, block)?;
        }
        self.write_control_filemark(TapeFileKind::ParityMap, tape_file_number)?;
        let entry = match self.filemark_map.push_parity_map(block_count) {
            Ok(entry) => entry,
            Err(err) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParityMap,
                    tape_file_number,
                    err,
                ));
            }
        };
        if entry.tape_file_number != tape_file_number || entry.block_count != block_count {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::ParityMap,
                tape_file_number,
                ParityError::Invariant("parity_map tape-file map entry changed before commit"),
            ));
        }
        if let Err(err) = self.commit_tape_file_boundary(TapeFileKind::ParityMap, tape_file_number)
        {
            self.poisoned = true;
            return Err(err);
        }
        self.control_metadata_hashes
            .insert(tape_file_number, canonical_metadata_hash);
        Ok(())
    }

    fn write_control_block_and_filemark(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
        block: &[u8],
    ) -> Result<(), ParityError> {
        self.write_control_block(kind, tape_file_number, block)?;
        self.write_control_filemark(kind, tape_file_number)
    }

    fn write_control_block(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
        block: &[u8],
    ) -> Result<(), ParityError> {
        let event = match kind {
            TapeFileKind::Bootstrap => EarlyWarningReserveEvent::BootstrapBlock,
            TapeFileKind::ParityMap => EarlyWarningReserveEvent::BootstrapBlock,
            _ => {
                return Err(ParityError::Invariant(
                    "write_control_block called for non-control tape-file kind",
                ));
            }
        };
        let outcome = match self.backend.write_block(block) {
            Ok(outcome) => outcome,
            Err(e) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    kind,
                    tape_file_number,
                    ParityError::TapeIo(e),
                ));
            }
        };
        self.record_physical_position(outcome.position_after.lba);
        if outcome.bytes_written as usize != block.len() {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                kind,
                tape_file_number,
                ParityError::Invariant("control block write completed short"),
            ));
        }
        if outcome.end_of_medium {
            self.poisoned = true;
            let message = match kind {
                TapeFileKind::Bootstrap => {
                    "bootstrap block write reached end of medium before trailing filemark"
                }
                TapeFileKind::ParityMap => {
                    "parity_map block write reached end of medium before trailing filemark"
                }
                _ => "control block write reached end of medium before trailing filemark",
            };
            return Err(self.abandon_tape_file_boundary_or(
                kind,
                tape_file_number,
                ParityError::Invariant(message),
            ));
        }
        if let Err(err) = self.record_success_and_check_early_warning_reserve(
            event,
            outcome.early_warning,
            outcome.end_of_medium,
        ) {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(kind, tape_file_number, err));
        }
        Ok(())
    }

    fn write_control_filemark(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
    ) -> Result<(), ParityError> {
        let event = match kind {
            TapeFileKind::Bootstrap => EarlyWarningReserveEvent::BootstrapFilemark,
            TapeFileKind::ParityMap => EarlyWarningReserveEvent::BootstrapFilemark,
            _ => {
                return Err(ParityError::Invariant(
                    "write_control_filemark called for non-control tape-file kind",
                ));
            }
        };
        let outcome = match self.backend.write_one_filemark() {
            Ok(outcome) => outcome,
            Err(e) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    kind,
                    tape_file_number,
                    ParityError::TapeIo(e),
                ));
            }
        };
        self.record_physical_position(outcome.position_after.lba);
        if outcome.end_of_medium {
            self.poisoned = true;
            let message = match kind {
                TapeFileKind::Bootstrap => {
                    "bootstrap trailing filemark reached end of medium before catalog commit"
                }
                TapeFileKind::ParityMap => {
                    "parity_map trailing filemark reached end of medium before catalog commit"
                }
                _ => "control trailing filemark reached end of medium before catalog commit",
            };
            return Err(self.abandon_tape_file_boundary_or(
                kind,
                tape_file_number,
                ParityError::Invariant(message),
            ));
        }
        if let Err(err) = self.record_success_and_check_early_warning_reserve(
            event,
            outcome.early_warning,
            outcome.end_of_medium,
        ) {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(kind, tape_file_number, err));
        }
        Ok(())
    }

    /// Flush any partial trailing neighborhood and return the
    /// geometry the caller (Layer 5) records in the catalog or
    /// final bootstrap.
    ///
    /// The active object must already be closed with [`Self::finish_object`].
    /// On success, `finish()` writes the final bootstrap tape file with
    /// `is_final_map = true` after any trailing padding/parity work.
    ///
    /// **Partial epoch strategy** (`docs/layer3c-design.md` §5.4):
    /// if `data_blocks_in_neighborhood` is between 0 and `S × k`,
    /// `finish()` computes parity over real data plus implicit zero shards and
    /// emits a final sidecar. It does not write zero padding blocks to tape.
    pub fn finish(mut self) -> Result<FinalGeometry, ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "ParitySink::finish on poisoned sink",
            ));
        }
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "ParitySink::finish called while an object is active; call finish_object first",
            ));
        }
        let blocks_in_neighborhood = self.data_blocks_in_neighborhood;
        // Per codex idref=30bf15c0 (Medium #1): `data_area_end_lba`
        // is the LBA where user data ENDED (= the next LBA after
        // the last user data block), NOT the post-parity physical
        // cursor. We tracked that during every successful
        // `write_block` data write as `self.last_data_lba`; use it.
        let data_area_end_lba = self.last_data_lba;

        let bundle_start = self.filemark_map.next_tape_file_number()?;
        let sidecars_emitted = if blocks_in_neighborhood == 0 {
            // Either we're at BOT or the previous neighborhood
            // closed exactly. No padding needed; trailing
            // parity has already been emitted (if any).
            self.emit_pending_sidecars()?
        } else {
            // We're inside a partial epoch. Layer 3c v0.4.4 computes parity
            // over implicit zero shards and emits a sidecar; it does not write
            // zero padding blocks to tape.
            let block_size = self.block_size.ok_or(ParityError::Invariant(
                "finish: data_blocks_in_neighborhood > 0 but block_size unpinned",
            ))?;
            self.queue_partial_sidecar_without_writing_padding(block_size)?;
            self.emit_pending_sidecars()?
        };
        self.write_bootstrap_with_finality(true)?;
        if let Err(err) = self.commit_journal_map_range(
            CommittedBundleKind::Finish,
            bundle_start,
            &sidecars_emitted,
        ) {
            self.poisoned = true;
            return Err(err);
        }
        Ok(FinalGeometry { data_area_end_lba })
    }

    fn commit_journal_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), ParityError> {
        if let Some(journal) = self.journal.as_mut() {
            journal.commit_bundle(bundle)?;
        }
        Ok(())
    }

    fn commit_journal_map_range(
        &mut self,
        kind: CommittedBundleKind,
        start_tape_file_number: u32,
        sidecars: &[SidecarWriteSummary],
    ) -> Result<(), ParityError> {
        if self.journal.is_none() {
            return Ok(());
        }
        let start = usize::try_from(start_tape_file_number).map_err(|_| {
            ParityError::Invariant("journal bundle start tape-file number does not fit usize")
        })?;
        let map_entries = self.filemark_map.entries();
        if start > map_entries.len() {
            return Err(ParityError::Invariant(
                "journal bundle start is beyond the current filemark map",
            ));
        }
        let entries = map_entries[start..]
            .iter()
            .map(|entry| {
                if let Some(sidecar_entry) = sidecars
                    .iter()
                    .find(|sidecar| sidecar.tape_file_number == entry.tape_file_number)
                    .map(SidecarWriteSummary::tape_file_entry)
                {
                    Ok(sidecar_entry)
                } else {
                    self.control_tape_file_entry(entry)
                }
            })
            .collect::<Result<Vec<_>, ParityError>>()?;
        let bundle = CommittedBundle {
            kind,
            entries,
            highest_protected_ordinal: self.highest_protected_ordinal,
            total_committed_ordinals: self.filemark_map.total_data_ordinals()?,
        };
        self.commit_journal_bundle(&bundle)
    }

    fn control_entries_from(
        &self,
        start_tape_file_number: u32,
    ) -> Result<Vec<TapeFileEntry>, ParityError> {
        let start = usize::try_from(start_tape_file_number).map_err(|_| {
            ParityError::Invariant("control start tape-file number does not fit usize")
        })?;
        let entries = self.filemark_map.entries();
        if start > entries.len() {
            return Err(ParityError::Invariant(
                "control start is beyond the current filemark map",
            ));
        }
        entries[start..]
            .iter()
            .map(|entry| self.control_tape_file_entry(entry))
            .collect()
    }

    fn control_tape_file_entry(
        &self,
        entry: &TapeFileMapEntry,
    ) -> Result<TapeFileEntry, ParityError> {
        let mut journal_entry = TapeFileEntry::from_map_entry(entry.clone());
        if entry.kind == TapeFileKind::ParityMap {
            journal_entry.canonical_metadata_hash = Some(
                self.control_metadata_hashes
                    .get(&entry.tape_file_number)
                    .copied()
                    .ok_or(ParityError::Invariant(
                        "parity_map journal row missing payload hash",
                    ))?,
            );
        }
        if entry.kind == TapeFileKind::Object {
            journal_entry.bootstrap_object_row = self
                .bootstrap_object_rows
                .iter()
                .find(|row| row.tape_file_number == entry.tape_file_number)
                .cloned();
        }
        Ok(journal_entry)
    }

    fn record_object_bundle_for_bootstrap_policy(
        &mut self,
        previous_highest_protected_ordinal: u64,
    ) -> Result<(), ParityError> {
        if self.bootstrap_placement_policy.is_none() {
            return Ok(());
        }
        self.bootstrap_placement_state.bundles_since_last_bootstrap = self
            .bootstrap_placement_state
            .bundles_since_last_bootstrap
            .checked_add(1)
            .ok_or(ParityError::Invariant(
                "bootstrap placement bundle counter overflows",
            ))?;
        let newly_protected = self
            .highest_protected_ordinal
            .checked_sub(previous_highest_protected_ordinal)
            .ok_or(ParityError::Invariant(
                "bootstrap placement protection watermark moved backward",
            ))?;
        self.bootstrap_placement_state
            .protected_ordinals_since_last_bootstrap = self
            .bootstrap_placement_state
            .protected_ordinals_since_last_bootstrap
            .checked_add(newly_protected)
            .ok_or(ParityError::Invariant(
                "bootstrap placement protected ordinal counter overflows",
            ))?;
        Ok(())
    }

    fn emit_policy_bootstrap_if_due(&mut self) -> Result<Option<u32>, ParityError> {
        let Some(policy) = self.bootstrap_placement_policy.clone() else {
            return Ok(None);
        };
        let (bundle_floor, ordinal_floor) = self.active_bootstrap_policy_floors(&policy);
        let floor_tripped = self.bootstrap_placement_state.bundles_since_last_bootstrap
            >= bundle_floor
            || self
                .bootstrap_placement_state
                .protected_ordinals_since_last_bootstrap
                >= ordinal_floor;
        if !floor_tripped || !self.bootstrap_min_separation_satisfied(&policy) {
            return Ok(None);
        }
        let tape_file_number = self.write_bootstrap_with_finality(false)?;
        self.bootstrap_placement_state.reset_counters();
        Ok(Some(tape_file_number))
    }

    fn active_bootstrap_policy_floors(&self, policy: &BootstrapPlacementPolicy) -> (u64, u64) {
        let divisor = self.bootstrap_policy_eom_divisor(policy);
        (
            u64::from(policy.bundles_per_bootstrap).div_ceil(divisor),
            policy.ordinals_per_bootstrap.div_ceil(divisor),
        )
    }

    fn bootstrap_policy_eom_divisor(&self, policy: &BootstrapPlacementPolicy) -> u64 {
        let Some(total) = self
            .bootstrap_placement_state
            .estimated_total_tape_blocks
            .filter(|total| *total > 0)
        else {
            return 1;
        };
        let remaining = total.saturating_sub(self.last_physical_lba);
        let remaining_fraction = remaining as f64 / total as f64;
        policy
            .eom_taper
            .iter()
            .filter(|(threshold, _)| remaining_fraction <= *threshold)
            .map(|(_, divisor)| u64::from(*divisor))
            .max()
            .unwrap_or(1)
            .max(1)
    }

    fn bootstrap_min_separation_satisfied(&self, policy: &BootstrapPlacementPolicy) -> bool {
        match self.bootstrap_placement_state.last_bootstrap_start_lba {
            None => true,
            Some(last_bootstrap_start_lba) => {
                self.last_physical_lba
                    .saturating_sub(last_bootstrap_start_lba)
                    >= policy.min_physical_separation_blocks
            }
        }
    }

    /// Read-only accessor for the parity scheme this sink uses.
    pub fn scheme(&self) -> &ParityScheme {
        &self.scheme
    }

    /// Current neighborhood index (= number of completed
    /// neighborhoods at the head). Useful for tests.
    pub fn neighborhood_idx(&self) -> u64 {
        self.neighborhood_idx
    }

    /// Data-blocks-written-so-far in the current neighborhood.
    /// Useful for tests.
    pub fn data_blocks_in_neighborhood(&self) -> u64 {
        self.data_blocks_in_neighborhood
    }

    fn reset_parity_accumulators(&mut self) -> Result<(), ParityError> {
        let fixed_block_size = usize::try_from(self.block_size_bytes)
            .map_err(|_| ParityError::Invariant("fixed block size does not fit usize"))?;
        let stripes = self.scheme.stripes_per_neighborhood as usize;
        self.parity_accumulators =
            new_epoch_parity_accumulators(&self.codec, stripes, fixed_block_size);
        Ok(())
    }

    fn advance_to_next_epoch(&mut self) -> Result<(), ParityError> {
        self.reset_parity_accumulators()?;
        self.neighborhood_idx += 1;
        self.data_blocks_in_neighborhood = 0;
        Ok(())
    }

    /// Internal: record one object-data block in epoch accounting.
    /// `stripe_index = n % S`, `row = n / S` per the row-major
    /// interleave (`docs/layer3c-design-v0.2.md` §5.2). Returns
    /// the (stripe_index, row) the block landed at.
    ///
    /// Object writes update parity accumulators and drop the shard; object data
    /// is not retained after the fixed block has been forwarded to raw tape.
    fn record_data_block(&mut self, buf: &[u8]) -> Result<(u32, u16), ParityError> {
        let s = self.scheme.stripes_per_neighborhood as u64;
        let k = self.scheme.data_blocks_per_stripe as u64;

        let position_in_neighborhood = self.data_blocks_in_neighborhood;
        if position_in_neighborhood >= s * k {
            // Should not happen: emit_parity_for_neighborhood
            // rolls over before we get here. Belt-and-braces
            // invariant — if it fires, the stripe accounting
            // got out of sync.
            return Err(ParityError::Invariant(
                "ParitySink: data row overrun (parity flush missed a boundary)",
            ));
        }
        let stripe_index = (position_in_neighborhood % s) as u32;
        let row = (position_in_neighborhood / s) as u16;
        let fixed_block_size = usize::try_from(self.block_size_bytes)
            .map_err(|_| ParityError::Invariant("fixed block size does not fit usize"))?;
        if buf.len() != fixed_block_size {
            return Err(ParityError::Invariant(
                "sidecar-only parity requires object writes to match the configured fixed block size",
            ));
        }
        let accumulators = self
            .parity_accumulators
            .get_mut(stripe_index as usize)
            .ok_or(ParityError::Invariant(
                "epoch parity accumulator stripe outside S",
            ))?;
        self.codec.accumulate(row as usize, buf, accumulators)?;
        self.current_epoch_data_crc64s.push(data_shard_crc64(buf));
        self.data_blocks_in_neighborhood += 1;
        Ok((stripe_index, row))
    }

    /// Internal: queue a sidecar for a completed epoch, then roll over to the
    /// next epoch.
    ///
    /// Sidecars are emitted only at object close or final finish, so the
    /// body-facing write outcome remains the raw data-block outcome.
    fn emit_parity_for_neighborhood(&mut self) -> Result<(), ParityError> {
        let Some(block_size) = self.block_size else {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "emit_parity called before any data write",
            ));
        };

        self.queue_epoch_sidecar_from_accumulators(block_size, false)?;
        self.advance_to_next_epoch()?;
        Ok(())
    }

    fn queue_partial_sidecar_without_writing_padding(
        &mut self,
        block_size: usize,
    ) -> Result<(), ParityError> {
        self.queue_epoch_sidecar_from_accumulators(block_size, true)?;
        self.advance_to_next_epoch()?;
        Ok(())
    }

    fn queue_epoch_sidecar_from_accumulators(
        &mut self,
        block_size: usize,
        allow_partial_epoch: bool,
    ) -> Result<(), ParityError> {
        let m = self.codec.parity_blocks();
        let mut parity_shards =
            Vec::with_capacity(self.parity_accumulators.len().checked_mul(m).ok_or(
                ParityError::Invariant("sidecar parity shard count overflows"),
            )?);
        for stripe in &self.parity_accumulators {
            if stripe.len() != m {
                self.poisoned = true;
                return Err(ParityError::Invariant(
                    "epoch parity accumulator count does not match m",
                ));
            }
            for shard in stripe {
                if shard.len() != block_size {
                    self.poisoned = true;
                    return Err(ParityError::Invariant(
                        "epoch parity accumulator block size mismatch",
                    ));
                }
            }
        }
        let epoch_accumulators = std::mem::take(&mut self.parity_accumulators);
        for stripe in epoch_accumulators {
            parity_shards.extend(stripe);
        }
        self.queue_epoch_sidecar_with_parity_shards(parity_shards, block_size, allow_partial_epoch)
    }

    fn queue_epoch_sidecar_with_parity_shards(
        &mut self,
        parity_shards: Vec<Vec<u8>>,
        block_size: usize,
        allow_partial_epoch: bool,
    ) -> Result<(), ParityError> {
        let s = self.scheme.stripes_per_neighborhood as usize;
        let k = self.codec.data_blocks();
        let m = self.codec.parity_blocks();
        let logical_data = s
            .checked_mul(k)
            .ok_or(ParityError::Invariant("sidecar real data count overflows"))?;
        let real_data = self.current_epoch_data_crc64s.len();
        if real_data == 0 {
            return Ok(());
        }
        if real_data > logical_data {
            self.poisoned = true;
            return Err(ParityError::Invariant("epoch data CRC count exceeds S*k"));
        }
        if !allow_partial_epoch && real_data != logical_data {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "completed epoch data CRC count does not match S*k",
            ));
        }

        let fixed_block_size = usize::try_from(self.block_size_bytes)
            .map_err(|_| ParityError::Invariant("fixed block size does not fit usize"))?;
        if block_size != fixed_block_size {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "sidecar-only parity requires object writes to match the configured fixed block size",
            ));
        }

        let expected_parity_shards = s.checked_mul(m).ok_or(ParityError::Invariant(
            "sidecar parity shard count overflows",
        ))?;
        if parity_shards.len() != expected_parity_shards {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "sidecar parity shard count does not match S*m",
            ));
        }
        if parity_shards.iter().any(|shard| shard.len() != block_size) {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "sidecar parity shard block size mismatch",
            ));
        }
        if let Some(object) = self.active_object {
            let pending_now = u64::try_from(self.pending_sidecars.len())
                .map_err(|_| ParityError::Invariant("pending sidecar count does not fit u64"))?;
            let queued_for_object = pending_now
                .checked_sub(object.pending_sidecars_at_start)
                .ok_or(ParityError::Invariant(
                    "pending sidecar count regressed during object write",
                ))?;
            if queued_for_object >= object.pending_sidecar_limit {
                self.poisoned = true;
                return Err(ParityError::Invariant(
                    "completed sidecar count exceeded object-start capacity reserve",
                ));
            }
        }

        let start = self
            .neighborhood_idx
            .checked_mul(logical_data as u64)
            .ok_or(ParityError::Invariant("sidecar protected range overflows"))?;
        let end = start
            .checked_add(real_data as u64)
            .ok_or(ParityError::Invariant("sidecar protected range overflows"))?;
        let data_shard_crc64s = std::mem::take(&mut self.current_epoch_data_crc64s);
        self.pending_sidecars.push(PendingSidecar {
            epoch_id: self.neighborhood_idx,
            block_size: self.block_size_bytes,
            protected_ordinal_start: start,
            protected_ordinal_end_exclusive: end,
            parity_shards,
            data_shard_crc64s,
        });
        Ok(())
    }

    fn emit_pending_sidecars(&mut self) -> Result<Vec<SidecarWriteSummary>, ParityError> {
        let pending = std::mem::take(&mut self.pending_sidecars);
        let mut emitted = Vec::with_capacity(pending.len());
        for sidecar in pending {
            match self.emit_one_sidecar(sidecar) {
                Ok(summary) => emitted.push(summary),
                Err(err) => {
                    self.poisoned = true;
                    return Err(err);
                }
            }
        }
        Ok(emitted)
    }

    fn emit_one_sidecar(
        &mut self,
        sidecar: PendingSidecar,
    ) -> Result<SidecarWriteSummary, ParityError> {
        let descriptor = SidecarDescriptor {
            tape_uuid: self.tape_uuid,
            epoch_id: sidecar.epoch_id,
            k: self.scheme.data_blocks_per_stripe,
            m: self.scheme.parity_blocks_per_stripe,
            stripes_per_epoch: self.scheme.stripes_per_neighborhood,
            block_size: sidecar.block_size,
            protected_ordinal_start: sidecar.protected_ordinal_start,
            protected_ordinal_end_exclusive: sidecar.protected_ordinal_end_exclusive,
        };
        let encoded = match encode_sidecar_tape_file(
            &descriptor,
            &sidecar.parity_shards,
            sidecar.data_shard_crc64s,
        ) {
            Ok(encoded) => encoded,
            Err(err) => {
                self.poisoned = true;
                return Err(err);
            }
        };
        let tape_file_number = self.filemark_map.next_tape_file_number()?;
        self.durable_boundary
            .begin_tape_file(TapeFileKind::ParitySidecar, tape_file_number)?;

        for block in &encoded.blocks {
            let outcome = match self.backend.write_block(block) {
                Ok(outcome) => outcome,
                Err(err) => {
                    self.poisoned = true;
                    return Err(self.abandon_tape_file_boundary_or(
                        TapeFileKind::ParitySidecar,
                        tape_file_number,
                        ParityError::TapeIo(err),
                    ));
                }
            };
            self.record_physical_position(outcome.position_after.lba);
            if outcome.bytes_written as usize != block.len() {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParitySidecar,
                    tape_file_number,
                    ParityError::Invariant("sidecar block write completed short"),
                ));
            }
            if outcome.end_of_medium {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParitySidecar,
                    tape_file_number,
                    ParityError::Invariant("sidecar block write reached end of medium"),
                ));
            }
            if let Err(err) = self.record_success_and_check_early_warning_reserve(
                EarlyWarningReserveEvent::SidecarBlock,
                outcome.early_warning,
                outcome.end_of_medium,
            ) {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParitySidecar,
                    tape_file_number,
                    err,
                ));
            }
        }

        let filemark_outcome = match self.backend.write_one_filemark() {
            Ok(outcome) => outcome,
            Err(err) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParitySidecar,
                    tape_file_number,
                    ParityError::TapeIo(err),
                ));
            }
        };
        self.record_physical_position(filemark_outcome.position_after.lba);
        if filemark_outcome.end_of_medium {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::ParitySidecar,
                tape_file_number,
                ParityError::Invariant(
                    "sidecar trailing filemark reached end of medium before catalog commit",
                ),
            ));
        }
        if let Err(err) = self.record_success_and_check_early_warning_reserve(
            EarlyWarningReserveEvent::SidecarFilemark,
            filemark_outcome.early_warning,
            filemark_outcome.end_of_medium,
        ) {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::ParitySidecar,
                tape_file_number,
                err,
            ));
        }
        let block_count = encoded.blocks.len() as u64;
        let entry = match self.filemark_map.push_parity_sidecar(
            block_count,
            sidecar.epoch_id,
            sidecar.protected_ordinal_start,
            sidecar.protected_ordinal_end_exclusive,
        ) {
            Ok(entry) => entry,
            Err(err) => {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParitySidecar,
                    tape_file_number,
                    err,
                ));
            }
        };
        if entry.tape_file_number != tape_file_number {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                TapeFileKind::ParitySidecar,
                tape_file_number,
                ParityError::Invariant("sidecar tape-file number changed before catalog commit"),
            ));
        }
        if let Err(err) =
            self.commit_tape_file_boundary(TapeFileKind::ParitySidecar, tape_file_number)
        {
            self.poisoned = true;
            return Err(err);
        }
        self.highest_protected_ordinal = self
            .highest_protected_ordinal
            .max(sidecar.protected_ordinal_end_exclusive);

        let summary = SidecarWriteSummary {
            tape_file_number: entry.tape_file_number,
            epoch_id: sidecar.epoch_id,
            block_count,
            protected_ordinal_start: sidecar.protected_ordinal_start,
            protected_ordinal_end_exclusive: sidecar.protected_ordinal_end_exclusive,
            sidecar_header_block_count: encoded.header.shard_index_block_count,
            parity_shard_block_count: encoded.header.parity_block_count,
            canonical_metadata_hash: encoded.header.canonical_metadata_hash,
            final_partial_epoch: encoded.header.real_data_shard_count
                < encoded.header.logical_shard_count,
            filemark_outcome,
        };
        self.sidecar_directory_entries
            .push(sidecar_summary_to_directory_entry(&summary));

        Ok(summary)
    }

    fn validate_capacity_reserve_input(
        &self,
        input: &CapacityReserveInput,
    ) -> Result<(), ParityError> {
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "capacity reserve requested while another object is active",
            ));
        }
        if input.block_size_bytes != self.block_size_bytes as u64 {
            return Err(ParityError::Invariant(
                "capacity reserve block size does not match ParitySink",
            ));
        }
        let data_shards_per_epoch =
            self.scheme.stripes_per_neighborhood as u64 * self.scheme.data_blocks_per_stripe as u64;
        if input.data_shards_per_epoch != data_shards_per_epoch {
            return Err(ParityError::Invariant(
                "capacity reserve data_shards_per_epoch does not match ParitySink",
            ));
        }
        let parity_shards_per_epoch = self.scheme.stripes_per_neighborhood as u64
            * self.scheme.parity_blocks_per_stripe as u64;
        if input.parity_shards_per_epoch != parity_shards_per_epoch {
            return Err(ParityError::Invariant(
                "capacity reserve parity_shards_per_epoch does not match ParitySink",
            ));
        }
        if input.current_epoch_fill_blocks != self.data_blocks_in_neighborhood {
            return Err(ParityError::Invariant(
                "capacity reserve current epoch fill does not match ParitySink",
            ));
        }
        Ok(())
    }

    fn start_object_after_reserve(
        &mut self,
        input: CapacityReserveInput,
        report: CapacityReserveReport,
    ) -> Result<u32, ParityError> {
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "begin_object called while another object is active",
            ));
        }
        if self.bootstrap_placement_policy.is_some() {
            self.bootstrap_placement_state.estimated_total_tape_blocks = Some(
                self.last_physical_lba
                    .checked_add(input.remaining_tape_blocks)
                    .ok_or(ParityError::Invariant(
                        "bootstrap placement capacity estimate overflows",
                    ))?,
            );
        }
        let tape_file_number = self.filemark_map.next_tape_file_number()?;
        self.durable_boundary
            .begin_tape_file(TapeFileKind::Object, tape_file_number)?;
        self.early_warning_reserve = Some(EarlyWarningReserveState::new(input, report));
        self.pending_bootstrap_object_row = None;
        self.active_object = Some(ActiveObject {
            tape_file_number,
            projected_size_blocks: input.projected_object_blocks,
            pending_sidecars_at_start: u64::try_from(self.pending_sidecars.len())
                .map_err(|_| ParityError::Invariant("pending sidecar count does not fit u64"))?,
            pending_sidecar_limit: report.epochs_completed_by_object,
            written_blocks: 0,
        });
        Ok(tape_file_number)
    }

    fn record_success_and_check_early_warning_reserve(
        &mut self,
        event: EarlyWarningReserveEvent,
        early_warning: bool,
        end_of_medium: bool,
    ) -> Result<(), ParityError> {
        if end_of_medium {
            return Ok(());
        }
        let Some(reserve) = self.early_warning_reserve.as_mut() else {
            return Ok(());
        };
        reserve.record_successful_event(event)?;
        if early_warning {
            reserve.ensure_covers_outstanding_commitments()?;
        }
        Ok(())
    }

    fn record_physical_position(&mut self, position_after_lba: u64) {
        self.last_physical_lba = position_after_lba;
    }

    fn validate_v1_post_object_bundle_bound(
        &self,
        first_parity_data_ordinal: u64,
        data_block_count: u64,
    ) -> Result<(), ParityError> {
        let total_committed_ordinals_after = first_parity_data_ordinal
            .checked_add(data_block_count)
            .ok_or(ParityError::Invariant(
                "object commit bundle total ordinal count overflows",
            ))?;
        if self.highest_protected_ordinal > total_committed_ordinals_after {
            return Err(ParityError::Invariant(
                "object commit bundle protection watermark exceeds committed ordinals",
            ));
        }
        let epoch_data_shards = self.epoch_data_shards()?;
        let unprotected_after_bundle = total_committed_ordinals_after
            .checked_sub(self.highest_protected_ordinal)
            .ok_or(ParityError::Invariant(
                "object commit bundle watermark exceeds committed ordinals",
            ))?;
        if unprotected_after_bundle >= epoch_data_shards {
            return Err(ParityError::Invariant(
                "object commit bundle violates v1 bounded restart invariant",
            ));
        }
        Ok(())
    }

    fn commit_tape_file_boundary(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
    ) -> Result<(), ParityError> {
        self.durable_boundary
            .commit_tape_file(kind, tape_file_number)
    }

    fn abandon_tape_file_boundary_or(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u32,
        err: ParityError,
    ) -> ParityError {
        match self
            .durable_boundary
            .abandon_tape_file(kind, tape_file_number)
        {
            Ok(_) => err,
            Err(boundary_err) => boundary_err,
        }
    }

    fn abandon_active_object_boundary(&mut self) -> Result<(), ParityError> {
        if let Some(object) = self.active_object {
            self.durable_boundary
                .abandon_tape_file(TapeFileKind::Object, object.tape_file_number)?;
        }
        Ok(())
    }

    fn abandon_active_object_boundary_or_tape_io(&mut self, err: TapeIoError) -> TapeIoError {
        match self.abandon_active_object_boundary() {
            Ok(()) => err,
            Err(boundary_err) => parity_error_to_tape_io(boundary_err),
        }
    }
}

impl<'a> BlockSink for ParitySink<'a> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if self.poisoned {
            return Err(TapeIoError::CheckCondition(ScsiError::InvalidInput(
                "ParitySink poisoned after prior transport error; abandon session",
            )));
        }
        let Some(object) = self.active_object else {
            return Err(TapeIoError::CheckCondition(ScsiError::InvalidInput(
                "ParitySink: write_block outside active object",
            )));
        };
        if object.written_blocks >= object.projected_size_blocks {
            return Err(TapeIoError::CheckCondition(ScsiError::InvalidInput(
                "ParitySink: object exceeded projected_size_blocks",
            )));
        }
        if buf.len() != self.block_size_bytes as usize {
            return Err(TapeIoError::CheckCondition(ScsiError::InvalidInput(
                "ParitySink: sidecar-only object blocks must match configured fixed block size",
            )));
        }
        // Pin the block size only after validating it against the configured
        // fixed tape block size, so one malformed first write cannot poison
        // the session's expected shard length.
        match self.block_size {
            None => self.block_size = Some(buf.len()),
            Some(expected) if expected != buf.len() => {
                return Err(TapeIoError::CheckCondition(ScsiError::InvalidInput(
                    "ParitySink: heterogeneous block sizes within a parity session",
                )));
            }
            Some(_) => {}
        }

        // Forward to the inner sink first. If the inner write
        // fails, we don't bump stripe accounting — the failed
        // LBA didn't actually consume a slot. Transport errors
        // poison the sink because resuming would mis-place
        // subsequent parity LBAs.
        let data_outcome = match self.backend.write_block(buf) {
            Ok(o) => o,
            Err(e) => {
                if e.is_completion_unknown() {
                    self.poisoned = true;
                    return Err(self.abandon_active_object_boundary_or_tape_io(e));
                }
                return Err(e);
            }
        };
        self.record_physical_position(data_outcome.position_after.lba);
        if (data_outcome.bytes_written as usize) < buf.len() {
            self.poisoned = true;
            return Err(
                self.abandon_active_object_boundary_or_tape_io(invalid_input(
                    "ParitySink: object data block write completed short before trailing filemark",
                )),
            );
        }
        if data_outcome.end_of_medium {
            self.poisoned = true;
            return Err(self.abandon_active_object_boundary_or_tape_io(invalid_input(
                "ParitySink: object data block write reached end of medium before trailing filemark",
            )));
        }
        // Track the logical end-of-user-data LBA. The inner
        // sink's reported position_after is the post-write
        // (next-free) LBA from the inner BlockSink's view, which
        // is the logical data-area end before any later sidecar
        // tape files are emitted at object close or finish.
        self.last_data_lba = data_outcome.position_after.lba;
        if let Some(object) = self.active_object.as_mut() {
            object.written_blocks += 1;
        }
        if let Err(err) = self.record_success_and_check_early_warning_reserve(
            EarlyWarningReserveEvent::ObjectDataBlock,
            data_outcome.early_warning,
            data_outcome.end_of_medium,
        ) {
            self.poisoned = true;
            return Err(
                self.abandon_active_object_boundary_or_tape_io(parity_error_to_tape_io(err))
            );
        }

        // Record the block for parity computation.
        if let Err(invariant) = self.record_data_block(buf) {
            self.poisoned = true;
            let _ = self.abandon_active_object_boundary();
            return Err(TapeIoError::CheckCondition(ScsiError::InvalidInput(
                match invariant {
                    ParityError::Invariant(s) => s,
                    _ => "ParitySink stripe accounting failure",
                },
            )));
        }

        // If we've just filled every data row in the neighborhood,
        // queue a parity sidecar for later emission at object close
        // or final finish. No physical parity blocks are written on
        // the body-facing write path.
        let s = self.scheme.stripes_per_neighborhood as u64;
        let k = self.scheme.data_blocks_per_stripe as u64;
        if self.data_blocks_in_neighborhood == s * k {
            match self.emit_parity_for_neighborhood() {
                Ok(()) => Ok(data_outcome),
                Err(parity_err) => {
                    self.poisoned = true;
                    let _ = self.abandon_active_object_boundary();
                    Err(match parity_err {
                        ParityError::TapeIo(inner) => inner,
                        ParityError::Invariant(msg) => {
                            TapeIoError::CheckCondition(ScsiError::InvalidInput(msg))
                        }
                        _ => TapeIoError::CheckCondition(ScsiError::InvalidInput(
                            "parity emission failed",
                        )),
                    })
                }
            }
        } else {
            Ok(data_outcome)
        }
    }

    fn write_filemarks(&mut self, _count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        Err(invalid_input(
            "ParitySink: body-facing write_filemarks is disabled; Layer 3c owns object, sidecar, and bootstrap filemarks",
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.backend.position()
    }
}

#[cfg(test)]
mod tests;
