//! Pool-targeted object write core for the Phase 1 non-hardware path.
//!
//! This module composes Layer 4 catalog state, Layer 3b `rem-object-v1`
//! streaming, Layer 3c parity, and the existing in-memory-compatible
//! `BlockSink` adapter. It intentionally contains the tape-selection boundary
//! so the later policy workstream can replace that one function without
//! changing the write engine.

use std::cell::Cell;
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc as std_mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

use remanence_aead::{RecipientPublicKey, SealReport};
use remanence_format::{
    write_encrypted_rem_object_from_readers, write_rem_tar_object_from_readers, RemTarFileLayout,
    RemTarFileSpec, RemTarFileStream, RemTarObjectOptions, FORMAT_ID,
};
use remanence_library::{
    BlockSink, BlockSource, PipelinedWriteDiagnostics, TapeIoError, TapePosition, VecBlockSink,
    WriteBatchOutcome, WriteFilemarksOutcome, WriteOutcome,
};
use remanence_parity::{
    bootstrap::{parse_bootstrap_block, write_bootstrap_block},
    checked_bounded_resume_summary, sole_bot_filemark_map_digest, BlockSinkRawTapeSink,
    BootstrapPayload, BoundedResumeWriterSeed, CapacityReserveCause, CommittedBundle,
    CommittedBundleKind, FileTapeFileJournal, ObjectWriteSummary, ParityConfig, ParityError,
    ParityScheme, ParitySchemeRecord, ParitySink, ParitySinkSessionState, PhysicalPositionHint,
    RawTapeSink, RawWriteOutcome, SchemeId, TapeFileEntry, TapeFileJournal, TapeFileKind,
    TerminalComponentCommit, TerminalComponentReconcileEvidence, TerminalPrefixPlan,
    TerminalPrefixReconcileEvidence, TerminalTailAuthority, TerminalTailProgress,
    TerminalTailRunOutcome, TerminalTripleCapacityRuntimeState, TerminalTripleCloseInput,
    TerminalTripleObjectReservation, TerminalTripleWritePlan,
};
#[cfg(test)]
use remanence_parity::{CommittedState, JournalError};
use remanence_state::{
    effective_tape_pool_capacity_bytes, validate_tape_pool_capacity_invariant,
    watermark_floor_bytes, CatalogIndex, NativeObjectCopyProjectionInput, NativeObjectCopyRecord,
    NativeObjectFileProjectionInput, NativeObjectProjectionInput, NativeObjectRecord, StateError,
    TapeJournalIndexInput, TapePoolConfig, TapeRecord, OBJECT_COPY_REPRESENTATION_ENCRYPTED,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use remanence_stream::{
    plan_prepared_object, prepare_regular_file, write_prepared_object_to_parity_from_readers,
    FileCatalogProjection, ObjectCatalogProjection, ObjectCopyProjection, PreparedFile,
    StreamingAuditEvent, StreamingCatalogProjection, StreamingError, StreamingObjectPlan,
    StreamingObjectWriteReport,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::pool_selection::{
    capacity_admission_disposition, AdmissionDisposition, CapacityAdmissionInput, CompleteOrFill,
    FillOldest, PoolSelectionContext, PoolSelectionPolicy, Selection, TapeFitState,
};
use crate::{append_mode_for_tape_file_number, bytes_to_hex, pb, timestamp_from_rfc3339};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const VERIFY_BOOTSTRAP_READ_BYTES: usize = 1024 * 1024;
const NO_PARITY_BOOTSTRAP_BLOCKS: u64 = 1;
/// Fresh media begins with the one-block identity bootstrap and its trailing
/// filemark before any Object can be admitted.
const PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS: u64 = 2;
const TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS: u64 = 4;
/// Unpooled media has no ordinary fill policy. Its close-only authority uses
/// the maximally conservative nonempty band `L=0, H=1`, reserving every block
/// above the first for a possible terminal close.
const UNPOOLED_TERMINAL_LOW_WATERMARK_BLOCKS: u64 = 0;
const UNPOOLED_TERMINAL_HIGH_WATERMARK_BLOCKS: u64 = 1;

/// Exact terminal-triple authority paired with its atomic parity-spool grant.
#[derive(Debug)]
struct ParityCapacityReservation {
    reservation: TerminalTripleObjectReservation,
    _spool_permit: crate::io_memory::IoMemoryPermit,
}

impl ParityCapacityReservation {
    #[cfg(test)]
    fn report(&self) -> &remanence_parity::TerminalTripleCloseReport {
        self.reservation.report()
    }

    fn into_parts(
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
    io_memory: Arc<crate::io_memory::IoMemoryReservation>,
}

impl PoolWriteResources {
    /// Create a shared resource handle using the configured byte ceiling.
    pub fn new(io_memory_ceiling_bytes: u64) -> Result<Self, String> {
        Ok(Self {
            io_memory: crate::io_memory::IoMemoryReservation::new(io_memory_ceiling_bytes)?,
        })
    }

    fn io_memory(&self) -> &Arc<crate::io_memory::IoMemoryReservation> {
        &self.io_memory
    }
}

/// Marks entry into the raw write boundary while allowing position-only
/// validation to remain recoverable. The owner uses this distinction to
/// restore a detached session after local/source failure, but fence it once a
/// transport write may have changed media.
struct CapacityTrackingRawTapeSink<'a> {
    inner: &'a mut dyn RawTapeSink,
    write_attempted: &'a Cell<bool>,
    position_ready: bool,
}

impl<'a> CapacityTrackingRawTapeSink<'a> {
    fn new(inner: &'a mut dyn RawTapeSink, write_attempted: &'a Cell<bool>) -> Self {
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
struct DirectSequentialTerminalAuthority<'a> {
    checkpoint: &'a mut remanence_state::FileCheckpointJournalLease,
    parity_journal: Option<&'a mut FileTapeFileJournal>,
    intent: remanence_state::TerminalFinalizationIntent,
    cursor_proved_for: TerminalTailProgress,
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
    static FAIL_PARITY_POST_WRITE_PROJECTION: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn parity_post_write_projection_gate() -> Result<(), PoolWriteError> {
    if FAIL_PARITY_POST_WRITE_PROJECTION.with(|flag| flag.replace(false)) {
        Err(PoolWriteError::InvalidInput(
            "injected post-write parity projection failure".to_string(),
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(test))]
fn parity_post_write_projection_gate() -> Result<(), PoolWriteError> {
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

/// Live plaintext source metadata and reader supplied by the append RPC.
pub struct StreamedWriteSource {
    reader: Arc<Mutex<Box<dyn Read + Send>>>,
    size_bytes: u64,
    content_sha256: [u8; 32],
    control: Arc<crate::append_ring::AppendRingControl>,
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
    /// Stored representation to write to tape.
    pub representation: PoolWriteRepresentation,
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

fn append_commit_info_from_pool_copy(copy: &PoolWriteObjectCopyRecord) -> pb::AppendCommitInfo {
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
    append_commit_diagnostics: AppendCommitDiagnostics,
    sealed_after_write: bool,
    checkpoint_projection: Option<remanence_state::CheckpointObjectProjection>,
    post_write_used_bytes: u64,
    hardware_early_warning: bool,
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
    pub pool_id: String,
    /// Unique eligible tape selected inside the pool.
    pub tape_uuid: TapeUuid,
    /// Fixed block size recorded for the selected tape.
    pub block_size: u32,
    /// Parity configuration recorded for the selected tape.
    pub parity_config: ParityConfig,
}

/// LTO cartridge generation parsed from a barcode media-type suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtoGen {
    /// LTO-1 native media.
    Lto1,
    /// LTO-2 native media.
    Lto2,
    /// LTO-3 native media.
    Lto3,
    /// LTO-4 native media.
    Lto4,
    /// LTO-5 native media.
    Lto5,
    /// LTO-6 native media.
    Lto6,
    /// LTO-7 native media.
    Lto7,
    /// LTO-7 Type-M initialized media.
    M8,
    /// LTO-8 native media.
    Lto8,
    /// LTO-9 native media.
    Lto9,
}

impl LtoGen {
    /// Numeric LTO generation, with Type-M represented as LTO-8 media class.
    pub fn generation_number(self) -> u8 {
        match self {
            Self::Lto1 => 1,
            Self::Lto2 => 2,
            Self::Lto3 => 3,
            Self::Lto4 => 4,
            Self::Lto5 => 5,
            Self::Lto6 => 6,
            Self::Lto7 => 7,
            Self::M8 | Self::Lto8 => 8,
            Self::Lto9 => 9,
        }
    }
}

impl fmt::Display for LtoGen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Lto1 => "LTO-1",
            Self::Lto2 => "LTO-2",
            Self::Lto3 => "LTO-3",
            Self::Lto4 => "LTO-4",
            Self::Lto5 => "LTO-5",
            Self::Lto6 => "LTO-6",
            Self::Lto7 => "LTO-7",
            Self::M8 => "LTO-7 Type-M",
            Self::Lto8 => "LTO-8",
            Self::Lto9 => "LTO-9",
        };
        f.write_str(label)
    }
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
struct NoParityAppendContext {
    tape_file_number: u64,
    previous_total_committed_ordinals: u64,
    fresh_tape: bool,
    expected_append_lba: Option<u64>,
}

impl NoParityAppendContext {
    /// Return the physical EOD LBA where the next no-parity tape file starts.
    ///
    /// `previous_total_committed_ordinals` counts object data only. A dense
    /// prefix ending before `tape_file_number` also contains one bootstrap
    /// block and one trailing filemark for every preceding tape file.
    fn expected_append_lba(self) -> Result<u64, PoolWriteError> {
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

    fn object_total_committed_ordinals(self, object_blocks: u64) -> Result<u64, PoolWriteError> {
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
    fn object_start_lba(self) -> Result<u64, PoolWriteError> {
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
enum BatchedAppendPosition {
    FreshTape,
    JournalEod(u64),
    CurrentBoundary(u64),
}

/// Provisional append context carried by the drive actor between objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BatchedNoParityAppendContext {
    append: NoParityAppendContext,
    position: BatchedAppendPosition,
    /// Number of committed Object recovery rows that the terminal index will
    /// stream from the durable checkpoint journal.
    object_row_count: u64,
}

#[derive(Clone, Debug)]
enum PoolWriteDurability {
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

/// Select a tape for pool-targeted writes using the configured default policy.
pub fn select_tape_in_pool(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
) -> Result<SelectedTape, SelectTapeError> {
    match pool_cfg.selection_policy {
        remanence_state::PoolSelectionPolicyName::CompleteOrFill => {
            select_tape_in_pool_with_policy(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &CompleteOrFill,
            )
        }
        remanence_state::PoolSelectionPolicyName::FillOldest => select_tape_in_pool_with_policy(
            state,
            pool_cfg,
            object_size,
            reserved_tape_uuids,
            &FillOldest,
        ),
    }
}

/// Select a tape for a write session under the sole checkpointed write mode.
/// Select a tape that is either fresh or carries a durable checkpoint
/// journal, as required by the single checkpointed write mode.
pub fn select_tape_in_pool_for_write_session(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    checkpoint_journal_dir: &Path,
) -> Result<SelectedTape, SelectTapeError> {
    match pool_cfg.selection_policy {
        remanence_state::PoolSelectionPolicyName::CompleteOrFill => {
            select_tape_in_pool_with_policy_and_batched_eligibility(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &CompleteOrFill,
                checkpoint_journal_dir,
            )
        }
        remanence_state::PoolSelectionPolicyName::FillOldest => {
            select_tape_in_pool_with_policy_and_batched_eligibility(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &FillOldest,
                checkpoint_journal_dir,
            )
        }
    }
}

/// Why an operator-pinned tape cannot open a write session.
///
/// Pinning replaces pool *selection*, never *admission*: every check that
/// makes a tape a valid pool-mode candidate still gates here, plus the
/// mandatory pool guard. Pools carry copy-class segregation, so a silent
/// cross-pool write is policy corruption — a guard mismatch is a refusal
/// that names both pools, never a warning.
#[derive(Debug, Error)]
pub enum PinnedTapeError {
    /// No catalog row exists for the pinned UUID. An uninitialized cartridge
    /// has no tape UUID at all (identity is minted by tape init), so this
    /// also covers "that tape was never initialized".
    #[error("tape {tape_uuid} is not in the catalog; an uninitialized cartridge has no tape UUID — run `rem tape init` first")]
    UnknownTape {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
    },
    /// The pinned tape is not a data tape (e.g. a cleaning cartridge).
    #[error("tape {tape_uuid} is a {kind} tape, not a data tape")]
    NotADataTape {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// Catalog tape kind.
        kind: String,
    },
    /// The tape's catalog pool assignment does not match the caller's guard.
    #[error("tape {tape_uuid} is assigned to pool {}, not the required pool {required_pool_id}; pools carry copy-class segregation, so the guard must name the tape's actual pool", actual_pool_id.as_deref().unwrap_or("(none)"))]
    PoolGuardMismatch {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// Pool the caller claimed the tape belongs to.
        required_pool_id: String,
        /// Pool the catalog actually assigns the tape to, if any.
        actual_pool_id: Option<String>,
    },
    /// The tape fails the same hard writability preconditions pool-mode
    /// candidates must pass (lifecycle state, geometry, capacity, parity
    /// append rules, pool block size).
    #[error("tape {tape_uuid} is not writable: {reason}")]
    NotWritable {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// The failed precondition.
        reason: WritabilityError,
    },
    /// A media-readiness quarantine or io fence blocks this tape.
    #[error("tape {tape_uuid} is fenced by quarantine {quarantine_id}: {reason}")]
    Fenced {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// Owning quarantine id.
        quarantine_id: String,
        /// Fence reason.
        reason: String,
    },
    /// The tape carries committed data but no adopted checkpoint journal, so
    /// batched append positioning would be unsafe — the same rule pool-mode
    /// candidates must pass.
    #[error("tape {tape_uuid} carries committed data but no adopted checkpoint journal; batched append positioning would be unsafe")]
    NotBatchEligible {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
    },
    /// UUID/geometry projection failure (shared with pool selection).
    #[error(transparent)]
    Select(#[from] SelectTapeError),
    /// Catalog access failure.
    #[error(transparent)]
    State(#[from] remanence_state::StateError),
}

fn tape_is_fresh_for_checkpoint_admission(
    state: &CatalogIndex,
    tape: &TapeRecord,
    tape_uuid: &TapeUuid,
) -> Result<bool, StateError> {
    if tape.total_committed_ordinals != 0 {
        return Ok(false);
    }
    match tape.last_committed_tape_file {
        None => Ok(state.list_tape_files(tape_uuid)?.is_empty()),
        Some(0) if tape.scheme_id.is_some() => {
            let files = state.list_tape_files(tape_uuid)?;
            Ok(matches!(
                files.as_slice(),
                [file]
                    if file.tape_file_number == 0
                        && file.kind == "bootstrap"
                        && file.block_count == 1
                        && file.object_id.is_none()
            ))
        }
        _ => Ok(false),
    }
}

/// Admit one operator-pinned tape for a write session.
///
/// `pool_cfg` is the configuration of `required_pool_id`, resolved by the
/// caller; guard-shape validation (empty guard, allow_unpooled semantics)
/// happens before config resolution and therefore also caller-side.
pub fn admit_pinned_tape_for_write_session(
    state: &CatalogIndex,
    tape_uuid: TapeUuid,
    required_pool_id: &str,
    pool_cfg: &TapePoolConfig,
    checkpoint_journal_dir: &Path,
) -> Result<SelectedTape, PinnedTapeError> {
    let uuid_text = Uuid::from_bytes(tape_uuid);
    let tape = state
        .get_tape(&tape_uuid)?
        .ok_or(PinnedTapeError::UnknownTape {
            tape_uuid: uuid_text,
        })?;
    if tape.kind != "data" {
        return Err(PinnedTapeError::NotADataTape {
            tape_uuid: uuid_text,
            kind: tape.kind.clone(),
        });
    }
    let actual_pool = tape
        .pool_id
        .as_deref()
        .map(str::trim)
        .filter(|pool| !pool.is_empty());
    if actual_pool != Some(required_pool_id) {
        return Err(PinnedTapeError::PoolGuardMismatch {
            tape_uuid: uuid_text,
            required_pool_id: required_pool_id.to_string(),
            actual_pool_id: actual_pool.map(str::to_string),
        });
    }
    match check_writability_preconditions(&tape, 0)
        .and_then(|_| check_pool_block_size_precondition(&tape, pool_cfg))
    {
        Ok(()) => {}
        Err(WritabilityError::ParityAppendUnsupported { .. }) => {
            // The gate below requires a durable checkpoint record. Session
            // open compares that record with the sink journal before LOCATE.
        }
        Err(reason) => {
            return Err(PinnedTapeError::NotWritable {
                tape_uuid: uuid_text,
                reason,
            });
        }
    }
    let conflicts = state.tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())?;
    if let Some(conflict) = conflicts.first() {
        return Err(PinnedTapeError::Fenced {
            tape_uuid: uuid_text,
            quarantine_id: conflict.quarantine_id.clone(),
            reason: conflict.reason.clone(),
        });
    }
    let fresh = tape_is_fresh_for_checkpoint_admission(state, &tape, &tape_uuid)?;
    if !fresh {
        let checkpoint_journal_tapes = checkpoint_journal_tape_uuids(checkpoint_journal_dir)?;
        if !tape_carries_checkpoint(checkpoint_journal_dir, &checkpoint_journal_tapes, tape_uuid)? {
            return Err(PinnedTapeError::NotBatchEligible {
                tape_uuid: uuid_text,
            });
        }
    }
    Ok(selected_tape_from_record(tape, required_pool_id)?)
}

/// Select an eligible tape from a pool using a caller-supplied pure policy.
///
/// This is the narrow integration adapter for the current non-hardware path:
/// catalog rows are projected into [`TapeFitState`] values and the policy
/// remains free of catalog/session/hardware access. Live-session reservations
/// are caller-projected and filtered out before the policy ranks candidates.
pub fn select_tape_in_pool_with_policy(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    policy: &dyn PoolSelectionPolicy,
) -> Result<SelectedTape, SelectTapeError> {
    select_tape_in_pool_with_policy_and_eligibility(
        state,
        pool_cfg,
        object_size,
        reserved_tape_uuids,
        policy,
        None,
    )
}

fn select_tape_in_pool_with_policy_and_batched_eligibility(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    policy: &dyn PoolSelectionPolicy,
    checkpoint_journal_dir: &Path,
) -> Result<SelectedTape, SelectTapeError> {
    select_tape_in_pool_with_policy_and_eligibility(
        state,
        pool_cfg,
        object_size,
        reserved_tape_uuids,
        policy,
        Some(checkpoint_journal_dir),
    )
}

fn select_tape_in_pool_with_policy_and_eligibility(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    policy: &dyn PoolSelectionPolicy,
    checkpoint_journal_dir: Option<&Path>,
) -> Result<SelectedTape, SelectTapeError> {
    let requested_pool_id = pool_cfg.id.trim();
    let pool =
        state
            .get_tape_pool(requested_pool_id)?
            .ok_or_else(|| SelectTapeError::UnknownPool {
                pool_id: requested_pool_id.to_string(),
            })?;
    let pool_id = pool.pool_id;

    let tapes = state.list_tapes(
        Some(pool_id.as_str()),
        remanence_state::TapeKindFilter::Data,
    )?;
    if tapes.is_empty() {
        return Err(SelectTapeError::EmptyPool { pool_id });
    }
    validate_pool_capacity_invariant_for_tapes(pool_cfg, &tapes)?;
    let checkpoint_journal_tapes = checkpoint_journal_dir
        .map(checkpoint_journal_tape_uuids)
        .transpose()?;

    // 2a-2 owns the hard writability precondition (state/geometry/capacity fit);
    // the policy ranks only the tapes that pass it (design §6 boundary).
    let mut reasons = Vec::new();
    let mut eligible = Vec::new();
    let mut batched_ineligible = Vec::new();
    for tape in tapes {
        match check_writability_preconditions(&tape, object_size)
            .and_then(|_| check_pool_block_size_precondition(&tape, pool_cfg))
        {
            Ok(()) => {}
            Err(WritabilityError::ParityAppendUnsupported { .. })
                if checkpoint_journal_dir.is_some() =>
            {
                // The checkpoint-specific eligibility gate below requires a
                // durable record before admission. Session open proves that
                // it agrees with the sink journal before LOCATE.
            }
            Err(err) => {
                reasons.push(err);
                continue;
            }
        }
        let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id.as_str())?;
        let conflicts = state
            .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
            .map_err(SelectTapeError::State)?;
        if let Some(conflict) = conflicts.first() {
            reasons.push(WritabilityError::TapeIoFence {
                quarantine_id: conflict.quarantine_id.clone(),
                reason: conflict.reason.clone(),
            });
            continue;
        }
        if let (Some(checkpoint_journal_dir), Some(checkpoint_journal_tapes)) =
            (checkpoint_journal_dir, checkpoint_journal_tapes.as_ref())
        {
            let fresh = tape_is_fresh_for_checkpoint_admission(state, &tape, &tape_uuid)?;
            let carries_checkpoint = fresh
                || tape_carries_checkpoint(
                    checkpoint_journal_dir,
                    checkpoint_journal_tapes,
                    tape_uuid,
                )?;
            if !carries_checkpoint {
                batched_ineligible.push(format!(
                    "{} ({})",
                    tape.voltag.as_deref().unwrap_or("<no-voltag>"),
                    Uuid::from_bytes(tape_uuid)
                ));
                continue;
            }
        }
        eligible.push(tape);
    }
    if eligible.is_empty() {
        if !batched_ineligible.is_empty() {
            return Err(SelectTapeError::NoBatchedEligibleTapes {
                pool_id,
                ineligible_candidates: batched_ineligible,
            });
        }
        return Err(SelectTapeError::NoWritableTapes { pool_id, reasons });
    }
    eligible.sort_by(compare_tapes_for_pool_selection);

    let mut ranked = Vec::with_capacity(eligible.len());
    let mut reserved_tape_count = 0usize;
    for (index, tape) in eligible.into_iter().enumerate() {
        match tape_fit_state_from_record(&tape, pool_cfg, pool_id.as_str(), index as u64) {
            Ok(candidate) if reserved_tape_uuids.contains(&candidate.tape_uuid) => {
                reserved_tape_count += 1;
            }
            Ok(candidate) => ranked.push((tape, candidate)),
            Err(err) => reasons.push(err),
        }
    }
    if ranked.is_empty() {
        if reserved_tape_count > 0 {
            return Err(SelectTapeError::NoUnreservedWritableTapes {
                pool_id,
                reserved_tape_count,
            });
        }
        return Err(SelectTapeError::NoWritableTapes { pool_id, reasons });
    }

    let candidates = ranked
        .iter()
        .map(|(_, candidate)| candidate.clone())
        .collect::<Vec<_>>();

    let ctx = PoolSelectionContext {
        candidates: &candidates,
        projected_footprint: object_size,
    };
    match policy.select(&ctx) {
        Selection::UseTape { tape_uuid } => ranked
            .into_iter()
            .find(|(_, candidate)| candidate.tape_uuid == tape_uuid)
            .map(|(tape, _)| selected_tape_from_record(tape, pool_id.as_str()))
            .unwrap_or_else(|| {
                Err(SelectTapeError::NoWritableTapes {
                    pool_id: pool_id.clone(),
                    reasons: Vec::new(),
                })
            }),
        Selection::NeedFreshTape => Err(SelectTapeError::NoWritableTapes { pool_id, reasons }),
    }
}

fn checkpoint_journal_tape_uuids(
    checkpoint_journal_dir: &Path,
) -> Result<HashSet<TapeUuid>, StateError> {
    let mut journal_tapes = HashSet::new();
    for path in remanence_state::list_checkpoint_journals(checkpoint_journal_dir)? {
        let tape_uuid = remanence_state::tape_uuid_from_checkpoint_path(path.as_path())?;
        journal_tapes.insert(tape_uuid);
    }
    Ok(journal_tapes)
}

fn tape_carries_checkpoint(
    checkpoint_journal_dir: &Path,
    checkpoint_journal_tapes: &HashSet<TapeUuid>,
    tape_uuid: TapeUuid,
) -> Result<bool, StateError> {
    if !checkpoint_journal_tapes.contains(&tape_uuid) {
        return Ok(false);
    }
    remanence_state::FileCheckpointJournal::open(checkpoint_journal_dir, tape_uuid)?
        .last()
        .map(|record| record.is_some())
}

/// Write one regular file to a caller-named pool using the Phase 1
/// non-hardware-compatible `BlockSink` path, commit catalog rows, and return
/// the resulting object locator.
#[cfg(test)]
pub fn write_object_to_pool(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
        return Ok(result);
    }
    let source_size = request.source.size_bytes()?;
    let reserved_tape_uuids = HashSet::new();
    let selected = select_tape_in_pool(state, pool_cfg, source_size, &reserved_tape_uuids)?;
    write_to_selected_tape_inner(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        false,
        PoolWriteDurability::PerObject,
    )
}

/// Select a checkpoint-eligible tape and run the direct batch-of-one core.
pub fn write_object_to_pool_checkpointed(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    checkpoint_journal_dir: &Path,
    parity_journal_path_for: impl FnOnce(TapeUuid) -> PathBuf,
    resources: &PoolWriteResources,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
        return Ok(result);
    }
    let source_size = request.source.size_bytes()?;
    let selected = select_tape_in_pool_for_write_session(
        state,
        pool_cfg,
        source_size,
        &HashSet::new(),
        checkpoint_journal_dir,
    )?;
    let parity_journal_path = parity_journal_path_for(selected.tape_uuid);
    write_to_selected_tape_checkpointed(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        checkpoint_journal_dir,
        &parity_journal_path,
        resources,
    )
}

/// Write one regular file to a previously selected tape without re-running
/// pool tape selection.
///
/// This is the select-once entrypoint for callers that already opened a write
/// session against a concrete tape. The selected tape's [`ParityConfig`]
/// controls whether the write uses the existing parity path or the direct
/// no-parity bootstrap/body/filemark path.
#[cfg(test)]
pub fn write_to_selected_tape(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
    write_to_selected_tape_with_live_counter(state, sink, pool_cfg, request, selected, None)
}

fn terminal_trigger_for_seal_reason(
    reason: TapeSealReason,
) -> Result<remanence_state::TerminalFinalizationTrigger, PoolWriteError> {
    match reason {
        TapeSealReason::ReachedLowWatermark => {
            Ok(remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark)
        }
        TapeSealReason::HardwareEarlyWarning => {
            Ok(remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning)
        }
        TapeSealReason::PoolCloseOut => {
            Ok(remanence_state::TerminalFinalizationTrigger::PoolCloseOut)
        }
        TapeSealReason::NoPendingObjectFits => {
            Ok(remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits)
        }
        TapeSealReason::OperatorCloseOut => Err(PoolWriteError::InvalidInput(
            "direct automatic terminal finalization cannot invent manual operator identity"
                .to_string(),
        )),
    }
}

/// Validate one already-planned close-only terminal tail through the shared
/// exact capacity authority.
///
/// Parity finalization must emit its pending sidecar/ParityMap prefix before
/// `first_start_lba`; the barrier-proved cursor therefore charges that prefix
/// without modeling it a second time. The returned report covers exactly the
/// remaining A/gap/B/gap/C tail plus the shared safety allowance under the
/// selected pool/cartridge C/L/H basis.
pub(crate) fn authorize_terminal_close_only_plan(
    state: &CatalogIndex,
    pool_cfg: Option<&TapePoolConfig>,
    selected: &SelectedTape,
    first_start_lba: u64,
    counts: remanence_parity::TapeIndexReplicaCounts,
    planned_eod_lba: u64,
) -> Result<remanence_parity::TerminalTripleCloseReport, PoolWriteError> {
    let capacity_blocks = terminal_capacity_basis_blocks(state, pool_cfg, selected)?;
    let (low_watermark_blocks, high_watermark_blocks) =
        terminal_watermark_blocks(capacity_blocks, pool_cfg)?;
    let remaining_tape_blocks = capacity_blocks
        .checked_sub(first_start_lba)
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(format!(
                "terminal cursor {first_start_lba} exceeds capacity basis {capacity_blocks}"
            ))
        })?;
    let report = TerminalTripleCloseInput {
        projected_object_present: false,
        projected_object_blocks: 0,
        block_size_bytes: selected.block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 1,
        parity_shards_per_epoch: 0,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object: 0,
        structural_entries_before_object: counts.structural_entry_count,
        object_rows_before_object: counts.object_row_count,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS,
        remaining_tape_blocks,
        capacity_basis_blocks: capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 0,
    }
    .evaluate()?;
    let planned_tail_charge = planned_eod_lba
        .checked_sub(first_start_lba)
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "terminal layout ends before its starting cursor".to_string(),
            )
        })?;
    if report.terminal_tail_charge_blocks != planned_tail_charge {
        return Err(PoolWriteError::InvalidInput(format!(
            "terminal close authority/layout mismatch: calculator={} planned={planned_tail_charge}",
            report.terminal_tail_charge_blocks
        )));
    }
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn build_direct_terminal_plan(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    previous: &remanence_state::CheckpointJournalRecord,
    source: &mut remanence_state::CheckpointTerminalIndexRecordSource<'_>,
    first_tape_file_number: u64,
    first_start_lba: u64,
    terminal_prefix: Option<&TerminalPrefixPlan>,
    trigger: remanence_state::TerminalFinalizationTrigger,
) -> Result<
    (
        remanence_state::TerminalFinalizationIntent,
        TerminalTripleWritePlan,
    ),
    PoolWriteError,
> {
    let summary = source.summary();
    let replica =
        remanence_parity::checked_tape_index_replica_layout(selected.block_size, summary.counts)
            .map_err(|error| {
                PoolWriteError::InvalidInput(format!("plan direct terminal replica: {error}"))
            })?;
    let separation_records = remanence_parity::index_separation_records(
        selected.block_size,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal separation: {error}"))
    })?;
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        selected.block_size,
        first_tape_file_number,
        first_start_lba,
        replica.replica_record_count,
        separation_records,
    )
    .map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal layout: {error}"))
    })?;
    authorize_terminal_close_only_plan(
        state,
        Some(pool_cfg),
        selected,
        first_start_lba,
        summary.counts,
        layout.expected_eod_lba,
    )?;
    let edition_sequence = previous.ordinal.checked_add(1).ok_or_else(|| {
        PoolWriteError::InvalidInput("terminal edition sequence overflows u64".to_string())
    })?;
    let edition_id = *Uuid::new_v4().as_bytes();
    let writer_version = format!("remanence-api/{}", env!("CARGO_PKG_VERSION"));
    let write_timestamp = now_rfc3339()?;
    let edition = remanence_parity::plan_tape_index_edition(
        remanence_parity::TapeIndexEditionDescriptor {
            tape_uuid: selected.tape_uuid,
            edition_id,
            edition_sequence,
            scope: summary.scope,
            counts: summary.counts,
            block_size: selected.block_size,
            compression_enabled: false,
            writer_version: writer_version.clone(),
            write_timestamp: write_timestamp.clone(),
            terminal_layout: layout,
        },
        source,
    )
    .map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal edition: {error}"))
    })?;
    let intent = remanence_state::TerminalFinalizationIntent {
        tape_uuid: selected.tape_uuid,
        trigger,
        manual: None,
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        edition_id,
        edition_sequence,
        edition_digest: edition.edition_digest,
        writer_version,
        write_timestamp,
        terminal_prefix: terminal_prefix.map(remanence_state::TerminalFinalizationPrefixPlan::from),
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)?,
    };
    let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal writer: {error}"))
    })?;
    Ok((intent, plan))
}

#[allow(clippy::too_many_arguments)]
fn execute_direct_terminal_tail(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    selected: &SelectedTape,
    intent: remanence_state::TerminalFinalizationIntent,
    plan: &TerminalTripleWritePlan,
    source: &mut remanence_state::CheckpointTerminalIndexRecordSource<'_>,
    mut parity_journal: Option<&mut FileTapeFileJournal>,
    parity_prefix: Option<(ParitySinkSessionState, &TerminalPrefixPlan)>,
) -> Result<remanence_state::CheckpointJournalRecord, PoolWriteError> {
    let write_attempted = Cell::new(false);
    let execution = (|| -> Result<_, PoolWriteError> {
        let mut raw = BlockSinkRawTapeSink::new(sink);
        let mut tracked = CapacityTrackingRawTapeSink::new(&mut raw, &write_attempted);
        if let Some((session_state, prefix)) = parity_prefix {
            let journal = parity_journal.as_deref_mut().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "direct parity terminal prefix has no parity journal".to_string(),
                )
            })?;
            let parity = ParitySink::from_session_state(&mut tracked, journal, session_state)?;
            parity.close_for_terminal_index(prefix, TerminalPrefixReconcileEvidence::Absent)?;
        }
        let completed_intent = {
            let mut authority = DirectSequentialTerminalAuthority {
                checkpoint,
                parity_journal,
                intent,
                cursor_proved_for: TerminalTailProgress::BeforeReplicaA,
            };
            match remanence_parity::write_terminal_tail(&mut tracked, source, &mut authority, plan)
                .map_err(|error| {
                    PoolWriteError::InvalidInput(format!("write direct terminal triple: {error}"))
                })? {
                TerminalTailRunOutcome::Complete => {}
                TerminalTailRunOutcome::RecoveryRequired {
                    progress,
                    component,
                    evidence,
                } => {
                    return Err(PoolWriteError::InvalidInput(format!(
                        "direct terminal continuation requires source-capable reconciliation at {progress:?} for {:?}/{}: {evidence:?}",
                        component.kind, component.ordinal
                    )));
                }
            }
            authority.intent
        };
        if completed_intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC
        {
            return Err(PoolWriteError::InvalidInput(
                "direct terminal writer completed before final replica C became durable"
                    .to_string(),
            ));
        }
        let replica_c = plan.edition.descriptor.terminal_layout.components[4];
        let final_bundle =
            remanence_parity::terminal_component_bundle(plan, replica_c).map_err(|error| {
                PoolWriteError::InvalidInput(format!("build direct final C authority: {error}"))
            })?;
        let final_record = remanence_state::CheckpointJournalRecord {
            ordinal: previous.ordinal.checked_add(1).ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "terminal checkpoint ordinal overflows u64".to_string(),
                )
            })?,
            committed_object_count: previous.committed_object_count,
            eod_partition: plan.edition.descriptor.terminal_layout.partition,
            eod_lba: plan.edition.descriptor.terminal_layout.expected_eod_lba,
            tape_uuid: selected.tape_uuid,
            batch_id: completed_intent.edition_id,
            next_tape_file_number: replica_c
                .planned_tape_file_number
                .checked_add(1)
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "terminal next tape-file number overflows u64".to_string(),
                    )
                })?,
            block_size: selected.block_size,
            objects: Vec::new(),
            scheme: previous.scheme.clone(),
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: Some(final_bundle),
            terminal_finalization: Some(completed_intent),
            sealed_after_write: true,
        };
        checkpoint.append_terminal_finalization(std::slice::from_ref(&final_record))?;
        state.project_checkpoint_record(&final_record)?;
        Ok(final_record)
    })();
    match execution {
        Ok(record) => Ok(record),
        Err(error) if write_attempted.get() => Err(fence_after_terminal_motion(
            state,
            selected,
            "terminal_finalization",
            error,
        )),
        Err(error) => Err(error),
    }
}

#[allow(clippy::too_many_arguments)]
fn finalize_direct_checkpoint_prefix(
    state: &mut CatalogIndex,
    pool_cfg: &TapePoolConfig,
    sink: &mut dyn BlockSink,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    records: &[remanence_state::CheckpointJournalRecord],
    selected: &SelectedTape,
    trigger: remanence_state::TerminalFinalizationTrigger,
    mut parity_journal: Option<&mut FileTapeFileJournal>,
    parity_state: Option<ParitySinkSessionState>,
) -> Result<remanence_state::CheckpointJournalRecord, PoolWriteError> {
    let previous = records.last().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "direct terminal finalization has no committed checkpoint".to_string(),
        )
    })?;
    let observed = sink.position()?;
    if observed.partition != previous.eod_partition || observed.lba != previous.eod_lba {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(format!(
            "direct terminal cursor is partition {} lba {}, expected checkpoint partition {} lba {}",
            observed.partition, observed.lba, previous.eod_partition, previous.eod_lba
        ))));
    }
    match (&selected.parity_config, parity_state) {
        (ParityConfig::None, None) => {
            let mut source =
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(
                    checkpoint,
                )?;
            let first_file = source.summary().scope.covered_prefix_tape_file_count;
            let (intent, plan) = build_direct_terminal_plan(
                state,
                pool_cfg,
                selected,
                previous,
                &mut source,
                first_file,
                observed.lba,
                None,
                trigger,
            )?;
            checkpoint.begin_terminal_finalization(&intent)?;
            execute_direct_terminal_tail(
                state,
                sink,
                checkpoint,
                previous,
                selected,
                intent,
                &plan,
                &mut source,
                None,
                None,
            )
        }
        (ParityConfig::Scheme(_), Some(session_state)) => {
            let journal = parity_journal.as_deref_mut().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "direct parity terminal finalization has no parity journal".to_string(),
                )
            })?;
            let (prefix, session_state) = {
                let mut raw = BlockSinkRawTapeSink::new(sink);
                let parity = ParitySink::from_session_state(&mut raw, journal, session_state)?;
                let prefix = parity.plan_terminal_index_close()?;
                let state = parity.into_session_state()?;
                (prefix, state)
            };
            let persisted = remanence_state::TerminalFinalizationPrefixPlan::from(&prefix);
            let mut source = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                checkpoint,
                journal,
                &persisted,
            )?;
            let (intent, plan) = build_direct_terminal_plan(
                state,
                pool_cfg,
                selected,
                previous,
                &mut source,
                prefix.tail_start_tape_file_number,
                prefix.tail_start_lba,
                Some(&prefix),
                trigger,
            )?;
            checkpoint.begin_terminal_finalization(&intent)?;
            execute_direct_terminal_tail(
                state,
                sink,
                checkpoint,
                previous,
                selected,
                intent,
                &plan,
                &mut source,
                parity_journal,
                Some((session_state, &prefix)),
            )
        }
        (ParityConfig::None, Some(_)) => Err(PoolWriteError::InvalidInput(
            "parity-off direct finalization unexpectedly has parity session state".to_string(),
        )),
        (ParityConfig::Scheme(_), None) => Err(PoolWriteError::InvalidInput(
            "parity direct finalization is missing its terminal session state".to_string(),
        )),
    }
}

