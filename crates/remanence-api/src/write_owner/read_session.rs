//! Read-session opening and tape-I/O admission checks.

use crate::closeout_obligation::RetryCauseExt as _;
use remanence_library::DriveHandle;
use remanence_state::CatalogIndex;
use tokio::sync::mpsc;
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::DriveCommand;
use super::actor_runtime::{
    record_read_session, record_session_close_snapshot, record_session_snapshot,
    OpenReadActorRequest, ReadSessionInput, WriteOwnerConfig,
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

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_drive_open_read(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    admission_work: Option<crate::drive_pool::ClosingGuard>,
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

    let session_id = Uuid::new_v4();
    if let Some(lifecycle) = &cfg.lifecycle {
        lifecycle.set_holder(
            &crate::drive_pool::DriveKey::new(&library_serial, bay),
            drive_serial.as_deref(),
            "read session",
            session_id,
        );
    }

    let endpoint = remanence_state::DriveKey::new(&library_serial, bay);
    match index.tape_io_is_fenced(Some(&tape_uuid), barcode.as_deref(), Some(&endpoint)) {
        Ok(false) => {}
        result => {
            let status = match result {
                Err(error) => status_from_state_error(error),
                _ => match index.tape_io_admission_conflicts(&tape_uuid, barcode.as_deref()) {
                    Err(error) => status_from_state_error(error),
                    Ok(mut conflicts) => {
                        crate::tape_admission::order_conflicts(index, &mut conflicts);
                        let message = "session blocked by active tape-I/O fence".to_owned();
                        let message = if let Some(first) = conflicts.first() {
                            crate::tape_admission::fence_payload(
                                index,
                                tape_uuid,
                                first,
                                message,
                                true,
                                remanence_state::library_log::AuditActor::System,
                            )
                        } else {
                            message
                        };
                        Status::failed_precondition(message)
                    }
                },
            };
            let _ = reply.send(Err(status));
            return;
        }
    }
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
            load_continues_move: needs_drive_load,
            lifecycle: cfg.lifecycle.as_ref(),
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
    if let Err(status) = record_read_session(
        index,
        cfg,
        ReadSessionInput {
            session_id,
            event: remanence_state::library_log::SessionEvent::Opened,
            tape_uuid,
            endpoint: cfg
                .mounted()
                .map(|m| m.endpoint.clone())
                .unwrap_or_else(|| remanence_library::DriveKey::Bay {
                    library_serial: library_serial.clone(),
                    bay,
                }),
            drive_uuid: drive_uuid.clone(),
            drive_serial: drive_serial.clone(),
            reason: None,
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
        // As on ordinary close, leave the tape seated and let idle dismount
        // own UNLOAD plus MOVE. No session was registered by the lost caller.
        record_session_close_snapshot(
            index,
            cfg,
            drive,
            drive_uuid.clone(),
            session_id,
            tape_uuid,
            snapshot_misses,
        );
        if let Err(error) = record_read_session(
            index,
            cfg,
            ReadSessionInput {
                session_id,
                event: remanence_state::library_log::SessionEvent::Closed,
                tape_uuid,
                endpoint: cfg
                    .mounted()
                    .map(|m| m.endpoint.clone())
                    .unwrap_or_else(|| remanence_library::DriveKey::Bay {
                        library_serial: library_serial.clone(),
                        bay,
                    }),
                drive_uuid,
                drive_serial,
                reason: Some("read open reply receiver disappeared".into()),
            },
        ) {
            tracing::error!(bay, %error, "failed to audit cancelled read open close");
        }
        if let (Some(lifecycle), Some(home_slot)) = (&cfg.lifecycle, source_slot) {
            let parked = lifecycle.park_cartridge(crate::drive_pool::SeatedCartridge {
                bay,
                library_serial,
                barcode,
                home_slot,
                tape_uuid: Some(tape_uuid),
                prior_session_id: Some(session_id),
            });
            if let Some(sender) = &lifecycle.timer_park_tx {
                if sender.send(parked).is_err() {
                    tracing::error!(bay, "cancelled read open could not arm idle dismount");
                }
            }
        }
        tracing::warn!(
            bay,
            "read open reply receiver disappeared; cartridge stays seated"
        );
        return;
    }

    // The open reply hands live ownership to the session manager.
    drop(admission_work);
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::ReadFile {
                session_id: requested,
                object_id,
                file_id,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                chunk_tx
                    .set_shutdown_signal(cfg.lifecycle.as_ref().map(|l| l.shutting_down.clone()));
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
                chunk_tx
                    .set_shutdown_signal(cfg.lifecycle.as_ref().map(|l| l.shutting_down.clone()));
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
                    if let Err(err) = record_read_session(
                        index,
                        cfg,
                        ReadSessionInput {
                            session_id,
                            event: remanence_state::library_log::SessionEvent::Closed,
                            tape_uuid,
                            endpoint: cfg.mounted().map(|m| m.endpoint.clone()).unwrap_or_else(
                                || remanence_library::DriveKey::Bay {
                                    library_serial: library_serial.clone(),
                                    bay,
                                },
                            ),
                            drive_uuid: drive_uuid.clone(),
                            drive_serial: drive_serial.clone(),
                            reason: None,
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
            DriveCommand::OpenMountedWrite { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::FinishMountedInvocation { reply } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::FinalizeMounted { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::SupersedeMounted { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
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
            DriveCommand::CloseoutTape { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition("session already active")));
            }
            DriveCommand::SupersedeTail { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "drive has an active session",
                )));
            }
            DriveCommand::FinalizeTape { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::WaitReady {
                handle,
                reservation,
                ..
            } => {
                handle.publish_failed("read session already active", &[("phase", "admission")]);
                drop(reservation);
                handle.wake_readiness();
            }
            DriveCommand::PrepareRemoval { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "drive has an active session",
                )));
            }
            DriveCommand::Unload { reply, .. } => {
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
            DriveCommand::Close { reply, .. }
            | DriveCommand::Abort { reply, .. }
            | DriveCommand::DisconnectWrite { reply, .. } => {
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
    let mut conflicts = index
        .tape_io_admission_conflicts(tape_uuid, barcode)
        .map_err(status_from_state_error)?;
    crate::tape_admission::order_conflicts(index, &mut conflicts);
    let Some(first) = conflicts.first() else {
        if let Some(message) = crate::tape_admission::contradicted_payload(
            index,
            *tape_uuid,
            action,
            remanence_state::library_log::AuditActor::System,
        ) {
            return Err(crate::closeout_obligation::RetryCause::Operator
                .attach(Status::failed_precondition(message)));
        }
        return Ok(());
    };
    let message = crate::tape_admission::fence_payload(
        index,
        *tape_uuid,
        first,
        format!(
        "{action} blocked by active tape-I/O fence {} tape_uuid={} barcode={} reason={}; release via `rem tape quarantine release {}` before retrying",
        first.quarantine_id,
        Uuid::from_bytes(*tape_uuid),
        barcode.unwrap_or("(unknown)"),
        first.reason,
        first.quarantine_id
        ),
        true,
        remanence_state::library_log::AuditActor::System,
    );
    Err(crate::closeout_obligation::RetryCause::Operator
        .attach(Status::failed_precondition(message)))
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    use crate::tape_admission::test_support::{assert_code, drive, Fixture, T};
    use remanence_state::library_log as lib;

    /// The shared session site chooses the typed first conflict, preserves
    /// other payloads, and counts native-loss or operator-retry D3 once.
    #[test]
    fn l1b_session_admission_payload_precedence_and_count() {
        for case in ["native", "ordinary", "suffix", "contradicted"] {
            let mut f = Fixture::new();
            let expected = match case {
                "native" => {
                    f.raise(T, lib::FenceCause::NativeLogBehindProjection);
                    f.raise(T, lib::FenceCause::TransferError);
                    Some(1)
                }
                "ordinary" => {
                    f.raise(T, lib::FenceCause::TransferError);
                    None
                }
                "suffix" => {
                    f.raise(T, lib::FenceCause::SessionSuffixContradiction);
                    f.raise(T, lib::FenceCause::NativeLogBehindProjection);
                    None
                }
                _ => {
                    f.contradict(T);
                    Some(2)
                }
            };
            let index = f.index();
            let error = session_open_reject_tape_io_fences(&index, &T, None, "open read session")
                .unwrap_err();
            if case == "contradicted" {
                assert_eq!(error.metadata().get("remanence-retry").unwrap(), "operator");
            }
            if let Some(code) = expected {
                assert!(
                    error.message().starts_with(if code == 1 {
                        "TapeLogBehindProjection: "
                    } else {
                        "ContradictedTapeAdmission: "
                    }),
                    "{error}"
                );
            } else {
                let cause = if case == "suffix" {
                    "session_suffix_contradiction"
                } else {
                    "transfer_error"
                };
                let fence = index
                    .list_active_tape_io_fences()
                    .unwrap()
                    .into_iter()
                    .find(|r| r.reason == cause)
                    .unwrap();
                assert_eq!(error.message(),format!("open read session blocked by active tape-I/O fence {} tape_uuid={} barcode=(unknown) reason={cause}; release via `rem tape quarantine release {}` before retrying",fence.quarantine_id,Uuid::from_bytes(T),fence.quarantine_id));
            }
            drop(index);
            let records = f.refusals();
            assert_eq!(records.len(), usize::from(expected.is_some()));
            if let Some(code) = expected {
                assert_code(&records[0], T, code);
            }
        }
    }

    /// The read actor's first fence refusal is coded before its readiness probe.
    #[test]
    fn l1b_read_first_fence_payload_precedence_and_count() {
        for case in ["native", "ordinary", "suffix"] {
            let mut f = Fixture::new();
            if case == "suffix" {
                f.raise(T, lib::FenceCause::SessionSuffixContradiction);
            }
            if case != "ordinary" {
                f.raise(T, lib::FenceCause::NativeLogBehindProjection);
            }
            if case != "suffix" {
                f.raise(T, lib::FenceCause::TransferError);
            }
            let mut drive = drive();
            let cfg = f.config(&drive);
            let mut index = f.index();
            let (_tx, mut rx) = mpsc::channel(1);
            let (reply, mut answer) = tokio::sync::oneshot::channel();
            handle_drive_open_read(
                1,
                &mut index,
                &cfg,
                &mut rx,
                &mut drive,
                &mut 0,
                None,
                OpenReadActorRequest {
                    tape_uuid: T,
                    needs_drive_load: false,
                    library_serial: "ADMISSION".into(),
                    barcode: None,
                    source_slot: None,
                    drive_uuid: None,
                    drive_serial: None,
                    resume_target: None,
                    daemon_epoch: 1,
                    reply,
                },
            );
            let error = answer.try_recv().unwrap().unwrap_err();
            assert_eq!(
                error.message(),
                if case == "native" {
                    "TapeLogBehindProjection: session blocked by active tape-I/O fence"
                } else {
                    "session blocked by active tape-I/O fence"
                }
            );
            drop(index);
            drop(cfg);
            drop(drive);
            let records = f.refusals();
            assert_eq!(records.len(), usize::from(case == "native"));
            if case == "native" {
                assert_code(&records[0], T, 1);
            }
        }
    }
}
