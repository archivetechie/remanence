//! Pool-targeted object write core for the Phase 1 non-hardware path.
//!
//! This module composes Layer 4 catalog state, Layer 3b `rao-v1`
//! streaming, Layer 3c parity, and the existing in-memory-compatible
//! `BlockSink` adapter. It intentionally contains the tape-selection boundary
//! so the later policy workstream can replace that one function without
//! changing the write engine.

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use remanence_aead::{seal_to_vec, RaoMetadata, RootKey, SealOptions, SealReport};
use remanence_format::{
    write_rem_tar_object_from_readers, RemTarFileLayout, RemTarFileStream, RemTarObjectOptions,
    FORMAT_ID,
};
use remanence_library::{
    BlockSink, BlockSource, TapeIoError, TapePosition, VecBlockSink, WriteFilemarksOutcome,
    WriteOutcome,
};
use remanence_parity::{
    bootstrap::{parse_bootstrap_block, write_bootstrap_block, BootstrapObjectRow},
    BlockSinkRawTapeSink, BootstrapObjectRowAdmission, BootstrapPayload, CapacityReserveInput,
    CommittedBundle, CommittedBundleKind, FilemarkMapDigest, ObjectWriteSummary, ParityConfig,
    ParityScheme, ParitySchemeRecord, ParitySink, SchemeId, TapeFileEntry, TapeFileKind,
};
use remanence_state::{
    validate_tape_pool_capacity_invariant, watermark_floor_bytes, CatalogIndex,
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
    StateError, TapeJournalIndexInput, TapePoolConfig, TapeRecord,
    OBJECT_COPY_REPRESENTATION_ENCRYPTED, OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use remanence_stream::{
    plan_prepared_object, prepare_regular_file, write_prepared_object_to_parity,
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
    CompleteOrFill, FillOldest, PoolSelectionContext, PoolSelectionPolicy, Selection, TapeFitState,
};
use crate::{bytes_to_hex, pb, timestamp_from_rfc3339};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const VERIFY_BOOTSTRAP_READ_BYTES: usize = 1024 * 1024;

/// Binary UUID used for physical tape identifiers and object identifiers.
pub type TapeUuid = [u8; 16];

/// Stored RAO representation requested for a pool write.
#[derive(Clone, Debug)]
pub enum PoolWriteRepresentation {
    /// Store the canonical plaintext `rao-v1` tar/PAX object.
    Plaintext,
    /// Store the RAO1 encrypted representation.
    Encrypted {
        /// Root key material used to seal the object.
        root_key: RootKey,
        /// Opaque 16-byte RAO key identifier to record in the envelope.
        key_id: [u8; 16],
    },
}

/// Inputs for writing one regular file as one `rao-v1` object to a pool.
#[derive(Clone, Debug)]
pub struct WriteObjectToPoolRequest {
    /// Pool requested by the caller.
    pub pool_id: String,
    /// Local regular file to stream into the object.
    pub source_path: PathBuf,
    /// UTF-8 relative path to record inside the `rao-v1` object.
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
    /// Logical payload bytes, excluding generated `rao-v1` metadata.
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
        pb::ObjectRecord {
            object_id: self.object_id.to_vec(),
            caller_object_id: self.caller_object_id.clone(),
            content_sha256: self.content_sha256.to_vec(),
            logical_size_bytes: self.logical_size_bytes,
            body_format: self.body_format.clone(),
            caller_metadata: Default::default(),
            created_at: timestamp_from_rfc3339(self.created_at_utc.as_str()),
            copies: self
                .copies
                .iter()
                .map(PoolWriteObjectCopyRecord::to_proto)
                .collect(),
        }
    }

    /// Return the object id as canonical UUID text for catalog lookups.
    pub fn object_id_text(&self) -> String {
        Uuid::from_bytes(self.object_id).to_string()
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
    /// Stored RAO representation: `plaintext` or `encrypted`.
    pub representation: String,
    /// Opaque RAO key id for encrypted copies.
    pub key_id: Option<[u8; 16]>,
    /// Encrypted RAO metadata frame length.
    pub metadata_frame_len: Option<u64>,
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
        }
    }
}

