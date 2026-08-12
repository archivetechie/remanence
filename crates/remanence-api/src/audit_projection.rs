//! Durable audit append and catalog-projection helpers.

use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use ciborium::value::Value as CborValue;
use remanence_state::{
    AlarmRecord, AuditActor, AuditEvent, AuditEventRecord, AuditRecord, AuditSubject, CatalogIndex,
    DriveHealthSnapshotRecord, FileAuditLog, SourceLayer,
};
use tonic::Status;
use uuid::Uuid;

use crate::hex_encoding::bytes_to_hex;
use crate::startup_media_readiness::status_from_state_error;

pub(crate) const FINALIZE_TAPE_OPERATION_KIND: &str = "finalize_tape";

pub(crate) struct OperationAuditInput<'a> {
    pub(crate) actor: AuditActor,
    pub(crate) operation_id: Uuid,
    pub(crate) operation_kind: &'a str,
    pub(crate) event: AuditEvent,
    pub(crate) subject_kind: &'a str,
    pub(crate) subject_id: Option<String>,
    pub(crate) idempotency_key: Option<Uuid>,
    pub(crate) detail: BTreeMap<String, CborValue>,
}

pub(crate) struct ProjectedAuditInput<'a> {
    pub(crate) actor: AuditActor,
    pub(crate) source_layer: SourceLayer,
    pub(crate) operation_id: Option<Uuid>,
    pub(crate) session_id: Option<Uuid>,
    pub(crate) idempotency_key: Option<Uuid>,
    pub(crate) event: AuditEvent,
    pub(crate) subject_kind: &'a str,
    pub(crate) subject_id: Option<String>,
    pub(crate) detail: BTreeMap<String, CborValue>,
}

/// Append one audit fact and immediately feed that exact record through the
/// catalog projector. All API-side evidence writers share this funnel so live
/// state and rebuild replay cannot drift through duplicated append logic.
pub(crate) fn append_and_project_audit(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_fsync: bool,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    input: ProjectedAuditInput<'_>,
) -> Result<AuditRecord, Status> {
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("audit append lock poisoned"))?;
    append_and_project_audit_locked(index, audit_dir, audit_fsync, input)
}

pub(crate) fn append_and_project_audit_locked(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_fsync: bool,
    input: ProjectedAuditInput<'_>,
) -> Result<AuditRecord, Status> {
    fs::create_dir_all(audit_dir).map_err(|err| {
        Status::internal(format!(
            "create audit directory {}: {err}",
            audit_dir.display()
        ))
    })?;
    let mut audit = FileAuditLog::open(audit_dir, audit_fsync)
        .map_err(|err| Status::internal(err.to_string()))?;
    let (_, record) = audit
        .append_and_return_record(AuditEventRecord {
            actor: input.actor,
            source_layer: input.source_layer,
            operation_id: input.operation_id,
            session_id: input.session_id,
            idempotency_key: input.idempotency_key,
            event: input.event,
            subject: AuditSubject {
                kind: input.subject_kind.to_string(),
                id: input.subject_id,
            },
            detail: input.detail,
        })
        .map_err(status_from_state_error)?;
    index
        .project_audit_record(&record)
        .map_err(status_from_state_error)?;
    Ok(record)
}

