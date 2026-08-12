//! Drive/changer actor startup, command dispatch, health, and session audit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use ciborium::value::Value as CborValue;
use remanence_library::{ChangerHandle, DiscoveryReport, DriveHandle, StaticAllowlist};
use remanence_state::{
    AlarmRecord, AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex,
    CleaningConfig, DriveHealthSnapshotInput, DriveHealthSnapshotRecord, FileAuditLog, SourceLayer,
    TapeIoConfig,
};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::ReadResumeTarget;
use super::actor_protocol::{ChangerCommand, DriveCommand};
use super::read_session::handle_drive_open_read;
use super::readiness::handle_drive_wait_ready;
use super::reconcile::handle_reconcile;
use super::robotics::{
    clear_library_snapshot_persist_alarm, handle_robotics, observe_refreshed_library,
    publish_library_snapshot, record_library_observation_failure,
};
use super::terminal_finalize::handle_drive_finalize_tape;
use super::terminal_inventory::{handle_drive_tape_inventory, handle_drive_verify_tape_index};
use super::terminal_types::ManualFinalizeTapeMountRequest;
use super::write_session::handle_drive_open_write;
use super::{DriveKey, ExclusiveGuard, SelectedTape, WriteAdmissionCoordinator};
use crate::audit_projection::{
    alarm_audit_detail, append_and_project_audit, drive_health_audit_detail, ProjectedAuditInput,
};
use crate::{pb, status_from_state_error};
use remanence_state::TapePoolConfig;

#[derive(Clone)]
pub(crate) struct WriteOwnerConfig {
    pub index_path: PathBuf,
    pub report: DiscoveryReport,
    pub policy: StaticAllowlist,
    pub audit_dir: PathBuf,
    pub audit_fsync: bool,
    pub audit_append_lock: Arc<std::sync::Mutex<()>>,
    pub reservations: Arc<HashMap<crate::drive_pool::DriveKey, AtomicBool>>,
    pub actor_library_serial: String,
    pub library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
    pub snapshot_miss_alarm: u32,
    pub managed_library_serials: Arc<HashSet<String>>,
    pub cleaning: CleaningConfig,
    pub tape_io: TapeIoConfig,
    pub io_memory: Arc<crate::io_memory::IoMemoryReservation>,
    /// Cross-drive claims that make replay-key and canonical-UUID admission
    /// atomic through checkpoint projection.
    pub write_admissions: WriteAdmissionCoordinator,
    pub checkpoint_journal_dir: PathBuf,
    pub checkpoint_max_bytes: u64,
    pub checkpoint_max_objects: u64,
    pub checkpoint_max_age_seconds: u64,
    pub session_idle_seconds: u64,
    pub lifecycle: Option<crate::drive_pool::DrivePoolLifecycle>,
    /// Durable calibration-control store for the wrap-map read
    /// ordering lifecycle (design-read-ordering.md §6.5). The drive
    /// actors run the load harvest against it at session open when
    /// the open freshly mounted the cartridge.
    pub calibration_store: remanence_state::CalibrationControlStore,
}

