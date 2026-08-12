//! Terminal-index inventory, full verification, BOT recovery, and protobuf projection.

use std::time::Instant;

use remanence_library::DriveHandle;
use remanence_parity::{
    read_terminal_index_inventory_streamed, BotStructuralRecoveryEvent,
    BotStructuralRecoveryReason, DriveHandleRawSource, ScanWalkControl, TerminalInventoryOutcome,
    TerminalInventoryStreamEvent, TerminalReplicaEvidence, TerminalReplicaFailureKind,
};
use remanence_state::CatalogIndex;
use tokio::sync::mpsc;
use tonic::Status;
use uuid::Uuid;

use super::actor_runtime::WriteOwnerConfig;
use super::bot_recovery;
use super::read_session::session_open_reject_tape_io_fences;
use super::readiness::session_open_short_probe_or_load;
use super::restore::verify_loaded_tape_identity;
use super::SessionOpenReadinessContext;
use crate::{pb, status_from_state_error, TapeUuid};

pub(crate) fn prepare_drive_for_read(
    index: &CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    session_id: Uuid,
) -> Result<(), Status> {
    let block_size = catalog_tape_block_size(index, tape_uuid)?;
    prepare_drive_for_fixed_read(drive, tape_uuid, block_size, session_id)
}

pub(crate) fn catalog_tape_block_size(
    index: &CatalogIndex,
    tape_uuid: &TapeUuid,
) -> Result<u32, Status> {
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::failed_precondition("tape catalog row is missing"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::failed_precondition("tape block_size is missing"))?;
    u32::try_from(block_size).map_err(|_| Status::internal("tape block size does not fit u32"))
}

pub(crate) fn prepare_drive_for_fixed_read(
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    block_size: u32,
    session_id: Uuid,
) -> Result<(), Status> {
    let started = Instant::now();
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config: {err}")))?;
    let target_cfg = crate::drive_mode::fixed_uncompressed_target(current_cfg, block_size);
    drive
        .write_config(target_cfg)
        .map_err(|err| Status::internal(format!("set fixed read config: {err}")))?;
    let verified = drive
        .read_config()
        .map_err(|err| Status::internal(format!("verify fixed read config: {err}")))?;
    if verified.block_size != target_cfg.block_size {
        return Err(Status::failed_precondition(format!(
            "fixed read mode verification mismatch: expected {:?}, got {:?}",
            target_cfg.block_size, verified.block_size
        )));
    }
    tracing::info!(
        target: "remanence_read_diag",
        phase = "drive_prepare_read",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(*tape_uuid),
        status = "ok",
        selected_block_size_bytes = block_size,
        prior_block_size = ?current_cfg.block_size,
        target_block_size = ?target_cfg.block_size,
        elapsed_ms = crate::diagnostics::duration_ms(started.elapsed()),
        "remanence_read_diag",
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_drive_tape_inventory(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    tape_uuid: TapeUuid,
    needs_drive_load: bool,
    library_serial: &str,
    barcode: Option<&str>,
    source_slot: Option<u16>,
    drive_serial: Option<&str>,
    stream_tx: &mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
) -> Result<(), Status> {
    session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "read terminal tape inventory",
            bay,
            library_serial,
            barcode,
            source_slot,
            drive_serial,
            needs_drive_load,
        },
    )?;
    session_open_reject_tape_io_fences(index, &tape_uuid, barcode, "read terminal tape inventory")?;
    verify_loaded_tape_identity(drive, &tape_uuid)?;
    let block_size = catalog_tape_block_size(index, &tape_uuid)?;
    prepare_drive_for_fixed_read(drive, &tape_uuid, block_size, Uuid::new_v4())?;

    let outcome = {
        let mut source = DriveHandleRawSource::new(drive);
        read_terminal_index_inventory_streamed(&mut source, &tape_uuid, block_size, |event| {
            send_inventory_stream_item(stream_tx, terminal_inventory_event_to_proto(event))
                .map_err(|error| error.message().to_string())
        })
        .map_err(status_from_terminal_inventory_read_error)?
    };
    if let TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) = &outcome {
        let summary = {
            let mut source = DriveHandleRawSource::new(drive);
            bot_recovery::recover_terminal_inventory_with_checkpoint_authority_controlled(
                &mut source,
                &cfg.checkpoint_journal_dir,
                &tape_uuid,
                block_size,
                |event| {
                    let item = bot_recovery_control_event_to_proto(
                        event,
                        tape_uuid,
                        block_size,
                        recovery.reason,
                    );
                    if send_inventory_stream_item(stream_tx, item).is_ok() {
                        ScanWalkControl::Continue
                    } else {
                        ScanWalkControl::Abort
                    }
                },
                |object| {
                    send_inventory_stream_item(stream_tx, bot_recovered_object_to_proto(object))
                        .map_err(|error| error.message().to_string())
                },
            )
            .map_err(status_from_bot_structural_recovery_error)?
        };
        send_inventory_stream_item(
            stream_tx,
            pb::TapeInventoryStreamItem {
                item: Some(pb::tape_inventory_stream_item::Item::Summary(
                    bot_structural_recovery_to_proto(tape_uuid, summary),
                )),
            },
        )?;
        return Ok(());
    }
    send_inventory_stream_item(
        stream_tx,
        pb::TapeInventoryStreamItem {
            item: Some(pb::tape_inventory_stream_item::Item::Summary(
                terminal_inventory_to_proto(tape_uuid, outcome),
            )),
        },
    )
}

