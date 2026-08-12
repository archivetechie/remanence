//! Pool-write requests, results, resources, policies, and errors.

use std::cell::Cell;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use remanence_aead::RecipientPublicKey;
use remanence_library::{TapeConfig, TapeIoError, WormMediaState};
use remanence_parity::{
    FileTapeFileJournal, ParityConfig, ParityError, PhysicalPositionHint, RawTapeSink,
    RawWriteOutcome, TapeFileJournal, TerminalComponentCommit, TerminalComponentReconcileEvidence,
    TerminalTailAuthority, TerminalTailProgress, TerminalTripleObjectReservation,
};
use remanence_state::StateError;
use remanence_stream::{StreamingError, StreamingObjectWriteReport};
use thiserror::Error;
use uuid::Uuid;

use super::capacity::{sha256_file, source_file_size};
use super::NO_PARITY_BOOTSTRAP_BLOCKS;
use crate::{append_mode_for_tape_file_number, bytes_to_hex, pb, timestamp_from_rfc3339};

#[derive(Debug)]
pub(crate) struct ParityCapacityReservation {
    pub(super) reservation: TerminalTripleObjectReservation,
    pub(super) _spool_permit: crate::io_memory::IoMemoryPermit,
}

impl ParityCapacityReservation {
    #[cfg(test)]
    pub(super) fn report(&self) -> &remanence_parity::TerminalTripleCloseReport {
        self.reservation.report()
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        TerminalTripleObjectReservation,
        crate::io_memory::IoMemoryPermit,
    ) {
        (self.reservation, self._spool_permit)
    }
}

/// Process-local resources shared by direct checkpointed pool writes.
///
/// Callers construct one handle from the configured I/O-memory ceiling and
/// reuse it across concurrent direct writes. Daemon-owned writes use the same
/// reservation primitive through their long-lived owner configuration.
#[derive(Clone, Debug)]
pub struct PoolWriteResources {
    pub(super) io_memory: Arc<crate::io_memory::IoMemoryReservation>,
    pub(super) write_admissions: crate::write_admission::WriteAdmissionCoordinator,
}

pub(super) fn direct_write_admissions() -> crate::write_admission::WriteAdmissionCoordinator {
    static COORDINATOR: OnceLock<crate::write_admission::WriteAdmissionCoordinator> =
        OnceLock::new();
    COORDINATOR.get_or_init(Default::default).clone()
}

impl PoolWriteResources {
    /// Create a shared resource handle using the configured byte ceiling.
    pub fn new(io_memory_ceiling_bytes: u64) -> Result<Self, String> {
        Ok(Self {
            io_memory: crate::io_memory::IoMemoryReservation::new(io_memory_ceiling_bytes)?,
            write_admissions: direct_write_admissions(),
        })
    }

    pub(super) fn io_memory(&self) -> &Arc<crate::io_memory::IoMemoryReservation> {
        &self.io_memory
    }
}

/// Marks entry into the raw write boundary while allowing position-only
/// validation to remain recoverable. The owner uses this distinction to
/// restore a detached session after local/source failure, but fence it once a
/// transport write may have changed media.
pub(super) struct CapacityTrackingRawTapeSink<'a> {
    inner: &'a mut dyn RawTapeSink,
    write_attempted: &'a Cell<bool>,
    position_ready: bool,
}

impl<'a> CapacityTrackingRawTapeSink<'a> {
    pub(super) fn new(inner: &'a mut dyn RawTapeSink, write_attempted: &'a Cell<bool>) -> Self {
        Self {
            inner,
            write_attempted,
            position_ready: false,
        }
    }

    fn ensure_position_ready(&mut self) -> Result<(), ParityError> {
        if !self.position_ready {
            self.inner.position()?;
            self.position_ready = true;
        }
        Ok(())
    }
}

impl RawTapeSink for CapacityTrackingRawTapeSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.ensure_position_ready()?;
        self.write_attempted.set(true);
        let result = self.inner.write_fixed_block(buf);
        if result.is_err() {
            self.position_ready = false;
        }
        result
    }

    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError> {
        self.ensure_position_ready()?;
        self.write_attempted.set(true);
        let result = self.inner.write_filemarks(count, immed);
        if result.is_err() {
            self.position_ready = false;
        }
        result
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position = self.inner.position()?;
        self.position_ready = true;
        Ok(position)
    }
}

/// Durable authority for the direct writer's uninterrupted terminal tail.
///
/// The direct sink has no raw read side, so this authority can only attest the
/// next component as absent while continuing from the cursor proved by the
/// immediately preceding checkpoint/prefix/component barrier. A pending
/// intent encountered by a later invocation is rejected by ordinary
/// checkpoint replay and must be reconciled by a source-capable owner.
/// Terminal admission is irreversible: recovery may complete missing tail
/// components, but it must never resume Object writes on this tape.
pub(super) struct DirectSequentialTerminalAuthority<'a> {
    pub(super) checkpoint: &'a mut remanence_state::FileCheckpointJournalLease,
    pub(super) parity_journal: Option<&'a mut FileTapeFileJournal>,
    pub(super) intent: remanence_state::TerminalFinalizationIntent,
    pub(super) cursor_proved_for: TerminalTailProgress,
}