/// Ensure the durable manual-finalization completion event exists exactly once.
///
/// The sealed checkpoint carries the original operation identity, so both
/// startup replay and an explicit retry can repair a crash after final SQLite
/// projection without a drive or changer. The daemon's process-lifetime state
/// lock excludes other state owners, while this append lock spans the
/// in-process read-before-append decision. An already durable record is
/// reprojected instead of duplicated, and a new completion record always
/// forces fsync.
pub(crate) fn ensure_manual_finalize_finished_audit(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    tape_uuid: [u8; 16],
    finalization: &remanence_state::TerminalFinalizationIntent,
) -> Result<(), Status> {
    let Some(manual) = finalization.manual.as_ref() else {
        return Ok(());
    };
    if finalization.tape_uuid != tape_uuid
        || finalization.trigger != remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        || finalization.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC
        || finalization.recovery_required
        || manual.operation_kind != FINALIZE_TAPE_OPERATION_KIND
    {
        return Err(Status::failed_precondition(
            "sealed manual finalization has invalid completion audit authority",
        ));
    }
    let operation_id = Uuid::from_bytes(manual.operation_id);
    let idempotency_key = Uuid::from_bytes(manual.idempotency_key);
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("audit append lock poisoned"))?;
    fs::create_dir_all(audit_dir).map_err(|error| {
        Status::internal(format!(
            "create audit directory {}: {error}",
            audit_dir.display()
        ))
    })?;

    let scope = index
        .idempotency_scope_record(
            manual.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
        )
        .map_err(status_from_state_error)?
        .ok_or_else(|| {
            Status::failed_precondition(
                "sealed manual finalization has no projected idempotency binding",
            )
        })?;
    if scope.operation_id != operation_id
        || scope.request_fingerprint.as_slice() != manual.request_fingerprint
    {
        return Err(Status::failed_precondition(
            "sealed manual finalization differs from its idempotency binding",
        ));
    }

    let mut durable = None;
    let mut durable_count = 0usize;
    let mut conflict = None;
    let mut incompatible_terminal = None;
    let mut latest_completion_unknown_sequence = None;
    FileAuditLog::replay_incremental(audit_dir, |record| {
        if record.operation_id != Some(operation_id) {
            return ControlFlow::Continue(());
        }
        match &record.event {
            AuditEvent::OperationFinished => {
                if manual_finalize_finished_audit_matches(
                    &record,
                    tape_uuid,
                    idempotency_key,
                    manual,
                ) {
                    durable_count += 1;
                    durable.get_or_insert(record);
                } else {
                    conflict.get_or_insert(record);
                }
            }
            AuditEvent::CompletionUnknown => {
                if manual_finalize_completion_unknown_audit_matches(
                    &record,
                    tape_uuid,
                    idempotency_key,
                    manual,
                ) {
                    latest_completion_unknown_sequence = Some(record.sequence);
                } else {
                    incompatible_terminal.get_or_insert(record);
                }
            }
            AuditEvent::OperationFailed
            | AuditEvent::CancelledBeforeDispatch
            | AuditEvent::CompletedAfterCancel => {
                incompatible_terminal.get_or_insert(record);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    })
    .map_err(status_from_state_error)?;
    if let Some(record) = incompatible_terminal {
        return Err(Status::failed_precondition(format!(
            "operation {operation_id} has incompatible durable terminal audit record {} ({:?})",
            record.record_uuid, record.event
        )));
    }
    if let Some(record) = conflict {
        return Err(Status::failed_precondition(format!(
            "operation {} has conflicting durable OperationFinished audit record {}",
            operation_id, record.record_uuid
        )));
    }
    if durable_count > 1 {
        return Err(Status::failed_precondition(format!(
            "operation {operation_id} has {durable_count} durable OperationFinished audit records"
        )));
    }
    if let Some(record) = durable {
        if latest_completion_unknown_sequence.is_some_and(|sequence| sequence > record.sequence) {
            return Err(Status::failed_precondition(format!(
                "operation {operation_id} has CompletionUnknown authority after its durable OperationFinished record"
            )));
        }
        index
            .project_audit_record(&record)
            .map_err(status_from_state_error)?;
        return Ok(());
    }

    match scope.terminal_state.as_deref() {
        None | Some("completion_unknown") => {}
        Some("finished") => {
            return Err(Status::failed_precondition(
                "manual finalization is projected finished without its durable completion audit",
            ));
        }
        Some(state) => {
            return Err(Status::failed_precondition(format!(
                "manual finalization has incompatible projected terminal state {state:?}"
            )));
        }
    }

    append_and_project_audit_locked(
        index,
        audit_dir,
        true,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(operation_id),
            session_id: None,
            idempotency_key: Some(idempotency_key),
            event: AuditEvent::OperationFinished,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(tape_uuid).to_string()),
            detail: BTreeMap::from([
                (
                    "tape_uuid".to_string(),
                    CborValue::Bytes(tape_uuid.to_vec()),
                ),
                (
                    "actor_fingerprint".to_string(),
                    CborValue::Text(manual.actor_fingerprint.clone()),
                ),
                (
                    "finalization_progress".to_string(),
                    CborValue::Text("after_replica_c".to_string()),
                ),
                (
                    "operation_kind".to_string(),
                    CborValue::Text(FINALIZE_TAPE_OPERATION_KIND.to_string()),
                ),
            ]),
        },
    )?;
    Ok(())
}