pub(crate) fn status_from_terminal_inventory_read_error(
    error: remanence_parity::TerminalInventoryReadError,
) -> Status {
    match error {
        remanence_parity::TerminalInventoryReadError::BlockSize(_) => {
            Status::failed_precondition(format!("read terminal tape inventory: {error}"))
        }
        remanence_parity::TerminalInventoryReadError::Source { .. } => {
            Status::unavailable(format!("read terminal tape inventory: {error}"))
        }
        remanence_parity::TerminalInventoryReadError::SelectedReplica { .. }
        | remanence_parity::TerminalInventoryReadError::TerminalIndexReplicaConflict { .. } => {
            Status::data_loss(format!("read terminal tape inventory: {error}"))
        }
        remanence_parity::TerminalInventoryReadError::StreamVisitor { .. } => {
            Status::cancelled("terminal inventory receiver closed")
        }
    }
}

pub(crate) fn send_inventory_stream_item(
    stream_tx: &mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
    item: pb::TapeInventoryStreamItem,
) -> Result<(), Status> {
    stream_tx
        .blocking_send(Ok(item))
        .map_err(|_| Status::cancelled("terminal inventory receiver closed"))
}

pub(crate) fn terminal_inventory_event_to_proto(
    event: TerminalInventoryStreamEvent,
) -> pb::TapeInventoryStreamItem {
    use pb::tape_inventory_stream_item::Item;
    let item = match event {
        TerminalInventoryStreamEvent::ReplicaAttemptStarted {
            attempt_id,
            replica_ordinal,
        } => Item::ReplicaAttemptStarted(pb::TapeInventoryReplicaAttemptStarted {
            attempt_id,
            replica_ordinal: u32::from(replica_ordinal),
        }),
        TerminalInventoryStreamEvent::StructuralEntry {
            attempt_id,
            replica_ordinal,
            entry,
        } => Item::StructuralEntry(terminal_structural_entry_to_proto(
            attempt_id,
            replica_ordinal,
            entry,
        )),
        TerminalInventoryStreamEvent::ObjectRow {
            attempt_id,
            replica_ordinal,
            row,
        } => Item::ObjectRow(terminal_object_row_to_proto(
            attempt_id,
            replica_ordinal,
            row,
        )),
        TerminalInventoryStreamEvent::ReplicaAttemptRejected {
            attempt_id,
            replica_ordinal,
            failure,
        } => Item::ReplicaAttemptRejected(pb::TapeInventoryReplicaAttemptRejected {
            attempt_id,
            replica_ordinal: u32::from(replica_ordinal),
            failure_kind: terminal_replica_failure_kind_name(failure.kind).to_string(),
            detail: failure.detail,
        }),
    };
    pb::TapeInventoryStreamItem { item: Some(item) }
}

