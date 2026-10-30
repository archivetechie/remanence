//! Drive/changer actor startup, command dispatch, health, and session audit.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
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
use super::readiness::{handle_drive_wait_ready_and_release, DriveReadinessPoll};
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
    pub placement: super::ActorPlacement,
    pub snapshot_miss_alarm: u32,
    pub managed_library_serials: Arc<HashSet<String>>,
    pub cleaning: CleaningConfig,
    pub tape_io: TapeIoConfig,
    pub tape_pools: Vec<TapePoolConfig>,
    pub verification_route: remanence_parity::VerificationRoute,
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
    /// Durable calibration-control store for the wrap-map read-ordering lifecycle. The drive
    /// actors run the load harvest against it at session open when
    /// the open freshly mounted the cartridge.
    pub calibration_store: remanence_state::CalibrationControlStore,
}

pub(crate) fn spawn_changer_actor(
    mut changer: ChangerHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<ChangerCommand> {
    let (tx, rx) = mpsc::channel::<ChangerCommand>(16);
    let actor_name = format!("rem-changer-actor-{}", cfg.library().serial);
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
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let actor_name = format!("rem-drive-actor-{}-{bay:04x}", cfg.library().serial);
    std::thread::Builder::new()
        .name(actor_name)
        .spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                drive_loop(bay, &mut drive, cfg, actor_tx, rx)
            })
        })
        .expect("spawn drive actor thread");
    tx
}

/// Own the physical mount until all actor-local leases and accumulators are gone.
pub(crate) fn spawn_mounted_actor(
    mut mounted: crate::MountedTape,
    cfg: WriteOwnerConfig,
) -> std::io::Result<(mpsc::Sender<DriveCommand>, std::thread::JoinHandle<()>)> {
    let (tx, rx) = mpsc::channel(16);
    let actor_tx = tx.clone();
    let audit_error = cfg.mounted().expect("mounted config").audit_error.clone();
    let name = format!("rem-mounted-{}", cfg.endpoint(0).encode());
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    let thread = std::thread::Builder::new().name(name).spawn(move || {
        // Zero is only a dispatch ordinal for the legacy loop. Physical coordinates
        // always come from ActorPlacement; standalone sessions publish no bay.
        let finish = tracing::dispatcher::with_default(&dispatch, || {
            drive_loop(0, &mut mounted.drive, cfg, actor_tx, rx)
        });
        let cleanup = mounted
            .finish()
            .map_err(|e| Status::unavailable(e.to_string()));
        if let Some(finish) = finish {
            let audit = audit_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map_or(Ok(()), |e| {
                    Err(Status::internal(format!("drive audit failed: {e}")))
                });
            let _ = finish.reply.send(finish.result.and(cleanup).and(audit));
        }
    })?;
    Ok((tx, thread))
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
                cfg.library().serial.clone(),
            );
            return;
        }
    };
    if let Some(lifecycle) = &cfg.lifecycle {
        index.set_readiness_observer(lifecycle.readiness_wake.clone());
    }
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::PlanningMove {
                src,
                dst,
                ownership,
                reply,
            } => {
                let result =
                    super::robotics::admit_fenced_elements(&index, changer.library(), &[src, dst])
                        .and_then(|()| {
                            changer.move_medium(src, dst, &cfg.policy).map_err(|err| {
                                Status::failed_precondition(format!("move medium: {err}"))
                            })
                        });
                {
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
                    publish_library_snapshot(&cfg.library().snapshot, changer.library().clone());
                }
                if result.is_ok() {
                    ownership.moved();
                }
                let _ = reply.send(ownership.complete(result));
            }
            ChangerCommand::Move { src, dst, reply } => {
                let result =
                    super::robotics::admit_fenced_elements(&index, changer.library(), &[src, dst])
                        .and_then(|()| {
                            changer.move_medium(src, dst, &cfg.policy).map_err(|err| {
                                Status::failed_precondition(format!("move medium: {err}"))
                            })
                        });
                {
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
                    publish_library_snapshot(&cfg.library().snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Reconcile { tape_uuid, handle } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved_library(
                    cfg.reservations.clone(),
                    cfg.library().serial.clone(),
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
                    cfg.library().serial.clone(),
                );
                let outcome = handle_robotics(&mut index, &cfg, &library_serial, action, &handle);
                finish_robotics_and_release(
                    &mut index,
                    &cfg,
                    changer,
                    &library_serial,
                    &handle,
                    outcome,
                    _exclusive_guard,
                );
            }
        }
    }
}

