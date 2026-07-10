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
use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc as std_mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

use remanence_aead::{seal_to_vec, RaoMetadata, RootKey, SealOptions, SealReport};
use remanence_format::{
    write_rem_tar_object_from_readers, RemTarFileLayout, RemTarFileStream, RemTarObjectOptions,
    FORMAT_ID,
};
use remanence_library::{
    BlockSink, BlockSource, PipelinedWriteDiagnostics, TapeIoError, TapePosition, VecBlockSink,
    WriteBatchOutcome, WriteFilemarksOutcome, WriteOutcome,
};
use remanence_parity::{
    bootstrap::{parse_bootstrap_block, write_bootstrap_block, BootstrapObjectRow},
    BlockSinkRawTapeSink, BootstrapObjectRowAdmission, BootstrapPayload, CapacityReserveInput,
    CommittedBundle, CommittedBundleKind, FilemarkMapDigest, ObjectWriteSummary, ParityConfig,
    ParityScheme, ParitySchemeRecord, ParitySink, SchemeId, TapeFileEntry, TapeFileKind,
};
use remanence_state::{
    validate_tape_pool_capacity_invariant, watermark_floor_bytes, CatalogIndex,
    NativeObjectCopyProjectionInput, NativeObjectCopyRecord, NativeObjectFileProjectionInput,
    NativeObjectProjectionInput, NativeObjectRecord, StateError, TapeJournalIndexInput,
    TapePoolConfig, TapeRecord, OBJECT_COPY_REPRESENTATION_ENCRYPTED,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
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
use crate::{append_mode_for_tape_file_number, bytes_to_hex, pb, timestamp_from_rfc3339};

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
        let append_commit_info = self.copies.first().map(append_commit_info_from_pool_copy);
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
            append_commit_info,
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

fn append_commit_info_from_pool_copy(copy: &PoolWriteObjectCopyRecord) -> pb::AppendCommitInfo {
    pb::AppendCommitInfo {
        append_mode: append_mode_for_tape_file_number(copy.tape_file_number) as i32,
        tape_uuid: copy.tape_uuid.to_vec(),
        voltag: None,
        tape_file_number: copy.tape_file_number,
        first_body_lba: copy.first_body_lba,
        position_before_lba: None,
        position_after_lba: None,
        journal_record_ordinal: None,
        estimated_remaining_bytes: None,
        sealed_after_write: None,
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
}

impl PoolWriteResult {
    /// True when this result was returned from the catalog replay path.
    pub fn is_replay(&self) -> bool {
        self.write_report.is_none()
    }

    /// Borrow the streaming report for callers that require proof of a new write.
    pub fn write_report(&self) -> Option<&StreamingObjectWriteReport> {
        self.write_report.as_ref()
    }

    /// Borrow the streaming report and panic if the result was a replay.
    #[cfg(test)]
    pub fn expect_write_report(&self) -> &StreamingObjectWriteReport {
        self.write_report
            .as_ref()
            .expect("pool write result should include a new write report")
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
        if let Err(err) = check_writability_preconditions(&tape, object_size)
            .and_then(|_| check_pool_block_size_precondition(&tape, pool_cfg))
        {
            reasons.push(err);
            continue;
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
        eligible.push(tape);
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
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
        return Ok(result);
    }
    let source_size = source_file_size(&request.source_path)?;
    let reserved_tape_uuids = HashSet::new();
    let selected = select_tape_in_pool(state, pool_cfg, source_size, &reserved_tape_uuids)?;
    write_to_selected_tape_inner(state, sink, pool_cfg, request, selected, false)
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
    write_to_selected_tape_with_live_counter_impl(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        live_write_counter,
        true,
    )
}

pub(crate) fn write_to_selected_tape_with_live_counter_after_replay_check(
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
        false,
    )
}

fn write_to_selected_tape_with_live_counter_impl(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
    check_replay: bool,
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
            )
        }
        None => {
            write_to_selected_tape_inner(state, sink, pool_cfg, request, selected, check_replay)
        }
    }
}

fn write_to_selected_tape_inner<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut S,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    check_replay: bool,
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
    let stored_size_bytes = stored_footprint_bytes(&stored, &prepared, selected.block_size)?;
    ensure_selected_tape_has_capacity(state, &selected, stored_size_bytes)?;
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
    write_batch_blocks: u32,
    effective_batch_blocks: u32,
    position_check_bytes: u64,
    staging_ring_buffers: u32,
    gap_p50_us: u64,
    gap_p95_us: u64,
    gap_max_us: u64,
    ioctl_p50_us: u64,
    ioctl_p95_us: u64,
    ioctl_max_us: u64,
    cadence_us: u64,
    effective_feed_bytes_per_second: u64,
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

