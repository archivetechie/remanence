//! Catalog lookups, stream adapters, wire validation, and protobuf conversion.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::pin::Pin;

use remanence_format_driver::{
    ArchiveGapCause, ArchiveGapRange, DamageRange, DamageStatus, EntryCatalogSink, EntryKind,
    ForeignFormatRegistry, FormatError, NormalizedEntry, ScanIntegrityBasis, SourceRequirement,
};
use remanence_state::{
    AuditActor, CatalogIndex, CatalogUnitFilter, CatalogUnitRecord, DriveCorrelationRollupRecord,
    NativeObjectCopyRecord, NativeObjectFileRecord, NativeObjectRecord, OperationRecord,
    StateError, TapeFileRecord, TapePoolRecord, TapeRecord,
};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{Response, Status};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::pb;

const CATALOG_STREAM_BUFFER: usize = 32;

pub(crate) fn find_object_for_key(
    state: &ApiState,
    key: Option<pb::get_object_request::Key>,
) -> Result<Option<NativeObjectRecord>, Status> {
    match key.ok_or_else(|| Status::invalid_argument("missing object lookup key"))? {
        pb::get_object_request::Key::ObjectId(value) => {
            let object_id = decode_object_id(&value)?;
            state
                .index()?
                .get_native_object(object_id.as_str())
                .map_err(|err| Status::internal(err.to_string()))
        }
        pb::get_object_request::Key::ContentSha256(hash) => {
            find_object_by_content_sha256(state, hash.as_slice())
        }
        pb::get_object_request::Key::ContentDigest(digest) => {
            if digest.algorithm != remanence_state::DIGEST_ALGORITHM_SHA256 {
                return Err(Status::invalid_argument(
                    "content_digest.algorithm must be sha256",
                ));
            }
            find_object_by_content_sha256(state, digest.value.as_slice())
        }
        pb::get_object_request::Key::CallerObjectId(caller_id) => state
            .index()?
            .get_native_object_by_caller_object_id(caller_id.as_str())
            .map_err(|err| Status::internal(err.to_string())),
    }
}

pub(crate) fn find_object_by_content_sha256(
    state: &ApiState,
    hash: &[u8],
) -> Result<Option<NativeObjectRecord>, Status> {
    if hash.len() != 32 {
        return Err(Status::invalid_argument(
            "content SHA-256 lookup value must be exactly 32 bytes",
        ));
    }
    state
        .index()?
        .get_native_object_by_content_hash(hash)
        .map_err(|err| match err {
            StateError::AmbiguousCatalogLookup(message) => Status::failed_precondition(message),
            other => Status::internal(other.to_string()),
        })
}

pub(crate) fn find_copy_object_for_key(
    state: &ApiState,
    key: Option<pb::find_object_copies_request::Key>,
) -> Result<Option<NativeObjectRecord>, Status> {
    let get_key = match key.ok_or_else(|| Status::invalid_argument("missing object lookup key"))? {
        pb::find_object_copies_request::Key::ObjectId(value) => {
            pb::get_object_request::Key::ObjectId(value)
        }
        pb::find_object_copies_request::Key::ContentSha256(value) => {
            pb::get_object_request::Key::ContentSha256(value)
        }
        pb::find_object_copies_request::Key::CallerObjectId(value) => {
            pb::get_object_request::Key::CallerObjectId(value)
        }
        pb::find_object_copies_request::Key::ContentDigest(value) => {
            pb::get_object_request::Key::ContentDigest(value)
        }
    };
    find_object_for_key(state, Some(get_key))
}

pub(crate) fn catalog_unit_filter(value: i32) -> CatalogUnitFilter {
    if value == pb::CatalogUnitOriginFilter::NativeObjects as i32 {
        CatalogUnitFilter::NativeObjects
    } else if value == pb::CatalogUnitOriginFilter::ForeignArchives as i32 {
        CatalogUnitFilter::ForeignArchives
    } else {
        CatalogUnitFilter::All
    }
}