pub(crate) fn terminal_structural_entry_to_proto(
    attempt_id: u64,
    replica_ordinal: u16,
    entry: remanence_parity::TapeIndexReplicaMapEntry,
) -> pb::TapeInventoryStructuralEntry {
    let kind = match entry.kind {
        remanence_parity::TapeIndexReplicaFileKind::Object => {
            pb::TapeInventoryStructuralKind::Object
        }
        remanence_parity::TapeIndexReplicaFileKind::ParitySidecar => {
            pb::TapeInventoryStructuralKind::ParitySidecar
        }
        remanence_parity::TapeIndexReplicaFileKind::Bootstrap => {
            pb::TapeInventoryStructuralKind::Bootstrap
        }
        remanence_parity::TapeIndexReplicaFileKind::ParityMap => {
            pb::TapeInventoryStructuralKind::ParityMap
        }
        remanence_parity::TapeIndexReplicaFileKind::TapeIndexReplica => {
            pb::TapeInventoryStructuralKind::TapeIndexReplica
        }
        remanence_parity::TapeIndexReplicaFileKind::IndexSeparationExtent => {
            pb::TapeInventoryStructuralKind::IndexSeparationExtent
        }
    };
    pb::TapeInventoryStructuralEntry {
        attempt_id,
        replica_ordinal: u32::from(replica_ordinal),
        tape_file_number: entry.tape_file_number,
        kind: kind as i32,
        block_count: entry.block_count,
        first_parity_data_ordinal: entry.first_parity_data_ordinal,
        protected_ordinal_start: entry.protected_ordinal_start,
        protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
        epoch_id: entry.epoch_id,
    }
}

pub(crate) fn terminal_object_row_to_proto(
    attempt_id: u64,
    replica_ordinal: u16,
    row: remanence_parity::TapeIndexReplicaObjectRow,
) -> pb::TapeInventoryObjectRow {
    use pb::tape_inventory_object_row::Representation;
    let representation = match row.representation {
        remanence_parity::ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => Representation::Plaintext(pb::TapeInventoryPlaintextRecovery {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256: manifest_sha256.to_vec(),
        }),
        remanence_parity::ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => Representation::Encrypted(pb::TapeInventoryEncryptedRecovery {
            recipient_epoch_ids: recipient_epoch_ids
                .into_iter()
                .map(|epoch_id| epoch_id.to_vec())
                .collect(),
            metadata_frame_len,
            key_frame_len,
        }),
    };
    pb::TapeInventoryObjectRow {
        attempt_id,
        replica_ordinal: u32::from(replica_ordinal),
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id: row.object_id,
        representation: Some(representation),
    }
}

pub(crate) fn bot_recovered_object_to_proto(
    object: &remanence_parity::BotRecoveredObject,
) -> pb::TapeInventoryStreamItem {
    let state = match object.state {
        remanence_parity::BotRecoveredObjectState::Recovered => {
            pb::TapeInventoryBotObjectState::Recovered
        }
        remanence_parity::BotRecoveredObjectState::Unknown => {
            pb::TapeInventoryBotObjectState::Unknown
        }
        remanence_parity::BotRecoveredObjectState::Incomplete => {
            pb::TapeInventoryBotObjectState::Incomplete
        }
    };
    pb::TapeInventoryStreamItem {
        item: Some(pb::tape_inventory_stream_item::Item::BotObject(
            pb::TapeInventoryBotObject {
                tape_file_number: object.tape_file_number,
                stored_block_count: object.stored_block_count,
                object_id: object.object_id.clone(),
                state: state as i32,
            },
        )),
    }
}

