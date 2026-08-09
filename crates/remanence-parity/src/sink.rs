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

use crate::bootstrap::{write_bootstrap_block, BootstrapPayload, ParitySchemeRecord};
use crate::capacity::{
    CapacityReserveCause, TerminalTripleCloseInput, TerminalTripleCloseReport,
    TerminalTripleObjectReservation,
};
use crate::codec::ReedSolomonCodec;
use crate::durable::DurableBoundaryState;
use crate::error::ParityError;
#[cfg(test)]
use crate::filemark_map::FilemarkMap;
use crate::filemark_map::{FilemarkMapBuilder, FilemarkMapDigest, TapeFileKind, TapeFileMapEntry};
use crate::journal::{
    CommittedBundle, CommittedBundleKind, FileTapeFileJournalCommittedSnapshot, TapeFileEntry,
    TapeFileJournal,
};
use crate::model::{FinalGeometry, ParityScheme};
use crate::parity_map::{
    encode_parity_map_tape_file, parse_parity_map_tape_file, ParityMapPayload,
    SidecarEpochDirectory, SidecarEpochDirectoryEntry, SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
    SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
};
use crate::raw::{
    PhysicalPositionHint, RawReadOutcome, RawTapeSink, RawTapeSource, RawWriteOutcome,
};
use crate::resume::{
    checked_bounded_resume_summary, streamed_filemark_map_digest, BoundedResumeSummary,
    ResumeAppendResult, ResumeLiveEpochState,
};
use crate::sidecar::{
    data_shard_crc64, encode_sidecar_tape_file, parse_sidecar_tape_file, SidecarDescriptor,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveObject {
    tape_file_number: u64,
    projected_size_blocks: u64,
    pending_sidecars_at_start: u64,
    pending_sidecar_limit: u64,
    written_blocks: u64,
    block_size_before_object: Option<usize>,
    early_warning_reserve_before_object: Option<EarlyWarningReserveState>,
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
    fn reserve_cost_blocks(self, input: TerminalTripleCloseInput) -> u64 {
        match self {
            Self::ObjectDataBlock => 0,
            Self::ObjectFilemark => filemark_reserve_cost_blocks(input.object_filemark_blocks),
            Self::SidecarBlock => 1,
            Self::SidecarFilemark => filemark_reserve_cost_blocks(input.sidecar_filemark_blocks),
            Self::BootstrapBlock => 1,
            Self::BootstrapFilemark => 1,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EarlyWarningReserveState {
    input: TerminalTripleCloseInput,
    report: TerminalTripleCloseReport,
    object_blocks_written: u64,
    reserve_blocks_consumed: u64,
}

impl EarlyWarningReserveState {
    fn new(input: TerminalTripleCloseInput, report: TerminalTripleCloseReport) -> Self {
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
        let total_reserve_blocks = self
            .report
            .required_tape_blocks
            .checked_sub(self.input.projected_object_blocks)
            .ok_or(ParityError::Invariant(
                "terminal reserve is smaller than its projected Object",
            ))?;
        let remaining_reserve_blocks = total_reserve_blocks
            .checked_sub(self.reserve_blocks_consumed)
            .ok_or(ParityError::Invariant(
                "early-warning reserve consumption exceeded the terminal reserve",
            ))?;
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
    is_terminal: bool,
}

fn terminal_triple_runtime_state_from_parts(
    current_epoch_fill_blocks: u64,
    pending_sidecars: &[PendingSidecar],
    sidecar_entries_before_object: usize,
    structural_entries_before_object: usize,
    object_rows_before_object: usize,
    used_tape_blocks: u64,
) -> Result<TerminalTripleCapacityRuntimeState, ParityError> {
    let pending_completed_sidecars = u64::try_from(pending_sidecars.len())
        .map_err(|_| ParityError::Invariant("pending sidecar count does not fit u64"))?;
    let pending_completed_epoch_parity_bytes =
        pending_sidecars.iter().try_fold(0u64, |total, sidecar| {
            sidecar
                .parity_shards
                .iter()
                .try_fold(total, |subtotal, shard| {
                    subtotal
                        .checked_add(u64::try_from(shard.len()).map_err(|_| {
                            ParityError::Invariant("pending parity shard length does not fit u64")
                        })?)
                        .ok_or(ParityError::Invariant(
                            "pending parity shard byte count overflows",
                        ))
                })
        })?;
    Ok(TerminalTripleCapacityRuntimeState {
        current_epoch_fill_blocks,
        pending_completed_sidecars,
        pending_completed_epoch_parity_bytes,
        sidecar_entries_before_object: u64::try_from(sidecar_entries_before_object).map_err(
            |_| ParityError::Invariant("sidecar directory entry count does not fit u64"),
        )?,
        structural_entries_before_object: u64::try_from(structural_entries_before_object)
            .map_err(|_| ParityError::Invariant("structural entry count does not fit u64"))?,
        object_rows_before_object: u64::try_from(object_rows_before_object)
            .map_err(|_| ParityError::Invariant("Object recovery-row count does not fit u64"))?,
        used_tape_blocks,
    })
}

/// Parity-owned state used to build an object-start capacity reservation.
///
/// This projection keeps callers from reconstructing private epoch and spool
/// invariants. `used_tape_blocks` is the last physical LBA proved by the raw
/// sink, so it includes Object/control bodies and filemarks rather than only
/// logical Object ordinals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalTripleCapacityRuntimeState {
    /// Object-data shards already accumulated in the open epoch.
    pub current_epoch_fill_blocks: u64,
    /// Completed sidecars still staged at this Object boundary.
    pub pending_completed_sidecars: u64,
    /// Parity-shard bytes held by those completed sidecars.
    pub pending_completed_epoch_parity_bytes: u64,
    /// Sidecar-directory rows already committed before the proposed Object.
    pub sidecar_entries_before_object: u64,
    /// Committed structural rows in the prefix before the proposed Object.
    pub structural_entries_before_object: u64,
    /// Committed Object recovery rows in exact bijection with Objects.
    pub object_rows_before_object: u64,
    /// Last physical LBA proved by the attached raw sink.
    pub used_tape_blocks: u64,
}

struct ParitySinkBackend<'a>(&'a mut dyn RawTapeSink);

impl ParitySinkBackend<'_> {
    fn locate_for_overwrite(&mut self, position: PhysicalPositionHint) -> Result<(), TapeIoError> {
        self.0
            .locate_for_overwrite(position)
            .map_err(parity_error_to_tape_io)
    }

    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        match self
            .0
            .write_fixed_block(buf)
            .map_err(parity_error_to_tape_io)?
        {
            RawWriteOutcome::WroteBlock {
                bytes_written,
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

    fn write_filemarks(
        &mut self,
        count: u32,
        immed: bool,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        match self
            .0
            .write_filemarks(count, immed)
            .map_err(parity_error_to_tape_io)?
        {
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
                "RawTapeSink::write_filemarks returned a block outcome",
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
    pub tape_file_number: u64,
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
    pub sidecar_header_block_count: u64,
    /// Raw parity-shard block count in the sidecar body.
    pub parity_shard_block_count: u64,
    /// Canonical metadata hash shared by the primary and tail sidecar copies.
    pub canonical_metadata_hash: [u8; 32],
    /// True when this sidecar protects a final partial epoch.
    pub final_partial_epoch: bool,
    /// Outcome of the sidecar's synchronous trailing filemark write.
    pub filemark_outcome: WriteFilemarksOutcome,
    /// Volume-global LBA of block 0 of this sidecar tape file. The sidecar
    /// occupies `[start, start + block_count)`, exclusive; its trailing
    /// filemark at `start + block_count` is outside the span. Dead-reckoned
    /// at file begin; the covering barrier's device-proved position
    /// transitively validates it.
    pub physical_start_lba: Option<u64>,
}

/// Result of closing the currently active object tape file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectWriteSummary {
    /// Tape-file number assigned by
    /// [`ParitySink::begin_object_with_terminal_triple_reservation`].
    pub tape_file_number: u64,
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
    /// Volume-global LBA of block 0 of this object tape file. The file
    /// occupies `[start, start + block_count)`, exclusive; the trailing
    /// filemark at `start + block_count` is outside the span; on a fresh
    /// tape the bootstrap prefix precedes this start and is excluded at
    /// capture. Dead-reckoned at file begin; the covering barrier's
    /// device-proved position transitively validates it.
    pub physical_start_lba: Option<u64>,
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
        object_entry.physical_start_hint = self.physical_start_lba;
        entries.push(object_entry);
        entries.extend(
            self.sidecars_emitted
                .iter()
                .map(SidecarWriteSummary::tape_file_entry),
        );
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
            physical_start_hint: self.physical_start_lba,
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: Some(self.epoch_id),
            protected_ordinal_start: Some(self.protected_ordinal_start),
            protected_ordinal_end_exclusive: Some(self.protected_ordinal_end_exclusive),
            canonical_metadata_hash: Some(self.canonical_metadata_hash),
            object_recovery_row: None,
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
    /// First tape-file number available after the checkpoint watermark.
    pub next_tape_file_number: u64,
    /// Protection watermark after the checkpoint bundle.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals after the checkpoint bundle.
    pub total_committed_ordinals: u64,
    /// Sidecars emitted by this stop, including a short open epoch if present.
    pub sidecars_emitted: Vec<SidecarWriteSummary>,
    /// Successful zero-count synchronous barrier outcome.
    pub barrier_outcome: WriteFilemarksOutcome,
    /// Physical blocks used through the proved post-barrier position.
    pub used_tape_blocks: u64,
    /// True for the terminal Finish close.
    pub is_terminal: bool,
    /// Sidecar-only control/finish bundle made durable by this stop, if the
    /// barrier emitted a partial-epoch sidecar.
    pub committed_bundle: Option<CommittedBundle>,
}

/// Barrier-proved final parity prefix immediately before replica A.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalPrefixCloseResult {
    /// Final partial sidecars emitted by this close, if any.
    pub sidecars_emitted: Vec<SidecarWriteSummary>,
    /// External final ParityMap emitted when sidecar metadata exists.
    pub parity_map_tape_file_number: Option<u64>,
    /// Successful zero-count synchronous barrier outcome.
    pub barrier_outcome: WriteFilemarksOutcome,
    /// Exact post-barrier physical cursor.
    pub used_tape_blocks: u64,
    /// Terminal-prefix journal bundle, containing no Bootstrap.
    pub committed_bundle: CommittedBundle,
}

/// Pure deterministic terminal-prefix plan persisted before close motion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalPrefixPlan {
    /// First tape-file number that terminal-prefix emission may use.
    pub start_tape_file_number: u64,
    /// First tape-file number reserved for replica A after the prefix.
    pub tail_start_tape_file_number: u64,
    /// Physical cursor before terminal-prefix emission.
    pub start_lba: u64,
    /// Exact physical cursor where replica A begins.
    pub tail_start_lba: u64,
    /// External final ParityMap tape-file number, when one is required.
    pub parity_map_tape_file_number: Option<u64>,
    /// Exact sidecar directory state after the planned prefix.
    pub sidecar_directory_entries: Vec<SidecarEpochDirectoryEntry>,
    /// Exact final-prefix rows and W/T values to persist as immutable intent.
    pub committed_bundle: CommittedBundle,
}

/// Physical classification of a persisted terminal-prefix plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalPrefixReconcileEvidence {
    /// No prefix bytes exist at the planned start.
    Absent,
    /// Every planned file body and trailing filemark fully validates.
    Complete,
    /// The prefix is partial or invalid and can be overwritten at its start.
    TornRewritable,
    /// The prefix is partial or invalid on WORM media.
    TornWorm,
    /// The planned start or physical state could not be proved.
    Unproved,
}

/// Bounded full-file reconciliation of a persisted terminal-prefix plan.
///
/// `Complete` validates every sidecar/ParityMap body and trailing filemark and
/// leaves the cursor at `tail_start_lba`. `Absent` proves EOD at `start_lba`.
/// Torn results restore the planned start when possible, ready for a later
/// rewritable-sink overwrite decision.
pub fn reconcile_terminal_prefix(
    source: &mut dyn RawTapeSource,
    plan: &TerminalPrefixPlan,
    tape_uuid: &[u8; 16],
    block_size: u32,
    rewritable: bool,
) -> TerminalPrefixReconcileEvidence {
    if !terminal_prefix_plan_is_well_formed(plan) {
        return TerminalPrefixReconcileEvidence::Unproved;
    }
    let start = PhysicalPositionHint::new(plan.start_lba);
    if source
        .configure_fixed_block_size(block_size)
        .and_then(|()| source.locate_physical(start))
        .is_err()
    {
        return TerminalPrefixReconcileEvidence::Unproved;
    }
    if plan.committed_bundle.entries.is_empty() {
        return TerminalPrefixReconcileEvidence::Complete;
    }
    let block_size = match usize::try_from(block_size) {
        Ok(size) => size,
        Err(_) => return TerminalPrefixReconcileEvidence::Unproved,
    };
    for (entry_index, entry) in plan.committed_bundle.entries.iter().enumerate() {
        let Some(start_lba) = entry.physical_start_hint else {
            return TerminalPrefixReconcileEvidence::Unproved;
        };
        let file_start = PhysicalPositionHint::new(start_lba);
        if source.locate_physical(file_start).is_err() {
            return TerminalPrefixReconcileEvidence::Unproved;
        }
        let mut blocks = Vec::new();
        let mut block = vec![0; block_size];
        for block_index in 0..entry.block_count {
            match source.read_record(&mut block) {
                Ok(RawReadOutcome::Block { bytes, .. }) if bytes == block_size => {
                    blocks.push(block.clone());
                }
                Ok(RawReadOutcome::EndOfData { position_after })
                    if entry_index == 0
                        && block_index == 0
                        && position_after.lba == plan.start_lba =>
                {
                    if source.locate_physical(start).is_err() {
                        return TerminalPrefixReconcileEvidence::Unproved;
                    }
                    return TerminalPrefixReconcileEvidence::Absent;
                }
                _ => return classify_terminal_prefix_torn(source, start, rewritable),
            }
        }
        let valid = match entry.kind {
            TapeFileKind::ParitySidecar => {
                parse_sidecar_tape_file(&blocks, tape_uuid).is_ok_and(|decoded| {
                    entry.canonical_metadata_hash == Some(decoded.header.canonical_metadata_hash)
                        && entry.epoch_id == Some(decoded.header.epoch_id)
                        && entry.protected_ordinal_start
                            == Some(decoded.header.protected_ordinal_start)
                        && entry.protected_ordinal_end_exclusive
                            == Some(decoded.header.protected_ordinal_end_exclusive)
                })
            }
            TapeFileKind::ParityMap => {
                parse_parity_map_tape_file(&blocks, tape_uuid).is_ok_and(|decoded| {
                    entry.canonical_metadata_hash == Some(decoded.header.payload_sha256)
                        && decoded.payload.directory.directory_scope_tape_file_count
                            == plan.tail_start_tape_file_number
                        && decoded
                            .payload
                            .directory
                            .directory_scope_total_data_ordinals
                            == plan.committed_bundle.total_committed_ordinals
                        && decoded
                            .payload
                            .directory
                            .directory_scope_highest_protected_ordinal
                            == plan.committed_bundle.highest_protected_ordinal
                })
            }
            _ => false,
        };
        if !valid {
            return classify_terminal_prefix_torn(source, start, rewritable);
        }
        let Some(expected_position_after) = start_lba
            .checked_add(entry.block_count)
            .and_then(|lba| lba.checked_add(1))
        else {
            return TerminalPrefixReconcileEvidence::Unproved;
        };
        match source.read_record(&mut block) {
            Ok(RawReadOutcome::Filemark { position_after })
                if position_after.lba == expected_position_after => {}
            _ => return classify_terminal_prefix_torn(source, start, rewritable),
        }
    }
    match source.position() {
        Ok(position) if position.lba == plan.tail_start_lba => {
            TerminalPrefixReconcileEvidence::Complete
        }
        _ => classify_terminal_prefix_torn(source, start, rewritable),
    }
}

fn terminal_prefix_plan_is_well_formed(plan: &TerminalPrefixPlan) -> bool {
    if plan.committed_bundle.kind != CommittedBundleKind::TerminalPrefix
        || crate::journal::validate_committed_bundle_shape(&plan.committed_bundle).is_err()
    {
        return false;
    }
    let Ok(entry_count) = u64::try_from(plan.committed_bundle.entries.len()) else {
        return false;
    };
    if plan.start_tape_file_number.checked_add(entry_count)
        != Some(plan.tail_start_tape_file_number)
    {
        return false;
    }
    let mut expected_tape_file = plan.start_tape_file_number;
    let mut expected_lba = plan.start_lba;
    for entry in &plan.committed_bundle.entries {
        if entry.tape_file_number != expected_tape_file
            || entry.physical_start_hint != Some(expected_lba)
        {
            return false;
        }
        let Some(next_lba) = expected_lba
            .checked_add(entry.block_count)
            .and_then(|lba| lba.checked_add(1))
        else {
            return false;
        };
        expected_lba = next_lba;
        let Some(next_file) = expected_tape_file.checked_add(1) else {
            return false;
        };
        expected_tape_file = next_file;
    }
    if expected_lba != plan.tail_start_lba {
        return false;
    }
    let observed_parity_map = plan
        .committed_bundle
        .entries
        .iter()
        .find(|entry| entry.kind == TapeFileKind::ParityMap)
        .map(|entry| entry.tape_file_number);
    if observed_parity_map != plan.parity_map_tape_file_number {
        return false;
    }
    let planned_sidecars = plan
        .committed_bundle
        .entries
        .iter()
        .filter(|entry| entry.kind == TapeFileKind::ParitySidecar)
        .count();
    let existing_sidecars = plan
        .sidecar_directory_entries
        .iter()
        .filter(|entry| entry.tape_file_number < plan.start_tape_file_number)
        .count();
    planned_sidecars
        .checked_add(existing_sidecars)
        .is_some_and(|count| count == plan.sidecar_directory_entries.len())
}

fn classify_terminal_prefix_torn(
    source: &mut dyn RawTapeSource,
    start: PhysicalPositionHint,
    rewritable: bool,
) -> TerminalPrefixReconcileEvidence {
    if source.locate_physical(start).is_err() {
        return TerminalPrefixReconcileEvidence::Unproved;
    }
    if rewritable {
        TerminalPrefixReconcileEvidence::TornRewritable
    } else {
        TerminalPrefixReconcileEvidence::TornWorm
    }
}

/// Reason the session-scoped parity sink closes its current epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// Checkpoint boundary; a short epoch is legal and non-terminal.
    Barrier,
    /// Terminal tape close.
    Finish,
}

/// Opaque actor-carried state for one logical parity write session.
///
/// The raw transport and journal handles are deliberately excluded so an
/// owner actor can release their mutable borrows between commands while the
/// epoch accumulator, file-numbering state, and sidecar directory survive
/// across object appends.
#[allow(missing_debug_implementations)]
pub struct ParitySinkSessionState {
    scheme: ParityScheme,
    tape_uuid: [u8; 16],
    codec: ReedSolomonCodec,
    block_size_bytes: u32,
    neighborhood_idx: u64,
    current_epoch_start: u64,
    data_blocks_in_neighborhood: u64,
    parity_accumulators: Vec<Vec<Vec<u8>>>,
    current_epoch_data_crc64s: Vec<u64>,
    pending_sidecars: Vec<PendingSidecar>,
    highest_protected_ordinal: u64,
    block_size: Option<usize>,
    poisoned: bool,
    last_data_lba: u64,
    filemark_map: FilemarkMapBuilder,
    committed_prefix_snapshot: Option<FileTapeFileJournalCommittedSnapshot>,
    sidecar_directory_entries: Vec<SidecarEpochDirectoryEntry>,
    committed_object_count: u64,
    durable_boundary: DurableBoundaryState,
    control_metadata_hashes: BTreeMap<u64, [u8; 32]>,
    early_warning_reserve: Option<EarlyWarningReserveState>,
    hardware_early_warning_seen: bool,
    bot_bootstrap_committed: bool,
    next_parity_map_sequence: u64,
    last_physical_lba: u64,
    tape_file_start_lbas: BTreeMap<u64, u64>,
}

impl ParitySinkSessionState {
    /// Scheme pinned to this detached logical session.
    pub fn scheme(&self) -> &ParityScheme {
        &self.scheme
    }

    /// Total object-data ordinals already present in this logical session.
    pub fn total_committed_ordinals(&self) -> Result<u64, ParityError> {
        self.filemark_map.total_data_ordinals()
    }

    /// Last physical LBA proved by the sink.
    pub fn used_tape_blocks(&self) -> u64 {
        self.last_physical_lba
    }

    /// Structural rows retained from the live session, excluding the frozen
    /// replay-backed committed prefix.
    pub fn retained_live_structural_row_count(&self) -> usize {
        self.filemark_map.entries().len()
    }

    /// Return the detached-session inputs for one object-start capacity
    /// decision.  This can be evaluated before the owner relinquishes the
    /// only resumable copy of the session state.
    pub fn terminal_triple_capacity_runtime_state(
        &self,
    ) -> Result<TerminalTripleCapacityRuntimeState, ParityError> {
        terminal_triple_runtime_state_from_parts(
            self.data_blocks_in_neighborhood,
            &self.pending_sidecars,
            self.sidecar_directory_entries.len(),
            usize::try_from(self.filemark_map.tape_file_count()?)
                .map_err(|_| ParityError::Invariant("structural row count does not fit usize"))?,
            usize::try_from(self.committed_object_count)
                .map_err(|_| ParityError::Invariant("committed Object count does not fit usize"))?,
            self.last_physical_lba,
        )
    }

    /// Whether any successful raw operation in this session reported early
    /// warning. The bit is sticky so a later checkpoint barrier cannot lose
    /// an EW observed in the middle of an object or sidecar.
    pub fn hardware_early_warning_seen(&self) -> bool {
        self.hardware_early_warning_seen
    }
}

/// Production resume seed backed by replayable bounded journal authority.
#[derive(Debug)]
pub struct BoundedResumeWriterSeed<'a> {
    /// Frozen file-backed journal authority.
    pub committed_prefix_snapshot: FileTapeFileJournalCommittedSnapshot,
    /// Validated bounded resume summary.
    pub committed_prefix_summary: BoundedResumeSummary,
    /// Completed resume plan.
    pub resume_result: &'a ResumeAppendResult,
    /// Optional rebuilt open epoch.
    pub live_epoch: Option<ResumeLiveEpochState>,
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

    /// First ordinal in the explicitly ranged current epoch. Epoch ids are
    /// bare monotonic labels and never derive this value.
    current_epoch_start: u64,

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

    /// Structural map of tape files emitted by this sink.
    filemark_map: FilemarkMapBuilder,
    committed_prefix_snapshot: Option<FileTapeFileJournalCommittedSnapshot>,

    /// Sidecar-directory rows available for bootstrap/parity_map root of
    /// trust emission. The canonical filemark-map digest does not include
    /// these metadata fields; they are carried separately in bootstrap CBOR or
    /// a parity_map control file.
    sidecar_directory_entries: Vec<SidecarEpochDirectoryEntry>,

    /// Committed Object rows represented by the durable prefix. This scalar
    /// preserves capacity authority without retaining the recovery rows.
    committed_object_count: u64,

    /// Catalog-visible commit-point state for object, sidecar, and bootstrap
    /// tape files.
    durable_boundary: DurableBoundaryState,

    /// Metadata hashes for newly emitted control tape files whose structural
    /// filemark-map rows do not carry the hash.
    control_metadata_hashes: BTreeMap<u64, [u8; 32]>,

    /// Runtime guard for EW-only outcomes. A pre-write capacity reserve admits
    /// the object; each successful raw operation consumes that model so every
    /// later EW signal can be handled by one "does the reserve still cover the
    /// not-yet-durable commitments?" predicate.
    early_warning_reserve: Option<EarlyWarningReserveState>,

    /// Sticky hardware EW observation for barrier-time sealing.
    hardware_early_warning_seen: bool,

    /// Whether the sole tape-file-0 BOT Bootstrap is committed.
    bot_bootstrap_committed: bool,

    /// Next sink-owned parity_map sequence number.
    next_parity_map_sequence: u64,

    /// Physical cursor after the most recent successful raw operation. This
    /// avoids issuing extra POSITION probes solely for placement distance
    /// accounting.
    last_physical_lba: u64,

    /// Volume-global LBA of block 0 for every tape file begun by this
    /// logical session, keyed by tape-file number. Captured from
    /// [`Self::last_physical_lba`] at each file's begin — the position after
    /// the previous file's trailing filemark IS the new file's block 0.
    /// Files committed before this session (a resumed prefix) are absent,
    /// never guessed.
    tape_file_start_lbas: BTreeMap<u64, u64>,
}

