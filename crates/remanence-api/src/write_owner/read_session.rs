//! Read-session opening and tape-I/O admission checks.

use remanence_library::DriveHandle;
use remanence_state::{AuditEvent, CatalogIndex};
use tokio::sync::mpsc;
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::DriveCommand;
use super::actor_runtime::{
    record_session_close_snapshot, record_session_event, record_session_snapshot,
    OpenReadActorRequest, SessionAuditInput, WriteOwnerConfig,
};
use super::readiness::session_open_short_probe_or_load;
use super::restore::{
    now_rfc3339, position_read_resume, read_session_proto, stream_one_file_range,
    stream_one_object, verify_loaded_tape_identity,
};
use super::terminal_inventory::prepare_drive_for_read;
use super::write_session::run_load_calibration_harvest;
use super::SessionOpenReadinessContext;
use crate::{pb, status_from_state_error, TapeUuid};

pub(super) fn handle_drive_open_read(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    request: OpenReadActorRequest,
) {
    let OpenReadActorRequest {
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
    } = request;

    if let Err(status) = session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "open read session",
            bay,
            library_serial: library_serial.as_str(),
            barcode: barcode.as_deref(),
            source_slot,
            drive_serial: drive_serial.as_deref(),
            needs_drive_load,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if let Err(status) = session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode.as_deref(),
        "open read session",
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if resume_target
        .as_ref()
        .is_some_and(|target| target.tape_uuid != tape_uuid)
    {
        let _ = reply.send(Err(Status::invalid_argument(
            "resume target tape UUID does not match mounted read target",
        )));
        return;
    }
    let session_id = Uuid::new_v4();
    let position_proof = match resume_target.as_ref() {
        Some(target) => {
            if let Err(status) = prepare_drive_for_read(index, drive, &tape_uuid, session_id) {
                let _ = reply.send(Err(status));
                return;
            }
            match position_read_resume(index, drive, target) {
                Ok(proof) => Some(proof),
                Err(status) => {
                    let _ = reply.send(Err(status));
                    return;
                }
            }
        }
        None => {
            if let Err(status) = verify_loaded_tape_identity(drive, &tape_uuid) {
                let _ = reply.send(Err(status));
                return;
            }
            if let Err(status) = prepare_drive_for_read(index, drive, &tape_uuid, session_id) {
                let _ = reply.send(Err(status));
                return;
            }
            None
        }
    };
    // Load-time wrap-map harvest + fence install (design §6.5); same
    // rule as the write path: fresh mount only, after identity.
    if needs_drive_load {
        run_load_calibration_harvest(index, drive, cfg, &tape_uuid, barcode.as_deref());
    }
    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    if let Err(status) = record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id,
            session_kind: "read",
            event: AuditEvent::SessionOpened,
            tape_uuid: Some(tape_uuid),
            library_serial: Some(library_serial.clone()),
            drive_bay: Some(bay),
            drive_uuid: drive_uuid.clone(),
            drive_serial: drive_serial.clone(),
            abort_reason: None,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    let open_reply = read_session_proto(
        session_id,
        &tape_uuid,
        pb::read_session::State::ReadSessionStateOpen,
        opened_at_utc.as_str(),
        bay,
        position_proof,
        daemon_epoch,
    );
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::ReadFile {
                session_id: requested,
                object_id,
                file_id,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                if requested != session_id {
                    let _ =
                        chunk_tx.blocking_send(Err(Status::not_found("read session not found")));
                    continue;
                }
                let result = if file_id.is_empty() {
                    stream_one_object(
                        index,
                        drive,
                        cfg,
                        session_id,
                        &tape_uuid,
                        object_id.as_str(),
                        stream_chunk_bytes,
                        chunk_tx.clone(),
                    )
                } else {
                    String::from_utf8(file_id)
                        .map_err(|err| {
                            Status::invalid_argument(format!("file_id is not utf-8: {err}"))
                        })
                        .and_then(|file_id| {
                            stream_one_file_range(
                                index,
                                drive,
                                cfg,
                                session_id,
                                &tape_uuid,
                                object_id.as_str(),
                                file_id.as_str(),
                                0,
                                0,
                                stream_chunk_bytes,
                                chunk_tx.clone(),
                            )
                        })
                };
                if let Err(status) = result {
                    record_session_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        "read-failure",
                        snapshot_misses,
                    );
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::ReadObjectRange {
                session_id: requested,
                object_id,
                file_id,
                start_byte,
                end_byte,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                if requested != session_id {
                    let _ =
                        chunk_tx.blocking_send(Err(Status::not_found("read session not found")));
                    continue;
                }
                if let Err(status) = stream_one_file_range(
                    index,
                    drive,
                    cfg,
                    session_id,
                    &tape_uuid,
                    object_id.as_str(),
                    file_id.as_str(),
                    start_byte,
                    end_byte,
                    stream_chunk_bytes,
                    chunk_tx.clone(),
                ) {
                    record_session_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        "read-failure",
                        snapshot_misses,
                    );
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::CloseRead {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    record_session_close_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        snapshot_misses,
                    );
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateClosed,
                        opened_at_utc.as_str(),
                        bay,
                        position_proof,
                        daemon_epoch,
                    ))
                } else {
                    Err(Status::not_found("read session not found"))
                };
                if status.is_ok() {
                    if let Err(err) = record_session_event(
                        index,
                        cfg,
                        SessionAuditInput {
                            session_id,
                            session_kind: "read",
                            event: AuditEvent::SessionClosed,
                            tape_uuid: Some(tape_uuid),
                            library_serial: Some(library_serial.clone()),
                            drive_bay: Some(bay),
                            drive_uuid: drive_uuid.clone(),
                            drive_serial: drive_serial.clone(),
                            abort_reason: None,
                        },
                    ) {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                }
                let _ = reply.send(status);
                if requested == session_id {
                    break;
                }
            }
            DriveCommand::GetRead {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateOpen,
                        opened_at_utc.as_str(),
                        bay,
                        position_proof,
                        daemon_epoch,
                    ))
                } else {
                    Err(Status::not_found("read session not found"))
                };
                let _ = reply.send(status);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::TapeInventory { reply, .. } => {
                let message = "read session already active";
                let _ = reply.send(Err(Status::failed_precondition(message)));
            }
            DriveCommand::VerifyTapeIndex { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::FinalizeTape { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed("read session already active", &[("phase", "admission")]);
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "active session is a read session",
                    )));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
        }
    }
}

pub(super) fn session_open_reject_tape_io_fences(
    index: &CatalogIndex,
    tape_uuid: &TapeUuid,
    barcode: Option<&str>,
    action: &str,
) -> Result<(), Status> {
    let conflicts = index
        .tape_io_admission_conflicts(tape_uuid, barcode)
        .map_err(status_from_state_error)?;
    let Some(first) = conflicts.first() else {
        return Ok(());
    };
    Err(Status::failed_precondition(format!(
        "{action} blocked by active tape-I/O fence {} tape_uuid={} barcode={} reason={}; release via `rem tape quarantine release {}` before retrying",
        first.quarantine_id,
        Uuid::from_bytes(*tape_uuid),
        barcode.unwrap_or("(unknown)"),
        first.reason,
        first.quarantine_id
    )))
}