/// Full report returned by `write_object_to_pool`.
#[derive(Debug)]
pub struct PoolWriteResult {
    /// Locator/object record for the caller.
    pub object: PoolWriteObjectRecord,
    /// Lower-layer streaming write report for tests and future audit wiring.
    pub write_report: StreamingObjectWriteReport,
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
    /// The prepared source payload hash did not match the caller-supplied guard.
    #[error("content SHA-256 mismatch: expected {expected}, actual {actual}")]
    ContentHashMismatch {
        /// Expected SHA-256 as lowercase hex.
        expected: String,
        /// Actual prepared payload SHA-256 as lowercase hex.
        actual: String,
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
    /// Timestamp formatting failed.
    #[error("format timestamp: {0}")]
    TimeFormat(#[from] time::error::Format),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NoParityAppendContext {
    tape_file_number: u32,
    previous_total_committed_ordinals: u64,
    fresh_tape: bool,
}

impl NoParityAppendContext {
    fn object_total_committed_ordinals(self, object_blocks: u64) -> Result<u64, PoolWriteError> {
        self.previous_total_committed_ordinals
            .checked_add(object_blocks)
            .ok_or_else(|| {
                PoolWriteError::InvalidInput("no-parity committed ordinal overflow".to_string())
            })
    }
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

    // 2a-2 owns the hard writability precondition (state/geometry/capacity fit);
    // the policy ranks only the tapes that pass it (design §6 boundary).
    let mut reasons = Vec::new();
    let mut eligible = Vec::new();
    for tape in tapes {
        match check_writability_preconditions(&tape, object_size)
            .and_then(|_| check_pool_block_size_precondition(&tape, pool_cfg))
        {
            Ok(()) => eligible.push(tape),
            Err(err) => reasons.push(err),
        }
    }
    if eligible.is_empty() {
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

/// Write one regular file to a caller-named pool using the Phase 1
/// non-hardware-compatible `BlockSink` path, commit catalog rows, and return
/// the resulting object locator.
pub fn write_object_to_pool(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    let source_size = source_file_size(&request.source_path)?;
    let reserved_tape_uuids = HashSet::new();
    let selected = select_tape_in_pool(state, pool_cfg, source_size, &reserved_tape_uuids)?;
    write_to_selected_tape(state, sink, pool_cfg, request, selected)
}

/// Write one regular file to a previously selected tape without re-running
/// pool tape selection.
///
/// This is the select-once entrypoint for callers that already opened a write
/// session against a concrete tape. The selected tape's [`ParityConfig`]
/// controls whether the write uses the existing parity path or the direct
/// no-parity bootstrap/body/filemark path.
pub fn write_to_selected_tape(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
    write_to_selected_tape_with_live_counter(state, sink, pool_cfg, request, selected, None)
}

pub(crate) fn write_to_selected_tape_with_live_counter(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
) -> Result<PoolWriteResult, PoolWriteError> {
    match live_write_counter {
        Some(counter) => {
            let mut live_counted_sink = LiveCounterBlockSink::new(sink, counter);
            write_to_selected_tape_inner(state, &mut live_counted_sink, pool_cfg, request, selected)
        }
        None => write_to_selected_tape_inner(state, sink, pool_cfg, request, selected),
    }
}

fn write_to_selected_tape_inner<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut S,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
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
    let stored_size_bytes = stored_footprint_bytes(&stored, &prepared, selected.block_size)?;
    ensure_selected_tape_has_capacity(state, &selected, stored_size_bytes)?;
    let prepare_elapsed = prepare_started.elapsed();
    let payload_bytes = prepared_payload_bytes(&prepared);
    tracing::info!(
        target: "remanence_write_diag",
        "remanence_write_diag phase=prepare caller_object_id={:?} pool_id={:?} tape_uuid={} parity={} representation={} payload_bytes={} selected_block_size_bytes={} projected_object_blocks={} elapsed_ms={:.3} throughput_mib_s={:.3}",
        request.caller_object_id,
        selected.pool_id,
        uuid_text(selected.tape_uuid),
        parity_label(&selected.parity_config),
        stored.representation_label(),
        payload_bytes,
        selected.block_size,
        stored_projected_blocks,
        crate::diagnostics::duration_ms(prepare_elapsed),
        crate::diagnostics::mib_per_s(payload_bytes, prepare_elapsed),
    );

    // Only the hardware-backed tape transfer below is counted live. The spool
    // write already finished in mount.rs, and parity/object replay only reads
    // the prepared in-memory object.
    let mut counted_sink = CountingBlockSink::new(sink);
    let prepared_write = PreparedPoolWrite { prepared, stored };
    match selected.parity_config.clone() {
        ParityConfig::Scheme(scheme) => write_parity_object_to_selected_tape(
            state,
            &mut counted_sink,
            pool_cfg,
            request,
            selected,
            prepared_write,
            scheme,
        ),
        ParityConfig::None => write_no_parity_object_to_selected_tape(
            state,
            &mut counted_sink,
            pool_cfg,
            request,
            selected,
            prepared_write,
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
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
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
            filemark_map_digest: Some(initial_bootstrap_map_digest()),
            tape_uuid,
            written_by_version: written_by_version.into(),
            written_at: written_at.into(),
            sequence: 0,
            block_size_bytes: block_size,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
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
        "L9" => Some(LtoGen::Lto9),
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
    let used = tape
        .total_committed_ordinals
        .checked_mul(block_size)
        .ok_or_else(|| missing_geometry("used capacity overflows u64"))?;
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
    position_calls: u64,
    early_warning: bool,
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

    fn record_filemarks(&mut self, count: u32, early_warning: bool) {
        self.filemark_calls = self.filemark_calls.saturating_add(1);
        self.filemarks = self.filemarks.saturating_add(u64::from(count));
        self.early_warning |= early_warning;
    }

    fn record_position(&mut self, position: TapePosition) {
        self.position_calls = self.position_calls.saturating_add(1);
        self.early_warning |= position.block_position_end_of_warning;
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

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

impl<'a> LiveCounterBlockSink<'a> {
    pub(crate) fn new(
        inner: &'a mut dyn BlockSink,
        live_write_counter: Arc<crate::DriveByteCounters>,
    ) -> Self {
        Self {
            inner,
            live_write_counter,
        }
    }
}

impl<'a, S: BlockSink + ?Sized> CountingBlockSink<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            stats: BlockSinkStats::default(),
        }
    }

    fn stats(&self) -> BlockSinkStats {
        self.stats
    }
}

impl<'a> BlockSink for LiveCounterBlockSink<'a> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.live_write_counter
            .record_write_bytes(u64::from(outcome.bytes_written));
        Ok(outcome)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

impl<'a, S: BlockSink + ?Sized> BlockSink for CountingBlockSink<'a, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.stats
            .record_block(u64::from(outcome.bytes_written), outcome.early_warning);
        Ok(outcome)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let outcome = self.inner.write_filemarks(count)?;
        self.stats.record_filemarks(count, outcome.early_warning);
        Ok(outcome)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.position()?;
        self.stats.record_position(position);
        Ok(position)
    }
}

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
    let transfer_started = Instant::now();
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> = (|| {
        let mut raw = BlockSinkRawTapeSink::new(sink);
        let mut parity =
            ParitySink::new_sidecar_only(&mut raw, scheme.clone(), tape_uuid, block_size)?;
        parity.write_bootstrap()?;
        match &stored {
            PreparedStoredObject::Plaintext => Ok(write_prepared_object_to_parity(
                &mut parity,
                tape_uuid,
                &prepared.options,
                &prepared.files,
                capacity_input(
                    &scheme,
                    block_size,
                    prepared.plan.layout.projected_size_blocks,
                ),
            )?),
            PreparedStoredObject::Encrypted(encrypted) => write_encrypted_object_to_parity(
                &mut parity,
                tape_uuid,
                &prepared,
                encrypted,
                &scheme,
                block_size,
            ),
        }
    })();
    let transfer_elapsed = transfer_started.elapsed();
    let write_report = match write_report {
        Ok(write_report) => {
            let stats = sink.stats();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
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
    match commit_result {
        Ok(()) => {
            log_commit_diagnostics(&request, &selected, &prepared, commit_elapsed, "ok", None)
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
    }
    Ok(pool_write_result(
        request,
        selected,
        prepared,
        stored.copy_representation(),
        write_report,
    ))
}

fn write_no_parity_object_to_selected_tape<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut CountingBlockSink<'_, S>,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared_write: PreparedPoolWrite,
) -> Result<PoolWriteResult, PoolWriteError> {
    let PreparedPoolWrite { prepared, stored } = prepared_write;
    let tape_uuid = selected.tape_uuid;
    let append = no_parity_append_context(state, &selected)?;
    let transfer_started = Instant::now();
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> = (|| {
        if append.fresh_tape {
            write_no_parity_bootstrap(
                sink,
                tape_uuid,
                selected.block_size,
                &prepared.write_timestamp,
            )?;
        }
        match &stored {
            PreparedStoredObject::Plaintext => {
                let mut readers = open_prepared_readers(&prepared.files)?;
                let mut streams = Vec::with_capacity(prepared.files.len());
                for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
                    streams.push(RemTarFileStream::new(file.spec.clone(), reader));
                }
                let mut object_sink = ObjectDigestBlockSink::new(sink);
                let layout = write_rem_tar_object_from_readers(
                    &mut object_sink,
                    &prepared.options,
                    &mut streams,
                )
                .map_err(StreamingError::from)?;
                let object_digest = object_sink.finish_digest();
                let filemark_outcome = sink.write_filemarks(1)?;
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
                write_fixed_blocks(sink, prepared.options.chunk_size, &encrypted.sealed)?;
                let filemark_outcome = sink.write_filemarks(1)?;
                no_parity_encrypted_write_report(
                    tape_uuid,
                    &prepared,
                    encrypted,
                    filemark_outcome,
                    append,
                )
            }
        }
    })();
    let transfer_elapsed = transfer_started.elapsed();
    let write_report = match write_report {
        Ok(write_report) => {
            let stats = sink.stats();
            log_transfer_diagnostics(
                &request,
                &selected,
                &prepared,
                stored.projected_size_blocks(&prepared),
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
            first_parity_data_ordinal: None,
            protected_until_ordinal: None,
            scheme: None,
            copy_representation: stored.copy_representation(),
        },
        pool_cfg,
        transfer_stats.early_warning,
    );
    let commit_elapsed = commit_started.elapsed();
    match commit_result {
        Ok(()) => {
            log_commit_diagnostics(&request, &selected, &prepared, commit_elapsed, "ok", None)
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
    }
    Ok(pool_write_result(
        request,
        selected,
        prepared,
        stored.copy_representation(),
        write_report,
    ))
}

struct CommitPoolWriteProjection {
    first_parity_data_ordinal: Option<u64>,
    protected_until_ordinal: Option<u64>,
    scheme: Option<ParityScheme>,
    copy_representation: CopyRepresentation,
}

fn commit_pool_write(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    write_report: &StreamingObjectWriteReport,
    projection: CommitPoolWriteProjection,
    pool_cfg: &TapePoolConfig,
    hardware_early_warning: bool,
) -> Result<(), PoolWriteError> {
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
    state.project_native_object_and_committed_tape_file_bundle(
        NativeObjectProjectionInput {
            object_id: write_report.catalog.object.object_id.clone(),
            caller_object_id: Some(write_report.catalog.object.caller_object_id.clone()),
            body_format: write_report.catalog.object.body_format.clone(),
            logical_size_bytes: Some(write_report.catalog.object.logical_size_bytes),
            content_hash: Some(prepared.content_sha256.to_vec()),
            metadata_hash,
            created_at_utc: Some(prepared.write_timestamp.clone()),
        },
        &file_projections,
        &[NativeObjectCopyProjectionInput {
            object_id: write_report.catalog.object_copy.object_id.clone(),
            tape_uuid: selected.tape_uuid,
            tape_file_number: write_report.catalog.object_copy.tape_file_number,
            first_body_lba,
            first_parity_data_ordinal: projection.first_parity_data_ordinal,
            protected_until_ordinal: projection.protected_until_ordinal,
            status: "committed".to_string(),
            representation: projection.copy_representation.representation.to_string(),
            key_id: projection
                .copy_representation
                .key_id
                .map(|key_id| key_id.to_vec()),
            metadata_frame_len: projection.copy_representation.metadata_frame_len,
            plaintext_digest: Some(write_report.catalog.object_copy.plaintext_digest.to_vec()),
            stored_digest: Some(write_report.catalog.object_copy.stored_digest.to_vec()),
        }],
        TapeJournalIndexInput {
            tape_uuid: selected.tape_uuid,
            block_size: selected.block_size,
            scheme: projection.scheme,
            journal_offset_bytes: 0,
        },
        &write_report.catalog.tape_file_bundle,
    )?;
    seal_selected_tape_if_needed(state, selected, pool_cfg, hardware_early_warning)?;
    Ok(())
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

fn pool_write_result(
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    prepared: PreparedPoolObject,
    copy_representation: CopyRepresentation,
    write_report: StreamingObjectWriteReport,
) -> PoolWriteResult {
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
            tape_file_number: u64::from(write_report.catalog.object_copy.tape_file_number),
            first_body_lba,
            pool_id: selected.pool_id,
            representation: copy_representation.representation.to_string(),
            key_id: copy_representation.key_id,
            metadata_frame_len: copy_representation.metadata_frame_len,
        }],
    };

    PoolWriteResult {
        object,
        write_report,
    }
}

struct PreparedPoolObject {
    content_sha256: [u8; 32],
    object_uuid: Uuid,
    write_timestamp: String,
    options: RemTarObjectOptions,
    files: Vec<PreparedFile>,
    plan: StreamingObjectPlan,
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
            Self::Encrypted(encrypted) => CopyRepresentation::encrypted(
                encrypted.envelope.header.key_id,
                encrypted.envelope.metadata_frame_len,
            ),
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

#[derive(Clone, Copy)]
struct CopyRepresentation {
    representation: &'static str,
    key_id: Option<[u8; 16]>,
    metadata_frame_len: Option<u64>,
}

impl CopyRepresentation {
    fn plaintext() -> Self {
        Self {
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT,
            key_id: None,
            metadata_frame_len: None,
        }
    }

    fn encrypted(key_id: [u8; 16], metadata_frame_len: u64) -> Self {
        Self {
            representation: OBJECT_COPY_REPRESENTATION_ENCRYPTED,
            key_id: Some(key_id),
            metadata_frame_len: Some(metadata_frame_len),
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
    request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    stored_projected_blocks: u64,
    outcome: TransferDiagnosticOutcome<'_>,
) {
    let payload_bytes = prepared_payload_bytes(prepared);
    tracing::info!(
        target: "remanence_write_diag",
        "remanence_write_diag phase=transfer caller_object_id={:?} pool_id={:?} tape_uuid={} parity={} status={} error={:?} payload_bytes={} selected_block_size_bytes={} format_chunk_size_bytes={} projected_object_blocks={} sink_write_bytes={} block_write_calls={} min_block_bytes={} max_block_bytes={} filemark_calls={} filemarks={} position_calls={} early_warning={} scsi_write_cdb=WRITE6_VARIABLE drive_write_per_block_read_position=true write_filemarks_immed=false elapsed_ms={:.3} throughput_mib_s={:.3}",
        request.caller_object_id,
        selected.pool_id,
        uuid_text(selected.tape_uuid),
        parity_label(&selected.parity_config),
        outcome.status,
        outcome.error.unwrap_or(""),
        payload_bytes,
        selected.block_size,
        prepared.options.chunk_size,
        stored_projected_blocks,
        outcome.stats.block_write_bytes,
        outcome.stats.block_write_calls,
        outcome.stats.min_block_bytes.unwrap_or(0),
        outcome.stats.max_block_bytes.unwrap_or(0),
        outcome.stats.filemark_calls,
        outcome.stats.filemarks,
        outcome.stats.position_calls,
        outcome.stats.early_warning,
        crate::diagnostics::duration_ms(outcome.elapsed),
        crate::diagnostics::mib_per_s(payload_bytes, outcome.elapsed),
    );
}

struct TransferDiagnosticOutcome<'a> {
    stats: BlockSinkStats,
    elapsed: Duration,
    status: &'static str,
    error: Option<&'a str>,
}

fn log_commit_diagnostics(
    request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    elapsed: Duration,
    status: &str,
    error: Option<&str>,
) {
    let payload_bytes = prepared_payload_bytes(prepared);
    tracing::info!(
        target: "remanence_write_diag",
        "remanence_write_diag phase=commit caller_object_id={:?} pool_id={:?} tape_uuid={} parity={} status={} error={:?} payload_bytes={} elapsed_ms={:.3} throughput_mib_s={:.3}",
        request.caller_object_id,
        selected.pool_id,
        uuid_text(selected.tape_uuid),
        parity_label(&selected.parity_config),
        status,
        error.unwrap_or(""),
        payload_bytes,
        crate::diagnostics::duration_ms(elapsed),
        crate::diagnostics::mib_per_s(payload_bytes, elapsed),
    );
}

fn prepare_pool_object(
    request: &WriteObjectToPoolRequest,
    block_size: u32,
) -> Result<PreparedPoolObject, PoolWriteError> {
    let _ = source_file_size(&request.source_path)?;
    let content_sha256 = sha256_file(&request.source_path)?;
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
    let files = vec![prepare_regular_file(
        &request.source_path,
        &request.archive_path,
        Uuid::new_v4().to_string(),
    )?];
    let plan = plan_prepared_object(&options, &files)?;
    Ok(PreparedPoolObject {
        content_sha256,
        object_uuid,
        write_timestamp,
        options,
        files,
        plan,
    })
}

fn prepare_stored_object(
    prepared: &PreparedPoolObject,
    representation: &PoolWriteRepresentation,
) -> Result<PreparedStoredObject, PoolWriteError> {
    match representation {
        PoolWriteRepresentation::Plaintext => Ok(PreparedStoredObject::Plaintext),
        PoolWriteRepresentation::Encrypted { root_key, key_id } => {
            Ok(PreparedStoredObject::Encrypted(Box::new(
                seal_prepared_object(prepared, root_key, *key_id)?,
            )))
        }
    }
}

fn seal_prepared_object(
    prepared: &PreparedPoolObject,
    root_key: &RootKey,
    key_id: [u8; 16],
) -> Result<PreparedEncryptedPoolObject, PoolWriteError> {
    let mut plaintext_sink = VecBlockSink::new();
    let mut readers = open_prepared_readers(&prepared.files)?;
    let mut streams = Vec::with_capacity(prepared.files.len());
    for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
        streams.push(RemTarFileStream::new(file.spec.clone(), reader));
    }
    let plaintext_layout =
        write_rem_tar_object_from_readers(&mut plaintext_sink, &prepared.options, &mut streams)
            .map_err(StreamingError::from)?;
    if plaintext_layout.projected_size_blocks != prepared.plan.layout.projected_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "sealed plaintext layout differs from pre-admission plan".to_string(),
        ));
    }
    let plaintext = flatten_blocks(plaintext_sink.blocks, prepared.options.chunk_size)?;
    if plaintext.len() as u64 != plaintext_layout.total_size_bytes {
        return Err(PoolWriteError::InvalidInput(format!(
            "sealed plaintext byte length {} does not match layout {}",
            plaintext.len(),
            plaintext_layout.total_size_bytes
        )));
    }
    let plaintext_digest = sha256_array(&plaintext);
    let chunk_size = u32::try_from(prepared.options.chunk_size)
        .map_err(|_| PoolWriteError::InvalidInput("RAO chunk size exceeds u32".to_string()))?;
    let metadata = RaoMetadata::new(
        plaintext_layout.total_size_bytes,
        plaintext_digest,
        chunk_size,
    )
    .map_err(|error| PoolWriteError::InvalidInput(format!("build RAO metadata: {error}")))?;
    let metadata_plaintext = metadata
        .to_cbor_bytes(chunk_size)
        .map_err(|error| PoolWriteError::InvalidInput(format!("encode RAO metadata: {error}")))?;
    let seal_options = SealOptions {
        chunk_size,
        key_id,
        object_id: prepared.options.object_id.clone(),
        plaintext_size: plaintext_layout.total_size_bytes,
        plaintext_digest,
    };
    let (sealed, envelope) = seal_to_vec(&plaintext, root_key, &seal_options)
        .map_err(|error| PoolWriteError::InvalidInput(format!("seal encrypted RAO: {error}")))?;
    let expected_metadata_frame_len = u64::try_from(metadata_plaintext.len())
        .ok()
        .and_then(|len| len.checked_add(16))
        .ok_or_else(|| PoolWriteError::InvalidInput("RAO metadata length overflow".to_string()))?;
    if envelope.metadata_frame_len != expected_metadata_frame_len {
        return Err(PoolWriteError::InvalidInput(
            "encrypted RAO metadata frame length changed during seal".to_string(),
        ));
    }
    let block_count = u64::try_from(sealed.len() / prepared.options.chunk_size)
        .map_err(|_| PoolWriteError::InvalidInput("sealed RAO block count overflow".to_string()))?;
    if sealed.len() % prepared.options.chunk_size != 0 || block_count != envelope.stored_size_blocks
    {
        return Err(PoolWriteError::InvalidInput(
            "sealed RAO bytes do not match envelope block count".to_string(),
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
                "RAO block length {} does not match chunk size {block_size}",
                block.len()
            )));
        }
        out.extend_from_slice(&block);
    }
    Ok(out)
}