impl TerminalTailAuthority for DirectSequentialTerminalAuthority<'_> {
    fn load_progress(&mut self) -> Result<TerminalTailProgress, String> {
        Ok(parity_progress_from_state(self.intent.progress))
    }

    fn reconcile_next(
        &mut self,
        progress: TerminalTailProgress,
        _component: remanence_parity::TerminalTailComponentPlan,
    ) -> Result<TerminalComponentReconcileEvidence, String> {
        if progress != self.cursor_proved_for || progress != self.load_progress()? {
            return Err(
                "direct terminal writer lost its uninterrupted barrier-proved cursor authority"
                    .to_string(),
            );
        }
        Ok(TerminalComponentReconcileEvidence::Absent)
    }

    fn commit_after_barrier(&mut self, commit: &TerminalComponentCommit) -> Result<(), String> {
        if commit.previous_progress != self.cursor_proved_for {
            return Err(
                "direct terminal component did not continue from the proved cursor".to_string(),
            );
        }
        if let Some(journal) = self.parity_journal.as_deref_mut() {
            journal
                .commit_terminal_component_transition(
                    &commit.journal_bundle,
                    &commit.checkpoint_bundle,
                )
                .map_err(|error| format!("commit terminal component journal: {error}"))?;
        }
        self.intent = self
            .checkpoint
            .advance_terminal_finalization(
                state_progress_from_parity(commit.previous_progress),
                state_progress_from_parity(commit.next_progress),
            )
            .map_err(|error| format!("advance terminal checkpoint progress: {error}"))?;
        self.cursor_proved_for = commit.next_progress;
        Ok(())
    }
}

const fn parity_progress_from_state(
    progress: remanence_state::TerminalFinalizationProgress,
) -> TerminalTailProgress {
    use remanence_state::TerminalFinalizationProgress as State;
    match progress {
        State::BeforeReplicaA => TerminalTailProgress::BeforeReplicaA,
        State::AfterReplicaA => TerminalTailProgress::AfterReplicaA,
        State::AfterSeparationAb => TerminalTailProgress::AfterSeparationAb,
        State::AfterReplicaB => TerminalTailProgress::AfterReplicaB,
        State::AfterSeparationBc => TerminalTailProgress::AfterSeparationBc,
        State::AfterReplicaC => TerminalTailProgress::AfterReplicaC,
    }
}

