//! Changer motion, load sequencing, snapshot publication, and library-operation audit.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use ciborium::value::Value as CborValue;
use remanence_library::{
    classify_media_readiness_error_ref, DriveOpError, LoadError, MediaFamily, MediaReadiness,
    MediaReadinessWaitOptions,
};
use remanence_state::{AuditActor, AuditEvent, CatalogIndex};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tonic::Status;

use super::actor_protocol::RoboticsAction;
use super::actor_runtime::{
    clear_alarm_with_evidence, raise_alarm_with_evidence, WriteOwnerConfig,
};
use super::cleaning::run_cleaning_sequence;
use super::readiness::{
    poll_drive_media_readiness, record_session_open_readiness_poll_transition,
    session_open_media_family, session_open_readiness_state, session_open_readiness_summary,
};
use super::reconcile::publish_running;
use super::{LOAD_READY_POLL_INTERVAL, LOAD_READY_TIMEOUT};
use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::drive_collection::observe_drive_catalog_from_libraries;
use crate::pb;

pub(crate) fn handle_robotics(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: String,
    action: RoboticsAction,
    handle: crate::operations::OperationHandle,
) {
    if library_serial != cfg.actor_library_serial {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!(
                "robotics request for library {library_serial} reached actor for {}",
                cfg.actor_library_serial
            ),
            &[("phase", "routing")],
        );
        return;
    }
    if handle.is_cancelled() {
        cancel_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            "cancelled before dispatch",
        );
        return;
    }
    publish_running(&handle, &[("phase", "open")]);
    if let Err(err) = record_library_event(
        index,
        cfg,
        &handle,
        &library_serial,
        AuditEvent::OperationStarted,
        robotics_detail(&action),
    ) {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let lib = match cfg.report.library(&library_serial) {
        Some(lib) => lib,
        None => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "open")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("open library: {err}"),
                &[("phase", "open")],
            );
            return;
        }
    };

    publish_running(&handle, &[("phase", "refresh")]);
    if let Err(err) = library.refresh() {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("refresh inventory: {err}"),
            &[("phase", "refresh")],
        );
        return;
    }

    publish_running(&handle, &[("phase", "execute")]);
    let action_result = match &action {
        RoboticsAction::Refresh => Ok(()),
        RoboticsAction::Move { src, dst } => library
            .move_medium(*src, *dst, &cfg.policy)
            .map_err(|err| err.to_string()),
        RoboticsAction::Load {
            slot,
            bay,
            wait_ready,
        } => run_load_sequence(index, cfg, &handle, &mut library, *slot, *bay, *wait_ready),
        RoboticsAction::Unload { bay, destination } => library
            .unload(*bay, *destination, &cfg.policy)
            .map_err(|err| err.to_string()),
        RoboticsAction::Clean {
            drive_uuid,
            trigger,
        } => run_cleaning_sequence(
            index,
            cfg,
            &handle,
            &mut library,
            drive_uuid.as_slice(),
            trigger.as_str(),
        )
        .map_err(|err| err.to_string()),
    };

    let observe_result = observe_refreshed_library(index, cfg, library.library())
        .map_err(|err| err.message().to_string());
    publish_library_snapshot(&cfg.library_snapshot, library.library().clone());

    match (action_result, observe_result) {
        (Ok(()), Ok(())) => {
            if let Err(err) = record_library_event(
                index,
                cfg,
                &handle,
                &library_serial,
                AuditEvent::OperationFinished,
                BTreeMap::new(),
            ) {
                fail_library_operation(
                    index,
                    cfg,
                    &handle,
                    &library_serial,
                    &format!("record operation finish audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(pb::OperationState::Succeeded, &[("phase", "done")]);
        }
        (Ok(()), Err(message)) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("observe refreshed drive catalog: {message}"),
                &[("phase", "catalog")],
            );
        }
        (Err(message), _) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                message.as_str(),
                &[("phase", "execute")],
            );
        }
    }
}

