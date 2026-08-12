//! Write-session state, append handling, close, and media preparation.

use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};

use ciborium::value::Value as CborValue;
use remanence_library::{DriveHandle, DriveHandleSink, DriveHandleSource, TapeConfig};
use remanence_parity::DriveHandleRawSink;
use remanence_state::{
    AuditActor, AuditEvent, CatalogIndex, SourceLayer, TapeIoFenceRecord, TapePoolConfig,
    TerminalFinalizationOutcome, TerminalFinalizationProjectionInput,
};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::DriveCommand;
use super::actor_runtime::{
    insert_optional_audit_text, record_session_close_snapshot, record_session_event,
    record_session_snapshot, OpenWriteActorRequest, SessionAuditInput, WriteOwnerConfig,
};
use super::checkpoint::{
    arm_checkpoint_idle_close, arm_checkpoint_timer, extend_durable_checkpoint_records,
    fence_failed_checkpoint_batch, fence_failed_parity_raw_write, open_parity_actor_session,
    park_timer_closed_session, perform_checkpoint_barrier, project_checkpoint_authority_bounded,
    validate_parity_actor_authority, ParityActorSession, PendingCheckpointBatch, SessionAppendGate,
};
use super::read_session::session_open_reject_tape_io_fences;
use super::readiness::session_open_short_probe_or_load;
use super::restore::{
    now_rfc3339, session_proto, status_from_pool_write_error, WriteSessionProtoInput,
};
use super::terminal_finalize::{
    finalize_terminal_no_parity, finalize_terminal_with_parity,
    finalize_terminal_with_parity_journal,
};
use super::terminal_types::TerminalFinalizeSpec;
use super::{
    retain_replayed_committed_receipt, send_checkpoint_actor_reply,
    validate_provisional_replay_guards, AppendFinishOutcome, CheckpointTrigger,
    CloseWriteActorDiagnostics, CloseWriteActorReply, SelectedTape, SessionOpenReadinessContext,
    TapeUuid,
};
use crate::audit_projection::{
    append_and_project_audit, ensure_tape_sealed_audit, ProjectedAuditInput,
};
use crate::{
    pb, status_from_state_error, verify_tape_identity, PoolWriteError, WriteObjectToPoolRequest,
};

pub(super) struct CloseWriteActorInput<'a> {
    index: &'a mut CatalogIndex,
    cfg: &'a WriteOwnerConfig,
    drive: &'a mut DriveHandle,
    drive_uuid: &'a Option<Vec<u8>>,
    drive_serial: &'a Option<String>,
    snapshot_misses: &'a mut u32,
    session_id: Uuid,
    tape_uuid: TapeUuid,
    target_kind: pb::write_session::TargetKind,
    library_serial: &'a str,
    bay: u16,
    objects_committed: u64,
    bytes_committed: u64,
    opened_at_utc: &'a str,
    last_checkpoint_at_utc: Option<&'a str>,
    state: pb::write_session::State,
    append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics,
    checkpointed_objects: &'a [pb::ObjectRecord],
    /// Set only on the abort path, and only when the caller gave a reason.
    abort_reason: Option<String>,
}

pub(super) fn close_write_actor(
    input: CloseWriteActorInput<'_>,
) -> Result<CloseWriteActorReply, Status> {
    let mut diagnostics = CloseWriteActorDiagnostics {
        filemark_write_drain: input.append_commit_diagnostics.filemark_write_drain,
        catalog_journal_fsync: input.append_commit_diagnostics.catalog_journal_fsync,
        ..CloseWriteActorDiagnostics::default()
    };

    let snapshot_started = Instant::now();
    record_session_close_snapshot(
        input.index,
        input.cfg,
        input.drive,
        input.drive_uuid.clone(),
        input.session_id,
        input.tape_uuid,
        input.snapshot_misses,
    );
    diagnostics.drive_snapshot = snapshot_started.elapsed();

    let mut session = session_proto(WriteSessionProtoInput {
        session_id: input.session_id,
        tape_uuid: &input.tape_uuid,
        target_kind: input.target_kind,
        state: input.state,
        objects_committed: input.objects_committed,
        bytes_committed: input.bytes_committed,
        opened_at_utc: input.opened_at_utc,
        last_checkpoint_at_utc: input.last_checkpoint_at_utc,
        drive_element_address: input.bay,
        pending_batch: None,
    });
    session.checkpointed_objects = input.checkpointed_objects.to_vec();
    session.committed_copies = input
        .checkpointed_objects
        .iter()
        .flat_map(|object| object.copies.iter().cloned())
        .collect();
    let audit_started = Instant::now();
    record_session_event(
        input.index,
        input.cfg,
        SessionAuditInput {
            session_id: input.session_id,
            session_kind: "write",
            event: AuditEvent::SessionClosed,
            tape_uuid: Some(input.tape_uuid),
            library_serial: Some(input.library_serial.to_string()),
            drive_bay: Some(input.bay),
            drive_uuid: input.drive_uuid.clone(),
            drive_serial: input.drive_serial.clone(),
            abort_reason: input.abort_reason,
        },
    )?;
    diagnostics.session_audit_projection = audit_started.elapsed();

    Ok(CloseWriteActorReply {
        session,
        diagnostics,
    })
}

/// Mutable authority owned by one drive actor for the lifetime of an open
/// write session. Command handlers borrow this state so each transition's
/// inputs and mutations remain local and visible to the borrow checker.
type DeferredWriteCloseReply = (
    oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    CloseWriteActorReply,
);

pub(super) struct WriteSessionState<'a> {
    bay: u16,
    index: &'a mut CatalogIndex,
    cfg: &'a WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    drive: &'a mut DriveHandle,
    snapshot_misses: &'a mut u32,
    pool_cfg: TapePoolConfig,
    selected: SelectedTape,
    target_kind: pb::write_session::TargetKind,
    library_serial: String,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    session_id: Uuid,
    tape_uuid: TapeUuid,
    opened_at_utc: String,
    objects_committed: u64,
    bytes_committed: u64,
    last_checkpoint_at_utc: Option<String>,
    checkpoint_lease: remanence_state::FileCheckpointJournalLease,
    durable_checkpoint_records: Vec<remanence_state::CheckpointJournalRecord>,
    parity_session: Option<ParityActorSession>,
    next_batched_append: Option<crate::pool_write::BatchedNoParityAppendContext>,
    checkpoint_ordinal: u64,
    tape_committed_object_count: u64,
    pending_batch: Option<PendingCheckpointBatch>,
    committed_receipts: Vec<pb::ObjectRecord>,
    timer_checkpoint_waiting: Option<Uuid>,
    append_gate: SessionAppendGate,
    append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics,
    deferred_close_reply: Option<DeferredWriteCloseReply>,
}

