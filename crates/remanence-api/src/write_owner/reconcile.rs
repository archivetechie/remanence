//! Changer/catalog reconciliation and its operation lifecycle projections.

use std::collections::{BTreeMap, HashMap};

use ciborium::value::Value as CborValue;
use remanence_parity::{
    scan_reconstruct_filemark_map, DriveHandleRawSource, FilemarkMap, TapeFileEntry, TapeFileKind,
};
use remanence_state::{AuditActor, AuditEvent, CatalogIndex};
use tonic::Status;
use uuid::Uuid;

use super::actor_runtime::WriteOwnerConfig;
use super::restore::verify_loaded_tape_identity;
use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::{load_tape_by_uuid, pb};

pub(crate) fn handle_reconcile(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
    handle: crate::operations::OperationHandle,
) {
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled before dispatch");
        return;
    }
    publish_running(&handle, &[("phase", "mount")]);
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        &handle,
        &tape_uuid,
        AuditEvent::OperationStarted,
        BTreeMap::new(),
    ) {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let library_serial = cfg.actor_library_serial.as_str();
    let lib = match cfg.report.library(library_serial) {
        Some(lib) => lib,
        None => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("open library: {err}"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    let mut drive = match load_tape_by_uuid(index, &mut library, &cfg.policy, &tape_uuid) {
        Ok(drive) => drive,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("mount tape: {err}"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    if let Err(status) = verify_loaded_tape_identity(&mut drive, &tape_uuid) {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            &format!("tape identity: {}", status.message()),
            &[("phase", "mount")],
        );
        return;
    }
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled after mount");
        return;
    }

    let tape = match index.get_tape(&tape_uuid) {
        Ok(Some(tape)) => tape,
        Ok(None) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                "tape not found in catalog",
                &[("phase", "scan")],
            );
            return;
        }
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("catalog lookup: {err}"),
                &[("phase", "scan")],
            );
            return;
        }
    };
    let Some(block_size) = tape
        .block_size
        .and_then(|block_size| u32::try_from(block_size).ok())
    else {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            "tape block size is unknown or outside u32 range",
            &[("phase", "scan")],
        );
        return;
    };

    publish_running(&handle, &[("phase", "scan")]);
    let scan = {
        let mut source = DriveHandleRawSource::new(&mut drive);
        scan_reconstruct_filemark_map(&mut source, &tape_uuid, block_size)
    };
    let scan = match scan {
        Ok(scan) => scan,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("scan filemark map: {err}"),
                &[("phase", "scan")],
            );
            return;
        }
    };
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled after scan");
        return;
    }

    match reconcile_tape_files(index, &tape_uuid, &scan, &handle) {
        Ok(report) => {
            let rebuilt = report.tape_files_rebuilt.to_string();
            let mut detail = BTreeMap::new();
            detail.insert("tape_files".to_string(), CborValue::Text(rebuilt.clone()));
            if let Err(err) = record_reconcile_event(
                index,
                cfg,
                &handle,
                &tape_uuid,
                AuditEvent::OperationFinished,
                detail,
            ) {
                fail_operation(
                    index,
                    cfg,
                    &handle,
                    &tape_uuid,
                    &format!("record operation finish audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(
                pb::OperationState::Succeeded,
                &[("phase", "complete"), ("tape_files", rebuilt.as_str())],
            );
        }
        Err(ReconcileExit::Cancelled(message)) => {
            cancel_operation(index, cfg, &handle, &tape_uuid, message.as_str());
        }
        Err(ReconcileExit::Failed(message)) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                message.as_str(),
                &[("phase", "project")],
            );
        }
    }
}

pub(crate) enum ReconcileExit {
    Cancelled(String),
    Failed(String),
}

pub(crate) fn reconcile_tape_files(
    index: &mut CatalogIndex,
    tape_uuid: &[u8; 16],
    scan: &FilemarkMap,
    handle: &crate::operations::OperationHandle,
) -> Result<remanence_state::TapeJournalIndexReport, ReconcileExit> {
    let existing = index
        .list_tape_files(tape_uuid)
        .map_err(|err| ReconcileExit::Failed(format!("list existing tape files: {err}")))?;
    let existing_object_ids = existing
        .into_iter()
        .filter(|entry| entry.kind == "object")
        .filter_map(|entry| entry.object_id.map(|id| (entry.tape_file_number, id)))
        .collect::<HashMap<_, _>>();

    let mut entries = Vec::with_capacity(scan.entries().len());
    for (idx, map_entry) in scan.entries().iter().enumerate() {
        if handle.is_cancelled() {
            return Err(ReconcileExit::Cancelled(format!(
                "cancelled after {} tape files",
                entries.len()
            )));
        }
        let mut entry = TapeFileEntry::from_map_entry(map_entry.clone());
        if map_entry.kind == TapeFileKind::Object {
            entry.object_id = existing_object_ids
                .get(&map_entry.tape_file_number)
                .cloned();
        }
        entries.push(entry);
        let count = (idx + 1).to_string();
        publish_running(
            handle,
            &[("phase", "project"), ("tape_files", count.as_str())],
        );
    }

    index
        .reconcile_tape_files_projection(
            *tape_uuid,
            &entries,
            scan.max_sidecar_end_exclusive(),
            scan.total_data_ordinals(),
        )
        .map_err(|err| ReconcileExit::Failed(format!("project tape files: {err}")))
}

pub(crate) fn fail_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        handle,
        tape_uuid,
        AuditEvent::OperationFailed,
        detail,
    ) {
        let audit_error = format!("{error_summary}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), progress);
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

pub(crate) fn cancel_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        handle,
        tape_uuid,
        AuditEvent::CancelledBeforeDispatch,
        detail,
    ) {
        let audit_error = format!("{detail_message}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), &[("phase", "audit")]);
    } else {
        handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", detail_message)],
        );
    }
}

pub(crate) fn publish_running(
    handle: &crate::operations::OperationHandle,
    progress: &[(&str, &str)],
) {
    handle.publish_state(pb::OperationState::Running, progress);
}

pub(crate) fn record_reconcile_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    if tape_uuid.iter().any(|byte| *byte != 0) {
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
    }
    let subject_id = if tape_uuid.iter().any(|byte| *byte != 0) {
        Some(Uuid::from_bytes(*tape_uuid).to_string())
    } else {
        Some(handle.op_id_uuid().to_string())
    };
    let subject_kind = if tape_uuid.iter().any(|byte| *byte != 0) {
        "tape"
    } else {
        "operation"
    };
    append_operation_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        OperationAuditInput {
            actor: AuditActor::System,
            operation_id: handle.op_id_uuid(),
            operation_kind: handle.operation_kind(),
            event,
            subject_kind,
            subject_id,
            idempotency_key: None,
            detail,
        },
    )
}