pub(crate) fn bot_recovery_control_event_to_proto(
    event: &BotStructuralRecoveryEvent,
    tape_uuid: [u8; 16],
    block_size: u32,
    reason: BotStructuralRecoveryReason,
) -> pb::TapeInventoryStreamItem {
    use pb::tape_inventory_stream_item::Item;
    let item = match event {
        BotStructuralRecoveryEvent::Started => {
            Item::BotRecoveryStarted(pb::TapeInventoryBotRecoveryStarted {
                tape_uuid: tape_uuid.to_vec(),
                block_size,
                reason: match reason {
                    BotStructuralRecoveryReason::NoUsableTerminalLayout => {
                        pb::TapeInventoryBotRecoveryReason::NoUsableTerminalLayout as i32
                    }
                    BotStructuralRecoveryReason::AllMembersInvalid => {
                        pb::TapeInventoryBotRecoveryReason::AllMembersInvalid as i32
                    }
                },
            })
        }
        BotStructuralRecoveryEvent::Progress(progress) => {
            Item::BotRecoveryProgress(pb::TapeInventoryBotRecoveryProgress {
                tape_file_number: progress.tape_file_number,
                partition: progress.position.partition,
                position_lba: progress.position.lba,
                structural_candidate_count: progress.structural_candidate_count,
                elapsed_millis: u64::try_from(progress.elapsed.as_millis()).unwrap_or(u64::MAX),
            })
        }
    };
    pb::TapeInventoryStreamItem { item: Some(item) }
}

pub(crate) fn terminal_replica_failure_kind_name(kind: TerminalReplicaFailureKind) -> &'static str {
    match kind {
        TerminalReplicaFailureKind::Missing => "missing",
        TerminalReplicaFailureKind::HeaderRead => "header_read",
        TerminalReplicaFailureKind::HeaderInvalid => "header_invalid",
        TerminalReplicaFailureKind::FooterRead => "footer_read",
        TerminalReplicaFailureKind::FooterInvalid => "footer_invalid",
        TerminalReplicaFailureKind::LocalBinding => "local_binding",
        TerminalReplicaFailureKind::TrailingFilemark => "trailing_filemark",
        TerminalReplicaFailureKind::PayloadInvalid => "payload_invalid",
        TerminalReplicaFailureKind::CrossSurvivorConflict => "cross_survivor_conflict",
    }
}

pub(crate) fn status_from_bot_structural_recovery_error(
    error: remanence_parity::BotStructuralRecoveryError,
) -> Status {
    match &error {
        remanence_parity::BotStructuralRecoveryError::Scan { .. } => {
            Status::unavailable(format!("BOT structural tape recovery failed: {error}"))
        }
        remanence_parity::BotStructuralRecoveryError::Visitor { .. } => {
            Status::cancelled("terminal inventory receiver closed")
        }
        remanence_parity::BotStructuralRecoveryError::Aborted { .. } => {
            Status::cancelled(format!("BOT structural recovery was cancelled: {error}"))
        }
        remanence_parity::BotStructuralRecoveryError::TapeIdentityMismatch => {
            Status::failed_precondition(format!(
                "BOT structural tape recovery refused the physical identity: {error}"
            ))
        }
        remanence_parity::BotStructuralRecoveryError::ObjectAuthority { .. }
        | remanence_parity::BotStructuralRecoveryError::ConflictingObjectAuthority { .. }
        | remanence_parity::BotStructuralRecoveryError::ArithmeticOverflow { .. } => {
            Status::data_loss(format!("BOT structural tape recovery failed: {error}"))
        }
    }
}