impl WriteSessionState<'_> {
    /// Dispatch commands until a close transition releases the drive actor.
    ///
    /// A successful close reply is returned to the caller so the outer owner
    /// can drop this state, including its checkpoint lease, before
    /// acknowledging the transition.
    fn run(&mut self, rx: &mut mpsc::Receiver<DriveCommand>) -> Option<DeferredWriteCloseReply> {
        while let Some(command) = rx.blocking_recv() {
            match command {
                command @ DriveCommand::AppendFinish { .. } => {
                    self.handle_append_finish(command);
                }
                command @ DriveCommand::Checkpoint { .. } => {
                    self.handle_checkpoint(command);
                }
                command @ DriveCommand::TimerIdleClose { .. } => {
                    if self.handle_timer_idle_close(command) {
                        break;
                    }
                }
                command @ DriveCommand::Close { .. } => {
                    if self.handle_close(command) {
                        break;
                    }
                }
                command @ DriveCommand::Abort { .. } => {
                    if self.handle_abort(command) {
                        break;
                    }
                }
                command @ DriveCommand::Get { .. } => {
                    self.handle_get(command);
                }
                DriveCommand::OpenWrite { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::OpenRead { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::TapeInventory { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::VerifyTapeIndex { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::FinalizeTape { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::WaitReady { handle, .. } => {
                    handle
                        .publish_failed("write session already active", &[("phase", "admission")]);
                }
                DriveCommand::Unload { reply } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::PollHealth { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::Heartbeat { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::ReadFile { chunk_tx, .. }
                | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                    let _ = chunk_tx.blocking_send(Err(Status::failed_precondition(
                        "active session is a write session",
                    )));
                }
                DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "active session is a write session",
                    )));
                }
            }
        }
        self.deferred_close_reply.take()
    }

    /// Complete one accepted append and advance provisional checkpoint state.
    fn handle_append_finish(&mut self, command: DriveCommand) {
        let DriveCommand::AppendFinish {
            session_id: requested,
            source,
            archive_path,
            caller_object_id,
            expected_content_sha256,
            expected_object_id,
            input_kind,
            live_write_counter,
            reply,
        } = command
        else {
            unreachable!("append handler received another drive command");
        };
        let cfg = self.cfg;
        let pool_cfg = self.pool_cfg.clone();
        let selected = self.selected.clone();
        let drive_uuid = self.drive_uuid.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            source.remove_completed_path();
            let _ = reply.send(Err(Status::not_found("write session not found")));
            return;
        }
        if let Err(status) = self.append_gate.check() {
            source.remove_completed_path();
            let _ = reply.send(Err(status));
            return;
        }
        // Any accepted append call is session activity, including a
        // catalog idempotency replay; invalidate a prior timer-close.
        self.timer_checkpoint_waiting = None;
        let source_size = source.size_bytes().unwrap_or(0);
        let stream_control = source.stream_control();
        let cleanup_path = match &source {
            crate::WriteObjectSource::Path(path) => Some(path.clone()),
            crate::WriteObjectSource::Streamed(_) => None,
        };
        let current_caller_object_id = caller_object_id.clone();
        if let Some((provisional_index, pending)) = self.pending_batch.as_ref().and_then(|batch| {
            batch
                .objects
                .iter()
                .enumerate()
                .find(|(_, pending)| pending.object.caller_object_id == caller_object_id)
                .map(|(index, pending)| (index, (batch.batch_id, pending)))
        }) {
            let (batch_id, pending) = pending;
            let pending_archive_path =
                if pending.input_kind() == crate::WriteObjectInputKind::LogicalFile {
                    match pending
                        .checkpoint_projection()
                        .map(|projection| projection.files.as_slice())
                    {
                        Some([file]) => Some(file.path.as_str()),
                        _ => {
                            source.remove_completed_path();
                            let _ = reply.send(Err(Status::internal(
                                "pending logical-file replay has an invalid member-path projection",
                            )));
                            return;
                        }
                    }
                } else {
                    None
                };
            let requested_hash = source.content_sha256();
            source.remove_completed_path();
            match requested_hash {
                Ok(hash) => match validate_provisional_replay_guards(
                    &caller_object_id,
                    pending.input_kind(),
                    pending_archive_path,
                    pending.object.object_id,
                    pending.object.content_sha256,
                    input_kind,
                    &archive_path,
                    expected_object_id,
                    expected_content_sha256,
                    hash,
                ) {
                    Ok(()) => {
                        let provisional_ordinal = provisional_index as u64 + 1;
                        let record = pending
                            .object
                            .to_written_proto(batch_id, provisional_ordinal);
                        let _ = reply.send(Ok(AppendFinishOutcome {
                            record,
                            replay: true,
                        }));
                    }
                    Err(status) => {
                        let _ = reply.send(Err(status));
                    }
                },
                Err(err) => {
                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                }
            }
            return;
        }
        let request = WriteObjectToPoolRequest {
            pool_id: pool_cfg.id.clone(),
            source,
            archive_path,
            caller_object_id,
            expected_content_sha256,
            expected_object_id,
            representation: crate::PoolWriteRepresentation::Plaintext,
            input_kind,
        };
        let mut write_admission = match cfg.write_admissions.reserve(
            pool_cfg.id.as_str(),
            request.caller_object_id.as_str(),
            request.expected_object_id,
        ) {
            Ok(reservation) => Some(reservation),
            Err(status) => {
                request.source.remove_completed_path();
                let _ = reply.send(Err(status));
                return;
            }
        };
        let append_started = Instant::now();
        let mut parity_raw_write_attempted = false;
        let result = match crate::pool_write::maybe_replay_pool_write(
            self.index, &pool_cfg, &request,
        ) {
            Ok(Some(result)) => Ok(result),
            Ok(None) => {
                if let Some(parity_session) = self.parity_session.as_mut() {
                    let mut raw = DriveHandleRawSink::new(self.drive);
                    match parity_session.journal.as_mut() {
                            Some(parity_journal) => {
                                crate::pool_write::write_batched_parity_to_selected_tape_after_replay_check(
                                    self.index,
                                    &mut raw,
                                    parity_journal,
                                    &mut parity_session.sink_state,
                                    &pool_cfg,
                                    request,
                                    selected.clone(),
                                    &cfg.io_memory,
                                    &mut parity_raw_write_attempted,
                                )
                            }
                            None => Err(PoolWriteError::InvalidInput(
                                "active parity session has no append journal".to_string(),
                            )),
                        }
                } else {
                    let mut sink = DriveHandleSink(self.drive);
                    if let Some(append) = self.next_batched_append.clone() {
                        self.next_batched_append = Some(append.clone());
                        crate::pool_write::write_batched_to_selected_tape_after_replay_check(
                            self.index,
                            &mut sink,
                            &pool_cfg,
                            request,
                            selected.clone(),
                            live_write_counter,
                            append,
                        )
                    } else {
                        Err(PoolWriteError::InvalidInput(
                            "checkpoint append context is unavailable".to_string(),
                        ))
                    }
                }
            }
            Err(err) => Err(err),
        };
        let append_elapsed = append_started.elapsed();
        if let Some(path) = cleanup_path {
            let _ = std::fs::remove_file(path);
        }
        match result {
            Ok(result) => {
                let logical_size = result.object.logical_size_bytes;
                let replay = result.is_replay();
                self.append_commit_diagnostics
                    .accumulate(result.append_commit_diagnostics());
                let response_record = if !replay {
                    if let Some(previous) = self.next_batched_append.as_ref() {
                        let next_context =
                            match crate::pool_write::next_batched_append_context(previous, &result)
                            {
                                Ok(context) => context,
                                Err(err) => {
                                    self.append_gate.record_failure();
                                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                                    return;
                                }
                            };
                        self.next_batched_append = Some(next_context);
                    }
                    let new_batch = self.pending_batch.is_none();
                    let batch = self.pending_batch.get_or_insert_with(|| {
                        PendingCheckpointBatch::new(StdDuration::from_secs(
                            cfg.checkpoint_max_age_seconds,
                        ))
                    });
                    let provisional_ordinal = batch.objects.len() as u64 + 1;
                    let written = result
                        .object
                        .to_written_proto(batch.batch_id, provisional_ordinal);
                    let object_id = result.object.object_id;
                    let batch_id = batch.batch_id;
                    batch.push(
                        logical_size,
                        result,
                        write_admission
                            .take()
                            .expect("new tape write retains its admission through checkpoint"),
                    );
                    let timer_arm_failed = if new_batch {
                        match arm_checkpoint_timer(
                            self.actor_tx.clone(),
                            session_id,
                            batch_id,
                            StdDuration::from_secs(cfg.checkpoint_max_age_seconds),
                        ) {
                            Ok(()) => false,
                            Err(err) => {
                                tracing::error!(
                                    session_id = %session_id,
                                    batch_id = %batch_id,
                                    error = %err,
                                    "checkpoint timer could not start; forcing an immediate barrier"
                                );
                                true
                            }
                        }
                    } else {
                        false
                    };
                    if timer_arm_failed || batch.should_checkpoint(cfg) {
                        let outcome = perform_checkpoint_barrier(
                            self.index,
                            self.drive,
                            &mut self.checkpoint_lease,
                            tape_uuid,
                            &mut self.checkpoint_ordinal,
                            &mut self.tape_committed_object_count,
                            batch,
                            self.parity_session.as_mut(),
                            &selected,
                            &pool_cfg,
                            cfg,
                        );
                        match outcome {
                            Ok(outcome) => {
                                if let Some(previous) = self.next_batched_append.as_ref() {
                                    let checkpoint_context = match crate::pool_write::batched_append_context_after_checkpoint(
                                        previous,
                                        &outcome.checkpoint_record,
                                    ) {
                                        Ok(context) => context,
                                        Err(err) => {
                                            extend_durable_checkpoint_records(
                                                &mut self.durable_checkpoint_records,
                                                &outcome,
                                            );
                                            self.append_gate.record_failure();
                                            self.pending_batch = None;
                                            let _ = reply.send(Err(status_from_pool_write_error(err)));
                                            return;
                                        }
                                    };
                                    self.next_batched_append = Some(checkpoint_context);
                                }
                                extend_durable_checkpoint_records(
                                    &mut self.durable_checkpoint_records,
                                    &outcome,
                                );
                                self.objects_committed =
                                    self.objects_committed.saturating_add(outcome.object_count);
                                self.bytes_committed =
                                    self.bytes_committed.saturating_add(outcome.logical_bytes);
                                self.last_checkpoint_at_utc =
                                    Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                                self.append_commit_diagnostics.accumulate(
                                    crate::pool_write::AppendCommitDiagnostics {
                                        filemark_write_drain: outcome.filemark_drain,
                                        catalog_journal_fsync: outcome.journal_projection,
                                    },
                                );
                                if outcome.sealed_after_write {
                                    self.append_gate.record_sealed();
                                }
                                let committed = outcome
                                    .committed_objects
                                    .iter()
                                    .find(|record| record.object_id == object_id)
                                    .cloned()
                                    .expect("threshold checkpoint returns current object");
                                self.committed_receipts.extend(outcome.committed_objects);
                                self.pending_batch = None;
                                committed
                            }
                            Err(failure) => {
                                let quarantine_identities = failure.requires_identity_quarantine();
                                let status = if failure.journal_durable || failure.fence_handled {
                                    failure.status
                                } else {
                                    fence_failed_checkpoint_batch(
                                        self.index,
                                        cfg,
                                        &selected,
                                        batch,
                                        failure.status,
                                    )
                                };
                                self.append_gate.record_failure();
                                if quarantine_identities {
                                    self.pending_batch
                                        .as_mut()
                                        .expect("checkpoint failure retains its pending batch")
                                        .quarantine_write_admissions_until_restart();
                                }
                                self.pending_batch = None;
                                let _ = reply.send(Err(status));
                                return;
                            }
                        }
                    } else {
                        written
                    }
                } else {
                    if !replay {
                        self.objects_committed = self.objects_committed.saturating_add(1);
                        self.bytes_committed = self.bytes_committed.saturating_add(logical_size);
                        self.last_checkpoint_at_utc =
                            Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                    }
                    result.object.to_proto()
                };
                tracing::info!(
                    target: "remanence_write_diag",
                    phase = "drive_append_total",
                    session_id = %session_id,
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    payload_bytes = source_size,
                    block_size_bytes = selected.block_size,
                    replay,
                    elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                    throughput_mib_s = if replay {
                        0.0
                    } else {
                        crate::diagnostics::mib_per_s(logical_size, append_elapsed)
                    },
                    "remanence_write_diag",
                );
                if replay {
                    retain_replayed_committed_receipt(
                        &mut self.committed_receipts,
                        &response_record,
                    );
                }
                let _ = reply.send(Ok(AppendFinishOutcome {
                    record: response_record,
                    replay,
                }));
            }
            Err(err) => {
                let terminal_trigger = match &err {
                    PoolWriteError::TerminalCloseRequired { .. } => {
                        Some(remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits)
                    }
                    _ => None,
                };
                if let Some(terminal_trigger) =
                    terminal_trigger.filter(|_| self.pending_batch.is_none())
                {
                    let original_error = err.to_string();
                    let terminal_reason = match terminal_trigger {
                        remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits => {
                            "whole-Object terminal-capacity rollover"
                        }
                        _ => "automatic terminal finalization",
                    };
                    if self.durable_checkpoint_records.is_empty() {
                        let status = Status::resource_exhausted(format!(
                            "{terminal_reason} rejected the Object on a fresh tape; no Object was written and the tape may accept a smaller object: {original_error}"
                        ));
                        let _ = reply.send(Err(status));
                        return;
                    }
                    let drive_config = match self.drive.read_config() {
                        Ok(config) => config,
                        Err(error) => {
                            self.append_gate.record_failure();
                            let _ = reply.send(Err(Status::unavailable(format!(
                                "read {terminal_reason} drive config: {error}"
                            ))));
                            return;
                        }
                    };
                    if drive_config.write_protected {
                        self.append_gate.record_failure();
                        let _ = reply.send(Err(Status::failed_precondition(format!(
                            "tape is write-protected and cannot complete {terminal_reason} finalization"
                        ))));
                        return;
                    }
                    let rewritable = matches!(
                        drive_config.worm,
                        remanence_library::WormMediaState::NotWorm
                    );
                    let spec =
                        TerminalFinalizeSpec::automatic(&selected, &pool_cfg, terminal_trigger);
                    let terminal_result = match &selected.parity_config {
                        remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
                            self.index,
                            cfg,
                            self.drive,
                            &mut self.checkpoint_lease,
                            self.durable_checkpoint_records
                                .last()
                                .expect("automatic finalization requires checkpoint authority"),
                            None,
                            &spec,
                            &selected,
                            None,
                            rewritable,
                        ),
                        remanence_parity::ParityConfig::Scheme(_) => {
                            match self
                                .parity_session
                                .as_mut()
                                .and_then(|session| session.journal.take())
                            {
                                Some(parity_journal) => finalize_terminal_with_parity_journal(
                                    self.index,
                                    cfg,
                                    self.drive,
                                    &mut self.checkpoint_lease,
                                    self.durable_checkpoint_records.last().expect(
                                        "automatic finalization requires checkpoint authority",
                                    ),
                                    None,
                                    &spec,
                                    &selected,
                                    None,
                                    parity_journal,
                                    rewritable,
                                ),
                                None => Err(Status::internal(
                                    "automatic parity rollover has no session-owned append journal",
                                )),
                            }
                        }
                    };
                    match terminal_result {
                        Ok(result) => {
                            self.append_gate.record_sealed();
                            if let Some(record) = result.final_record {
                                self.checkpoint_ordinal = record.ordinal;
                                self.durable_checkpoint_records.clear();
                                self.durable_checkpoint_records.push(record);
                                if let Err(error) =
                                    append_tape_sealed_evidence(self.index, cfg, selected.tape_uuid)
                                {
                                    tracing::warn!(%error, %terminal_reason, "failed to append automatic terminal seal evidence");
                                }
                                let _ = reply.send(Err(Status::resource_exhausted(
                                    format!(
                                        "{terminal_reason} was reached before Object tape motion; selected tape was terminally finalized, reopen against the pool to roll placement: {original_error}"
                                    ),
                                )));
                            } else {
                                let _ = reply.send(Err(Status::unavailable(format!(
                                    "{terminal_reason} permanently closed Object admission, but the terminal tail requires recovery: {original_error}"
                                ))));
                            }
                        }
                        Err(error) => {
                            self.append_gate.record_sealed();
                            let _ = reply.send(Err(error));
                        }
                    }
                    return;
                }
                let tape_started = if self.parity_session.is_some() {
                    parity_raw_write_attempted
                } else {
                    stream_control
                        .as_ref()
                        .map(|control| control.tape_started())
                        .unwrap_or(true)
                };
                if tape_started {
                    self.append_gate.record_failure();
                }
                let original_error = err.to_string();
                let mut status = status_from_pool_write_error(err);
                let mut fence_audited_here = false;
                if tape_started {
                    let failed_batch = self.pending_batch.take();
                    if let Some(batch) = failed_batch.as_ref() {
                        status = Status::unavailable(format!(
                            "checkpoint batch {} failed while appending {}; re-send all {} prior WRITTEN objects and the current object: {original_error}",
                            batch.batch_id,
                            current_caller_object_id,
                            batch.objects.len(),
                        ));
                    }
                    if parity_raw_write_attempted {
                        let fenced = fence_failed_parity_raw_write(
                            self.index,
                            cfg,
                            &selected,
                            "append",
                            Some(current_caller_object_id.as_str()),
                            failed_batch.as_ref(),
                            original_error.as_str(),
                            status,
                        );
                        status = fenced.0;
                        // The helper either audits the exact fence it just
                        // persisted or returns an error describing why it
                        // could not. Never fall back to auditing an unrelated
                        // "latest" active fence for this write failure.
                        fence_audited_here = true;
                    } else if let Some(batch) = failed_batch.as_ref() {
                        status = fence_failed_checkpoint_batch(
                            self.index, cfg, &selected, batch, status,
                        );
                        fence_audited_here = true;
                    }
                }
                tracing::info!(
                    target: "remanence_write_diag",
                    phase = "drive_append_total",
                    session_id = %session_id,
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    payload_bytes = source_size,
                    block_size_bytes = selected.block_size,
                    status = "error",
                    error = %status,
                    elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                    throughput_mib_s = crate::diagnostics::mib_per_s(source_size, append_elapsed),
                    "remanence_write_diag",
                );
                if !fence_audited_here {
                    if let Err(audit_err) =
                        append_latest_tape_io_fence_evidence(self.index, cfg, selected.tape_uuid)
                    {
                        tracing::warn!(
                            "failed to append tape-I/O fence evidence after write error: {audit_err}"
                        );
                    }
                }
                record_session_snapshot(
                    self.index,
                    cfg,
                    self.drive,
                    drive_uuid.clone(),
                    session_id,
                    tape_uuid,
                    "append-failure",
                    self.snapshot_misses,
                );
                let _ = reply.send(Err(status));
            }
        }
    }

    /// Commit the pending checkpoint batch, preserving timer-trigger behavior.
    fn handle_checkpoint(&mut self, command: DriveCommand) {
        let DriveCommand::Checkpoint {
            session_id: requested,
            trigger,
            expected_batch_id,
            reply,
        } = command
        else {
            unreachable!("checkpoint handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let pool_cfg = self.pool_cfg.clone();
        let selected = self.selected.clone();
        let target_kind = self.target_kind;
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            if let Some(reply) = reply {
                let _ = reply.send(Err(Status::not_found("write session not found")));
            }
            return;
        }
        let Some(batch) = self.pending_batch.as_ref() else {
            if let Some(reply) = reply {
                let session = session_proto(WriteSessionProtoInput {
                    session_id,
                    tape_uuid: &tape_uuid,
                    target_kind,
                    state: pb::write_session::State::WriteSessionStateCheckpointed,
                    objects_committed: self.objects_committed,
                    bytes_committed: self.bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
                    drive_element_address: bay,
                    pending_batch: None,
                });
                send_checkpoint_actor_reply(reply, session, &mut self.committed_receipts);
            }
            return;
        };
        if expected_batch_id.is_some_and(|expected| expected != batch.batch_id) {
            return;
        }
        let timer_batch_id = batch.batch_id;
        if trigger == CheckpointTrigger::Timer
            && Instant::now() > batch.deadline + StdDuration::from_secs(1)
        {
            let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
            let detail = serde_json::json!({
                "session_id": session_id.to_string(),
                "batch_id": batch.batch_id.to_string(),
                "deadline_overrun_seconds": Instant::now()
                    .saturating_duration_since(batch.deadline)
                    .as_secs(),
            })
            .to_string();
            let _ = self.index.raise_alarm(
                condition_key.as_str(),
                "checkpoint-barrier-overdue",
                "warning",
                Some(detail.as_str()),
            );
        }
        match perform_checkpoint_barrier(
            self.index,
            self.drive,
            &mut self.checkpoint_lease,
            tape_uuid,
            &mut self.checkpoint_ordinal,
            &mut self.tape_committed_object_count,
            batch,
            self.parity_session.as_mut(),
            &selected,
            &pool_cfg,
            cfg,
        ) {
            Ok(outcome) => {
                if let Some(previous) = self.next_batched_append.as_ref() {
                    let checkpoint_context =
                        match crate::pool_write::batched_append_context_after_checkpoint(
                            previous,
                            &outcome.checkpoint_record,
                        ) {
                            Ok(context) => context,
                            Err(err) => {
                                extend_durable_checkpoint_records(
                                    &mut self.durable_checkpoint_records,
                                    &outcome,
                                );
                                self.append_gate.record_failure();
                                self.pending_batch = None;
                                let status = status_from_pool_write_error(err);
                                if let Some(reply) = reply {
                                    let _ = reply.send(Err(status));
                                } else {
                                    tracing::error!(session_id = %session_id, error = %status, "checkpoint committed but next append context failed");
                                }
                                return;
                            }
                        };
                    self.next_batched_append = Some(checkpoint_context);
                }
                extend_durable_checkpoint_records(&mut self.durable_checkpoint_records, &outcome);
                self.objects_committed =
                    self.objects_committed.saturating_add(outcome.object_count);
                self.bytes_committed = self.bytes_committed.saturating_add(outcome.logical_bytes);
                self.last_checkpoint_at_utc =
                    Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                self.append_commit_diagnostics.accumulate(
                    crate::pool_write::AppendCommitDiagnostics {
                        filemark_write_drain: outcome.filemark_drain,
                        catalog_journal_fsync: outcome.journal_projection,
                    },
                );
                if outcome.sealed_after_write {
                    self.append_gate.record_sealed();
                }
                self.pending_batch = None;
                let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
                let _ = self.index.clear_alarm(condition_key.as_str());
                if trigger == CheckpointTrigger::Timer {
                    self.timer_checkpoint_waiting = Some(timer_batch_id);
                    if let Err(err) = arm_checkpoint_idle_close(
                        self.actor_tx.clone(),
                        session_id,
                        timer_batch_id,
                        StdDuration::from_secs(cfg.session_idle_seconds),
                    ) {
                        let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
                        let detail = serde_json::json!({
                            "session_id": session_id.to_string(),
                            "batch_id": timer_batch_id.to_string(),
                            "error": format!("idle close timer spawn failed: {err}"),
                        })
                        .to_string();
                        let _ = self.index.raise_alarm(
                            condition_key.as_str(),
                            "checkpoint-barrier-overdue",
                            "error",
                            Some(detail.as_str()),
                        );
                        tracing::error!(
                            session_id = %session_id,
                            batch_id = %timer_batch_id,
                            error = %err,
                            "checkpoint idle-close timer could not start"
                        );
                    }
                }
                self.committed_receipts.extend(outcome.committed_objects);
                let session = session_proto(WriteSessionProtoInput {
                    session_id,
                    tape_uuid: &tape_uuid,
                    target_kind,
                    state: pb::write_session::State::WriteSessionStateCheckpointed,
                    objects_committed: self.objects_committed,
                    bytes_committed: self.bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
                    drive_element_address: bay,
                    pending_batch: None,
                });
                if let Some(reply) = reply {
                    send_checkpoint_actor_reply(reply, session, &mut self.committed_receipts);
                }
            }
            Err(failure) => {
                let quarantine_identities = failure.requires_identity_quarantine();
                let status = if failure.journal_durable || failure.fence_handled {
                    failure.status
                } else {
                    fence_failed_checkpoint_batch(self.index, cfg, &selected, batch, failure.status)
                };
                self.append_gate.record_failure();
                if quarantine_identities {
                    self.pending_batch
                        .as_mut()
                        .expect("checkpoint failure retains its pending batch")
                        .quarantine_write_admissions_until_restart();
                }
                self.pending_batch = None;
                if let Some(reply) = reply {
                    let _ = reply.send(Err(status));
                } else {
                    tracing::error!(
                        session_id = %session_id,
                        batch_id = %timer_batch_id,
                        error = %status,
                        "timer-fired checkpoint barrier failed"
                    );
                }
            }
        }
    }

    /// Close an idle session only when the timer still names its latest batch.
    fn handle_timer_idle_close(&mut self, command: DriveCommand) -> bool {
        let DriveCommand::TimerIdleClose {
            session_id: requested,
            checkpoint_batch_id,
        } = command
        else {
            unreachable!("idle-close handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let target_kind = self.target_kind;
        let library_serial = self.library_serial.clone();
        let drive_uuid = self.drive_uuid.clone();
        let drive_serial = self.drive_serial.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id
            || self.timer_checkpoint_waiting != Some(checkpoint_batch_id)
            || self.pending_batch.is_some()
        {
            return false;
        }
        let result = close_write_actor(CloseWriteActorInput {
            index: self.index,
            cfg,
            drive: self.drive,
            drive_uuid: &drive_uuid,
            drive_serial: &drive_serial,
            snapshot_misses: self.snapshot_misses,
            session_id,
            tape_uuid,
            target_kind,
            library_serial: library_serial.as_str(),
            bay,
            objects_committed: self.objects_committed,
            bytes_committed: self.bytes_committed,
            opened_at_utc: opened_at_utc.as_str(),
            last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
            state: pb::write_session::State::WriteSessionStateClosed,
            append_commit_diagnostics: self.append_commit_diagnostics,
            checkpointed_objects: &self.committed_receipts,
            abort_reason: None,
        });
        if let Err(err) = result {
            tracing::error!(session_id = %session_id, error = %err, "idle checkpoint close failed");
            return false;
        }
        if let Err(err) = park_timer_closed_session(cfg, session_id) {
            tracing::error!(session_id = %session_id, error = %err, "timer-closed session could not enter idle eviction");
        }
        true
    }

    /// Flush any pending batch and perform a normal session close.
    fn handle_close(&mut self, command: DriveCommand) -> bool {
        let DriveCommand::Close {
            session_id: requested,
            reply,
        } = command
        else {
            unreachable!("close handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let pool_cfg = self.pool_cfg.clone();
        let selected = self.selected.clone();
        let target_kind = self.target_kind;
        let library_serial = self.library_serial.clone();
        let drive_uuid = self.drive_uuid.clone();
        let drive_serial = self.drive_serial.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            let _ = reply.send(Err(Status::not_found("write session not found")));
            return false;
        }
        if let Some(batch) = self.pending_batch.as_ref() {
            match perform_checkpoint_barrier(
                self.index,
                self.drive,
                &mut self.checkpoint_lease,
                tape_uuid,
                &mut self.checkpoint_ordinal,
                &mut self.tape_committed_object_count,
                batch,
                self.parity_session.as_mut(),
                &selected,
                &pool_cfg,
                cfg,
            ) {
                Ok(outcome) => {
                    if let Some(previous) = self.next_batched_append.as_ref() {
                        let checkpoint_context =
                            match crate::pool_write::batched_append_context_after_checkpoint(
                                previous,
                                &outcome.checkpoint_record,
                            ) {
                                Ok(context) => context,
                                Err(err) => {
                                    extend_durable_checkpoint_records(
                                        &mut self.durable_checkpoint_records,
                                        &outcome,
                                    );
                                    self.append_gate.record_failure();
                                    self.pending_batch = None;
                                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                                    return false;
                                }
                            };
                        self.next_batched_append = Some(checkpoint_context);
                    }
                    extend_durable_checkpoint_records(
                        &mut self.durable_checkpoint_records,
                        &outcome,
                    );
                    self.objects_committed =
                        self.objects_committed.saturating_add(outcome.object_count);
                    self.bytes_committed =
                        self.bytes_committed.saturating_add(outcome.logical_bytes);
                    self.last_checkpoint_at_utc =
                        Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                    self.append_commit_diagnostics.accumulate(
                        crate::pool_write::AppendCommitDiagnostics {
                            filemark_write_drain: outcome.filemark_drain,
                            catalog_journal_fsync: outcome.journal_projection,
                        },
                    );
                    if outcome.sealed_after_write {
                        self.append_gate.record_sealed();
                    }
                    self.committed_receipts.extend(outcome.committed_objects);
                    self.pending_batch = None;
                }
                Err(failure) => {
                    let quarantine_identities = failure.requires_identity_quarantine();
                    let status = if failure.journal_durable || failure.fence_handled {
                        failure.status
                    } else {
                        fence_failed_checkpoint_batch(
                            self.index,
                            cfg,
                            &selected,
                            batch,
                            failure.status,
                        )
                    };
                    self.append_gate.record_failure();
                    if quarantine_identities {
                        self.pending_batch
                            .as_mut()
                            .expect("checkpoint failure retains its pending batch")
                            .quarantine_write_admissions_until_restart();
                    }
                    self.pending_batch = None;
                    let _ = reply.send(Err(status));
                    return false;
                }
            }
        }
        let result = close_write_actor(CloseWriteActorInput {
            index: self.index,
            cfg,
            drive: self.drive,
            drive_uuid: &drive_uuid,
            drive_serial: &drive_serial,
            snapshot_misses: self.snapshot_misses,
            session_id,
            tape_uuid,
            target_kind,
            library_serial: library_serial.as_str(),
            bay,
            objects_committed: self.objects_committed,
            bytes_committed: self.bytes_committed,
            opened_at_utc: opened_at_utc.as_str(),
            last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
            state: pb::write_session::State::WriteSessionStateClosed,
            append_commit_diagnostics: self.append_commit_diagnostics,
            checkpointed_objects: &self.committed_receipts,
            abort_reason: None,
        });
        match result {
            Ok(result) => {
                self.deferred_close_reply = Some((reply, result));
                true
            }
            Err(err) => {
                let _ = reply.send(Err(err));
                false
            }
        }
    }

    /// Close the actor in the aborted state without committing new work.
    fn handle_abort(&mut self, command: DriveCommand) -> bool {
        let DriveCommand::Abort {
            session_id: requested,
            reason,
            reply,
        } = command
        else {
            unreachable!("abort handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let target_kind = self.target_kind;
        let library_serial = self.library_serial.clone();
        let drive_uuid = self.drive_uuid.clone();
        let drive_serial = self.drive_serial.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            let _ = reply.send(Err(Status::not_found("write session not found")));
            return false;
        }
        let result = close_write_actor(CloseWriteActorInput {
            index: self.index,
            cfg,
            drive: self.drive,
            drive_uuid: &drive_uuid,
            drive_serial: &drive_serial,
            snapshot_misses: self.snapshot_misses,
            session_id,
            tape_uuid,
            target_kind,
            library_serial: library_serial.as_str(),
            bay,
            objects_committed: self.objects_committed,
            bytes_committed: self.bytes_committed,
            opened_at_utc: opened_at_utc.as_str(),
            last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
            state: pb::write_session::State::WriteSessionStateAborted,
            append_commit_diagnostics: self.append_commit_diagnostics,
            checkpointed_objects: &self.committed_receipts,
            abort_reason: reason,
        });
        match result {
            Ok(result) => {
                self.deferred_close_reply = Some((reply, result));
                true
            }
            Err(err) => {
                let _ = reply.send(Err(err));
                false
            }
        }
    }

    /// Report the current write-session projection without mutating it.
    fn handle_get(&mut self, command: DriveCommand) {
        let DriveCommand::Get {
            session_id: requested,
            reply,
        } = command
        else {
            unreachable!("get handler received another drive command");
        };
        let bay = self.bay;
        let target_kind = self.target_kind;
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        let status = if requested == session_id {
            Ok(session_proto(WriteSessionProtoInput {
                session_id,
                tape_uuid: &tape_uuid,
                target_kind,
                state: if self.pending_batch.is_none() && self.last_checkpoint_at_utc.is_some() {
                    pb::write_session::State::WriteSessionStateCheckpointed
                } else {
                    pb::write_session::State::WriteSessionStateOpen
                },
                objects_committed: self.objects_committed,
                bytes_committed: self.bytes_committed,
                opened_at_utc: opened_at_utc.as_str(),
                last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
                drive_element_address: bay,
                pending_batch: self.pending_batch.as_ref(),
            }))
        } else {
            Err(Status::not_found("write session not found"))
        };
        let _ = reply.send(status);
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn handle_drive_open_write(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    request: OpenWriteActorRequest,
) {
    let OpenWriteActorRequest {
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
    } = request;
    let actor_open_started = Instant::now();
    let session_id = Uuid::new_v4();
    let tape_uuid = selected.tape_uuid;
    if let Err(status) = session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode.as_deref(),
        "open write session",
    ) {
        let _ = reply.send(Err(status));
        return;
    }

    // Recovery authority must be complete and mutually consistent before any
    // load, rewind, locate, drive-mode change, or write preparation. Keep both
    // journal leases across the later media work so another writer cannot
    // change the validated prefix in between.
    let checkpoint_journal = match remanence_state::FileCheckpointJournal::open(
        cfg.checkpoint_journal_dir.as_path(),
        tape_uuid,
    ) {
        Ok(journal) => journal,
        Err(err) => {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    };
    let pending_terminal_intent = match checkpoint_journal.terminal_finalization_intent() {
        Ok(intent) => intent,
        Err(err) => {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    };
    let (mut checkpoint_lease, pending_terminal_intent) = if pending_terminal_intent.is_some() {
        let lease = match checkpoint_journal.acquire_exclusive_for_terminal_recovery() {
            Ok(lease) => lease,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
        let intent = match lease.terminal_finalization_intent() {
            Ok(intent) => intent,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
        (lease, intent)
    } else {
        let lease = match checkpoint_journal.acquire_exclusive() {
            Ok(lease) => lease,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
        (lease, None)
    };
    let durable_checkpoint_records =
        match project_checkpoint_authority_bounded(index, &checkpoint_lease) {
            Ok(records) => records,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
    if durable_checkpoint_records
        .last()
        .is_some_and(|record| record.sealed_after_write)
    {
        let _ = reply.send(Err(Status::failed_precondition(format!(
            "checkpoint authority records tape {} as terminal and sealed",
            Uuid::from_bytes(tape_uuid)
        ))));
        return;
    }
    if pending_terminal_intent.is_none() {
        if let Err(err) = crate::pool_write::ensure_empty_checkpoint_matches_catalog_freshness(
            index,
            &selected,
            &durable_checkpoint_records,
        ) {
            let _ = reply.send(Err(status_from_pool_write_error(err)));
            return;
        }
        if let Err(err) = crate::pool_write::ensure_selected_tape_accepts_session_write(
            index, &pool_cfg, &selected,
        ) {
            let _ = reply.send(Err(status_from_pool_write_error(err)));
            return;
        }
    }
    let parity_authority = if pending_terminal_intent.is_none()
        && matches!(
            selected.parity_config,
            remanence_parity::ParityConfig::Scheme(_)
        ) {
        match validate_parity_actor_authority(
            cfg,
            &selected,
            &checkpoint_lease,
            &durable_checkpoint_records,
        ) {
            Ok(authority) => Some(authority),
            Err(status) => {
                let _ = reply.send(Err(status));
                return;
            }
        }
    } else {
        None
    };

    if let Err(status) = session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "open write session",
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

    let media_policy = if pending_terminal_intent.is_some() {
        WriteMediaPolicy::TerminalAppend
    } else {
        WriteMediaPolicy::RewritableObject
    };
    if let Err(status) = prepare_drive_for_write(
        drive,
        &tape_uuid,
        selected.block_size,
        session_id,
        media_policy,
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    // Load-time wrap-map harvest + fence install, one act (design
    // §6.5): only when this open freshly mounted the cartridge — a
    // session on an already-seated tape is the same load, and
    // re-issuing REOWP mid-load would return a snapshot a write may
    // already have invalidated. Runs after identity is proven (the
    // fence binds this tape_uuid) and before any media-modifying CDB
    // of the load can dispatch.
    if needs_drive_load {
        run_load_calibration_harvest(index, drive, cfg, &tape_uuid, barcode.as_deref());
    }
    if let Some(intent) = pending_terminal_intent {
        if intent.manual.is_some() {
            let _ = reply.send(Err(Status::failed_precondition(format!(
                "tape {} has an operator-owned terminal finalization in progress; retry FinalizeTape with its original idempotency key",
                Uuid::from_bytes(tape_uuid)
            ))));
            return;
        }
        let drive_config = match drive.read_config() {
            Ok(config) => config,
            Err(error) => {
                let _ = reply.send(Err(Status::unavailable(format!(
                    "read automatic terminal-recovery drive config: {error}"
                ))));
                return;
            }
        };
        if drive_config.write_protected {
            let _ = reply.send(Err(Status::failed_precondition(
                "tape is write-protected and automatic terminal recovery cannot continue",
            )));
            return;
        }
        let rewritable = matches!(
            drive_config.worm,
            remanence_library::WormMediaState::NotWorm
        );
        let spec = TerminalFinalizeSpec::resume(&intent, selected.block_size, &pool_cfg);
        if let Err(error) =
            index.project_terminal_finalization(TerminalFinalizationProjectionInput {
                tape_uuid,
                trigger: intent.trigger,
                operation_id: None,
                progress: intent.progress,
                edition_digest: intent.edition_digest,
                layout_digest: intent.layout.layout_digest,
                outcome: if intent.recovery_required {
                    TerminalFinalizationOutcome::RecoveryRequired
                } else {
                    TerminalFinalizationOutcome::InProgress
                },
                updated_at_utc: None,
            })
        {
            let _ = reply.send(Err(status_from_state_error(error)));
            return;
        }
        let result = match &selected.parity_config {
            remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
                index,
                cfg,
                drive,
                &mut checkpoint_lease,
                durable_checkpoint_records
                    .last()
                    .expect("pending terminal intent requires checkpoint authority"),
                Some(intent),
                &spec,
                &selected,
                None,
                rewritable,
            ),
            remanence_parity::ParityConfig::Scheme(_) => finalize_terminal_with_parity(
                index,
                cfg,
                drive,
                &mut checkpoint_lease,
                durable_checkpoint_records
                    .last()
                    .expect("pending terminal intent requires checkpoint authority"),
                Some(intent),
                &spec,
                &selected,
                None,
                rewritable,
            ),
        };
        match result {
            Ok(result) if result.final_record.is_some() => {
                if let Err(error) = append_tape_sealed_evidence(index, cfg, tape_uuid) {
                    tracing::warn!(error = %error, "failed to append recovered tape sealing evidence");
                }
                let _ = reply.send(Err(Status::failed_precondition(format!(
                    "tape {} completed terminal finalization during open recovery and cannot accept Objects",
                    Uuid::from_bytes(tape_uuid)
                ))));
            }
            Ok(_) => {
                let _ = reply.send(Err(Status::unavailable(format!(
                    "tape {} still requires terminal recovery and cannot accept Objects",
                    Uuid::from_bytes(tape_uuid)
                ))));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
        return;
    }
    tracing::info!(
        target: "remanence_write_diag",
        phase = "drive_open_actor",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(tape_uuid),
        needs_drive_load,
        block_size_bytes = selected.block_size,
        elapsed_ms = crate::diagnostics::duration_ms(actor_open_started.elapsed()),
        "remanence_write_diag",
    );

    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let objects_committed = 0u64;
    let bytes_committed = 0u64;
    let last_checkpoint_at_utc = None;
    let last_durable_checkpoint = durable_checkpoint_records.last().cloned();
    let parity_session = if let Some(authority) = parity_authority {
        match open_parity_actor_session(
            index,
            drive,
            cfg,
            &selected,
            &durable_checkpoint_records,
            authority,
        ) {
            Ok(session) => Some(session),
            Err(status) => {
                let _ = reply.send(Err(status));
                return;
            }
        }
    } else {
        None
    };
    let next_batched_append = if parity_session.is_none() {
        match crate::pool_write::first_batched_append_context(
            index,
            &selected,
            &durable_checkpoint_records,
        ) {
            Ok(context) => Some(context),
            Err(err) => {
                let _ = reply.send(Err(status_from_pool_write_error(err)));
                return;
            }
        }
    } else {
        None
    };
    let checkpoint_ordinal = last_durable_checkpoint
        .as_ref()
        .map_or(0, |record| record.ordinal);
    let tape_committed_object_count = last_durable_checkpoint
        .as_ref()
        .map_or(0, |record| record.committed_object_count);
    let pending_batch: Option<PendingCheckpointBatch> = None;
    let committed_receipts = Vec::<pb::ObjectRecord>::new();
    let timer_checkpoint_waiting: Option<Uuid> = None;
    if let Err(status) = record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id,
            session_kind: "write",
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
    let open_reply = session_proto(WriteSessionProtoInput {
        session_id,
        tape_uuid: &tape_uuid,
        target_kind,
        state: pb::write_session::State::WriteSessionStateOpen,
        objects_committed,
        bytes_committed,
        opened_at_utc: opened_at_utc.as_str(),
        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
        drive_element_address: bay,
        pending_batch: None,
    });
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    let mut session_state = WriteSessionState {
        bay,
        index,
        cfg,
        actor_tx,
        drive,
        snapshot_misses,
        pool_cfg,
        selected,
        target_kind,
        library_serial,
        drive_uuid,
        drive_serial,
        session_id,
        tape_uuid,
        opened_at_utc,
        objects_committed,
        bytes_committed,
        last_checkpoint_at_utc,
        checkpoint_lease,
        durable_checkpoint_records,
        parity_session,
        next_batched_append,
        checkpoint_ordinal,
        tape_committed_object_count,
        pending_batch,
        committed_receipts,
        timer_checkpoint_waiting,
        append_gate: SessionAppendGate::default(),
        append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics::default(),
        deferred_close_reply: None,
    };
    let deferred_close_reply = session_state.run(rx);
    drop(session_state);
    if let Some((reply, result)) = deferred_close_reply {
        let _ = reply.send(Ok(result));
    }
}

pub(super) fn append_latest_tape_io_fence_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    let fence = index
        .list_active_tape_io_fences()
        .map_err(crate::status_from_state_error)?
        .into_iter()
        .find(|fence| fence.tape_uuid.as_slice() == tape_uuid);
    let Some(fence) = fence else {
        return Ok(());
    };
    append_tape_io_fence_evidence(index, cfg, &fence, AuditEvent::TapeIoFenceRaised)
}

pub(super) fn append_tape_sealed_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    ensure_tape_sealed_audit(
        index,
        cfg.audit_dir.as_path(),
        &cfg.audit_append_lock,
        tape_uuid,
    )
}

pub(super) fn append_tape_io_fence_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    fence: &TapeIoFenceRecord,
    event: AuditEvent,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(fence.tape_uuid.clone()),
        ),
        (
            "quarantine_id".to_string(),
            CborValue::Text(fence.quarantine_id.clone()),
        ),
        ("reason".to_string(), CborValue::Text(fence.reason.clone())),
    ]);
    insert_optional_audit_text(&mut detail, "barcode", fence.barcode.as_ref());
    insert_optional_audit_text(&mut detail, "evidence_json", fence.evidence_json.as_ref());
    insert_optional_audit_text(&mut detail, "release_ack", fence.release_ack.as_ref());
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
            subject_kind: "tape_io_fence",
            subject_id: Some(fence.quarantine_id.clone()),
            detail,
        },
    )?;
    Ok(())
}

/// Run the load-time wrap-map harvest for a freshly mounted cartridge
/// (design §6.5): install the volume's real media-write fence and
/// harvest REOWP in one act, via
/// `calibration::harvest_and_install_calibration`. Ordering is an
/// optimisation, so no harvest outcome fails the session open; the
/// result is durably recorded in the calibration store and logged
/// here. The one exception is enforced elsewhere: once the fence is
/// installed, a write whose durable epoch advance cannot be recorded
/// is refused by the media gate itself.
pub(super) fn run_load_calibration_harvest(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    tape_uuid: &TapeUuid,
    barcode: Option<&str>,
) {
    use crate::calibration::HarvestOutcome;
    let outcome = crate::calibration::harvest_and_install_calibration(
        drive,
        index,
        &cfg.calibration_store,
        *tape_uuid,
        barcode,
    );
    let tape = Uuid::from_bytes(*tape_uuid);
    match outcome {
        HarvestOutcome::Calibrated {
            write_epoch,
            calibration_generation,
            wrap_count,
        } => tracing::info!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            write_epoch,
            calibration_generation,
            wrap_count,
            "wrap-map harvest calibrated the volume"
        ),
        HarvestOutcome::UnsupportedFormat {
            calibration_generation,
            detail,
        } => tracing::info!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            calibration_generation,
            detail = detail.as_str(),
            "wrap-map harvest: unsupported format for this load"
        ),
        HarvestOutcome::Uncalibrated {
            calibration_generation,
            detail,
        } => tracing::warn!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            calibration_generation,
            detail = detail.as_str(),
            "wrap-map harvest left the volume uncalibrated"
        ),
        HarvestOutcome::StoreUnavailable { detail } => tracing::warn!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            detail = detail.as_str(),
            "calibration store unavailable during load harvest; volume state unchanged"
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteMediaPolicy {
    RewritableObject,
    TerminalAppend,
}

pub(super) fn validate_write_media_policy(
    current_cfg: TapeConfig,
    media_policy: WriteMediaPolicy,
) -> Result<(), Status> {
    if media_policy == WriteMediaPolicy::TerminalAppend {
        return if current_cfg.write_protected {
            Err(Status::failed_precondition("tape is write-protected"))
        } else {
            Ok(())
        };
    }
    crate::pool_write::require_rewritable_object_media(current_cfg)
        .map_err(|error| Status::failed_precondition(error.to_string()))
}

pub(super) fn prepare_drive_for_write(
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    block_size: u32,
    session_id: Uuid,
    media_policy: WriteMediaPolicy,
) -> Result<(), Status> {
    let prepare_started = Instant::now();
    drive
        .reverify_invalidated_state()
        .map_err(|err| Status::failed_precondition(format!("reverify drive state: {err}")))?;
    let rewind_verify_started = Instant::now();
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before write verify: {err}")))?;
    let rewind_verify_elapsed = rewind_verify_started.elapsed();
    let verify_started = Instant::now();
    {
        let mut source = DriveHandleSource(drive);
        verify_tape_identity(&mut source, tape_uuid)
            .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    }
    let verify_elapsed = verify_started.elapsed();
    let rewind_write_started = Instant::now();
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before write: {err}")))?;
    let rewind_write_elapsed = rewind_write_started.elapsed();
    let read_config_started = Instant::now();
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config before write: {err}")))?;
    let read_config_elapsed = read_config_started.elapsed();
    validate_write_media_policy(current_cfg, media_policy)?;
    let write_config_started = Instant::now();
    let (target_cfg, verified_cfg) =
        crate::drive_mode::configure_fixed_uncompressed_write(drive, current_cfg, block_size)
            .map_err(|err| {
                Status::failed_precondition(format!("verify fixed-block write config: {err}"))
            })?;
    let staging_ring_buffers = drive.staging_ring_buffers();
    let effective_batch_blocks = drive.requested_write_batch_blocks().min(
        drive
            .sg_reserved_size_bytes()
            .checked_div(block_size.max(1))
            .unwrap_or(1)
            .max(1),
    );
    let effective_ring_bytes = u64::from(staging_ring_buffers)
        .saturating_mul(u64::from(effective_batch_blocks))
        .saturating_mul(u64::from(block_size));
    tracing::info!(
        target: "remanence_write_diag",
        phase = "drive_prepare",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(*tape_uuid),
        selected_block_size_bytes = block_size,
        prior_block_size = ?current_cfg.block_size,
        prior_compression = current_cfg.compression,
        target_block_size = ?target_cfg.block_size,
        target_compression = target_cfg.compression,
        verified_block_size = ?verified_cfg.block_size,
        verified_compression = verified_cfg.compression,
        staging_ring_buffers,
        effective_batch_blocks,
        effective_ring_bytes,
        rewind_verify_ms = crate::diagnostics::duration_ms(rewind_verify_elapsed),
        verify_bootstrap_ms = crate::diagnostics::duration_ms(verify_elapsed),
        rewind_write_ms = crate::diagnostics::duration_ms(rewind_write_elapsed),
        read_config_ms = crate::diagnostics::duration_ms(read_config_elapsed),
        write_config_ms = crate::diagnostics::duration_ms(write_config_started.elapsed()),
        elapsed_ms = crate::diagnostics::duration_ms(prepare_started.elapsed()),
        "remanence_write_diag",
    );
    Ok(())
}