pub(crate) fn spawn_changer_actor(
    mut changer: ChangerHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<ChangerCommand> {
    let (tx, rx) = mpsc::channel::<ChangerCommand>(16);
    let actor_name = format!("rem-changer-actor-{}", cfg.actor_library_serial);
    std::thread::Builder::new()
        .name(actor_name)
        .spawn(move || changer_loop(&mut changer, cfg, rx))
        .expect("spawn changer actor thread");
    tx
}

pub(crate) fn spawn_drive_actor(
    bay: u16,
    mut drive: DriveHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<DriveCommand> {
    let (tx, rx) = mpsc::channel::<DriveCommand>(16);
    let actor_tx = tx.clone();
    let actor_name = format!("rem-drive-actor-{}-{bay:04x}", cfg.actor_library_serial);
    std::thread::Builder::new()
        .name(actor_name)
        .spawn(move || drive_loop(bay, &mut drive, cfg, actor_tx, rx))
        .expect("spawn drive actor thread");
    tx
}

pub(super) fn changer_loop(
    changer: &mut ChangerHandle,
    cfg: WriteOwnerConfig,
    mut rx: mpsc::Receiver<ChangerCommand>,
) {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_changer_commands(
                rx,
                format!("open catalog index: {err}"),
                cfg.reservations.clone(),
                cfg.actor_library_serial.clone(),
            );
            return;
        }
    };
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::Move { src, dst, reply } => {
                let result = changer
                    .move_medium(src, dst, &cfg.policy)
                    .map_err(|err| Status::internal(format!("move medium: {err}")));
                if result.is_ok() {
                    match observe_refreshed_library(&mut index, &cfg, changer.library()) {
                        Ok(()) => clear_library_snapshot_persist_alarm(
                            &mut index,
                            &cfg,
                            changer.library().serial.as_str(),
                        ),
                        Err(err) => record_library_observation_failure(
                            &mut index,
                            &cfg,
                            changer.library(),
                            err.message(),
                        ),
                    }
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Refresh { reply } => {
                let result = changer
                    .refresh()
                    .map_err(|err| Status::internal(format!("refresh inventory: {err}")))
                    .and_then(|()| observe_refreshed_library(&mut index, &cfg, changer.library()));
                if result.is_ok() {
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Reconcile { tape_uuid, handle } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved_library(
                    cfg.reservations.clone(),
                    cfg.actor_library_serial.clone(),
                );
                handle_reconcile(&mut index, &cfg, tape_uuid, handle);
                refresh_actor_changer(changer, &cfg);
            }
            ChangerCommand::Robotics {
                library_serial,
                action,
                handle,
            } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved_library(
                    cfg.reservations.clone(),
                    cfg.actor_library_serial.clone(),
                );
                handle_robotics(&mut index, &cfg, library_serial, action, handle);
                refresh_actor_changer(changer, &cfg);
            }
        }
    }
}

pub(super) fn refresh_actor_changer(changer: &mut ChangerHandle, cfg: &WriteOwnerConfig) {
    if changer.refresh().is_ok() {
        match CatalogIndex::open(cfg.index_path.as_path()) {
            Ok(mut index) => {
                if let Err(err) = observe_refreshed_library(&mut index, cfg, changer.library()) {
                    tracing::warn!("failed to observe refreshed drive catalog: {err}");
                }
            }
            Err(err) => tracing::warn!("failed to open catalog for refreshed drive catalog: {err}"),
        }
        publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
    }
}

pub(super) fn drain_failed_changer_commands(
    mut rx: mpsc::Receiver<ChangerCommand>,
    message: String,
    reservations: Arc<HashMap<DriveKey, AtomicBool>>,
    library_serial: String,
) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::Move { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Refresh { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Reconcile { handle, .. } | ChangerCommand::Robotics { handle, .. } => {
                handle.publish_failed(message.as_str(), &[("phase", "catalog")]);
                crate::drive_pool::release_library_reservations(&reservations, &library_serial);
            }
        }
    }
}