pub(crate) fn native_object_stream(
    index_path: PathBuf,
) -> Pin<Box<dyn Stream<Item = Result<pb::ObjectRecord, Status>> + Send + 'static>> {
    let (tx, rx) = tokio::sync::mpsc::channel(CATALOG_STREAM_BUFFER);
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<(), Status> {
            let index = CatalogIndex::open_read_only(&index_path)
                .map_err(|err| Status::internal(err.to_string()))?;
            index
                .for_each_native_object(|record| {
                    let item = object_record_to_proto(record);
                    send_stream_item(&tx, item)
                })
                .map_err(|err| Status::internal(err.to_string()))
        })();
        if let Err(status) = result {
            let _ = tx.blocking_send(Err(status));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

pub(crate) fn catalog_unit_stream(
    index_path: PathBuf,
    filter: CatalogUnitFilter,
) -> Pin<Box<dyn Stream<Item = Result<pb::CatalogUnit, Status>> + Send + 'static>> {
    let (tx, rx) = tokio::sync::mpsc::channel(CATALOG_STREAM_BUFFER);
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<(), Status> {
            let index = CatalogIndex::open_read_only(&index_path)
                .map_err(|err| Status::internal(err.to_string()))?;
            index
                .for_each_catalog_unit(filter, |record| {
                    let item = catalog_unit_to_proto(record);
                    send_stream_item(&tx, item)
                })
                .map_err(|err| Status::internal(err.to_string()))
        })();
        if let Err(status) = result {
            let _ = tx.blocking_send(Err(status));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

pub(crate) fn send_stream_item<T>(
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
    item: Result<T, Status>,
) -> ControlFlow<()> {
    let should_continue = match item {
        Ok(value) => tx.blocking_send(Ok(value)).is_ok(),
        Err(status) => {
            let _ = tx.blocking_send(Err(status));
            false
        }
    };
    if should_continue {
        ControlFlow::Continue(())
    } else {
        ControlFlow::Break(())
    }
}

pub(crate) async fn blocking_status<T, F>(work: F) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Status> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| Status::internal(format!("blocking task failed: {err}")))?
}

pub(crate) fn operation_to_proto(record: OperationRecord) -> Result<pb::OperationStatus, Status> {
    let operation_id = encode_uuid_text(record.operation_id.as_str())?;
    let error_summary = match record.state.as_str() {
        "failed" => record
            .error_summary
            .clone()
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or_else(|| "operation failed".to_string()),
        "completion_unknown" => "completion unknown".to_string(),
        _ => String::new(),
    };
    Ok(pb::OperationStatus {
        operation_id,
        operation_kind: record.operation_kind,
        state: operation_state(record.state.as_str()) as i32,
        created_at: timestamp_from_rfc3339(record.started_at_utc.as_str()),
        updated_at: timestamp_from_rfc3339(record.updated_at_utc.as_str()),
        progress: std::collections::HashMap::new(),
        error_summary,
    })
}

pub(crate) fn operation_state(value: &str) -> pb::OperationState {
    match value {
        "queued" => pb::OperationState::Queued,
        "running" | "cancel_requested" => pb::OperationState::Running,
        "finished" | "completed_after_cancel" => pb::OperationState::Succeeded,
        "failed" => pb::OperationState::Failed,
        "cancelled_before_dispatch" => pb::OperationState::Cancelled,
        "completion_unknown" => pb::OperationState::CompletionUnknown,
        _ => pb::OperationState::Unspecified,
    }
}

pub(crate) fn tape_to_proto(record: TapeRecord) -> pb::Tape {
    pb::Tape {
        tape_uuid: record.tape_uuid,
        // The record already carries all of these as Option, from nullable
        // columns. The line below for written_extent_lba was the only one that
        // let the absence through -- every other field flattened it here, one
        // line away, and the comment on that field explained why it must not.
        voltag: record.voltag,
        body_format: record.body_format,
        block_size_bytes: record.block_size,
        data_blocks_per_stripe: record.data_blocks_per_stripe,
        parity_blocks_per_stripe: record.parity_blocks_per_stripe,
        stripes_per_neighborhood: record.stripes_per_neighborhood,
        last_committed_tape_file: record.last_committed_tape_file,
        state: tape_state(record.state.as_str()) as i32,
        updated_at: timestamp_from_rfc3339(record.updated_at_utc.as_str()),
        pool_id: record.pool_id,
        correlation_rollups: Vec::new(),
        // Barrier-proved measurement; absent stays absent on the wire.
        written_extent_lba: record.written_extent_lba,
        kind: Some(record.kind),
        scheme_id: record.scheme_id,
        assignment_generation: record.assignment_generation,
    }
}