fn sha256_array(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn write_fixed_blocks(
    sink: &mut dyn BlockSink,
    block_size: usize,
    bytes: &[u8],
) -> Result<u64, PoolWriteError> {
    if bytes.len() % block_size != 0 {
        return Err(PoolWriteError::InvalidInput(
            "stored RAO bytes are not block aligned".to_string(),
        ));
    }
    let mut blocks = 0u64;
    for block in bytes.chunks_exact(block_size) {
        sink.write_block(block)?;
        blocks = blocks
            .checked_add(1)
            .ok_or_else(|| PoolWriteError::InvalidInput("block count overflow".to_string()))?;
    }
    Ok(blocks)
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

fn open_prepared_readers(files: &[PreparedFile]) -> Result<Vec<File>, PoolWriteError> {
    files
        .iter()
        .map(|file| {
            File::open(&file.source_path).map_err(|source| PoolWriteError::Io {
                context: "open source file for streaming",
                path: file.source_path.clone(),
                source,
            })
        })
        .collect()
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
    let object_close = ObjectWriteSummary {
        tape_file_number: append.tape_file_number,
        first_parity_data_ordinal: 0,
        projected_size_blocks: prepared.plan.layout.projected_size_blocks,
        data_block_count: layout.projected_size_blocks,
        filemark_outcome,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 0,
        control_tape_files_emitted: Vec::new(),
        bootstrap_object_row: None,
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
            physical_start_hint: None,
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            bootstrap_object_row: None,
        });
    }
    tape_file_entries.push(TapeFileEntry {
        tape_file_number: object_close.tape_file_number,
        kind: TapeFileKind::Object,
        block_count: layout.projected_size_blocks,
        physical_start_hint: None,
        object_id: Some(prepared.options.object_id.clone()),
        first_parity_data_ordinal: None,
        epoch_id: None,
        protected_ordinal_start: None,
        protected_ordinal_end_exclusive: None,
        canonical_metadata_hash: None,
        bootstrap_object_row: None,
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
    scheme: &ParityScheme,
    block_size: u32,
) -> Result<StreamingObjectWriteReport, PoolWriteError> {
    let opened = parity.begin_object_with_capacity_reserve_and_bootstrap_object_row(
        capacity_input(scheme, block_size, encrypted.envelope.stored_size_blocks),
        BootstrapObjectRowAdmission::EncryptedRao,
    )?;
    let blocks_written = write_fixed_blocks(
        parity,
        prepared.options.chunk_size,
        encrypted.sealed.as_slice(),
    )?;
    if blocks_written != encrypted.envelope.stored_size_blocks {
        return Err(PoolWriteError::InvalidInput(
            "encrypted RAO write count differs from envelope".to_string(),
        ));
    }
    parity.record_bootstrap_object_row(BootstrapObjectRow::encrypted(
        opened.0,
        encrypted.envelope.stored_size_blocks,
        encrypted.envelope.header.key_id,
        encrypted.envelope.metadata_frame_len,
    ))?;
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
        control_tape_files_emitted: Vec::new(),
        bootstrap_object_row: Some(BootstrapObjectRow::encrypted(
            append.tape_file_number,
            encrypted.envelope.stored_size_blocks,
            encrypted.envelope.header.key_id,
            encrypted.envelope.metadata_frame_len,
        )),
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
                physical_start_hint: None,
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                bootstrap_object_row: None,
            });
        }
        entries.push(TapeFileEntry {
            tape_file_number: object_close.tape_file_number,
            kind: TapeFileKind::Object,
            block_count: encrypted.envelope.stored_size_blocks,
            physical_start_hint: None,
            object_id: Some(prepared.options.object_id.clone()),
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            bootstrap_object_row: object_close.bootstrap_object_row.clone(),
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

fn initial_bootstrap_map_digest() -> FilemarkMapDigest {
    FilemarkMapDigest {
        map_sha256: [0u8; 32],
        tape_file_count: 1,
        map_total_data_ordinals: 0,
        highest_protected_ordinal: 0,
        is_final_map: false,
    }
}

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
    let capacity = tape_capacity_bytes(tape)?;
    let block_size = tape_block_size(tape)?;
    let used_bytes = tape
        .total_committed_ordinals
        .checked_mul(block_size)
        .ok_or_else(|| missing_geometry("used capacity overflows u64"))?;
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

fn tape_capacity_bytes(tape: &TapeRecord) -> Result<u64, WritabilityError> {
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
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
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
            ParityConfig::Scheme(_) => Err(PoolWriteError::ParityAppendUnsupported {
                tape_uuid: uuid_text(selected.tape_uuid),
                total_committed_ordinals: tape.total_committed_ordinals,
            }),
        };
    }
    Ok(())
}

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
        Some(last) => u32::try_from(last)
            .map_err(|_| {
                PoolWriteError::MissingTapeGeometry(
                    "last_committed_tape_file overflows u32".to_string(),
                )
            })?
            .checked_add(1)
            .ok_or_else(|| {
                PoolWriteError::MissingTapeGeometry(
                    "next no-parity tape file overflows u32".to_string(),
                )
            })?,
        None => 1,
    };
    Ok(NoParityAppendContext {
        tape_file_number,
        previous_total_committed_ordinals,
        fresh_tape: previous_total_committed_ordinals == 0
            && tape.last_committed_tape_file.is_none(),
    })
}