pub(super) fn drive_loop(
    bay: u16,
    drive: &mut DriveHandle,
    cfg: WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    mut rx: mpsc::Receiver<DriveCommand>,
) {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_drive_commands(rx, format!("open catalog index: {err}"));
            return;
        }
    };
    let mut snapshot_misses = 0u32;
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::WaitReady {
                operation_id,
                family,
                options,
                handle,
                reservation: _reservation,
            } => handle_drive_wait_ready(
                bay,
                &mut index,
                drive,
                operation_id,
                family,
                options,
                &handle,
            ),
            DriveCommand::OpenWrite {
                pool_cfg,
                selected,
                target_kind,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                reply,
            } => handle_drive_open_write(
                bay,
                &mut index,
                &cfg,
                actor_tx.clone(),
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenWriteActorRequest {
                    pool_cfg,
                    selected,
                    target_kind,
                    needs_drive_load,
                    library_serial,
                    barcode,
                    source_slot,
                    drive_uuid,
                    drive_serial,
                    reply,
                },
            ),
            DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                resume_target,
                daemon_epoch,
                reply,
            } => handle_drive_open_read(
                bay,
                &mut index,
                &cfg,
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenReadActorRequest {
                    tape_uuid,
                    needs_drive_load,
                    library_serial,
                    barcode,
                    source_slot,
                    drive_uuid,
                    drive_serial,
                    resume_target,
                    daemon_epoch,
                    reply,
                },
            ),
            DriveCommand::TapeInventory {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_serial,
                stream_tx,
                reply,
            } => {
                let result = handle_drive_tape_inventory(
                    bay,
                    &mut index,
                    &cfg,
                    drive,
                    tape_uuid,
                    needs_drive_load,
                    library_serial.as_str(),
                    barcode.as_deref(),
                    source_slot,
                    drive_serial.as_deref(),
                    &stream_tx,
                );
                if let Err(error) = &result {
                    let _ = stream_tx
                        .blocking_send(Err(Status::new(error.code(), error.message().to_string())));
                }
                let _ = reply.send(result);
            }
            DriveCommand::VerifyTapeIndex {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_serial,
                reply,
            } => {
                let result = handle_drive_verify_tape_index(
                    bay,
                    &mut index,
                    &cfg,
                    drive,
                    tape_uuid,
                    needs_drive_load,
                    library_serial.as_str(),
                    barcode.as_deref(),
                    source_slot,
                    drive_serial.as_deref(),
                );
                let _ = reply.send(result);
            }
            DriveCommand::FinalizeTape {
                request,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                reply,
            } => {
                let result = handle_drive_finalize_tape(
                    bay,
                    &mut index,
                    &cfg,
                    drive,
                    &mut snapshot_misses,
                    ManualFinalizeTapeMountRequest {
                        request,
                        needs_drive_load,
                        library_serial,
                        barcode,
                        source_slot,
                        drive_uuid,
                        drive_serial,
                    },
                );
                let _ = reply.send(result);
            }
            DriveCommand::Unload { reply } => {
                let started = Instant::now();
                let result = drive
                    .unload()
                    .map(|()| started.elapsed())
                    .map_err(|err| Status::internal(format!("unload drive: {err}")));
                let _ = reply.send(result);
            }
            DriveCommand::PollHealth {
                drive_uuid,
                trigger,
                session_id,
                tape_uuid,
                reply,
            } => {
                let result = collect_drive_health_snapshot(
                    &mut index,
                    &cfg,
                    drive,
                    DriveSnapshotRequest {
                        drive_uuid,
                        trigger,
                        session_id,
                        tape_uuid,
                    },
                );
                let _ = reply.send(result);
            }
            DriveCommand::Heartbeat { drive_uuid, reply } => {
                let result = drive
                    .test_unit_ready()
                    .map_err(|err| Status::unavailable(format!("drive heartbeat: {err}")))
                    .and_then(|_| {
                        index
                            .touch_drive_last_seen(&drive_uuid)
                            .map(|_| ())
                            .map_err(status_from_state_error)
                    });
                let _ = reply.send(result);
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::failed_precondition("no active write session")));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::not_found("no active write session")));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active write session")));
            }
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active write session")));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::not_found("no active read session")));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active read session")));
            }
        }
    }
}

pub(super) fn drain_failed_drive_commands(mut rx: mpsc::Receiver<DriveCommand>, message: String) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed(&message, &[("phase", "drive_actor")]);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::TapeInventory { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::VerifyTapeIndex { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::FinalizeTape { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::internal(message.clone())));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::internal(message.clone())));
            }
        }
    }
}