/// Write one object and complete one shared checkpoint barrier.
///
/// This is the daemon-independent batch-of-one core used by direct-SCSI
/// callers. Both checkpoint and Layer 3c journals are the same per-tape files
/// used by daemon sessions, so a later mount resumes from identical durable
/// state.
#[allow(clippy::too_many_arguments)]
pub fn write_to_selected_tape_checkpointed(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    checkpoint_journal_dir: &Path,
    parity_journal_path: &Path,
    resources: &PoolWriteResources,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
        return Ok(result);
    }
    let checkpoint_journal =
        remanence_state::FileCheckpointJournal::open(checkpoint_journal_dir, selected.tape_uuid)?;
    let mut checkpoint_lease = checkpoint_journal.acquire_exclusive()?;
    let mut last_checkpoint = None;
    checkpoint_lease.for_each_record_bounded(|record| {
        state.project_checkpoint_record(record)?;
        last_checkpoint = Some(record.clone());
        Ok(())
    })?;
    let prior_records: Vec<_> = last_checkpoint.into_iter().collect();
    ensure_empty_checkpoint_matches_catalog_freshness(state, &selected, &prior_records)?;
    ensure_selected_tape_accepts_session_write(state, pool_cfg, &selected)?;
    let next_ordinal = prior_records.last().map_or(Ok(1), |record| {
        record.ordinal.checked_add(1).ok_or_else(|| {
            PoolWriteError::InvalidInput("checkpoint ordinal overflows u64".to_string())
        })
    })?;
    let next_committed_count = prior_records.last().map_or(Ok(1), |record| {
        record.committed_object_count.checked_add(1).ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "checkpoint committed object count overflows u64".to_string(),
            )
        })
    })?;
    let batch_id = *Uuid::new_v4().as_bytes();

    let (
        mut result,
        next_tape_file_number,
        sync,
        scheme,
        object_bundles,
        barrier_bundle,
        mut terminal_parity_state,
    ) = match &selected.parity_config {
        ParityConfig::None => {
            let append = first_batched_append_context(state, &selected, &prior_records)?;
            let result = write_batched_to_selected_tape_after_replay_check(
                state,
                sink,
                pool_cfg,
                request,
                selected.clone(),
                None,
                append,
            )?;
            let projection = result.checkpoint_projection().cloned().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "batch-of-one object is missing checkpoint projection".to_string(),
                )
            })?;
            let sync = sink.write_filemarks(0)?;
            let next_tape_file_number = projection
                .copy
                .tape_file_number
                .checked_add(1)
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "checkpoint next tape-file number overflows u64".to_string(),
                    )
                })?;
            (
                result,
                next_tape_file_number,
                sync,
                None,
                Vec::new(),
                None,
                None,
            )
        }
        ParityConfig::Scheme(parity_scheme) => {
            let mut parity_journal = FileTapeFileJournal::open(
                parity_journal_path,
                selected.tape_uuid,
                selected.block_size,
                parity_scheme.clone(),
            )
            .map_err(ParityError::from)?;
            if parity_journal.orphaned_bundles_preserved_on_open() != 0 {
                tracing::warn!(
                    tape_uuid = %uuid_text(selected.tape_uuid),
                    orphaned_bundle_count = parity_journal.orphaned_bundles_preserved_on_open(),
                    "preserved sink-journal bundles beyond the last checkpoint watermark; reconciliation required"
                );
            }
            let snapshot = parity_journal
                .committed_snapshot_bounded()
                .map_err(ParityError::from)?;
            let summary = checked_bounded_resume_summary(&snapshot)?;
            if prior_records.is_empty() {
                if summary.committed_tape_file_count != 0 {
                    return Err(PoolWriteError::InvalidInput(
                        "parity journal has a committed prefix but checkpoint authority is empty"
                            .to_string(),
                    ));
                }
            } else {
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed(
                    &checkpoint_lease,
                    &parity_journal,
                )?;
            }
            let session_state = if summary.committed_tape_file_count == 0 {
                let located = sink.locate(0)?;
                if located.partition != 0 || located.lba != 0 {
                    return Err(PoolWriteError::InvalidInput(format!(
                        "fresh parity BOT locate reported partition {} lba {}, expected partition 0 lba 0",
                        located.partition, located.lba
                    )));
                }
                let fresh_session_state = {
                    let mut raw = BlockSinkRawTapeSink::new(sink);
                    let mut parity = ParitySink::new_with_journal(
                        &mut raw,
                        &mut parity_journal,
                        parity_scheme.clone(),
                        selected.tape_uuid,
                        selected.block_size,
                    )?;
                    parity.write_bootstrap()?;
                    parity.into_session_state()?
                };
                project_fresh_parity_bootstrap_bundle(state, &selected, parity_scheme)?;
                fresh_session_state
            } else {
                let checkpoint = prior_records.last().ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "non-fresh parity tape has no checkpoint journal".to_string(),
                    )
                })?;
                sink.locate(checkpoint.eod_lba)?;
                let plan = summary.append_plan(parity_scheme)?;
                if !plan.sidecars_to_emit.is_empty()
                    || plan.highest_protected_ordinal_before_rebuild != plan.next_data_ordinal
                {
                    return Err(PoolWriteError::InvalidInput(
                        "checkpointed parity tape retains an open epoch".to_string(),
                    ));
                }
                let resume_result = plan.complete(Vec::new())?;
                let resume_session_state = {
                    let mut raw = BlockSinkRawTapeSink::new(sink);
                    let parity = ParitySink::new_sidecar_only_from_bounded_resume(
                        &mut raw,
                        &mut parity_journal,
                        parity_scheme.clone(),
                        selected.tape_uuid,
                        selected.block_size,
                        BoundedResumeWriterSeed {
                            committed_prefix_snapshot: snapshot,
                            committed_prefix_summary: summary,
                            resume_result: &resume_result,
                            live_epoch: None,
                        },
                    )?;
                    parity.into_session_state()?
                };
                resume_session_state
            };
            let mut raw_write_attempted = false;
            let parity_append = (|| -> Result<_, PoolWriteError> {
                let (session_state, mut result) = {
                    let mut raw = BlockSinkRawTapeSink::new(sink);
                    let mut session_state = Some(session_state);
                    let result = write_batched_parity_to_selected_tape_after_replay_check(
                        state,
                        &mut raw,
                        &mut parity_journal,
                        &mut session_state,
                        pool_cfg,
                        request,
                        selected.clone(),
                        resources.io_memory(),
                        &mut raw_write_attempted,
                    )?;
                    let session_state = session_state.ok_or_else(|| {
                        PoolWriteError::InvalidInput(
                            "successful parity batch-of-one lost its session state".to_string(),
                        )
                    })?;
                    (session_state, result)
                };
                let object_bundle = result
                    .write_report()
                    .ok_or_else(|| {
                        PoolWriteError::InvalidInput(
                            "parity batch-of-one is missing its write report".to_string(),
                        )
                    })?
                    .catalog
                    .tape_file_bundle
                    .clone();
                let (closed, session_state) = {
                    let mut raw = BlockSinkRawTapeSink::new(sink);
                    let mut parity = ParitySink::from_session_state(
                        &mut raw,
                        &mut parity_journal,
                        session_state,
                    )?;
                    let closed = parity.close_open_epoch(remanence_parity::CloseReason::Barrier)?;
                    let state = parity.into_session_state()?;
                    (closed, state)
                };
                result.hardware_early_warning |= session_state.hardware_early_warning_seen();
                Ok((
                    result,
                    closed.next_tape_file_number,
                    closed.barrier_outcome,
                    Some(parity_scheme.clone()),
                    vec![object_bundle],
                    closed.committed_bundle,
                    Some(session_state),
                ))
            })();
            match parity_append {
                Ok(result) => result,
                Err(error) if raw_write_attempted => {
                    return Err(fence_after_terminal_motion(
                        state,
                        &selected,
                        "parity_append",
                        error,
                    ));
                }
                Err(error) => return Err(error),
            }
        }
    };
    let captured = sink.position()?;
    if captured.partition != sync.position_after.partition
        || captured.lba != sync.position_after.lba
    {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(
            "batch-of-one barrier position proof mismatch".to_string(),
        )));
    }
    let projection = result.checkpoint_projection().cloned().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "batch-of-one object is missing checkpoint projection".to_string(),
        )
    })?;
    let record = remanence_state::CheckpointJournalRecord {
        ordinal: next_ordinal,
        committed_object_count: next_committed_count,
        eod_partition: captured.partition,
        eod_lba: captured.lba,
        tape_uuid: selected.tape_uuid,
        batch_id,
        next_tape_file_number,
        block_size: selected.block_size,
        objects: vec![projection],
        scheme,
        object_tape_file_bundles: object_bundles,
        barrier_bundle,
        terminal_finalization: None,
        sealed_after_write: false,
    };
    let used_bytes = captured
        .lba
        .checked_mul(u64::from(selected.block_size))
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "batch-of-one post-barrier used-byte count overflows u64".to_string(),
            )
        })?;
    let seal_reason = selected_tape_seal_reason_at_barrier(
        state,
        &selected,
        pool_cfg,
        TapePositionAfterWrite {
            used_bytes,
            early_warning: result.hardware_early_warning || sync.early_warning,
        },
    )?;
    checkpoint_lease.append(&record)?;
    state.project_checkpoint_record(&record)?;
    if let Some(seal_reason) = seal_reason {
        let mut authority_records = prior_records;
        authority_records.push(record);
        let trigger = terminal_trigger_for_seal_reason(seal_reason)?;
        match &selected.parity_config {
            ParityConfig::None => {
                finalize_direct_checkpoint_prefix(
                    state,
                    pool_cfg,
                    sink,
                    &mut checkpoint_lease,
                    &authority_records,
                    &selected,
                    trigger,
                    None,
                    None,
                )?;
            }
            ParityConfig::Scheme(parity_scheme) => {
                let parity_state = terminal_parity_state.take().ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "sealed parity batch has no terminal session state".to_string(),
                    )
                })?;
                let mut parity_journal = FileTapeFileJournal::open(
                    parity_journal_path,
                    selected.tape_uuid,
                    selected.block_size,
                    parity_scheme.clone(),
                )
                .map_err(ParityError::from)?;
                finalize_direct_checkpoint_prefix(
                    state,
                    pool_cfg,
                    sink,
                    &mut checkpoint_lease,
                    &authority_records,
                    &selected,
                    trigger,
                    Some(&mut parity_journal),
                    Some(parity_state),
                )?;
            }
        }
        result.sealed_after_write = true;
    }
    Ok(result)
}

pub(crate) fn project_fresh_parity_bootstrap_bundle(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    scheme: &ParityScheme,
) -> Result<(), PoolWriteError> {
    // The caller holds the exclusive journal handle and invokes this only
    // after `ParitySink::write_bootstrap` returned success at the proved BOT
    // cursor. Project that exact sole-BOT write without asking ordinary
    // bounded-resume replay to accept its intentionally uncheckpointed state.
    let bootstrap_bundle = CommittedBundle {
        kind: CommittedBundleKind::BotBootstrap,
        entries: vec![TapeFileEntry {
            tape_file_number: 0,
            kind: TapeFileKind::Bootstrap,
            block_count: 1,
            physical_start_hint: Some(0),
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        }],
        highest_protected_ordinal: 0,
        total_committed_ordinals: 0,
    };
    state.project_committed_tape_file_bundle(
        TapeJournalIndexInput {
            tape_uuid: selected.tape_uuid,
            block_size: selected.block_size,
            scheme: Some(scheme.clone()),
            journal_offset_bytes: 0,
        },
        &bootstrap_bundle,
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_to_selected_tape_with_live_counter(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
) -> Result<PoolWriteResult, PoolWriteError> {
    write_to_selected_tape_with_live_counter_impl(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        live_write_counter,
        true,
        PoolWriteDurability::PerObject,
    )
}

/// Write one parity-off object into a server-owned provisional batch.
pub(crate) fn write_batched_to_selected_tape_after_replay_check(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
    append: BatchedNoParityAppendContext,
) -> Result<PoolWriteResult, PoolWriteError> {
    if !matches!(selected.parity_config, ParityConfig::None) {
        return Err(PoolWriteError::InvalidInput(
            "batched checkpointing is not admitted on parity-enabled tapes".to_string(),
        ));
    }
    write_to_selected_tape_with_live_counter_impl(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        live_write_counter,
        false,
        PoolWriteDurability::Batched(append),
    )
}

/// Write one parity-protected object through the actor-carried logical sink.
///
/// On success, `session_state` contains the same session advanced to the next
/// Object boundary. Every failure before raw motion leaves the original state
/// available to the owner; a write-path failure after reattachment consumes it
/// so the owner fences the uncertain session.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_batched_parity_to_selected_tape_after_replay_check(
    state: &CatalogIndex,
    raw: &mut dyn RawTapeSink,
    journal: &mut dyn TapeFileJournal,
    session_state: &mut Option<ParitySinkSessionState>,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    io_memory: &Arc<crate::io_memory::IoMemoryReservation>,
    raw_write_attempted: &mut bool,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    ensure_selected_tape_accepts_session_write(state, pool_cfg, &selected)?;
    if matches!(selected.parity_config, ParityConfig::None) {
        return Err(PoolWriteError::InvalidInput(
            "parity session append requested for a parity-off tape".to_string(),
        ));
    }
    let prepared = prepare_pool_object(&request, selected.block_size)?;
    if let Some(expected) = request.expected_content_sha256 {
        if prepared.content_sha256 != expected {
            return Err(PoolWriteError::ContentHashMismatch {
                expected: bytes_to_hex(&expected),
                actual: bytes_to_hex(&prepared.content_sha256),
            });
        }
    }
    let stored = prepare_stored_object(&prepared, &request.representation)?;
    let capacity_blocks = parity_capacity_basis_blocks(state, pool_cfg, &selected)?;
    let projected_object_blocks = match &stored {
        PreparedStoredObject::Plaintext => prepared.plan.layout.projected_size_blocks,
        PreparedStoredObject::Encrypted(encrypted) => encrypted.envelope.stored_size_blocks,
    };
    let mut plaintext_readers = if matches!(&stored, PreparedStoredObject::Plaintext) {
        Some(open_prepared_readers(&prepared)?)
    } else {
        None
    };
    let detached = session_state.as_ref().ok_or_else(|| {
        PoolWriteError::InvalidInput("parity sink session state is unavailable".to_string())
    })?;
    // The detached session is the live, exclusive append authority between
    // shared barriers.  In particular, a fresh session contains the sole BOT
    // Bootstrap before the first checkpoint marker exists, so the ordinary
    // bounded-prefix loader must not be asked to interpret that intentional
    // one-bundle suffix.  Resumed sessions were themselves constructed from a
    // bounded snapshot, and reattachment below checks any currently available
    // bounded authority against this carried state.
    let runtime = detached.terminal_triple_capacity_runtime_state()?;
    let structural_entries = runtime.structural_entries_before_object;
    let object_rows = runtime.object_rows_before_object;
    let capacity = reserve_parity_object_capacity(
        runtime,
        detached.scheme(),
        &selected,
        (pool_cfg, structural_entries, object_rows),
        capacity_blocks,
        projected_object_blocks,
        io_memory,
    )?;
    let (capacity, _spool_permit) = capacity.into_parts();
    let detached = session_state.take().ok_or_else(|| {
        PoolWriteError::InvalidInput("parity sink session state is unavailable".to_string())
    })?;
    let write_attempted = Cell::new(false);
    let mut tracked = CapacityTrackingRawTapeSink::new(raw, &write_attempted);
    let mut parity = match ParitySink::try_from_session_state(&mut tracked, journal, detached) {
        Ok(parity) => parity,
        Err((error, detached)) => {
            *session_state = Some(*detached);
            *raw_write_attempted |= write_attempted.get();
            return Err(error.into());
        }
    };
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> = match &stored {
        PreparedStoredObject::Plaintext => {
            let readers = plaintext_readers.as_mut().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "prepared plaintext Object is missing its source readers".to_string(),
                )
            })?;
            write_prepared_object_to_parity_from_readers(
                &mut parity,
                selected.tape_uuid,
                &prepared.options,
                &prepared.files,
                readers,
                capacity,
            )
            .map_err(PoolWriteError::from)
        }
        PreparedStoredObject::Encrypted(encrypted) => write_encrypted_object_to_parity(
            &mut parity,
            selected.tape_uuid,
            &prepared,
            encrypted,
            capacity,
        ),
    };
    let write_report = match write_report {
        Ok(report) => report,
        Err(error) => {
            let attempted = write_attempted.get();
            *raw_write_attempted |= attempted;
            if !attempted {
                parity.rollback_unwritten_object()?;
                *session_state = Some(parity.into_session_state()?);
            }
            return Err(error);
        }
    };
    // From this point onward the Object writer has completed one or more raw
    // commands. Publish that fact before any projection or detach operation
    // can fail so the owner cannot mistake a post-motion error for a safe
    // retry.
    *raw_write_attempted |= write_attempted.get();
    parity_post_write_projection_gate()?;
    let mut checkpoint_projection = checkpoint_projection_for_no_parity_write(
        &selected,
        &prepared,
        &write_report,
        stored.copy_representation(),
    )?;
    checkpoint_projection.copy.first_parity_data_ordinal =
        write_report.catalog.object_copy.first_parity_data_ordinal;
    checkpoint_projection.copy.protected_until_ordinal =
        write_report.catalog.object_copy.protected_until_ordinal;
    checkpoint_projection.fresh_tape = false;
    let mut result = pool_write_result(
        request,
        selected,
        prepared,
        stored.copy_representation(),
        write_report,
        AppendCommitDiagnostics::default(),
        false,
        Some(checkpoint_projection),
    )?;
    let detached = parity.into_session_state()?;
    result.hardware_early_warning |= detached.hardware_early_warning_seen();
    *session_state = Some(detached);
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn write_to_selected_tape_with_live_counter_impl(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
    check_replay: bool,
    durability: PoolWriteDurability,
) -> Result<PoolWriteResult, PoolWriteError> {
    match live_write_counter {
        Some(counter) => {
            let mut live_counted_sink =
                LiveCounterBlockSink::new(sink, counter, selected.block_size);
            write_to_selected_tape_inner(
                state,
                &mut live_counted_sink,
                pool_cfg,
                request,
                selected,
                check_replay,
                durability,
            )
        }
        None => write_to_selected_tape_inner(
            state,
            sink,
            pool_cfg,
            request,
            selected,
            check_replay,
            durability,
        ),
    }
}

fn write_to_selected_tape_inner<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut S,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    check_replay: bool,
    durability: PoolWriteDurability,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if check_replay {
        if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
            return Ok(result);
        }
    }
    ensure_selected_tape_accepts_write(state, pool_cfg, &selected)?;
    let block_size = selected.block_size;
    let prepare_started = Instant::now();
    let prepared = prepare_pool_object(&request, block_size)?;
    if let Some(expected) = request.expected_content_sha256 {
        if prepared.content_sha256 != expected {
            return Err(PoolWriteError::ContentHashMismatch {
                expected: bytes_to_hex(&expected),
                actual: bytes_to_hex(&prepared.content_sha256),
            });
        }
    }
    let stored = prepare_stored_object(&prepared, &request.representation)?;
    let stored_projected_blocks = stored.projected_size_blocks(&prepared);
    let mut stored_size_bytes = stored_footprint_bytes(&stored, &prepared, selected.block_size)?;
    if matches!(&durability, PoolWriteDurability::Batched(_)) {
        stored_size_bytes = stored_size_bytes
            .checked_add(u64::from(selected.block_size).saturating_mul(3))
            .ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "batched barrier capacity reservation overflows u64".to_string(),
                )
            })?;
    }
    let provisional_used_lba = match &durability {
        #[cfg(test)]
        PoolWriteDurability::PerObject => None,
        PoolWriteDurability::Batched(context) => Some(context.append.expected_append_lba()?),
    };
    if let (ParityConfig::None, PoolWriteDurability::Batched(context)) =
        (&selected.parity_config, &durability)
    {
        ensure_no_parity_terminal_close_capacity(
            state,
            pool_cfg,
            &selected,
            context,
            stored_projected_blocks,
        )?;
    }
    ensure_selected_tape_has_capacity(
        state,
        pool_cfg,
        &selected,
        stored_size_bytes,
        provisional_used_lba,
    )?;
    let prepare_elapsed = prepare_started.elapsed();
    let payload_bytes = prepared_payload_bytes(&prepared);
    tracing::info!(
        target: "remanence_write_diag",
        phase = "prepare",
        pool_id = %selected.pool_id,
        tape_uuid = %uuid_text(selected.tape_uuid),
        parity = parity_label(&selected.parity_config),
        representation = stored.representation_label(),
        payload_bytes,
        selected_block_size_bytes = selected.block_size,
        projected_object_blocks = stored_projected_blocks,
        elapsed_ms = crate::diagnostics::duration_ms(prepare_elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, prepare_elapsed),
        "remanence_write_diag",
    );

    // Only the hardware-backed tape transfer below is counted live. The spool
    // write already finished in mount.rs, and parity/object replay only reads
    // the prepared in-memory object.
    let mut counted_sink = CountingBlockSink::new(sink, selected.block_size);
    let prepared_write = PreparedPoolWrite { prepared, stored };
    match selected.parity_config.clone() {
        ParityConfig::Scheme(scheme) => {
            #[cfg(test)]
            {
                write_parity_object_to_selected_tape(
                    state,
                    &mut counted_sink,
                    pool_cfg,
                    request,
                    selected,
                    prepared_write,
                    scheme,
                )
            }
            #[cfg(not(test))]
            {
                let _ = (scheme, prepared_write);
                Err(PoolWriteError::InvalidInput(
                    "the legacy per-object parity path is test-only; use the checkpointed batch core"
                        .to_string(),
                ))
            }
        }
        ParityConfig::None => write_no_parity_object_to_selected_tape(
            state,
            &mut counted_sink,
            pool_cfg,
            request,
            selected,
            prepared_write,
            durability,
        ),
    }
}

/// Verify that the block at BOT is a bootstrap for the expected tape UUID.
///
/// The helper uses only the generic [`BlockSource`] surface so tests can run
/// against [`remanence_library::VecBlockSource`]. It leaves the source
/// positioned immediately after the bootstrap block on success.
pub fn verify_tape_identity(
    source: &mut dyn BlockSource,
    expected_tape_uuid: &[u8; 16],
) -> Result<(), TapeIdentityError> {
    source
        .locate(0)
        .map_err(|err| TapeIdentityError::AbsentBootstrap(format!("locate BOT: {err}")))?;
    let mut block = vec![0u8; VERIFY_BOOTSTRAP_READ_BYTES];
    let read = source
        .read_block(&mut block)
        .map_err(|err| TapeIdentityError::AbsentBootstrap(format!("read BOT: {err}")))?;
    let payload = parse_bootstrap_block(&block[..read])
        .map_err(|err| TapeIdentityError::AbsentBootstrap(err.to_string()))?;
    if &payload.tape_uuid != expected_tape_uuid {
        return Err(TapeIdentityError::Mismatch {
            expected: uuid_text(*expected_tape_uuid),
            actual: uuid_text(payload.tape_uuid),
        });
    }
    Ok(())
}

/// Build the bootstrap payload for a newly provisioned tape.
pub fn build_tape_bootstrap(
    tape_uuid: TapeUuid,
    block_size: u32,
    parity: ParityConfig,
    written_at: impl Into<String>,
    written_by_version: impl Into<String>,
) -> BootstrapPayload {
    match parity {
        ParityConfig::None => BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: written_by_version.into(),
            written_at: written_at.into(),
            sequence: 0,
            block_size_bytes: block_size,
            drive_compression: false,
        },
        ParityConfig::Scheme(scheme) => BootstrapPayload {
            scheme: Some(ParitySchemeRecord {
                id: scheme.id.as_str().to_string(),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(
                sole_bot_filemark_map_digest()
                    .expect("canonical sole-BOT filemark map is structurally valid"),
            ),
            tape_uuid,
            written_by_version: written_by_version.into(),
            written_at: written_at.into(),
            sequence: 0,
            block_size_bytes: block_size,
            drive_compression: false,
        },
    }
}

/// Write one bootstrap tape file through a generic block sink.
pub fn write_tape_bootstrap(
    sink: &mut dyn BlockSink,
    payload: &BootstrapPayload,
) -> Result<(), PoolWriteError> {
    let mut block = vec![0u8; payload.block_size_bytes as usize];
    write_bootstrap_block(payload, &mut block)?;
    sink.write_block(&block)?;
    sink.write_filemarks(1)?;
    Ok(())
}

/// Parse an LTO generation from the barcode media-type suffix.
pub fn lto_generation_from_voltag(voltag: &str) -> Option<LtoGen> {
    let trimmed = voltag.trim();
    if !trimmed.is_ascii() {
        return None;
    }
    let suffix_start = trimmed.len().checked_sub(2)?;
    let suffix = trimmed[suffix_start..].to_ascii_uppercase();
    match suffix.as_str() {
        "L1" => Some(LtoGen::Lto1),
        "L2" => Some(LtoGen::Lto2),
        "L3" => Some(LtoGen::Lto3),
        "L4" => Some(LtoGen::Lto4),
        "L5" => Some(LtoGen::Lto5),
        "L6" => Some(LtoGen::Lto6),
        "L7" => Some(LtoGen::Lto7),
        "M8" => Some(LtoGen::M8),
        "L8" => Some(LtoGen::Lto8),
        "L9" | "LZ" => Some(LtoGen::Lto9),
        _ => None,
    }
}

/// Parse a drive LTO generation from common INQUIRY product strings.
pub fn lto_generation_from_drive_product(product: &str) -> Option<LtoGen> {
    let product = product.to_ascii_uppercase();
    for (needle, generation) in [
        ("LTO-9", LtoGen::Lto9),
        ("LTO9", LtoGen::Lto9),
        ("ULTRIUM 9", LtoGen::Lto9),
        ("LTO-8", LtoGen::Lto8),
        ("LTO8", LtoGen::Lto8),
        ("ULTRIUM 8", LtoGen::Lto8),
        ("LTO-7", LtoGen::Lto7),
        ("LTO7", LtoGen::Lto7),
        ("ULTRIUM 7", LtoGen::Lto7),
        ("LTO-6", LtoGen::Lto6),
        ("LTO6", LtoGen::Lto6),
        ("ULTRIUM 6", LtoGen::Lto6),
        ("LTO-5", LtoGen::Lto5),
        ("LTO5", LtoGen::Lto5),
        ("ULTRIUM 5", LtoGen::Lto5),
        ("LTO-4", LtoGen::Lto4),
        ("LTO4", LtoGen::Lto4),
        ("ULTRIUM 4", LtoGen::Lto4),
        ("LTO-3", LtoGen::Lto3),
        ("LTO3", LtoGen::Lto3),
        ("ULTRIUM 3", LtoGen::Lto3),
        ("LTO-2", LtoGen::Lto2),
        ("LTO2", LtoGen::Lto2),
        ("ULTRIUM 2", LtoGen::Lto2),
        ("LTO-1", LtoGen::Lto1),
        ("LTO1", LtoGen::Lto1),
        ("ULTRIUM 1", LtoGen::Lto1),
    ] {
        if product.contains(needle) {
            return Some(generation);
        }
    }
    None
}

/// Return whether an LTO drive generation can read a cartridge generation.
///
/// This is an explicit media compatibility table, not the historical
/// "read two generations back" formula. LTO-8 and LTO-9 intentionally break
/// that formula, and Type-M (`M8`) is modeled as its own media generation.
pub fn can_read(drive: LtoGen, tape: LtoGen) -> bool {
    match drive {
        LtoGen::Lto5 => matches!(tape, LtoGen::Lto5 | LtoGen::Lto4 | LtoGen::Lto3),
        LtoGen::Lto6 => matches!(tape, LtoGen::Lto6 | LtoGen::Lto5 | LtoGen::Lto4),
        LtoGen::Lto7 => matches!(tape, LtoGen::Lto7 | LtoGen::Lto6 | LtoGen::Lto5),
        LtoGen::Lto8 => matches!(tape, LtoGen::Lto8 | LtoGen::Lto7 | LtoGen::M8),
        LtoGen::Lto9 => matches!(tape, LtoGen::Lto9 | LtoGen::Lto8),
        LtoGen::Lto1 | LtoGen::Lto2 | LtoGen::Lto3 | LtoGen::Lto4 | LtoGen::M8 => false,
    }
}

/// Return whether an LTO drive generation can write a cartridge generation.
///
/// The table mirrors the authoritative init-flow design and is kept separate
/// from [`can_read`] because read and write compatibility differ.
pub fn can_write(drive: LtoGen, tape: LtoGen) -> bool {
    match drive {
        LtoGen::Lto5 => matches!(tape, LtoGen::Lto5 | LtoGen::Lto4),
        LtoGen::Lto6 => matches!(tape, LtoGen::Lto6 | LtoGen::Lto5),
        LtoGen::Lto7 => matches!(tape, LtoGen::Lto7 | LtoGen::Lto6),
        LtoGen::Lto8 => matches!(tape, LtoGen::Lto8 | LtoGen::Lto7 | LtoGen::M8),
        LtoGen::Lto9 => matches!(tape, LtoGen::Lto9 | LtoGen::Lto8),
        LtoGen::Lto1 | LtoGen::Lto2 | LtoGen::Lto3 | LtoGen::Lto4 | LtoGen::M8 => false,
    }
}

/// Native/raw cartridge capacity in bytes for one LTO generation.
pub fn raw_capacity_bytes(generation: LtoGen) -> u64 {
    LTO_RAW_CAPACITY_BYTES
        .iter()
        .find_map(|(candidate, bytes)| (*candidate == generation).then_some(*bytes))
        .expect("all LTO generations have a raw capacity entry")
}

/// Check that a catalog tape row is a hard-valid target for `object_size`.
pub fn check_writability_preconditions(
    tape: &TapeRecord,
    object_size: u64,
) -> Result<(), WritabilityError> {
    if tape.state != "ready" {
        return Err(WritabilityError::NotReady {
            state: tape.state.clone(),
        });
    }
    let block_size = tape
        .block_size
        .ok_or_else(|| missing_geometry("block_size is null"))?;
    if block_size == 0 {
        return Err(missing_geometry("block_size is zero"));
    }
    validate_scheme_columns(tape)?;
    if tape.total_committed_ordinals > 0 && tape.scheme_id.is_some() {
        return Err(WritabilityError::ParityAppendUnsupported {
            total_committed_ordinals: tape.total_committed_ordinals,
        });
    }
    let voltag = tape
        .voltag
        .as_deref()
        .ok_or_else(|| missing_geometry("voltag is null"))?;
    let generation = lto_generation_from_voltag(voltag)
        .ok_or_else(|| missing_geometry("voltag does not end in a known LTO suffix"))?;
    let raw_capacity = raw_capacity_bytes(generation);
    let used = tape_physical_used_bytes(tape, block_size)?;
    if used > raw_capacity || object_size > raw_capacity - used {
        return Err(WritabilityError::InsufficientCapacity {
            object_size,
            raw_capacity,
            used,
        });
    }
    Ok(())
}

fn check_pool_block_size_precondition(
    tape: &TapeRecord,
    pool_cfg: &TapePoolConfig,
) -> Result<(), WritabilityError> {
    let tape_block_size = tape_block_size(tape)?;
    if tape_block_size != pool_cfg.block_size_bytes {
        return Err(WritabilityError::BlockSizeMismatch {
            tape_block_size,
            pool_block_size: pool_cfg.block_size_bytes,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
struct BlockSinkStats {
    block_write_calls: u64,
    block_write_bytes: u64,
    min_block_bytes: Option<u64>,
    max_block_bytes: Option<u64>,
    filemark_calls: u64,
    filemarks: u64,
    filemark_write_drain: Duration,
    position_calls: u64,
    early_warning: bool,
    write_batch_blocks: u32,
    effective_batch_blocks: u32,
    position_check_bytes: u64,
    staging_ring_buffers: u32,
    staging_wait_samples: u64,
    staging_wait_p50_us: u64,
    staging_wait_p95_us: u64,
    staging_wait_max_us: u64,
    staging_wait_mean_us: u64,
    refill_samples: u64,
    refill_p50_us: u64,
    refill_p95_us: u64,
    refill_max_us: u64,
    refill_mean_us: u64,
    gap_samples: u64,
    gap_p50_us: u64,
    gap_p95_us: u64,
    gap_max_us: u64,
    gap_mean_us: u64,
    ioctl_samples: u64,
    ioctl_p50_us: u64,
    ioctl_p95_us: u64,
    ioctl_max_us: u64,
    ioctl_mean_us: u64,
    first_60s_ioctl_samples: u64,
    first_60s_ioctl_p50_us: u64,
    first_60s_ioctl_p95_us: u64,
    first_60s_ioctl_max_us: u64,
    first_60s_ioctl_mean_us: u64,
    accounting_samples: u64,
    accounting_p50_us: u64,
    accounting_p95_us: u64,
    accounting_max_us: u64,
    accounting_mean_us: u64,
    cadence_us: u64,
    effective_feed_bytes_per_second: u64,
    time_to_first_ioctl_ms: u64,
    steady_reached: bool,
    time_to_steady_ms: u64,
    steady_window_seconds: u32,
    steady_threshold_percent: u32,
    ramp_observation_seconds: u32,
}

impl BlockSinkStats {
    fn record_block(&mut self, bytes: u64, early_warning: bool) {
        self.block_write_calls = self.block_write_calls.saturating_add(1);
        self.block_write_bytes = self.block_write_bytes.saturating_add(bytes);
        self.early_warning |= early_warning;
        self.min_block_bytes = Some(
            self.min_block_bytes
                .map_or(bytes, |current| current.min(bytes)),
        );
        self.max_block_bytes = Some(
            self.max_block_bytes
                .map_or(bytes, |current| current.max(bytes)),
        );
    }

    fn record_filemarks(&mut self, count: u32, early_warning: bool, elapsed: Duration) {
        self.filemark_calls = self.filemark_calls.saturating_add(1);
        self.filemarks = self.filemarks.saturating_add(u64::from(count));
        self.filemark_write_drain = self.filemark_write_drain.saturating_add(elapsed);
        self.early_warning |= early_warning;
    }

    fn record_position(&mut self, position: TapePosition) {
        self.position_calls = self.position_calls.saturating_add(1);
        self.early_warning |= position.block_position_end_of_warning;
    }

    fn record_staging(&mut self, diagnostics: &StagingPhaseDiagnostics) {
        self.staging_wait_samples = diagnostics.wait_us.samples;
        self.staging_wait_p50_us = diagnostics.wait_us.percentile(50, 100);
        self.staging_wait_p95_us = diagnostics.wait_us.percentile(95, 100);
        self.staging_wait_max_us = diagnostics.wait_us.max_us;
        self.staging_wait_mean_us = diagnostics.wait_us.mean();
        self.refill_samples = diagnostics.refill_us.samples;
        self.refill_p50_us = diagnostics.refill_us.percentile(50, 100);
        self.refill_p95_us = diagnostics.refill_us.percentile(95, 100);
        self.refill_max_us = diagnostics.refill_us.max_us;
        self.refill_mean_us = diagnostics.refill_us.mean();
    }
}

pub(crate) struct LiveCounterBlockSink<'a> {
    inner: &'a mut dyn BlockSink,
    live_write_counter: Arc<crate::DriveByteCounters>,
}

struct CountingBlockSink<'a, S: BlockSink + ?Sized> {
    inner: &'a mut S,
    stats: BlockSinkStats,
}

struct ObjectDigestBlockSink<'a, S: BlockSink + ?Sized> {
    inner: &'a mut S,
    hasher: Sha256,
}

#[derive(Clone, Copy)]
struct StagedSinkCaps {
    block_size: usize,
    batch_blocks: u32,
    requested_write_batch_blocks: u32,
    position_check_bytes: u64,
}

impl StagedSinkCaps {
    fn from_inner<S: BlockSink + ?Sized>(inner: &S, block_size: usize) -> Self {
        let block_size_u32 = u32::try_from(block_size).unwrap_or(u32::MAX);
        let batch_blocks = inner.write_batch_blocks(block_size_u32).max(1);
        Self {
            block_size,
            batch_blocks,
            requested_write_batch_blocks: inner.requested_write_batch_blocks().max(1),
            position_check_bytes: inner.position_check_bytes(),
        }
    }
}

const MAX_PIPELINE_WINDOW_BUFFERS: usize =
    remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS as usize;

#[derive(Default)]
struct RingAccounting {
    allocated: AtomicU32,
    dropped: AtomicU32,
}

const HOT_PHASE_HISTOGRAM_UPPER_US: [u64; 12] = [
    10,
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_500,
    5_000,
    10_000,
    50_000,
    u64::MAX,
];

#[derive(Default)]
struct HotPhaseHistogram {
    buckets: [u64; HOT_PHASE_HISTOGRAM_UPPER_US.len()],
    samples: u64,
    sum_us: u64,
    max_us: u64,
}

impl HotPhaseHistogram {
    fn record(&mut self, duration: Duration) {
        let sample_us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let bucket = HOT_PHASE_HISTOGRAM_UPPER_US
            .iter()
            .position(|upper| sample_us <= *upper)
            .unwrap_or(HOT_PHASE_HISTOGRAM_UPPER_US.len() - 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(sample_us);
        self.max_us = self.max_us.max(sample_us);
    }

    fn percentile(&self, numerator: u64, denominator: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let wanted = self
            .samples
            .saturating_mul(numerator)
            .saturating_add(denominator - 1)
            / denominator;
        let mut seen = 0u64;
        for (count, upper) in self.buckets.iter().zip(HOT_PHASE_HISTOGRAM_UPPER_US) {
            seen = seen.saturating_add(*count);
            if seen >= wanted {
                return if upper == u64::MAX {
                    self.max_us
                } else {
                    upper
                };
            }
        }
        self.max_us
    }

    fn mean(&self) -> u64 {
        self.sum_us.checked_div(self.samples.max(1)).unwrap_or(0)
    }
}

#[derive(Default)]
struct StagingPhaseDiagnostics {
    wait_us: HotPhaseHistogram,
    refill_us: HotPhaseHistogram,
}

struct PageAlignedBuffer {
    storage: Vec<u8>,
    start: usize,
    capacity: usize,
    used: usize,
    refill_elapsed: Duration,
    accounting: Arc<RingAccounting>,
}

impl PageAlignedBuffer {
    fn try_new(capacity: usize, accounting: Arc<RingAccounting>) -> Result<Self, TapeIoError> {
        let page_alignment = system_page_size();
        let allocation_bytes = capacity
            .checked_add(page_alignment - 1)
            .ok_or_else(|| TapeIoError::OperationFailed("staging buffer size overflow".into()))?;
        let mut storage = Vec::new();
        storage.try_reserve_exact(allocation_bytes).map_err(|err| {
            TapeIoError::OperationFailed(format!(
                "failed to allocate page-aligned staging buffer: {err}"
            ))
        })?;
        storage.resize(allocation_bytes, 0);
        let address = storage.as_ptr() as usize;
        let start = (page_alignment - (address % page_alignment)) % page_alignment;
        debug_assert_eq!((address + start) % page_alignment, 0);
        debug_assert!(start + capacity <= storage.len());
        accounting.allocated.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            storage,
            start,
            capacity,
            used: 0,
            refill_elapsed: Duration::ZERO,
            accounting,
        })
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), TapeIoError> {
        let end = self
            .used
            .checked_add(bytes.len())
            .ok_or_else(|| TapeIoError::OperationFailed("staging buffer cursor overflow".into()))?;
        if end > self.capacity {
            return Err(TapeIoError::OperationFailed(
                "staging buffer exceeded fixed batch capacity".into(),
            ));
        }
        let destination = self.start + self.used..self.start + end;
        self.storage[destination].copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    fn bytes(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.used]
    }

    fn is_full(&self) -> bool {
        self.used == self.capacity
    }

    fn reset(&mut self) {
        self.used = 0;
        self.refill_elapsed = Duration::ZERO;
    }
}

fn system_page_size() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) takes no pointers and has no memory side
    // effects. A non-positive result falls back to a conservative 4 KiB.
    let reported = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(reported)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(4096)
}