pub(crate) fn tape_to_proto_with_rollups(
    record: TapeRecord,
    rollups: Vec<DriveCorrelationRollupRecord>,
) -> pb::Tape {
    let mut tape = tape_to_proto(record);
    tape.correlation_rollups = rollups
        .into_iter()
        .map(correlation_rollup_to_proto)
        .collect();
    tape
}

pub(crate) fn correlation_rollup_to_proto(
    record: DriveCorrelationRollupRecord,
) -> pb::DriveCorrelationRollup {
    pb::DriveCorrelationRollup {
        tape_uuid: record.tape_uuid.unwrap_or_default(),
        voltag: record.voltag.unwrap_or_default(),
        drive_uuid: record.drive_uuid.unwrap_or_default(),
        drive_serial: record.drive_serial.unwrap_or_default(),
        session_count: u64::try_from(record.session_count).unwrap_or_default(),
        snapshot_count: u64::try_from(record.snapshot_count).unwrap_or_default(),
        write_errors_corrected: u64::try_from(record.write_errors_corrected).unwrap_or_default(),
        write_errors_uncorrected: u64::try_from(record.write_errors_uncorrected)
            .unwrap_or_default(),
        read_errors_corrected: u64::try_from(record.read_errors_corrected).unwrap_or_default(),
        read_errors_uncorrected: u64::try_from(record.read_errors_uncorrected).unwrap_or_default(),
        first_session_utc: record
            .first_session_utc
            .as_deref()
            .and_then(timestamp_from_rfc3339),
        last_session_utc: record
            .last_session_utc
            .as_deref()
            .and_then(timestamp_from_rfc3339),
    }
}

pub(crate) fn tape_state(value: &str) -> pb::tape::State {
    match value {
        "ingested" => pb::tape::State::TapeStateReady,
        "ready" => pb::tape::State::TapeStateReady,
        "sealed" => pb::tape::State::TapeStateSealed,
        "finalized" => pb::tape::State::TapeStateSealed,
        "finalizing" => pb::tape::State::TapeStateFinalizing,
        "finalized_degraded" => pb::tape::State::TapeStateFinalizedDegraded,
        "recovery_required" => pb::tape::State::TapeStateRecoveryRequired,
        "completion_unknown" => pb::tape::State::TapeStateCompletionUnknown,
        "ingestion_pending" => pb::tape::State::TapeStateInventoried,
        "degraded" => pb::tape::State::TapeStateDegraded,
        "failed" => pb::tape::State::TapeStateFailed,
        "retired" => pb::tape::State::TapeStateRetired,
        _ => pb::tape::State::TapeStateUnspecified,
    }
}