pub(super) struct OpenWriteActorRequest {
    pub(super) pool_cfg: TapePoolConfig,
    pub(super) selected: SelectedTape,
    pub(super) target_kind: pb::write_session::TargetKind,
    pub(super) needs_drive_load: bool,
    pub(super) library_serial: String,
    pub(super) barcode: Option<String>,
    pub(super) source_slot: Option<u16>,
    pub(super) drive_uuid: Option<Vec<u8>>,
    pub(super) drive_serial: Option<String>,
    pub(super) reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
}

pub(super) struct OpenReadActorRequest {
    pub(super) tape_uuid: [u8; 16],
    pub(super) needs_drive_load: bool,
    pub(super) library_serial: String,
    pub(super) barcode: Option<String>,
    pub(super) source_slot: Option<u16>,
    pub(super) drive_uuid: Option<Vec<u8>>,
    pub(super) drive_serial: Option<String>,
    pub(super) resume_target: Option<ReadResumeTarget>,
    pub(super) daemon_epoch: u64,
    pub(super) reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
}

pub(super) struct SessionAuditInput {
    pub(super) session_id: Uuid,
    pub(super) session_kind: &'static str,
    pub(super) event: AuditEvent,
    pub(super) tape_uuid: Option<[u8; 16]>,
    pub(super) library_serial: Option<String>,
    pub(super) drive_bay: Option<u16>,
    pub(super) drive_uuid: Option<Vec<u8>>,
    pub(super) drive_serial: Option<String>,
    /// Only ever set for an aborted write session, and only when the caller
    /// supplied one.
    pub(super) abort_reason: Option<String>,
}

pub(super) fn record_session_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    input: SessionAuditInput,
) -> Result<(), Status> {
    let _guard = cfg
        .audit_append_lock
        .lock()
        .map_err(|_| Status::internal("session audit append lock poisoned"))?;
    std::fs::create_dir_all(cfg.audit_dir.as_path()).map_err(|err| {
        Status::internal(format!(
            "create session audit directory {}: {err}",
            cfg.audit_dir.display()
        ))
    })?;
    let mut detail = BTreeMap::new();
    detail.insert(
        "session_kind".to_string(),
        CborValue::Text(input.session_kind.to_string()),
    );
    if let Some(tape_uuid) = input.tape_uuid {
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
    }
    if let Some(library_serial) = input.library_serial {
        detail.insert(
            "library_serial".to_string(),
            CborValue::Text(library_serial),
        );
    }
    if let Some(drive_bay) = input.drive_bay {
        detail.insert(
            "drive_bay".to_string(),
            CborValue::Integer(u64::from(drive_bay).into()),
        );
    }
    if let Some(drive_uuid) = input.drive_uuid {
        detail.insert("drive_uuid".to_string(), CborValue::Bytes(drive_uuid));
    }
    if let Some(drive_serial) = input.drive_serial {
        detail.insert("drive_serial".to_string(), CborValue::Text(drive_serial));
    }
    if let Some(abort_reason) = input.abort_reason {
        detail.insert("abort_reason".to_string(), CborValue::Text(abort_reason));
    }
    let mut audit = FileAuditLog::open(cfg.audit_dir.as_path(), cfg.audit_fsync)
        .map_err(crate::status_from_state_error)?;
    let (_, record) = audit
        .append_and_return_record(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(input.session_id),
            idempotency_key: None,
            event: input.event,
            subject: AuditSubject {
                kind: input.session_kind.to_string(),
                id: Some(input.session_id.to_string()),
            },
            detail,
        })
        .map_err(crate::status_from_state_error)?;
    index
        .project_audit_record(&record)
        .map_err(crate::status_from_state_error)
}

pub(super) struct DriveSnapshotRequest {
    drive_uuid: Vec<u8>,
    trigger: &'static str,
    session_id: Option<Uuid>,
    tape_uuid: Option<[u8; 16]>,
}