impl Drop for PageAlignedBuffer {
    fn drop(&mut self) {
        self.accounting.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

struct PipelinedBatch {
    buffer: PageAlignedBuffer,
    cdb: [u8; 6],
    records: u32,
    block_size_bytes: u32,
}

struct PipelinedWindow {
    batches: [Option<PipelinedBatch>; MAX_PIPELINE_WINDOW_BUFFERS],
    len: usize,
    bytes: u64,
}

impl PipelinedWindow {
    fn new() -> Self {
        Self {
            batches: std::array::from_fn(|_| None),
            len: 0,
            bytes: 0,
        }
    }

    fn push(&mut self, batch: PipelinedBatch) -> Result<(), TapeIoError> {
        let slot = self.batches.get_mut(self.len).ok_or_else(|| {
            TapeIoError::OperationFailed("pipelined window exceeded ring depth".into())
        })?;
        self.bytes = self
            .bytes
            .checked_add(batch.buffer.used as u64)
            .ok_or_else(|| TapeIoError::OperationFailed("pipelined window byte overflow".into()))?;
        *slot = Some(batch);
        self.len += 1;
        Ok(())
    }

    fn first_records(&self) -> u32 {
        self.batches[0]
            .as_ref()
            .expect("non-empty window has first batch")
            .records
    }

    fn last_records(&self) -> u32 {
        self.batches[self.len - 1]
            .as_ref()
            .expect("non-empty window has last batch")
            .records
    }
}

// The fixed-size window is intentionally inline: boxing it would add one heap
// allocation per staging window and violate the steady-state allocation rule.
#[allow(clippy::large_enum_variant)]
enum PipelinedSinkCommand {
    WriteWindow(PipelinedWindow),
    Barrier {
        reply: std_mpsc::Sender<Result<Option<WriteBatchOutcome>, String>>,
    },
    WriteFilemarks {
        count: u32,
        reply: std_mpsc::Sender<Result<WriteFilemarksOutcome, String>>,
    },
    WriteFilemarksImmediate {
        count: u32,
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    SpaceToEndOfData {
        reply: std_mpsc::Sender<Result<TapePosition, String>>,
    },
    Locate {
        lba: u64,
        reply: std_mpsc::Sender<Result<TapePosition, String>>,
    },
    Position {
        reply: std_mpsc::Sender<Result<TapePosition, String>>,
    },
}

struct StagedBlockSink {
    tx: std_mpsc::SyncSender<PipelinedSinkCommand>,
    free_rx: std_mpsc::Receiver<PageAlignedBuffer>,
    submitter_done_rx: std_mpsc::Receiver<()>,
    poison: Arc<Mutex<Option<String>>>,
    caps: StagedSinkCaps,
    ring_buffers: usize,
    current: Option<PageAlignedBuffer>,
    window: PipelinedWindow,
    cursor: Option<TapePosition>,
    diagnostics: StagingPhaseDiagnostics,
}

impl<'a, S: BlockSink + ?Sized> ObjectDigestBlockSink<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish_digest(self) -> [u8; 32] {
        let digest = self.hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

impl<S: BlockSink + ?Sized> BlockSink for ObjectDigestBlockSink<'_, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.hasher.update(buf);
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.hasher.update(buf);
        Ok(outcome)
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

impl StagedBlockSink {
    fn new(
        tx: std_mpsc::SyncSender<PipelinedSinkCommand>,
        free_rx: std_mpsc::Receiver<PageAlignedBuffer>,
        submitter_done_rx: std_mpsc::Receiver<()>,
        poison: Arc<Mutex<Option<String>>>,
        caps: StagedSinkCaps,
        ring_buffers: usize,
    ) -> Self {
        Self {
            tx,
            free_rx,
            submitter_done_rx,
            poison,
            caps,
            ring_buffers,
            current: None,
            window: PipelinedWindow::new(),
            cursor: None,
            diagnostics: StagingPhaseDiagnostics::default(),
        }
    }

    fn check_poison(&self) -> Result<(), TapeIoError> {
        if let Some(message) = staged_poison_message(&self.poison) {
            Err(TapeIoError::OperationFailed(format!(
                "pipelined transfer poisoned after sink error: {message}"
            )))
        } else {
            Ok(())
        }
    }

    fn acquire_buffer(&mut self) -> Result<(), TapeIoError> {
        if self.current.is_some() {
            return Ok(());
        }
        self.check_poison()?;
        let wait_started = Instant::now();
        let received = self.free_rx.recv();
        self.diagnostics.wait_us.record(wait_started.elapsed());
        let buffer = received.map_err(|_| {
            TapeIoError::OperationFailed("pipelined staging ring was closed".into())
        })?;
        self.current = Some(buffer);
        Ok(())
    }

    fn finish_current_batch(&mut self) -> Result<(), TapeIoError> {
        let Some(buffer) = self.current.take() else {
            return Ok(());
        };
        if buffer.used == 0 {
            self.current = Some(buffer);
            return Ok(());
        }
        self.diagnostics.refill_us.record(buffer.refill_elapsed);
        let block_size_bytes = u32::try_from(self.caps.block_size)
            .map_err(|_| TapeIoError::OperationFailed("batch block size exceeds u32".into()))?;
        let records = records_in_staged_batch(buffer.bytes(), block_size_bytes)?;
        let cdb = remanence_scsi::read_write::build_write_fixed_cdb(records);
        self.window.push(PipelinedBatch {
            buffer,
            cdb,
            records,
            block_size_bytes,
        })?;
        if self.window.len == self.ring_buffers {
            self.send_window()?;
        }
        Ok(())
    }

    fn send_window(&mut self) -> Result<(), TapeIoError> {
        if self.window.len == 0 {
            return Ok(());
        }
        self.check_poison()?;
        let window = std::mem::replace(&mut self.window, PipelinedWindow::new());
        self.tx
            .send(PipelinedSinkCommand::WriteWindow(window))
            .map_err(|_| TapeIoError::OperationFailed("pipelined submitter stopped".into()))?;
        self.check_poison()
    }

    fn flush_pending(&mut self) -> Result<(), TapeIoError> {
        self.finish_current_batch()?;
        self.send_window()
    }

    fn request<T>(
        &self,
        build: impl FnOnce(std_mpsc::Sender<Result<T, String>>) -> PipelinedSinkCommand,
    ) -> Result<T, TapeIoError> {
        self.check_poison()?;
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| TapeIoError::OperationFailed("pipelined submitter stopped".into()))?;
        match reply_rx.recv() {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(TapeIoError::OperationFailed(format!(
                "pipelined submitter error: {message}"
            ))),
            Err(_) => Err(TapeIoError::OperationFailed(
                "pipelined submitter dropped reply".into(),
            )),
        }
    }

    fn seed_cursor(&mut self) -> Result<TapePosition, TapeIoError> {
        if let Some(position) = self.cursor {
            return Ok(position);
        }
        let position = self.request(|reply| PipelinedSinkCommand::Position { reply })?;
        self.cursor = Some(position);
        Ok(position)
    }

    fn advance_cursor(&mut self, records: u32) -> Result<TapePosition, TapeIoError> {
        let before = self.seed_cursor()?;
        let lba = before
            .lba
            .checked_add(u64::from(records))
            .ok_or_else(|| TapeIoError::OperationFailed("batch position overflow".into()))?;
        let position = TapePosition {
            lba,
            partition: before.partition,
            beginning_of_partition: lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: before.block_position_end_of_warning,
        };
        self.cursor = Some(position);
        Ok(position)
    }

    fn finish(mut self) -> (Result<(), TapeIoError>, StagingPhaseDiagnostics) {
        let result = self.flush_pending().and_then(|()| {
            self.request(|reply| PipelinedSinkCommand::Barrier { reply })
                .map(|_| ())
        });
        (result, self.diagnostics)
    }

    fn abort(self, message: String) -> StagingPhaseDiagnostics {
        set_staged_poison(&self.poison, message);
        let StagedBlockSink {
            tx,
            free_rx,
            submitter_done_rx,
            diagnostics,
            ..
        } = self;
        drop(tx);
        let _free_rx = free_rx;
        let _ = submitter_done_rx.recv();
        diagnostics
    }
}

impl BlockSink for StagedBlockSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if buf.len() != self.caps.block_size {
            return Err(TapeIoError::OperationFailed(format!(
                "pipelined fixed write requires {}-byte records, got {}",
                self.caps.block_size,
                buf.len()
            )));
        }
        self.acquire_buffer()?;
        let refill_started = Instant::now();
        let append_result = self.current.as_mut().expect("buffer acquired").append(buf);
        let refill_elapsed = refill_started.elapsed();
        self.current
            .as_mut()
            .expect("buffer remains acquired")
            .refill_elapsed += refill_elapsed;
        append_result?;
        let position = self.advance_cursor(1)?;
        if self
            .current
            .as_ref()
            .is_some_and(PageAlignedBuffer::is_full)
        {
            self.finish_current_batch()?;
        }
        Ok(WriteOutcome::from_computed_position(
            u32::try_from(buf.len()).unwrap_or(u32::MAX),
            false,
            false,
            position,
        ))
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        if block_size_bytes as usize != self.caps.block_size
            || buf.is_empty()
            || buf.len() % self.caps.block_size != 0
        {
            return Err(TapeIoError::OperationFailed(
                "pipelined batch must contain whole configured records".into(),
            ));
        }
        self.flush_pending()?;
        let _: Option<WriteBatchOutcome> =
            self.request(|reply| PipelinedSinkCommand::Barrier { reply })?;
        let records = u32::try_from(buf.len() / self.caps.block_size)
            .map_err(|_| TapeIoError::OperationFailed("batch record count overflow".into()))?;
        for block in buf.chunks_exact(self.caps.block_size) {
            self.write_block(block)?;
        }
        self.flush_pending()?;
        let outcome = self
            .request(|reply| PipelinedSinkCommand::Barrier { reply })?
            .ok_or_else(|| {
                TapeIoError::OperationFailed("pipelined batch barrier lost its outcome".into())
            })?;
        if outcome.records_written != records {
            return Err(TapeIoError::OperationFailed(format!(
                "pipelined batch outcome mismatch: requested={records} written={}",
                outcome.records_written
            )));
        }
        self.cursor = Some(outcome.position_after);
        Ok(outcome)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.caps.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.caps.requested_write_batch_blocks
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.ring_buffers as u32
    }

    fn position_check_bytes(&self) -> u64 {
        self.caps.position_check_bytes
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.flush_pending()?;
        let outcome =
            self.request(|reply| PipelinedSinkCommand::WriteFilemarks { count, reply })?;
        self.cursor = Some(outcome.position_after);
        Ok(outcome)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.flush_pending()?;
        self.request(|reply| PipelinedSinkCommand::WriteFilemarksImmediate { count, reply })
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::SpaceToEndOfData { reply })?;
        self.cursor = Some(position);
        Ok(position)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::Locate { lba, reply })?;
        self.cursor = Some(position);
        Ok(position)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::Position { reply })?;
        self.cursor = Some(position);
        Ok(position)
    }
}

#[cfg(test)]
fn run_staged_transfer<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    run_staged_transfer_with_safety(inner, block_size, producer, |_| Ok(()))
}

#[cfg(test)]
fn run_staged_transfer_with_safety<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
    on_safety_error: impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    run_ring_staged_transfer(inner, block_size, producer, None, on_safety_error)?.result
}

#[cfg(test)]
fn run_fenced_staged_transfer<S, R>(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    run_ring_staged_transfer(inner, block_size, producer, None, |error| {
        let error = error.to_string();
        record_tape_io_fence_for_transfer_error(
            state,
            selected,
            tape_io_fence_reason_for_transfer_error(&error),
            &error,
        )
    })?
    .result
}

fn run_counted_fenced_staged_transfer<S, R>(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    inner: &mut CountingBlockSink<'_, S>,
    block_size: usize,
    overlap_control: Option<Arc<crate::append_ring::AppendRingControl>>,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    let tape_write_control = overlap_control.as_ref().map(Arc::clone);
    let outcome =
        run_ring_staged_transfer(inner, block_size, producer, tape_write_control, |error| {
            if overlap_control
                .as_ref()
                .is_some_and(|control| !control.tape_started())
            {
                return Ok(());
            }
            let error = error.to_string();
            record_tape_io_fence_for_transfer_error(
                state,
                selected,
                tape_io_fence_reason_for_transfer_error(&error),
                &error,
            )
        })?;
    inner.stats.record_staging(&outcome.staging_diagnostics);
    outcome.result
}

struct RingStagedTransferOutcome<R> {
    result: Result<R, PoolWriteError>,
    staging_diagnostics: StagingPhaseDiagnostics,
}

fn run_ring_staged_transfer<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
    tape_write_control: Option<Arc<crate::append_ring::AppendRingControl>>,
    mut on_safety_error: impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<RingStagedTransferOutcome<R>, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    let ring_buffers = usize::try_from(inner.staging_ring_buffers()).map_err(|_| {
        PoolWriteError::InvalidInput("staging ring depth does not fit usize".into())
    })?;
    if !(remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS as usize..=MAX_PIPELINE_WINDOW_BUFFERS)
        .contains(&ring_buffers)
    {
        return Err(PoolWriteError::InvalidInput(format!(
            "staging ring depth must be {}..={}, got {ring_buffers}",
            remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS,
            remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS,
        )));
    }
    let caps = StagedSinkCaps::from_inner(inner, block_size);
    let batch_bytes = block_size
        .checked_mul(caps.batch_blocks as usize)
        .ok_or_else(|| PoolWriteError::InvalidInput("staging batch bytes overflow".into()))?;
    let ring_bytes = batch_bytes
        .checked_mul(ring_buffers)
        .ok_or_else(|| PoolWriteError::InvalidInput("staging ring bytes overflow".into()))?;
    tracing::info!(
        target: "remanence_write_diag",
        phase = "staging_ring_open",
        effective_mode = "fixed_pipelined",
        staging_ring_buffers = ring_buffers,
        effective_batch_blocks = caps.batch_blocks,
        block_size_bytes = block_size,
        effective_ring_bytes = ring_bytes,
        "remanence_write_diag",
    );

    let accounting = Arc::new(RingAccounting::default());
    let (free_tx, free_rx) = std_mpsc::sync_channel(ring_buffers);
    for _ in 0..ring_buffers {
        let buffer = PageAlignedBuffer::try_new(batch_bytes, Arc::clone(&accounting))?;
        free_tx
            .try_send(buffer)
            .map_err(|_| PoolWriteError::InvalidInput("failed to seed staging free ring".into()))?;
    }
    let (submit_tx, submit_rx) = std_mpsc::sync_channel(1);
    let (submitter_done_tx, submitter_done_rx) = std_mpsc::channel();
    let poison = Arc::new(Mutex::new(None::<String>));
    inner.reset_pipelined_write_diagnostics();
    let result = std::thread::scope(|scope| {
        let producer_poison = Arc::clone(&poison);
        let producer_handle = scope.spawn(move || {
            let mut staged = StagedBlockSink::new(
                submit_tx,
                free_rx,
                submitter_done_rx,
                producer_poison,
                caps,
                ring_buffers,
            );
            let result = producer(&mut staged);
            match result {
                Ok(value) => {
                    let (finish_result, diagnostics) = staged.finish();
                    (
                        finish_result.map(|()| value).map_err(PoolWriteError::from),
                        diagnostics,
                    )
                }
                Err(err) => {
                    let diagnostics = staged.abort(err.to_string());
                    (Err(err), diagnostics)
                }
            }
        });

        let submitter_result = drain_pipelined_transfer(
            inner,
            submit_rx,
            free_tx,
            &poison,
            tape_write_control.as_deref(),
            &mut on_safety_error,
        );
        let _ = submitter_done_tx.send(());
        let (producer_result, staging_diagnostics) = match producer_handle.join() {
            Ok(result) => result,
            Err(_) => (
                Err(PoolWriteError::InvalidInput(
                    "pipelined staging producer thread panicked".into(),
                )),
                StagingPhaseDiagnostics::default(),
            ),
        };
        log_staging_phase_diagnostics(&staging_diagnostics);
        let result = match submitter_result {
            Ok(()) => match producer_result {
                Ok(value) => Ok(value),
                Err(primary) => {
                    let safety_error = TapeIoError::OperationFailed(primary.to_string());
                    match on_safety_error(&safety_error) {
                        Ok(()) => Err(primary),
                        Err(secondary) => Err(attach_secondary(
                            primary,
                            "tape-I/O fence persistence",
                            secondary,
                        )),
                    }
                }
            },
            Err(err) => Err(err),
        };
        (result, staging_diagnostics)
    });
    let (mut result, staging_diagnostics) = result;
    inner.publish_pipelined_write_diagnostics();
    let allocated = accounting.allocated.load(Ordering::Relaxed);
    let dropped = accounting.dropped.load(Ordering::Relaxed);
    if allocated != dropped {
        let imbalance = PoolWriteError::InvalidInput(format!(
            "staging ring accounting imbalance: allocated={allocated} dropped={dropped}"
        ));
        result = match result {
            Ok(_) => {
                let safety_error = TapeIoError::OperationFailed(imbalance.to_string());
                let fence_result = on_safety_error(&safety_error);
                inner.flush_pending_pipeline_audit();
                match fence_result {
                    Ok(()) => Err(imbalance),
                    Err(secondary) => Err(attach_secondary(
                        imbalance,
                        "tape-I/O fence persistence",
                        secondary,
                    )),
                }
            }
            Err(primary) => Err(attach_secondary(
                primary,
                "staging ring accounting",
                imbalance,
            )),
        };
    }
    Ok(RingStagedTransferOutcome {
        result,
        staging_diagnostics,
    })
}

fn log_staging_phase_diagnostics(diagnostics: &StagingPhaseDiagnostics) {
    tracing::info!(
        target: "remanence_write_diag",
        phase = "staging_subphases",
        staging_wait_samples = diagnostics.wait_us.samples,
        staging_wait_p50_us = diagnostics.wait_us.percentile(50, 100),
        staging_wait_p95_us = diagnostics.wait_us.percentile(95, 100),
        staging_wait_max_us = diagnostics.wait_us.max_us,
        staging_wait_mean_us = diagnostics.wait_us.mean(),
        refill_samples = diagnostics.refill_us.samples,
        refill_p50_us = diagnostics.refill_us.percentile(50, 100),
        refill_p95_us = diagnostics.refill_us.percentile(95, 100),
        refill_max_us = diagnostics.refill_us.max_us,
        refill_mean_us = diagnostics.refill_us.mean(),
        "remanence_write_diag",
    );
}

fn drain_pipelined_transfer<S: BlockSink + ?Sized>(
    inner: &mut S,
    rx: std_mpsc::Receiver<PipelinedSinkCommand>,
    free_tx: std_mpsc::SyncSender<PageAlignedBuffer>,
    poison: &Arc<Mutex<Option<String>>>,
    tape_write_control: Option<&crate::append_ring::AppendRingControl>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<(), PoolWriteError> {
    let mut completed_since_barrier: Option<WriteBatchOutcome> = None;
    while let Ok(command) = rx.recv() {
        if let Some(message) = staged_poison_message(poison) {
            discard_pipelined_command(command, &free_tx, message);
            continue;
        }
        let result = match command {
            PipelinedSinkCommand::WriteWindow(window) => execute_pipelined_window(
                inner,
                window,
                &free_tx,
                tape_write_control,
                on_safety_error,
            )
            .map(|window_outcome| {
                completed_since_barrier = Some(match completed_since_barrier {
                    Some(accumulated) => merge_batch_outcomes(accumulated, window_outcome),
                    None => window_outcome,
                });
            }),
            PipelinedSinkCommand::Barrier { reply } => {
                let _ = reply.send(Ok(completed_since_barrier.take()));
                Ok(())
            }
            PipelinedSinkCommand::WriteFilemarks { count, reply } => {
                if let Some(control) = tape_write_control {
                    control.mark_tape_started();
                }
                match inner.write_filemarks_pipelined(count) {
                    Ok(outcome) => {
                        let _ = reply.send(Ok(outcome));
                        Ok(())
                    }
                    Err(err) => {
                        let failure = finish_transfer_failure(inner, err, on_safety_error);
                        let _ = reply.send(Err(failure.to_string()));
                        Err(failure)
                    }
                }
            }
            PipelinedSinkCommand::WriteFilemarksImmediate { count, reply } => {
                if let Some(control) = tape_write_control {
                    control.mark_tape_started();
                }
                match inner.write_filemarks_immediate(count) {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        let failure = finish_transfer_failure(inner, err, on_safety_error);
                        let _ = reply.send(Err(failure.to_string()));
                        Err(failure)
                    }
                }
            }
            PipelinedSinkCommand::SpaceToEndOfData { reply } => {
                match inner.space_to_end_of_data_pipelined() {
                    Ok(position) => {
                        let _ = reply.send(Ok(position));
                        Ok(())
                    }
                    Err(err) => {
                        let failure = finish_transfer_failure(inner, err, on_safety_error);
                        let _ = reply.send(Err(failure.to_string()));
                        Err(failure)
                    }
                }
            }
            PipelinedSinkCommand::Locate { lba, reply } => match inner.locate(lba) {
                Ok(position) => {
                    let _ = reply.send(Ok(position));
                    Ok(())
                }
                Err(err) => {
                    let failure = finish_transfer_failure(inner, err, on_safety_error);
                    let _ = reply.send(Err(failure.to_string()));
                    Err(failure)
                }
            },
            PipelinedSinkCommand::Position { reply } => match inner.position_pipelined() {
                Ok(position) => {
                    let _ = reply.send(Ok(position));
                    Ok(())
                }
                Err(err) => {
                    let failure = finish_transfer_failure(inner, err, on_safety_error);
                    let _ = reply.send(Err(failure.to_string()));
                    Err(failure)
                }
            },
        };
        if let Err(err) = result {
            set_staged_poison(poison, err.to_string());
            while let Ok(queued) = rx.try_recv() {
                discard_pipelined_command(queued, &free_tx, err.to_string());
            }
            drop(rx);
            drop(free_tx);
            return Err(err);
        }
    }
    Ok(())
}

fn attach_secondary(
    primary: PoolWriteError,
    context: &'static str,
    secondary: PoolWriteError,
) -> PoolWriteError {
    PoolWriteError::TransferWithSecondary {
        primary: primary.to_string(),
        context,
        secondary: secondary.to_string(),
    }
}

fn finish_transfer_failure<S: BlockSink + ?Sized>(
    inner: &mut S,
    error: TapeIoError,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> PoolWriteError {
    let fence_result = on_safety_error(&error);
    inner.flush_pending_pipeline_audit();
    let primary = PoolWriteError::from(error);
    match fence_result {
        Ok(()) => primary,
        Err(secondary) => attach_secondary(primary, "tape-I/O fence persistence", secondary),
    }
}

fn execute_pipelined_window<S: BlockSink + ?Sized>(
    inner: &mut S,
    mut window: PipelinedWindow,
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    tape_write_control: Option<&crate::append_ring::AppendRingControl>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<WriteBatchOutcome, PoolWriteError> {
    let command_count = window.len as u32;
    let bytes = window.bytes;
    let first_records = window.first_records();
    let last_records = window.last_records();
    inner.begin_pipelined_write_window(command_count, bytes, first_records, last_records);
    let started = Instant::now();
    let mut completed: Option<WriteBatchOutcome> = None;
    for index in 0..window.len {
        let batch = window.batches[index]
            .take()
            .expect("window slot below len is occupied");
        let requested = batch.records;
        let requested_bytes = u64::from(requested) * u64::from(batch.block_size_bytes);
        if let Some(control) = tape_write_control {
            control.mark_tape_started();
        }
        let result = inner.write_block_batch_pipelined(
            batch.buffer.bytes(),
            batch.block_size_bytes,
            &batch.cdb,
        );
        let buffer_return_error = return_ring_buffer(free_tx, batch.buffer).err();
        let outcome = match result {
            Ok(outcome)
                if outcome.records_written == requested
                    && u64::from(outcome.bytes_written) == requested_bytes
                    && !outcome.end_of_medium =>
            {
                if let Some(error) = buffer_return_error {
                    return finish_pipelined_window_failure(
                        inner,
                        &mut window,
                        free_tx,
                        on_safety_error,
                        command_count,
                        bytes,
                        first_records,
                        last_records,
                        TapeIoError::OperationFailed(error.to_string()),
                        None,
                    );
                }
                outcome
            }
            Ok(outcome) => {
                let err = TapeIoError::PartialBatchUncommittable {
                    requested_records: requested,
                    written_records: outcome.records_written,
                    requested_bytes,
                    written_bytes: u64::from(outcome.bytes_written),
                    end_of_medium: outcome.end_of_medium,
                    sense: None,
                };
                return finish_pipelined_window_failure(
                    inner,
                    &mut window,
                    free_tx,
                    on_safety_error,
                    command_count,
                    bytes,
                    first_records,
                    last_records,
                    err,
                    buffer_return_error,
                );
            }
            Err(err) => {
                return finish_pipelined_window_failure(
                    inner,
                    &mut window,
                    free_tx,
                    on_safety_error,
                    command_count,
                    bytes,
                    first_records,
                    last_records,
                    err,
                    buffer_return_error,
                );
            }
        };
        completed = Some(match completed {
            Some(accumulated) => merge_batch_outcomes(accumulated, outcome),
            None => outcome,
        });
    }
    inner.finish_pipelined_write_window_success(
        command_count,
        bytes,
        first_records,
        last_records,
        started.elapsed(),
    );
    completed.ok_or_else(|| PoolWriteError::InvalidInput("empty pipelined window".into()))
}

fn merge_batch_outcomes(
    accumulated: WriteBatchOutcome,
    next: WriteBatchOutcome,
) -> WriteBatchOutcome {
    WriteBatchOutcome::from_computed_position(
        accumulated
            .records_written
            .saturating_add(next.records_written),
        accumulated.bytes_written.saturating_add(next.bytes_written),
        accumulated.early_warning || next.early_warning,
        accumulated.end_of_medium || next.end_of_medium,
        next.position_after,
    )
}

#[allow(clippy::too_many_arguments)]
fn finish_pipelined_window_failure<S: BlockSink + ?Sized, T>(
    inner: &mut S,
    window: &mut PipelinedWindow,
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
    command_count: u32,
    bytes: u64,
    first_records: u32,
    last_records: u32,
    error: TapeIoError,
    mut secondary: Option<PoolWriteError>,
) -> Result<T, PoolWriteError> {
    for batch in window.batches.iter_mut().filter_map(Option::take) {
        if let Err(error) = return_ring_buffer(free_tx, batch.buffer) {
            secondary.get_or_insert(error);
        }
    }
    let fence_result = on_safety_error(&error);
    inner.finish_pipelined_write_window_error(
        command_count,
        bytes,
        first_records,
        last_records,
        &error,
    );
    let mut primary = PoolWriteError::from(error);
    if let Err(error) = fence_result {
        primary = attach_secondary(primary, "tape-I/O fence persistence", error);
    }
    if let Some(error) = secondary {
        primary = attach_secondary(primary, "staging buffer return", error);
    }
    Err(primary)
}

fn return_ring_buffer(
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    mut buffer: PageAlignedBuffer,
) -> Result<(), PoolWriteError> {
    buffer.reset();
    free_tx.try_send(buffer).map_err(|err| match err {
        std_mpsc::TrySendError::Full(_) => PoolWriteError::InvalidInput(
            "staging buffer return path filled despite ring-sized capacity".into(),
        ),
        std_mpsc::TrySendError::Disconnected(_) => {
            PoolWriteError::InvalidInput("staging buffer return path disconnected".into())
        }
    })
}

fn discard_pipelined_command(
    command: PipelinedSinkCommand,
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    message: String,
) {
    match command {
        PipelinedSinkCommand::WriteWindow(mut window) => {
            for batch in window.batches.iter_mut().filter_map(Option::take) {
                let _ = return_ring_buffer(free_tx, batch.buffer);
            }
        }
        PipelinedSinkCommand::Barrier { reply } => {
            let _ = reply.send(Err(message));
        }
        PipelinedSinkCommand::WriteFilemarks { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        PipelinedSinkCommand::WriteFilemarksImmediate { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        PipelinedSinkCommand::SpaceToEndOfData { reply }
        | PipelinedSinkCommand::Locate { reply, .. }
        | PipelinedSinkCommand::Position { reply } => {
            let _ = reply.send(Err(message));
        }
    }
}

fn records_in_staged_batch(data: &[u8], block_size_bytes: u32) -> Result<u32, TapeIoError> {
    if block_size_bytes == 0 {
        return Err(TapeIoError::OperationFailed(
            "staged write batch block size must be nonzero".to_string(),
        ));
    }
    let block_size = block_size_bytes as usize;
    if data.is_empty() || data.len() % block_size != 0 {
        return Err(TapeIoError::OperationFailed(
            "staged write batch must contain whole records".to_string(),
        ));
    }
    u32::try_from(data.len() / block_size).map_err(|_| {
        TapeIoError::OperationFailed("staged write batch record count overflow".to_string())
    })
}

fn staged_poison_message(poison: &Arc<Mutex<Option<String>>>) -> Option<String> {
    poison.lock().unwrap_or_else(|err| err.into_inner()).clone()
}

fn set_staged_poison(poison: &Arc<Mutex<Option<String>>>, message: String) {
    let mut guard = poison.lock().unwrap_or_else(|err| err.into_inner());
    guard.get_or_insert(message);
}

impl<'a> LiveCounterBlockSink<'a> {
    pub(crate) fn new(
        inner: &'a mut dyn BlockSink,
        live_write_counter: Arc<crate::DriveByteCounters>,
        block_size_bytes: u32,
    ) -> Self {
        live_write_counter.configure_tape_io(
            inner.staging_ring_buffers(),
            inner.write_batch_blocks(block_size_bytes),
        );
        Self {
            inner,
            live_write_counter,
        }
    }
}

impl<'a, S: BlockSink + ?Sized> CountingBlockSink<'a, S> {
    fn new(inner: &'a mut S, block_size: u32) -> Self {
        let write_batch_blocks = inner.requested_write_batch_blocks().max(1);
        let effective_batch_blocks = inner.write_batch_blocks(block_size).max(1);
        let position_check_bytes = inner.position_check_bytes();
        let staging_ring_buffers = inner.staging_ring_buffers();
        Self {
            inner,
            stats: BlockSinkStats {
                write_batch_blocks,
                effective_batch_blocks,
                position_check_bytes,
                staging_ring_buffers,
                ..BlockSinkStats::default()
            },
        }
    }

    fn stats(&self) -> BlockSinkStats {
        let mut stats = self.stats;
        let diagnostics = self.inner.pipelined_write_diagnostics();
        stats.gap_samples = diagnostics.gap_samples;
        stats.gap_p50_us = diagnostics.gap_p50_us;
        stats.gap_p95_us = diagnostics.gap_p95_us;
        stats.gap_max_us = diagnostics.gap_max_us;
        stats.gap_mean_us = diagnostics.gap_mean_us;
        stats.ioctl_samples = diagnostics.ioctl_samples;
        stats.ioctl_p50_us = diagnostics.ioctl_p50_us;
        stats.ioctl_p95_us = diagnostics.ioctl_p95_us;
        stats.ioctl_max_us = diagnostics.ioctl_max_us;
        stats.ioctl_mean_us = diagnostics.ioctl_mean_us;
        stats.first_60s_ioctl_samples = diagnostics.first_60s_ioctl_samples;
        stats.first_60s_ioctl_p50_us = diagnostics.first_60s_ioctl_p50_us;
        stats.first_60s_ioctl_p95_us = diagnostics.first_60s_ioctl_p95_us;
        stats.first_60s_ioctl_max_us = diagnostics.first_60s_ioctl_max_us;
        stats.first_60s_ioctl_mean_us = diagnostics.first_60s_ioctl_mean_us;
        stats.accounting_samples = diagnostics.accounting_samples;
        stats.accounting_p50_us = diagnostics.accounting_p50_us;
        stats.accounting_p95_us = diagnostics.accounting_p95_us;
        stats.accounting_max_us = diagnostics.accounting_max_us;
        stats.accounting_mean_us = diagnostics.accounting_mean_us;
        stats.cadence_us = diagnostics.cadence_us;
        stats.effective_feed_bytes_per_second = diagnostics.effective_feed_bytes_per_second;
        stats.time_to_first_ioctl_ms = diagnostics.time_to_first_ioctl_ms;
        stats.steady_reached = diagnostics.steady_reached;
        stats.time_to_steady_ms = diagnostics.time_to_steady_ms;
        stats.steady_window_seconds = diagnostics.steady_window_seconds;
        stats.steady_threshold_percent = diagnostics.steady_threshold_percent;
        stats.ramp_observation_seconds = diagnostics.ramp_observation_seconds;
        stats
    }
}

impl<'a> BlockSink for LiveCounterBlockSink<'a> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.live_write_counter
            .record_write_bytes(u64::from(outcome.bytes_written));
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.live_write_counter
            .record_write_bytes(u64::from(outcome.bytes_written));
        Ok(outcome)
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let result = self
            .inner
            .write_block_batch_pipelined(buf, block_size_bytes, cdb);
        match &result {
            Ok(outcome) => self
                .live_write_counter
                .record_write_bytes(u64::from(outcome.bytes_written)),
            Err(TapeIoError::GoodWriteTripwire { outcome, .. }) => self
                .live_write_counter
                .record_write_bytes(u64::from(outcome.bytes_written)),
            Err(_) => {}
        }
        result
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.inner.staging_ring_buffers()
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        self.inner.pipelined_write_diagnostics()
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.inner.reset_pipelined_write_diagnostics();
        self.live_write_counter
            .record_tape_io_diagnostics(PipelinedWriteDiagnostics::default());
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.live_write_counter
            .record_tape_io_diagnostics(self.inner.pipelined_write_diagnostics());
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.inner
            .begin_pipelined_write_window(command_count, bytes, first_records, last_records);
    }

    fn finish_pipelined_write_window_success(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        duration: Duration,
    ) {
        self.inner.finish_pipelined_write_window_success(
            command_count,
            bytes,
            first_records,
            last_records,
            duration,
        );
    }

    fn finish_pipelined_write_window_error(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        error: &TapeIoError,
    ) {
        self.inner.finish_pipelined_write_window_error(
            command_count,
            bytes,
            first_records,
            last_records,
            error,
        );
    }

    fn flush_pending_pipeline_audit(&mut self) {
        self.inner.flush_pending_pipeline_audit();
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks_pipelined(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
    }

    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data_pipelined()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }

    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position_pipelined()
    }
}

impl<'a, S: BlockSink + ?Sized> BlockSink for CountingBlockSink<'a, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.stats
            .record_block(u64::from(outcome.bytes_written), outcome.early_warning);
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.stats
            .record_block(u64::from(outcome.bytes_written), outcome.early_warning);
        Ok(outcome)
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let result = self
            .inner
            .write_block_batch_pipelined(buf, block_size_bytes, cdb);
        match &result {
            Ok(outcome) => self
                .stats
                .record_block(u64::from(outcome.bytes_written), outcome.early_warning),
            Err(TapeIoError::GoodWriteTripwire { outcome, .. }) => self
                .stats
                .record_block(u64::from(outcome.bytes_written), outcome.early_warning),
            Err(_) => {}
        }
        result
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.inner.staging_ring_buffers()
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        self.inner.pipelined_write_diagnostics()
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.inner.reset_pipelined_write_diagnostics();
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.inner.publish_pipelined_write_diagnostics();
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.inner
            .begin_pipelined_write_window(command_count, bytes, first_records, last_records);
    }

    fn finish_pipelined_write_window_success(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        duration: Duration,
    ) {
        self.inner.finish_pipelined_write_window_success(
            command_count,
            bytes,
            first_records,
            last_records,
            duration,
        );
    }

    fn finish_pipelined_write_window_error(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        error: &TapeIoError,
    ) {
        self.inner.finish_pipelined_write_window_error(
            command_count,
            bytes,
            first_records,
            last_records,
            error,
        );
    }

    fn flush_pending_pipeline_audit(&mut self) {
        self.inner.flush_pending_pipeline_audit();
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let started = Instant::now();
        let outcome = self.inner.write_filemarks(count)?;
        self.stats
            .record_filemarks(count, outcome.early_warning, started.elapsed());
        Ok(outcome)
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let started = Instant::now();
        let outcome = self.inner.write_filemarks_pipelined(count)?;
        self.stats
            .record_filemarks(count, outcome.early_warning, started.elapsed());
        Ok(outcome)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        let started = Instant::now();
        self.inner.write_filemarks_immediate(count)?;
        self.stats.record_filemarks(count, false, started.elapsed());
        Ok(())
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.space_to_end_of_data()?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.space_to_end_of_data_pipelined()?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.locate(lba)?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.position()?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.position_pipelined()?;
        self.stats.record_position(position);
        Ok(position)
    }
}

/// Write-side hysteresis gate layered immediately above the existing bounded
/// hardware staging funnel. A pause flushes the current safe batch, waits for
/// the receive ring to refill, and re-proves the exact next physical LBA.
struct OverlapBlockSink<'a> {
    inner: &'a mut dyn BlockSink,
    control: Arc<crate::append_ring::AppendRingControl>,
    expected_initial_lba: u64,
    expected_next_lba: u64,
    initial_position_proved: bool,
    write_started: bool,
    low_water_events: u64,
}

impl OverlapBlockSink<'_> {
    fn ensure_prefill(&self) -> Result<(), TapeIoError> {
        if self.control.prefill_satisfied() {
            Ok(())
        } else {
            Err(TapeIoError::OperationFailed(
                "overlap first-block gate reached before high-water prefill and live-source validation"
                    .to_string(),
            ))
        }
    }

    fn prove_position(
        &self,
        observed: TapePosition,
        expected_lba: u64,
        context: &str,
    ) -> Result<(), TapeIoError> {
        if observed.partition != 0 || observed.lba != expected_lba {
            return Err(TapeIoError::OperationFailed(format!(
                "overlap write position drift during {context}: expected partition 0 lba {expected_lba}, observed partition {} lba {}",
                observed.partition, observed.lba
            )));
        }
        Ok(())
    }

    fn prove_initial_position(&mut self) -> Result<(), TapeIoError> {
        if !self.initial_position_proved {
            self.ensure_prefill()?;
            let observed = self.inner.position()?;
            self.prove_position(observed, self.expected_initial_lba, "first-block gate")?;
            self.initial_position_proved = true;
        }
        Ok(())
    }

    fn pause_if_low(&mut self) -> Result<(), TapeIoError> {
        if !self.write_started || !self.control.should_pause() {
            return Ok(());
        }
        let expected = self.expected_next_lba;
        let before_pause = self.inner.position()?;
        self.prove_position(before_pause, expected, "low-water pause boundary")?;
        self.low_water_events = self.low_water_events.saturating_add(1);
        let pause_started = Instant::now();
        tracing::info!(
            target: "remanence_write_diag",
            phase = "overlap_pause",
            low_water_events = self.low_water_events,
            ring_occupancy_bytes = self.control.occupancy_bytes(),
            ring_low_bytes = self.control.low_bytes(),
            expected_next_lba = expected,
            "remanence_write_diag",
        );
        self.control
            .wait_for_resume()
            .map_err(|err| TapeIoError::OperationFailed(format!("overlap refill failed: {err}")))?;
        let observed = self.inner.position()?;
        let proof = self.prove_position(observed, expected, "low-water resume");
        tracing::info!(
            target: "remanence_write_diag",
            phase = "overlap_resume_proof",
            low_water_events = self.low_water_events,
            ring_occupancy_bytes = self.control.occupancy_bytes(),
            ring_high_bytes = self.control.high_bytes(),
            pause_duration_ms = crate::diagnostics::duration_ms(pause_started.elapsed()),
            expected_next_lba = expected,
            observed_next_lba = observed.lba,
            resume_proof_ok = proof.is_ok(),
            "remanence_write_diag",
        );
        proof
    }
}

impl BlockSink for OverlapBlockSink<'_> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.prove_initial_position()?;
        if let Some(message) = self.control.failure_message() {
            return Err(TapeIoError::OperationFailed(format!(
                "overlap source failed before WRITE submission: {message}"
            )));
        }
        self.pause_if_low()?;
        self.write_started = true;
        let outcome = self.inner.write_block(buf)?;
        let expected = self.expected_next_lba.checked_add(1).ok_or_else(|| {
            TapeIoError::OperationFailed("overlap expected next LBA overflow".to_string())
        })?;
        self.prove_position(outcome.position_after, expected, "WRITE completion")?;
        self.expected_next_lba = expected;
        Ok(outcome)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.ensure_prefill()?;
        let observed = self.inner.space_to_end_of_data()?;
        self.prove_position(observed, self.expected_initial_lba, "append-position gate")?;
        self.initial_position_proved = true;
        Ok(observed)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.ensure_prefill()?;
        let observed = self.inner.locate(lba)?;
        self.prove_position(observed, lba, "checkpoint recovery LOCATE")?;
        self.expected_next_lba = lba;
        self.initial_position_proved = true;
        Ok(observed)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