const fn state_progress_from_parity(
    progress: TerminalTailProgress,
) -> remanence_state::TerminalFinalizationProgress {
    use remanence_state::TerminalFinalizationProgress as State;
    match progress {
        TerminalTailProgress::BeforeReplicaA => State::BeforeReplicaA,
        TerminalTailProgress::AfterReplicaA => State::AfterReplicaA,
        TerminalTailProgress::AfterSeparationAb => State::AfterSeparationAb,
        TerminalTailProgress::AfterReplicaB => State::AfterReplicaB,
        TerminalTailProgress::AfterSeparationBc => State::AfterSeparationBc,
        TerminalTailProgress::AfterReplicaC => State::AfterReplicaC,
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FAIL_PARITY_POST_WRITE_PROJECTION: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
pub(super) fn parity_post_write_projection_gate() -> Result<(), PoolWriteError> {
    if FAIL_PARITY_POST_WRITE_PROJECTION.with(|flag| flag.replace(false)) {
        Err(PoolWriteError::InvalidInput(
            "injected post-write parity projection failure".to_string(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
pub(super) fn parity_post_write_projection_gate() -> Result<(), PoolWriteError> {
    Ok(())
}

/// Binary UUID used for physical tape identifiers and object identifiers.
pub type TapeUuid = [u8; 16];

/// Stored REM-OBJECT representation requested for a pool write.
#[derive(Clone, Debug)]
pub enum PoolWriteRepresentation {
    /// Store the canonical plaintext `rem-object-v1` tar/PAX object.
    Plaintext,
    /// Store the REMO encrypted representation.
    Encrypted {
        /// Public recipient epochs to which the per-object DEK is wrapped.
        recipients: Vec<RecipientPublicKey>,
    },
}

/// Interpretation of the bytes supplied for one pool write.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WriteObjectInputKind {
    /// One logical regular file which the daemon wraps as a REM object.
    #[default]
    LogicalFile,
    /// An already-built canonical plaintext REM object written byte-for-byte.
    CanonicalPlaintextRemObject,
}

/// Live plaintext source metadata and reader supplied by the append RPC.
pub struct StreamedWriteSource {
    pub(super) reader: Arc<Mutex<Box<dyn Read + Send>>>,
    pub(super) size_bytes: u64,
    pub(super) content_sha256: [u8; 32],
    pub(super) control: Arc<crate::append_ring::AppendRingControl>,
}

impl fmt::Debug for StreamedWriteSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamedWriteSource")
            .field("size_bytes", &self.size_bytes)
            .field("content_sha256", &bytes_to_hex(&self.content_sha256))
            .finish_non_exhaustive()
    }
}

impl StreamedWriteSource {
    pub(crate) fn new(
        reader: impl Read + Send + 'static,
        size_bytes: u64,
        content_sha256: [u8; 32],
        control: Arc<crate::append_ring::AppendRingControl>,
    ) -> Self {
        Self {
            reader: Arc::new(Mutex::new(Box::new(reader))),
            size_bytes,
            content_sha256,
            control,
        }
    }

    #[cfg(test)]
    pub(crate) fn read_all_for_test(&self) -> std::io::Result<Vec<u8>> {
        let mut reader = self
            .reader
            .lock()
            .map_err(|_| std::io::Error::other("streamed test reader lock poisoned"))?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

/// Payload source for one pool write.
#[derive(Debug)]
pub enum WriteObjectSource {
    /// Completed local file used by the legacy serial and in-process paths.
    Path(PathBuf),
    /// Live bounded-ring consumer used only by plaintext overlap appends.
    Streamed(StreamedWriteSource),
}

impl WriteObjectSource {
    pub(crate) fn size_bytes(&self) -> Result<u64, PoolWriteError> {
        match self {
            Self::Path(path) => source_file_size(path),
            Self::Streamed(source) => Ok(source.size_bytes),
        }
    }

    pub(crate) fn content_sha256(&self) -> Result<[u8; 32], PoolWriteError> {
        match self {
            Self::Path(path) => sha256_file(path),
            Self::Streamed(source) => Ok(source.content_sha256),
        }
    }

    pub(crate) fn stream_control(&self) -> Option<Arc<crate::append_ring::AppendRingControl>> {
        match self {
            Self::Path(_) => None,
            Self::Streamed(source) => Some(Arc::clone(&source.control)),
        }
    }

    pub(crate) fn remove_completed_path(&self) {
        if let Self::Path(path) = self {
            let _ = fs::remove_file(path);
        }
    }
}

/// Inputs for writing one regular file as one `rem-object-v1` object to a pool.
#[derive(Debug)]
pub struct WriteObjectToPoolRequest {
    /// Pool requested by the caller.
    pub pool_id: String,
    /// Completed path or live bounded-ring consumer.
    pub source: WriteObjectSource,
    /// UTF-8 relative path to record inside the `rem-object-v1` object.
    pub archive_path: PathBuf,
    /// Opaque caller/orchestrator object id.
    pub caller_object_id: String,
    /// Optional caller-supplied payload SHA-256 that must match before tape I/O.
    pub expected_content_sha256: Option<[u8; 32]>,
    /// Optional embedded object UUID guard for canonical-object ingestion.
    pub expected_object_id: Option<[u8; 16]>,
    /// Stored representation to write to tape.
    pub representation: PoolWriteRepresentation,
    /// Whether `source` is a logical file or a complete canonical object.
    pub input_kind: WriteObjectInputKind,
}

/// One object record returned by the reusable pool-targeted write core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolWriteObjectRecord {
    /// Remanence-assigned object UUID bytes.
    pub object_id: [u8; 16],
    /// Opaque caller/orchestrator object id.
    pub caller_object_id: String,
    /// SHA-256 of the source payload bytes.
    pub content_sha256: [u8; 32],
    /// Logical payload bytes, excluding generated `rem-object-v1` metadata.
    pub logical_size_bytes: u64,
    /// Body format id.
    pub body_format: String,
    /// RFC3339 UTC creation timestamp.
    pub created_at_utc: String,
    /// Concrete copy locators written for this object.
    pub copies: Vec<PoolWriteObjectCopyRecord>,
}

impl PoolWriteObjectRecord {
    /// Convert to the generated Layer 5 protobuf `ObjectRecord`.
    pub fn to_proto(&self) -> pb::ObjectRecord {
        let append_commit_info = self.copies.first().map(append_commit_info_from_pool_copy);
        pb::ObjectRecord {
            object_id: self.object_id.to_vec(),
            caller_object_id: Some(self.caller_object_id.clone()),
            content_sha256: Some(self.content_sha256.to_vec()),
            logical_size_bytes: Some(self.logical_size_bytes),
            body_format: Some(self.body_format.clone()),
            caller_metadata: Default::default(),
            created_at: timestamp_from_rfc3339(self.created_at_utc.as_str()),
            copies: self
                .copies
                .iter()
                .map(PoolWriteObjectCopyRecord::to_proto)
                .collect(),
            append_commit_info,
            content_digest: Some(pb::Digest {
                algorithm: remanence_state::DIGEST_ALGORITHM_SHA256.to_string(),
                value: self.content_sha256.to_vec(),
            }),
            metadata_digest: None,
        }
    }

    /// Return the object id as canonical UUID text for catalog lookups.
    pub fn object_id_text(&self) -> String {
        Uuid::from_bytes(self.object_id).to_string()
    }

    /// Convert a provisional object into a locator-free WRITTEN acknowledgement.
    pub(crate) fn to_written_proto(
        &self,
        batch_id: Uuid,
        provisional_ordinal: u64,
    ) -> pb::ObjectRecord {
        let mut record = self.to_proto();
        record.copies.clear();
        record.append_commit_info = Some(pb::AppendCommitInfo {
            append_mode: pb::AppendMode::Unspecified as i32,
            tape_uuid: Vec::new(),
            voltag: None,
            tape_file_number: None,
            first_body_lba: 0,
            position_before_lba: None,
            position_after_lba: None,
            journal_record_ordinal: None,
            estimated_remaining_bytes: None,
            sealed_after_write: None,
            durability: pb::AppendDurability::Written as i32,
            batch_id: batch_id.as_bytes().to_vec(),
            provisional_ordinal: Some(provisional_ordinal),
        });
        record
    }
}

/// Copy locator returned by the reusable pool-targeted write core.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolWriteObjectCopyRecord {
    /// Actual tape selected inside the requested pool.
    pub tape_uuid: TapeUuid,
    /// Filemark-delimited object tape-file number.
    pub tape_file_number: u64,
    /// First object-local body LBA containing payload data.
    pub first_body_lba: u64,
    /// Pool requested and snapshotted for the write.
    pub pool_id: String,
    /// Stored REM-OBJECT representation: `plaintext` or `encrypted`.
    pub representation: String,
    /// Lowercase 32-hex recipient epoch ids for encrypted copies.
    pub recipient_epoch_ids: Option<Vec<String>>,
    /// Encrypted REM-OBJECT metadata frame length.
    pub metadata_frame_len: Option<u64>,
    /// SHA-256 of the canonical plaintext REM-OBJECT object bytes.
    pub plaintext_digest: Option<[u8; 32]>,
    /// SHA-256 of the stored representation bytes.
    pub stored_digest: Option<[u8; 32]>,
}

impl PoolWriteObjectCopyRecord {
    fn to_proto(&self) -> pb::ObjectCopy {
        pb::ObjectCopy {
            tape_uuid: self.tape_uuid.to_vec(),
            tape_file_number: self.tape_file_number,
            first_body_lba: self.first_body_lba,
            last_verified_at: None,
            health: pb::object_copy::Health::ObjectCopyHealthOk as i32,
            pool_id: self.pool_id.clone(),
            plaintext_digest: self.plaintext_digest.map(|value| pb::Digest {
                algorithm: remanence_state::DIGEST_ALGORITHM_SHA256.to_string(),
                value: value.to_vec(),
            }),
            stored_digest: self.stored_digest.map(|value| pb::Digest {
                algorithm: remanence_state::DIGEST_ALGORITHM_SHA256.to_string(),
                value: value.to_vec(),
            }),
            // The write acknowledgement does not consult the copy→tape-file
            // join; the span is served by the catalog read APIs. Absent =
            // unknown here, honestly, never a guessed zero.
            global_start_block: None,
            global_end_block: None,
        }
    }
}

pub(super) fn append_commit_info_from_pool_copy(
    copy: &PoolWriteObjectCopyRecord,
) -> pb::AppendCommitInfo {
    pb::AppendCommitInfo {
        append_mode: append_mode_for_tape_file_number(copy.tape_file_number) as i32,
        tape_uuid: copy.tape_uuid.to_vec(),
        voltag: None,
        tape_file_number: Some(copy.tape_file_number),
        first_body_lba: copy.first_body_lba,
        position_before_lba: None,
        position_after_lba: None,
        journal_record_ordinal: None,
        estimated_remaining_bytes: None,
        sealed_after_write: None,
        durability: pb::AppendDurability::Checkpointed as i32,
        batch_id: Vec::new(),
        provisional_ordinal: None,
    }
}

/// Full report returned by `write_object_to_pool`.
#[derive(Debug)]
pub struct PoolWriteResult {
    /// Locator/object record for the caller.
    pub object: PoolWriteObjectRecord,
    /// Lower-layer streaming write report when this call performed a new tape write.
    ///
    /// A caller-object replay returns the already committed object and leaves
    /// this empty because no tape transfer happened in that call.
    pub write_report: Option<StreamingObjectWriteReport>,
    pub(super) append_commit_diagnostics: AppendCommitDiagnostics,
    pub(super) sealed_after_write: bool,
    pub(super) checkpoint_projection: Option<remanence_state::CheckpointObjectProjection>,
    pub(super) post_write_used_bytes: u64,
    pub(super) hardware_early_warning: bool,
    pub(super) input_kind: WriteObjectInputKind,
}

impl PoolWriteResult {
    /// True when this result was returned from the catalog replay path.
    pub fn is_replay(&self) -> bool {
        self.write_report.is_none()
    }

    /// Whether this commit crossed a sealing threshold and closed the tape.
    pub fn sealed_after_write(&self) -> bool {
        self.sealed_after_write
    }

    /// Borrow the streaming report for callers that require proof of a new write.
    pub fn write_report(&self) -> Option<&StreamingObjectWriteReport> {
        self.write_report.as_ref()
    }

    pub(crate) fn append_commit_diagnostics(&self) -> AppendCommitDiagnostics {
        self.append_commit_diagnostics
    }

    /// Borrow the replayable projection held until a batched barrier.
    pub(crate) fn checkpoint_projection(
        &self,
    ) -> Option<&remanence_state::CheckpointObjectProjection> {
        self.checkpoint_projection.as_ref()
    }

    pub(crate) fn post_write_used_bytes(&self) -> u64 {
        self.post_write_used_bytes
    }

    pub(crate) fn hardware_early_warning(&self) -> bool {
        self.hardware_early_warning
    }

    pub(crate) fn input_kind(&self) -> WriteObjectInputKind {
        self.input_kind
    }

    /// Borrow the streaming report and panic if the result was a replay.
    #[cfg(test)]
    pub fn expect_write_report(&self) -> &StreamingObjectWriteReport {
        self.write_report
            .as_ref()
            .expect("pool write result should include a new write report")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AppendCommitDiagnostics {
    pub(crate) filemark_write_drain: Duration,
    pub(crate) catalog_journal_fsync: Duration,
}

impl AppendCommitDiagnostics {
    pub(crate) fn accumulate(&mut self, other: Self) {
        self.filemark_write_drain = self
            .filemark_write_drain
            .saturating_add(other.filemark_write_drain);
        self.catalog_journal_fsync = self
            .catalog_journal_fsync
            .saturating_add(other.catalog_journal_fsync);
    }
}

/// Canonical pool selection returned by the Phase 1 tape selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedTape {
    /// Normalized pool id resolved through the catalog.
    pub(crate) pool_id: String,
    /// Unique eligible tape selected inside the pool.
    pub(crate) tape_uuid: TapeUuid,
    /// Fixed block size recorded for the selected tape.
    pub(crate) block_size: u32,
    /// Parity configuration recorded for the selected tape.
    pub(crate) parity_config: ParityConfig,
}

impl SelectedTape {
    /// Return the catalog-normalized pool id that admitted this selection.
    pub fn pool_id(&self) -> &str {
        &self.pool_id
    }

    /// Return the selected tape UUID.
    pub fn tape_uuid(&self) -> TapeUuid {
        self.tape_uuid
    }

    /// Return the selected fixed block size.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Return the selected parity geometry.
    pub fn parity_config(&self) -> &ParityConfig {
        &self.parity_config
    }
}

/// Result of pinned-tape admission before a write session resolves media actors.
///
/// The recovery-only variant deliberately remains distinct from an ordinarily
/// writable selection so an `AfterReplicaC` companion, or a sealed checkpoint
/// whose companion was already retired, can reach host preflight without ever
/// becoming authority to mount or write the tape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinnedWriteDisposition {
    /// The tape passed ordinary Object-write admission.
    Writable(SelectedTape),
    /// The tape may be used only to finish terminal host bookkeeping.
    HostOnlyTerminalRecovery(SelectedTape),
}

/// Hard writability precondition failure for one tape candidate.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WritabilityError {
    /// The tape is not in the provisioned write-ready state.
    #[error("tape is not ready for writing: state={state:?}")]
    NotReady {
        /// Observed `tapes.state` value.
        state: String,
    },
    /// The catalog row lacks the geometry needed for a write decision.
    #[error("tape is missing write geometry: {reason}")]
    MissingGeometry {
        /// Human-readable missing or inconsistent field.
        reason: String,
    },
    /// The object does not fit in the tape's remaining raw capacity.
    #[error(
        "insufficient tape capacity: object_size={object_size}, raw_capacity={raw_capacity}, used={used}"
    )]
    InsufficientCapacity {
        /// Candidate object size in bytes.
        object_size: u64,
        /// Raw LTO cartridge capacity in bytes.
        raw_capacity: u64,
        /// Catalog-accounted used capacity in bytes.
        used: u64,
    },
    /// The tape's fixed block size does not match the pool's configured block size.
    #[error(
        "tape block size {tape_block_size} does not match pool configured block size {pool_block_size}"
    )]
    BlockSizeMismatch {
        /// Fixed block size recorded on the tape row.
        tape_block_size: u64,
        /// Fixed block size configured for the pool.
        pool_block_size: u64,
    },
    /// The current parity writer opens at BOT; true append is not implemented yet.
    #[error(
        "parity tape already has committed contents; true append is not implemented: total_committed_ordinals={total_committed_ordinals}"
    )]
    ParityAppendUnsupported {
        /// Catalog-accounted committed ordinals already present on the tape.
        total_committed_ordinals: u64,
    },
    /// The tape has an active tape-I/O quarantine fence.
    #[error("active tape-I/O fence {quarantine_id}: {reason}")]
    TapeIoFence {
        /// Operator-facing quarantine id.
        quarantine_id: String,
        /// Fence reason.
        reason: String,
    },
}