struct PageAlignedBuffer {
    storage: Vec<u8>,
    start: usize,
    capacity: usize,
    used: usize,
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
    SpaceToEndOfData {
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

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
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
        let buffer = self.free_rx.recv().map_err(|_| {
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

    fn finish(mut self) -> Result<(), TapeIoError> {
        self.flush_pending()?;
        self.request(|reply| PipelinedSinkCommand::Barrier { reply })
            .map(|_| ())
    }

    fn abort(self, message: String) {
        set_staged_poison(&self.poison, message);
        let StagedBlockSink {
            tx,
            free_rx,
            submitter_done_rx,
            ..
        } = self;
        drop(tx);
        let _free_rx = free_rx;
        let _ = submitter_done_rx.recv();
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
        self.current
            .as_mut()
            .expect("buffer acquired")
            .append(buf)?;
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

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::SpaceToEndOfData { reply })?;
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
    run_ring_staged_transfer(inner, block_size, producer, on_safety_error)
}

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
    run_staged_transfer_with_safety(inner, block_size, producer, |error| {
        let error = error.to_string();
        record_tape_io_fence_for_transfer_error(
            state,
            selected,
            tape_io_fence_reason_for_transfer_error(&error),
            &error,
        )
    })
}

fn run_ring_staged_transfer<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
    mut on_safety_error: impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<R, PoolWriteError>
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
                Ok(value) => staged
                    .finish()
                    .map(|()| value)
                    .map_err(PoolWriteError::from),
                Err(err) => {
                    staged.abort(err.to_string());
                    Err(err)
                }
            }
        });

        let submitter_result =
            drain_pipelined_transfer(inner, submit_rx, free_tx, &poison, &mut on_safety_error);
        let _ = submitter_done_tx.send(());
        let producer_result = producer_handle.join().unwrap_or_else(|_| {
            Err(PoolWriteError::InvalidInput(
                "pipelined staging producer thread panicked".into(),
            ))
        });
        match submitter_result {
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
        }
    });
    let allocated = accounting.allocated.load(Ordering::Relaxed);
    let dropped = accounting.dropped.load(Ordering::Relaxed);
    if allocated != dropped {
        let imbalance = PoolWriteError::InvalidInput(format!(
            "staging ring accounting imbalance: allocated={allocated} dropped={dropped}"
        ));
        return match result {
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
    result
}

fn drain_pipelined_transfer<S: BlockSink + ?Sized>(
    inner: &mut S,
    rx: std_mpsc::Receiver<PipelinedSinkCommand>,
    free_tx: std_mpsc::SyncSender<PageAlignedBuffer>,
    poison: &Arc<Mutex<Option<String>>>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<(), PoolWriteError> {
    let mut completed_since_barrier: Option<WriteBatchOutcome> = None;
    while let Ok(command) = rx.recv() {
        if let Some(message) = staged_poison_message(poison) {
            discard_pipelined_command(command, &free_tx, message);
            continue;
        }
        let result = match command {
            PipelinedSinkCommand::WriteWindow(window) => {
                execute_pipelined_window(inner, window, &free_tx, on_safety_error).map(
                    |window_outcome| {
                        completed_since_barrier = Some(match completed_since_barrier {
                            Some(accumulated) => merge_batch_outcomes(accumulated, window_outcome),
                            None => window_outcome,
                        });
                    },
                )
            }
            PipelinedSinkCommand::Barrier { reply } => {
                let _ = reply.send(Ok(completed_since_barrier.take()));
                Ok(())
            }
            PipelinedSinkCommand::WriteFilemarks { count, reply } => {
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
        let result = inner.write_block_batch_pipelined(
            batch.buffer.bytes(),
            batch.block_size_bytes,
            &batch.cdb,
        );
        let buffer_return_error = return_ring_buffer(free_tx, batch.buffer).err();
        let outcome = match result {
            Ok(outcome) if outcome.records_written == requested && !outcome.end_of_medium => {
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
        PipelinedSinkCommand::SpaceToEndOfData { reply }
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
        stats.gap_p50_us = diagnostics.gap_p50_us;
        stats.gap_p95_us = diagnostics.gap_p95_us;
        stats.gap_max_us = diagnostics.gap_max_us;
        stats.ioctl_p50_us = diagnostics.ioctl_p50_us;
        stats.ioctl_p95_us = diagnostics.ioctl_p95_us;
        stats.ioctl_max_us = diagnostics.ioctl_max_us;
        stats.cadence_us = diagnostics.cadence_us;
        stats.effective_feed_bytes_per_second = diagnostics.effective_feed_bytes_per_second;
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
        self.live_write_counter.configure_tape_io(
            self.inner.staging_ring_buffers(),
            self.inner.write_batch_blocks(block_size_bytes),
        );
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
        self.live_write_counter
            .record_tape_io_diagnostics(self.inner.pipelined_write_diagnostics());
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
        let outcome = self.inner.write_filemarks(count)?;
        self.stats.record_filemarks(count, outcome.early_warning);
        Ok(outcome)
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let outcome = self.inner.write_filemarks_pipelined(count)?;
        self.stats.record_filemarks(count, outcome.early_warning);
        Ok(outcome)
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
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> =
        run_fenced_staged_transfer(state, &selected, sink, block_size as usize, |staged| {
            let mut raw = BlockSinkRawTapeSink::new(staged);
            let mut parity =
                ParitySink::new_sidecar_only(&mut raw, scheme.clone(), tape_uuid, block_size)?;
            parity.write_bootstrap()?;
            let report = match &stored {
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
            }?;
            Ok(report)
        });
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
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> =
        run_fenced_staged_transfer(
            state,
            &selected,
            sink,
            selected.block_size as usize,
            |staged| {
                if append.fresh_tape {
                    write_no_parity_bootstrap(
                        staged,
                        tape_uuid,
                        selected.block_size,
                        &prepared.write_timestamp,
                    )?;
                } else {
                    position_no_parity_append(staged)?;
                }
                let report = match &stored {
                    PreparedStoredObject::Plaintext => {
                        let mut readers = open_prepared_readers(&prepared.files)?;
                        let mut streams = Vec::with_capacity(prepared.files.len());
                        for (file, reader) in prepared.files.iter().zip(readers.iter_mut()) {
                            streams.push(RemTarFileStream::new(file.spec.clone(), reader));
                        }
                        let mut object_sink = ObjectDigestBlockSink::new(staged);
                        let layout = write_rem_tar_object_from_readers(
                            &mut object_sink,
                            &prepared.options,
                            &mut streams,
                        )
                        .map_err(StreamingError::from)?;
                        let object_digest = object_sink.finish_digest();
                        let filemark_outcome = staged.write_filemarks(1)?;
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
                        write_fixed_blocks(staged, prepared.options.chunk_size, &encrypted.sealed)?;
                        let filemark_outcome = staged.write_filemarks(1)?;
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
        key_id: projection
            .copy_representation
            .key_id
            .map(|key_id| key_id.to_vec()),
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
        write_report: Some(write_report),
    }
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
    source_file_size(&request.source_path)?;
    let requested_hash = sha256_file(&request.source_path)?;
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
    let key_id = copy
        .key_id
        .as_deref()
        .map(|value| {
            value
                .try_into()
                .map_err(|_| replay_object_invalid(&copy.object_id, "copy key_id is not 16 bytes"))
        })
        .transpose()?;
    Ok(PoolWriteObjectCopyRecord {
        tape_uuid,
        tape_file_number: u64::from(copy.tape_file_number),
        first_body_lba: copy.first_body_lba,
        pool_id: pool_id.to_string(),
        representation: copy.representation.clone(),
        key_id,
        metadata_frame_len: copy.metadata_frame_len,
    })
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
    _request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    prepared: &PreparedPoolObject,
    stored_projected_blocks: u64,
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
        position_calls = outcome.stats.position_calls,
        early_warning = outcome.stats.early_warning,
        scsi_write_cdb = "WRITE6_FIXED_BATCH",
        write_batch_blocks = outcome.stats.write_batch_blocks,
        effective_batch_blocks = outcome.stats.effective_batch_blocks,
        position_check_bytes = outcome.stats.position_check_bytes,
        staging_ring_buffers = outcome.stats.staging_ring_buffers,
        gap_p50_us = outcome.stats.gap_p50_us,
        gap_p95_us = outcome.stats.gap_p95_us,
        gap_max_us = outcome.stats.gap_max_us,
        ioctl_p50_us = outcome.stats.ioctl_p50_us,
        ioctl_p95_us = outcome.stats.ioctl_p95_us,
        ioctl_max_us = outcome.stats.ioctl_max_us,
        cadence_us = outcome.stats.cadence_us,
        effective_feed_bytes_per_second = outcome.stats.effective_feed_bytes_per_second,
        write_filemarks_immed = false,
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

fn position_no_parity_append(sink: &mut dyn BlockSink) -> Result<TapePosition, PoolWriteError> {
    sink.space_to_end_of_data().map_err(PoolWriteError::from)
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
        ordered_events: Arc<Mutex<Vec<String>>>,
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
                ordered_events: Arc::new(Mutex::new(Vec::new())),
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

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.events.push("position".to_string());
            if self.fail_position {
                return Err(TapeIoError::OperationFailed(
                    "injected READ POSITION failure".into(),
                ));
            }
            self.inner.position()
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
        let error = execute_pipelined_window(&mut sink, window, &free_tx, &mut |_error| {
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
        stats.record_filemarks(1, true);
        assert!(stats.early_warning);

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
                key_id: None,
                metadata_frame_len: None,
            }],
        };

        let proto = object.to_proto();
        let info = proto
            .append_commit_info
            .expect("append commit info from first copy");
        assert_eq!(info.append_mode, pb::AppendMode::Append as i32);
        assert_eq!(info.tape_uuid, vec![0x44; 16]);
        assert_eq!(info.tape_file_number, 3);
        assert_eq!(info.first_body_lba, 9);
        assert_eq!(info.position_before_lba, None);
        assert_eq!(info.position_after_lba, None);
        assert_eq!(info.journal_record_ordinal, None);
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
}