pub(crate) fn bot_recovery_reason_detail(reason: BotStructuralRecoveryReason) -> &'static str {
    match reason {
        BotStructuralRecoveryReason::NoUsableTerminalLayout => {
            "no usable terminal layout; structural recovery from BOT is required"
        }
        BotStructuralRecoveryReason::AllMembersInvalid => {
            "terminal replicas A, B, and C are invalid; structural recovery from BOT is required"
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_drive_verify_tape_index(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    tape_uuid: TapeUuid,
    needs_drive_load: bool,
    library_serial: &str,
    barcode: Option<&str>,
    source_slot: Option<u16>,
    drive_serial: Option<&str>,
) -> Result<pb::TapeIndexVerification, Status> {
    session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "fully verify terminal tape index",
            bay,
            library_serial,
            barcode,
            source_slot,
            drive_serial,
            needs_drive_load,
        },
    )?;
    session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode,
        "fully verify terminal tape index",
    )?;
    verify_loaded_tape_identity(drive, &tape_uuid)?;
    let block_size = catalog_tape_block_size(index, &tape_uuid)?;
    prepare_drive_for_fixed_read(drive, &tape_uuid, block_size, Uuid::new_v4())?;

    let outcome = {
        let mut source = DriveHandleRawSource::new(drive);
        bot_recovery::verify_terminal_index_with_checkpoint_authority(
            &mut source,
            &cfg.checkpoint_journal_dir,
            &tape_uuid,
            block_size,
        )
        .map_err(status_from_terminal_index_verification_error)?
    };
    Ok(terminal_verification_to_proto(tape_uuid, outcome))
}

pub(crate) fn terminal_verification_to_proto(
    tape_uuid: TapeUuid,
    outcome: remanence_parity::TerminalIndexVerificationOutcome,
) -> pb::TapeIndexVerification {
    use remanence_parity::TerminalIndexVerificationOutcome as Outcome;
    match outcome {
        Outcome::VerifiedComplete(verified) => {
            terminal_verified_to_proto(tape_uuid, *verified, true)
        }
        Outcome::VerifiedDegraded(verified) => {
            terminal_verified_to_proto(tape_uuid, *verified, false)
        }
        Outcome::RecoveryRequired(recovery) => pb::TapeIndexVerification {
            tape_uuid: tape_uuid.to_vec(),
            state: pb::TapeIndexVerificationState::RecoveryRequired as i32,
            fast_inventory: None,
            detail: recovery.detail,
            replica_health: recovery
                .replicas
                .iter()
                .enumerate()
                .map(|(index, evidence)| terminal_replica_health(index, evidence))
                .collect(),
            separation_health: (1u32..=2)
                .map(|separation_ordinal| pb::TapeIndexSeparationHealth {
                    separation_ordinal,
                    state: pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown
                        as i32,
                    verified_interior_record_count: 0,
                    detail: "canonical prefix authority unavailable".to_string(),
                })
                .collect(),
            measured_eod_lba: recovery.measured_eod.lba,
            verified_prefix_tape_file_count: 0,
            verified_prefix_record_count: 0,
            measured_tape_file_count: recovery.bot_recovery.structural_entry_count,
            edition_digest: Vec::new(),
            layout_digest: Vec::new(),
            payload_digest: Vec::new(),
            canonical_map_digest: Vec::new(),
            verification_basis: "bot_structural_recovery".to_string(),
            recovery_inventory: Some(bot_structural_recovery_to_proto(
                tape_uuid,
                recovery.bot_recovery,
            )),
        },
    }
}

pub(crate) fn terminal_verified_to_proto(
    tape_uuid: TapeUuid,
    verified: remanence_parity::TerminalIndexVerification,
    complete: bool,
) -> pb::TapeIndexVerification {
    pb::TapeIndexVerification {
        tape_uuid: tape_uuid.to_vec(),
        state: if complete {
            pb::TapeIndexVerificationState::VerifiedComplete as i32
        } else {
            pb::TapeIndexVerificationState::VerifiedDegraded as i32
        },
        fast_inventory: None,
        detail: if complete {
            "physical prefix, A/B/C, AB/BC, and terminal EOD validated".to_string()
        } else {
            "canonical physical prefix verified from a surviving replica; degraded terminal component evidence is attached".to_string()
        },
        replica_health: verified
            .replicas
            .iter()
            .enumerate()
            .map(|(index, evidence)| terminal_replica_health(index, evidence))
            .collect(),
        separation_health: terminal_separation_health(&verified.separations),
        measured_eod_lba: verified.measured_eod.lba,
        verified_prefix_tape_file_count: verified.verified_prefix_tape_file_count,
        verified_prefix_record_count: verified.verified_prefix_record_count,
        measured_tape_file_count: verified.measured_tape_file_count,
        edition_digest: verified.edition.edition_digest.to_vec(),
        layout_digest: verified.edition.layout_digest.to_vec(),
        payload_digest: verified.selected_payload.payload_sha256.to_vec(),
        canonical_map_digest: verified.selected_payload.canonical_map_sha256.to_vec(),
        verification_basis: "measured_full_physical".to_string(),
        recovery_inventory: None,
    }
}