/// Drive-reported media state that cannot support ordinary Object ingest.
///
/// Whole-Object restart recovery may replace an uncommitted tail. Callers
/// must obtain this positive rewritable-media check before MODE SELECT or any
/// Object write; append-only terminal finalization uses a separate policy.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ObjectWriteMediaError {
    /// The loaded cartridge reports its write-protect switch active.
    #[error("tape is write-protected")]
    WriteProtected,
    /// The loaded cartridge is positively identified as WORM.
    #[error(
        "ordinary Object writes require rewritable media; a WORM tape cannot replace an interrupted uncommitted Object tail"
    )]
    Worm,
    /// The drive did not provide recognized rewritable/WORM media evidence.
    #[error(
        "ordinary Object writes require media positively identified as rewritable; the loaded tape's WORM state is unknown"
    )]
    UnknownWormState,
}

/// Require positive drive evidence that ordinary Object ingest can replace a
/// torn, uncommitted tail after restart.
pub(crate) fn require_rewritable_object_media(
    current_cfg: TapeConfig,
) -> Result<(), ObjectWriteMediaError> {
    if current_cfg.write_protected {
        return Err(ObjectWriteMediaError::WriteProtected);
    }
    match current_cfg.worm {
        WormMediaState::NotWorm => Ok(()),
        WormMediaState::Worm => Err(ObjectWriteMediaError::Worm),
        WormMediaState::Unknown => Err(ObjectWriteMediaError::UnknownWormState),
    }
}