pub(super) fn collect_drive_health_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    request: DriveSnapshotRequest,
) -> Result<DriveHealthSnapshotRecord, Status> {
    let alerts = drive
        .read_tape_alerts()
        .map_err(|err| Status::unavailable(format!("read TapeAlert page: {err}")))?;
    let counters = drive
        .read_error_counters()
        .map_err(|err| Status::unavailable(format!("read error counter pages: {err}")))?;
    let tape_uuid_text = request
        .tape_uuid
        .map(|uuid| Uuid::from_bytes(uuid).to_string())
        .unwrap_or_default();
    let raw_pages = format!(
        "{{\"tape_uuid\":\"{}\",\"tape_alert\":true,\"write_error_counter\":true,\"read_error_counter\":true}}",
        tape_uuid_text
    );
    let snapshot = index
        .record_drive_health_snapshot(DriveHealthSnapshotInput {
            drive_uuid: request.drive_uuid.clone(),
            trigger: request.trigger.to_string(),
            session_id: request.session_id.map(|uuid| uuid.to_string()),
            tape_alert_flags: Some(tape_alert_flags_json(alerts.active())),
            write_errors_corrected: counters.write_errors_corrected.and_then(u64_to_i64),
            write_errors_uncorrected: counters.write_errors_uncorrected.and_then(u64_to_i64),
            read_errors_corrected: counters.read_errors_corrected.and_then(u64_to_i64),
            read_errors_uncorrected: counters.read_errors_uncorrected.and_then(u64_to_i64),
            raw_pages: Some(raw_pages),
            at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    if alerts.is_set(20) || alerts.is_set(21) {
        let due = if alerts.is_set(20) { "now" } else { "periodic" };
        index
            .observe_managed_drive_cleaning_due(&request.drive_uuid, due)
            .map_err(crate::status_from_state_error)?;
    } else {
        index
            .touch_drive_last_seen(&request.drive_uuid)
            .map_err(crate::status_from_state_error)?;
    }
    append_drive_health_evidence(index, cfg, &snapshot)?;
    Ok(snapshot)
}

/// Append the durable evidence twin for a just-committed health snapshot and
/// project that exact record through the same replay funnel used at rebuild.
pub(super) fn append_drive_health_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    snapshot: &DriveHealthSnapshotRecord,
) -> Result<(), Status> {
    let detail = drive_health_audit_detail(index, snapshot)?;
    append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: snapshot
                .session_id
                .as_deref()
                .and_then(|value| Uuid::parse_str(value).ok()),
            idempotency_key: None,
            event: AuditEvent::DriveHealthObserved,
            subject_kind: "drive",
            subject_id: Some(crate::bytes_to_hex(snapshot.drive_uuid.as_slice())),
            detail,
        },
    )?;
    Ok(())
}

pub(super) fn insert_optional_audit_text(
    detail: &mut BTreeMap<String, CborValue>,
    key: &str,
    value: Option<&String>,
) {
    if let Some(value) = value {
        detail.insert(key.to_string(), CborValue::Text(value.clone()));
    }
}

pub(super) fn raise_alarm_with_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    condition_key: &str,
    kind: &str,
    severity: &str,
    alarm_detail: Option<&str>,
) -> Result<AlarmRecord, Status> {
    let alarm = index
        .raise_alarm(condition_key, kind, severity, alarm_detail)
        .map_err(crate::status_from_state_error)
        .inspect_err(
            |error| tracing::warn!(condition_key, %error, "failed to raise catalog alarm"),
        )?;
    append_alarm_evidence(index, cfg, &alarm, AuditEvent::AlarmRaised).inspect_err(
        |error| tracing::warn!(condition_key, %error, "failed to append raised-alarm evidence"),
    )?;
    Ok(alarm)
}