impl<'a> ParitySink<'a> {
    fn projected_map_digest_for_builder(
        &self,
        builder: &FilemarkMapBuilder,
        provisional: &[TapeFileMapEntry],
        highest_protected_ordinal: u64,
        covers_complete_map: bool,
    ) -> Result<FilemarkMapDigest, ParityError> {
        if let Some(snapshot) = self.committed_prefix_snapshot.as_ref() {
            let mut appended = builder.entries().to_vec();
            appended.extend_from_slice(provisional);
            streamed_filemark_map_digest(
                snapshot,
                &appended,
                highest_protected_ordinal,
                covers_complete_map,
            )
        } else {
            builder.projected_digest(provisional, covers_complete_map)
        }
    }

    /// Detach the logical session state after an object or barrier boundary.
    ///
    /// This is the actor handoff seam: no tape file may be active, and the
    /// next command must reattach the state to the same tape journal.
    pub fn into_session_state(self) -> Result<ParitySinkSessionState, ParityError> {
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "cannot detach parity session state while an object is active",
            ));
        }
        Ok(ParitySinkSessionState {
            scheme: self.scheme,
            tape_uuid: self.tape_uuid,
            codec: self.codec,
            block_size_bytes: self.block_size_bytes,
            neighborhood_idx: self.neighborhood_idx,
            current_epoch_start: self.current_epoch_start,
            data_blocks_in_neighborhood: self.data_blocks_in_neighborhood,
            parity_accumulators: self.parity_accumulators,
            current_epoch_data_crc64s: self.current_epoch_data_crc64s,
            pending_sidecars: self.pending_sidecars,
            highest_protected_ordinal: self.highest_protected_ordinal,
            block_size: self.block_size,
            poisoned: self.poisoned,
            last_data_lba: self.last_data_lba,
            filemark_map: self.filemark_map,
            committed_prefix_snapshot: self.committed_prefix_snapshot,
            sidecar_directory_entries: self.sidecar_directory_entries,
            committed_object_count: self.committed_object_count,
            durable_boundary: self.durable_boundary,
            control_metadata_hashes: self.control_metadata_hashes,
            early_warning_reserve: self.early_warning_reserve,
            hardware_early_warning_seen: self.hardware_early_warning_seen,
            bot_bootstrap_committed: self.bot_bootstrap_committed,
            next_parity_map_sequence: self.next_parity_map_sequence,
            last_physical_lba: self.last_physical_lba,
            tape_file_start_lbas: self.tape_file_start_lbas,
        })
    }

    /// Reattach actor-carried state to the transport and durable sink journal.
    pub fn from_session_state(
        inner: &'a mut dyn RawTapeSink,
        journal: &'a mut dyn TapeFileJournal,
        state: ParitySinkSessionState,
    ) -> Result<Self, ParityError> {
        Self::try_from_session_state(inner, journal, state).map_err(|(error, _state)| error)
    }

    /// Reattach actor-carried state without destroying it when validation of
    /// the transport or journal fails before any tape motion.
    pub fn try_from_session_state(
        inner: &'a mut dyn RawTapeSink,
        journal: &'a mut dyn TapeFileJournal,
        mut state: ParitySinkSessionState,
    ) -> Result<Self, (ParityError, Box<ParitySinkSessionState>)> {
        if journal.tape_uuid() != state.tape_uuid {
            return Err((
                ParityError::SessionOpen(
                    "journal tape UUID does not match parity session state".into(),
                ),
                Box::new(state),
            ));
        }
        let observed = match inner.position() {
            Ok(observed) => observed,
            Err(error) => return Err((error, Box::new(state))),
        };
        if observed.partition != 0 || observed.lba != state.last_physical_lba {
            return Err((
                ParityError::SessionOpen(format!(
                    "parity session transport is at partition {} lba {}, expected partition 0 lba {}",
                    observed.partition, observed.lba, state.last_physical_lba
                )),
                Box::new(state),
            ));
        }
        if let Ok(snapshot) = journal.committed_snapshot_bounded_authority() {
            let summary = match checked_bounded_resume_summary(&snapshot) {
                Ok(summary) => summary,
                Err(error) => return Err((error, Box::new(state))),
            };
            let state_total = match state.filemark_map.total_data_ordinals() {
                Ok(total) => total,
                Err(error) => return Err((error, Box::new(state))),
            };
            if summary.append_position.lba != state.last_physical_lba
                || summary.highest_protected_ordinal != state.highest_protected_ordinal
                || summary.total_committed_ordinals != state_total
                || summary.sidecar_directory_entries != state.sidecar_directory_entries
            {
                return Err((
                    ParityError::SessionOpen(
                        "bounded journal checkpoint disagrees with detached parity state".into(),
                    ),
                    Box::new(state),
                ));
            }
            state.filemark_map = FilemarkMapBuilder::from_bounded_prefix(
                summary.committed_tape_file_count,
                summary.total_committed_ordinals,
            );
            state.committed_prefix_snapshot = Some(snapshot);
            state.committed_object_count = summary.committed_object_count;
        }
        Ok(Self {
            backend: ParitySinkBackend(inner),
            journal: Some(journal),
            scheme: state.scheme,
            tape_uuid: state.tape_uuid,
            codec: state.codec,
            block_size_bytes: state.block_size_bytes,
            neighborhood_idx: state.neighborhood_idx,
            current_epoch_start: state.current_epoch_start,
            data_blocks_in_neighborhood: state.data_blocks_in_neighborhood,
            parity_accumulators: state.parity_accumulators,
            current_epoch_data_crc64s: state.current_epoch_data_crc64s,
            pending_sidecars: state.pending_sidecars,
            highest_protected_ordinal: state.highest_protected_ordinal,
            block_size: state.block_size,
            poisoned: state.poisoned,
            last_data_lba: state.last_data_lba,
            active_object: None,
            filemark_map: state.filemark_map,
            committed_prefix_snapshot: state.committed_prefix_snapshot,
            sidecar_directory_entries: state.sidecar_directory_entries,
            committed_object_count: state.committed_object_count,
            durable_boundary: state.durable_boundary,
            control_metadata_hashes: state.control_metadata_hashes,
            early_warning_reserve: state.early_warning_reserve,
            hardware_early_warning_seen: state.hardware_early_warning_seen,
            bot_bootstrap_committed: state.bot_bootstrap_committed,
            next_parity_map_sequence: state.next_parity_map_sequence,
            last_physical_lba: state.last_physical_lba,
            tape_file_start_lbas: state.tape_file_start_lbas,
        })
    }

    /// Construct a new sidecar-only parity sink wrapping `inner`.
    ///
    /// `block_size_bytes` is the tape's logical block size
    /// (from `DriveHandle::read_config()` or the format
    /// `WriteParams`). Pinned at construction because the
    /// bootstrap write (step 11.7) happens before any data
    /// write and needs to know how big the on-tape buffer is.
    /// Returns [`ParityError::InvalidScheme`] if the scheme
    /// fails validation.
    #[cfg(test)]
    pub(crate) fn new(
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
    #[cfg(test)]
    pub(crate) fn new_sidecar_only(
        inner: &'a mut dyn RawTapeSink,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
    ) -> Result<Self, ParityError> {
        let mut sink = Self::new(inner, scheme, tape_uuid, block_size_bytes)?;
        // Most sink unit tests target body/sidecar fault sequencing rather
        // than BOT emission. Model those tests honestly as an append session
        // whose one-file committed prefix is the already-written BOT.
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])?;
        sink.filemark_map = FilemarkMapBuilder::from_committed_prefix(&bot_map);
        sink.bot_bootstrap_committed = true;
        sink.durable_boundary = DurableBoundaryState::from_last_committed_tape_file_number(Some(0));
        Ok(sink)
    }

    /// Construct a sidecar-only parity sink after a §7.8 resume operation.
    ///
    /// The seed must carry the Layer 5 catalog prefix after any
    /// resume-generated sidecars have committed. Its live epoch is consumed
    /// by value so the writer can continue accumulating the rebuilt open epoch
    /// without cloning its data shards.
    pub fn new_sidecar_only_from_bounded_resume(
        inner: &'a mut dyn RawTapeSink,
        journal: &'a mut dyn TapeFileJournal,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
        resume_seed: BoundedResumeWriterSeed<'_>,
    ) -> Result<Self, ParityError> {
        if journal.tape_uuid() != tape_uuid {
            return Err(ParityError::SessionOpen(
                "journal tape UUID does not match parity sink tape UUID".into(),
            ));
        }
        Self::new_sidecar_only_from_bounded_resume_inner(
            inner,
            Some(journal),
            scheme,
            tape_uuid,
            block_size_bytes,
            resume_seed,
        )
    }

    fn new_sidecar_only_from_bounded_resume_inner(
        inner: &'a mut dyn RawTapeSink,
        journal: Option<&'a mut dyn TapeFileJournal>,
        scheme: ParityScheme,
        tape_uuid: [u8; 16],
        block_size_bytes: u32,
        resume_seed: BoundedResumeWriterSeed<'_>,
    ) -> Result<Self, ParityError> {
        let mut sink = Self::new_with_backend(
            ParitySinkBackend(inner),
            journal,
            scheme,
            tape_uuid,
            block_size_bytes,
        )?;
        sink.validate_bounded_resume_prefix(
            &resume_seed.committed_prefix_summary,
            resume_seed.resume_result,
        )?;
        let expected_append_position = resume_seed.committed_prefix_summary.append_position;
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
        sink.filemark_map = FilemarkMapBuilder::from_bounded_prefix(
            resume_seed
                .committed_prefix_summary
                .committed_tape_file_count,
            resume_seed
                .committed_prefix_summary
                .total_committed_ordinals,
        );
        sink.committed_prefix_snapshot = Some(resume_seed.committed_prefix_snapshot);
        sink.sidecar_directory_entries = resume_seed
            .committed_prefix_summary
            .sidecar_directory_entries;
        sink.committed_object_count = resume_seed.committed_prefix_summary.committed_object_count;
        sink.durable_boundary = DurableBoundaryState::from_last_committed_tape_file_number(
            resume_seed
                .committed_prefix_summary
                .last_committed_tape_file_number,
        );
        sink.highest_protected_ordinal = resume_seed.resume_result.highest_protected_ordinal;
        sink.bot_bootstrap_committed = resume_seed.committed_prefix_summary.bot_bootstrap_committed;
        sink.next_parity_map_sequence = resume_seed
            .committed_prefix_summary
            .next_parity_map_sequence;
        sink.last_data_lba = actual_append_position.lba;
        sink.last_physical_lba = actual_append_position.lba;
        sink.load_resume_live_epoch(resume_seed.resume_result, resume_seed.live_epoch)?;
        Ok(sink)
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
            current_epoch_start: 0,
            data_blocks_in_neighborhood: 0,
            parity_accumulators,
            current_epoch_data_crc64s: Vec::new(),
            pending_sidecars: Vec::new(),
            highest_protected_ordinal: 0,
            block_size: None,
            poisoned: false,
            last_data_lba: 0,
            active_object: None,
            filemark_map: FilemarkMapBuilder::new(),
            committed_prefix_snapshot: None,
            sidecar_directory_entries: Vec::new(),
            committed_object_count: 0,
            durable_boundary: DurableBoundaryState::new(),
            control_metadata_hashes: BTreeMap::new(),
            early_warning_reserve: None,
            hardware_early_warning_seen: false,
            bot_bootstrap_committed: false,
            next_parity_map_sequence: 0,
            last_physical_lba: 0,
            tape_file_start_lbas: BTreeMap::new(),
        })
    }

    fn validate_bounded_resume_prefix(
        &self,
        summary: &BoundedResumeSummary,
        resume_result: &ResumeAppendResult,
    ) -> Result<(), ParityError> {
        let sidecar_count = u64::try_from(resume_result.sidecars_emitted.len())
            .map_err(|_| ParityError::Invariant("resume sidecar count does not fit u64"))?;
        let expected_tape_files = resume_result
            .append_after_tape_file_number
            .checked_add(1)
            .and_then(|count| count.checked_add(sidecar_count))
            .ok_or(ParityError::Invariant(
                "resume committed prefix tape-file count overflows",
            ))?;
        if summary.committed_tape_file_count != expected_tape_files {
            return Err(ParityError::Invariant(
                "resume committed prefix does not include exactly the committed resume sidecars",
            ));
        }
        if summary.total_committed_ordinals != resume_result.next_data_ordinal {
            return Err(ParityError::Invariant(
                "resume committed prefix total ordinals do not match ResumeAppendResult",
            ));
        }
        if summary.highest_protected_ordinal != resume_result.highest_protected_ordinal {
            return Err(ParityError::Invariant(
                "resume committed prefix protection watermark does not match ResumeAppendResult",
            ));
        }
        if summary.scheme() != &self.scheme {
            return Err(ParityError::Invariant("resume summary scheme mismatch"));
        }
        for (index, sidecar) in resume_result.sidecars_emitted.iter().enumerate() {
            let expected_tape_file_number = resume_result
                .append_after_tape_file_number
                .checked_add(1)
                .and_then(|value| value.checked_add(u64::try_from(index).ok()?))
                .ok_or(ParityError::Invariant(
                    "resume sidecar tape-file number overflows",
                ))?;
            if sidecar.tape_file_number != expected_tape_file_number {
                return Err(ParityError::Invariant(
                    "resume sidecar tape-file numbers are not contiguous after the append point",
                ));
            }
            let directory = summary
                .sidecar_directory_entries
                .iter()
                .find(|entry| entry.tape_file_number == expected_tape_file_number)
                .ok_or(ParityError::Invariant(
                    "resume sidecar is absent from bounded directory",
                ))?;
            if directory.sidecar_total_block_count != sidecar.block_count
                || directory.epoch_id != sidecar.epoch_id
                || directory.protected_ordinal_start != sidecar.protected_ordinal_start
                || directory.protected_ordinal_end_exclusive
                    != sidecar.protected_ordinal_end_exclusive
            {
                return Err(ParityError::Invariant("bounded resume sidecar mismatch"));
            }
        }
        Ok(())
    }

    fn load_resume_live_epoch(
        &mut self,
        resume_result: &ResumeAppendResult,
        live_epoch: Option<ResumeLiveEpochState>,
    ) -> Result<(), ParityError> {
        let Some(live) = live_epoch else {
            if resume_result.live_epoch_start != resume_result.next_data_ordinal {
                return Err(ParityError::Invariant(
                    "resume result without live epoch has a non-empty live range",
                ));
            }
            if resume_result.highest_protected_ordinal != resume_result.next_data_ordinal {
                return Err(ParityError::Invariant(
                    "resume result without live epoch does not end at its protection watermark",
                ));
            }
            self.neighborhood_idx = resume_result.next_epoch_id;
            self.current_epoch_start = resume_result.next_data_ordinal;
            return Ok(());
        };

        let epoch_data_shards = self.epoch_data_shards()?;

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
        if protected_ordinal_start != resume_result.highest_protected_ordinal {
            return Err(ParityError::Invariant(
                "resume live epoch does not start at the protected-range watermark",
            ));
        }
        if epoch_id != resume_result.next_epoch_id {
            return Err(ParityError::Invariant(
                "resume live epoch id does not match the next monotonic epoch id",
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
        self.current_epoch_start = protected_ordinal_start;
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

    /// Consume one checked terminal-triple reservation and begin its Object.
    pub fn begin_object_with_terminal_triple_reservation(
        &mut self,
        reservation: TerminalTripleObjectReservation,
    ) -> Result<(u64, TerminalTripleCloseReport), ParityError> {
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
        self.validate_terminal_triple_reservation(&reservation)?;
        let (input, report) = reservation.into_parts();
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
        let filemark_outcome = match self.backend.write_filemarks(1, true) {
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
        let first_parity_data_ordinal =
            entry
                .first_parity_data_ordinal
                .ok_or(ParityError::Invariant(
                    "object map entry missing first parity data ordinal",
                ))?;
        let sidecars_emitted = self.emit_pending_sidecars()?;
        if let Err(err) = self
            .validate_v1_post_object_bundle_bound(first_parity_data_ordinal, object.written_blocks)
        {
            self.poisoned = true;
            return Err(err);
        }
        let summary = ObjectWriteSummary {
            tape_file_number: object.tape_file_number,
            first_parity_data_ordinal,
            projected_size_blocks: object.projected_size_blocks,
            data_block_count: object.written_blocks,
            filemark_outcome,
            sidecars_emitted,
            highest_protected_ordinal: self.highest_protected_ordinal,
            physical_start_lba: self.tape_file_start_lba(object.tape_file_number),
        };
        let committed_object_count = self
            .committed_object_count
            .checked_add(1)
            .ok_or(ParityError::Invariant("committed Object count overflows"))?;
        if let Err(err) = self.commit_journal_map_range(
            CommittedBundleKind::Object,
            bundle_start,
            &summary.sidecars_emitted,
        ) {
            self.poisoned = true;
            return Err(err);
        }
        self.committed_object_count = committed_object_count;
        Ok(summary)
    }

    /// Tape-file number for the active object, if any.
    pub fn active_object_tape_file_number(&self) -> Option<u64> {
        self.active_object.map(|object| object.tape_file_number)
    }

    /// Object blocks written since the current `begin_object`.
    pub fn active_object_blocks_written(&self) -> Option<u64> {
        self.active_object.map(|object| object.written_blocks)
    }

    /// Emit the sole schema-major 2 Bootstrap at BOT.
    pub fn write_bootstrap(&mut self) -> Result<u64, ParityError> {
        let bundle_start = self.filemark_map.next_tape_file_number()?;
        if bundle_start != 0 || self.bot_bootstrap_committed {
            return Err(ParityError::Invariant(
                "schema-major 2 permits only the sole tape-file-0 BOT Bootstrap",
            ));
        }
        let bootstrap_entry = TapeFileMapEntry::bootstrap(0, 1);
        let digest = self.projected_map_digest_for_builder(
            &self.filemark_map,
            std::slice::from_ref(&bootstrap_entry),
            self.highest_protected_ordinal,
            false,
        )?;
        let tape_file_number = self.write_prepared_bootstrap(0, self.bootstrap_payload(digest))?;
        if let Err(err) =
            self.commit_journal_map_range(CommittedBundleKind::BotBootstrap, bundle_start, &[])
        {
            self.poisoned = true;
            return Err(err);
        }
        Ok(tape_file_number)
    }

    /// Close the open epoch as needed and commit a resumable journal checkpoint.
    pub fn checkpoint(&mut self) -> Result<CheckpointResult, ParityError> {
        self.close_open_epoch(CloseReason::Barrier)
    }

    fn peek_parity_map_sequence(&self) -> Result<u64, ParityError> {
        self.next_parity_map_sequence
            .checked_add(1)
            .ok_or(ParityError::Invariant("parity_map sequence overflow"))?;
        Ok(self.next_parity_map_sequence)
    }

    fn bootstrap_payload(&self, digest: FilemarkMapDigest) -> BootstrapPayload {
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
            sequence: 0,
            block_size_bytes: self.block_size_bytes,
            drive_compression: false,
        }
    }

    fn sidecar_directory_for_scope(
        &self,
        directory_scope_tape_file_count: u64,
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

    fn write_prepared_bootstrap(
        &mut self,
        tape_file_number: u64,
        payload: BootstrapPayload,
    ) -> Result<u64, ParityError> {
        self.durable_boundary
            .begin_tape_file(TapeFileKind::Bootstrap, tape_file_number)?;
        self.note_tape_file_start(tape_file_number);
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
            .projected_map_digest_for_builder(
                &self.filemark_map,
                &[],
                self.highest_protected_ordinal,
                false,
            )
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
        self.bot_bootstrap_committed = true;
        Ok(tape_file_number)
    }

    fn write_prepared_parity_map(
        &mut self,
        tape_file_number: u64,
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
        self.note_tape_file_start(tape_file_number);
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

    fn write_terminal_prefix_parity_map(&mut self) -> Result<Option<u64>, ParityError> {
        if self.sidecar_directory_entries.is_empty() {
            return Ok(None);
        }
        let tape_file_number = self.filemark_map.next_tape_file_number()?;
        let sequence = self.peek_parity_map_sequence()?;
        let provisional_directory = self.sidecar_directory_for_scope(
            tape_file_number
                .checked_add(1)
                .ok_or(ParityError::Invariant("terminal ParityMap scope overflows"))?,
            self.filemark_map.total_data_ordinals()?,
            self.highest_protected_ordinal,
            true,
        )?;
        let provisional = encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: self.tape_uuid,
                sequence,
                directory: provisional_directory,
                canonical_map_digest: [0; 32],
                writer_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                write_timestamp: None,
            },
            self.block_size_bytes,
        )?;
        let block_count = u64::try_from(provisional.blocks.len())
            .map_err(|_| ParityError::Invariant("terminal ParityMap block count overflows u64"))?;
        let entry = TapeFileMapEntry::parity_map(tape_file_number, block_count);
        let digest = self.projected_map_digest_for_builder(
            &self.filemark_map,
            std::slice::from_ref(&entry),
            self.highest_protected_ordinal,
            true,
        )?;
        let directory = self.sidecar_directory_for_scope(
            digest.tape_file_count,
            digest.map_total_data_ordinals,
            digest.highest_protected_ordinal,
            true,
        )?;
        let encoded = encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: self.tape_uuid,
                sequence,
                directory,
                canonical_map_digest: digest.map_sha256,
                writer_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                write_timestamp: None,
            },
            self.block_size_bytes,
        )?;
        if encoded.blocks.len() as u64 != block_count {
            return Err(ParityError::Invariant(
                "terminal ParityMap geometry changed after digest finalization",
            ));
        }
        self.write_prepared_parity_map(
            tape_file_number,
            block_count,
            encoded.header.payload_sha256,
            &encoded.blocks,
        )?;
        self.next_parity_map_sequence = sequence
            .checked_add(1)
            .ok_or(ParityError::Invariant("parity_map sequence overflow"))?;
        Ok(Some(tape_file_number))
    }

    fn write_control_block_and_filemark(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u64,
        block: &[u8],
    ) -> Result<(), ParityError> {
        self.write_control_block(kind, tape_file_number, block)?;
        self.write_control_filemark(kind, tape_file_number)
    }

    fn write_control_block(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u64,
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
        if outcome.bytes_written as usize != block.len() {
            self.poisoned = true;
            return Err(self.abandon_tape_file_boundary_or(
                kind,
                tape_file_number,
                ParityError::Invariant("control block write completed with a nonexact byte count"),
            ));
        }
        self.record_physical_position(outcome.position_after.lba);
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
        tape_file_number: u64,
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
        let outcome = match self.backend.write_filemarks(1, true) {
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
    /// geometry the caller (Layer 5) records in the catalog.
    ///
    /// The active object must already be closed with [`Self::finish_object`].
    /// On success, `finish()` emits only any pending final partial sidecar;
    /// the sole Bootstrap remains the tape-file-0 BOT record.
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
        // Per codex idref=30bf15c0 (Medium #1): `data_area_end_lba`
        // is the LBA where user data ENDED (= the next LBA after
        // the last user data block), NOT the post-parity physical
        // cursor. We tracked that during every successful
        // `write_block` data write as `self.last_data_lba`; use it.
        let data_area_end_lba = self.last_data_lba;

        self.close_open_epoch(CloseReason::Finish)?;
        Ok(FinalGeometry { data_area_end_lba })
    }

    /// Compute the exact final parity prefix without issuing a media command.
    ///
    /// The caller persists this immutable plan together with
    /// `BeforeReplicaA`, then passes the same value to
    /// [`Self::close_for_terminal_index`]. The execution path recomputes and
    /// compares it before motion, so stale or incomplete intent fails closed.
    pub fn plan_terminal_index_close(&self) -> Result<TerminalPrefixPlan, ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "plan_terminal_index_close called on poisoned parity sink",
            ));
        }
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "plan_terminal_index_close called while an object is active",
            ));
        }

        let start_tape_file_number = self.filemark_map.next_tape_file_number()?;
        let mut projected_map = self.filemark_map.clone();
        let mut projected_directory = self.sidecar_directory_entries.clone();
        let mut projected_highest_protected = self.highest_protected_ordinal;
        let mut tail_start_lba = self.last_physical_lba;
        let mut entries = Vec::new();

        for sidecar in self.terminal_prefix_pending_sidecars()? {
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
            let encoded = encode_sidecar_tape_file(
                &descriptor,
                &sidecar.parity_shards,
                sidecar.data_shard_crc64s,
            )?;
            let block_count = u64::try_from(encoded.blocks.len())
                .map_err(|_| ParityError::Invariant("sidecar block count overflows u64"))?;
            let map_entry = projected_map.push_parity_sidecar(
                block_count,
                sidecar.epoch_id,
                sidecar.protected_ordinal_start,
                sidecar.protected_ordinal_end_exclusive,
            )?;
            let mut flags =
                SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD;
            let final_partial_epoch = sidecar.is_terminal
                && encoded.header.real_data_shard_count < encoded.header.logical_shard_count;
            if final_partial_epoch {
                flags |= SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH;
            }
            projected_directory.push(SidecarEpochDirectoryEntry {
                tape_file_number: map_entry.tape_file_number,
                epoch_id: sidecar.epoch_id,
                protected_ordinal_start: sidecar.protected_ordinal_start,
                protected_ordinal_end_exclusive: sidecar.protected_ordinal_end_exclusive,
                sidecar_total_block_count: block_count,
                sidecar_header_block_count: encoded.header.shard_index_block_count,
                parity_shard_block_count: encoded.header.parity_block_count,
                canonical_metadata_hash: encoded.header.canonical_metadata_hash,
                flags,
            });
            entries.push(TapeFileEntry {
                tape_file_number: map_entry.tape_file_number,
                kind: TapeFileKind::ParitySidecar,
                block_count,
                physical_start_hint: Some(tail_start_lba),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: Some(sidecar.epoch_id),
                protected_ordinal_start: Some(sidecar.protected_ordinal_start),
                protected_ordinal_end_exclusive: Some(sidecar.protected_ordinal_end_exclusive),
                canonical_metadata_hash: Some(encoded.header.canonical_metadata_hash),
                object_recovery_row: None,
            });
            projected_highest_protected =
                projected_highest_protected.max(sidecar.protected_ordinal_end_exclusive);
            tail_start_lba = tail_start_lba
                .checked_add(block_count)
                .and_then(|lba| lba.checked_add(1))
                .ok_or(ParityError::Invariant(
                    "terminal prefix physical position overflows",
                ))?;
        }

        let parity_map_tape_file_number = if projected_directory.is_empty() {
            None
        } else {
            let tape_file_number = projected_map.next_tape_file_number()?;
            let sequence = self.peek_parity_map_sequence()?;
            let scope_tape_file_count = tape_file_number
                .checked_add(1)
                .ok_or(ParityError::Invariant("terminal ParityMap scope overflows"))?;
            let total_data_ordinals = projected_map.total_data_ordinals()?;
            let directory_for = |canonical_map_digest| {
                let directory = SidecarEpochDirectory {
                    directory_scope_tape_file_count: scope_tape_file_count,
                    directory_scope_total_data_ordinals: total_data_ordinals,
                    directory_scope_highest_protected_ordinal: projected_highest_protected,
                    is_final_directory: true,
                    entries: projected_directory
                        .iter()
                        .filter(|entry| entry.tape_file_number < scope_tape_file_count)
                        .cloned()
                        .collect(),
                };
                directory.validate()?;
                encode_parity_map_tape_file(
                    &ParityMapPayload {
                        tape_uuid: self.tape_uuid,
                        sequence,
                        directory,
                        canonical_map_digest,
                        writer_version: Some(env!("CARGO_PKG_VERSION").to_string()),
                        write_timestamp: None,
                    },
                    self.block_size_bytes,
                )
            };
            let provisional = directory_for([0; 32])?;
            let block_count = u64::try_from(provisional.blocks.len()).map_err(|_| {
                ParityError::Invariant("terminal ParityMap block count overflows u64")
            })?;
            let projected_entry = TapeFileMapEntry::parity_map(tape_file_number, block_count);
            let digest = self.projected_map_digest_for_builder(
                &projected_map,
                std::slice::from_ref(&projected_entry),
                projected_highest_protected,
                true,
            )?;
            let encoded = directory_for(digest.map_sha256)?;
            if encoded.blocks.len() as u64 != block_count {
                return Err(ParityError::Invariant(
                    "terminal ParityMap geometry changed after digest finalization",
                ));
            }
            let map_entry = projected_map.push_parity_map(block_count)?;
            entries.push(TapeFileEntry {
                tape_file_number: map_entry.tape_file_number,
                kind: TapeFileKind::ParityMap,
                block_count,
                physical_start_hint: Some(tail_start_lba),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: Some(encoded.header.payload_sha256),
                object_recovery_row: None,
            });
            tail_start_lba = tail_start_lba
                .checked_add(block_count)
                .and_then(|lba| lba.checked_add(1))
                .ok_or(ParityError::Invariant(
                    "terminal prefix physical position overflows",
                ))?;
            Some(tape_file_number)
        };
        let tail_start_tape_file_number = projected_map.next_tape_file_number()?;
        let committed_bundle = CommittedBundle {
            kind: CommittedBundleKind::TerminalPrefix,
            entries,
            highest_protected_ordinal: projected_highest_protected,
            total_committed_ordinals: projected_map.total_data_ordinals()?,
        };
        crate::journal::validate_committed_bundle_shape(&committed_bundle).map_err(|_| {
            ParityError::Invariant("planned terminal prefix violates sink-journal grammar")
        })?;
        Ok(TerminalPrefixPlan {
            start_tape_file_number,
            tail_start_tape_file_number,
            start_lba: self.last_physical_lba,
            tail_start_lba,
            parity_map_tape_file_number,
            sidecar_directory_entries: projected_directory,
            committed_bundle,
        })
    }

    fn terminal_prefix_pending_sidecars(&self) -> Result<Vec<PendingSidecar>, ParityError> {
        let mut pending = self.pending_sidecars.clone();
        if self.data_blocks_in_neighborhood == 0 {
            return Ok(pending);
        }
        let block_size = self.block_size.ok_or(ParityError::Invariant(
            "open epoch contains data but block size is unpinned",
        ))?;
        let expected_block_size = usize::try_from(self.block_size_bytes)
            .map_err(|_| ParityError::Invariant("fixed block size does not fit usize"))?;
        if block_size != expected_block_size {
            return Err(ParityError::Invariant(
                "terminal prefix partial sidecar block size mismatch",
            ));
        }
        let mut parity_shards = Vec::new();
        for stripe in &self.parity_accumulators {
            if stripe.len() != self.codec.parity_blocks()
                || stripe.iter().any(|shard| shard.len() != block_size)
            {
                return Err(ParityError::Invariant(
                    "terminal prefix parity accumulator shape mismatch",
                ));
            }
            parity_shards.extend(stripe.iter().cloned());
        }
        let protected_ordinal_end_exclusive = self
            .current_epoch_start
            .checked_add(self.data_blocks_in_neighborhood)
            .ok_or(ParityError::Invariant("sidecar protected range overflows"))?;
        pending.push(PendingSidecar {
            epoch_id: self.neighborhood_idx,
            block_size: self.block_size_bytes,
            protected_ordinal_start: self.current_epoch_start,
            protected_ordinal_end_exclusive,
            parity_shards,
            data_shard_crc64s: self.current_epoch_data_crc64s.clone(),
            is_terminal: true,
        });
        Ok(pending)
    }

    fn adopt_complete_terminal_prefix(
        &mut self,
        plan: &TerminalPrefixPlan,
    ) -> Result<(), ParityError> {
        if self.data_blocks_in_neighborhood > 0 {
            let block_size = self.block_size.ok_or(ParityError::Invariant(
                "open epoch contains data but block size is unpinned",
            ))?;
            self.queue_partial_sidecar_without_writing_padding(block_size, true)?;
        }
        self.pending_sidecars.clear();
        for entry in &plan.committed_bundle.entries {
            self.tape_file_start_lbas.insert(
                entry.tape_file_number,
                entry.physical_start_hint.ok_or(ParityError::Invariant(
                    "persisted terminal prefix row lacks physical start",
                ))?,
            );
            let mapped = match entry.kind {
                TapeFileKind::ParitySidecar => self.filemark_map.push_parity_sidecar(
                    entry.block_count,
                    entry.epoch_id.ok_or(ParityError::Invariant(
                        "persisted terminal sidecar lacks epoch",
                    ))?,
                    entry.protected_ordinal_start.ok_or(ParityError::Invariant(
                        "persisted terminal sidecar lacks range start",
                    ))?,
                    entry
                        .protected_ordinal_end_exclusive
                        .ok_or(ParityError::Invariant(
                            "persisted terminal sidecar lacks range end",
                        ))?,
                )?,
                TapeFileKind::ParityMap => {
                    let mapped = self.filemark_map.push_parity_map(entry.block_count)?;
                    self.control_metadata_hashes.insert(
                        entry.tape_file_number,
                        entry.canonical_metadata_hash.ok_or(ParityError::Invariant(
                            "persisted terminal ParityMap lacks payload hash",
                        ))?,
                    );
                    mapped
                }
                _ => {
                    return Err(ParityError::Invariant(
                        "persisted terminal prefix contains an invalid tape-file kind",
                    ));
                }
            };
            if mapped.tape_file_number != entry.tape_file_number
                || mapped.block_count != entry.block_count
            {
                return Err(ParityError::Invariant(
                    "persisted terminal prefix is not dense after committed prefix",
                ));
            }
        }
        self.sidecar_directory_entries = plan.sidecar_directory_entries.clone();
        self.highest_protected_ordinal = plan.committed_bundle.highest_protected_ordinal;
        self.last_physical_lba = plan.tail_start_lba;
        Ok(())
    }

    /// Close the final parity prefix for the terminal triple-index writer.
    ///
    /// This emits a pending partial sidecar and an external final ParityMap
    /// when parity metadata exists. It intentionally emits no legacy final
    /// Bootstrap. The zero-count barrier and exact post-barrier position are
    /// proved before the `TerminalPrefix` bundle and checkpoint watermark are
    /// fsynced.
    pub fn close_for_terminal_index(
        mut self,
        expected_plan: &TerminalPrefixPlan,
        evidence: TerminalPrefixReconcileEvidence,
    ) -> Result<TerminalPrefixCloseResult, ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "close_for_terminal_index called on poisoned parity sink",
            ));
        }
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "close_for_terminal_index called while an object is active",
            ));
        }
        let current_plan = self.plan_terminal_index_close()?;
        if &current_plan != expected_plan {
            return Err(ParityError::Invariant(
                "terminal prefix execution does not match persisted immutable plan",
            ));
        }
        let bundle_start = self.filemark_map.next_tape_file_number()?;
        let (sidecars_emitted, parity_map_tape_file_number) = match evidence {
            TerminalPrefixReconcileEvidence::Absent => {
                let position = self.backend.position()?;
                if position.lba != expected_plan.start_lba {
                    return Err(ParityError::SessionOpen(format!(
                        "absent terminal prefix cursor is at lba {}, expected {}",
                        position.lba, expected_plan.start_lba
                    )));
                }
                if self.data_blocks_in_neighborhood > 0 {
                    let block_size = self.block_size.ok_or(ParityError::Invariant(
                        "open epoch contains data but block size is unpinned",
                    ))?;
                    self.queue_partial_sidecar_without_writing_padding(block_size, true)?;
                }
                let sidecars = self.emit_pending_sidecars()?;
                let parity_map = self.write_terminal_prefix_parity_map()?;
                (sidecars, parity_map)
            }
            TerminalPrefixReconcileEvidence::TornRewritable => {
                self.backend
                    .locate_for_overwrite(PhysicalPositionHint::new(expected_plan.start_lba))?;
                if self.data_blocks_in_neighborhood > 0 {
                    let block_size = self.block_size.ok_or(ParityError::Invariant(
                        "open epoch contains data but block size is unpinned",
                    ))?;
                    self.queue_partial_sidecar_without_writing_padding(block_size, true)?;
                }
                let sidecars = self.emit_pending_sidecars()?;
                let parity_map = self.write_terminal_prefix_parity_map()?;
                (sidecars, parity_map)
            }
            TerminalPrefixReconcileEvidence::Complete => {
                let position = self.backend.position()?;
                if position.lba != expected_plan.tail_start_lba {
                    return Err(ParityError::SessionOpen(format!(
                        "complete terminal prefix cursor is at lba {}, expected {}",
                        position.lba, expected_plan.tail_start_lba
                    )));
                }
                self.adopt_complete_terminal_prefix(expected_plan)?;
                (Vec::new(), expected_plan.parity_map_tape_file_number)
            }
            TerminalPrefixReconcileEvidence::TornWorm
            | TerminalPrefixReconcileEvidence::Unproved => {
                return Err(ParityError::SessionOpen(format!(
                    "terminal prefix requires recovery: {evidence:?}"
                )));
            }
        };
        let barrier_outcome = match self.backend.write_filemarks(0, false) {
            Ok(outcome) => outcome,
            Err(error) => {
                self.poisoned = true;
                return Err(ParityError::TapeIo(error));
            }
        };
        self.record_physical_position(barrier_outcome.position_after.lba);
        if barrier_outcome.end_of_medium {
            self.poisoned = true;
            return Err(ParityError::TapeIo(TapeIoError::HardEndOfMedium {
                sense: Vec::new(),
            }));
        }
        let observed = match self.backend.position() {
            Ok(observed) => observed,
            Err(error) => {
                self.poisoned = true;
                return Err(ParityError::TapeIo(error));
            }
        };
        if observed.partition != barrier_outcome.position_after.partition
            || observed.lba != barrier_outcome.position_after.lba
        {
            self.poisoned = true;
            return Err(ParityError::SessionOpen(format!(
                "terminal prefix barrier observed partition {} lba {}, position re-read returned partition {} lba {}",
                barrier_outcome.position_after.partition,
                barrier_outcome.position_after.lba,
                observed.partition,
                observed.lba
            )));
        }
        let committed_bundle = if evidence == TerminalPrefixReconcileEvidence::Complete {
            expected_plan.committed_bundle.clone()
        } else {
            self.journal_map_range_bundle(
                CommittedBundleKind::TerminalPrefix,
                bundle_start,
                &sidecars_emitted,
            )?
        };
        if committed_bundle != expected_plan.committed_bundle
            || parity_map_tape_file_number != expected_plan.parity_map_tape_file_number
            || observed.lba != expected_plan.tail_start_lba
        {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "terminal prefix execution diverged from persisted immutable plan",
            ));
        }
        if committed_bundle
            .entries
            .iter()
            .any(|entry| entry.kind == TapeFileKind::Bootstrap)
        {
            self.poisoned = true;
            return Err(ParityError::Invariant(
                "terminal prefix unexpectedly contains a Bootstrap",
            ));
        }
        let checkpoint_bundle = CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: committed_bundle.highest_protected_ordinal,
            total_committed_ordinals: committed_bundle.total_committed_ordinals,
        };
        if let Some(journal) = self.journal.as_mut() {
            if let Err(error) =
                journal.commit_terminal_prefix_transition(&committed_bundle, &checkpoint_bundle)
            {
                self.poisoned = true;
                return Err(error.into());
            }
        }
        Ok(TerminalPrefixCloseResult {
            sidecars_emitted,
            parity_map_tape_file_number,
            barrier_outcome,
            used_tape_blocks: observed.lba,
            committed_bundle,
        })
    }

    /// Close the current explicit epoch and publish one checkpoint watermark.
    ///
    /// A short sidecar is emitted when needed. Schema-major 2 checkpointing
    /// never writes another Bootstrap after the sole BOT copy.
    pub fn close_open_epoch(
        &mut self,
        reason: CloseReason,
    ) -> Result<CheckpointResult, ParityError> {
        if self.poisoned {
            return Err(ParityError::Invariant(
                "close_open_epoch called on poisoned parity sink",
            ));
        }
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "close_open_epoch called while an object is active",
            ));
        }
        let is_terminal = reason == CloseReason::Finish;
        let bundle_start = self.filemark_map.next_tape_file_number()?;
        if self.data_blocks_in_neighborhood > 0 {
            let block_size = self.block_size.ok_or(ParityError::Invariant(
                "open epoch contains data but block size is unpinned",
            ))?;
            self.queue_partial_sidecar_without_writing_padding(block_size, is_terminal)?;
        }
        let sidecars_emitted = self.emit_pending_sidecars()?;
        let barrier_outcome = match self.backend.write_filemarks(0, false) {
            Ok(outcome) => outcome,
            Err(err) => {
                self.poisoned = true;
                return Err(ParityError::TapeIo(err));
            }
        };
        self.record_physical_position(barrier_outcome.position_after.lba);
        if barrier_outcome.end_of_medium {
            self.poisoned = true;
            return Err(ParityError::TapeIo(TapeIoError::HardEndOfMedium {
                sense: Vec::new(),
            }));
        }
        let kind = CommittedBundleKind::CheckpointSidecars;
        let committed_bundle = if sidecars_emitted.is_empty() {
            None
        } else {
            let bundle = self.journal_map_range_bundle(kind, bundle_start, &sidecars_emitted)?;
            if let Err(err) = self.commit_journal_bundle(&bundle) {
                self.poisoned = true;
                return Err(err);
            }
            Some(bundle)
        };
        self.commit_checkpointed_through()?;
        let next_tape_file_number = self.filemark_map.next_tape_file_number()?;
        Ok(CheckpointResult {
            next_tape_file_number,
            highest_protected_ordinal: self.highest_protected_ordinal,
            total_committed_ordinals: self.filemark_map.total_data_ordinals()?,
            sidecars_emitted,
            barrier_outcome,
            used_tape_blocks: self.last_physical_lba,
            is_terminal,
            committed_bundle,
        })
    }

    fn commit_checkpointed_through(&mut self) -> Result<(), ParityError> {
        let total_committed_ordinals = self.filemark_map.total_data_ordinals()?;
        self.commit_journal_bundle(&CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: self.highest_protected_ordinal,
            total_committed_ordinals,
        })
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
        start_tape_file_number: u64,
        sidecars: &[SidecarWriteSummary],
    ) -> Result<(), ParityError> {
        if self.journal.is_none() {
            return Ok(());
        }
        let bundle = self.journal_map_range_bundle(kind, start_tape_file_number, sidecars)?;
        self.commit_journal_bundle(&bundle)
    }

    fn journal_map_range_bundle(
        &self,
        kind: CommittedBundleKind,
        start_tape_file_number: u64,
        sidecars: &[SidecarWriteSummary],
    ) -> Result<CommittedBundle, ParityError> {
        let entries = self
            .filemark_map
            .session_entries_from(start_tape_file_number)?
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
        Ok(bundle)
    }

    fn control_tape_file_entry(
        &self,
        entry: &TapeFileMapEntry,
    ) -> Result<TapeFileEntry, ParityError> {
        let mut journal_entry = TapeFileEntry::from_map_entry(entry.clone());
        journal_entry.physical_start_hint = self.tape_file_start_lba(entry.tape_file_number);
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
        Ok(journal_entry)
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

    /// Return the sink-owned inputs for one object-start capacity decision.
    pub fn terminal_triple_capacity_runtime_state(
        &self,
    ) -> Result<TerminalTripleCapacityRuntimeState, ParityError> {
        terminal_triple_runtime_state_from_parts(
            self.data_blocks_in_neighborhood,
            &self.pending_sidecars,
            self.sidecar_directory_entries.len(),
            usize::try_from(self.filemark_map.tape_file_count()?)
                .map_err(|_| ParityError::Invariant("structural row count does not fit usize"))?,
            usize::try_from(self.committed_object_count)
                .map_err(|_| ParityError::Invariant("committed Object count does not fit usize"))?,
            self.last_physical_lba,
        )
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
        self.current_epoch_start = self
            .current_epoch_start
            .checked_add(self.data_blocks_in_neighborhood)
            .ok_or(ParityError::Invariant("epoch protected range overflows"))?;
        self.reset_parity_accumulators()?;
        self.neighborhood_idx = self
            .neighborhood_idx
            .checked_add(1)
            .ok_or(ParityError::Invariant("epoch id overflows"))?;
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

        self.queue_epoch_sidecar_from_accumulators(block_size, false, false)?;
        self.advance_to_next_epoch()?;
        Ok(())
    }

    fn queue_partial_sidecar_without_writing_padding(
        &mut self,
        block_size: usize,
        is_terminal: bool,
    ) -> Result<(), ParityError> {
        self.queue_epoch_sidecar_from_accumulators(block_size, true, is_terminal)?;
        self.advance_to_next_epoch()?;
        Ok(())
    }

    fn queue_epoch_sidecar_from_accumulators(
        &mut self,
        block_size: usize,
        allow_partial_epoch: bool,
        is_terminal: bool,
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
        self.queue_epoch_sidecar_with_parity_shards(
            parity_shards,
            block_size,
            allow_partial_epoch,
            is_terminal,
        )
    }

    fn queue_epoch_sidecar_with_parity_shards(
        &mut self,
        parity_shards: Vec<Vec<u8>>,
        block_size: usize,
        allow_partial_epoch: bool,
        is_terminal: bool,
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

        let start = self.current_epoch_start;
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
            is_terminal,
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
        self.note_tape_file_start(tape_file_number);

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
            if outcome.bytes_written as usize != block.len() {
                self.poisoned = true;
                return Err(self.abandon_tape_file_boundary_or(
                    TapeFileKind::ParitySidecar,
                    tape_file_number,
                    ParityError::Invariant(
                        "sidecar block write completed with a nonexact byte count",
                    ),
                ));
            }
            self.record_physical_position(outcome.position_after.lba);
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

        let filemark_outcome = match self.backend.write_filemarks(1, true) {
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
            final_partial_epoch: sidecar.is_terminal
                && encoded.header.real_data_shard_count < encoded.header.logical_shard_count,
            filemark_outcome,
            physical_start_lba: self.tape_file_start_lba(entry.tape_file_number),
        };
        self.sidecar_directory_entries
            .push(sidecar_summary_to_directory_entry(&summary));

        Ok(summary)
    }

    fn validate_terminal_triple_reservation(
        &self,
        reservation: &TerminalTripleObjectReservation,
    ) -> Result<(), ParityError> {
        let input = reservation.input();
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "capacity reserve requested while another object is active",
            ));
        }
        if input.block_size_bytes != self.block_size_bytes {
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
        let runtime = self.terminal_triple_capacity_runtime_state()?;
        if input.pending_completed_sidecars != runtime.pending_completed_sidecars {
            return Err(ParityError::Invariant(
                "capacity reserve pending sidecar count does not match ParitySink",
            ));
        }
        if input.sidecar_entries_before_object != runtime.sidecar_entries_before_object {
            return Err(ParityError::Invariant(
                "capacity reserve sidecar directory count does not match ParitySink",
            ));
        }
        if input.pending_completed_epoch_parity_bytes
            != runtime.pending_completed_epoch_parity_bytes
        {
            return Err(ParityError::Invariant(
                "terminal reservation pending parity bytes do not match ParitySink",
            ));
        }
        if input.structural_entries_before_object != runtime.structural_entries_before_object {
            return Err(ParityError::Invariant(
                "terminal reservation structural rows do not match ParitySink",
            ));
        }
        if input.object_rows_before_object != runtime.object_rows_before_object {
            return Err(ParityError::Invariant(
                "terminal reservation Object rows do not match ParitySink",
            ));
        }
        let expected_remaining = input
            .capacity_basis_blocks
            .checked_sub(runtime.used_tape_blocks)
            .ok_or(ParityError::Invariant(
                "terminal reservation physical cursor exceeds capacity basis",
            ))?;
        if input.remaining_tape_blocks != expected_remaining {
            return Err(ParityError::Invariant(
                "terminal reservation remaining capacity does not match ParitySink cursor",
            ));
        }
        Ok(())
    }

    fn start_object_after_reserve(
        &mut self,
        input: TerminalTripleCloseInput,
        report: TerminalTripleCloseReport,
    ) -> Result<u64, ParityError> {
        if self.active_object.is_some() {
            return Err(ParityError::Invariant(
                "begin_object called while another object is active",
            ));
        }
        let tape_file_number = self.filemark_map.next_tape_file_number()?;
        let pending_sidecars_at_start = u64::try_from(self.pending_sidecars.len())
            .map_err(|_| ParityError::Invariant("pending sidecar count does not fit u64"))?;
        let block_size_before_object = self.block_size;
        let early_warning_reserve_before_object = self.early_warning_reserve;
        self.durable_boundary
            .begin_tape_file(TapeFileKind::Object, tape_file_number)?;
        self.note_tape_file_start(tape_file_number);
        self.early_warning_reserve = Some(EarlyWarningReserveState::new(input, report));
        self.active_object = Some(ActiveObject {
            tape_file_number,
            projected_size_blocks: input.projected_object_blocks,
            pending_sidecars_at_start,
            pending_sidecar_limit: report.epochs_completed_by_object,
            written_blocks: 0,
            block_size_before_object,
            early_warning_reserve_before_object,
        });
        Ok(tape_file_number)
    }

    /// Roll back an Object that was admitted locally but never entered the
    /// raw write boundary. This is the owner recovery seam for source/read
    /// failures after admission: once any Object block has landed, rollback is
    /// forbidden and the session must be fenced instead.
    pub fn rollback_unwritten_object(&mut self) -> Result<(), ParityError> {
        let Some(object) = self.active_object else {
            return Ok(());
        };
        if object.written_blocks != 0
            || self.tape_file_start_lba(object.tape_file_number) != Some(self.last_physical_lba)
        {
            return Err(ParityError::Invariant(
                "cannot roll back an Object after raw tape motion",
            ));
        }
        self.durable_boundary
            .abandon_tape_file(TapeFileKind::Object, object.tape_file_number)?;
        self.tape_file_start_lbas.remove(&object.tape_file_number);
        self.block_size = object.block_size_before_object;
        self.early_warning_reserve = object.early_warning_reserve_before_object;
        self.active_object = None;
        Ok(())
    }

    fn record_success_and_check_early_warning_reserve(
        &mut self,
        event: EarlyWarningReserveEvent,
        early_warning: bool,
        end_of_medium: bool,
    ) -> Result<(), ParityError> {
        self.hardware_early_warning_seen |= early_warning;
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

    /// Capture the volume-global LBA of block 0 of a tape file that is about
    /// to receive its first write. At every file begin the physical cursor
    /// sits just past the previous file's trailing filemark (or at BOT on a
    /// fresh tape), which IS the new file's block 0. Dead-reckoned; the
    /// covering barrier's device-proved position transitively validates it.
    fn note_tape_file_start(&mut self, tape_file_number: u64) {
        self.tape_file_start_lbas
            .insert(tape_file_number, self.last_physical_lba);
    }

    /// Captured start LBA for a tape file begun by this logical session.
    /// Absent for prefix files committed before this session — absent, never
    /// guessed.
    fn tape_file_start_lba(&self, tape_file_number: u64) -> Option<u64> {
        self.tape_file_start_lbas.get(&tape_file_number).copied()
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
        let unprotected_after_bundle = total_committed_ordinals_after
            .checked_sub(self.highest_protected_ordinal)
            .ok_or(ParityError::Invariant(
                "object commit bundle watermark exceeds committed ordinals",
            ))?;
        let open_epoch_len = total_committed_ordinals_after
            .checked_sub(self.current_epoch_start)
            .ok_or(ParityError::Invariant(
                "object commit precedes current open epoch start",
            ))?;
        debug_assert_eq!(unprotected_after_bundle, open_epoch_len);
        if open_epoch_len >= self.epoch_data_shards()? {
            return Err(ParityError::Invariant(
                "object commit bundle violates v1 bounded restart invariant",
            ));
        }
        Ok(())
    }

    fn commit_tape_file_boundary(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u64,
    ) -> Result<(), ParityError> {
        self.durable_boundary
            .commit_tape_file(kind, tape_file_number)
    }

    fn abandon_tape_file_boundary_or(
        &mut self,
        kind: TapeFileKind,
        tape_file_number: u64,
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
                if e.is_completion_unknown()
                    || matches!(e, TapeIoError::PartialBatchUncommittable { .. })
                {
                    self.poisoned = true;
                    return Err(self.abandon_active_object_boundary_or_tape_io(e));
                }
                return Err(e);
            }
        };
        if data_outcome.bytes_written as usize != buf.len() {
            self.poisoned = true;
            return Err(
                self.abandon_active_object_boundary_or_tape_io(invalid_input(
                    "ParitySink: object data block write completed with a nonexact byte count before trailing filemark",
                )),
            );
        }
        self.record_physical_position(data_outcome.position_after.lba);
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