fn with_overlap_sink<R>(
    inner: &mut dyn BlockSink,
    control: Option<Arc<crate::append_ring::AppendRingControl>>,
    expected_initial_lba: u64,
    operation: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError>,
) -> Result<R, PoolWriteError> {
    match control {
        Some(control) => {
            let mut gated = OverlapBlockSink {
                inner,
                control,
                expected_initial_lba,
                expected_next_lba: expected_initial_lba,
                initial_position_proved: false,
                write_started: false,
                low_water_events: 0,
            };
            operation(&mut gated)
        }
        None => operation(inner),
    }
}

#[cfg(test)]
struct PerObjectTestJournal {
    tape_uuid: [u8; 16],
    bundles: Vec<CommittedBundle>,
}

#[cfg(test)]
impl TapeFileJournal for PerObjectTestJournal {
    fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
    }

    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
        self.bundles.push(bundle.clone());
        Ok(())
    }

    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        let retained_end = self
            .bundles
            .iter()
            .rposition(|bundle| bundle.kind == CommittedBundleKind::CheckpointedThrough)
            .map_or(0, |index| index + 1);
        let retained = &self.bundles[..retained_end];
        let last = retained
            .iter()
            .rev()
            .find(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough);
        Ok(CommittedState {
            entries: retained
                .iter()
                .filter(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough)
                .flat_map(|bundle| bundle.entries.iter().cloned())
                .collect(),
            highest_protected_ordinal: last.map_or(0, |bundle| bundle.highest_protected_ordinal),
            total_committed_ordinals: last.map_or(0, |bundle| bundle.total_committed_ordinals),
            orphaned_bundles: self.bundles[retained_end..].to_vec(),
        })
    }
}

#[cfg(test)]
fn write_parity_object_to_selected_tape<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut CountingBlockSink<'_, S>,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared_write: PreparedPoolWrite,
    scheme: ParityScheme,
) -> Result<PoolWriteResult, PoolWriteError> {
    let PreparedPoolWrite { prepared, stored } = prepared_write;
    let tape_uuid = selected.tape_uuid;
    let block_size = selected.block_size;
    let mut parity_journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let overlap_control = prepared.overlap_control();
    let capacity_blocks = parity_capacity_basis_blocks(state, pool_cfg, &selected)?;
    let io_memory = crate::io_memory::IoMemoryReservation::new(
        remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
    )
    .map_err(PoolWriteError::InvalidInput)?;
    let transfer_started = Instant::now();
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> =
        run_counted_fenced_staged_transfer(
            state,
            &selected,
            sink,
            block_size as usize,
            overlap_control.as_ref().map(Arc::clone),
            |staged| {
                with_overlap_sink(staged, overlap_control, 0, |gated| {
                    let mut raw = BlockSinkRawTapeSink::new(gated);
                    let mut parity = ParitySink::new_with_journal(
                        &mut raw,
                        &mut parity_journal,
                        scheme.clone(),
                        tape_uuid,
                        block_size,
                    )?;
                    parity.write_bootstrap()?;
                    let report = match &stored {
                        PreparedStoredObject::Plaintext => {
                            let mut readers = open_prepared_readers(&prepared)?;
                            let capacity = reserve_parity_object_capacity(
                                parity.terminal_triple_capacity_runtime_state()?,
                                parity.scheme(),
                                &selected,
                                (pool_cfg, 1, 0),
                                capacity_blocks,
                                prepared.plan.layout.projected_size_blocks,
                                &io_memory,
                            )?;
                            let (capacity, _spool_permit) = capacity.into_parts();
                            Ok(write_prepared_object_to_parity_from_readers(
                                &mut parity,
                                tape_uuid,
                                &prepared.options,
                                &prepared.files,
                                &mut readers,
                                capacity,
                            )?)
                        }
                        PreparedStoredObject::Encrypted(encrypted) => {
                            let capacity = reserve_parity_object_capacity(
                                parity.terminal_triple_capacity_runtime_state()?,
                                parity.scheme(),
                                &selected,
                                (pool_cfg, 1, 0),
                                capacity_blocks,
                                encrypted.envelope.stored_size_blocks,
                                &io_memory,
                            )?;
                            let (capacity, _spool_permit) = capacity.into_parts();
                            write_encrypted_object_to_parity(
                                &mut parity,
                                tape_uuid,
                                &prepared,
                                encrypted,
                                capacity,
                            )
                        }
                    }?;
                    Ok(report)
                })
            },
        );
    let transfer_elapsed = transfer_started.elapsed();
    let write_report = match write_report {
        Ok(write_report) => {
            let stats = sink.stats();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
                false,
                TransferDiagnosticOutcome {
                    stats,
                    elapsed: transfer_elapsed,
                    status: "ok",
                    error: None,
                },
            );
            (write_report, stats)
        }
        Err(err) => {
            let error = err.to_string();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
                false,
                TransferDiagnosticOutcome {
                    stats: sink.stats(),
                    elapsed: transfer_elapsed,
                    status: "error",
                    error: Some(error.as_str()),
                },
            );
            return Err(err);
        }
    };
    let (write_report, transfer_stats) = write_report;

    let commit_started = Instant::now();
    let commit_result = commit_pool_write(
        state,
        &selected,
        &prepared,
        &write_report,
        CommitPoolWriteProjection {
            first_parity_data_ordinal: write_report.catalog.object_copy.first_parity_data_ordinal,
            protected_until_ordinal: write_report.catalog.object_copy.protected_until_ordinal,
            scheme: Some(scheme),
            copy_representation: stored.copy_representation(),
        },
        pool_cfg,
        transfer_stats.early_warning,
    );
    let commit_elapsed = commit_started.elapsed();
    let sealed_after_write = match commit_result {
        Ok(sealed_after_write) => {
            log_commit_diagnostics(&request, &selected, &prepared, commit_elapsed, "ok", None);
            sealed_after_write
        }
        Err(err) => {
            let error = err.to_string();
            log_commit_diagnostics(
                &request,
                &selected,
                &prepared,
                commit_elapsed,
                "error",
                Some(error.as_str()),
            );
            return Err(err);
        }
    };
    pool_write_result(
        request,
        selected,
        prepared,
        stored.copy_representation(),
        write_report,
        AppendCommitDiagnostics {
            filemark_write_drain: transfer_stats.filemark_write_drain,
            catalog_journal_fsync: commit_elapsed,
        },
        sealed_after_write,
        None,
    )
}

fn write_no_parity_object_to_selected_tape<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut CountingBlockSink<'_, S>,
    _pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared_write: PreparedPoolWrite,
    durability: PoolWriteDurability,
) -> Result<PoolWriteResult, PoolWriteError> {
    let PreparedPoolWrite { prepared, stored } = prepared_write;
    let tape_uuid = selected.tape_uuid;
    let append = match &durability {
        #[cfg(test)]
        PoolWriteDurability::PerObject => no_parity_append_context(state, &selected)?,
        PoolWriteDurability::Batched(context) => context.append,
    };
    let expected_initial_lba = append.expected_append_lba()?;
    let overlap_control = prepared.overlap_control();
    let transfer_started = Instant::now();
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> =
        run_counted_fenced_staged_transfer(
            state,
            &selected,
            sink,
            selected.block_size as usize,
            overlap_control.as_ref().map(Arc::clone),
            |staged| {
                with_overlap_sink(staged, overlap_control, expected_initial_lba, |gated| {
                    match &durability {
                        #[cfg(test)]
                        PoolWriteDurability::PerObject if append.fresh_tape => {
                            write_no_parity_bootstrap(
                                gated,
                                tape_uuid,
                                selected.block_size,
                                &prepared.write_timestamp,
                            )?
                        }
                        PoolWriteDurability::Batched(BatchedNoParityAppendContext {
                            position: BatchedAppendPosition::FreshTape,
                            ..
                        }) => write_no_parity_bootstrap(
                            gated,
                            tape_uuid,
                            selected.block_size,
                            &prepared.write_timestamp,
                        )?,
                        #[cfg(test)]
                        PoolWriteDurability::PerObject => {
                            position_no_parity_append(gated)?;
                        }
                        PoolWriteDurability::Batched(BatchedNoParityAppendContext {
                            position: BatchedAppendPosition::JournalEod(lba),
                            ..
                        }) => position_no_parity_append_at_checkpoint(gated, *lba)?,
                        PoolWriteDurability::Batched(BatchedNoParityAppendContext {
                            position: BatchedAppendPosition::CurrentBoundary(lba),
                            ..
                        }) => prove_no_parity_append_boundary(gated, *lba)?,
                    }
                    let report = match &stored {
                        PreparedStoredObject::Plaintext => {
                            let mut readers = open_prepared_readers(&prepared)?;
                            let mut streams = Vec::with_capacity(prepared.files.len());
                            for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
                                streams.push(RemTarFileStream::new(
                                    file.spec.clone(),
                                    reader.as_mut(),
                                ));
                            }
                            let mut object_sink = ObjectDigestBlockSink::new(gated);
                            let layout = write_rem_tar_object_from_readers(
                                &mut object_sink,
                                &prepared.options,
                                &mut streams,
                            )
                            .map_err(StreamingError::from)?;
                            let object_digest = object_sink.finish_digest();
                            let filemark_outcome = write_object_delimiter(
                                gated,
                                &durability,
                                append,
                                layout.projected_size_blocks,
                            )?;
                            no_parity_write_report(
                                tape_uuid,
                                &prepared,
                                layout,
                                object_digest,
                                filemark_outcome,
                                append,
                            )
                        }
                        PreparedStoredObject::Encrypted(encrypted) => {
                            write_fixed_blocks(
                                gated,
                                prepared.options.chunk_size,
                                &encrypted.sealed,
                            )?;
                            let filemark_outcome = write_object_delimiter(
                                gated,
                                &durability,
                                append,
                                encrypted.envelope.stored_size_blocks,
                            )?;
                            no_parity_encrypted_write_report(
                                tape_uuid,
                                &prepared,
                                encrypted,
                                filemark_outcome,
                                append,
                            )
                        }
                    }?;
                    Ok(report)
                })
            },
        );
    let transfer_elapsed = transfer_started.elapsed();
    let write_report = match write_report {
        Ok(write_report) => {
            let stats = sink.stats();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
                matches!(&durability, PoolWriteDurability::Batched(_)),
                TransferDiagnosticOutcome {
                    stats,
                    elapsed: transfer_elapsed,
                    status: "ok",
                    error: None,
                },
            );
            (write_report, stats)
        }
        Err(err) => {
            let error = err.to_string();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
                matches!(&durability, PoolWriteDurability::Batched(_)),
                TransferDiagnosticOutcome {
                    stats: sink.stats(),
                    elapsed: transfer_elapsed,
                    status: "error",
                    error: Some(error.as_str()),
                },
            );
            return Err(err);
        }
    };
    let (write_report, _transfer_stats) = write_report;

    match durability {
        PoolWriteDurability::Batched(_) => {
            let checkpoint_projection = checkpoint_projection_for_no_parity_write(
                &selected,
                &prepared,
                &write_report,
                stored.copy_representation(),
            )?;
            pool_write_result(
                request,
                selected,
                prepared,
                stored.copy_representation(),
                write_report,
                AppendCommitDiagnostics {
                    filemark_write_drain: Duration::ZERO,
                    catalog_journal_fsync: Duration::ZERO,
                },
                false,
                Some(checkpoint_projection),
            )
        }
        #[cfg(test)]
        PoolWriteDurability::PerObject => {
            let commit_started = Instant::now();
            let commit_result = commit_pool_write(
                state,
                &selected,
                &prepared,
                &write_report,
                CommitPoolWriteProjection {
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    scheme: None,
                    copy_representation: stored.copy_representation(),
                },
                _pool_cfg,
                _transfer_stats.early_warning,
            );
            let commit_elapsed = commit_started.elapsed();
            let sealed_after_write = match commit_result {
                Ok(sealed_after_write) => {
                    log_commit_diagnostics(
                        &request,
                        &selected,
                        &prepared,
                        commit_elapsed,
                        "ok",
                        None,
                    );
                    sealed_after_write
                }
                Err(err) => {
                    let error = err.to_string();
                    log_commit_diagnostics(
                        &request,
                        &selected,
                        &prepared,
                        commit_elapsed,
                        "error",
                        Some(error.as_str()),
                    );
                    return Err(err);
                }
            };
            pool_write_result(
                request,
                selected,
                prepared,
                stored.copy_representation(),
                write_report,
                AppendCommitDiagnostics {
                    filemark_write_drain: _transfer_stats.filemark_write_drain,
                    catalog_journal_fsync: commit_elapsed,
                },
                sealed_after_write,
                None,
            )
        }
    }
}

fn record_tape_io_fence_for_transfer_error(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    reason: &str,
    error: &str,
) -> Result<(), PoolWriteError> {
    let barcode = state
        .get_tape(&selected.tape_uuid)?
        .and_then(|tape| tape.voltag);
    let evidence = format!(
        "{{\"pool_id\":\"{}\",\"tape_uuid\":\"{}\",\"error\":\"{}\"}}",
        json_escape(selected.pool_id.as_str()),
        uuid_text(selected.tape_uuid),
        json_escape(error),
    );
    state.record_tape_io_fence(remanence_state::TapeIoFenceInput {
        tape_uuid: selected.tape_uuid,
        barcode,
        reason: reason.to_string(),
        evidence_json: Some(evidence),
    })?;
    Ok(())
}

fn fence_after_terminal_motion(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    reason: &str,
    error: PoolWriteError,
) -> PoolWriteError {
    let detail = error.to_string();
    match record_tape_io_fence_for_transfer_error(state, selected, reason, detail.as_str()) {
        Ok(()) => error,
        Err(fence_error) => PoolWriteError::InvalidInput(format!(
            "{detail}; failed to persist required terminal tape-I/O fence: {fence_error}"
        )),
    }
}

fn tape_io_fence_reason_for_transfer_error(error: &str) -> &'static str {
    if error.contains("reset UNIT ATTENTION") {
        "reset_unit_attention"
    } else if error.contains("partial fixed batch uncommittable") {
        "partial_batch"
    } else if error.contains("position drift") {
        "position_drift"
    } else {
        "transfer_error"
    }
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
struct CommitPoolWriteProjection {
    first_parity_data_ordinal: Option<u64>,
    protected_until_ordinal: Option<u64>,
    scheme: Option<ParityScheme>,
    copy_representation: CopyRepresentation,
}

#[cfg(test)]
fn commit_pool_write(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    write_report: &StreamingObjectWriteReport,
    projection: CommitPoolWriteProjection,
    pool_cfg: &TapePoolConfig,
    hardware_early_warning: bool,
) -> Result<bool, PoolWriteError> {
    let first_body_lba = first_payload_body_lba(write_report);
    let metadata_hash =
        if projection.copy_representation.representation == OBJECT_COPY_REPRESENTATION_PLAINTEXT {
            Some(write_report.catalog.object.manifest_sha256.to_vec())
        } else {
            None
        };
    let file_projections = write_report
        .catalog
        .files
        .iter()
        .map(native_object_file_projection)
        .collect::<Vec<_>>();
    let object_projection = NativeObjectProjectionInput {
        object_id: write_report.catalog.object.object_id.clone(),
        caller_object_id: Some(write_report.catalog.object.caller_object_id.clone()),
        body_format: write_report.catalog.object.body_format.clone(),
        logical_size_bytes: Some(write_report.catalog.object.logical_size_bytes),
        content_hash: Some(prepared.content_sha256.to_vec()),
        metadata_hash,
        created_at_utc: Some(prepared.write_timestamp.clone()),
    };
    let copy_projection = NativeObjectCopyProjectionInput {
        object_id: write_report.catalog.object_copy.object_id.clone(),
        tape_uuid: selected.tape_uuid,
        tape_file_number: write_report.catalog.object_copy.tape_file_number,
        first_body_lba,
        first_parity_data_ordinal: projection.first_parity_data_ordinal,
        protected_until_ordinal: projection.protected_until_ordinal,
        status: "committed".to_string(),
        representation: projection.copy_representation.representation.to_string(),
        recipient_epoch_ids: projection.copy_representation.recipient_epoch_ids.clone(),
        metadata_frame_len: projection.copy_representation.metadata_frame_len,
        plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest.to_vec()),
        stored_digest: Some(write_report.catalog.object_copy.stored_digest.to_vec()),
    };
    let tape_input = TapeJournalIndexInput {
        tape_uuid: selected.tape_uuid,
        block_size: selected.block_size,
        scheme: projection.scheme,
        journal_offset_bytes: 0,
    };
    if tape_input.scheme.is_none() {
        state.project_native_object_append_commit(
            object_projection,
            &file_projections,
            &[copy_projection],
            tape_input,
            &write_report.catalog.tape_file_bundle,
        )?;
    } else {
        state.project_native_object_and_committed_tape_file_bundle(
            object_projection,
            &file_projections,
            &[copy_projection],
            tape_input,
            &write_report.catalog.tape_file_bundle,
        )?;
    }
    seal_selected_tape_if_needed(state, selected, pool_cfg, hardware_early_warning)
}

fn checkpoint_projection_for_no_parity_write(
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    write_report: &StreamingObjectWriteReport,
    copy_representation: CopyRepresentation,
) -> Result<remanence_state::CheckpointObjectProjection, PoolWriteError> {
    let file_projections = write_report
        .catalog
        .files
        .iter()
        .map(native_object_file_projection)
        .collect();
    let representation = match copy_representation.representation {
        OBJECT_COPY_REPRESENTATION_PLAINTEXT => {
            let manifest_first_chunk_lba = prepared
                .plan
                .layout
                .manifest
                .first_chunk_lba
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "prepared plaintext manifest has no body LBA".to_string(),
                    )
                })?;
            remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: manifest_first_chunk_lba.0,
                manifest_size_bytes: prepared.plan.layout.manifest.size_bytes,
                manifest_chunk_count: prepared.plan.layout.manifest.chunk_count,
                manifest_sha256: prepared.plan.layout.manifest_sha256,
            }
        }
        OBJECT_COPY_REPRESENTATION_ENCRYPTED => {
            remanence_state::CheckpointObjectRecoveryRepresentation::Encrypted {
                recipient_epoch_ids: copy_representation
                    .recovery_recipient_epoch_ids
                    .clone()
                    .ok_or_else(|| {
                        PoolWriteError::InvalidInput(
                            "prepared encrypted copy has no recipient epochs".to_string(),
                        )
                    })?,
                metadata_frame_len: copy_representation.metadata_frame_len.ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "prepared encrypted copy has no metadata frame length".to_string(),
                    )
                })?,
                key_frame_len: copy_representation.key_frame_len.ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "prepared encrypted copy has no key frame length".to_string(),
                    )
                })?,
            }
        }
        other => {
            return Err(PoolWriteError::InvalidInput(format!(
                "unsupported prepared copy representation {other:?}"
            )));
        }
    };
    Ok(remanence_state::CheckpointObjectProjection {
        object: NativeObjectProjectionInput {
            object_id: write_report.catalog.object.object_id.clone(),
            caller_object_id: Some(write_report.catalog.object.caller_object_id.clone()),
            body_format: write_report.catalog.object.body_format.clone(),
            logical_size_bytes: Some(write_report.catalog.object.logical_size_bytes),
            content_hash: Some(prepared.content_sha256.to_vec()),
            metadata_hash: (copy_representation.representation
                == OBJECT_COPY_REPRESENTATION_PLAINTEXT)
                .then(|| write_report.catalog.object.manifest_sha256.to_vec()),
            created_at_utc: Some(prepared.write_timestamp.clone()),
        },
        files: file_projections,
        copy: NativeObjectCopyProjectionInput {
            object_id: write_report.catalog.object_copy.object_id.clone(),
            tape_uuid: selected.tape_uuid,
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            first_body_lba: first_payload_body_lba(write_report),
            first_parity_data_ordinal: None,
            protected_until_ordinal: None,
            status: "committed".to_string(),
            representation: copy_representation.representation.to_string(),
            recipient_epoch_ids: copy_representation.recipient_epoch_ids,
            metadata_frame_len: copy_representation.metadata_frame_len,
            plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest.to_vec()),
            stored_digest: Some(write_report.catalog.object_copy.stored_digest.to_vec()),
        },
        block_size: selected.block_size,
        block_count: write_report.catalog.object_copy.data_block_count,
        fresh_tape: write_report
            .catalog
            .tape_file_bundle
            .entries
            .first()
            .is_some_and(|entry| entry.kind == TapeFileKind::Bootstrap),
        total_committed_ordinals: write_report
            .catalog
            .tape_file_bundle
            .total_committed_ordinals,
        object_recovery_row: remanence_state::CheckpointObjectRecoveryRow {
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            stored_block_count: write_report.catalog.object_copy.data_block_count,
            object_id: prepared.options.object_id.as_bytes().to_vec(),
            representation,
        },
    })
}

fn native_object_file_projection(file: &FileCatalogProjection) -> NativeObjectFileProjectionInput {
    NativeObjectFileProjectionInput {
        object_id: file.object_id.clone(),
        file_id: file.file_id.clone(),
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        file_sha256: file.file_sha256.to_vec(),
        first_chunk_lba: file.first_chunk_lba.map(|lba| lba.0),
        chunk_count: file.chunk_count,
        mtime: file.mtime.clone(),
        executable: file.executable,
    }
}

#[allow(clippy::too_many_arguments)]
fn pool_write_result(
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared: PreparedPoolObject,
    copy_representation: CopyRepresentation,
    write_report: StreamingObjectWriteReport,
    append_commit_diagnostics: AppendCommitDiagnostics,
    sealed_after_write: bool,
    checkpoint_projection: Option<remanence_state::CheckpointObjectProjection>,
) -> Result<PoolWriteResult, PoolWriteError> {
    let position_lba = write_report
        .object_close
        .filemark_outcome
        .position_after
        .lba;
    let post_write_used_bytes = checked_physical_used_bytes(position_lba, selected.block_size)?;
    let hardware_early_warning = write_report.object_close.filemark_outcome.early_warning
        || write_report
            .object_close
            .sidecars_emitted
            .iter()
            .any(|sidecar| sidecar.filemark_outcome.early_warning);
    let first_body_lba = first_payload_body_lba(&write_report);
    let object = PoolWriteObjectRecord {
        object_id: *prepared.object_uuid.as_bytes(),
        caller_object_id: request.caller_object_id,
        content_sha256: prepared.content_sha256,
        logical_size_bytes: write_report.catalog.object.logical_size_bytes,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: prepared.write_timestamp,
        copies: vec![PoolWriteObjectCopyRecord {
            tape_uuid: selected.tape_uuid,
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            first_body_lba,
            pool_id: selected.pool_id,
            representation: copy_representation.representation.to_string(),
            recipient_epoch_ids: copy_representation.recipient_epoch_ids,
            metadata_frame_len: copy_representation.metadata_frame_len,
            plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest),
            stored_digest: Some(write_report.catalog.object_copy.stored_digest),
        }],
    };

    Ok(PoolWriteResult {
        object,
        write_report: Some(write_report),
        append_commit_diagnostics,
        sealed_after_write,
        checkpoint_projection,
        post_write_used_bytes,
        hardware_early_warning,
    })
}

fn checked_physical_used_bytes(position_lba: u64, block_size: u32) -> Result<u64, PoolWriteError> {
    position_lba.checked_mul(u64::from(block_size)).ok_or(
        PoolWriteError::PhysicalUsedBytesOverflow {
            position_lba,
            block_size,
        },
    )
}

pub(crate) fn maybe_replay_pool_write(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    request: &WriteObjectToPoolRequest,
) -> Result<Option<PoolWriteResult>, PoolWriteError> {
    if request.caller_object_id.trim().is_empty() {
        return Ok(None);
    }
    let Some(existing) = state.get_native_object_by_pool_and_caller_object_id(
        pool_cfg.id.as_str(),
        request.caller_object_id.as_str(),
    )?
    else {
        return Ok(None);
    };
    let _ = request.source.size_bytes()?;
    let requested_hash = request.source.content_sha256()?;
    if let Some(expected) = request.expected_content_sha256 {
        if requested_hash != expected {
            return Err(PoolWriteError::ContentHashMismatch {
                expected: bytes_to_hex(&expected),
                actual: bytes_to_hex(&requested_hash),
            });
        }
    }
    let existing_hash = native_object_content_sha256(&existing)?;
    if existing_hash != requested_hash {
        return Err(PoolWriteError::CallerObjectIdConflict {
            pool_id: pool_cfg.id.clone(),
            caller_object_id: request.caller_object_id.clone(),
            existing_content_sha256: bytes_to_hex(&existing_hash),
            requested_content_sha256: bytes_to_hex(&requested_hash),
        });
    }
    Ok(Some(PoolWriteResult {
        object: pool_write_object_record_from_native(existing, pool_cfg.id.as_str())?,
        write_report: None,
        append_commit_diagnostics: AppendCommitDiagnostics::default(),
        sealed_after_write: false,
        checkpoint_projection: None,
        post_write_used_bytes: 0,
        hardware_early_warning: false,
    }))
}

fn pool_write_object_record_from_native(
    object: NativeObjectRecord,
    pool_id: &str,
) -> Result<PoolWriteObjectRecord, PoolWriteError> {
    let object_uuid = Uuid::parse_str(object.object_id.as_str()).map_err(|err| {
        replay_object_invalid(&object.object_id, format!("object_id is not a UUID: {err}"))
    })?;
    let content_sha256 = native_object_content_sha256(&object)?;
    let logical_size_bytes = object
        .logical_size_bytes
        .ok_or_else(|| replay_object_invalid(&object.object_id, "logical_size_bytes is missing"))?;
    let copies = object
        .copies
        .iter()
        .filter(|copy| copy.pool_id.as_deref() == Some(pool_id) && copy.status == "committed")
        .map(|copy| pool_write_copy_record_from_native(copy, pool_id))
        .collect::<Result<Vec<_>, _>>()?;
    if copies.is_empty() {
        return Err(replay_object_invalid(
            &object.object_id,
            format!("no committed copy in pool {pool_id}"),
        ));
    }
    Ok(PoolWriteObjectRecord {
        object_id: *object_uuid.as_bytes(),
        caller_object_id: object.caller_object_id.unwrap_or_default(),
        content_sha256,
        logical_size_bytes,
        body_format: object.body_format,
        created_at_utc: object.created_at_utc,
        copies,
    })
}

fn pool_write_copy_record_from_native(
    copy: &NativeObjectCopyRecord,
    pool_id: &str,
) -> Result<PoolWriteObjectCopyRecord, PoolWriteError> {
    let tape_uuid =
        copy.tape_uuid.as_slice().try_into().map_err(|_| {
            replay_object_invalid(&copy.object_id, "copy tape_uuid is not 16 bytes")
        })?;
    Ok(PoolWriteObjectCopyRecord {
        tape_uuid,
        tape_file_number: copy.tape_file_number,
        first_body_lba: copy.first_body_lba,
        pool_id: pool_id.to_string(),
        representation: copy.representation.clone(),
        recipient_epoch_ids: copy.recipient_epoch_ids.clone(),
        metadata_frame_len: copy.metadata_frame_len,
        plaintext_digest: optional_native_copy_digest(
            copy.plaintext_digest.as_deref(),
            &copy.object_id,
            "plaintext_digest",
        )?,
        stored_digest: optional_native_copy_digest(
            copy.stored_digest.as_deref(),
            &copy.object_id,
            "stored_digest",
        )?,
    })
}

fn optional_native_copy_digest(
    digest: Option<&[u8]>,
    object_id: &str,
    field: &str,
) -> Result<Option<[u8; 32]>, PoolWriteError> {
    digest
        .map(|digest| {
            digest
                .try_into()
                .map_err(|_| replay_object_invalid(object_id, format!("{field} is not 32 bytes")))
        })
        .transpose()
}

fn native_object_content_sha256(object: &NativeObjectRecord) -> Result<[u8; 32], PoolWriteError> {
    let Some(content_hash) = object.content_hash.as_deref() else {
        return Err(replay_object_invalid(
            &object.object_id,
            "content_hash is missing",
        ));
    };
    content_hash
        .try_into()
        .map_err(|_| replay_object_invalid(&object.object_id, "content_hash is not 32 bytes"))
}

fn replay_object_invalid(object_id: &str, reason: impl Into<String>) -> PoolWriteError {
    PoolWriteError::ReplayObjectInvalid {
        object_id: object_id.to_string(),
        reason: reason.into(),
    }
}

struct PreparedPoolObject {
    content_sha256: [u8; 32],
    object_uuid: Uuid,
    write_timestamp: String,
    options: RemTarObjectOptions,
    files: Vec<PreparedFile>,
    plan: StreamingObjectPlan,
    source: PreparedPoolSource,
}

impl PreparedPoolObject {
    fn overlap_control(&self) -> Option<Arc<crate::append_ring::AppendRingControl>> {
        match &self.source {
            PreparedPoolSource::Paths => None,
            PreparedPoolSource::Streamed { control, .. } => Some(Arc::clone(control)),
        }
    }
}

enum PreparedPoolSource {
    Paths,
    Streamed {
        reader: Arc<Mutex<Box<dyn Read + Send>>>,
        control: Arc<crate::append_ring::AppendRingControl>,
    },
}

struct PreparedPoolWrite {
    prepared: PreparedPoolObject,
    stored: PreparedStoredObject,
}

struct PreparedEncryptedPoolObject {
    plaintext_layout: remanence_format::RemTarObjectLayout,
    envelope: SealReport,
    sealed: Vec<u8>,
}

enum PreparedStoredObject {
    Plaintext,
    Encrypted(Box<PreparedEncryptedPoolObject>),
}

impl PreparedStoredObject {
    fn projected_size_blocks(&self, prepared: &PreparedPoolObject) -> u64 {
        match self {
            Self::Plaintext => prepared.plan.layout.projected_size_blocks,
            Self::Encrypted(encrypted) => encrypted.envelope.stored_size_blocks,
        }
    }

    fn representation_label(&self) -> &'static str {
        match self {
            Self::Plaintext => OBJECT_COPY_REPRESENTATION_PLAINTEXT,
            Self::Encrypted(_) => OBJECT_COPY_REPRESENTATION_ENCRYPTED,
        }
    }

    fn copy_representation(&self) -> CopyRepresentation {
        match self {
            Self::Plaintext => CopyRepresentation::plaintext(),
            Self::Encrypted(encrypted) => CopyRepresentation::encrypted(&encrypted.envelope),
        }
    }
}

fn stored_footprint_bytes(
    stored: &PreparedStoredObject,
    prepared: &PreparedPoolObject,
    selected_block_size: u32,
) -> Result<u64, PoolWriteError> {
    if prepared.options.chunk_size != selected_block_size as usize {
        return Err(PoolWriteError::InvalidInput(format!(
            "prepared chunk size {} does not match selected tape block size {selected_block_size}",
            prepared.options.chunk_size
        )));
    }
    stored
        .projected_size_blocks(prepared)
        .checked_mul(u64::from(selected_block_size))
        .ok_or_else(|| PoolWriteError::InvalidInput("stored object byte size overflow".to_string()))
}

#[derive(Clone)]
struct CopyRepresentation {
    representation: &'static str,
    recipient_epoch_ids: Option<Vec<String>>,
    recovery_recipient_epoch_ids: Option<Vec<[u8; 16]>>,
    metadata_frame_len: Option<u64>,
    key_frame_len: Option<u32>,
}

impl CopyRepresentation {
    fn plaintext() -> Self {
        Self {
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT,
            recipient_epoch_ids: None,
            recovery_recipient_epoch_ids: None,
            metadata_frame_len: None,
            key_frame_len: None,
        }
    }

    fn encrypted(envelope: &SealReport) -> Self {
        let recipient_epoch_ids = envelope
            .key_frame
            .slots
            .iter()
            .map(|slot| bytes_to_hex(&slot.recipient_epoch_id))
            .collect();
        let recovery_recipient_epoch_ids = envelope_recipient_epoch_ids(envelope);
        Self {
            representation: OBJECT_COPY_REPRESENTATION_ENCRYPTED,
            recipient_epoch_ids: Some(recipient_epoch_ids),
            recovery_recipient_epoch_ids: Some(recovery_recipient_epoch_ids),
            metadata_frame_len: Some(envelope.metadata_frame_len),
            key_frame_len: Some(envelope.header.key_frame_len),
        }
    }
}

fn prepared_payload_bytes(prepared: &PreparedPoolObject) -> u64 {
    prepared
        .files
        .iter()
        .fold(0u64, |acc, file| acc.saturating_add(file.spec.size_bytes))
}

fn parity_label(parity: &ParityConfig) -> &'static str {
    match parity {
        ParityConfig::Scheme(_) => "scheme",
        ParityConfig::None => "none",
    }
}

fn log_transfer_diagnostics(
    _request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    stored_projected_blocks: u64,
    write_filemarks_immed: bool,
    outcome: TransferDiagnosticOutcome<'_>,
) {
    let payload_bytes = prepared_payload_bytes(prepared);
    tracing::info!(
        target: "remanence_write_diag",
        phase = "transfer",
        pool_id = %selected.pool_id,
        tape_uuid = %uuid_text(selected.tape_uuid),
        parity = parity_label(&selected.parity_config),
        status = outcome.status,
        error = outcome.error.unwrap_or(""),
        payload_bytes,
        selected_block_size_bytes = selected.block_size,
        format_chunk_size_bytes = prepared.options.chunk_size,
        projected_object_blocks = stored_projected_blocks,
        sink_write_bytes = outcome.stats.block_write_bytes,
        block_write_calls = outcome.stats.block_write_calls,
        min_block_bytes = outcome.stats.min_block_bytes.unwrap_or(0),
        max_block_bytes = outcome.stats.max_block_bytes.unwrap_or(0),
        filemark_calls = outcome.stats.filemark_calls,
        filemarks = outcome.stats.filemarks,
        filemark_write_drain_ms = crate::diagnostics::duration_ms(
            outcome.stats.filemark_write_drain
        ),
        position_calls = outcome.stats.position_calls,
        early_warning = outcome.stats.early_warning,
        scsi_write_cdb = "WRITE6_FIXED_BATCH",
        write_batch_blocks = outcome.stats.write_batch_blocks,
        effective_batch_blocks = outcome.stats.effective_batch_blocks,
        position_check_bytes = outcome.stats.position_check_bytes,
        staging_ring_buffers = outcome.stats.staging_ring_buffers,
        staging_wait_samples = outcome.stats.staging_wait_samples,
        staging_wait_p50_us = outcome.stats.staging_wait_p50_us,
        staging_wait_p95_us = outcome.stats.staging_wait_p95_us,
        staging_wait_max_us = outcome.stats.staging_wait_max_us,
        staging_wait_mean_us = outcome.stats.staging_wait_mean_us,
        refill_samples = outcome.stats.refill_samples,
        refill_p50_us = outcome.stats.refill_p50_us,
        refill_p95_us = outcome.stats.refill_p95_us,
        refill_max_us = outcome.stats.refill_max_us,
        refill_mean_us = outcome.stats.refill_mean_us,
        gap_samples = outcome.stats.gap_samples,
        gap_p50_us = outcome.stats.gap_p50_us,
        gap_p95_us = outcome.stats.gap_p95_us,
        gap_max_us = outcome.stats.gap_max_us,
        gap_mean_us = outcome.stats.gap_mean_us,
        ioctl_samples = outcome.stats.ioctl_samples,
        ioctl_p50_us = outcome.stats.ioctl_p50_us,
        ioctl_p95_us = outcome.stats.ioctl_p95_us,
        ioctl_max_us = outcome.stats.ioctl_max_us,
        ioctl_mean_us = outcome.stats.ioctl_mean_us,
        first_60s_ioctl_samples = outcome.stats.first_60s_ioctl_samples,
        first_60s_ioctl_p50_us = outcome.stats.first_60s_ioctl_p50_us,
        first_60s_ioctl_p95_us = outcome.stats.first_60s_ioctl_p95_us,
        first_60s_ioctl_max_us = outcome.stats.first_60s_ioctl_max_us,
        first_60s_ioctl_mean_us = outcome.stats.first_60s_ioctl_mean_us,
        accounting_samples = outcome.stats.accounting_samples,
        accounting_p50_us = outcome.stats.accounting_p50_us,
        accounting_p95_us = outcome.stats.accounting_p95_us,
        accounting_max_us = outcome.stats.accounting_max_us,
        accounting_mean_us = outcome.stats.accounting_mean_us,
        cadence_us = outcome.stats.cadence_us,
        effective_feed_bytes_per_second = outcome.stats.effective_feed_bytes_per_second,
        time_to_first_ioctl_ms = outcome.stats.time_to_first_ioctl_ms,
        steady_reached = outcome.stats.steady_reached,
        time_to_steady_ms = outcome.stats.time_to_steady_ms,
        steady_window_seconds = outcome.stats.steady_window_seconds,
        steady_threshold_percent = outcome.stats.steady_threshold_percent,
        ramp_observation_seconds = outcome.stats.ramp_observation_seconds,
        write_filemarks_immed,
        elapsed_ms = crate::diagnostics::duration_ms(outcome.elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, outcome.elapsed),
        "remanence_write_diag",
    );
}

struct TransferDiagnosticOutcome<'a> {
    stats: BlockSinkStats,
    elapsed: Duration,
    status: &'static str,
    error: Option<&'a str>,
}

#[cfg(test)]
fn log_commit_diagnostics(
    _request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    elapsed: Duration,
    status: &str,
    error: Option<&str>,
) {
    let payload_bytes = prepared_payload_bytes(prepared);
    tracing::info!(
        target: "remanence_write_diag",
        phase = "commit",
        pool_id = %selected.pool_id,
        tape_uuid = %uuid_text(selected.tape_uuid),
        parity = parity_label(&selected.parity_config),
        status,
        error = error.unwrap_or(""),
        payload_bytes,
        elapsed_ms = crate::diagnostics::duration_ms(elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, elapsed),
        "remanence_write_diag",
    );
}

fn prepare_pool_object(
    request: &WriteObjectToPoolRequest,
    block_size: u32,
) -> Result<PreparedPoolObject, PoolWriteError> {
    let content_sha256 = request.source.content_sha256()?;
    let object_uuid = Uuid::new_v4();
    let object_id = object_uuid.to_string();
    let write_timestamp = now_rfc3339()?;
    let mut options = RemTarObjectOptions::new(
        object_id,
        request.caller_object_id.clone(),
        write_timestamp.clone(),
        Uuid::new_v4().to_string(),
    );
    options.chunk_size = block_size as usize;
    let (files, source) = match &request.source {
        WriteObjectSource::Path(source_path) => (
            vec![prepare_regular_file(
                source_path,
                &request.archive_path,
                Uuid::new_v4().to_string(),
            )?],
            PreparedPoolSource::Paths,
        ),
        WriteObjectSource::Streamed(streamed) => {
            if !matches!(request.representation, PoolWriteRepresentation::Plaintext) {
                return Err(PoolWriteError::InvalidInput(
                    "streamed pool sources support plaintext representation only".to_string(),
                ));
            }
            let archive_path = request.archive_path.to_str().ok_or_else(|| {
                PoolWriteError::InvalidInput("streamed archive path must be UTF-8".to_string())
            })?;
            let mut spec = RemTarFileSpec::new(
                archive_path,
                Uuid::new_v4().to_string(),
                streamed.size_bytes,
                streamed.content_sha256,
            );
            spec.mtime = None;
            spec.executable = Some(false);
            (
                vec![PreparedFile {
                    source_path: PathBuf::new(),
                    spec,
                }],
                PreparedPoolSource::Streamed {
                    reader: Arc::clone(&streamed.reader),
                    control: Arc::clone(&streamed.control),
                },
            )
        }
    };
    let plan = plan_prepared_object(&options, &files)?;
    Ok(PreparedPoolObject {
        content_sha256,
        object_uuid,
        write_timestamp,
        options,
        files,
        plan,
        source,
    })
}