#[allow(deprecated)]
pub(crate) fn tape_finalization_to_proto(
    tape_uuid: [u8; 16],
    operation_id: Option<Uuid>,
    projection: remanence_state::TerminalFinalizationProjection,
) -> pb::TapeFinalization {
    use remanence_state::{
        TerminalFinalizationOutcome as StateOutcome, TerminalFinalizationProgress as StateProgress,
        TerminalFinalizationTrigger as StateTrigger,
    };

    let progress = match projection.progress {
        StateProgress::BeforeReplicaA => pb::TapeFinalizationProgress::BeforeReplicaA,
        StateProgress::AfterReplicaA => pb::TapeFinalizationProgress::AfterReplicaA,
        StateProgress::AfterSeparationAb => pb::TapeFinalizationProgress::AfterSeparationAb,
        StateProgress::AfterReplicaB => pb::TapeFinalizationProgress::AfterReplicaB,
        StateProgress::AfterSeparationBc => pb::TapeFinalizationProgress::AfterSeparationBc,
        StateProgress::AfterReplicaC => pb::TapeFinalizationProgress::AfterReplicaC,
    };
    let outcome = match projection.outcome {
        StateOutcome::InProgress => pb::TapeFinalizationOutcome::Finalizing,
        StateOutcome::Finalized => pb::TapeFinalizationOutcome::Finalized,
        StateOutcome::FinalizedDegraded => pb::TapeFinalizationOutcome::FinalizedDegraded,
        StateOutcome::RecoveryRequired => pb::TapeFinalizationOutcome::RecoveryRequired,
    };
    let trigger = match projection.trigger {
        StateTrigger::ReachedLowWatermark => "reached_low_watermark",
        StateTrigger::HardwareEarlyWarning => "hardware_early_warning",
        StateTrigger::OperatorCloseOut => "operator_close_out",
        StateTrigger::PoolCloseOut => "pool_close_out",
        StateTrigger::NoPendingObjectFits => "no_pending_object_fits",
    };
    let completed = projection.completed_replicas;
    let completion_unknown_ordinal = if matches!(projection.outcome, StateOutcome::RecoveryRequired)
    {
        match projection.progress {
            StateProgress::BeforeReplicaA => Some(1),
            StateProgress::AfterSeparationAb => Some(2),
            StateProgress::AfterSeparationBc => Some(3),
            StateProgress::AfterReplicaA
            | StateProgress::AfterReplicaB
            | StateProgress::AfterReplicaC => None,
        }
    } else {
        None
    };
    let replica_progress = (1u8..=3)
        .map(|ordinal| {
            let state = if ordinal <= completed {
                pb::tape_index_replica_progress::State::TapeIndexReplicaProgressStateBarrierProved
            } else if completion_unknown_ordinal == Some(ordinal) {
                pb::tape_index_replica_progress::State::TapeIndexReplicaProgressStateCompletionUnknown
            } else {
                pb::tape_index_replica_progress::State::TapeIndexReplicaProgressStatePending
            };
            pb::TapeIndexReplicaProgress {
                replica_ordinal: u32::from(ordinal),
                state: state as i32,
                detail: String::new(),
            }
        })
        .collect();
    pb::TapeFinalization {
        tape_uuid: tape_uuid.to_vec(),
        operation_id: operation_id
            .map(|operation_id| operation_id.as_bytes().to_vec())
            .unwrap_or_default(),
        progress: progress as i32,
        completed_replicas: u32::from(completed),
        replica_health: Vec::new(),
        replica_progress,
        edition_digest: projection.edition_digest.to_vec(),
        layout_digest: projection.layout_digest.to_vec(),
        outcome: outcome as i32,
        trigger: trigger.to_string(),
        detail: String::new(),
    }
}

pub(crate) fn tape_file_to_proto(record: TapeFileRecord) -> Result<pb::TapeFile, Status> {
    let canonical_metadata_digest = catalog_digest_to_proto(
        record.canonical_metadata_hash_algorithm,
        record.canonical_metadata_hash,
    );
    Ok(pb::TapeFile {
        tape_uuid: record.tape_uuid,
        tape_file_number: record.tape_file_number,
        kind: record.kind,
        block_count: record.block_count,
        object_id: record
            .object_id
            .as_deref()
            .map(encode_uuid_text)
            .transpose()?
            .unwrap_or_default(),
        canonical_metadata_digest,
    })
}

pub(crate) fn native_object_file_to_proto(
    record: NativeObjectFileRecord,
) -> Result<pb::FileRecord, Status> {
    let file_digest = Some(pb::Digest {
        algorithm: record.file_digest_algorithm,
        value: record.file_sha256.clone(),
    });
    Ok(pb::FileRecord {
        object_id: encode_uuid_text(record.object_id.as_str())?,
        file_id: record.file_id.into_bytes(),
        path: record.path,
        size_bytes: record.size_bytes,
        file_sha256: record.file_sha256,
        first_chunk_body_lba: record.first_chunk_lba,
        chunk_count: record.chunk_count,
        file_digest,
    })
}

pub(crate) fn tape_pool_to_proto(record: TapePoolRecord) -> pb::TapePool {
    pb::TapePool {
        pool_id: record.pool_id,
        display_name: record.display_name.unwrap_or_default(),
        copy_class: record.copy_class.unwrap_or_default(),
        content_class: record.content_class.unwrap_or_default(),
    }
}