/// Reason an active tape should be sealed after a write boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapeSealReason {
    /// Actual post-write position reached or crossed the low watermark.
    ReachedLowWatermark,
    /// Hardware reported early warning before the software low watermark.
    HardwareEarlyWarning,
    /// Operator explicitly closed the tape.
    OperatorCloseOut,
    /// Operator explicitly closed all active tapes in the pool.
    PoolCloseOut,
    /// Scheduler/operator stated that no pending object fits this tape.
    NoPendingObjectFits,
}

/// Actual post-write position facts used for eager sealing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapePositionAfterWrite {
    /// Bytes actually consumed on tape after the object commit.
    pub used_bytes: u64,
    /// Whether hardware early-warning fired while committing the object.
    pub early_warning: bool,
}

/// Decide whether a tape should be sealed from actual post-write state.
///
/// Projection influences selection only. Sealing is triggered by actual
/// position, hardware early-warning, or an explicit close-out valve.
pub fn seal_decision_after_write(
    position: TapePositionAfterWrite,
    low_bytes: u64,
    force_reason: Option<TapeSealReason>,
) -> Option<TapeSealReason> {
    if position.early_warning {
        Some(TapeSealReason::HardwareEarlyWarning)
    } else if position.used_bytes >= low_bytes {
        Some(TapeSealReason::ReachedLowWatermark)
    } else {
        force_reason
    }
}