pub(crate) fn terminal_separation_health(
    evidence: &[remanence_parity::TerminalSeparationEvidence; 2],
) -> Vec<pb::TapeIndexSeparationHealth> {
    evidence
        .iter()
        .enumerate()
        .map(|(index, evidence)| {
            let (state, verified_interior_record_count, detail) = match evidence {
                remanence_parity::TerminalSeparationEvidence::Valid {
                    interior_record_count,
                } => (
                    pb::tape_index_separation_health::State::TapeIndexSeparationStateValid,
                    *interior_record_count,
                    "header_footer_zero_fill_and_filemark_valid".to_string(),
                ),
                remanence_parity::TerminalSeparationEvidence::Invalid { detail } => (
                    pb::tape_index_separation_health::State::TapeIndexSeparationStateInvalid,
                    0,
                    detail.clone(),
                ),
            };
            pb::TapeIndexSeparationHealth {
                separation_ordinal: u32::try_from(index + 1)
                    .expect("two separation ordinals fit u32"),
                state: state as i32,
                verified_interior_record_count,
                detail,
            }
        })
        .collect()
}

pub(crate) fn status_from_terminal_index_verification_error(
    error: remanence_parity::TerminalIndexVerificationError,
) -> Status {
    match error {
        remanence_parity::TerminalIndexVerificationError::Source { .. }
        | remanence_parity::TerminalIndexVerificationError::PrefixWalk { .. } => {
            Status::unavailable(format!("full terminal index verification failed: {error}"))
        }
        _ => Status::data_loss(format!("full terminal index verification failed: {error}")),
    }
}

pub(crate) fn bot_structural_recovery_to_proto(
    tape_uuid: TapeUuid,
    summary: remanence_parity::BotStructuralRecoverySummary,
) -> pb::TapeInventory {
    pb::TapeInventory {
        tape_uuid: tape_uuid.to_vec(),
        outcome: pb::TapeInventoryOutcome::BotStructuralRecovered as i32,
        selected_replica_ordinal: 0,
        replica_health: (1u32..=3)
            .map(|replica_ordinal| pb::TapeIndexReplicaHealth {
                replica_ordinal,
                state: pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32,
                detail: "terminal replica unavailable; BOT structural recovery used".to_string(),
            })
            .collect(),
        structural_entry_count: summary.structural_entry_count,
        object_row_count: summary.complete_object_count,
        edition_digest: Vec::new(),
        layout_digest: Vec::new(),
        payload_digest: Vec::new(),
        canonical_map_digest: summary.canonical_map_digest.to_vec(),
        inventory_basis: "bot_structural_recovery".to_string(),
        detail: format!(
            "terminal index unavailable; BOT recovery classified {} recovered, {} unknown, and {} incomplete Object candidates",
            summary.recovered_object_count,
            summary.unknown_object_count,
            summary.incomplete_object_count
        ),
        recovered_object_count: summary.recovered_object_count,
        unknown_object_count: summary.unknown_object_count,
        incomplete_object_count: summary.incomplete_object_count,
        damaged_region_count: summary.damaged_region_count,
        selected_attempt_id: 0,
    }
}