pub(super) fn clear_alarm_with_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    condition_key: &str,
) -> Result<Option<AlarmRecord>, Status> {
    let alarm = index
        .clear_alarm(condition_key)
        .map_err(crate::status_from_state_error)
        .inspect_err(
            |error| tracing::warn!(condition_key, %error, "failed to clear catalog alarm"),
        )?;
    if let Some(alarm) = alarm.as_ref() {
        append_alarm_evidence(index, cfg, alarm, AuditEvent::AlarmCleared).inspect_err(
            |error| {
                tracing::warn!(condition_key, %error, "failed to append cleared-alarm evidence")
            },
        )?;
    }
    Ok(alarm)
}

pub(super) fn append_alarm_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    alarm: &AlarmRecord,
    event: AuditEvent,
) -> Result<(), Status> {
    append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event,
            subject_kind: "alarm",
            subject_id: Some(alarm.condition_key.clone()),
            detail: alarm_audit_detail(alarm),
        },
    )?;
    Ok(())
}

pub(super) fn record_session_close_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    drive_uuid: Option<Vec<u8>>,
    session_id: Uuid,
    tape_uuid: [u8; 16],
    consecutive_misses: &mut u32,
) {
    record_session_snapshot(
        index,
        cfg,
        drive,
        drive_uuid,
        session_id,
        tape_uuid,
        "session-close",
        consecutive_misses,
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_session_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    drive_uuid: Option<Vec<u8>>,
    session_id: Uuid,
    tape_uuid: [u8; 16],
    trigger: &'static str,
    consecutive_misses: &mut u32,
) {
    let Some(drive_uuid) = drive_uuid else {
        return;
    };
    match collect_drive_health_snapshot(
        index,
        cfg,
        drive,
        DriveSnapshotRequest {
            drive_uuid: drive_uuid.clone(),
            trigger,
            session_id: Some(session_id),
            tape_uuid: Some(tape_uuid),
        },
    ) {
        Ok(_) => {
            clear_snapshot_persist_alarm(index, cfg, drive_uuid.as_slice());
            *consecutive_misses = 0;
        }
        Err(err) => {
            *consecutive_misses = consecutive_misses.saturating_add(1);
            tracing::warn!(
                "drive health snapshot missed session_id={} drive_uuid={} misses={} error={}",
                session_id,
                Uuid::from_slice(&drive_uuid)
                    .map(|uuid| uuid.to_string())
                    .unwrap_or_else(|_| crate::bytes_to_hex(&drive_uuid)),
                *consecutive_misses,
                err
            );
            if cfg.snapshot_miss_alarm > 0 && *consecutive_misses >= cfg.snapshot_miss_alarm {
                let condition_key = snapshot_persist_alarm_key(&drive_uuid);
                let detail = format!(
                    "{{\"session_id\":\"{}\",\"misses\":{},\"error\":\"{}\"}}",
                    session_id,
                    *consecutive_misses,
                    err.to_string().replace('"', "'")
                );
                if let Err(alarm_err) = raise_alarm_with_evidence(
                    index,
                    cfg,
                    condition_key.as_str(),
                    "snapshot-persist-failing",
                    "warning",
                    Some(detail.as_str()),
                ) {
                    tracing::warn!(
                        "failed to raise snapshot miss alarm condition_key={} error={}",
                        condition_key,
                        alarm_err
                    );
                }
            }
        }
    }
}

pub(super) fn snapshot_persist_alarm_key(drive_uuid: &[u8]) -> String {
    format!(
        "snapshot-persist-failing:{}",
        crate::bytes_to_hex(drive_uuid)
    )
}

pub(super) fn clear_snapshot_persist_alarm(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive_uuid: &[u8],
) {
    let condition_key = snapshot_persist_alarm_key(drive_uuid);
    if let Err(err) = clear_alarm_with_evidence(index, cfg, condition_key.as_str()) {
        tracing::warn!(
            "failed to clear snapshot miss alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

pub(super) fn tape_alert_flags_json(flags: &std::collections::BTreeSet<u8>) -> String {
    let body = flags
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

pub(super) fn u64_to_i64(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}