/// Errors from the placeholder pool tape selector.
#[derive(Debug, Error)]
pub enum SelectTapeError {
    /// No configured pool row exists for the requested id.
    #[error("unknown tape pool: {pool_id}")]
    UnknownPool {
        /// Requested pool id.
        pool_id: String,
    },
    /// The pool exists, but no tape is eligible for this Phase 1 writer.
    #[error("pool {pool_id} has no eligible tapes")]
    EmptyPool {
        /// Requested pool id.
        pool_id: String,
    },
    /// The pool contains tapes, but all fail hard writability preconditions.
    #[error("pool {pool_id} has no writable tapes ({reasons_len} rejection(s))", reasons_len = reasons.len())]
    NoWritableTapes {
        /// Requested pool id.
        pool_id: String,
        /// Per-candidate hard precondition failures.
        reasons: Vec<WritabilityError>,
    },
    /// Every otherwise-writable tape is reserved by another live write session.
    #[error(
        "pool {pool_id} has no unreserved writable tapes ({reserved_tape_count} reserved by live write session(s))"
    )]
    NoUnreservedWritableTapes {
        /// Requested pool id.
        pool_id: String,
        /// Candidate tapes excluded by the live-session reservation filter.
        reserved_tape_count: usize,
    },
    /// Every otherwise-writable candidate is unsafe for batched append.
    #[error(
        "pool {pool_id} has no checkpoint-eligible tapes; journal-less non-fresh candidates are accept-sealed and must be re-initialized before reuse: {ineligible_candidates:?}"
    )]
    NoBatchedEligibleTapes {
        /// Requested pool id.
        pool_id: String,
        /// Barcodes and UUIDs of candidates requiring checkpoint adoption.
        ineligible_candidates: Vec<String>,
    },
    /// More than one eligible tape exists; policy must choose later.
    #[error(
        "pool {pool_id} has {eligible_tape_count} eligible tapes; selection policy is not wired"
    )]
    AmbiguousNeedsPolicy {
        /// Requested pool id.
        pool_id: String,
        /// Number of eligible tapes found.
        eligible_tape_count: usize,
    },
    /// A selected tape row did not carry a 16-byte UUID.
    #[error("pool {pool_id} contains tape UUID with {actual_len} bytes")]
    InvalidTapeUuid {
        /// Requested pool id.
        pool_id: String,
        /// Actual byte length observed.
        actual_len: usize,
    },
    /// A selected tape row is missing or has invalid write geometry.
    #[error("pool {pool_id} contains tape with invalid write geometry: {reason}")]
    InvalidTapeGeometry {
        /// Requested pool id.
        pool_id: String,
        /// Human-readable geometry problem.
        reason: String,
    },
    /// Layer 4 state query failed.
    #[error(transparent)]
    State(#[from] StateError),
}