pub(crate) fn object_record_to_proto(
    record: NativeObjectRecord,
) -> Result<pb::ObjectRecord, Status> {
    let append_commit_info = record
        .copies
        .first()
        .map(append_commit_info_from_native_copy);
    let content_digest = catalog_digest_to_proto(
        record.content_hash_algorithm.clone(),
        record.content_hash.clone(),
    );
    let metadata_digest =
        catalog_digest_to_proto(record.metadata_hash_algorithm, record.metadata_hash);
    Ok(pb::ObjectRecord {
        object_id: encode_uuid_text(record.object_id.as_str())?,
        caller_object_id: record.caller_object_id,
        content_sha256: record.content_hash,
        logical_size_bytes: record.logical_size_bytes,
        // NOTE: `objects.body_format` is nullable, but this record type carries
        // it as a plain String -- the same upstream flattening DriveRecord.serial
        // had. Every other field on this projection is now honest; making this
        // one honest means fixing the record, which is the remaining piece of
        // this area rather than something to paper over here.
        body_format: Some(record.body_format),
        caller_metadata: std::collections::HashMap::new(),
        created_at: timestamp_from_rfc3339(record.created_at_utc.as_str()),
        copies: record.copies.iter().map(object_copy_to_proto).collect(),
        append_commit_info,
        content_digest,
        metadata_digest,
    })
}

pub(crate) fn object_copy_to_proto(copy: &NativeObjectCopyRecord) -> pb::ObjectCopy {
    let health = if copy.status == "committed" {
        pb::object_copy::Health::ObjectCopyHealthOk
    } else {
        pb::object_copy::Health::ObjectCopyHealthSuspect
    };
    pb::ObjectCopy {
        tape_uuid: copy.tape_uuid.clone(),
        tape_file_number: copy.tape_file_number,
        first_body_lba: copy.first_body_lba,
        last_verified_at: None,
        health: health as i32,
        pool_id: copy.pool_id.clone().unwrap_or_default(),
        plaintext_digest: catalog_digest_to_proto(
            copy.plaintext_digest_algorithm.clone(),
            copy.plaintext_digest.clone(),
        ),
        stored_digest: catalog_digest_to_proto(
            copy.stored_digest_algorithm.clone(),
            copy.stored_digest.clone(),
        ),
        // The span through the copy→tape-file join. Absent stays absent on
        // the wire (proto3 optional): a copy whose tape file predates span
        // capture is unknown, never zero.
        global_start_block: copy.global_start_block,
        global_end_block: copy.global_end_block,
    }
}

pub(crate) fn catalog_digest_to_proto(
    algorithm: Option<String>,
    value: Option<Vec<u8>>,
) -> Option<pb::Digest> {
    algorithm
        .zip(value)
        .map(|(algorithm, value)| pb::Digest { algorithm, value })
}

pub(crate) fn append_mode_for_tape_file_number(tape_file_number: u64) -> pb::AppendMode {
    match tape_file_number {
        0 => pb::AppendMode::Unspecified,
        1 => pb::AppendMode::Fresh,
        _ => pb::AppendMode::Append,
    }
}