/// A terminal watcher can retry only after refreshed inventory and released ownership.
pub(super) fn finish_robotics_and_release(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    changer: &mut ChangerHandle,
    library_serial: &str,
    handle: &crate::operations::OperationHandle,
    outcome: super::robotics::RoboticsOutcome,
    guard: ExclusiveGuard,
) {
    refresh_actor_changer(changer, cfg);
    drop(guard);
    outcome.publish(index, cfg, handle, library_serial);
    handle.wake_readiness();
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
        publish_library_snapshot(&cfg.library().snapshot, changer.library().clone());
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
            ChangerCommand::PlanningMove {
                reply, ownership, ..
            } => {
                let _ = reply.send(ownership.complete(Err(Status::internal(message.clone()))));
            }
            ChangerCommand::Move { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Reconcile { handle, .. } | ChangerCommand::Robotics { handle, .. } => {
                crate::drive_pool::release_library_reservations(&reservations, &library_serial);
                handle.publish_failed(
                    message.as_str(),
                    &[("phase", "catalog"), ("state", "unknown")],
                );
            }
        }
    }
}

pub(crate) struct MountedFinish {
    pub reply: oneshot::Sender<Result<(), Status>>,
    pub result: Result<(), Status>,
}

pub(super) fn drive_loop(
    bay: u16,
    drive: &mut DriveHandle,
    cfg: WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    mut rx: mpsc::Receiver<DriveCommand>,
) -> Option<MountedFinish> {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_drive_commands(rx, format!("open catalog index: {err}"));
            return None;
        }
    };
    if let Some(lifecycle) = &cfg.lifecycle {
        index.set_readiness_observer(lifecycle.readiness_wake.clone());
    }
    let mut snapshot_misses = 0u32;
    let mut retained: Option<super::checkpoint::RetainedParityEpoch> = None;
    while let Some(cmd) = rx.blocking_recv() {
        #[cfg(test)]
        match cfg
            .io_memory
            .epoch_test_action
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            4 => {
                retained = None;
            }
            5 => {
                if let Some(e) = retained.as_mut() {
                    e.identity_generation = e.identity_generation.wrapping_add(1);
                }
            }
            _ => {}
        }
        if matches!(
            &cmd,
            DriveCommand::OpenWrite { .. }
                | DriveCommand::OpenRead { .. }
                | DriveCommand::TapeInventory { .. }
                | DriveCommand::VerifyTapeIndex { .. }
                | DriveCommand::WaitReady { .. }
                | DriveCommand::FinalizeTape { .. }
                | DriveCommand::SupersedeTail { .. }
                | DriveCommand::PlanFinalization { .. }
        ) {
            if let Some(epoch) = retained.as_mut() {
                epoch.head_at_eod = false;
            }
        }
        match cmd {
            DriveCommand::OpenMountedWrite {
                host_only_recovery,
                pool_cfg,
                mut selected,
                reply,
            } => {
                let Some(mounted) = cfg.mounted() else {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "mounted command on library actor",
                    )));
                    continue;
                };
                if selected.tape_uuid != mounted.tape_uuid {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "request differs from mounted tape",
                    )));
                    continue;
                }
                selected.standalone_endpoint = Some(mounted.endpoint.clone());
                match super::preflight_automatic_terminal_completion(
                    &mut index,
                    super::ManualFinalizePreflightConfig {
                        checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                        audit_dir: &cfg.audit_dir,
                        audit_fsync: cfg.audit_fsync,
                        audit_append_lock: &cfg.audit_append_lock,
                    },
                    &selected,
                    &pool_cfg,
                ) {
                    Ok(false) if !host_only_recovery => {}
                    Ok(completed) => {
                        let detail = if completed {
                            "terminal finalization completed during host-only open recovery; tape cannot accept Objects"
                        } else {
                            "terminal recovery authority changed; refusing Object admission without media access"
                        };
                        let _ = reply.send(Err(super::ActorFailure::attach(
                            Status::failed_precondition(detail),
                            crate::PoolWriteError::TerminalCloseRequired {
                                detail: detail.into(),
                            },
                        )));
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                }
                let before = drive.media_write_dispatch_count();
                let finish = handle_drive_open_write(
                    bay,
                    &mut index,
                    &cfg,
                    actor_tx.clone(),
                    &mut rx,
                    drive,
                    &mut snapshot_misses,
                    OpenWriteActorRequest {
                        actor: mounted.actor.clone(),
                        pool_cfg,
                        selected,
                        target_kind:
                            pb::write_session::TargetKind::WriteSessionTargetKindPinnedTape,
                        needs_drive_load: false,
                        library_serial: None,
                        barcode: mounted.barcode.clone(),
                        source_slot: None,
                        drive_uuid: None,
                        drive_serial: match &mounted.endpoint {
                            remanence_library::DriveKey::Standalone { drive_serial } => {
                                Some(drive_serial.clone())
                            }
                            _ => None,
                        },
                        reply,
                    },
                    &mut retained,
                );
                let audit = super::mounted_commands::settle_audit(&mut index, &cfg, drive, before);
                if let Some(mut finish) = finish {
                    let closed = super::mounted_commands::close_epoch(
                        &mut index,
                        &cfg,
                        drive,
                        bay,
                        &mut retained,
                    );
                    finish.result = finish.result.and(audit).and(closed);
                    rx.close();
                    drain_failed_drive_commands(rx, "mounted actor finished".into());
                    return Some(finish);
                }
                if let Err(error) = audit {
                    *cfg.mounted()
                        .expect("mounted session")
                        .audit_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
                    tracing::error!(%error, "mounted session audit settlement failed");
                }
            }
            DriveCommand::FinishMountedInvocation { reply } => {
                if cfg.mounted().is_none() {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "mounted command on library actor",
                    )));
                    continue;
                }
                let result = super::mounted_commands::close_epoch(
                    &mut index,
                    &cfg,
                    drive,
                    bay,
                    &mut retained,
                );
                rx.close();
                drain_failed_drive_commands(rx, "mounted actor finished".into());
                return Some(MountedFinish { reply, result });
            }
            DriveCommand::FinalizeMounted { request, reply } => {
                let result = super::mounted_commands::finalize(&mut index, &cfg, drive, request);
                if result.is_ok() {
                    retained = None;
                }
                let _ = reply.send(result);
            }
            DriveCommand::SupersedeMounted { request, reply } => {
                let result = super::mounted_commands::supersede(&mut index, &cfg, drive, request);
                retained = None;
                let _ = reply.send(result);
            }
            DriveCommand::WaitReady {
                operation_id,
                family,
                options,
                cartridge_in_drive,
                handle,
                reservation,
            } => handle_drive_wait_ready_and_release(
                &mut index,
                drive,
                DriveReadinessPoll {
                    bay,
                    operation_id,
                    family,
                    options,
                    cartridge_in_drive,
                    phase: "grpc_wait_ready",
                },
                &handle,
                Some(reservation),
            ),
            DriveCommand::OpenWrite {
                actor,
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
            } => {
                let _ = handle_drive_open_write(
                    bay,
                    &mut index,
                    &cfg,
                    actor_tx.clone(),
                    &mut rx,
                    drive,
                    &mut snapshot_misses,
                    OpenWriteActorRequest {
                        actor,
                        pool_cfg,
                        selected,
                        target_kind,
                        needs_drive_load,
                        library_serial: Some(library_serial),
                        barcode,
                        source_slot,
                        drive_uuid,
                        drive_serial,
                        reply,
                    },
                    &mut retained,
                );
            }
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
                handle,
                progress,
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
                    Some((&handle, progress)),
                );
                let _ = reply.send(result);
            }
            DriveCommand::SupersedeTail {
                request,
                actor,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_serial,
                reply,
            } => {
                let result = (|| {
                    let tape_uuid = crate::catalog_conversion::decode_uuid_bytes(
                        &request.tape_uuid,
                        "tape_uuid",
                    )?;
                    super::readiness::session_open_short_probe_or_load(
                        &mut index,
                        drive,
                        super::SessionOpenReadinessContext {
                            action: "supersede tape tail",
                            bay,
                            library_serial: &library_serial,
                            barcode: barcode.as_deref(),
                            source_slot,
                            drive_serial: drive_serial.as_deref(),
                            needs_drive_load,
                        },
                    )?;
                    let tape = index
                        .get_tape(&tape_uuid)
                        .map_err(crate::status_from_state_error)?
                        .ok_or_else(|| Status::not_found("tape not found"))?;
                    let (size, _) = crate::pool_write::selected_tape_geometry(
                        &tape,
                        tape.pool_id.as_deref().unwrap_or("(unpooled)"),
                    )
                    .map_err(|e| Status::failed_precondition(e.to_string()))?;
                    super::write_session::prepare_drive_for_write(
                        drive,
                        &tape_uuid,
                        size,
                        Uuid::new_v4(),
                        super::write_session::WriteMediaPolicy::RewritableObject,
                    )?;
                    let generation = drive.identity_generation();
                    let mut preview = None;
                    let result = crate::mount_backend::supersede_mounted_tail(
                        &mut index,
                        &super::terminal_types::TerminalConfig::terminal_config(&cfg),
                        drive,
                        tape_uuid,
                        generation,
                        &Uuid::from_bytes(tape_uuid).to_string(),
                        actor,
                        |required, found| {
                            preview = Some(pb::SupersedeTailResponse {
                                found: found.to_vec(),
                                required_confirmation: required.into(),
                                completed: false,
                            });
                            match request.confirm_supersede.as_deref() {
                                None => Err("confirmation required".into()),
                                Some(value)
                                    if value == required && request.expected_found == found =>
                                {
                                    Ok(())
                                }
                                _ => {
                                    Err("confirmation or physical tail differs from the preview"
                                        .into())
                                }
                            }
                        },
                    );
                    match (result, preview) {
                        (Ok(_), Some(mut response)) => {
                            response.completed = true;
                            Ok(response)
                        }
                        (Err(_), Some(response)) if request.confirm_supersede.is_none() => {
                            Ok(response)
                        }
                        (Err(e), _) => Err(e),
                        _ => Err(Status::internal("supersede did not produce a preview")),
                    }
                })();
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
                    &mut retained,
                );
                if result.is_ok() {
                    retained = None;
                }
                let _ = reply.send(result);
            }
            DriveCommand::PlanFinalization {
                ownership,
                request,
                needs_drive_load,
                barcode,
                source_slot,
                drive_serial,
                reply,
            } => {
                let result = super::readiness::session_open_short_probe_or_load(
                    &mut index,
                    drive,
                    super::SessionOpenReadinessContext {
                        action: "plan terminal finalization",
                        bay,
                        library_serial: &cfg.library().serial,
                        barcode: barcode.as_deref(),
                        source_slot,
                        drive_serial: drive_serial.as_deref(),
                        needs_drive_load,
                    },
                )
                .and_then(|()| {
                    super::terminal_finalize::plan_mounted_epoch_finalization(
                        &mut index,
                        &cfg,
                        drive,
                        bay,
                        &request,
                        needs_drive_load,
                        &mut retained,
                    )
                });

                let _ = reply.send(ownership.complete(result));
            }
            DriveCommand::PrepareRemoval { reply, progress } => {
                let result = super::checkpoint::close_retained_before_removal(
                    &mut index,
                    drive,
                    &cfg,
                    bay,
                    &mut retained,
                    progress.as_ref(),
                );
                let _ = reply.send(result);
            }
            DriveCommand::Unload { reply } => {
                if let Err(error) = super::checkpoint::close_retained_before_removal(
                    &mut index,
                    drive,
                    &cfg,
                    bay,
                    &mut retained,
                    None,
                ) {
                    let _ = reply.send(Err(error));
                    continue;
                }
                let started = Instant::now();
                let result = drive
                    .unload()
                    .map(|()| started.elapsed())
                    .map_err(|err| match err {
                        remanence_library::DriveOpError::Inventory(_) => {
                            Status::failed_precondition(format!("unload drive: {err}"))
                        }
                        _ => Status::internal(format!("unload drive: {err}")),
                    });
                if let Some(library) = drive.observed_library() {
                    let _ = observe_refreshed_library(&mut index, &cfg, &library);
                    publish_library_snapshot(&cfg.library().snapshot, library);
                }
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
    None
}