fn prepare_stored_object(
    prepared: &PreparedPoolObject,
    representation: &PoolWriteRepresentation,
) -> Result<PreparedStoredObject, PoolWriteError> {
    match representation {
        PoolWriteRepresentation::Plaintext => Ok(PreparedStoredObject::Plaintext),
        PoolWriteRepresentation::Encrypted { recipients } => Ok(PreparedStoredObject::Encrypted(
            Box::new(seal_prepared_object(prepared, recipients)?),
        )),
    }
}

fn seal_prepared_object(
    prepared: &PreparedPoolObject,
    recipients: &[RecipientPublicKey],
) -> Result<PreparedEncryptedPoolObject, PoolWriteError> {
    let mut encrypted_sink = VecBlockSink::new();
    let mut readers = open_prepared_readers(prepared)?;
    let mut streams = Vec::with_capacity(prepared.files.len());
    for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
        streams.push(RemTarFileStream::new(file.spec.clone(), reader));
    }
    let report = write_encrypted_rem_object_from_readers(
        &mut encrypted_sink,
        &prepared.options,
        &mut streams,
        recipients,
    )
    .map_err(StreamingError::from)?;
    let plaintext_layout = report.plaintext_layout;
    if plaintext_layout.projected_size_blocks != prepared.plan.layout.projected_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "sealed plaintext layout differs from pre-admission plan".to_string(),
        ));
    }
    let envelope = report.envelope;
    let sealed = flatten_blocks(encrypted_sink.blocks, prepared.options.chunk_size)?;
    let block_count = u64::try_from(sealed.len() / prepared.options.chunk_size).map_err(|_| {
        PoolWriteError::InvalidInput("sealed REM-OBJECT block count overflow".to_string())
    })?;
    if sealed.len() % prepared.options.chunk_size != 0 || block_count != envelope.stored_size_blocks
    {
        return Err(PoolWriteError::InvalidInput(
            "sealed REM-OBJECT bytes do not match envelope block count".to_string(),
        ));
    }
    Ok(PreparedEncryptedPoolObject {
        plaintext_layout,
        envelope,
        sealed,
    })
}

fn flatten_blocks(blocks: Vec<Vec<u8>>, block_size: usize) -> Result<Vec<u8>, PoolWriteError> {
    let capacity = blocks
        .len()
        .checked_mul(block_size)
        .ok_or_else(|| PoolWriteError::InvalidInput("object byte length overflow".to_string()))?;
    let mut out = Vec::with_capacity(capacity);
    for block in blocks {
        if block.len() != block_size {
            return Err(PoolWriteError::InvalidInput(format!(
                "REM-OBJECT block length {} does not match chunk size {block_size}",
                block.len()
            )));
        }
        out.extend_from_slice(&block);
    }
    Ok(out)
}

fn envelope_recipient_epoch_ids(envelope: &SealReport) -> Vec<[u8; 16]> {
    envelope
        .key_frame
        .slots
        .iter()
        .map(|slot| slot.recipient_epoch_id)
        .collect()
}

fn write_fixed_blocks(
    sink: &mut dyn BlockSink,
    block_size: usize,
    bytes: &[u8],
) -> Result<u64, PoolWriteError> {
    if bytes.len() % block_size != 0 {
        return Err(PoolWriteError::InvalidInput(
            "stored REM-OBJECT bytes are not block aligned".to_string(),
        ));
    }
    let mut blocks = 0u64;
    for block in bytes.chunks_exact(block_size) {
        let outcome = sink.write_block(block)?;
        let expected_bytes = u64::try_from(block.len()).map_err(|_| {
            PoolWriteError::InvalidInput("fixed block length does not fit u64".to_string())
        })?;
        let written_bytes = u64::from(outcome.bytes_written);
        if written_bytes != expected_bytes || outcome.end_of_medium {
            return Err(PoolWriteError::TapeIo(
                TapeIoError::PartialBatchUncommittable {
                    requested_records: 1,
                    written_records: u32::from(written_bytes >= expected_bytes),
                    requested_bytes: expected_bytes,
                    written_bytes,
                    end_of_medium: outcome.end_of_medium,
                    sense: None,
                },
            ));
        }
        blocks = blocks
            .checked_add(1)
            .ok_or_else(|| PoolWriteError::InvalidInput("block count overflow".to_string()))?;
    }
    Ok(blocks)
}

#[cfg(test)]
fn position_no_parity_append(sink: &mut dyn BlockSink) -> Result<TapePosition, PoolWriteError> {
    sink.space_to_end_of_data().map_err(PoolWriteError::from)
}

fn position_no_parity_append_at_checkpoint(
    sink: &mut dyn BlockSink,
    journal_eod_lba: u64,
) -> Result<(), PoolWriteError> {
    let located = sink.locate(journal_eod_lba)?;
    if located.partition != 0 || located.lba != journal_eod_lba {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(
            format!(
                "checkpoint recovery LOCATE mismatch: expected partition 0 lba {journal_eod_lba}, observed partition {} lba {}",
                located.partition, located.lba
            ),
        )));
    }
    prove_no_parity_append_boundary(sink, journal_eod_lba)
}

fn prove_no_parity_append_boundary(
    sink: &mut dyn BlockSink,
    expected_lba: u64,
) -> Result<(), PoolWriteError> {
    let observed = sink.position()?;
    if observed.partition != 0 || observed.lba != expected_lba {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(
            format!(
                "checkpoint append position drift: expected partition 0 lba {expected_lba}, observed partition {} lba {}",
                observed.partition, observed.lba
            ),
        )));
    }
    Ok(())
}

fn write_object_delimiter(
    sink: &mut dyn BlockSink,
    durability: &PoolWriteDurability,
    append: NoParityAppendContext,
    object_blocks: u64,
) -> Result<WriteFilemarksOutcome, PoolWriteError> {
    match durability {
        #[cfg(test)]
        PoolWriteDurability::PerObject => Ok(sink.write_filemarks(1)?),
        PoolWriteDurability::Batched(_) => {
            sink.write_filemarks_immediate(1)?;
            // The object occupies [start, start + object_blocks); its
            // trailing filemark sits at start + object_blocks, and the
            // position after that filemark is one more.
            let position_after_lba = append
                .object_start_lba()?
                .checked_add(object_blocks)
                .and_then(|lba| lba.checked_add(1))
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "provisional delimiter position overflows u64".to_string(),
                    )
                })?;
            Ok(WriteFilemarksOutcome::from_computed_position(
                TapePosition {
                    lba: position_after_lba,
                    partition: 0,
                    beginning_of_partition: false,
                    end_of_partition: false,
                    block_position_end_of_warning: false,
                },
            ))
        }
    }
}

fn write_no_parity_bootstrap(
    sink: &mut dyn BlockSink,
    tape_uuid: TapeUuid,
    block_size: u32,
    written_at: &str,
) -> Result<(), PoolWriteError> {
    let payload = build_tape_bootstrap(
        tape_uuid,
        block_size,
        ParityConfig::None,
        written_at.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    write_tape_bootstrap(sink, &payload)
}

struct SharedStreamReader(Arc<Mutex<Box<dyn Read + Send>>>);

impl Read for SharedStreamReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .read(output)
    }
}

fn open_prepared_readers(
    prepared: &PreparedPoolObject,
) -> Result<Vec<Box<dyn Read + Send>>, PoolWriteError> {
    match &prepared.source {
        PreparedPoolSource::Paths => prepared
            .files
            .iter()
            .map(|file| {
                File::open(&file.source_path)
                    .map(|reader| Box::new(reader) as Box<dyn Read + Send>)
                    .map_err(|source| PoolWriteError::Io {
                        context: "open source file for streaming",
                        path: file.source_path.clone(),
                        source,
                    })
            })
            .collect(),
        PreparedPoolSource::Streamed { reader, .. } => {
            Ok(vec![Box::new(SharedStreamReader(Arc::clone(reader)))])
        }
    }
}

fn no_parity_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    layout: remanence_format::RemTarObjectLayout,
    object_digest: [u8; 32],
    filemark_outcome: remanence_library::WriteFilemarksOutcome,
    append: NoParityAppendContext,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    if layout.projected_size_blocks != prepared.plan.layout.projected_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "emitted no-parity layout differs from pre-admission plan".to_string(),
        ));
    }
    if prepared.files.len() != layout.files.len() {
        return Err(PoolWriteError::InvalidInput(
            "prepared file count does not match emitted no-parity layout".to_string(),
        ));
    }
    let logical_size_bytes = layout.files.iter().try_fold(0u64, |acc, file| {
        acc.checked_add(file.size_bytes)
            .ok_or_else(|| PoolWriteError::InvalidInput("logical size overflow".to_string()))
    })?;
    let files = layout
        .files
        .iter()
        .zip(prepared.files.iter())
        .map(|(file, prepared_file)| {
            no_parity_file_catalog_projection(&prepared.options.object_id, file, prepared_file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object_start_lba = append.object_start_lba()?;
    let object_close = ObjectWriteSummary {
        tape_file_number: append.tape_file_number,
        first_parity_data_ordinal: 0,
        projected_size_blocks: prepared.plan.layout.projected_size_blocks,
        data_block_count: layout.projected_size_blocks,
        filemark_outcome,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 0,
        physical_start_lba: Some(object_start_lba),
    };
    let object = ObjectCatalogProjection {
        object_id: prepared.options.object_id.clone(),
        caller_object_id: prepared.options.caller_object_id.clone(),
        body_format: FORMAT_ID.to_string(),
        logical_size_bytes,
        manifest_sha256: layout.manifest_sha256,
    };
    let object_copy = ObjectCopyProjection {
        object_id: prepared.options.object_id.clone(),
        tape_uuid,
        tape_file_number: object_close.tape_file_number,
        first_parity_data_ordinal: None,
        data_block_count: object_close.data_block_count,
        protected_until_ordinal: None,
        parity_state: None,
        plaintext_digest: object_digest,
        stored_digest: object_digest,
    };
    let mut tape_file_entries = Vec::with_capacity(if append.fresh_tape { 2 } else { 1 });
    if append.fresh_tape {
        tape_file_entries.push(TapeFileEntry {
            tape_file_number: 0,
            kind: TapeFileKind::Bootstrap,
            block_count: 1,
            // A fresh tape's bootstrap prefix starts at BOT by definition.
            physical_start_hint: Some(0),
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        });
    }
    tape_file_entries.push(TapeFileEntry {
        tape_file_number: object_close.tape_file_number,
        kind: TapeFileKind::Object,
        block_count: layout.projected_size_blocks,
        physical_start_hint: Some(object_start_lba),
        object_id: Some(prepared.options.object_id.clone()),
        first_parity_data_ordinal: None,
        epoch_id: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        canonical_metadata_hash: None,
        object_recovery_row: None,
    });
    let tape_file_bundle = CommittedBundle {
        kind: CommittedBundleKind::Object,
        entries: tape_file_entries,
        highest_protected_ordinal: 0,
        total_committed_ordinals: append
            .object_total_committed_ordinals(layout.projected_size_blocks)?,
    };
    let catalog = StreamingCatalogProjection {
        object,
        files,
        object_copy,
        tape_file_bundle,
    };
    let audit_events = vec![StreamingAuditEvent {
        kind: "streaming_object_committed_no_parity",
        object_id: prepared.options.object_id.clone(),
        summary: format!(
            "committed no-parity object {} to tape file {} ({} payload files, {} object blocks)",
            prepared.options.object_id,
            object_close.tape_file_number,
            prepared.files.len(),
            object_close.data_block_count
        ),
    }];
    Ok(StreamingObjectWriteReport {
        layout,
        object_close,
        catalog,
        audit_events,
    })
}

fn write_encrypted_object_to_parity(
    parity: &mut ParitySink<'_>,
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    encrypted: &PreparedEncryptedPoolObject,
    capacity: TerminalTripleObjectReservation,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let opened = parity.begin_object_with_terminal_triple_reservation(capacity)?;
    let blocks_written = write_fixed_blocks(
        parity,
        prepared.options.chunk_size,
        encrypted.sealed.as_slice(),
    )?;
    if blocks_written != encrypted.envelope.stored_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "encrypted REM-OBJECT write count differs from envelope".to_string(),
        ));
    }
    let object_close = parity.finish_object()?;
    if opened.0 != object_close.tape_file_number {
        return Err(PoolWriteError::InvalidInput(
            "parity encrypted object tape-file number changed during write".to_string(),
        ));
    }
    encrypted_write_report(
        tape_uuid,
        prepared,
        encrypted,
        object_close,
        "streaming_encrypted_object_committed",
        "committed encrypted object",
        None,
    )
}

fn no_parity_encrypted_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    encrypted: &PreparedEncryptedPoolObject,
    filemark_outcome: remanence_library::WriteFilemarksOutcome,
    append: NoParityAppendContext,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let total_committed_ordinals =
        append.object_total_committed_ordinals(encrypted.envelope.stored_size_blocks)?;
    let object_close = ObjectWriteSummary {
        tape_file_number: append.tape_file_number,
        first_parity_data_ordinal: 0,
        projected_size_blocks: encrypted.envelope.stored_size_blocks,
        data_block_count: encrypted.envelope.stored_size_blocks,
        filemark_outcome,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 0,
        physical_start_lba: Some(append.object_start_lba()?),
    };
    encrypted_write_report(
        tape_uuid,
        prepared,
        encrypted,
        object_close,
        "streaming_encrypted_object_committed_no_parity",
        "committed no-parity encrypted object",
        Some(UnprotectedObjectBundleContext {
            fresh_tape: append.fresh_tape,
            total_committed_ordinals,
        }),
    )
}

#[derive(Clone, Copy, Debug)]
struct UnprotectedObjectBundleContext {
    fresh_tape: bool,
    total_committed_ordinals: u64,
}

fn encrypted_write_report(
    tape_uuid: TapeUuid,
    prepared: &PreparedPoolObject,
    encrypted: &PreparedEncryptedPoolObject,
    object_close: ObjectWriteSummary,
    audit_kind: &'static str,
    audit_prefix: &'static str,
    unprotected_context: Option<UnprotectedObjectBundleContext>,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    if prepared.files.len() != encrypted.plaintext_layout.files.len() {
        return Err(PoolWriteError::InvalidInput(
            "prepared file count does not match encrypted plaintext layout".to_string(),
        ));
    }
    if object_close.data_block_count != encrypted.envelope.stored_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "encrypted object close block count differs from envelope".to_string(),
        ));
    }
    let logical_size_bytes =
        encrypted
            .plaintext_layout
            .files
            .iter()
            .try_fold(0u64, |acc, file| {
                acc.checked_add(file.size_bytes).ok_or_else(|| {
                    PoolWriteError::InvalidInput("logical size overflow".to_string())
                })
            })?;
    let files = encrypted
        .plaintext_layout
        .files
        .iter()
        .zip(prepared.files.iter())
        .map(|(file, prepared_file)| {
            no_parity_file_catalog_projection(&prepared.options.object_id, file, prepared_file)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object = ObjectCatalogProjection {
        object_id: prepared.options.object_id.clone(),
        caller_object_id: prepared.options.caller_object_id.clone(),
        body_format: FORMAT_ID.to_string(),
        logical_size_bytes,
        manifest_sha256: encrypted.plaintext_layout.manifest_sha256,
    };
    let parity_state = if object_close.highest_protected_ordinal > 0 {
        Some(remanence_parity::ObjectParityState::from_ordinals(
            object_close.first_parity_data_ordinal,
            object_close.data_block_count,
            object_close.highest_protected_ordinal,
        )?)
    } else {
        None
    };
    let object_copy = ObjectCopyProjection {
        object_id: prepared.options.object_id.clone(),
        tape_uuid,
        tape_file_number: object_close.tape_file_number,
        first_parity_data_ordinal: (object_close.highest_protected_ordinal > 0)
            .then_some(object_close.first_parity_data_ordinal),
        data_block_count: object_close.data_block_count,
        protected_until_ordinal: (object_close.highest_protected_ordinal > 0)
            .then_some(object_close.highest_protected_ordinal),
        parity_state,
        plaintext_digest: encrypted.envelope.plaintext.digest,
        stored_digest: encrypted.envelope.stored_digest,
    };
    let tape_file_bundle = if object_close.highest_protected_ordinal > 0 {
        let mut bundle = object_close.committed_bundle()?;
        for entry in &mut bundle.entries {
            if entry.kind == TapeFileKind::Object
                && entry.tape_file_number == object_close.tape_file_number
            {
                entry.object_id = Some(prepared.options.object_id.clone());
            }
        }
        bundle
    } else {
        let unprotected_context = unprotected_context.ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "unprotected encrypted object is missing commit context".to_string(),
            )
        })?;
        let mut entries = Vec::with_capacity(if unprotected_context.fresh_tape { 2 } else { 1 });
        if unprotected_context.fresh_tape {
            entries.push(TapeFileEntry {
                tape_file_number: 0,
                kind: TapeFileKind::Bootstrap,
                block_count: 1,
                // A fresh tape's bootstrap prefix starts at BOT by definition.
                physical_start_hint: Some(0),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                object_recovery_row: None,
            });
        }
        entries.push(TapeFileEntry {
            tape_file_number: object_close.tape_file_number,
            kind: TapeFileKind::Object,
            block_count: encrypted.envelope.stored_size_blocks,
            physical_start_hint: object_close.physical_start_lba,
            object_id: Some(prepared.options.object_id.clone()),
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        });
        CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries,
            highest_protected_ordinal: 0,
            total_committed_ordinals: unprotected_context.total_committed_ordinals,
        }
    };
    let catalog = StreamingCatalogProjection {
        object,
        files,
        object_copy,
        tape_file_bundle,
    };
    let audit_events = vec![StreamingAuditEvent {
        kind: audit_kind,
        object_id: prepared.options.object_id.clone(),
        summary: format!(
            "{audit_prefix} {} to tape file {} ({} payload files, {} stored blocks)",
            prepared.options.object_id,
            object_close.tape_file_number,
            prepared.files.len(),
            object_close.data_block_count
        ),
    }];
    Ok(StreamingObjectWriteReport {
        layout: encrypted.plaintext_layout.clone(),
        object_close,
        catalog,
        audit_events,
    })
}

fn no_parity_file_catalog_projection(
    object_id: &str,
    file: &RemTarFileLayout,
    prepared: &PreparedFile,
) -> Result<FileCatalogProjection, PoolWriteError> {
    let file_sha256 = file.file_sha256.ok_or_else(|| {
        PoolWriteError::InvalidInput(format!(
            "catalog projection supports regular files only, got {:?} for {}",
            file.entry_type, file.path
        ))
    })?;
    Ok(FileCatalogProjection {
        object_id: object_id.to_string(),
        file_id: file.file_id.clone(),
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        file_sha256,
        first_chunk_lba: file.first_chunk_lba,
        chunk_count: file.chunk_count,
        mtime: prepared.spec.mtime.clone(),
        executable: file.executable,
    })
}

const LTO_RAW_CAPACITY_BYTES: &[(LtoGen, u64)] = &[
    (LtoGen::Lto1, 100_000_000_000),
    (LtoGen::Lto2, 200_000_000_000),
    (LtoGen::Lto3, 400_000_000_000),
    (LtoGen::Lto4, 800_000_000_000),
    (LtoGen::Lto5, 1_500_000_000_000),
    (LtoGen::Lto6, 2_500_000_000_000),
    (LtoGen::Lto7, 6_000_000_000_000),
    (LtoGen::M8, 9_000_000_000_000),
    (LtoGen::Lto8, 12_000_000_000_000),
    (LtoGen::Lto9, 18_000_000_000_000),
];

fn validate_scheme_columns(tape: &TapeRecord) -> Result<(), WritabilityError> {
    match (
        tape.scheme_id.as_deref(),
        tape.data_blocks_per_stripe,
        tape.parity_blocks_per_stripe,
        tape.stripes_per_neighborhood,
    ) {
        (None, None, None, None) => Ok(()),
        (Some(scheme_id), Some(data), Some(parity), Some(stripes)) => {
            let scheme = ParityScheme {
                id: SchemeId::new_owned(scheme_id.to_string()),
                data_blocks_per_stripe: u16::try_from(data)
                    .map_err(|_| missing_geometry("data_blocks_per_stripe overflows u16"))?,
                parity_blocks_per_stripe: u16::try_from(parity)
                    .map_err(|_| missing_geometry("parity_blocks_per_stripe overflows u16"))?,
                stripes_per_neighborhood: stripes,
            };
            scheme
                .validate()
                .map_err(|err| missing_geometry(format!("invalid parity scheme: {err}")))?;
            Ok(())
        }
        _ => Err(missing_geometry(
            "parity scheme columns must be either all present or all null",
        )),
    }
}

fn validate_pool_capacity_invariant_for_tapes(
    pool_cfg: &TapePoolConfig,
    tapes: &[TapeRecord],
) -> Result<(), SelectTapeError> {
    // Pools may contain mixed LTO generations. The invariant is guaranteed
    // against the smallest known cartridge capacity so every known member has
    // at least the configured low/high band width. If no member capacity is
    // known yet, candidate projection will reject unknown media at first write.
    if let Some(capacity_bytes) = tapes
        .iter()
        .filter_map(|tape| {
            tape.voltag
                .as_deref()
                .and_then(lto_generation_from_voltag)
                .map(raw_capacity_bytes)
        })
        .min()
    {
        validate_tape_pool_capacity_invariant(pool_cfg, capacity_bytes)?;
    }
    Ok(())
}

fn tape_fit_state_from_record(
    tape: &TapeRecord,
    pool_cfg: &TapePoolConfig,
    pool_id: &str,
    barcode_order: u64,
) -> Result<TapeFitState, WritabilityError> {
    let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id)
        .map_err(|err| missing_geometry(err.to_string()))?;
    let raw_capacity = tape_capacity_bytes(tape)?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| missing_geometry(error.to_string()))?;
    let block_size = tape_block_size(tape)?;
    let used_bytes = tape_physical_used_bytes(tape, block_size)?;
    let usable_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_high)
        .map_err(|err| missing_geometry(err.to_string()))?;
    let low_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_low)
        .map_err(|err| missing_geometry(err.to_string()))?;

    Ok(TapeFitState {
        tape_uuid,
        barcode_order,
        // TODO(2b): project drive occupancy from resolve_load_target/session state.
        already_loaded: false,
        used_bytes,
        usable_bytes,
        low_bytes,
    })
}

pub(crate) fn tape_capacity_bytes(tape: &TapeRecord) -> Result<u64, WritabilityError> {
    let voltag = tape
        .voltag
        .as_deref()
        .ok_or_else(|| missing_geometry("voltag is null"))?;
    let generation = lto_generation_from_voltag(voltag)
        .ok_or_else(|| missing_geometry("voltag does not end in a known LTO suffix"))?;
    Ok(raw_capacity_bytes(generation))
}

fn tape_block_size(tape: &TapeRecord) -> Result<u64, WritabilityError> {
    let block_size = tape
        .block_size
        .ok_or_else(|| missing_geometry("block_size is null"))?;
    if block_size == 0 {
        return Err(missing_geometry("block_size is zero"));
    }
    Ok(block_size)
}

fn tape_physical_used_bytes(tape: &TapeRecord, block_size: u64) -> Result<u64, WritabilityError> {
    let used_lba = tape_physical_used_blocks(tape)?;
    used_lba
        .checked_mul(block_size)
        .ok_or_else(|| missing_geometry("physical used capacity overflows u64"))
}

fn tape_physical_used_blocks(tape: &TapeRecord) -> Result<u64, WritabilityError> {
    if let Some(lba) = tape.written_extent_lba {
        return Ok(lba);
    }
    let Some(last_tape_file) = tape.last_committed_tape_file else {
        if tape.total_committed_ordinals == 0 {
            return Ok(0);
        }
        // Compatibility projection for pre-checkpoint no-parity catalog rows:
        // each Object contains at least one ordinal, so at most one Object
        // filemark exists per ordinal, plus the initial one-block Bootstrap
        // and its filemark. This is deliberately an upper bound; live parity
        // admission never relies on it.
        return tape
            .total_committed_ordinals
            .checked_mul(2)
            .and_then(|blocks| blocks.checked_add(2))
            .ok_or_else(|| missing_geometry("legacy physical extent estimate overflows u64"));
    };
    let tape_file_count = last_tape_file
        .checked_add(1)
        .ok_or_else(|| missing_geometry("legacy tape-file count overflows u64"))?;
    // A dense legacy no-parity prefix contains the Object blocks plus at most
    // one single-block control body and one filemark per tape file. Parity
    // prefixes with Object data are rejected before this coarse selector and
    // are admitted only through checkpoint/session position proof.
    tape.total_committed_ordinals
        .checked_add(
            tape_file_count
                .checked_mul(2)
                .ok_or_else(|| missing_geometry("legacy control estimate overflows u64"))?,
        )
        .ok_or_else(|| missing_geometry("legacy physical extent estimate overflows u64"))
}

fn ensure_request_pool_matches_config(
    request: &WriteObjectToPoolRequest,
    pool_cfg: &TapePoolConfig,
) -> Result<(), PoolWriteError> {
    if request.pool_id.trim() == pool_cfg.id.trim() {
        Ok(())
    } else {
        Err(PoolWriteError::InvalidInput(format!(
            "request pool_id {} does not match pool config id {}",
            request.pool_id.trim(),
            pool_cfg.id.trim()
        )))
    }
}

fn ensure_selected_tape_accepts_write(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<(), PoolWriteError> {
    ensure_selected_tape_accepts_write_inner(state, pool_cfg, selected, false)
}

pub(crate) fn ensure_selected_tape_accepts_session_write(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<(), PoolWriteError> {
    ensure_selected_tape_accepts_write_inner(state, pool_cfg, selected, true)
}

/// An empty checkpoint authority is admissible only for catalog-fresh media.
/// This check belongs before any load, position, or write preparation because
/// treating a known written prefix as fresh could rewrite BOT.
pub(crate) fn ensure_empty_checkpoint_matches_catalog_freshness(
    state: &CatalogIndex,
    selected: &SelectedTape,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
) -> Result<(), PoolWriteError> {
    if !checkpoints.is_empty() {
        return Ok(());
    }
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.total_committed_ordinals != 0
        || tape.last_committed_tape_file.is_some()
        || tape.written_extent_lba.is_some()
    {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "checkpoint journal is empty but catalog records a written tape prefix; physical-tail reconciliation is required before append (total_committed_ordinals={}, last_committed_tape_file={:?}, written_extent_lba={:?})",
            tape.total_committed_ordinals,
            tape.last_committed_tape_file,
            tape.written_extent_lba,
        )));
    }
    Ok(())
}

fn ensure_selected_tape_accepts_write_inner(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    session_has_resume_authority: bool,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.state != "ready" {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape is not writable in state {}",
            tape.state
        )));
    }
    let conflicts =
        state.tape_io_admission_conflicts(&selected.tape_uuid, tape.voltag.as_deref())?;
    if let Some(conflict) = conflicts.first() {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape is blocked by active tape-I/O fence {}: {}",
            conflict.quarantine_id, conflict.reason
        )));
    }
    let tape_block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    if tape_block_size != u64::from(selected.block_size) {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "selected block size {} does not match catalog tape block_size {tape_block_size}",
            selected.block_size
        )));
    }
    if tape_block_size != pool_cfg.block_size_bytes {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape block size {tape_block_size} does not match pool configured block size {}",
            pool_cfg.block_size_bytes
        )));
    }
    if tape.total_committed_ordinals > 0 {
        return match selected.parity_config {
            ParityConfig::None => Ok(()),
            ParityConfig::Scheme(_) if session_has_resume_authority => Ok(()),
            ParityConfig::Scheme(_) => Err(PoolWriteError::ParityAppendUnsupported {
                tape_uuid: uuid_text(selected.tape_uuid),
                total_committed_ordinals: tape.total_committed_ordinals,
            }),
        };
    }
    Ok(())
}

#[cfg(test)]
fn no_parity_append_context(
    state: &CatalogIndex,
    selected: &SelectedTape,
) -> Result<NoParityAppendContext, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.scheme_id.is_some() {
        return Err(PoolWriteError::InvalidInput(
            "no-parity append context requested for parity tape".to_string(),
        ));
    }
    let previous_total_committed_ordinals = tape.total_committed_ordinals;
    if previous_total_committed_ordinals > 0 && tape.last_committed_tape_file.is_none() {
        return Err(PoolWriteError::MissingTapeGeometry(
            "no-parity tape has committed ordinals but no last_committed_tape_file".to_string(),
        ));
    }
    let tape_file_number = match tape.last_committed_tape_file {
        Some(last) => last.checked_add(1).ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "next no-parity tape file overflows u64".to_string(),
            )
        })?,
        None => 1,
    };
    Ok(NoParityAppendContext {
        tape_file_number,
        previous_total_committed_ordinals,
        fresh_tape: previous_total_committed_ordinals == 0
            && tape.last_committed_tape_file.is_none(),
        expected_append_lba: None,
    })
}

/// Seed a batched session from the durable checkpoint journal, never SQLite
/// counters. A non-fresh tape without a checkpoint record is not safe to
/// admit because SPACE(EOD) would preserve an uncommitted crash tail.
pub(crate) fn first_batched_append_context(
    state: &CatalogIndex,
    selected: &SelectedTape,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
) -> Result<BatchedNoParityAppendContext, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.scheme_id.is_some() || !matches!(selected.parity_config, ParityConfig::None) {
        return Err(PoolWriteError::InvalidInput(
            "batched append context requires a parity-off tape".to_string(),
        ));
    }
    match checkpoints.last() {
        Some(checkpoint) => {
            if checkpoint.tape_uuid != selected.tape_uuid {
                return Err(PoolWriteError::MissingTapeGeometry(
                    "checkpoint journal tape UUID does not match selected tape".to_string(),
                ));
            }
            let previous_total_committed_ordinals = checkpoint
                .objects
                .last()
                .map(|object| object.total_committed_ordinals)
                .ok_or_else(|| {
                    PoolWriteError::MissingTapeGeometry(
                        "checkpoint record has no object projection".to_string(),
                    )
                })?;
            let tape_file_number = checkpoint.next_tape_file_number;
            let object_row_count = checkpoint.committed_object_count;
            Ok(BatchedNoParityAppendContext {
                append: NoParityAppendContext {
                    tape_file_number,
                    previous_total_committed_ordinals,
                    fresh_tape: false,
                    expected_append_lba: Some(checkpoint.eod_lba),
                },
                position: BatchedAppendPosition::JournalEod(checkpoint.eod_lba),
                object_row_count,
            })
        }
        None if tape.total_committed_ordinals == 0 && tape.last_committed_tape_file.is_none() => {
            Ok(BatchedNoParityAppendContext {
                append: NoParityAppendContext {
                    tape_file_number: 1,
                    previous_total_committed_ordinals: 0,
                    fresh_tape: true,
                    expected_append_lba: None,
                },
                position: BatchedAppendPosition::FreshTape,
                object_row_count: 0,
            })
        }
        None => Err(PoolWriteError::MissingTapeGeometry(
            "batched append requires a checkpoint journal for a non-fresh tape".to_string(),
        )),
    }
}

/// Advance the session-local append context from the prior object's report.
pub(crate) fn next_batched_append_context(
    previous: &BatchedNoParityAppendContext,
    result: &PoolWriteResult,
) -> Result<BatchedNoParityAppendContext, PoolWriteError> {
    let report = result.write_report().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "cannot derive provisional append context from a replay".to_string(),
        )
    })?;
    let tape_file_number = report
        .catalog
        .object_copy
        .tape_file_number
        .checked_add(1)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "next provisional tape-file number overflows u64".to_string(),
            )
        })?;
    let expected_lba = report.object_close.filemark_outcome.position_after.lba;
    result.checkpoint_projection().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "batched append result is missing checkpoint projection".to_string(),
        )
    })?;
    let object_row_count = previous.object_row_count.checked_add(1).ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("checkpoint Object-row count overflows u64".to_string())
    })?;
    Ok(BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number,
            previous_total_committed_ordinals: report
                .catalog
                .tape_file_bundle
                .total_committed_ordinals,
            fresh_tape: false,
            expected_append_lba: Some(expected_lba),
        },
        position: BatchedAppendPosition::CurrentBoundary(expected_lba),
        object_row_count,
    })
}

/// Re-anchor an accumulated session-local context at a durable checkpoint.
pub(crate) fn batched_append_context_after_checkpoint(
    previous: &BatchedNoParityAppendContext,
    record: &remanence_state::CheckpointJournalRecord,
) -> Result<BatchedNoParityAppendContext, PoolWriteError> {
    let previous_total_committed_ordinals = record
        .objects
        .last()
        .map(|object| object.total_committed_ordinals)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "checkpoint record has no final object projection".to_string(),
            )
        })?;
    let tape_file_number = record.next_tape_file_number;
    Ok(BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number,
            previous_total_committed_ordinals,
            fresh_tape: false,
            expected_append_lba: Some(record.eod_lba),
        },
        position: BatchedAppendPosition::CurrentBoundary(record.eod_lba),
        object_row_count: previous.object_row_count,
    })
}

fn ensure_selected_tape_has_capacity(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    object_size: u64,
    provisional_used_lba: Option<u64>,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let catalog_used_lba = tape_physical_used_blocks(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let used_lba = provisional_used_lba.unwrap_or(catalog_used_lba);
    if used_lba < catalog_used_lba {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "provisional physical extent {used_lba} precedes catalog physical extent or conservative estimate {catalog_used_lba}"
        )));
    }
    let used = used_lba.checked_mul(block_size).ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("physical used capacity overflows u64".to_string())
    })?;
    if used > capacity || object_size > capacity - used {
        return Err(PoolWriteError::SelectedTapeInsufficientCapacity {
            object_size,
            raw_capacity: capacity,
            used,
        });
    }
    Ok(())
}

fn ensure_no_parity_terminal_close_capacity(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    context: &BatchedNoParityAppendContext,
    projected_object_blocks: u64,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity_bytes = tape_capacity_bytes(&tape)
        .map_err(|error| PoolWriteError::MissingTapeGeometry(error.to_string()))?;
    let capacity_bytes = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity_bytes)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let capacity_blocks = capacity_bytes / u64::from(selected.block_size);
    let fresh_prefix_blocks = if context.append.fresh_tape {
        NO_PARITY_BOOTSTRAP_BLOCKS + 1
    } else {
        0
    };
    let current_used_blocks = context
        .append
        .expected_append_lba()?
        .checked_add(fresh_prefix_blocks)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "no-parity physical cursor plus fresh prefix overflows u64".to_string(),
            )
        })?;
    if current_used_blocks > capacity_blocks {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "no-parity physical cursor {current_used_blocks} exceeds capacity basis {capacity_blocks}"
        )));
    }
    let structural_entries_before_object = context.append.tape_file_number;
    let object_rows_before_object = context.object_row_count;
    let (low_watermark_blocks, high_watermark_blocks) =
        terminal_watermark_blocks(capacity_blocks, Some(pool_cfg))?;
    let exact_input = TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks,
        block_size_bytes: selected.block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 1,
        parity_shards_per_epoch: 0,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object: 0,
        structural_entries_before_object,
        object_rows_before_object,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS,
        remaining_tape_blocks: capacity_blocks - current_used_blocks,
        capacity_basis_blocks: capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 0,
    };
    let exact = match exact_input.evaluate() {
        Ok(report) => report,
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            reserve_blocks: Some(required_reserve_blocks),
            ..
        }) if context.append.fresh_tape => {
            return Err(ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks,
                empty_tape_usable_blocks: capacity_blocks
                    .saturating_sub(NO_PARITY_BOOTSTRAP_BLOCKS + 1),
                required_reserve_blocks,
            }
            .into());
        }
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            ..
        }) => {
            return Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "no-parity exact close reserve requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            });
        }
        Err(error) => return Err(error.into()),
    };
    match capacity_admission_disposition(CapacityAdmissionInput {
        current_used_blocks,
        object_commit_charge_blocks: exact.prefix_commit_charge_blocks,
        close_bound_blocks: exact.close_bound_blocks,
        capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
    }) {
        AdmissionDisposition::AdmitRemainOpen | AdmissionDisposition::AdmitThenFinalize => Ok(()),
        AdmissionDisposition::FinalizePrefixAndRetry => {
            Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "no-parity exact close reserve requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            })
        }
        AdmissionDisposition::RejectInvalidCapacityPolicy => {
            Err(PoolWriteError::InvalidInput(format!(
                "invalid physical capacity policy: low={low_watermark_blocks}, high={high_watermark_blocks}, capacity={capacity_blocks} blocks"
            )))
        }
    }
}

#[cfg(test)]
fn seal_selected_tape_if_needed(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    hardware_early_warning: bool,
) -> Result<bool, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let used_bytes = tape
        .total_committed_ordinals
        .checked_mul(block_size)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry("used capacity overflows u64".to_string())
        })?;
    let low_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_low)?;
    if seal_decision_after_write(
        TapePositionAfterWrite {
            used_bytes,
            early_warning: hardware_early_warning,
        },
        low_bytes,
        None,
    )
    .is_some()
    {
        state.seal_tape(selected.tape_uuid)?;
        return Ok(true);
    }
    Ok(false)
}

/// Evaluate tape sealing at a shared checkpoint boundary without projecting it.
///
/// Callers that receive a seal reason must finalize terminal media, append its
/// terminal-only checkpoint authority, and then project that record.
pub(crate) fn selected_tape_seal_reason_at_barrier(
    state: &CatalogIndex,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    position: TapePositionAfterWrite,
) -> Result<Option<TapeSealReason>, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let low_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_low)?;
    Ok(seal_decision_after_write(position, low_bytes, None))
}

fn missing_geometry(reason: impl Into<String>) -> WritabilityError {
    WritabilityError::MissingGeometry {
        reason: reason.into(),
    }
}

fn selected_tape_from_record(
    tape: TapeRecord,
    pool_id: &str,
) -> Result<SelectedTape, SelectTapeError> {
    let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id)?;
    let (block_size, parity_config) = selected_tape_geometry(&tape, pool_id)?;
    Ok(SelectedTape {
        pool_id: pool_id.to_string(),
        tape_uuid,
        block_size,
        parity_config,
    })
}

fn compare_tapes_for_pool_selection(left: &TapeRecord, right: &TapeRecord) -> std::cmp::Ordering {
    left.voltag
        .as_deref()
        .unwrap_or("")
        .cmp(right.voltag.as_deref().unwrap_or(""))
        .then_with(|| left.tape_uuid.cmp(&right.tape_uuid))
}

fn tape_uuid_from_vec(value: Vec<u8>, pool_id: &str) -> Result<TapeUuid, SelectTapeError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| SelectTapeError::InvalidTapeUuid {
            pool_id: pool_id.to_string(),
            actual_len: value.len(),
        })
}