pub(crate) fn append_commit_info_from_native_copy(
    copy: &NativeObjectCopyRecord,
) -> pb::AppendCommitInfo {
    let tape_file_number = copy.tape_file_number;
    pb::AppendCommitInfo {
        append_mode: append_mode_for_tape_file_number(tape_file_number) as i32,
        tape_uuid: copy.tape_uuid.clone(),
        voltag: None,
        tape_file_number: Some(tape_file_number),
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

pub(crate) fn catalog_unit_to_proto(record: CatalogUnitRecord) -> Result<pb::CatalogUnit, Status> {
    let origin_kind = match record.origin_kind.as_str() {
        "native_object" => pb::CatalogUnitOriginKind::NativeObject,
        "foreign_archive" => pb::CatalogUnitOriginKind::ForeignArchive,
        other => {
            return Err(Status::internal(format!(
                "unknown catalog unit origin {other}"
            )))
        }
    };
    let origin = match origin_kind {
        pb::CatalogUnitOriginKind::NativeObject => {
            let object_id = record
                .native_object_id
                .as_deref()
                .ok_or_else(|| Status::internal("native catalog unit missing object id"))?;
            Some(pb::catalog_unit::Origin::Native(pb::NativeUnitSummary {
                object_id: encode_uuid_text(object_id)?,
            }))
        }
        pb::CatalogUnitOriginKind::ForeignArchive => Some(pb::catalog_unit::Origin::Foreign(
            pb::ForeignArchiveSummary {
                scan_id: record
                    .scan_id
                    .as_deref()
                    .map(encode_text_id)
                    .unwrap_or_default(),
                source_kind: record.source_kind.unwrap_or_default(),
                source_id: record.source_id.unwrap_or_default(),
                confidence: scan_confidence(record.confidence.as_deref()) as i32,
                last_scan_at: record
                    .last_scan_at_utc
                    .as_deref()
                    .and_then(timestamp_from_rfc3339),
                entry_count: record.entry_count.unwrap_or_default(),
                damage_event_count: record.damage_event_count.unwrap_or_default(),
            },
        )),
        pb::CatalogUnitOriginKind::Unspecified => None,
    };
    Ok(pb::CatalogUnit {
        unit_id: encode_text_id(record.unit_id.as_str()),
        tape_uuid: record.tape_uuid,
        format_id: record.format_id,
        origin_kind: origin_kind as i32,
        discovered_at: timestamp_from_rfc3339(record.created_at_utc.as_str()),
        origin,
    })
}

pub(crate) fn list_entries_for_unit(
    unit: CatalogUnitRecord,
    foreign_formats: &ForeignFormatRegistry,
) -> Result<Response<pb::ListEntriesInUnitResponse>, Status> {
    if unit.origin_kind != "foreign_archive" {
        return Err(Status::unimplemented(
            "native unit entry listing is not wired in this slice",
        ));
    }
    let adapter = foreign_formats
        .get(unit.format_id.as_str())
        .ok_or_else(|| {
            Status::unimplemented(format!(
                "foreign format {} is not registered in this distribution",
                unit.format_id
            ))
        })?;
    let source_kind = unit
        .source_kind
        .as_deref()
        .ok_or_else(|| Status::internal("foreign catalog unit missing source_kind"))?;
    if source_kind != "byte_stream_dump"
        || !adapter
            .supported_sources()
            .contains(&SourceRequirement::ByteStreamDump)
    {
        return Err(Status::unimplemented(format!(
            "foreign source kind {source_kind} is not supported by {}",
            adapter.id()
        )));
    }
    let source_id = unit
        .source_id
        .as_deref()
        .ok_or_else(|| Status::internal("foreign catalog unit missing source_id"))?;
    let file = std::fs::File::open(source_id)
        .map_err(|err| Status::internal(format!("open foreign dump source: {err}")))?;
    let mut reader = adapter
        .open_dump_reader(Box::new(file), &unit.adapter_state)
        .map_err(|err| Status::internal(format!("open foreign archive: {err}")))?;
    let mut collector = CatalogEntryCollector::new(encode_text_id(unit.unit_id.as_str()));
    let scan = reader
        .scan(&mut collector)
        .map_err(|err| Status::internal(format!("scan foreign archive: {err}")))?;
    collector.set_integrity_basis(catalog_integrity_basis(scan.integrity_basis));
    Ok(Response::new(pb::ListEntriesInUnitResponse {
        entries: collector.entries,
        next_page_token: None,
        archive_gaps: collector.archive_gaps,
    }))
}

pub(crate) struct CatalogEntryCollector {
    unit_id: Vec<u8>,
    entries: Vec<pb::CatalogEntry>,
    archive_gaps: Vec<pb::ArchiveGap>,
    positions: std::collections::HashMap<String, usize>,
    pending_states: std::collections::HashMap<String, pb::CatalogEntryState>,
}

impl CatalogEntryCollector {
    fn new(unit_id: Vec<u8>) -> Self {
        Self {
            unit_id,
            entries: Vec::new(),
            archive_gaps: Vec::new(),
            positions: std::collections::HashMap::new(),
            pending_states: std::collections::HashMap::new(),
        }
    }

    fn mark_state(&mut self, file_id: &str, state: pb::CatalogEntryState) {
        if let Some(position) = self.positions.get(file_id).copied() {
            self.entries[position].state = state as i32;
        } else {
            self.pending_states.insert(file_id.to_string(), state);
        }
    }

    fn set_integrity_basis(&mut self, basis: pb::IntegrityBasis) {
        for entry in &mut self.entries {
            entry.integrity_basis = basis as i32;
        }
    }
}

impl EntryCatalogSink for CatalogEntryCollector {
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        let file_id = entry.file_id.as_str().to_string();
        let state = self
            .pending_states
            .remove(file_id.as_str())
            .unwrap_or(pb::CatalogEntryState::Complete);
        self.positions.insert(file_id, self.entries.len());
        self.entries.push(normalized_entry_to_proto(
            self.unit_id.clone(),
            entry,
            state,
            pb::IntegrityBasis::Unknown,
        ));
        Ok(())
    }

    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        self.mark_state(
            range.file_id.as_str(),
            catalog_entry_state_for_damage(range.status),
        );
        Ok(())
    }

    fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        self.archive_gaps
            .push(archive_gap_to_proto(self.unit_id.clone(), range));
        Ok(())
    }
}