pub(crate) fn run_load_sequence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    slot: u16,
    bay: u16,
    wait_ready: bool,
) -> Result<(), String> {
    if !wait_ready {
        return library
            .load(slot, bay, &cfg.policy)
            .map_err(|error| error.to_string());
    }

    let barcode = library
        .library()
        .slots
        .iter()
        .find(|candidate| candidate.element_address == slot)
        .and_then(|candidate| candidate.cartridge.clone());
    let drive_bay = library
        .library()
        .drive_bays
        .iter()
        .find(|candidate| candidate.element_address == bay);
    let drive_serial = drive_bay
        .and_then(|candidate| candidate.installed.as_ref())
        .map(|drive| drive.serial.clone());
    let drive_sg = drive_bay
        .and_then(|candidate| candidate.installed.as_ref())
        .and_then(|drive| drive.sg_path.as_ref())
        .map(|path| path.display().to_string());
    let family = session_open_media_family(barcode.as_deref());
    let retryable_load_completion = match library.load(slot, bay, &cfg.policy) {
        Ok(()) => None,
        Err(error) => match retryable_readiness_from_load_error(&error, family) {
            Some(readiness) => Some(readiness),
            None => return Err(error.to_string()),
        },
    };

    let operation_id = handle.op_id_uuid();
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: library.library().serial.clone(),
            changer_sg: Some(library.library().changer_sg.display().to_string()),
            drive_element: bay,
            drive_sg,
            drive_serial,
            barcode,
            source_slot: Some(slot),
            media_generation: library
                .library()
                .drive_bays
                .iter()
                .find(|candidate| candidate.element_address == bay)
                .and_then(|candidate| candidate.loaded_tape.as_deref())
                .and_then(crate::lto_generation_from_voltag)
                .map(|generation| generation.generation_number()),
            phase: "load_drive_readiness".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: OffsetDateTime::now_utc()
                .checked_add(Duration::seconds(9_000))
                .and_then(|deadline| deadline.format(&Rfc3339).ok()),
            evidence_path: None,
        })
        .map_err(|error| format!("record load media-readiness operation: {error}"))?;
    if let Some(readiness) = retryable_load_completion.as_ref() {
        record_session_open_readiness_poll_transition(
            index,
            operation_id,
            "load_drive_completion",
            readiness,
            false,
        )
        .map_err(|error| format!("record LOAD readiness transition: {error}"))?;
    }

    handle.publish_state(
        pb::OperationState::Running,
        &[("phase", "readiness_poll"), ("state", "starting")],
    );
    let mut drive = library
        .open_drive(bay, &cfg.policy)
        .map_err(|error| format!("open drive 0x{bay:04x} for readiness wait: {error}"))?;
    let poll = poll_drive_media_readiness(
        index,
        &mut drive,
        operation_id,
        family,
        MediaReadinessWaitOptions {
            wait: true,
            timeout: LOAD_READY_TIMEOUT,
            poll_interval: LOAD_READY_POLL_INTERVAL,
        },
        handle,
        "load_drive_readiness",
    )?;
    if !poll.readiness.is_ready() {
        let state = if poll.timed_out {
            "timeout_unknown"
        } else {
            session_open_readiness_state(&poll.readiness)
        };
        return Err(format!(
            "load drive 0x{bay:04x} did not reach READY (state={state}): {}",
            session_open_readiness_summary(&poll.readiness)
        ));
    }
    library
        .refresh()
        .map_err(|error| format!("refresh inventory after READY: {error}"))
}

pub(crate) fn retryable_readiness_from_load_error(
    error: &LoadError,
    family: MediaFamily,
) -> Option<MediaReadiness> {
    let LoadError::DriveLoad(DriveOpError::ScsiError(error)) = error else {
        return None;
    };
    let readiness = classify_media_readiness_error_ref(error, family);
    readiness.is_retryable_wait().then_some(readiness)
}

pub(crate) fn observe_refreshed_library(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &remanence_library::Library,
) -> Result<(), Status> {
    observe_drive_catalog_from_libraries(
        index,
        std::iter::once(library),
        &cfg.managed_library_serials,
    )
}