pub(crate) fn selected_tape_geometry(
    tape: &TapeRecord,
    pool_id: &str,
) -> Result<(u32, ParityConfig), SelectTapeError> {
    let block_size = tape
        .block_size
        .ok_or_else(|| invalid_geometry(pool_id, "block_size is null"))
        .and_then(|value| {
            u32::try_from(value).map_err(|_| invalid_geometry(pool_id, "block_size overflows u32"))
        })?;
    let Some(scheme_id) = tape.scheme_id.clone() else {
        return Ok((block_size, ParityConfig::None));
    };
    let data_blocks_per_stripe = tape
        .data_blocks_per_stripe
        .ok_or_else(|| invalid_geometry(pool_id, "data_blocks_per_stripe is null"))
        .and_then(|value| {
            u16::try_from(value)
                .map_err(|_| invalid_geometry(pool_id, "data_blocks_per_stripe overflows u16"))
        })?;
    let parity_blocks_per_stripe = tape
        .parity_blocks_per_stripe
        .ok_or_else(|| invalid_geometry(pool_id, "parity_blocks_per_stripe is null"))
        .and_then(|value| {
            u16::try_from(value)
                .map_err(|_| invalid_geometry(pool_id, "parity_blocks_per_stripe overflows u16"))
        })?;
    let stripes_per_neighborhood = tape
        .stripes_per_neighborhood
        .ok_or_else(|| invalid_geometry(pool_id, "stripes_per_neighborhood is null"))?;
    let scheme = ParityScheme {
        id: SchemeId::new_owned(scheme_id),
        data_blocks_per_stripe,
        parity_blocks_per_stripe,
        stripes_per_neighborhood,
    };
    scheme
        .validate()
        .map_err(|err| invalid_geometry(pool_id, err.to_string()))?;
    Ok((block_size, ParityConfig::Scheme(scheme)))
}

fn invalid_geometry(pool_id: &str, reason: impl Into<String>) -> SelectTapeError {
    SelectTapeError::InvalidTapeGeometry {
        pool_id: pool_id.to_string(),
        reason: reason.into(),
    }
}

fn first_payload_body_lba(report: &StreamingObjectWriteReport) -> u64 {
    report
        .catalog
        .files
        .iter()
        .filter_map(|file| file.first_chunk_lba.map(|lba| lba.0))
        .min()
        .unwrap_or(0)
}

fn source_file_size(path: &Path) -> Result<u64, PoolWriteError> {
    let metadata = fs::metadata(path).map_err(|source| PoolWriteError::Io {
        context: "stat source file",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(PoolWriteError::InvalidInput(format!(
            "source path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata.len())
}

fn sha256_file(path: &Path) -> Result<[u8; 32], PoolWriteError> {
    let file = File::open(path).map_err(|source| PoolWriteError::Io {
        context: "open source file for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::with_capacity(HASH_BUFFER_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buf).map_err(|source| PoolWriteError::Io {
            context: "read source file for hashing",
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn parity_capacity_basis_blocks(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<u64, PoolWriteError> {
    terminal_capacity_basis_blocks(state, Some(pool_cfg), selected)
}

fn terminal_capacity_basis_blocks(
    state: &CatalogIndex,
    pool_cfg: Option<&TapePoolConfig>,
    selected: &SelectedTape,
) -> Result<u64, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity_bytes = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity_bytes = match pool_cfg {
        Some(pool_cfg) => effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity_bytes)
            .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?,
        None => raw_capacity_bytes,
    };
    let catalog_block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    if catalog_block_size != u64::from(selected.block_size) {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "selected block size {} does not match catalog tape block_size {catalog_block_size}",
            selected.block_size
        )));
    }
    let capacity_blocks = capacity_bytes / catalog_block_size;
    if capacity_blocks == 0 {
        return Err(PoolWriteError::MissingTapeGeometry(
            "tape capacity basis is smaller than one fixed block".to_string(),
        ));
    }
    Ok(capacity_blocks)
}

fn terminal_watermark_blocks(
    capacity_blocks: u64,
    pool_cfg: Option<&TapePoolConfig>,
) -> Result<(u64, u64), PoolWriteError> {
    match pool_cfg {
        Some(pool_cfg) => Ok((
            watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)?,
            watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_high)?,
        )),
        None => Ok((
            UNPOOLED_TERMINAL_LOW_WATERMARK_BLOCKS,
            UNPOOLED_TERMINAL_HIGH_WATERMARK_BLOCKS,
        )),
    }
}

fn reserve_parity_object_capacity(
    runtime: TerminalTripleCapacityRuntimeState,
    scheme: &ParityScheme,
    selected: &SelectedTape,
    terminal_authority: (&TapePoolConfig, u64, u64),
    capacity_blocks: u64,
    projected_object_blocks: u64,
    io_memory: &Arc<crate::io_memory::IoMemoryReservation>,
) -> Result<ParityCapacityReservation, PoolWriteError> {
    let remaining_tape_blocks = capacity_blocks
        .checked_sub(runtime.used_tape_blocks)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(format!(
                "physical tape position {} exceeds capacity basis {capacity_blocks}",
                runtime.used_tape_blocks
            ))
        })?;
    let data_shards_per_epoch = u64::from(scheme.data_blocks_per_stripe)
        .checked_mul(u64::from(scheme.stripes_per_neighborhood))
        .ok_or(ParityError::Invariant(
            "capacity reserve data-shard count overflows",
        ))?;
    let parity_shards_per_epoch = u64::from(scheme.parity_blocks_per_stripe)
        .checked_mul(u64::from(scheme.stripes_per_neighborhood))
        .ok_or(ParityError::Invariant(
            "capacity reserve parity-shard count overflows",
        ))?;
    let (pool_cfg, structural_entries_before_object, object_rows_before_object) =
        terminal_authority;
    if runtime.structural_entries_before_object != structural_entries_before_object
        || runtime.object_rows_before_object != object_rows_before_object
    {
        return Err(PoolWriteError::InvalidInput(format!(
            "terminal capacity journal/sink authority mismatch: journal S/R={structural_entries_before_object}/{object_rows_before_object}, sink S/R={}/{}",
            runtime.structural_entries_before_object, runtime.object_rows_before_object
        )));
    }
    let (low_watermark_blocks, high_watermark_blocks) =
        terminal_watermark_blocks(capacity_blocks, Some(pool_cfg))?;
    let mut input = TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks,
        block_size_bytes: selected.block_size,
        current_epoch_fill_blocks: runtime.current_epoch_fill_blocks,
        data_shards_per_epoch,
        parity_shards_per_epoch,
        pending_completed_sidecars: runtime.pending_completed_sidecars,
        sidecar_entries_before_object: runtime.sidecar_entries_before_object,
        structural_entries_before_object,
        object_rows_before_object,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS,
        remaining_tape_blocks,
        capacity_basis_blocks: capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
        pending_completed_epoch_parity_bytes: runtime.pending_completed_epoch_parity_bytes,
        remaining_spool_bytes: u64::MAX,
    };
    let report = match input.evaluate() {
        Ok(report) => report,
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            reserve_blocks: Some(required_reserve_blocks),
            ..
        }) if runtime.used_tape_blocks == PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS => {
            return Err(ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks,
                empty_tape_usable_blocks: capacity_blocks
                    .saturating_sub(PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS),
                required_reserve_blocks,
            }
            .into());
        }
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            ..
        }) => {
            return Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "exact terminal-close admission requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            });
        }
        Err(error) => return Err(error.into()),
    };
    match capacity_admission_disposition(CapacityAdmissionInput {
        current_used_blocks: runtime.used_tape_blocks,
        object_commit_charge_blocks: report.prefix_commit_charge_blocks,
        close_bound_blocks: report.close_bound_blocks,
        capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
    }) {
        AdmissionDisposition::AdmitRemainOpen | AdmissionDisposition::AdmitThenFinalize => {}
        AdmissionDisposition::FinalizePrefixAndRetry => {
            return Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "exact terminal-close admission requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            });
        }
        AdmissionDisposition::RejectInvalidCapacityPolicy => {
            return Err(PoolWriteError::InvalidInput(format!(
                "invalid physical capacity policy: low={low_watermark_blocks}, high={high_watermark_blocks}, capacity={capacity_blocks} blocks"
            )));
        }
    }
    let required_spool_bytes = report.required_spool_bytes;
    let spool_permit = io_memory
        .try_reserve_with_available(required_spool_bytes)
        .map_err(|available| ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::ParitySpoolCapacity,
            projected_object_blocks,
            remaining_blocks: None,
            reserve_blocks: None,
            remaining_spool_bytes: Some(available),
            required_spool_bytes: Some(required_spool_bytes),
        })?;
    input.remaining_spool_bytes = required_spool_bytes;
    let reservation = input.reserve_object()?;
    debug_assert_eq!(reservation.report(), &report);
    Ok(ParityCapacityReservation {
        reservation,
        _spool_permit: spool_permit,
    })
}