pub(crate) fn terminal_inventory_to_proto(
    tape_uuid: TapeUuid,
    outcome: TerminalInventoryOutcome,
) -> pb::TapeInventory {
    match outcome {
        TerminalInventoryOutcome::Inventory(selection) => {
            let outcome = if selection.is_degraded() {
                pb::TapeInventoryOutcome::Degraded
            } else {
                pb::TapeInventoryOutcome::Complete
            };
            let replica_health = selection
                .replicas
                .iter()
                .enumerate()
                .map(|(index, evidence)| terminal_replica_health(index, evidence))
                .collect();
            pb::TapeInventory {
                tape_uuid: tape_uuid.to_vec(),
                outcome: outcome as i32,
                selected_replica_ordinal: u32::from(selection.selected_replica_ordinal),
                replica_health,
                structural_entry_count: selection.payload.structural_entry_count,
                object_row_count: selection.payload.object_row_count,
                edition_digest: selection.edition.edition_digest.to_vec(),
                layout_digest: selection.edition.layout_digest.to_vec(),
                payload_digest: selection.payload.payload_sha256.to_vec(),
                canonical_map_digest: selection.payload.canonical_map_sha256.to_vec(),
                inventory_basis: "terminal_index_fast".to_string(),
                detail: if selection.is_degraded() {
                    format!(
                        "terminal inventory selected replica {}; degraded replica evidence is present",
                        selection.selected_replica_ordinal
                    )
                } else {
                    "terminal inventory selected replica C; all replica envelopes agree".to_string()
                },
                recovered_object_count: 0,
                unknown_object_count: 0,
                incomplete_object_count: 0,
                damaged_region_count: 0,
                selected_attempt_id: selection.selected_attempt_id,
            }
        }
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) => {
            let detail = bot_recovery_reason_detail(recovery.reason);
            pb::TapeInventory {
                tape_uuid: tape_uuid.to_vec(),
                outcome: pb::TapeInventoryOutcome::BotStructuralRecoveryRequired as i32,
                selected_replica_ordinal: 0,
                replica_health: recovery
                    .replicas
                    .iter()
                    .enumerate()
                    .map(|(index, evidence)| terminal_replica_health(index, evidence))
                    .collect(),
                structural_entry_count: 0,
                object_row_count: 0,
                edition_digest: Vec::new(),
                layout_digest: Vec::new(),
                payload_digest: Vec::new(),
                canonical_map_digest: Vec::new(),
                inventory_basis: "terminal_index_fast".to_string(),
                detail: detail.to_string(),
                recovered_object_count: 0,
                unknown_object_count: 0,
                incomplete_object_count: 0,
                damaged_region_count: 0,
                selected_attempt_id: 0,
            }
        }
    }
}

pub(crate) fn terminal_replica_health(
    index: usize,
    evidence: &TerminalReplicaEvidence,
) -> pb::TapeIndexReplicaHealth {
    let replica_ordinal = u32::try_from(index + 1).expect("three replica indexes fit u32");
    let (state, detail) = match evidence {
        TerminalReplicaEvidence::Valid { .. } => (
            pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete,
            "payload_valid".to_string(),
        ),
        TerminalReplicaEvidence::ConsistentEnvelope => (
            pb::tape_index_replica_health::State::TapeIndexReplicaStateEnvelopeValid,
            "envelope_valid_payload_not_read".to_string(),
        ),
        TerminalReplicaEvidence::Invalid(failure) => (
            pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid,
            format!(
                "{}: {}",
                terminal_replica_failure_name(failure.kind),
                failure.detail
            ),
        ),
    };
    pb::TapeIndexReplicaHealth {
        replica_ordinal,
        state: state as i32,
        detail,
    }
}

pub(crate) const fn terminal_replica_failure_name(
    kind: TerminalReplicaFailureKind,
) -> &'static str {
    match kind {
        TerminalReplicaFailureKind::Missing => "missing",
        TerminalReplicaFailureKind::HeaderRead => "header_read",
        TerminalReplicaFailureKind::HeaderInvalid => "header_invalid",
        TerminalReplicaFailureKind::FooterRead => "footer_read",
        TerminalReplicaFailureKind::FooterInvalid => "footer_invalid",
        TerminalReplicaFailureKind::LocalBinding => "local_binding",
        TerminalReplicaFailureKind::TrailingFilemark => "trailing_filemark",
        TerminalReplicaFailureKind::PayloadInvalid => "payload_invalid",
        TerminalReplicaFailureKind::CrossSurvivorConflict => "cross_survivor_conflict",
    }
}