pub(crate) fn library_snapshot_persist_alarm_key(library_serial: &str) -> String {
    format!("snapshot-persist-failing:library:{library_serial}")
}

pub(crate) fn record_library_observation_failure(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &remanence_library::Library,
    error: &str,
) {
    tracing::warn!(
        "failed to observe refreshed drive catalog library_serial={} error={}",
        library.serial,
        error
    );
    let condition_key = library_snapshot_persist_alarm_key(library.serial.as_str());
    let detail = format!(
        "{{\"library_serial\":\"{}\",\"error\":\"{}\"}}",
        library.serial.replace('"', "'"),
        error.replace('"', "'")
    );
    if let Err(err) = raise_alarm_with_evidence(
        index,
        cfg,
        condition_key.as_str(),
        "snapshot-persist-failing",
        "warning",
        Some(detail.as_str()),
    ) {
        tracing::warn!(
            "failed to raise library snapshot alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

pub(crate) fn clear_library_snapshot_persist_alarm(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: &str,
) {
    let condition_key = library_snapshot_persist_alarm_key(library_serial);
    if let Err(err) = clear_alarm_with_evidence(index, cfg, condition_key.as_str()) {
        tracing::warn!(
            "failed to clear library snapshot alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

pub(crate) fn publish_library_snapshot(
    cell: &RwLock<Arc<crate::LibrarySnapshot>>,
    updated: remanence_library::Library,
) {
    let mut snapshot_guard = cell.write().unwrap_or_else(|err| err.into_inner());
    let mut report = snapshot_guard.report.clone();
    match report
        .libraries
        .iter_mut()
        .find(|library| library.serial == updated.serial)
    {
        Some(slot) => *slot = updated,
        None => report.libraries.push(updated),
    }
    let snapshot = Arc::new(crate::LibrarySnapshot {
        report,
        captured_at: OffsetDateTime::now_utc(),
    });
    *snapshot_guard = snapshot;
}

pub(crate) fn record_library_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    detail.insert(
        "library_serial".to_string(),
        CborValue::Text(library_serial.to_string()),
    );
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
            subject_kind: "library",
            subject_id: Some(library_serial.to_string()),
            idempotency_key: None,
            detail,
        },
    )
}

pub(crate) fn fail_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    tracing::error!(
        target: "remanence_library_operation",
        operation_id = %handle.op_id_uuid(),
        operation_kind = %handle.operation_kind(),
        library_serial,
        error_summary,
        "library operation failed"
    );
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::OperationFailed,
        detail,
    ) {
        let audit_error = format!("{error_summary}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), progress);
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

pub(crate) fn cancel_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
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

pub(crate) fn robotics_detail(action: &RoboticsAction) -> BTreeMap<String, CborValue> {
    let mut detail = BTreeMap::new();
    match action {
        RoboticsAction::Refresh => {}
        RoboticsAction::Move { src, dst } => {
            detail.insert(
                "src".to_string(),
                CborValue::Integer(u64::from(*src).into()),
            );
            detail.insert(
                "dst".to_string(),
                CborValue::Integer(u64::from(*dst).into()),
            );
        }
        RoboticsAction::Load {
            slot,
            bay,
            wait_ready,
        } => {
            detail.insert(
                "slot".to_string(),
                CborValue::Integer(u64::from(*slot).into()),
            );
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
            detail.insert("wait_ready".to_string(), CborValue::Bool(*wait_ready));
        }
        RoboticsAction::Unload { bay, destination } => {
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
            if let Some(dst) = destination {
                detail.insert(
                    "destination".to_string(),
                    CborValue::Integer(u64::from(*dst).into()),
                );
            }
        }
        RoboticsAction::Clean {
            drive_uuid,
            trigger,
        } => {
            detail.insert(
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.clone()),
            );
            detail.insert("trigger".to_string(), CborValue::Text(trigger.clone()));
            detail.insert(
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            );
        }
    }
    detail
}