fn now_rfc3339() -> Result<String, PoolWriteError> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn uuid_text(value: [u8; 16]) -> String {
    Uuid::from_bytes(value).to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use remanence_aead::RecipientPrivateKey;

    use super::*;

    fn test_pool_write_resources() -> PoolWriteResources {
        PoolWriteResources::new(remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES)
            .expect("test pool-write resources")
    }

    fn test_capacity_pool_config(selected: &SelectedTape) -> TapePoolConfig {
        TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(selected.block_size),
            min_object_size_bytes: 0,
        }
    }

    /// Build a deterministic public recipient for encrypted pool-write tests.
    fn test_recipient(epoch_byte: u8, slot_index: u8) -> RecipientPublicKey {
        RecipientPrivateKey::new(
            [epoch_byte; 16],
            format!("pool-write-{epoch_byte:02x}"),
            [epoch_byte.wrapping_add(1); 32],
        )
        .expect("test private recipient")
        .public_key(slot_index)
        .expect("test public recipient")
    }

    #[test]
    fn cloned_pool_write_resources_share_one_atomic_budget() {
        let resources = PoolWriteResources::new(10).expect("shared resources");
        let clone = resources.clone();
        let held = resources
            .io_memory()
            .try_reserve(7)
            .expect("first clone reserves bytes");

        assert_eq!(
            clone.io_memory().try_reserve_with_available(4).unwrap_err(),
            3,
            "the second clone must observe the first clone's live grant"
        );
        drop(held);
        assert!(clone.io_memory().try_reserve_with_available(10).is_ok());
    }

    #[test]
    fn pool_write_result_rejects_physical_used_byte_overflow() {
        assert!(matches!(
            checked_physical_used_bytes(u64::MAX, 2),
            Err(PoolWriteError::PhysicalUsedBytesOverflow {
                position_lba: u64::MAX,
                block_size: 2,
            })
        ));
        assert_eq!(checked_physical_used_bytes(u64::MAX, 1).unwrap(), u64::MAX);
    }

    #[derive(Debug, Default)]
    struct LocateCountingSink {
        inner: VecBlockSink,
        locate_calls: u64,
    }

    impl BlockSink for LocateCountingSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.inner.write_block(buf)
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.inner.write_filemarks(count)
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.locate_calls = self.locate_calls.saturating_add(1);
            self.inner.locate(lba)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.position()
        }
    }

    #[derive(Debug, Default)]
    struct MisdirectedFreshLocateSink {
        inner: VecBlockSink,
    }

    impl BlockSink for MisdirectedFreshLocateSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.inner.write_block(buf)
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.inner.write_filemarks(count)
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            let mut position = self.inner.locate(lba)?;
            position.lba = 1;
            position.beginning_of_partition = false;
            Ok(position)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.position()
        }
    }

    #[derive(Debug)]
    struct FailOnBlockWriteSink {
        inner: VecBlockSink,
        fail_on_write: u64,
        write_calls: u64,
    }

    impl FailOnBlockWriteSink {
        /// Fail one fixed-block command after earlier writes have completed.
        fn new(fail_on_write: u64) -> Self {
            Self {
                inner: VecBlockSink::new(),
                fail_on_write,
                write_calls: 0,
            }
        }
    }

    impl BlockSink for FailOnBlockWriteSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.write_calls = self.write_calls.saturating_add(1);
            if self.write_calls == self.fail_on_write {
                return Err(TapeIoError::OperationFailed(
                    "injected parity raw write failure".to_string(),
                ));
            }
            self.inner.write_block(buf)
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.inner.write_filemarks(count)
        }

        fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
            self.inner.write_filemarks_immediate(count)
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.inner.locate(lba)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.position()
        }
    }

    /// Position-faithful sink for terminal-tail tests that does not retain the
    /// two one-GiB separation extents in memory.
    #[derive(Debug, Default)]
    struct SparseBlockSink {
        next_lba: u64,
        eod_lba: u64,
        block_writes: u64,
        filemark_writes: u64,
    }

    impl BlockSink for SparseBlockSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.next_lba = self.next_lba.checked_add(1).ok_or_else(|| {
                TapeIoError::OperationFailed("sparse sink LBA overflows u64".to_string())
            })?;
            self.eod_lba = self.eod_lba.max(self.next_lba);
            self.block_writes = self.block_writes.checked_add(1).ok_or_else(|| {
                TapeIoError::OperationFailed("sparse sink write count overflows u64".to_string())
            })?;
            Ok(WriteOutcome::from_device_position(
                u32::try_from(buf.len()).map_err(|_| {
                    TapeIoError::OperationFailed(
                        "sparse sink block length does not fit u32".to_string(),
                    )
                })?,
                false,
                false,
                TapePosition {
                    lba: self.next_lba,
                    partition: 0,
                    beginning_of_partition: false,
                    end_of_partition: false,
                    block_position_end_of_warning: false,
                },
            ))
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.next_lba = self.next_lba.checked_add(u64::from(count)).ok_or_else(|| {
                TapeIoError::OperationFailed("sparse sink LBA overflows u64".to_string())
            })?;
            self.eod_lba = self.eod_lba.max(self.next_lba);
            self.filemark_writes = self
                .filemark_writes
                .checked_add(u64::from(count))
                .ok_or_else(|| {
                    TapeIoError::OperationFailed(
                        "sparse sink filemark count overflows u64".to_string(),
                    )
                })?;
            Ok(WriteFilemarksOutcome::from_device_position(
                false,
                false,
                TapePosition {
                    lba: self.next_lba,
                    partition: 0,
                    beginning_of_partition: self.next_lba == 0,
                    end_of_partition: false,
                    block_position_end_of_warning: false,
                },
            ))
        }

        fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
            self.next_lba = self.eod_lba;
            self.position()
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.next_lba = lba;
            self.position()
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            Ok(TapePosition {
                lba: self.next_lba,
                partition: 0,
                beginning_of_partition: self.next_lba == 0,
                end_of_partition: false,
                block_position_end_of_warning: false,
            })
        }
    }

    #[derive(Debug)]
    struct RejectTerminalReplicaSink {
        inner: SparseBlockSink,
        tape_uuid: [u8; 16],
        terminal_replica_attempts: u64,
    }

    impl RejectTerminalReplicaSink {
        fn new(tape_uuid: [u8; 16]) -> Self {
            Self {
                inner: SparseBlockSink::default(),
                tape_uuid,
                terminal_replica_attempts: 0,
            }
        }
    }

    impl BlockSink for RejectTerminalReplicaSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            if remanence_parity::parse_tape_index_replica_header(buf, &self.tape_uuid).is_ok() {
                self.terminal_replica_attempts = self
                    .terminal_replica_attempts
                    .checked_add(1)
                    .ok_or_else(|| {
                        TapeIoError::OperationFailed(
                            "terminal replica attempt count overflows u64".to_string(),
                        )
                    })?;
                return Err(TapeIoError::OperationFailed(
                    "injected terminal replica failure".to_string(),
                ));
            }
            self.inner.write_block(buf)
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.inner.write_filemarks(count)
        }

        fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
            self.inner.write_filemarks_immediate(count)
        }

        fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.space_to_end_of_data()
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.inner.locate(lba)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.position()
        }
    }

    #[derive(Debug)]
    struct StagedTestSink {
        inner: VecBlockSink,
        batch_blocks: u32,
        fail_on_batch_call: Option<u64>,
        batch_error: Option<TapeIoError>,
        early_warning_on_batch_call: Option<u64>,
        fail_space_to_eod: bool,
        fail_position: bool,
        fail_filemark: bool,
        pending_deferred_audit: bool,
        audited_partial_sense: bool,
        batch_calls: u64,
        events: Vec<String>,
        ring_buffers: u32,
        cdbs: Vec<Vec<u8>>,
        alignments: Vec<usize>,
        diagnostic_ioctl_samples: u64,
        diagnostic_resets: u64,
        diagnostic_publications: u64,
        ordered_events: Arc<Mutex<Vec<String>>>,
        position_overrides: VecDeque<TapePosition>,
    }

    impl StagedTestSink {
        fn new(batch_blocks: u32) -> Self {
            assert!(batch_blocks > 1, "staged test must exercise batching");
            Self {
                inner: VecBlockSink::new(),
                batch_blocks,
                fail_on_batch_call: None,
                batch_error: None,
                early_warning_on_batch_call: None,
                fail_space_to_eod: false,
                fail_position: false,
                fail_filemark: false,
                pending_deferred_audit: false,
                audited_partial_sense: false,
                batch_calls: 0,
                events: Vec::new(),
                ring_buffers: remanence_library::DEFAULT_TAPE_IO_STAGING_RING_BUFFERS,
                cdbs: Vec::new(),
                alignments: Vec::new(),
                diagnostic_ioctl_samples: 0,
                diagnostic_resets: 0,
                diagnostic_publications: 0,
                ordered_events: Arc::new(Mutex::new(Vec::new())),
                position_overrides: VecDeque::new(),
            }
        }

        fn failing_on_batch(batch_blocks: u32, fail_on_batch_call: u64) -> Self {
            let mut sink = Self::new(batch_blocks);
            sink.fail_on_batch_call = Some(fail_on_batch_call);
            sink
        }

        fn with_ring(batch_blocks: u32, ring_buffers: u32) -> Self {
            let mut sink = Self::new(batch_blocks);
            sink.ring_buffers = ring_buffers;
            sink
        }
    }

    impl BlockSink for StagedTestSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.events.push(format!("write_block:{}", buf.len()));
            self.inner.write_block(buf)
        }

        fn write_block_batch(
            &mut self,
            buf: &[u8],
            block_size_bytes: u32,
        ) -> Result<WriteBatchOutcome, TapeIoError> {
            let records = records_in_staged_batch(buf, block_size_bytes)
                .expect("test batch contains whole records");
            self.batch_calls = self.batch_calls.saturating_add(1);
            self.diagnostic_ioctl_samples = self.diagnostic_ioctl_samples.saturating_add(1);
            self.events.push(format!("write_batch:{records}"));
            if let Some(error) = self.batch_error.take() {
                return Err(error);
            }
            if self.fail_on_batch_call == Some(self.batch_calls) {
                return Err(TapeIoError::OperationFailed(format!(
                    "injected sink failure on batch {}",
                    self.batch_calls
                )));
            }
            let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
            if self.early_warning_on_batch_call == Some(self.batch_calls) {
                Ok(WriteBatchOutcome::from_computed_position(
                    outcome.records_written,
                    outcome.bytes_written,
                    true,
                    false,
                    outcome.position_after,
                ))
            } else {
                Ok(outcome)
            }
        }

        fn write_block_batch_pipelined(
            &mut self,
            buf: &[u8],
            block_size_bytes: u32,
            cdb: &[u8],
        ) -> Result<WriteBatchOutcome, TapeIoError> {
            self.cdbs.push(cdb.to_vec());
            self.alignments
                .push((buf.as_ptr() as usize) % system_page_size());
            self.ordered_events
                .lock()
                .expect("ordered events")
                .push("classify".into());
            self.write_block_batch(buf, block_size_bytes)
        }

        fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
            self.batch_blocks
        }

        fn requested_write_batch_blocks(&self) -> u32 {
            self.batch_blocks
        }

        fn staging_ring_buffers(&self) -> u32 {
            self.ring_buffers
        }

        fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
            PipelinedWriteDiagnostics {
                ioctl_samples: self.diagnostic_ioctl_samples,
                ioctl_max_us: self.diagnostic_ioctl_samples.saturating_mul(1_000),
                ..PipelinedWriteDiagnostics::default()
            }
        }

        fn reset_pipelined_write_diagnostics(&mut self) {
            self.diagnostic_ioctl_samples = 0;
            self.diagnostic_resets = self.diagnostic_resets.saturating_add(1);
        }

        fn publish_pipelined_write_diagnostics(&mut self) {
            self.diagnostic_publications += 1;
        }

        fn begin_pipelined_write_window(
            &mut self,
            command_count: u32,
            bytes: u64,
            first_records: u32,
            last_records: u32,
        ) {
            self.events.push(format!(
                "intent:{command_count}:{bytes}:{first_records}:{last_records}"
            ));
        }

        fn finish_pipelined_write_window_success(
            &mut self,
            command_count: u32,
            bytes: u64,
            first_records: u32,
            last_records: u32,
            _duration: Duration,
        ) {
            self.events.push(format!(
                "span_ok:{command_count}:{bytes}:{first_records}:{last_records}"
            ));
        }

        fn finish_pipelined_write_window_error(
            &mut self,
            _command_count: u32,
            _bytes: u64,
            _first_records: u32,
            _last_records: u32,
            error: &TapeIoError,
        ) {
            self.audited_partial_sense = matches!(
                error,
                TapeIoError::PartialBatchUncommittable { sense: Some(_), .. }
            );
            self.ordered_events
                .lock()
                .expect("ordered events")
                .push("audit".into());
            self.events.push("span_error".into());
        }

        fn flush_pending_pipeline_audit(&mut self) {
            if self.pending_deferred_audit {
                self.pending_deferred_audit = false;
                self.ordered_events
                    .lock()
                    .expect("ordered events")
                    .push("audit".into());
            }
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.events.push(format!("filemark:{count}"));
            self.inner.write_filemarks(count)
        }

        fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
            self.events.push(format!("filemark_immediate:{count}"));
            self.inner.write_filemarks(count).map(|_| ())
        }

        fn write_filemarks_pipelined(
            &mut self,
            count: u32,
        ) -> Result<WriteFilemarksOutcome, TapeIoError> {
            if self.fail_filemark {
                self.pending_deferred_audit = true;
                return Err(TapeIoError::OperationFailed(
                    "injected WRITE FILEMARKS failure".into(),
                ));
            }
            self.write_filemarks(count)
        }

        fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
            self.events.push("space_eod".to_string());
            if self.fail_space_to_eod {
                return Err(TapeIoError::OperationFailed(
                    "injected space-to-EOD failure".into(),
                ));
            }
            self.inner.space_to_end_of_data()
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.events.push(format!("locate:{lba}"));
            self.inner.locate(lba)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.events.push("position".to_string());
            if self.fail_position {
                return Err(TapeIoError::OperationFailed(
                    "injected READ POSITION failure".into(),
                ));
            }
            match self.position_overrides.pop_front() {
                Some(position) => Ok(position),
                None => self.inner.position(),
            }
        }
    }

    fn tape_position_with_warning(block_position_end_of_warning: bool) -> TapePosition {
        TapePosition {
            lba: 0,
            partition: 0,
            beginning_of_partition: false,
            end_of_partition: false,
            block_position_end_of_warning,
        }
    }

    #[derive(Debug)]
    struct SingleBlockOutcomeSink {
        bytes_written: u32,
        early_warning: bool,
        end_of_medium: bool,
        writes: u64,
    }

    impl BlockSink for SingleBlockOutcomeSink {
        fn write_block(&mut self, _buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            self.writes = self.writes.saturating_add(1);
            Ok(WriteOutcome::from_computed_position(
                self.bytes_written,
                self.early_warning,
                self.end_of_medium,
                TapePosition {
                    lba: self.writes,
                    partition: 0,
                    beginning_of_partition: false,
                    end_of_partition: false,
                    block_position_end_of_warning: self.early_warning,
                },
            ))
        }

        fn write_filemarks(&mut self, _count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            Err(TapeIoError::OperationFailed(
                "single-block test sink does not write filemarks".to_string(),
            ))
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            Ok(TapePosition {
                lba: self.writes,
                partition: 0,
                beginning_of_partition: self.writes == 0,
                end_of_partition: false,
                block_position_end_of_warning: self.early_warning,
            })
        }
    }

    #[test]
    fn fixed_block_helper_rejects_short_bytes_and_hard_eom() {
        for (bytes_written, end_of_medium) in [(3, false), (4, true)] {
            let mut sink = SingleBlockOutcomeSink {
                bytes_written,
                early_warning: false,
                end_of_medium,
                writes: 0,
            };

            let error = write_fixed_blocks(&mut sink, 4, &[0xA5; 4])
                .expect_err("incomplete fixed block must fail");
            let message = error.to_string();
            assert!(
                message.contains("partial fixed batch uncommittable"),
                "{message}"
            );
            assert!(message.contains("requested_bytes=4"), "{message}");
            assert!(
                message.contains(&format!("written_bytes={bytes_written}")),
                "{message}"
            );
            assert!(
                message.contains(&format!("end_of_medium={end_of_medium}")),
                "{message}"
            );
        }
    }

    #[test]
    fn fixed_block_helper_accepts_full_bytes_with_early_warning() {
        let mut sink = SingleBlockOutcomeSink {
            bytes_written: 4,
            early_warning: true,
            end_of_medium: false,
            writes: 0,
        };

        let blocks = write_fixed_blocks(&mut sink, 4, &[0x5A; 8])
            .expect("full fixed blocks remain successful at early warning");

        assert_eq!(blocks, 2);
        assert_eq!(sink.writes, 2);
    }

    #[tokio::test]
    async fn overlap_first_block_gate_requires_high_prefill_then_position_proof() {
        let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity)
                .expect("ring");
        producer
            .push(&vec![0x11; crate::append_ring::APPEND_RING_SLAB_BYTES / 2])
            .await
            .expect("sub-high prefill");
        let mut sink = StagedTestSink::new(2);
        {
            let mut gated = OverlapBlockSink {
                inner: &mut sink,
                control: Arc::clone(&control),
                expected_initial_lba: 0,
                expected_next_lba: 0,
                initial_position_proved: false,
                write_started: false,
                low_water_events: 0,
            };
            let error = gated
                .write_block(&[0u8; 4])
                .expect_err("sub-high ring must not reach tape");
            assert!(error.to_string().contains("first-block gate"), "{error}");
            let error = gated
                .space_to_end_of_data()
                .expect_err("sub-high ring must not position an append");
            assert!(error.to_string().contains("first-block gate"), "{error}");
        }
        assert!(
            sink.events.is_empty(),
            "no tape command may precede the high-water gate: {:?}",
            sink.events
        );

        producer
            .push(&vec![0x22; crate::append_ring::APPEND_RING_SLAB_BYTES / 2])
            .await
            .expect("reach high prefill");
        {
            let mut gated = OverlapBlockSink {
                inner: &mut sink,
                control,
                expected_initial_lba: 0,
                expected_next_lba: 0,
                initial_position_proved: false,
                write_started: false,
                low_water_events: 0,
            };
            gated.write_block(&[0u8; 4]).expect("gated first block");
        }
        assert_eq!(sink.events, ["position", "write_block:4"]);
    }

    #[test]
    fn no_parity_append_lba_accepts_tape_files_beyond_u32() {
        let append = NoParityAppendContext {
            tape_file_number: u64::from(u32::MAX) + 1,
            previous_total_committed_ordinals: 11,
            fresh_tape: false,
            expected_append_lba: None,
        };

        assert_eq!(append.expected_append_lba().unwrap(), 4_294_967_308);
    }

    #[tokio::test]
    async fn overlap_append_gate_counts_bootstrap_and_committed_trailing_filemark() {
        let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity)
                .expect("ring");
        producer
            .push(&vec![0x21; crate::append_ring::APPEND_RING_SLAB_BYTES])
            .await
            .expect("reach high prefill");

        let object_blocks = 2u64;
        let append = NoParityAppendContext {
            tape_file_number: 2,
            previous_total_committed_ordinals: object_blocks,
            fresh_tape: false,
            expected_append_lba: None,
        };
        let expected = append.expected_append_lba().expect("expected append LBA");

        let mut sink = StagedTestSink::new(2);
        sink.write_block(&[0xb0; 4]).expect("bootstrap block");
        sink.write_filemarks(1).expect("bootstrap filemark");
        for _ in 0..object_blocks {
            sink.write_block(&[0x0b; 4])
                .expect("committed object block");
        }
        sink.write_filemarks(1)
            .expect("committed object trailing filemark");
        sink.inner.set_next_lba_for_test(0);
        sink.events.clear();

        {
            let mut gated = OverlapBlockSink {
                inner: &mut sink,
                control: Arc::clone(&control),
                expected_initial_lba: expected,
                expected_next_lba: expected,
                initial_position_proved: false,
                write_started: false,
                low_water_events: 0,
            };
            let observed = position_no_parity_append(&mut gated).expect("position and prove EOD");
            assert_eq!(expected, observed.lba);
            assert_eq!(observed.lba, 5, "block + filemark records define EOD");
        }
        assert_eq!(sink.events, ["space_eod"]);

        sink.inner.set_next_lba_for_test(0);
        sink.events.clear();
        let old_fencepost = expected.checked_sub(1).expect("nonzero append LBA");
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control,
            expected_initial_lba: old_fencepost,
            expected_next_lba: old_fencepost,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        let error = position_no_parity_append(&mut gated)
            .expect_err("one-LBA fencepost must still fail closed");
        assert!(
            error.to_string().contains(&format!(
                "expected partition 0 lba {old_fencepost}, observed partition 0 lba {expected}"
            )),
            "{error}"
        );
        assert_eq!(sink.events, ["space_eod"]);
    }

    #[test]
    fn batched_recovery_locates_to_journal_eod_and_overwrites_longer_physical_tail() {
        let mut sink = StagedTestSink::new(2);
        for seed in 0..7u8 {
            sink.write_block(&[seed; 4]).expect("seed physical tail");
        }
        sink.events.clear();

        position_no_parity_append_at_checkpoint(&mut sink, 3)
            .expect("journal EOD is authoritative despite longer physical tail");

        assert_eq!(sink.events, ["locate:3", "position"]);
        assert_eq!(sink.position().expect("position after locate").lba, 3);
        sink.write_block(&[0x99; 4])
            .expect("overwrite orphaned tail");
        assert_eq!(sink.position().expect("position after overwrite").lba, 4);
    }

    #[tokio::test]
    async fn overlap_batched_recovery_uses_locate_instead_of_space_eod() {
        let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity)
                .expect("ring");
        producer
            .push(&vec![0x21; crate::append_ring::APPEND_RING_SLAB_BYTES])
            .await
            .expect("reach high prefill");
        let mut sink = StagedTestSink::new(2);
        for seed in 0..7u8 {
            sink.write_block(&[seed; 4]).expect("seed physical tail");
        }
        sink.events.clear();

        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control,
            expected_initial_lba: 3,
            expected_next_lba: 3,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        position_no_parity_append_at_checkpoint(&mut gated, 3)
            .expect("overlap recovery locates to journal EOD");

        assert_eq!(sink.events, ["locate:3", "position"]);
    }

    #[test]
    fn overlap_low_water_pause_refills_then_reproves_next_lba() {
        let capacity = 4 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, mut consumer, control) =
            crate::append_ring::create_append_ring(&manager, capacity, 50, 25, 8 * capacity)
                .expect("ring");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(producer.push(&vec![
                0x31;
                2 * crate::append_ring::APPEND_RING_SLAB_BYTES + 1
            ]))
            .expect("initial high-water fill");

        let mut sink = StagedTestSink::new(2);
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control: Arc::clone(&control),
            expected_initial_lba: 0,
            expected_next_lba: 0,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        gated.write_block(&[0x41; 4]).expect("first block");

        let mut drained = vec![0u8; crate::append_ring::APPEND_RING_SLAB_BYTES + 1];
        consumer
            .read_exact(&mut drained)
            .expect("drain to low watermark");
        assert!(control.should_pause(), "ring must be at the low watermark");

        let refill = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("refill runtime");
            runtime.block_on(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                producer
                    .push(&vec![0x52; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES])
                    .await
                    .expect("refill to high watermark");
                producer
            })
        });
        gated
            .write_block(&[0x42; 4])
            .expect("resume after fresh proof");
        let producer = refill.join().expect("refill thread");
        drop(producer);
        drop(gated);

        assert_eq!(
            sink.events,
            [
                "position",
                "write_block:4",
                "position",
                "position",
                "write_block:4",
            ],
            "resume must flush/prove, wait, then issue a fresh proof before WRITE"
        );
    }

    #[test]
    fn overlap_resume_refuses_position_drift_before_the_next_write() {
        let capacity = 4 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, mut consumer, control) =
            crate::append_ring::create_append_ring(&manager, capacity, 50, 25, 8 * capacity)
                .expect("ring");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime
            .block_on(producer.push(&vec![
                0x61;
                2 * crate::append_ring::APPEND_RING_SLAB_BYTES + 1
            ]))
            .expect("initial high-water fill");

        let position = |lba| TapePosition {
            lba,
            partition: 0,
            beginning_of_partition: lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        };
        let mut sink = StagedTestSink::new(2);
        sink.position_overrides = [position(0), position(1), position(9)].into();
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control: Arc::clone(&control),
            expected_initial_lba: 0,
            expected_next_lba: 0,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        gated.write_block(&[0x71; 4]).expect("first block");
        let mut drained = vec![0u8; crate::append_ring::APPEND_RING_SLAB_BYTES + 1];
        consumer.read_exact(&mut drained).expect("drain to low");

        let refill = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("refill runtime");
            runtime.block_on(async move {
                producer
                    .push(&vec![0x72; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES])
                    .await
                    .expect("refill to high");
            });
        });
        let error = gated
            .write_block(&[0x73; 4])
            .expect_err("drifted resume must fail closed");
        refill.join().expect("refill thread");
        drop(gated);
        assert!(error.to_string().contains("position drift"), "{error}");
        assert_eq!(
            sink.events,
            ["position", "write_block:4", "position", "position"],
            "no WRITE may follow a failed resume proof"
        );
    }

    #[test]
    fn overlap_admission_plans_object_larger_than_legacy_64_gib_cap() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-large-overlap-admission-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x6b; 16];
        let block_size = 256 * 1024;
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "LARGE1L9".into(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "large.overlap".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::None,
        };
        let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
        let (_producer, consumer, control) = crate::append_ring::create_append_ring(
            &manager,
            ring_bytes,
            90,
            25,
            crate::APPEND_SPOOL_MAX_BYTES + 1,
        )
        .expect("ring");
        let digest = [0x42; 32];
        let request = WriteObjectToPoolRequest {
            pool_id: selected.pool_id.clone(),
            source: WriteObjectSource::Streamed(StreamedWriteSource::new(
                consumer,
                crate::APPEND_SPOOL_MAX_BYTES + 1,
                digest,
                control,
            )),
            archive_path: "large.bin".into(),
            caller_object_id: "overlap-larger-than-spool-cap".into(),
            expected_content_sha256: Some(digest),
            representation: PoolWriteRepresentation::Plaintext,
        };

        let prepared = prepare_pool_object(&request, selected.block_size)
            .expect("large streamed source must be plannable without payload");
        assert_eq!(
            prepared_payload_bytes(&prepared),
            crate::APPEND_SPOOL_MAX_BYTES + 1
        );
        let stored = prepare_stored_object(&prepared, &request.representation)
            .expect("plaintext stored plan");
        let footprint = stored_footprint_bytes(&stored, &prepared, selected.block_size)
            .expect("large footprint");
        ensure_selected_tape_has_capacity(
            &state,
            &test_capacity_pool_config(&selected),
            &selected,
            footprint,
            None,
        )
        .expect("mocked LTO-9 admits object beyond the legacy spool cap");
    }

    #[test]
    fn batched_capacity_uses_session_local_physical_extent() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-batched-capacity-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x6c; 16];
        let block_size = 256 * 1024;
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "BATCH1L9".into(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "batched.capacity".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::None,
        };
        let capacity = raw_capacity_bytes(LtoGen::Lto9);
        let provisional_used_lba = capacity / u64::from(block_size) - 1;
        let object_size = u64::from(block_size) * 2;

        ensure_selected_tape_has_capacity(
            &state,
            &test_capacity_pool_config(&selected),
            &selected,
            object_size,
            None,
        )
        .expect("empty SQLite projection alone would admit the object");
        let err = ensure_selected_tape_has_capacity(
            &state,
            &test_capacity_pool_config(&selected),
            &selected,
            object_size,
            Some(provisional_used_lba),
        )
        .expect_err("the provisional physical prefix consumes capacity");
        assert!(
            matches!(err, PoolWriteError::SelectedTapeInsufficientCapacity { .. }),
            "{err}"
        );
    }

    #[test]
    fn no_parity_exact_close_capacity_rolls_before_whole_object_motion() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-no-parity-terminal-capacity-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x73; 16];
        let block_size = 256 * 1024;
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "NCAP01L9".into(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "capacity.no-parity".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::None,
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.97,
            watermark_high: 0.98,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let capacity_blocks = raw_capacity_bytes(LtoGen::Lto9) / u64::from(block_size);
        let high_watermark_blocks = watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_high)
            .expect("high watermark");
        let context = BatchedNoParityAppendContext {
            append: NoParityAppendContext {
                tape_file_number: 3,
                previous_total_committed_ordinals: 1,
                fresh_tape: false,
                expected_append_lba: Some(high_watermark_blocks),
            },
            position: BatchedAppendPosition::JournalEod(high_watermark_blocks),
            object_row_count: 1,
        };

        let error =
            ensure_no_parity_terminal_close_capacity(&state, &pool_cfg, &selected, &context, 1)
                .expect_err("the current prefix must close before any block of the Object moves");
        assert!(matches!(
            error,
            PoolWriteError::TerminalCloseRequired { .. }
        ));

        let fresh = BatchedNoParityAppendContext {
            append: NoParityAppendContext {
                tape_file_number: 1,
                previous_total_committed_ordinals: 0,
                fresh_tape: true,
                expected_append_lba: None,
            },
            position: BatchedAppendPosition::FreshTape,
            object_row_count: 0,
        };
        let error = ensure_no_parity_terminal_close_capacity(
            &state,
            &pool_cfg,
            &selected,
            &fresh,
            capacity_blocks,
        )
        .expect_err("an impossible whole Object must be rejected on fresh media");
        assert!(matches!(
            error,
            PoolWriteError::Parity(ParityError::ObjectTooLargeForEmptyTape { .. })
        ));
    }

    #[test]
    fn downward_pool_cap_is_the_shared_selection_and_terminal_capacity_basis() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-downward-capacity-basis-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x74; 16];
        let block_size = 256 * 1024;
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "DCAP01L9".into(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "capacity.downward".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::None,
        };
        let cap_bytes = 4 * 1024 * 1024 * 1024_u64;
        let pool_cfg = TapePoolConfig {
            capacity_cap_bytes: Some(cap_bytes),
            ..test_capacity_pool_config(&selected)
        };
        let expected_c = cap_bytes / u64::from(block_size);

        assert_eq!(
            parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capped terminal C"),
            expected_c
        );
        let tape = state
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape");
        let fit = tape_fit_state_from_record(&tape, &pool_cfg, &selected.pool_id, 0)
            .expect("capped selection projection");
        assert_eq!(
            fit.usable_bytes,
            watermark_floor_bytes(cap_bytes, pool_cfg.watermark_high).expect("capped H")
        );
        ensure_selected_tape_has_capacity(&state, &pool_cfg, &selected, cap_bytes + 1, None)
            .expect_err("general admission must use the same capped C");

        let upward = TapePoolConfig {
            capacity_cap_bytes: Some(raw_capacity_bytes(LtoGen::Lto9)),
            ..pool_cfg
        };
        assert!(parity_capacity_basis_blocks(&state, &upward, &selected).is_err());
    }

    #[test]
    fn close_only_authority_uses_capped_c_at_equality_and_below_low() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-close-only-capacity-authority-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x5c; 16];
        let block_size = 256 * 1024;
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP005L9".into(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "capacity.close-only".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::None,
        };
        let capacity_blocks = 1_000_000;
        let pool_cfg = TapePoolConfig {
            watermark_low: 0.1,
            watermark_high: 0.2,
            capacity_cap_bytes: Some(capacity_blocks * u64::from(block_size)),
            ..test_capacity_pool_config(&selected)
        };
        let counts = remanence_parity::TapeIndexReplicaCounts {
            structural_entry_count: 1,
            object_row_count: 0,
        };
        let replica = remanence_parity::checked_tape_index_replica_layout(block_size, counts)
            .expect("replica layout");
        let gap_records = remanence_parity::index_separation_records(
            block_size,
            remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        )
        .expect("gap layout");
        let tail_charge = 3 * (replica.replica_record_count + 1) + 2 * (gap_records + 1);

        let equality_start = capacity_blocks - tail_charge - TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS;
        let equality_layout = remanence_parity::TerminalTailLayout::new(
            0,
            block_size,
            1,
            equality_start,
            replica.replica_record_count,
            gap_records,
        )
        .expect("equality tail layout");
        let equality = authorize_terminal_close_only_plan(
            &state,
            Some(&pool_cfg),
            &selected,
            equality_start,
            counts,
            equality_layout.expected_eod_lba,
        )
        .expect("C equality succeeds");
        assert_eq!(
            equality.required_tape_blocks,
            capacity_blocks - equality_start
        );

        let one_short_cfg = TapePoolConfig {
            capacity_cap_bytes: Some((capacity_blocks - 1) * u64::from(block_size)),
            ..pool_cfg.clone()
        };
        let one_short = authorize_terminal_close_only_plan(
            &state,
            Some(&one_short_cfg),
            &selected,
            equality_start,
            counts,
            equality_layout.expected_eod_lba,
        )
        .expect_err("one block below the exact capped C must fail");
        assert!(
            matches!(
                one_short,
                PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                    cause: CapacityReserveCause::TapeCapacity,
                    ..
                })
            ),
            "{one_short}"
        );

        let below_low_start = 100;
        assert!(
            below_low_start
                < watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)
                    .expect("low watermark")
        );
        let below_low_layout = remanence_parity::TerminalTailLayout::new(
            0,
            block_size,
            1,
            below_low_start,
            replica.replica_record_count,
            gap_records,
        )
        .expect("below-low tail layout");
        authorize_terminal_close_only_plan(
            &state,
            Some(&pool_cfg),
            &selected,
            below_low_start,
            counts,
            below_low_layout.expected_eod_lba,
        )
        .expect("manual close below L uses the same exact authority");

        let unpooled_capacity_blocks = raw_capacity_bytes(LtoGen::Lto9) / u64::from(block_size);
        assert_eq!(
            terminal_watermark_blocks(unpooled_capacity_blocks, None)
                .expect("unpooled terminal watermarks"),
            (0, 1)
        );
        let unpooled_equality_start =
            unpooled_capacity_blocks - tail_charge - TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS;
        let unpooled_equality_layout = remanence_parity::TerminalTailLayout::new(
            0,
            block_size,
            1,
            unpooled_equality_start,
            replica.replica_record_count,
            gap_records,
        )
        .expect("unpooled equality tail layout");
        let unpooled = authorize_terminal_close_only_plan(
            &state,
            None,
            &selected,
            unpooled_equality_start,
            counts,
            unpooled_equality_layout.expected_eod_lba,
        )
        .expect("unpooled close uses raw C with the canonical L/H basis");
        assert_eq!(
            unpooled.required_tape_blocks,
            unpooled_capacity_blocks - unpooled_equality_start
        );
    }

    #[test]
    fn parity_capacity_reservation_uses_physical_cursor_exact_layout_and_atomic_spool_grant() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-capacity-runtime-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x6d; 16];
        let block_size = 256 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("capacity-runtime-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP001L9".into(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "capacity.runtime".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::Scheme(scheme.clone()),
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.97,
            watermark_high: 0.98,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let capacity_blocks =
            parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capacity basis");
        let mut backing = VecBlockSink::new();
        let mut raw = BlockSinkRawTapeSink::new(&mut backing);
        let mut journal = PerObjectTestJournal {
            tape_uuid,
            bundles: Vec::new(),
        };
        let mut parity =
            ParitySink::new_with_journal(&mut raw, &mut journal, scheme, tape_uuid, block_size)
                .expect("parity sink");
        parity.write_bootstrap().expect("initial bootstrap");
        assert_eq!(
            parity
                .terminal_triple_capacity_runtime_state()
                .expect("runtime state")
                .used_tape_blocks,
            2,
            "physical cursor includes the bootstrap body and trailing filemark"
        );

        let io_memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024)
            .expect("spool reservation manager");
        let mismatch = reserve_parity_object_capacity(
            parity
                .terminal_triple_capacity_runtime_state()
                .expect("runtime state"),
            parity.scheme(),
            &selected,
            (&pool_cfg, 0, 0),
            capacity_blocks,
            2,
            &io_memory,
        )
        .expect_err("journal authority cannot omit the live BOT structural row");
        assert!(
            mismatch
                .to_string()
                .contains("journal/sink authority mismatch"),
            "{mismatch}"
        );
        assert_eq!(io_memory.granted(), 0);

        let reservation = reserve_parity_object_capacity(
            parity
                .terminal_triple_capacity_runtime_state()
                .expect("runtime state"),
            parity.scheme(),
            &selected,
            (&pool_cfg, 1, 0),
            capacity_blocks,
            2,
            &io_memory,
        )
        .expect("exact reservation");
        let report = *reservation.report();
        assert!(report.projected_object_present);
        assert_eq!(report.object_tape_file_blocks, 3);
        assert_eq!(
            report.safety_margin_blocks,
            TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS
        );
        assert!(report.final_parity_map_tape_file_blocks > 0);
        assert_eq!(io_memory.granted(), report.required_spool_bytes);

        let (reservation, spool_permit) = reservation.into_parts();
        parity
            .begin_object_with_terminal_triple_reservation(reservation)
            .expect("begin after atomic reservation");
        parity
            .write_block(&vec![0xA5; block_size as usize])
            .expect("first Object block");
        parity
            .write_block(&vec![0x5A; block_size as usize])
            .expect("second Object block");
        assert_eq!(
            io_memory.granted(),
            report.required_spool_bytes,
            "spool grant remains held while completed parity is staged"
        );
        parity.finish_object().expect("emit reserved sidecar");
        assert_eq!(
            io_memory.granted(),
            report.required_spool_bytes,
            "spool grant remains held through sidecar emission"
        );

        drop(spool_permit);
        assert_eq!(io_memory.granted(), 0);
    }

    #[test]
    fn exact_terminal_admission_rejects_high_watermark_crossing_before_spool_grant() {
        let block_size = 1024 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("terminal-admission-test"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        };
        let selected = SelectedTape {
            pool_id: "terminal.admission".to_string(),
            tape_uuid: [0x6c; 16],
            block_size,
            parity_config: ParityConfig::Scheme(scheme.clone()),
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.1,
            watermark_high: 0.2,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let io_memory =
            crate::io_memory::IoMemoryReservation::new(1).expect("one-byte spool manager");
        let error = reserve_parity_object_capacity(
            TerminalTripleCapacityRuntimeState {
                current_epoch_fill_blocks: 0,
                pending_completed_sidecars: 0,
                pending_completed_epoch_parity_bytes: 0,
                sidecar_entries_before_object: 0,
                structural_entries_before_object: 1,
                object_rows_before_object: 0,
                used_tape_blocks: 1_900,
            },
            &scheme,
            &selected,
            (&pool_cfg, 1, 0),
            10_000,
            200,
            &io_memory,
        )
        .expect_err("projected prefix crosses H and must roll before Object motion");
        assert!(
            error
                .to_string()
                .contains("requires finalizing the current prefix"),
            "{error}"
        );
        assert_eq!(
            io_memory.granted(),
            0,
            "rejected admission must precede the atomic parity spool grant"
        );
    }

    #[test]
    fn parity_spool_shortfall_is_rejected_before_object_tape_motion() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-capacity-shortfall-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x6e; 16];
        let block_size = 256 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("capacity-shortfall-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP002L9".into(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "capacity.shortfall".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::Scheme(scheme.clone()),
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.97,
            watermark_high: 0.98,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let capacity_blocks =
            parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capacity basis");
        let mut backing = VecBlockSink::new();
        let mut raw = BlockSinkRawTapeSink::new(&mut backing);
        let mut journal = PerObjectTestJournal {
            tape_uuid,
            bundles: Vec::new(),
        };
        let mut parity =
            ParitySink::new_with_journal(&mut raw, &mut journal, scheme, tape_uuid, block_size)
                .expect("parity sink");
        parity.write_bootstrap().expect("initial bootstrap");
        let position_before = parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state")
            .used_tape_blocks;
        let io_memory =
            crate::io_memory::IoMemoryReservation::new(1).expect("one-byte spool budget");

        let error = reserve_parity_object_capacity(
            parity
                .terminal_triple_capacity_runtime_state()
                .expect("runtime state"),
            parity.scheme(),
            &selected,
            (&pool_cfg, 1, 0),
            capacity_blocks,
            2,
            &io_memory,
        )
        .expect_err("completed parity epoch exceeds the atomic spool budget");
        assert!(
            matches!(
                error,
                PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                    cause: CapacityReserveCause::ParitySpoolCapacity,
                    remaining_spool_bytes: Some(1),
                    ..
                })
            ),
            "{error}"
        );
        assert_eq!(
            parity
                .terminal_triple_capacity_runtime_state()
                .expect("runtime state")
                .used_tape_blocks,
            position_before,
            "capacity rejection performs no Object write or filemark"
        );
        assert_eq!(io_memory.granted(), 0);
    }

    #[test]
    fn direct_encrypted_parity_write_uses_configured_spool_ceiling_before_object_motion() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-direct-encrypted-parity-spool-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "capacity.direct-encrypted";
        let tape_uuid = [0x71; 16];
        let block_size = 256 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("direct-encrypted-capacity-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        state
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP005L9".into(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision mocked LTO-9");
        state
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let pool_cfg = TapePoolConfig {
            id: pool_id.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let selected = select_tape_in_pool(&state, &pool_cfg, 32 * 1024, &HashSet::new())
            .expect("fresh parity tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, vec![0x5A; 32 * 1024]).expect("write payload");
        let checkpoint_dir = temp.path().join("checkpoints");
        let parity_journal_path = temp.path().join("parity.remjournal");
        let resources = PoolWriteResources::new(1).expect("one-byte configured spool ceiling");
        let mut sink = VecBlockSink::new();

        let error = write_to_selected_tape_checkpointed(
            &mut state,
            &mut sink,
            &pool_cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.into(),
                source: WriteObjectSource::Path(payload_path),
                archive_path: PathBuf::from("payload.bin"),
                caller_object_id: "direct-encrypted-capacity-caller".into(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Encrypted {
                    recipients: vec![test_recipient(0x71, 0), test_recipient(0x72, 1)],
                },
            },
            selected,
            &checkpoint_dir,
            &parity_journal_path,
            &resources,
        )
        .expect_err("configured spool ceiling must reject the encrypted Object");
        assert!(
            matches!(
                error,
                PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                    cause: CapacityReserveCause::ParitySpoolCapacity,
                    remaining_spool_bytes: Some(1),
                    ..
                })
            ),
            "{error}"
        );
        assert_eq!(
            sink.blocks.len(),
            1,
            "only the identity bootstrap is written"
        );
        assert_eq!(sink.filemarks, vec![1]);
        assert!(
            state
                .get_native_object_by_caller_object_id("direct-encrypted-capacity-caller")
                .expect("query caller object")
                .is_none(),
            "capacity rejection must not publish the Object"
        );
        assert!(
            remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
                .expect("open checkpoint journal")
                .replay()
                .expect("replay checkpoint journal")
                .is_empty(),
            "capacity rejection must not checkpoint the Object"
        );
    }

    #[test]
    fn direct_checkpointed_parity_raw_write_failure_persists_fence_and_blocks_retry() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-direct-parity-raw-fence-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "capacity.direct-fence";
        let tape_uuid = [0x72; 16];
        let block_size = 256 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("direct-parity-fence-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        state
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP006L9".into(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision mocked LTO-9");
        state
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let pool_cfg = TapePoolConfig {
            id: pool_id.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let selected = select_tape_in_pool(&state, &pool_cfg, 16 * 1024, &HashSet::new())
            .expect("fresh parity tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, vec![0xA6; 16 * 1024]).expect("write payload");
        let checkpoint_dir = temp.path().join("checkpoints");
        let parity_journal_path = temp.path().join("parity.remjournal");
        let mut sink = FailOnBlockWriteSink::new(2);

        let error = write_to_selected_tape_checkpointed(
            &mut state,
            &mut sink,
            &pool_cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.into(),
                source: WriteObjectSource::Path(payload_path),
                archive_path: PathBuf::from("payload.bin"),
                caller_object_id: "direct-parity-fence-caller".into(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected.clone(),
            &checkpoint_dir,
            &parity_journal_path,
            &test_pool_write_resources(),
        )
        .expect_err("completion-unknown Object command must fail and fence");
        assert!(error
            .to_string()
            .contains("injected parity raw write failure"));
        assert_eq!(
            sink.inner.blocks.len(),
            1,
            "identity bootstrap completed first"
        );
        assert_eq!(sink.inner.filemarks, vec![1]);
        assert!(
            remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
                .expect("open checkpoint journal")
                .replay()
                .expect("replay checkpoint journal")
                .is_empty(),
            "failed Object is not checkpointed"
        );
        let fences = state
            .tape_io_admission_conflicts(&tape_uuid, Some("CAP006L9"))
            .expect("query persisted fence");
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].reason, "parity_append");
        let retry_error = ensure_selected_tape_accepts_session_write(&state, &pool_cfg, &selected)
            .expect_err("persisted fence blocks retry before tape motion");
        assert!(matches!(retry_error, PoolWriteError::InvalidInput(_)));
        assert!(retry_error.to_string().contains("active tape-I/O fence"));
    }

    #[test]
    fn parity_capacity_distinguishes_fresh_media_limit_from_current_tape_shortfall() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-capacity-boundary-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x6f; 16];
        let block_size = 256 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("capacity-boundary-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP003L9".into(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision mocked LTO-9");
        let selected = SelectedTape {
            pool_id: "capacity.boundary".into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::Scheme(scheme.clone()),
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.97,
            watermark_high: 0.98,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let mut backing = VecBlockSink::new();
        let mut raw = BlockSinkRawTapeSink::new(&mut backing);
        let mut journal = PerObjectTestJournal {
            tape_uuid,
            bundles: Vec::new(),
        };
        let mut parity = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            scheme.clone(),
            tape_uuid,
            block_size,
        )
        .expect("parity sink");
        parity.write_bootstrap().expect("initial bootstrap");
        let fresh_runtime = parity
            .terminal_triple_capacity_runtime_state()
            .expect("fresh runtime state");
        assert_eq!(
            fresh_runtime.used_tape_blocks,
            PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS
        );

        let baseline_memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024)
            .expect("baseline spool manager");
        let capacity_blocks = 1_000_000;
        let baseline = reserve_parity_object_capacity(
            fresh_runtime,
            &scheme,
            &selected,
            (&pool_cfg, 1, 0),
            capacity_blocks,
            2,
            &baseline_memory,
        )
        .expect("fresh media admits the object under a valid exact profile");
        let required = baseline.report().required_tape_blocks;
        drop(baseline);

        let oversized_memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024)
            .expect("oversized spool manager");
        let error = reserve_parity_object_capacity(
            fresh_runtime,
            &scheme,
            &selected,
            (&pool_cfg, 1, 0),
            capacity_blocks,
            capacity_blocks,
            &oversized_memory,
        )
        .expect_err("the Object is impossible on every fresh replacement tape");
        assert!(
            matches!(
                error,
                PoolWriteError::Parity(ParityError::ObjectTooLargeForEmptyTape { .. })
            ),
            "{error}"
        );

        let current_runtime = TerminalTripleCapacityRuntimeState {
            used_tape_blocks: capacity_blocks - required + 1,
            ..fresh_runtime
        };
        let current_memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024)
            .expect("current-tape spool manager");
        let error = reserve_parity_object_capacity(
            current_runtime,
            &scheme,
            &selected,
            (&pool_cfg, 1, 0),
            capacity_blocks,
            2,
            &current_memory,
        )
        .expect_err("the same Object no longer fits the current nonempty tape");
        assert!(
            matches!(error, PoolWriteError::TerminalCloseRequired { .. }),
            "{error}"
        );
    }

    #[test]
    fn batched_parity_post_motion_projection_failure_sets_dirty_after_retryable_spool_rejection() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-session-retention-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "capacity.session";
        let tape_uuid = [0x70; 16];
        let block_size = 256 * 1024;
        let scheme = ParityScheme {
            id: SchemeId::new_static("capacity-session-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "CAP004L9".into(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision mocked LTO-9");
        let pool_cfg = TapePoolConfig {
            id: pool_id.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let selected = SelectedTape {
            pool_id: pool_id.into(),
            tape_uuid,
            block_size,
            parity_config: ParityConfig::Scheme(scheme.clone()),
        };
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(
            &payload_path,
            b"session retention after no-motion rejection",
        )
        .expect("write payload");
        let request = || WriteObjectToPoolRequest {
            pool_id: pool_id.into(),
            source: WriteObjectSource::Path(payload_path.clone()),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "capacity-session-caller".into(),
            expected_content_sha256: None,
            representation: PoolWriteRepresentation::Plaintext,
        };

        let mut backing = VecBlockSink::new();
        let mut raw = BlockSinkRawTapeSink::new(&mut backing);
        let mut journal = PerObjectTestJournal {
            tape_uuid,
            bundles: Vec::new(),
        };
        let mut parity =
            ParitySink::new_with_journal(&mut raw, &mut journal, scheme, tape_uuid, block_size)
                .expect("parity sink");
        parity.write_bootstrap().expect("initial bootstrap");
        let position_before = parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state")
            .used_tape_blocks;
        let mut session_state = Some(parity.into_session_state().expect("detach session"));

        let short_memory =
            crate::io_memory::IoMemoryReservation::new(1).expect("one-byte shared spool budget");
        let mut raw_write_attempted = false;
        let error = write_batched_parity_to_selected_tape_after_replay_check(
            &state,
            &mut raw,
            &mut journal,
            &mut session_state,
            &pool_cfg,
            request(),
            selected.clone(),
            &short_memory,
            &mut raw_write_attempted,
        )
        .expect_err("atomic spool shortfall must reject before Object motion");
        assert!(
            matches!(
                error,
                PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                    cause: CapacityReserveCause::ParitySpoolCapacity,
                    ..
                })
            ),
            "{error}"
        );
        assert!(!raw_write_attempted);
        assert!(
            session_state.is_some(),
            "no-motion rejection retains the session"
        );
        assert_eq!(
            raw.position().expect("position after rejection").lba,
            position_before
        );

        let ample_memory = crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024)
            .expect("ample shared spool budget");
        let result = write_batched_parity_to_selected_tape_after_replay_check(
            &state,
            &mut raw,
            &mut journal,
            &mut session_state,
            &pool_cfg,
            request(),
            selected.clone(),
            &ample_memory,
            &mut raw_write_attempted,
        )
        .expect("the same retained session accepts a later retry");
        assert!(!result.is_replay());
        assert!(raw_write_attempted);
        assert!(session_state.is_some());

        raw_write_attempted = false;
        FAIL_PARITY_POST_WRITE_PROJECTION.with(|flag| flag.set(true));
        let error = write_batched_parity_to_selected_tape_after_replay_check(
            &state,
            &mut raw,
            &mut journal,
            &mut session_state,
            &pool_cfg,
            request(),
            selected,
            &ample_memory,
            &mut raw_write_attempted,
        )
        .expect_err("post-motion projection failure is injected");
        assert!(
            error
                .to_string()
                .contains("injected post-write parity projection failure"),
            "{error}"
        );
        assert!(
            raw_write_attempted,
            "the caller must see raw motion before projection failure"
        );
        assert!(
            session_state.is_none(),
            "post-motion failure consumes the uncertain session for fencing"
        );
    }

    fn capacity_tape_record() -> TapeRecord {
        TapeRecord {
            tape_uuid: vec![0x71; 16],
            voltag: Some("CAP005L9".into()),
            kind: "data".into(),
            pool_id: Some("capacity.projection".into()),
            assignment_generation: 0,
            body_format: None,
            block_size: Some(4096),
            scheme_id: None,
            data_blocks_per_stripe: None,
            parity_blocks_per_stripe: None,
            stripes_per_neighborhood: None,
            last_committed_tape_file: None,
            total_committed_ordinals: 0,
            written_extent_lba: None,
            terminal_finalization: None,
            state: "ready".into(),
            updated_at_utc: "2026-08-09T00:00:00Z".into(),
        }
    }

    #[test]
    fn physical_extent_projection_prefers_authority_and_fails_closed_on_legacy_overflow() {
        let authoritative = TapeRecord {
            written_extent_lba: Some(73),
            last_committed_tape_file: Some(u64::MAX),
            total_committed_ordinals: u64::MAX,
            ..capacity_tape_record()
        };
        assert_eq!(tape_physical_used_blocks(&authoritative).unwrap(), 73);

        let empty = capacity_tape_record();
        assert_eq!(tape_physical_used_blocks(&empty).unwrap(), 0);

        let ordinal_only = TapeRecord {
            total_committed_ordinals: 9,
            ..capacity_tape_record()
        };
        assert_eq!(tape_physical_used_blocks(&ordinal_only).unwrap(), 20);

        let with_file_count = TapeRecord {
            total_committed_ordinals: 9,
            last_committed_tape_file: Some(3),
            ..capacity_tape_record()
        };
        assert_eq!(tape_physical_used_blocks(&with_file_count).unwrap(), 17);

        let ordinal_overflow = TapeRecord {
            total_committed_ordinals: u64::MAX,
            ..capacity_tape_record()
        };
        assert!(tape_physical_used_blocks(&ordinal_overflow).is_err());

        let file_count_overflow = TapeRecord {
            last_committed_tape_file: Some(u64::MAX),
            ..capacity_tape_record()
        };
        assert!(tape_physical_used_blocks(&file_count_overflow).is_err());
    }

    #[test]
    fn selector_fit_uses_barrier_proved_physical_extent_at_the_boundary() {
        let cfg = TapePoolConfig {
            id: "capacity.projection".into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let capacity = raw_capacity_bytes(LtoGen::Lto9);
        let usable = watermark_floor_bytes(capacity, cfg.watermark_high).expect("usable bytes");
        let boundary_lba = usable / cfg.block_size_bytes;
        let full = TapeRecord {
            written_extent_lba: Some(boundary_lba),
            ..capacity_tape_record()
        };
        let fitting = TapeRecord {
            tape_uuid: vec![0x72; 16],
            written_extent_lba: Some(boundary_lba - 1),
            ..capacity_tape_record()
        };
        let full_fit = tape_fit_state_from_record(&full, &cfg, &cfg.id, 0).expect("full fit");
        let fitting_fit =
            tape_fit_state_from_record(&fitting, &cfg, &cfg.id, 1).expect("fitting fit");
        let candidates = [full_fit, fitting_fit];
        let selection = FillOldest.select(&PoolSelectionContext {
            candidates: &candidates,
            projected_footprint: cfg.block_size_bytes,
        });
        assert_eq!(
            selection,
            Selection::UseTape {
                tape_uuid: [0x72; 16]
            }
        );
    }

    #[test]
    fn l3_ordering_filemark_waits_for_final_clean_staged_batch() {
        let mut sink = StagedTestSink::new(2);

        run_staged_transfer(&mut sink, 4, |staged| {
            staged.write_block(&[1; 4])?;
            staged.write_block(&[2; 4])?;
            staged.write_block(&[3; 4])?;
            staged.write_filemarks(1)?;
            Ok(())
        })
        .expect("staged transfer succeeds");

        assert_eq!(
            sink.events,
            vec![
                "position",
                "intent:2:12:2:1",
                "write_batch:2",
                "write_batch:1",
                "span_ok:2:12:2:1",
                "filemark:1",
            ],
            "WRITE FILEMARKS must be actor-ordered after the final clean data batch"
        );
    }

    #[test]
    fn l3_crash_after_producer_read_before_batch_write_leaves_no_tape_bytes() {
        let mut sink = StagedTestSink::with_ring(2, 2);

        let err = run_staged_transfer(&mut sink, 4, |staged| {
            staged.write_block(&[1; 4])?;
            Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
                "kill after producer read before batch write".to_string(),
            ))
        })
        .expect_err("source-side kill must fail transfer");

        assert!(err.to_string().contains("producer read"));
        assert!(
            sink.inner.blocks.is_empty(),
            "pending process-local buffer must not reach tape after source-side kill"
        );
        assert!(
            !sink
                .events
                .iter()
                .any(|event| event.starts_with("filemark")),
            "source-side failure must not emit filemark: {:?}",
            sink.events
        );
    }

    #[test]
    fn l3_source_error_discards_unsubmitted_window_without_filemark() {
        let mut sink = StagedTestSink::new(2);

        let err = run_staged_transfer(&mut sink, 4, |staged| {
            for value in 0..4u8 {
                staged.write_block(&[value; 4])?;
            }
            Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
                "injected source error after first batch".to_string(),
            ))
        })
        .expect_err("source-side error must fail transfer");

        assert!(err.to_string().contains("source error"));
        assert_eq!(
            sink.events,
            vec!["position"],
            "an unsubmitted partial ring window is discarded after producer failure"
        );
    }

    #[test]
    fn l3_sink_error_with_queued_buffers_drains_and_poisons_filemark() {
        let mut sink = StagedTestSink::failing_on_batch(2, 2);

        let err = run_staged_transfer(&mut sink, 4, |staged| {
            for value in 0..5u8 {
                staged.write_block(&[value; 4])?;
            }
            staged.write_filemarks(1)?;
            Ok(())
        })
        .expect_err("sink failure must fail transfer");

        assert!(err.to_string().contains("injected sink failure"));
        assert_eq!(
            sink.events,
            vec![
                "position",
                "intent:3:20:2:1",
                "write_batch:2",
                "write_batch:2",
                "span_error",
            ],
            "queued producer buffers are drained after poison, but no filemark reaches the sink"
        );
    }

    fn fence_test_fixture() -> (tempfile::TempDir, CatalogIndex, SelectedTape) {
        let temp = tempfile::Builder::new()
            .prefix("remanence-transfer-fence-")
            .tempdir()
            .expect("tempdir");
        let mut state =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let tape_uuid = [0x5a; 16];
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "FENCE1L9".into(),
                block_size: 4,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        (
            temp,
            state,
            SelectedTape {
                pool_id: "fence.test".into(),
                tape_uuid,
                block_size: 4,
                parity_config: ParityConfig::None,
            },
        )
    }

    fn assert_one_transfer_fence(state: &CatalogIndex, expected_error: &str) {
        let fences = state
            .list_active_tape_io_fences()
            .expect("list active tape-I/O fences");
        assert_eq!(
            fences.len(),
            1,
            "exactly one safety funnel persists a fence"
        );
        assert!(
            fences[0]
                .evidence_json
                .as_deref()
                .is_some_and(|evidence| evidence.contains(expected_error)),
            "fence evidence must retain the transfer failure: {fences:?}"
        );
    }

    #[test]
    fn overlap_recovery_cut_matrix_never_projects_partial_object() {
        for cut in [
            "before-first-block",
            "payload",
            "finish-validation",
            "filemark",
        ] {
            let (temp, mut state, selected) = fence_test_fixture();
            let mut sink = StagedTestSink::new(2);
            if cut == "filemark" {
                sink.fail_filemark = true;
            }
            let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
            let manager =
                crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("memory manager");
            let (_producer, _consumer, control) =
                crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
                    .expect("ring");
            let cut_control = Arc::clone(&control);
            let mut counted = CountingBlockSink::new(&mut sink, selected.block_size);
            let result = run_counted_fenced_staged_transfer(
                &mut state,
                &selected,
                &mut counted,
                4,
                Some(control),
                |staged| match cut {
                    "before-first-block" => Err(PoolWriteError::InvalidInput(
                        "injected disconnect before first block".into(),
                    )),
                    "payload" => {
                        cut_control.mark_tape_started();
                        staged.write_block(&[0x11; 4])?;
                        Err(PoolWriteError::InvalidInput(
                            "injected disconnect during payload".into(),
                        ))
                    }
                    "finish-validation" => {
                        cut_control.mark_tape_started();
                        staged.write_block(&[0x22; 4])?;
                        Err(PoolWriteError::InvalidInput(
                            "injected Finish digest disagreement".into(),
                        ))
                    }
                    "filemark" => {
                        cut_control.mark_tape_started();
                        staged.write_block(&[0x33; 4])?;
                        staged.write_filemarks(1)?;
                        Ok(())
                    }
                    other => panic!("unhandled cut {other}"),
                },
            );
            assert!(result.is_err(), "cut {cut} must fail closed");
            assert!(
                state
                    .list_native_objects()
                    .expect("list native objects")
                    .is_empty(),
                "cut {cut} must not project an object"
            );
            let fences = state
                .list_active_tape_io_fences()
                .expect("list tape-I/O fences");
            if cut == "before-first-block" {
                assert!(
                    fences.is_empty(),
                    "pre-write failure has no uncertain tape tail"
                );
            } else {
                assert_eq!(fences.len(), 1, "cut {cut} must fence the tape");
            }

            let retry_pool = "fence.retry";
            let retry_tape = [0x6d; 16];
            state
                .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                    pool_id: retry_pool.into(),
                    display_name: None,
                    copy_class: None,
                    content_class: None,
                    created_at_utc: None,
                })
                .expect("project retry pool");
            state
                .provision_tape(remanence_state::ProvisionTapeInput {
                    tape_uuid: retry_tape,
                    voltag: "RETRY1L9".into(),
                    block_size: 4096,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision clean retry tape");
            state
                .project_tape_pool_membership(retry_tape, retry_pool)
                .expect("assign clean retry tape");
            let retry_cfg = TapePoolConfig {
                id: retry_pool.into(),
                display_name: None,
                copy_class: None,
                content_class: None,
                selection_policy: Default::default(),
                watermark_low: 0.0001,
                watermark_high: 1.0,
                capacity_cap_bytes: None,
                block_size_bytes: 4096,
                min_object_size_bytes: 0,
            };
            let retry_payload = format!("replay from zero after {cut}").into_bytes();
            let retry_digest: [u8; 32] = Sha256::digest(&retry_payload).into();
            let retry_path = temp.path().join("retry.bin");
            fs::write(&retry_path, &retry_payload).expect("write retained caller source");
            let retry_request = || WriteObjectToPoolRequest {
                pool_id: retry_pool.into(),
                source: WriteObjectSource::Path(retry_path.clone()),
                archive_path: "retry.bin".into(),
                caller_object_id: format!("retry-after-{cut}"),
                expected_content_sha256: Some(retry_digest),
                representation: PoolWriteRepresentation::Plaintext,
            };
            let mut retry_sink = VecBlockSink::new();
            let landed =
                write_object_to_pool(&mut state, &mut retry_sink, &retry_cfg, retry_request())
                    .expect("re-send from byte zero lands on a clean tape");
            assert!(!landed.is_replay());
            let blocks_after_landing = retry_sink.blocks.len();
            let replayed =
                write_object_to_pool(&mut state, &mut retry_sink, &retry_cfg, retry_request())
                    .expect("same caller id and digest replays idempotently");
            assert!(replayed.is_replay());
            assert_eq!(retry_sink.blocks.len(), blocks_after_landing);
        }
    }

    #[test]
    fn overlap_staged_only_failure_does_not_raise_a_false_tape_fence() {
        let (_temp, mut state, selected) = fence_test_fixture();
        let mut sink = StagedTestSink::new(2);
        let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
        let (_producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
                .expect("ring");
        let mut counted = CountingBlockSink::new(&mut sink, selected.block_size);
        let error = run_counted_fenced_staged_transfer(
            &mut state,
            &selected,
            &mut counted,
            4,
            Some(Arc::clone(&control)),
            |staged| {
                staged.write_block(&[0x91; 4])?;
                Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
                    "source failed while first block was still process-local".into(),
                ))
            },
        )
        .expect_err("source failure aborts staged transfer");
        assert!(error.to_string().contains("process-local"), "{error}");
        assert!(!control.tape_started());
        assert!(sink.inner.blocks.is_empty());
        assert!(
            state
                .list_active_tape_io_fences()
                .expect("list fences")
                .is_empty(),
            "no physical WRITE attempt means there is no uncertain tape tail"
        );
    }

    #[test]
    fn producer_error_after_committed_window_records_tape_io_fence() {
        let (_temp, mut state, selected) = fence_test_fixture();
        let mut sink = StagedTestSink::with_ring(2, 2);
        let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
            for value in 0..4u8 {
                staged.write_block(&[value; 4])?;
            }
            staged.position()?;
            Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
                "producer source read failed after committed window".into(),
            ))
        })
        .expect_err("producer failure stops transfer");
        assert!(error.to_string().contains("producer source read failed"));
        assert_eq!(sink.batch_calls, 2, "one full ring window reached tape");
        assert_one_transfer_fence(&state, "producer source read failed");
    }

    #[test]
    fn space_to_eod_error_records_tape_io_fence() {
        let (_temp, mut state, selected) = fence_test_fixture();
        let mut sink = StagedTestSink::new(2);
        sink.fail_space_to_eod = true;
        let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
            staged.space_to_end_of_data().map_err(PoolWriteError::from)
        })
        .expect_err("SPACE(EOD) failure stops transfer");
        assert!(error.to_string().contains("space-to-EOD failure"));
        assert_one_transfer_fence(&state, "space-to-EOD failure");
    }

    #[test]
    fn position_error_records_tape_io_fence() {
        let (_temp, mut state, selected) = fence_test_fixture();
        let mut sink = StagedTestSink::new(2);
        sink.fail_position = true;
        let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
            staged.write_block(&[1; 4]).map_err(PoolWriteError::from)
        })
        .expect_err("READ POSITION failure stops transfer");
        assert!(error.to_string().contains("READ POSITION failure"));
        assert_one_transfer_fence(&state, "READ POSITION failure");
    }

    #[test]
    fn disconnected_free_ring_cannot_mask_inflight_tape_failure() {
        let mut sink = StagedTestSink::with_ring(2, 2);
        sink.batch_error = Some(TapeIoError::PartialBatchUncommittable {
            requested_records: 2,
            written_records: 1,
            requested_bytes: 8,
            written_bytes: 4,
            end_of_medium: false,
            sense: Some(vec![0x70, 0, 0x40]),
        });
        let ordered = Arc::clone(&sink.ordered_events);
        let accounting = Arc::new(RingAccounting::default());
        let mut buffer = PageAlignedBuffer::try_new(8, accounting).expect("test ring buffer");
        buffer.append(&[1; 8]).expect("fill test ring buffer");
        let mut window = PipelinedWindow::new();
        window
            .push(PipelinedBatch {
                buffer,
                cdb: remanence_scsi::read_write::build_write_fixed_cdb(2),
                records: 2,
                block_size_bytes: 4,
            })
            .expect("one in-flight batch");
        let (free_tx, free_rx) = std_mpsc::sync_channel(2);
        drop(free_rx); // producer-side staged sink disappeared mid-window
        let error = execute_pipelined_window(&mut sink, window, &free_tx, None, &mut |_error| {
            ordered.lock().expect("ordered events").push("fence".into());
            Ok(())
        })
        .expect_err("in-flight tape WRITE fails after producer drops receiver");
        let message = error.to_string();
        assert!(
            message.contains("partial fixed batch uncommittable"),
            "{message}"
        );
        assert!(message.contains("staging buffer return"), "{message}");
        assert!(
            sink.audited_partial_sense,
            "deferred WRITE sense must be audited"
        );
        let ordered = sink.ordered_events.lock().expect("ordered events");
        assert_eq!(&ordered[..3], ["classify", "fence", "audit"]);
    }

    #[test]
    fn filemark_fence_failure_still_flushes_deferred_audit_and_reports_both() {
        let mut sink = StagedTestSink::new(2);
        sink.fail_filemark = true;
        let ordered = Arc::clone(&sink.ordered_events);
        let error = run_staged_transfer_with_safety(
            &mut sink,
            4,
            |staged| staged.write_filemarks(1).map_err(PoolWriteError::from),
            |_error| {
                ordered.lock().expect("ordered events").push("fence".into());
                Err(PoolWriteError::InvalidInput(
                    "injected fence callback failure".into(),
                ))
            },
        )
        .expect_err("filemark and fence both fail");
        let message = error.to_string();
        assert!(message.contains("WRITE FILEMARKS failure"), "{message}");
        assert!(message.contains("fence callback failure"), "{message}");
        assert_eq!(
            sink.ordered_events
                .lock()
                .expect("ordered events")
                .as_slice(),
            ["fence", "audit"]
        );
    }

    #[test]
    fn pipelined_ring_rebuilds_trailing_cdb_and_uses_page_aligned_buffers() {
        let mut sink = StagedTestSink::with_ring(2, 4);

        run_staged_transfer(&mut sink, 4, |staged| {
            for value in 0..5u8 {
                staged.write_block(&[value; 4])?;
            }
            Ok(())
        })
        .expect("pipelined transfer succeeds");

        assert_eq!(sink.diagnostic_publications, 1);

        assert_eq!(
            sink.cdbs,
            vec![
                remanence_scsi::read_write::build_write_fixed_cdb(2).to_vec(),
                remanence_scsi::read_write::build_write_fixed_cdb(2).to_vec(),
                remanence_scsi::read_write::build_write_fixed_cdb(1).to_vec(),
            ],
            "the trailing partial buffer must rebuild TRANSFER LENGTH"
        );
        assert!(
            sink.alignments.iter().all(|alignment| *alignment == 0),
            "all submitted payload slices must be page aligned: {:?}",
            sink.alignments
        );
        assert!(sink.events.contains(&"intent:3:20:2:1".to_string()));
        assert!(sink.events.contains(&"span_ok:3:20:2:1".to_string()));
    }

    #[test]
    fn hot_phase_histogram_reports_exact_mean_and_bucketed_tail() {
        let mut histogram = HotPhaseHistogram::default();
        histogram.record(Duration::from_micros(11));
        histogram.record(Duration::from_micros(39));

        assert_eq!(histogram.mean(), 25);
        assert_eq!(histogram.percentile(50, 100), 25);
        assert_eq!(histogram.percentile(95, 100), 50);
        assert_eq!(histogram.max_us, 39);
    }

    #[test]
    fn transfer_stats_include_staging_wait_and_refill_histograms() {
        let mut diagnostics = StagingPhaseDiagnostics::default();
        diagnostics.wait_us.record(Duration::from_micros(11));
        diagnostics.wait_us.record(Duration::from_micros(39));
        diagnostics.refill_us.record(Duration::from_micros(101));

        let mut stats = BlockSinkStats::default();
        stats.record_staging(&diagnostics);

        assert_eq!(stats.staging_wait_samples, 2);
        assert_eq!(stats.staging_wait_mean_us, 25);
        assert_eq!(stats.staging_wait_p95_us, 50);
        assert_eq!(stats.refill_samples, 1);
        assert_eq!(stats.refill_mean_us, 101);
    }

    #[test]
    fn pipelined_synchronous_batch_propagates_successful_early_warning() {
        let mut sink = StagedTestSink::with_ring(2, 4);
        sink.early_warning_on_batch_call = Some(1);

        let outcome = run_staged_transfer(&mut sink, 4, |staged| {
            staged
                .write_block_batch(&[7; 8], 4)
                .map_err(PoolWriteError::from)
        })
        .expect("full-record EW remains successful");

        assert_eq!(outcome.records_written, 2);
        assert_eq!(outcome.bytes_written, 8);
        assert!(outcome.early_warning);
        assert!(!outcome.end_of_medium);
    }

    #[test]
    fn pipelined_terminal_poison_fences_before_audit_and_discards_queued_batches() {
        let mut sink = StagedTestSink::with_ring(2, 4);
        sink.fail_on_batch_call = Some(2);
        let ordered = Arc::clone(&sink.ordered_events);

        let err = run_staged_transfer_with_safety(
            &mut sink,
            4,
            |staged| {
                for value in 0..8u8 {
                    staged.write_block(&[value; 4])?;
                }
                staged.write_filemarks(1)?;
                Ok(())
            },
            |error| {
                assert!(error.to_string().contains("injected sink failure"));
                ordered.lock().expect("ordered events").push("fence".into());
                Ok(())
            },
        )
        .expect_err("second hot submission fails");

        assert!(err.to_string().contains("injected sink failure"));
        assert_eq!(
            sink.batch_calls, 2,
            "queued batches must not issue after poison"
        );
        assert!(!sink
            .events
            .iter()
            .any(|event| event.starts_with("filemark")));
        let ordered = sink.ordered_events.lock().expect("ordered events");
        let fence = ordered.iter().position(|event| event == "fence").unwrap();
        let audit = ordered.iter().position(|event| event == "audit").unwrap();
        assert!(
            fence < audit,
            "safety persistence must precede audit: {ordered:?}"
        );
    }

    #[test]
    fn pipelined_diagnostics_reset_for_each_staged_transfer() {
        let mut sink = StagedTestSink::with_ring(2, 4);

        run_staged_transfer(&mut sink, 4, |staged| {
            for value in 0..4u8 {
                staged.write_block(&[value; 4])?;
            }
            Ok(())
        })
        .expect("first transfer succeeds");
        assert_eq!(sink.pipelined_write_diagnostics().ioctl_samples, 2);
        assert_eq!(sink.pipelined_write_diagnostics().ioctl_max_us, 2_000);

        run_staged_transfer(&mut sink, 4, |staged| {
            staged.write_block(&[9; 4])?;
            Ok(())
        })
        .expect("second transfer succeeds");

        assert_eq!(sink.diagnostic_resets, 2);
        assert_eq!(sink.diagnostic_publications, 2);
        assert_eq!(sink.pipelined_write_diagnostics().ioctl_samples, 1);
        assert_eq!(
            sink.pipelined_write_diagnostics().ioctl_max_us,
            1_000,
            "the prior transfer maximum must not survive"
        );
    }

    #[test]
    fn pipelined_ring_rejects_invalid_runtime_depths_and_checked_size_overflow() {
        for ring_buffers in [0, 1, 17] {
            let mut sink = StagedTestSink::with_ring(2, ring_buffers);
            let err = run_staged_transfer(&mut sink, 4, |_staged| Ok::<_, PoolWriteError>(()))
                .expect_err("invalid ring depth rejects before spawning");
            assert!(err.to_string().contains("staging ring depth"), "{err}");
        }

        let mut sink = StagedTestSink::with_ring(u32::MAX, 16);
        let err = run_staged_transfer(&mut sink, usize::MAX, |_staged| Ok::<_, PoolWriteError>(()))
            .expect_err("ring byte multiplication must be checked");
        assert!(
            err.to_string().contains("staging batch bytes overflow"),
            "{err}"
        );
    }

    #[test]
    fn block_sink_stats_latches_hardware_early_warning() {
        let mut stats = BlockSinkStats::default();
        stats.record_block(256 * 1024, true);
        assert!(stats.early_warning);

        let mut stats = BlockSinkStats::default();
        stats.record_filemarks(1, true, Duration::from_millis(7));
        assert!(stats.early_warning);
        assert_eq!(stats.filemark_write_drain, Duration::from_millis(7));

        let mut stats = BlockSinkStats::default();
        stats.record_position(tape_position_with_warning(true));
        assert!(stats.early_warning);
    }

    #[test]
    fn write_failure_with_position_secondary_keeps_partial_batch_fence_reason() {
        let error = TapeIoError::WriteFailureWithPositionError {
            write_error: Box::new(TapeIoError::PartialBatchUncommittable {
                requested_records: 4,
                written_records: 2,
                requested_bytes: 16,
                written_bytes: 8,
                end_of_medium: true,
                sense: Some(vec![0x70, 0, 0x40]),
            }),
            position_error: Box::new(TapeIoError::OperationFailed(
                "injected arbitration READ POSITION failure".into(),
            )),
        };
        let message = error.to_string();
        assert_eq!(
            tape_io_fence_reason_for_transfer_error(&message),
            "partial_batch"
        );
        assert!(message.contains("arbitration READ POSITION failure"));
    }

    #[test]
    fn live_write_counter_advances_during_transfer() {
        let counter = Arc::new(crate::DriveByteCounters::new(0));
        let mut sink = VecBlockSink::new();
        let mut live_sink = LiveCounterBlockSink::new(&mut sink, Arc::clone(&counter), 4);

        let first = live_sink.write_block(b"abc").expect("first write");
        assert_eq!(first.bytes_written, 3);
        assert_eq!(counter.write_bytes(), 3);
        assert!(counter.write_bytes() > 0);
        assert!(counter.write_bytes() < 8);

        live_sink.write_filemarks(1).expect("filemark write");
        assert_eq!(counter.write_bytes(), 3);

        let second = live_sink.write_block(b"defgh").expect("second write");
        assert_eq!(second.bytes_written, 5);
        assert_eq!(counter.write_bytes(), 8);
    }

    #[test]
    fn pool_write_record_to_proto_carries_append_commit_info() {
        let object = PoolWriteObjectRecord {
            object_id: [0x11; 16],
            caller_object_id: "caller-object".to_string(),
            content_sha256: [0x22; 32],
            logical_size_bytes: 123,
            body_format: FORMAT_ID.to_string(),
            created_at_utc: "2026-07-05T00:00:00Z".to_string(),
            copies: vec![PoolWriteObjectCopyRecord {
                tape_uuid: [0x44; 16],
                tape_file_number: 3,
                first_body_lba: 9,
                pool_id: "camera.copy-a".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some([0x33; 32]),
                stored_digest: Some([0x44; 32]),
            }],
        };

        let proto = object.to_proto();
        let info = proto
            .append_commit_info
            .expect("append commit info from first copy");
        assert_eq!(info.append_mode, pb::AppendMode::Append as i32);
        assert_eq!(info.tape_uuid, vec![0x44; 16]);
        assert_eq!(info.tape_file_number, Some(3));
        assert_eq!(info.first_body_lba, 9);
        assert_eq!(info.position_before_lba, None);
        assert_eq!(info.position_after_lba, None);
        assert_eq!(info.journal_record_ordinal, None);
        assert_eq!(
            proto.copies[0]
                .plaintext_digest
                .as_ref()
                .map(|digest| digest.value.as_slice()),
            Some(&[0x33; 32][..])
        );
        assert_eq!(
            proto.copies[0]
                .stored_digest
                .as_ref()
                .map(|digest| digest.algorithm.as_str()),
            Some("sha256")
        );
    }

    #[test]
    fn written_ack_never_exposes_provisional_tape_locators() {
        let object = PoolWriteObjectRecord {
            object_id: [0x11; 16],
            caller_object_id: "caller-object".to_string(),
            content_sha256: [0x22; 32],
            logical_size_bytes: 123,
            body_format: FORMAT_ID.to_string(),
            created_at_utc: "2026-07-05T00:00:00Z".to_string(),
            copies: vec![PoolWriteObjectCopyRecord {
                tape_uuid: [0x44; 16],
                tape_file_number: 37,
                first_body_lba: 9,
                pool_id: "camera.copy-a".to_string(),
                representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: Some([0x33; 32]),
                stored_digest: Some([0x44; 32]),
            }],
        };
        let batch_id = Uuid::from_bytes([0x55; 16]);

        let proto = object.to_written_proto(batch_id, 4);
        let info = proto.append_commit_info.expect("WRITTEN append info");

        assert!(proto.copies.is_empty(), "copy locators remain invisible");
        assert_eq!(info.durability, pb::AppendDurability::Written as i32);
        assert!(info.tape_uuid.is_empty());
        assert_eq!(info.tape_file_number, None);
        assert_eq!(info.first_body_lba, 0);
        assert_eq!(info.batch_id, batch_id.as_bytes());
        assert_eq!(info.provisional_ordinal, Some(4));
    }

    #[test]
    fn pool_write_record_to_proto_leaves_append_info_absent_without_copies() {
        let object = PoolWriteObjectRecord {
            object_id: [0x11; 16],
            caller_object_id: "caller-object".to_string(),
            content_sha256: [0x22; 32],
            logical_size_bytes: 123,
            body_format: FORMAT_ID.to_string(),
            created_at_utc: "2026-07-05T00:00:00Z".to_string(),
            copies: Vec::new(),
        };

        let proto = object.to_proto();
        assert!(proto.copies.is_empty());
        assert!(proto.append_commit_info.is_none());
    }

    #[test]
    fn append_finish_does_not_double_count() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-pool-write-live-counter")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open test index");
        let pool_id = "camera.copy-a";
        let tape_uuid = [4u8; 16];
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: Some("copy-a".to_string()),
                content_class: Some("camera".to_string()),
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN001L1".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("project tape membership");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            selection_policy: Default::default(),
            watermark_low: 0.0001,
            watermark_high: 1.0,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let selected = select_tape_in_pool(&index, &cfg, 6, &HashSet::new()).expect("select tape");

        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"abcdef").expect("write payload");
        let request = WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path.clone()),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "caller-object".to_string(),
            expected_content_sha256: None,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let counter = Arc::new(crate::DriveByteCounters::new(0));
        let mut sink = VecBlockSink::new();
        let result = write_to_selected_tape_with_live_counter(
            &mut index,
            &mut sink,
            &cfg,
            request,
            selected,
            Some(counter.clone()),
        )
        .expect("write object");

        let physical_bytes = sink
            .blocks
            .iter()
            .map(|block| block.len() as u64)
            .sum::<u64>();
        assert!(physical_bytes > 0);
        assert_eq!(counter.write_bytes(), physical_bytes);
        assert_eq!(result.object.logical_size_bytes, 6);
    }

    #[test]
    fn batch_of_one_core_journals_tape_for_later_daemon_admission() {
        const BLOCK_SIZE: u32 = 256 * 1024;
        let temp = tempfile::Builder::new()
            .prefix("remanence-batch-one-journal-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "batch.one";
        let tape_uuid = [0x8E; 16];
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "BAT001L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"payload").expect("write payload");
        let request = || WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path.clone()),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "batch-one-caller".to_string(),
            expected_content_sha256: None,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let checkpoint_dir = temp.path().join("checkpoints");
        let checkpoint_handle =
            remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
                .expect("open checkpoint authority");
        let held_lease = checkpoint_handle
            .acquire_exclusive()
            .expect("hold competing checkpoint lease");
        let mut blocked_sink = LocateCountingSink::default();
        write_to_selected_tape_checkpointed(
            &mut index,
            &mut blocked_sink,
            &cfg,
            request(),
            selected.clone(),
            &checkpoint_dir,
            &temp.path().join("unused-parity.remjournal"),
            &test_pool_write_resources(),
        )
        .expect_err("a competing checkpoint writer must reject before tape positioning");
        assert_eq!(blocked_sink.locate_calls, 0);
        drop(held_lease);

        let mut sink = VecBlockSink::new();
        let result = write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            request(),
            selected,
            &checkpoint_dir,
            &temp.path().join("unused-parity.remjournal"),
            &test_pool_write_resources(),
        )
        .expect("batch-of-one checkpoint succeeds");

        let checkpoint = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("reopen shared checkpoint journal")
            .last()
            .expect("replay checkpoint journal")
            .expect("batch-of-one record exists");
        assert_eq!(checkpoint.committed_object_count, 1);
        assert_eq!(checkpoint.objects.len(), 1);
        assert!(index
            .get_native_object(&Uuid::from_bytes(result.object.object_id).to_string())
            .expect("query projected object")
            .is_some());

        let admitted = select_tape_in_pool_for_write_session(
            &index,
            &cfg,
            7,
            &HashSet::new(),
            &checkpoint_dir,
        )
        .expect("daemon selector accepts CLI-journaled non-fresh tape");
        assert_eq!(admitted.tape_uuid, tape_uuid);
    }

    #[test]
    fn direct_watermark_seal_journals_exact_terminal_authority() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-terminal-success-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "terminal.success";
        let tape_uuid = [0x99; 16];
        let block_size = 1024 * 1024;
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "TER000L9".to_string(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.000_000_000_001,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"terminal authority payload").expect("write payload");
        let checkpoint_dir = temp.path().join("checkpoints");
        let mut sink = SparseBlockSink::default();
        let result = write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.to_string(),
                source: WriteObjectSource::Path(payload_path),
                archive_path: PathBuf::from("payload.bin"),
                caller_object_id: "terminal-success-caller".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
            &checkpoint_dir,
            &temp.path().join("unused-parity.remjournal"),
            &test_pool_write_resources(),
        )
        .expect("write, checkpoint, and terminalize tape");
        assert!(result.sealed_after_write());

        let records = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("reopen checkpoint authority")
            .replay()
            .expect("replay checkpoint authority");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].objects.len(), 1);
        assert!(!records[0].sealed_after_write);
        let terminal = &records[1];
        assert!(terminal.objects.is_empty());
        assert!(terminal.sealed_after_write);
        assert_eq!(terminal.committed_object_count, 1);
        assert_eq!(terminal.ordinal, records[0].ordinal + 1);
        assert_eq!(
            terminal.barrier_bundle.as_ref().map(|bundle| bundle.kind),
            Some(CommittedBundleKind::TerminalComponent)
        );
        let finalization = terminal
            .terminal_finalization
            .as_ref()
            .expect("structured terminal intent is final authority");
        assert_eq!(
            finalization.progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaC
        );
        assert_eq!(finalization.layout.components[4].ordinal, 3);
        assert_eq!(
            terminal.next_tape_file_number,
            finalization.layout.components[4].tape_file_number + 1
        );
        let physical_eod = sink.position().expect("read terminal position");
        assert_eq!(terminal.eod_partition, physical_eod.partition);
        assert_eq!(terminal.eod_lba, physical_eod.lba);
        let tape = index
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape exists");
        assert_eq!(tape.state, "sealed");
        assert_eq!(tape.written_extent_lba, Some(terminal.eod_lba));
    }

    #[test]
    fn direct_parity_watermark_seal_commits_prefix_and_terminal_components_before_final_c() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-terminal-success-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "parity.terminal.success";
        let tape_uuid = [0x98; 16];
        let block_size = 1024 * 1024;
        let scheme = ParityScheme {
            id: remanence_parity::SchemeId::new_static("parity-terminal-success-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "TER998L9".to_string(),
                block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision parity tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.000_000_000_001,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"parity terminal authority payload").expect("write payload");
        let checkpoint_dir = temp.path().join("checkpoints");
        let parity_journal_path = temp.path().join("parity.remjournal");
        let mut sink = SparseBlockSink::default();
        let result = write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.to_string(),
                source: WriteObjectSource::Path(payload_path),
                archive_path: PathBuf::from("payload.bin"),
                caller_object_id: "parity-terminal-success-caller".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
            &checkpoint_dir,
            &parity_journal_path,
            &test_pool_write_resources(),
        )
        .expect("write, checkpoint, and terminalize parity tape");
        assert!(result.sealed_after_write());

        let records = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("reopen checkpoint authority")
            .replay()
            .expect("replay checkpoint authority");
        assert_eq!(records.len(), 2);
        assert!(!records[0].sealed_after_write);
        let terminal = &records[1];
        assert!(terminal.sealed_after_write);
        assert!(terminal.objects.is_empty());
        assert_eq!(terminal.scheme.as_ref(), Some(&scheme));
        assert_eq!(
            terminal.barrier_bundle.as_ref().map(|bundle| bundle.kind),
            Some(CommittedBundleKind::TerminalComponent)
        );
        let finalization = terminal
            .terminal_finalization
            .as_ref()
            .expect("structured terminal intent is final authority");
        assert_eq!(
            finalization.progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaC
        );
        assert!(finalization.terminal_prefix.is_some());
        let physical_eod = sink.position().expect("read terminal position");
        assert_eq!(terminal.eod_lba, physical_eod.lba);
        let committed =
            FileTapeFileJournal::open(&parity_journal_path, tape_uuid, block_size, scheme)
                .and_then(|journal| journal.load_committed())
                .expect("replay terminal sink authority");
        assert!(committed.orphaned_bundles.is_empty());
        assert_eq!(
            committed.entries.last().map(|entry| entry.kind),
            Some(TapeFileKind::TapeIndexReplica)
        );
        let tape = index
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape exists");
        assert_eq!(tape.state, "sealed");
        assert_eq!(tape.written_extent_lba, Some(terminal.eod_lba));
    }

    #[test]
    fn direct_terminal_failure_preserves_object_checkpoint_and_structured_intent() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-terminal-authority-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "terminal.authority";
        let tape_uuid = [0x9A; 16];
        let block_size = 1024 * 1024;
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "TER001L9".to_string(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.000_000_000_001,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"terminal authority payload").expect("write payload");
        let checkpoint_dir = temp.path().join("checkpoints");
        let mut sink = RejectTerminalReplicaSink::new(tape_uuid);
        let error = write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.to_string(),
                source: WriteObjectSource::Path(payload_path),
                archive_path: PathBuf::from("payload.bin"),
                caller_object_id: "terminal-authority-caller".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
            &checkpoint_dir,
            &temp.path().join("unused-parity.remjournal"),
            &test_pool_write_resources(),
        )
        .expect_err("terminal media failure must be fatal");
        assert!(error
            .to_string()
            .contains("injected terminal replica failure"));
        assert_eq!(sink.terminal_replica_attempts, 1);

        let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("reopen checkpoint authority");
        let recovery = journal
            .replay()
            .expect_err("terminal failure must retain a structured finalization lock");
        assert!(
            recovery
                .to_string()
                .contains("pending terminal finalization intent"),
            "{recovery}"
        );
        let recovery_authority = journal
            .acquire_exclusive_for_terminal_recovery()
            .and_then(|mut lease| lease.replay_for_terminal_recovery())
            .expect("source-capable owner can recover structured authority");
        assert_eq!(recovery_authority.records.len(), 1);
        assert_eq!(recovery_authority.records[0].objects.len(), 1);
        assert_eq!(
            recovery_authority
                .finalization_intent
                .as_ref()
                .map(|intent| intent.progress),
            Some(remanence_state::TerminalFinalizationProgress::BeforeReplicaA)
        );
        let tape = index
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape exists");
        assert_eq!(tape.state, "ready");
        let fences = index
            .tape_io_admission_conflicts(&tape_uuid, Some("TER001L9"))
            .expect("query terminal fence");
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].reason, "terminal_finalization");
        assert!(
            index
                .get_native_object_by_caller_object_id("terminal-authority-caller")
                .expect("query caller object")
                .is_some(),
            "ordinary Object authority must precede structured terminal intent"
        );
    }

    #[test]
    fn direct_fresh_parity_requires_exact_bot_position_before_bootstrap_write() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-fresh-parity-bot-proof-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "parity.bot.proof";
        let tape_uuid = [0x9C; 16];
        let scheme = ParityScheme {
            id: remanence_parity::SchemeId::new_static("parity-bot-proof-test"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        };
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "BOTPRF1L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision parity tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"payload").expect("write payload");
        let checkpoint_dir = temp.path().join("checkpoints");
        let parity_journal_path = temp.path().join("parity.remjournal");
        let mut sink = MisdirectedFreshLocateSink::default();

        let error = write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.to_string(),
                source: WriteObjectSource::Path(payload_path),
                archive_path: PathBuf::from("payload.bin"),
                caller_object_id: "parity-bot-proof-caller".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
            &checkpoint_dir,
            &parity_journal_path,
            &test_pool_write_resources(),
        )
        .expect_err("misdirected BOT locate must reject");
        assert!(
            error.to_string().contains("expected partition 0 lba 0"),
            "{error}"
        );
        assert!(sink.inner.blocks.is_empty());
        assert!(sink.inner.filemarks.is_empty());
        assert!(
            remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
                .expect("open checkpoint journal")
                .replay()
                .expect("replay empty checkpoint journal")
                .is_empty()
        );
        let parity_state = FileTapeFileJournal::open(&parity_journal_path, tape_uuid, 4096, scheme)
            .and_then(|journal| journal.load_committed())
            .expect("replay empty parity journal");
        assert!(parity_state.entries.is_empty());
        assert!(parity_state.orphaned_bundles.is_empty());
        let tape = index
            .get_tape(&tape_uuid)
            .expect("query tape")
            .expect("tape exists");
        assert_eq!(tape.last_committed_tape_file, None);
        assert_eq!(tape.written_extent_lba, None);
    }

    #[test]
    fn bootstrap_only_parity_orphan_requires_physical_reconciliation_before_positioning() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-daemon-resume-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "parity.resume";
        let tape_uuid = [0x8F; 16];
        let scheme = ParityScheme {
            id: remanence_parity::SchemeId::new_static("parity-daemon-resume-test"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        };
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "PAR001L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision parity tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let checkpoint_dir = temp.path().join("checkpoints");
        let parity_journal_path = temp.path().join("parity.remjournal");

        // Model a crash after the fresh BOT was durably written and projected,
        // but before the first object checkpoint existed. The BOT bundle is
        // intentionally still orphaned in the sink journal.
        {
            let mut journal = FileTapeFileJournal::open(
                &parity_journal_path,
                tape_uuid,
                selected.block_size,
                scheme.clone(),
            )
            .expect("open fresh parity journal");
            journal
                .commit_bundle(&CommittedBundle {
                    kind: CommittedBundleKind::BotBootstrap,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 0,
                        kind: TapeFileKind::Bootstrap,
                        block_count: 1,
                        physical_start_hint: Some(0),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        object_recovery_row: None,
                    }],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                })
                .expect("journal fresh BOT bundle");
            project_fresh_parity_bootstrap_bundle(&mut index, &selected, &scheme)
                .expect("project fresh BOT before simulated crash");
            let state = journal.load_committed().expect("load orphaned BOT");
            assert!(state.entries.is_empty());
            assert_eq!(state.orphaned_bundles.len(), 1);
        }
        let bootstrap_only = index
            .get_tape(&tape_uuid)
            .expect("query bootstrap-only tape")
            .expect("bootstrap-only tape exists");
        assert_eq!(bootstrap_only.total_committed_ordinals, 0);
        assert_eq!(bootstrap_only.last_committed_tape_file, Some(0));

        let recovered = select_tape_in_pool_for_write_session(
            &index,
            &cfg,
            7,
            &HashSet::new(),
            &checkpoint_dir,
        )
        .expect("pool selector admits a parity BOT without an object checkpoint");
        let fresh_pinned =
            admit_pinned_tape_for_write_session(&index, tape_uuid, pool_id, &cfg, &checkpoint_dir)
                .expect("pinned selector admits a parity BOT without an object checkpoint");
        assert_eq!(fresh_pinned.tape_uuid, tape_uuid);

        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"payload").expect("write payload");
        let request = WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "parity-resume-caller".to_string(),
            expected_content_sha256: None,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let mut sink = LocateCountingSink::default();
        let error = write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            request,
            recovered,
            &checkpoint_dir,
            &parity_journal_path,
            &test_pool_write_resources(),
        )
        .expect_err("an orphaned BOT must require physical reconciliation");
        assert!(error.to_string().contains("physical-tail reconciliation"));
        assert_eq!(
            sink.locate_calls, 0,
            "orphaned evidence must be rejected before positioning tape"
        );

        let preserved = FileTapeFileJournal::open(
            &parity_journal_path,
            tape_uuid,
            u32::try_from(cfg.block_size_bytes).expect("test block size fits u32"),
            scheme,
        )
        .and_then(|journal| journal.load_committed())
        .expect("reopen preserved orphan evidence");
        assert!(preserved.entries.is_empty());
        assert_eq!(preserved.orphaned_bundles.len(), 1);
    }

    #[test]
    fn checkpointed_parity_tape_is_admitted_for_daemon_resume() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-daemon-resume-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        let pool_id = "parity.resume";
        let tape_uuid = [0x90; 16];
        let scheme = ParityScheme {
            id: remanence_parity::SchemeId::new_static("parity-daemon-resume-test"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        };
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "PAR002L9".to_string(),
                block_size: 256 * 1024,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision parity tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape");
        let cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: 256 * 1024,
            min_object_size_bytes: 0,
        };
        let selected =
            select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
        let checkpoint_dir = temp.path().join("checkpoints");
        let parity_journal_path = temp.path().join("parity.remjournal");

        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"payload").expect("write payload");
        let request = WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "parity-resume-caller".to_string(),
            expected_content_sha256: None,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let mut sink = LocateCountingSink::default();
        write_to_selected_tape_checkpointed(
            &mut index,
            &mut sink,
            &cfg,
            request,
            selected,
            &checkpoint_dir,
            &parity_journal_path,
            &test_pool_write_resources(),
        )
        .expect("fresh parity write reaches the first checkpoint");
        assert_eq!(sink.locate_calls, 1, "fresh write positions at BOT once");

        let admitted = select_tape_in_pool_for_write_session(
            &index,
            &cfg,
            7,
            &HashSet::new(),
            &checkpoint_dir,
        )
        .expect("daemon selector admits checkpointed parity tape");
        assert_eq!(admitted.tape_uuid, tape_uuid);

        let pinned =
            admit_pinned_tape_for_write_session(&index, tape_uuid, pool_id, &cfg, &checkpoint_dir)
                .expect("pinned selector admits checkpoint-authorized parity tape");
        assert_eq!(pinned.tape_uuid, tape_uuid);

        let committed = FileTapeFileJournal::open(
            &parity_journal_path,
            tape_uuid,
            u32::try_from(cfg.block_size_bytes).expect("test block size fits u32"),
            scheme,
        )
        .and_then(|mut journal| {
            let state = journal.load_committed()?;
            let next_file = state
                .entries
                .last()
                .expect("checkpointed journal has a tape-file prefix")
                .tape_file_number
                .checked_add(1)
                .expect("test tape-file number");
            journal.commit_bundle(&CommittedBundle {
                kind: CommittedBundleKind::Object,
                entries: vec![TapeFileEntry {
                    tape_file_number: next_file,
                    kind: TapeFileKind::Object,
                    block_count: 1,
                    physical_start_hint: Some(100),
                    object_id: Some("sink-ahead".to_string()),
                    first_parity_data_ordinal: Some(state.total_committed_ordinals),
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: state.highest_protected_ordinal,
                total_committed_ordinals: state
                    .total_committed_ordinals
                    .checked_add(1)
                    .expect("test ordinal"),
            })?;
            journal.commit_bundle(&CommittedBundle {
                kind: CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: state.highest_protected_ordinal,
                total_committed_ordinals: state
                    .total_committed_ordinals
                    .checked_add(1)
                    .expect("test ordinal"),
            })?;
            journal.load_committed()
        })
        .expect("advance only the sink journal past the shared checkpoint");
        assert!(committed.orphaned_bundles.is_empty());

        let second_payload_path = temp.path().join("second-payload.bin");
        std::fs::write(&second_payload_path, b"second payload").expect("write second payload");
        let second_request = WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(second_payload_path),
            archive_path: PathBuf::from("second-payload.bin"),
            caller_object_id: "parity-resume-second-caller".to_string(),
            expected_content_sha256: None,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let mut mismatch_sink = LocateCountingSink::default();
        let error = write_to_selected_tape_checkpointed(
            &mut index,
            &mut mismatch_sink,
            &cfg,
            second_request,
            pinned.clone(),
            &checkpoint_dir,
            &parity_journal_path,
            &test_pool_write_resources(),
        )
        .expect_err("disagreeing durable resume authorities must fail closed");
        assert!(
            error
                .to_string()
                .contains("bounded parity terminal authority mismatch"),
            "{error}"
        );
        assert_eq!(
            mismatch_sink.locate_calls, 0,
            "authority disagreement must be rejected before positioning tape"
        );

        let missing_journal_path = temp.path().join("missing-parity.remjournal");
        let mut missing_journal_sink = LocateCountingSink::default();
        let error = write_to_selected_tape_checkpointed(
            &mut index,
            &mut missing_journal_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: pool_id.to_string(),
                source: WriteObjectSource::Path(temp.path().join("second-payload.bin")),
                archive_path: PathBuf::from("missing-journal-payload.bin"),
                caller_object_id: "parity-resume-missing-journal".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            pinned,
            &checkpoint_dir,
            &missing_journal_path,
            &test_pool_write_resources(),
        )
        .expect_err("a missing sink journal must not turn a checkpointed tape into fresh media");
        assert!(
            error
                .to_string()
                .contains("bounded parity terminal authority mismatch"),
            "{error}"
        );
        assert_eq!(
            missing_journal_sink.locate_calls, 0,
            "a missing sink authority must be rejected before append positioning"
        );
    }

    #[test]
    fn lto_generation_parses_m8_type_m_suffix() {
        assert_eq!(lto_generation_from_voltag("RMN001M8"), Some(LtoGen::M8));
        assert_eq!(lto_generation_from_voltag("rmn001m8"), Some(LtoGen::M8));
        assert_eq!(raw_capacity_bytes(LtoGen::M8), 9_000_000_000_000);
    }

    #[test]
    fn lto_generation_treats_lz_as_lto9_media_class() {
        assert_eq!(lto_generation_from_voltag("RMN001LZ"), Some(LtoGen::Lto9));
        assert_eq!(lto_generation_from_voltag("rmn001lz"), Some(LtoGen::Lto9));
        assert_eq!(raw_capacity_bytes(LtoGen::Lto9), 18_000_000_000_000);
    }

    #[test]
    fn lto_generation_rejects_non_ascii_without_panic() {
        assert_eq!(lto_generation_from_voltag("éX"), None);
    }

    #[test]
    fn lto_drive_generation_parses_common_inquiry_products() {
        assert_eq!(
            lto_generation_from_drive_product("Ultrium 9-SCSI"),
            Some(LtoGen::Lto9)
        );
        assert_eq!(
            lto_generation_from_drive_product("LTO-8 HH"),
            Some(LtoGen::Lto8)
        );
        assert_eq!(lto_generation_from_drive_product("unknown"), None);
    }

    #[test]
    fn lto_read_compatibility_uses_design_table() {
        let cases = [
            (
                LtoGen::Lto5,
                &[LtoGen::Lto5, LtoGen::Lto4, LtoGen::Lto3][..],
            ),
            (
                LtoGen::Lto6,
                &[LtoGen::Lto6, LtoGen::Lto5, LtoGen::Lto4][..],
            ),
            (
                LtoGen::Lto7,
                &[LtoGen::Lto7, LtoGen::Lto6, LtoGen::Lto5][..],
            ),
            (LtoGen::Lto8, &[LtoGen::Lto8, LtoGen::Lto7, LtoGen::M8][..]),
            (LtoGen::Lto9, &[LtoGen::Lto9, LtoGen::Lto8][..]),
        ];
        let all_tapes = [
            LtoGen::Lto1,
            LtoGen::Lto2,
            LtoGen::Lto3,
            LtoGen::Lto4,
            LtoGen::Lto5,
            LtoGen::Lto6,
            LtoGen::Lto7,
            LtoGen::M8,
            LtoGen::Lto8,
            LtoGen::Lto9,
        ];

        for (drive, readable) in cases {
            for tape in all_tapes {
                assert_eq!(
                    can_read(drive, tape),
                    readable.contains(&tape),
                    "drive={drive:?} tape={tape:?}"
                );
            }
        }
        assert!(!can_read(LtoGen::Lto8, LtoGen::Lto6));
        assert!(!can_read(LtoGen::Lto9, LtoGen::Lto7));
        assert!(!can_read(LtoGen::Lto9, LtoGen::M8));
    }

    #[test]
    fn lto_write_compatibility_uses_design_table() {
        let cases = [
            (LtoGen::Lto5, &[LtoGen::Lto5, LtoGen::Lto4][..]),
            (LtoGen::Lto6, &[LtoGen::Lto6, LtoGen::Lto5][..]),
            (LtoGen::Lto7, &[LtoGen::Lto7, LtoGen::Lto6][..]),
            (LtoGen::Lto8, &[LtoGen::Lto8, LtoGen::Lto7, LtoGen::M8][..]),
            (LtoGen::Lto9, &[LtoGen::Lto9, LtoGen::Lto8][..]),
        ];
        let all_tapes = [
            LtoGen::Lto1,
            LtoGen::Lto2,
            LtoGen::Lto3,
            LtoGen::Lto4,
            LtoGen::Lto5,
            LtoGen::Lto6,
            LtoGen::Lto7,
            LtoGen::M8,
            LtoGen::Lto8,
            LtoGen::Lto9,
        ];

        for (drive, writable) in cases {
            for tape in all_tapes {
                assert_eq!(
                    can_write(drive, tape),
                    writable.contains(&tape),
                    "drive={drive:?} tape={tape:?}"
                );
            }
        }
        assert!(!can_write(LtoGen::Lto8, LtoGen::Lto6));
        assert!(!can_write(LtoGen::Lto9, LtoGen::Lto7));
        assert!(!can_write(LtoGen::Lto9, LtoGen::M8));
    }

    // -- pinned-tape admission matrix ---------------------------------------
    //
    // Pinning replaces selection, never admission: these tests pin the
    // refusals. The batch-eligibility branch (committed tape without an
    // adopted checkpoint journal) reuses the same helpers the pool-mode
    // selection tests already exercise and needs a written tape to stage, so
    // it is not duplicated here.

    struct PinnedFixture {
        index: CatalogIndex,
        pool_cfg: TapePoolConfig,
        journal_dir: std::path::PathBuf,
        _temp: tempfile::TempDir,
    }

    const PINNED_POOL: &str = "camera.copy-a";
    const PINNED_TAPE: TapeUuid = [0x5a; 16];

    fn pinned_fixture() -> PinnedFixture {
        let temp = tempfile::Builder::new()
            .prefix("remanence-pinned-admission-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
        index
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: PINNED_POOL.to_string(),
                display_name: None,
                copy_class: Some("copy-a".to_string()),
                content_class: Some("camera".to_string()),
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: PINNED_TAPE,
                voltag: "PIN001L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(PINNED_TAPE, PINNED_POOL)
            .expect("project membership");
        let pool_cfg = TapePoolConfig {
            id: PINNED_POOL.to_string(),
            display_name: None,
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            selection_policy: Default::default(),
            watermark_low: 0.98,
            watermark_high: 1.0,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let journal_dir = temp.path().join("checkpoint-journals");
        std::fs::create_dir_all(&journal_dir).expect("journal dir");
        PinnedFixture {
            index,
            pool_cfg,
            journal_dir,
            _temp: temp,
        }
    }

    fn admit(
        fixture: &PinnedFixture,
        tape_uuid: TapeUuid,
        guard: &str,
    ) -> Result<SelectedTape, PinnedTapeError> {
        admit_pinned_tape_for_write_session(
            &fixture.index,
            tape_uuid,
            guard,
            &fixture.pool_cfg,
            &fixture.journal_dir,
        )
    }

    #[test]
    fn pinned_admission_accepts_matching_pooled_tape() {
        let fixture = pinned_fixture();
        let selected = admit(&fixture, PINNED_TAPE, PINNED_POOL).expect("admit pinned tape");
        assert_eq!(selected.tape_uuid, PINNED_TAPE);
        assert_eq!(selected.pool_id, PINNED_POOL);
        assert_eq!(selected.block_size, 4096);
    }

    #[test]
    fn pinned_admission_refuses_unknown_tape() {
        let fixture = pinned_fixture();
        let error = admit(&fixture, [0x77; 16], PINNED_POOL).unwrap_err();
        assert!(
            matches!(error, PinnedTapeError::UnknownTape { .. }),
            "{error}"
        );
        // The message must teach the uninitialized-cartridge case.
        assert!(error.to_string().contains("rem tape init"), "{error}");
    }

    #[test]
    fn pinned_admission_refuses_pool_guard_mismatch_naming_both_pools() {
        let fixture = pinned_fixture();
        let error = admit(&fixture, PINNED_TAPE, "offsite.copy-b").unwrap_err();
        match &error {
            PinnedTapeError::PoolGuardMismatch {
                required_pool_id,
                actual_pool_id,
                ..
            } => {
                assert_eq!(required_pool_id, "offsite.copy-b");
                assert_eq!(actual_pool_id.as_deref(), Some(PINNED_POOL));
            }
            other => panic!("expected PoolGuardMismatch, got {other}"),
        }
        let message = error.to_string();
        assert!(message.contains("offsite.copy-b"), "{message}");
        assert!(message.contains(PINNED_POOL), "{message}");
    }

    #[test]
    fn pinned_admission_refuses_unpooled_tape_under_a_guard() {
        let fixture = pinned_fixture();
        let unpooled: TapeUuid = [0x5b; 16];
        {
            let mut index =
                CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
            index
                .provision_tape(remanence_state::ProvisionTapeInput {
                    tape_uuid: unpooled,
                    voltag: "PIN002L9".to_string(),
                    block_size: 4096,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision unpooled tape");
        }
        let fresh =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        let error = admit_pinned_tape_for_write_session(
            &fresh,
            unpooled,
            PINNED_POOL,
            &fixture.pool_cfg,
            &fixture.journal_dir,
        )
        .unwrap_err();
        match &error {
            PinnedTapeError::PoolGuardMismatch { actual_pool_id, .. } => {
                assert_eq!(actual_pool_id, &None);
            }
            other => panic!("expected PoolGuardMismatch, got {other}"),
        }
    }

    #[test]
    fn pinned_admission_refuses_cleaning_cartridge() {
        let fixture = pinned_fixture();
        let cleaning = {
            let mut index =
                CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
            let record = index
                .ensure_cleaning_cartridge("CLN901L9")
                .expect("cleaning cartridge");
            let mut uuid = [0u8; 16];
            uuid.copy_from_slice(&record.tape_uuid);
            // A cleaning cartridge must refuse even if someone projected it
            // into the guarded pool.
            index
                .project_tape_pool_membership(uuid, PINNED_POOL)
                .expect("project cleaning membership");
            uuid
        };
        let fresh =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        let error = admit_pinned_tape_for_write_session(
            &fresh,
            cleaning,
            PINNED_POOL,
            &fixture.pool_cfg,
            &fixture.journal_dir,
        )
        .unwrap_err();
        assert!(
            matches!(error, PinnedTapeError::NotADataTape { .. }),
            "{error}"
        );
    }

    #[test]
    fn pinned_admission_refuses_sealed_tape() {
        let fixture = pinned_fixture();
        {
            let mut index =
                CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
            index.seal_tape(PINNED_TAPE).expect("seal tape");
        }
        let fresh =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        let error = admit_pinned_tape_for_write_session(
            &fresh,
            PINNED_TAPE,
            PINNED_POOL,
            &fixture.pool_cfg,
            &fixture.journal_dir,
        )
        .unwrap_err();
        match &error {
            PinnedTapeError::NotWritable { reason, .. } => {
                assert!(
                    matches!(reason, WritabilityError::NotReady { .. }),
                    "{reason}"
                );
            }
            other => panic!("expected NotWritable, got {other}"),
        }
    }

    #[test]
    fn pinned_admission_refuses_fenced_tape() {
        let fixture = pinned_fixture();
        {
            let mut index =
                CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
            index
                .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                    tape_uuid: PINNED_TAPE,
                    barcode: Some("PIN001L9".to_string()),
                    reason: "partial_batch".to_string(),
                    evidence_json: None,
                })
                .expect("record fence");
        }
        let fresh =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        let error = admit_pinned_tape_for_write_session(
            &fresh,
            PINNED_TAPE,
            PINNED_POOL,
            &fixture.pool_cfg,
            &fixture.journal_dir,
        )
        .unwrap_err();
        match &error {
            PinnedTapeError::Fenced { reason, .. } => {
                assert_eq!(reason, "partial_batch");
            }
            other => panic!("expected Fenced, got {other}"),
        }
    }
}