pub(super) fn drain_failed_drive_commands(mut rx: mpsc::Receiver<DriveCommand>, message: String) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::FinishMountedInvocation { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
                break;
            }
            DriveCommand::OpenMountedWrite { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::FinalizeMounted { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::SupersedeMounted { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::WaitReady {
                handle,
                reservation,
                ..
            } => {
                handle.publish_failed(
                    &message,
                    &[("phase", "drive_actor"), ("state", "dispatch_failed")],
                );
                drop(reservation);
                handle.wake_readiness();
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
            DriveCommand::SupersedeTail { reply, .. } => {
                let _ = reply.send(Err(Status::unavailable("drive actor unavailable")));
            }
            DriveCommand::FinalizeTape { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::PlanFinalization {
                reply, ownership, ..
            } => {
                let _ = reply.send(ownership.complete(Err(Status::failed_precondition(
                    "drive actor cannot plan finalization in its current state",
                ))));
            }
            DriveCommand::PrepareRemoval { reply, .. } => {
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
    pub(super) actor: remanence_state::AuditActor,
    pub(super) pool_cfg: TapePoolConfig,
    pub(super) selected: SelectedTape,
    pub(super) target_kind: pb::write_session::TargetKind,
    pub(super) needs_drive_load: bool,
    pub(super) library_serial: Option<String>,
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
    mut input: SessionAuditInput,
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
    if let Some(mounted) = cfg.mounted() {
        input.library_serial = None;
        input.drive_bay = None;
        input.drive_uuid = None;
        if let remanence_library::DriveKey::Bay {
            library_serial,
            bay,
        } = &mounted.endpoint
        {
            input.library_serial = Some(library_serial.clone());
            input.drive_bay = Some(*bay);
        }
        detail.insert(
            "endpoint_key".into(),
            CborValue::Text(mounted.endpoint.encode()),
        );
    }
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
    let mut audit = FileAuditLog::open(
        cfg.audit_dir.as_path(),
        cfg.audit_fsync || cfg.mounted().is_some(),
    )
    .map_err(crate::status_from_state_error)?;
    let (_, record) = audit
        .append_and_return_record(AuditEventRecord {
            actor: cfg
                .mounted()
                .map_or(AuditActor::System, |m| m.actor.clone()),
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
    if let Some(mounted) = cfg.mounted() {
        *mounted
            .audit_session
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = if record.event == AuditEvent::SessionOpened {
            Some(input.session_id)
        } else {
            None
        };
    }
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