fn ensure_selected_tape_has_capacity(
    state: &CatalogIndex,
    selected: &SelectedTape,
    object_size: u64,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let used = tape
        .total_committed_ordinals
        .checked_mul(block_size)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry("used capacity overflows u64".to_string())
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

fn seal_selected_tape_if_needed(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    hardware_early_warning: bool,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
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
    }
    Ok(())
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

fn selected_tape_geometry(
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

fn capacity_input(
    scheme: &ParityScheme,
    block_size: u32,
    projected_object_blocks: u64,
) -> CapacityReserveInput {
    let data_shards_per_epoch =
        u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
    let parity_shards_per_epoch =
        u64::from(scheme.parity_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
    CapacityReserveInput {
        projected_object_blocks,
        block_size_bytes: u64::from(block_size),
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch,
        parity_shards_per_epoch,
        sidecar_index_block_count: 1,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        bootstrap_filemark_blocks: 1,
        pending_completed_sidecars: 0,
        remaining_bootstrap_count: 1,
        safety_margin_blocks: 4,
        remaining_tape_blocks: 1_000_000,
        empty_tape_usable_blocks: 1_000_000,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 1_000_000_000,
    }
}

fn now_rfc3339() -> Result<String, PoolWriteError> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

fn uuid_text(value: [u8; 16]) -> String {
    Uuid::from_bytes(value).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tape_position_with_warning(block_position_end_of_warning: bool) -> TapePosition {
        TapePosition {
            lba: 0,
            partition: 0,
            beginning_of_partition: false,
            end_of_partition: false,
            block_position_end_of_warning,
        }
    }

    #[test]
    fn block_sink_stats_latches_hardware_early_warning() {
        let mut stats = BlockSinkStats::default();
        stats.record_block(256 * 1024, true);
        assert!(stats.early_warning);

        let mut stats = BlockSinkStats::default();
        stats.record_filemarks(1, true);
        assert!(stats.early_warning);

        let mut stats = BlockSinkStats::default();
        stats.record_position(tape_position_with_warning(true));
        assert!(stats.early_warning);
    }

    #[test]
    fn live_write_counter_advances_during_transfer() {
        let counter = Arc::new(crate::DriveByteCounters::new(0));
        let mut sink = VecBlockSink::new();
        let mut live_sink = LiveCounterBlockSink::new(&mut sink, Arc::clone(&counter));

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
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let selected = select_tape_in_pool(&index, &cfg, 6, &HashSet::new()).expect("select tape");

        let payload_path = temp.path().join("payload.bin");
        std::fs::write(&payload_path, b"abcdef").expect("write payload");
        let request = WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source_path: payload_path.clone(),
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
    fn lto_generation_parses_m8_type_m_suffix() {
        assert_eq!(lto_generation_from_voltag("RMN001M8"), Some(LtoGen::M8));
        assert_eq!(lto_generation_from_voltag("rmn001m8"), Some(LtoGen::M8));
        assert_eq!(raw_capacity_bytes(LtoGen::M8), 9_000_000_000_000);
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
}