pub(crate) fn manual_finalize_finished_audit_matches(
    record: &AuditRecord,
    tape_uuid: [u8; 16],
    idempotency_key: Uuid,
    manual: &remanence_state::ManualTerminalFinalizationIdentity,
) -> bool {
    manual_finalize_audit_identity_matches(record, tape_uuid, idempotency_key, manual)
        && record.detail.len() == 4
        && matches!(
            record.detail.get("finalization_progress"),
            Some(CborValue::Text(value)) if value == "after_replica_c"
        )
}

pub(crate) fn manual_finalize_completion_unknown_audit_matches(
    record: &AuditRecord,
    tape_uuid: [u8; 16],
    idempotency_key: Uuid,
    manual: &remanence_state::ManualTerminalFinalizationIdentity,
) -> bool {
    manual_finalize_audit_identity_matches(record, tape_uuid, idempotency_key, manual)
        && record.detail.len() == 5
        && matches!(
            record.detail.get("finalization_progress"),
            Some(CborValue::Text(value))
                if matches!(
                    value.as_str(),
                    "before_replica_a"
                        | "after_replica_a"
                        | "after_separation_ab"
                        | "after_replica_b"
                        | "after_separation_bc"
                        | "after_replica_c"
                )
        )
        && matches!(
            record.detail.get("recovery_detail"),
            Some(CborValue::Text(value)) if !value.is_empty()
        )
}

pub(crate) fn manual_finalize_audit_identity_matches(
    record: &AuditRecord,
    tape_uuid: [u8; 16],
    idempotency_key: Uuid,
    manual: &remanence_state::ManualTerminalFinalizationIdentity,
) -> bool {
    record.actor == AuditActor::System
        && record.source_layer == SourceLayer::Layer5
        && record.session_id.is_none()
        && record.idempotency_key == Some(idempotency_key)
        && audit_subject_matches_tape(record, tape_uuid)
        && matches!(
            record.detail.get("operation_kind"),
            Some(CborValue::Text(value)) if value == FINALIZE_TAPE_OPERATION_KIND
        )
        && matches!(
            record.detail.get("actor_fingerprint"),
            Some(CborValue::Text(value)) if value == &manual.actor_fingerprint
        )
        && matches!(
            record.detail.get("tape_uuid"),
            Some(CborValue::Bytes(value)) if value.as_slice() == tape_uuid
        )
}

/// Ensure the catalog-evidence audit contains one fsynced tape-sealed fact.
pub(crate) fn ensure_tape_sealed_audit(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("audit append lock poisoned"))?;
    fs::create_dir_all(audit_dir).map_err(|error| {
        Status::internal(format!(
            "create audit directory {}: {error}",
            audit_dir.display()
        ))
    })?;
    let mut durable = None;
    let mut durable_count = 0usize;
    let mut conflict = None;
    FileAuditLog::replay_incremental(audit_dir, |record| {
        if record.event != AuditEvent::TapeSealed || !audit_subject_matches_tape(&record, tape_uuid)
        {
            return ControlFlow::Continue(());
        }
        if tape_sealed_audit_matches(&record, tape_uuid) {
            durable_count += 1;
            durable.get_or_insert(record);
        } else {
            conflict.get_or_insert(record);
        }
        ControlFlow::Continue(())
    })
    .map_err(status_from_state_error)?;
    if let Some(record) = conflict {
        return Err(Status::failed_precondition(format!(
            "tape {} has conflicting durable TapeSealed audit record {}",
            Uuid::from_bytes(tape_uuid),
            record.record_uuid
        )));
    }
    if durable_count > 1 {
        return Err(Status::failed_precondition(format!(
            "tape {} has {durable_count} durable TapeSealed audit records",
            Uuid::from_bytes(tape_uuid)
        )));
    }
    if let Some(record) = durable {
        index
            .project_audit_record(&record)
            .map_err(status_from_state_error)?;
        return Ok(());
    }
    append_and_project_audit_locked(
        index,
        audit_dir,
        true,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::TapeSealed,
            subject_kind: "tape",
            subject_id: Some(bytes_to_hex(tape_uuid.as_slice())),
            detail: BTreeMap::new(),
        },
    )?;
    Ok(())
}