/// Errors from the reusable pool-targeted write core.
#[derive(Debug, Error)]
pub enum PoolWriteError {
    /// Tape selection failed before the write opened.
    #[error(transparent)]
    Select(#[from] SelectTapeError),
    /// Layer 4 state projection failed.
    #[error(transparent)]
    State(#[from] StateError),
    /// Layer 5 write-core input is invalid.
    #[error("invalid pool write input: {0}")]
    InvalidInput(String),
    /// The selected tape is missing required write geometry.
    #[error("selected tape is missing write geometry: {0}")]
    MissingTapeGeometry(String),
    /// Converting the device-proved physical cursor to bytes overflowed.
    #[error(
        "physical used-byte count overflows u64: position_lba={position_lba}, block_size={block_size}"
    )]
    PhysicalUsedBytesOverflow {
        /// Barrier-proved physical cursor in fixed blocks.
        position_lba: u64,
        /// Selected fixed block size in bytes.
        block_size: u32,
    },
    /// The selected parity tape already contains committed contents.
    #[error(
        "selected parity tape {tape_uuid} already has committed contents; true append is not implemented: total_committed_ordinals={total_committed_ordinals}"
    )]
    ParityAppendUnsupported {
        /// Selected tape UUID as canonical text.
        tape_uuid: String,
        /// Catalog-accounted committed ordinals already present on the tape.
        total_committed_ordinals: u64,
    },
    /// The exact prepared representation cannot fit on the selected tape.
    #[error(
        "selected tape has insufficient capacity: object_size={object_size}, raw_capacity={raw_capacity}, used={used}"
    )]
    SelectedTapeInsufficientCapacity {
        /// Prepared stored object size in bytes.
        object_size: u64,
        /// Raw LTO cartridge capacity in bytes.
        raw_capacity: u64,
        /// Catalog-accounted used capacity in bytes.
        used: u64,
    },
    /// The proposed whole Object cannot fit while preserving the immutable
    /// terminal close reserve, so the current durable prefix must close and
    /// placement must retry on another tape.
    #[error(
        "current tape prefix must be terminally finalized before this Object can be placed: {detail}"
    )]
    TerminalCloseRequired {
        /// Exact close-admission diagnostic.
        detail: String,
    },
    /// The prepared source payload hash did not match the caller-supplied guard.
    #[error("content SHA-256 mismatch: expected {expected}, actual {actual}")]
    ContentHashMismatch {
        /// Expected SHA-256 as lowercase hex.
        expected: String,
        /// Actual prepared payload SHA-256 as lowercase hex.
        actual: String,
    },
    /// A caller-object replay found the key bound to different content.
    #[error(
        "caller_object_id replay conflict in pool {pool_id}: caller_object_id={caller_object_id:?}, existing content_sha256={existing_content_sha256}, requested content_sha256={requested_content_sha256}"
    )]
    CallerObjectIdConflict {
        /// Pool that scopes the idempotency key.
        pool_id: String,
        /// Opaque caller/orchestrator object id.
        caller_object_id: String,
        /// Existing committed content SHA-256 as lowercase hex.
        existing_content_sha256: String,
        /// Requested source content SHA-256 as lowercase hex.
        requested_content_sha256: String,
    },
    /// A caller-object replay changed the ingestion semantics bound by the
    /// original request.
    #[error(
        "caller_object_id replay changed input kind in pool {pool_id}: caller_object_id={caller_object_id:?}, existing={existing_input_kind:?}, requested={requested_input_kind:?}"
    )]
    CallerObjectIdInputKindConflict {
        /// Pool that scopes the idempotency key.
        pool_id: String,
        /// Opaque caller/orchestrator object id.
        caller_object_id: String,
        /// Input kind inferred from the committed object's exact member/hash
        /// projection.
        existing_input_kind: WriteObjectInputKind,
        /// Input kind supplied by the retry.
        requested_input_kind: WriteObjectInputKind,
    },
    /// A logical-file replay changed the member path bound by the original
    /// request.
    #[error(
        "caller_object_id replay changed archive path in pool {pool_id}: caller_object_id={caller_object_id:?}, existing={existing_archive_path:?}, requested={requested_archive_path:?}"
    )]
    CallerObjectIdArchivePathConflict {
        /// Pool that scopes the idempotency key.
        pool_id: String,
        /// Opaque caller/orchestrator object id.
        caller_object_id: String,
        /// Path projected for the committed logical file.
        existing_archive_path: String,
        /// Path supplied by the retry.
        requested_archive_path: String,
    },
    /// A replay changed the stored copy representation or encrypted recipient
    /// epochs bound by the original request.
    #[error(
        "caller_object_id replay changed stored representation in pool {pool_id}: caller_object_id={caller_object_id:?}, existing={existing_representations:?}, requested={requested_representation} recipients={requested_recipient_epoch_ids:?}"
    )]
    CallerObjectIdRepresentationConflict {
        /// Pool that scopes the idempotency key.
        pool_id: String,
        /// Opaque caller/orchestrator object id.
        caller_object_id: String,
        /// Committed representation/recipient summaries in this pool.
        existing_representations: Vec<String>,
        /// Representation supplied by the retry.
        requested_representation: &'static str,
        /// Ordered recipient epoch ids supplied by an encrypted retry.
        requested_recipient_epoch_ids: Option<Vec<String>>,
    },
    /// A replay candidate was found but lacks fields required for a response.
    #[error("catalog replay object {object_id} is incomplete: {reason}")]
    ReplayObjectInvalid {
        /// Existing object id.
        object_id: String,
        /// Missing or malformed field description.
        reason: String,
    },
    /// Filesystem I/O failed at the named path.
    #[error("{context} at {}: {source}", path.display())]
    Io {
        /// Operation being performed.
        context: &'static str,
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// Layer 3b/3c streaming orchestration failed.
    #[error(transparent)]
    Streaming(#[from] StreamingError),
    /// Layer 3c parity failed outside the streaming helper.
    #[error(transparent)]
    Parity(#[from] remanence_parity::ParityError),
    /// Block sink I/O failed outside the parity wrapper.
    #[error(transparent)]
    TapeIo(#[from] TapeIoError),
    /// Another direct writer owns the same replay key or canonical Object UUID.
    #[error("write identity admission conflict: {0}")]
    WriteAdmissionConflict(String),
    /// Durable checkpoint authority could not be reconciled before admission.
    #[error("checkpoint authority reconciliation failed: {0}")]
    CheckpointReconciliation(String),
    /// The actual loaded drive did not report media safe for ordinary Object ingest.
    #[error(transparent)]
    ObjectWriteMedia(#[from] ObjectWriteMediaError),
    /// BOT identity did not match the selected tape.
    #[error(transparent)]
    TapeIdentity(#[from] TapeIdentityError),
    /// A transfer's primary failure survived a secondary safety/plumbing failure.
    #[error("{primary}; secondary {context}: {secondary}")]
    TransferWithSecondary {
        /// The device or producer failure that caused the transfer to stop.
        primary: String,
        /// The secondary operation that also failed.
        context: &'static str,
        /// The secondary failure detail.
        secondary: String,
    },
    /// Timestamp formatting failed.
    #[error("format timestamp: {0}")]
    TimeFormat(#[from] time::error::Format),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoParityAppendContext {
    pub(super) tape_file_number: u64,
    pub(super) previous_total_committed_ordinals: u64,
    pub(super) fresh_tape: bool,
    pub(super) expected_append_lba: Option<u64>,
}

impl NoParityAppendContext {
    /// Return the physical EOD LBA where the next no-parity tape file starts.
    ///
    /// `previous_total_committed_ordinals` counts object data only. A dense
    /// prefix ending before `tape_file_number` also contains one bootstrap
    /// block and one trailing filemark for every preceding tape file.
    pub(super) fn expected_append_lba(self) -> Result<u64, PoolWriteError> {
        if let Some(expected) = self.expected_append_lba {
            return Ok(expected);
        }
        if self.fresh_tape {
            return Ok(0);
        }
        self.previous_total_committed_ordinals
            .checked_add(self.tape_file_number)
            .and_then(|lba| lba.checked_add(NO_PARITY_BOOTSTRAP_BLOCKS))
            .ok_or_else(|| {
                PoolWriteError::MissingTapeGeometry(
                    "expected no-parity append LBA overflows u64".to_string(),
                )
            })
    }

    pub(super) fn object_total_committed_ordinals(
        self,
        object_blocks: u64,
    ) -> Result<u64, PoolWriteError> {
        self.previous_total_committed_ordinals
            .checked_add(object_blocks)
            .ok_or_else(|| {
                PoolWriteError::InvalidInput("no-parity committed ordinal overflow".to_string())
            })
    }

    /// Volume-global LBA of block 0 of the object tape file this append
    /// writes. The object occupies `[start, start + block_count)`,
    /// exclusive; the trailing filemark at `start + block_count` is outside
    /// the span; on a fresh tape the bootstrap prefix (bootstrap block plus
    /// its trailing filemark) precedes this start and is excluded at
    /// capture. Dead-reckoned; the covering barrier's device-proved
    /// position transitively validates the arithmetic.
    pub(super) fn object_start_lba(self) -> Result<u64, PoolWriteError> {
        let fresh_prefix_records = if self.fresh_tape {
            NO_PARITY_BOOTSTRAP_BLOCKS + 1 // bootstrap block + its trailing filemark
        } else {
            0
        };
        self.expected_append_lba()?
            .checked_add(fresh_prefix_records)
            .ok_or_else(|| {
                PoolWriteError::InvalidInput("no-parity object start LBA overflows u64".to_string())
            })
    }
}

/// Session-local positioning rule for one batched parity-off append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BatchedAppendPosition {
    FreshTape,
    JournalEod(u64),
    CurrentBoundary(u64),
}

/// Provisional append context carried by the drive actor between objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchedNoParityAppendContext {
    pub(super) append: NoParityAppendContext,
    pub(super) position: BatchedAppendPosition,
    /// Number of committed Object recovery rows that the terminal index will
    /// stream from the durable checkpoint journal.
    pub(super) object_row_count: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum PoolWriteDurability {
    #[cfg(test)]
    PerObject,
    Batched(BatchedNoParityAppendContext),
}

/// Errors returned when verifying a physical tape identity at BOT.
#[derive(Debug, Error)]
pub enum TapeIdentityError {
    /// No parseable bootstrap block was present at BOT.
    #[error("absent bootstrap at BOT: {0}")]
    AbsentBootstrap(String),
    /// The bootstrap tape UUID did not match the expected tape.
    #[error("tape identity mismatch: expected {expected}, found {actual}")]
    Mismatch {
        /// Expected tape UUID text.
        expected: String,
        /// Bootstrap tape UUID text.
        actual: String,
    },
}