pub(crate) fn catalog_integrity_basis(basis: ScanIntegrityBasis) -> pb::IntegrityBasis {
    match basis {
        ScanIntegrityBasis::Unknown => pb::IntegrityBasis::Unknown,
        ScanIntegrityBasis::ContentHash => pb::IntegrityBasis::ContentHash,
        ScanIntegrityBasis::FormatChecksum => pb::IntegrityBasis::FormatChecksum,
        ScanIntegrityBasis::ParityConsistency => pb::IntegrityBasis::ParityConsistency,
    }
}

pub(crate) fn normalized_entry_to_proto(
    unit_id: Vec<u8>,
    entry: &NormalizedEntry,
    state: pb::CatalogEntryState,
    integrity_basis: pb::IntegrityBasis,
) -> pb::CatalogEntry {
    pb::CatalogEntry {
        unit_id,
        entry_id: encode_text_id(entry.file_id.as_str()),
        path: entry.path.clone(),
        kind: catalog_entry_kind(entry.kind) as i32,
        size_bytes: entry.size_bytes,
        mtime: None,
        state: state as i32,
        integrity_basis: integrity_basis as i32,
    }
}

pub(crate) fn catalog_entry_kind(kind: EntryKind) -> pb::CatalogEntryKind {
    match kind {
        EntryKind::RegularFile => pb::CatalogEntryKind::RegularFile,
        EntryKind::Directory => pb::CatalogEntryKind::Directory,
        EntryKind::Symlink => pb::CatalogEntryKind::Symlink,
        EntryKind::Hardlink => pb::CatalogEntryKind::Hardlink,
        EntryKind::Special => pb::CatalogEntryKind::Special,
    }
}

pub(crate) fn catalog_entry_state_for_damage(status: DamageStatus) -> pb::CatalogEntryState {
    match status {
        DamageStatus::ChecksumFailed | DamageStatus::ReadError => pb::CatalogEntryState::Damaged,
        DamageStatus::Missing => pb::CatalogEntryState::Partial,
        DamageStatus::Unsupported => pb::CatalogEntryState::Unsupported,
    }
}

pub(crate) fn archive_gap_to_proto(unit_id: Vec<u8>, range: &ArchiveGapRange) -> pb::ArchiveGap {
    pb::ArchiveGap {
        unit_id,
        source_start: range.source_start,
        source_end: range.source_end,
        cause: archive_gap_cause(range.cause) as i32,
    }
}

pub(crate) fn archive_gap_cause(cause: ArchiveGapCause) -> pb::ArchiveGapCause {
    match cause {
        ArchiveGapCause::UnrecognizedData => pb::ArchiveGapCause::UnrecognizedData,
        ArchiveGapCause::ReadError => pb::ArchiveGapCause::ReadError,
        ArchiveGapCause::Missing => pb::ArchiveGapCause::Missing,
        ArchiveGapCause::Resync => pb::ArchiveGapCause::Resync,
        ArchiveGapCause::Unsupported => pb::ArchiveGapCause::Unsupported,
    }
}

pub(crate) fn scan_confidence(value: Option<&str>) -> pb::CatalogScanConfidence {
    match value {
        Some("low") => pb::CatalogScanConfidence::Low,
        Some("medium") => pb::CatalogScanConfidence::Medium,
        Some("high") => pb::CatalogScanConfidence::High,
        _ => pb::CatalogScanConfidence::Unspecified,
    }
}

pub(crate) fn encode_text_id(value: &str) -> Vec<u8> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.as_bytes().to_vec())
        .unwrap_or_else(|_| value.as_bytes().to_vec())
}

pub(crate) fn decode_object_id(value: &[u8]) -> Result<String, Status> {
    let uuid = decode_uuid_bytes(value, "object_id")?;
    Ok(Uuid::from_bytes(uuid).to_string())
}