pub(crate) fn audit_subject_matches_tape(record: &AuditRecord, tape_uuid: [u8; 16]) -> bool {
    record.subject.kind == "tape"
        && record.subject.id.as_deref().is_some_and(|value| {
            Uuid::parse_str(value).is_ok_and(|parsed| parsed.as_bytes() == &tape_uuid)
        })
}

pub(crate) fn tape_sealed_audit_matches(record: &AuditRecord, tape_uuid: [u8; 16]) -> bool {
    record.actor == AuditActor::System
        && record.source_layer == SourceLayer::Layer4
        && record.operation_id.is_none()
        && record.session_id.is_none()
        && record.idempotency_key.is_none()
        && record.detail.is_empty()
        && audit_subject_matches_tape(record, tape_uuid)
}

pub(crate) fn drive_health_audit_detail(
    index: &CatalogIndex,
    snapshot: &DriveHealthSnapshotRecord,
) -> Result<BTreeMap<String, CborValue>, Status> {
    let mut detail = BTreeMap::from([
        (
            "drive_uuid".to_string(),
            CborValue::Bytes(snapshot.drive_uuid.clone()),
        ),
        (
            "at_utc".to_string(),
            CborValue::Text(snapshot.at_utc.clone()),
        ),
        (
            "trigger".to_string(),
            CborValue::Text(snapshot.trigger.clone()),
        ),
    ]);
    for (key, value) in [
        ("session_id", snapshot.session_id.as_ref()),
        ("tape_alert_flags", snapshot.tape_alert_flags.as_ref()),
        ("raw_pages", snapshot.raw_pages.as_ref()),
    ] {
        if let Some(value) = value {
            detail.insert(key.to_string(), CborValue::Text(value.clone()));
        }
    }
    for (key, value) in [
        ("write_errors_corrected", snapshot.write_errors_corrected),
        (
            "write_errors_uncorrected",
            snapshot.write_errors_uncorrected,
        ),
        ("read_errors_corrected", snapshot.read_errors_corrected),
        ("read_errors_uncorrected", snapshot.read_errors_uncorrected),
    ] {
        if let Some(value) = value {
            detail.insert(key.to_string(), CborValue::Integer(value.into()));
        }
    }
    if let Some(drive) = index
        .get_drive_by_uuid(snapshot.drive_uuid.as_slice())
        .map_err(status_from_state_error)?
    {
        // Only assert a serial when there is one. An absent key is not the
        // same claim as a key whose value is the empty string.
        if let Some(serial) = drive.serial {
            detail.insert("drive_serial".to_string(), CborValue::Text(serial));
        }
        detail.insert("managed".to_string(), CborValue::Text(drive.managed));
        for (key, value) in [
            ("vendor", drive.vendor.as_ref()),
            ("product", drive.product.as_ref()),
            ("firmware_rev", drive.firmware_rev.as_ref()),
        ] {
            if let Some(value) = value {
                detail.insert(key.to_string(), CborValue::Text(value.clone()));
            }
        }
    }
    Ok(detail)
}

pub(crate) fn alarm_audit_detail(alarm: &AlarmRecord) -> BTreeMap<String, CborValue> {
    let mut detail = BTreeMap::from([
        ("kind".to_string(), CborValue::Text(alarm.kind.clone())),
        (
            "severity".to_string(),
            CborValue::Text(alarm.severity.clone()),
        ),
    ]);
    if let Some(value) = alarm.detail.as_ref() {
        detail.insert("detail".to_string(), CborValue::Text(value.clone()));
    }
    detail
}

pub(crate) fn append_operation_audit(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_fsync: bool,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    input: OperationAuditInput<'_>,
) -> Result<(), Status> {
    let mut detail = input.detail;
    detail
        .entry("operation_kind".to_string())
        .or_insert_with(|| CborValue::Text(input.operation_kind.to_string()));
    append_and_project_audit(
        index,
        audit_dir,
        audit_fsync,
        audit_append_lock,
        ProjectedAuditInput {
            actor: input.actor,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(input.operation_id),
            session_id: None,
            idempotency_key: input.idempotency_key,
            event: input.event,
            subject_kind: input.subject_kind,
            subject_id: input.subject_id,
            detail,
        },
    )?;
    Ok(())
}