pub(crate) fn decode_uuid_bytes(value: &[u8], field: &str) -> Result<[u8; 16], Status> {
    value.try_into().map_err(|_| {
        Status::invalid_argument(format!("{field} must be a 16-byte UUID byte string"))
    })
}

pub(crate) fn decode_optional_idempotency(
    value: Option<&pb::IdempotencyKey>,
) -> Result<Option<Uuid>, Status> {
    value
        .filter(|key| !key.value.is_empty())
        .map(|key| decode_uuid_bytes(key.value.as_slice(), "idempotency_key.value"))
        .transpose()
        .map(|uuid| uuid.map(Uuid::from_bytes))
}

pub(crate) fn decode_required_idempotency(
    value: Option<&pb::IdempotencyKey>,
    rpc: &str,
) -> Result<Uuid, Status> {
    decode_optional_idempotency(value)?.ok_or_else(|| {
        Status::invalid_argument(format!(
            "{rpc} requires a non-empty 16-byte idempotency_key"
        ))
    })
}

pub(crate) fn audit_actor_fingerprint(actor: &AuditActor) -> String {
    match actor {
        AuditActor::System => "system".to_string(),
        AuditActor::User(identity) => format!("user:{identity}"),
        AuditActor::Service(identity) => format!("service:{identity}"),
    }
}

/// Hash the exact state-changing request independently of its retry key.
/// Length-prefixing prevents optional/present-empty and concatenation aliases;
/// pool identifiers are canonicalized by validation, while reason bytes are
/// deliberately preserved exactly for audit and retry conflict detection.
pub(crate) fn manual_finalize_request_fingerprint(
    tape_uuid: [u8; 16],
    expected_pool_id: Option<&str>,
    actor_fingerprint: &str,
    reason: &[u8],
) -> [u8; 32] {
    const DOMAIN: &[u8] = b"REM-FINALIZE-TAPE-REQUEST-V1\0";
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(tape_uuid);
    match expected_pool_id {
        Some(pool_id) => {
            hasher.update([1]);
            hasher.update((pool_id.len() as u64).to_be_bytes());
            hasher.update(pool_id.as_bytes());
        }
        None => hasher.update([0]),
    }
    // Stable code for TerminalFinalizationTrigger::OperatorCloseOut.
    hasher.update([4]);
    hasher.update((actor_fingerprint.len() as u64).to_be_bytes());
    hasher.update(actor_fingerprint.as_bytes());
    hasher.update((reason.len() as u64).to_be_bytes());
    hasher.update(reason);
    hasher.finalize().into()
}

pub(crate) fn reject_unimplemented_idempotency(
    value: Option<&pb::IdempotencyKey>,
    rpc: &str,
) -> Result<(), Status> {
    if decode_optional_idempotency(value)?.is_some() {
        return Err(Status::unimplemented(format!(
            "{rpc} idempotency_key replay is not wired yet"
        )));
    }
    Ok(())
}

pub(crate) fn encode_uuid_text(value: &str) -> Result<Vec<u8>, Status> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.as_bytes().to_vec())
        .map_err(|err| Status::internal(format!("stored UUID text is not a UUID: {err}")))
}

pub(crate) fn decode_text_id(value: &[u8], field: &str) -> Result<String, Status> {
    String::from_utf8(value.to_vec())
        .map_err(|err| Status::invalid_argument(format!("{field} is not utf-8: {err}")))
}

pub(crate) fn timestamp_from_rfc3339(value: &str) -> Option<prost_types::Timestamp> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    Some(prost_types::Timestamp {
        seconds: parsed.unix_timestamp(),
        nanos: parsed.nanosecond() as i32,
    })
}

pub(crate) fn alarm_record_to_proto(record: remanence_state::AlarmRecord) -> pb::Alarm {
    pb::Alarm {
        alarm_id: u64::try_from(record.alarm_id).unwrap_or_default(),
        condition_key: record.condition_key,
        kind: record.kind,
        severity: record.severity,
        state: record.state,
        first_seen_utc: timestamp_from_rfc3339(record.first_seen_utc.as_str()),
        last_seen_utc: timestamp_from_rfc3339(record.last_seen_utc.as_str()),
        acked_by: record.acked_by.unwrap_or_default(),
        acked_at_utc: record
            .acked_at_utc
            .as_deref()
            .and_then(timestamp_from_rfc3339),
        detail: record.detail.unwrap_or_default(),
    }
}
