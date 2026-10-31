//! Terminal-triple planning, writing, reconciliation, and audit projection.

use remanence_state::authority::TapeAuthorityView;
use remanence_state::authority::TapeAuthorityWriter as _;
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use ciborium::value::Value as CborValue;
use remanence_library::DriveHandle;
use remanence_parity::{
    reconcile_terminal_prefix, reconcile_terminal_tail_next, DriveHandleRawSink,
    DriveHandleRawSource, PhysicalPositionHint, TerminalPrefixPlan,
    TerminalPrefixReconcileEvidence, TerminalTailStepOutcome, TerminalTailWriteError,
    TerminalTripleWritePlan,
};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex, FileAuditLog,
    ManualTerminalFinalizationAcceptanceInput, SourceLayer, TapePoolConfig,
    TerminalFinalizationOutcome, TerminalFinalizationProjectionInput,
};
use tonic::Status;
use uuid::Uuid;

use super::actor_runtime::{record_session_close_snapshot, WriteOwnerConfig};
use super::checkpoint::{fence_failed_parity_raw_write, project_checkpoint_authority_bounded};
use super::read_session::session_open_reject_tape_io_fences;
use super::readiness::session_open_short_probe_or_load;
use super::restore::{status_from_parity_error, status_from_selected_write_error};
use super::terminal_types::TerminalConfig;
use super::terminal_types::{
    parity_progress_from_state, persist_terminal_recovery_required,
    reconcile_and_authorize_parity_resume, reconcile_terminal_component_host_authority,
    state_progress_from_parity, terminal_reconciliation_outcome, ManualFinalizePreflightConfig,
    ManualFinalizeTapeActorReply, ManualFinalizeTapeActorRequest, ManualFinalizeTapeMountRequest,
    TerminalFinalizeAuditConfig, TerminalFinalizeResult, TerminalFinalizeSpec,
    TerminalTailCatalogAuthority,
};
use super::write_session::{
    prepare_drive_for_write, run_load_calibration_harvest, WriteMediaPolicy,
};
use super::{SelectedTape, SessionOpenReadinessContext};
use crate::audit_projection::{
    append_operation_audit, ensure_manual_finalize_finished_audit, ensure_tape_sealed_audit,
    OperationAuditInput,
};
use crate::FINALIZE_TAPE_OPERATION_KIND;

pub(super) fn handle_drive_finalize_tape(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    mounted: ManualFinalizeTapeMountRequest,
    retained: &mut Option<super::checkpoint::RetainedParityEpoch>,
) -> Result<ManualFinalizeTapeActorReply, Status> {
    let ManualFinalizeTapeMountRequest {
        request,
        needs_drive_load,
        library_serial,
        barcode,
        source_slot,
        drive_uuid,
        drive_serial,
    } = mounted;
    let epoch = retained.take().filter(|e| {
        !needs_drive_load
            && e.selected.tape_uuid == request.tape_uuid
            && e.identity_generation == drive.identity_generation()
            && cfg.lifecycle.as_ref().is_some_and(|l| {
                l.parked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .by_drive
                    .get(&crate::drive_pool::DriveKey::new(&library_serial, bay))
                    .map_or(e.parked_generation == 0, |p| {
                        p.generation == e.parked_generation
                            && p.seated.tape_uuid == Some(request.tape_uuid)
                    })
            })
    });
    let (carried, _permit) = epoch.map_or((None, None), |e| {
        (Some((e.identity_generation, e.state)), Some(e.permit))
    });
    finalize_mounted_tape(
        index,
        &cfg.terminal_config(),
        drive,
        &request,
        barcode.as_deref(),
        carried,
        |index, drive| {
            session_open_short_probe_or_load(
                index,
                drive,
                SessionOpenReadinessContext {
                    action: "finalize tape",
                    bay,
                    library_serial: library_serial.as_str(),
                    barcode: barcode.as_deref(),
                    source_slot,
                    drive_serial: drive_serial.as_deref(),
                    needs_drive_load,
                },
            )?;
            prepare_drive_for_write(
                drive,
                &request.tape_uuid,
                request.block_size,
                request.candidate_operation_id,
                WriteMediaPolicy::TerminalAppend,
            )?;
            if needs_drive_load {
                run_load_calibration_harvest(
                    index,
                    drive,
                    cfg,
                    &request.tape_uuid,
                    barcode.as_deref(),
                );
            }
            Ok(())
        },
        |index, drive| {
            record_session_close_snapshot(
                index,
                cfg,
                drive,
                drive_uuid,
                request.candidate_operation_id,
                request.tape_uuid,
                snapshot_misses,
            );
        },
    )
}

/// The shared finalization funnel after ownership has been established. The
/// actor supplies load readiness; OperatorMount supplies its already-held lock.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finalize_mounted_tape(
    index: &mut CatalogIndex,
    cfg: &super::terminal_types::TerminalWriteConfig,
    drive: &mut DriveHandle,
    request: &ManualFinalizeTapeActorRequest,
    barcode: Option<&str>,
    carried: Option<(u64, remanence_parity::ParitySinkSessionState)>,
    prepare: impl FnOnce(&mut CatalogIndex, &mut DriveHandle) -> Result<(), Status>,
    finish: impl FnOnce(&mut CatalogIndex, &mut DriveHandle),
) -> Result<ManualFinalizeTapeActorReply, Status> {
    validate_manual_finalize_owned_request(index, request)?;
    session_open_reject_tape_io_fences(index, &request.tape_uuid, barcode, "finalize tape")?;

    let checkpoint_journal = remanence_state::TapeAuthority::open(
        cfg.checkpoint_journal_dir.as_path(),
        request.tape_uuid,
        index.path(),
    )
    .map_err(crate::status_from_state_error)?;
    let pending_intent = checkpoint_journal
        .pending_finalization()
        .map_err(crate::status_from_state_error)?;
    let (mut checkpoint_lease, existing_intent) = if pending_intent.is_some() {
        let lease = checkpoint_journal
            .acquire_exclusive_for_terminal_recovery()
            .map_err(crate::status_from_state_error)?;
        let intent = lease
            .pending_finalization()
            .map_err(crate::status_from_state_error)?;
        (lease, intent)
    } else {
        let lease = checkpoint_journal
            .acquire_exclusive()
            .map_err(crate::status_from_state_error)?;
        (lease, None)
    };
    let records = project_checkpoint_authority_bounded(index, &checkpoint_lease)
        .map_err(crate::status_from_state_error)?;
    if records.last().is_some_and(|record| record.sealed) {
        let sealed = records
            .last()
            .expect("sealed checkpoint predicate requires a final record");
        let completion = sealed.finalization.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "sealed checkpoint authority has no terminal-finalization completion",
            )
        })?;
        ensure_tape_sealed_audit(
            index,
            cfg.audit_dir.as_path(),
            &cfg.audit_append_lock,
            request.tape_uuid,
        )?;
        ensure_manual_finalize_finished_audit(
            index,
            cfg.audit_dir.as_path(),
            &cfg.audit_append_lock,
            request.tape_uuid,
            completion,
        )?;
        let projection = index
            .terminal_finalization(&request.tape_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| {
                Status::failed_precondition(
                    "sealed checkpoint authority has no terminal-finalization projection",
                )
            })?;
        if projection.operation_id != Some(request.candidate_operation_id) {
            return Err(Status::already_exists(
                "tape was finalized by a different operation",
            ));
        }
        return Ok(ManualFinalizeTapeActorReply {
            operation_id: request.candidate_operation_id,
            projection,
        });
    }
    if records.is_empty() {
        return Err(Status::failed_precondition(
            "manual finalization requires at least one durable checkpoint",
        ));
    }
    let accepted_intent = existing_intent.as_ref().ok_or_else(|| {
        Status::failed_precondition(
            "FinalizeTape drive dispatch is missing its durable BeforeReplicaA intent",
        )
    })?;
    validate_manual_finalize_intent(request, accepted_intent)?;
    require_manual_finalize_preflight_binding(index, request)?;
    prepare(index, drive)?;
    // Readiness can invalidate identity even when no load was requested.
    let carried = carried
        .filter(|(generation, _)| *generation == drive.identity_generation())
        .map(|(_, state)| state);
    let drive_config = drive
        .read_config()
        .map_err(|error| Status::unavailable(format!("read finalization drive config: {error}")))?;
    if drive_config.write_protected {
        return Err(Status::failed_precondition(
            "tape is write-protected and cannot be finalized",
        ));
    }
    let rewritable = matches!(
        drive_config.worm,
        remanence_library::WormMediaState::NotWorm
    );
    let selected = SelectedTape {
        standalone_endpoint: cfg.mounted_endpoint.clone().or_else(|| {
            drive
                .endpoint()
                .durable_key()
                .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. }))
                .cloned()
        }),
        pool_id: request
            .expected_pool_id
            .clone()
            .unwrap_or_else(|| "(unpooled)".to_string()),
        tape_uuid: request.tape_uuid,
        block_size: request.block_size,
        parity_config: request.parity_config.clone(),
    };
    let spec = TerminalFinalizeSpec::operator(request);

    let result = match request.parity_config {
        remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
            index,
            cfg,
            drive,
            &mut checkpoint_lease,
            records
                .last()
                .expect("manual finalization requires checkpoint authority"),
            existing_intent,
            &spec,
            &selected,
            Some(request),
            rewritable,
        ),
        remanence_parity::ParityConfig::Scheme(_) => finalize_terminal_with_parity(
            index,
            cfg,
            drive,
            &mut checkpoint_lease,
            records
                .last()
                .expect("manual finalization requires checkpoint authority"),
            existing_intent,
            &spec,
            &selected,
            Some(request),
            rewritable,
            carried,
        ),
    };

    finish(index, drive);
    result.map(|result| ManualFinalizeTapeActorReply {
        operation_id: request.candidate_operation_id,
        projection: result.projection,
    })
}

pub(super) fn validate_manual_finalize_intent(
    request: &ManualFinalizeTapeActorRequest,
    intent: &remanence_state::authority::AuthorityFinalization,
) -> Result<(), Status> {
    let manual = intent.manual.as_ref().ok_or_else(|| {
        Status::failed_precondition("pending terminal finalization is not an operator close-out")
    })?;
    if intent.tape_uuid != request.tape_uuid
        || intent.trigger != remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        || manual.operation_id != *request.candidate_operation_id.as_bytes()
        || manual.operation_kind != FINALIZE_TAPE_OPERATION_KIND
        || manual.actor_fingerprint != request.actor_fingerprint
        || manual.idempotency_key != *request.idempotency_key.as_bytes()
        || manual.request_fingerprint != request.request_fingerprint
        || manual.assigned_pool_id != request.expected_pool_id
        || manual.expected_pool_id != request.expected_pool_id
        || manual.assignment_generation != request.assignment_generation
        || manual.reason != request.reason
    {
        return Err(Status::already_exists(
            "pending terminal finalization belongs to a different exact request",
        ));
    }
    Ok(())
}

pub(super) fn authorize_terminal_intent_capacity(
    index: &CatalogIndex,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    intent: &remanence_state::authority::AuthorityFinalization,
    counts: remanence_parity::TapeIndexReplicaCounts,
) -> Result<(), Status> {
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        // Capacity admission governs future tape motion. Once replica C is
        // durably proved, the reserved tail is already on media; reapplying a
        // changed cap or watermark here could only obstruct host metadata
        // completion and cannot protect tape capacity.
        return Ok(());
    }
    crate::pool_write::authorize_terminal_close_only_plan(
        index,
        spec.pool_config.as_ref(),
        selected,
        intent.layout.components[0].start_lba,
        counts,
        intent.layout.expected_eod_lba,
    )
    .map_err(|err| status_from_selected_write_error(selected, err))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_terminal_prefix_without_motion(
    index: &CatalogIndex,
    selected: &SelectedTape,
    checkpoint: &remanence_state::TapeAuthorityLease,
    _checkpoint_journal_dir: &Path,
    previous: &remanence_state::authority::AuthorityCommit,
    request: &ManualFinalizeTapeActorRequest,
    spec: &TerminalFinalizeSpec,
    prepared_prefix: Option<&TerminalPrefixPlan>,
) -> Result<
    (
        remanence_state::authority::AuthorityFinalization,
        TerminalTripleWritePlan,
    ),
    Status,
> {
    let scheme = match &request.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity terminal-prefix preflight called for parity-off tape",
            ));
        }
    };
    let journal = checkpoint
        .open_structural(request.block_size, scheme.clone())
        .map_err(|error| Status::failed_precondition(format!("open parity journal: {error}")))?;

    // Prove the checkpoint/parity bijection and reject every orphan before
    // accepting the immutable terminal plan. The close seed retains only
    // one length-bounded bundle plus epoch-directory metadata.
    let base_authority =
        remanence_state::TerminalIndexSource::new_replay_backed(checkpoint, &journal)
            .map_err(crate::status_from_state_error)?;
    if base_authority
        .summary()
        .scope
        .covered_prefix_tape_file_count
        == 0
    {
        return Err(Status::failed_precondition(
            "checkpointed parity tape has no replayable committed prefix",
        ));
    }
    let snapshot = journal
        .freeze()
        .map_err(|error| Status::failed_precondition(format!("replay parity journal: {error}")))?;
    let prefix = if let Some(prefix) = prepared_prefix {
        prefix.clone()
    } else {
        remanence_parity::plan_checkpointed_terminal_index_close(
            &snapshot,
            remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
                .capture()
                .map_err(|error| status_from_parity_error(&error, error.to_string()))?,
        )
        .map_err(|error| status_from_parity_error(&error, error.to_string()))?
    };
    let expected_start_file = previous.next_file;
    if prefix.start_tape_file_number != expected_start_file
        || prefix.start_lba != previous.cursor.lba
    {
        return Err(Status::failed_precondition(format!(
            "bounded terminal close seed starts at tape file {}/LBA {}, expected {expected_start_file}/{}",
            prefix.start_tape_file_number, prefix.start_lba, previous.cursor.lba
        )));
    }
    let persisted = remanence_state::authority::AuthorityPrefixPlan::from(&prefix);
    let source =
        remanence_state::TerminalIndexSource::new_replay_backed_with_planned_terminal_prefix(
            checkpoint, &journal, &persisted,
        )
        .map_err(crate::status_from_state_error)?;
    let mut source = source;
    build_new_terminal_plan(
        index,
        selected,
        spec,
        previous,
        &mut source,
        TerminalPlanPosition {
            first_tape_file_number: prefix.tail_start_tape_file_number,
            first_start_lba: prefix.tail_start_lba,
            terminal_prefix: Some(&prefix),
        },
    )
}

#[derive(Clone, Copy)]
pub(super) struct TerminalPlanPosition<'a> {
    first_tape_file_number: u64,
    first_start_lba: u64,
    terminal_prefix: Option<&'a TerminalPrefixPlan>,
}

pub(super) fn build_new_terminal_plan(
    index: &CatalogIndex,
    selected: &SelectedTape,
    spec: &TerminalFinalizeSpec,
    previous: &remanence_state::authority::AuthorityCommit,
    source: &mut remanence_state::TerminalIndexSource,
    position: TerminalPlanPosition<'_>,
) -> Result<
    (
        remanence_state::authority::AuthorityFinalization,
        TerminalTripleWritePlan,
    ),
    Status,
> {
    let TerminalPlanPosition {
        first_tape_file_number,
        first_start_lba,
        terminal_prefix,
    } = position;
    let summary = source.summary();
    let replica =
        remanence_parity::checked_tape_index_replica_layout(spec.block_size, summary.counts)
            .map_err(|error| {
                Status::failed_precondition(format!("plan terminal replica: {error}"))
            })?;
    let separation_records = remanence_parity::index_separation_records(
        spec.block_size,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .map_err(|error| Status::failed_precondition(format!("plan terminal separation: {error}")))?;
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        spec.block_size,
        first_tape_file_number,
        first_start_lba,
        replica.replica_record_count,
        separation_records,
    )
    .map_err(|error| Status::failed_precondition(format!("plan terminal layout: {error}")))?;
    crate::pool_write::authorize_terminal_close_only_plan(
        index,
        spec.pool_config.as_ref(),
        selected,
        first_start_lba,
        summary.counts,
        layout.expected_eod_lba,
    )
    .map_err(|err| status_from_selected_write_error(selected, err))?;
    let edition_sequence = previous
        .ordinal
        .checked_add(1)
        .ok_or_else(|| Status::failed_precondition("terminal edition sequence overflows u64"))?;
    let edition_id = Uuid::new_v4();
    let diagnostics = match terminal_prefix {
        Some(prefix) => prefix.diagnostics.clone(),
        None => remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
            .capture()
            .map_err(|error| status_from_parity_error(&error, error.to_string()))?,
    };
    let writer_version = diagnostics.writer_version.clone();
    let write_timestamp = diagnostics.write_timestamp.clone();
    let plan = remanence_parity::assemble_terminal_plan(
        spec.tape_uuid,
        spec.block_size,
        false,
        edition_sequence,
        summary.scope,
        summary.counts,
        source,
        layout,
        diagnostics,
        *edition_id.as_bytes(),
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .map_err(|error| Status::failed_precondition(format!("plan terminal edition: {error}")))?;
    let intent = remanence_state::authority::AuthorityFinalization {
        tape_uuid: spec.tape_uuid,
        trigger: spec.trigger,
        manual: spec.manual.clone(),
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id: *edition_id.as_bytes(),
        edition_sequence,
        edition_digest: plan.edition.edition_digest,
        writer_version,
        write_timestamp,
        terminal_prefix: terminal_prefix.map(remanence_state::authority::AuthorityPrefixPlan::from),
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
            .map_err(crate::status_from_state_error)?,
    };
    Ok((intent, plan))
}

pub(super) fn publish_terminal_intent(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    spec: &TerminalFinalizeSpec,
    intent: &remanence_state::authority::AuthorityFinalization,
) -> Result<(), Status> {
    if let Some(fault) = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(spec.tape_uuid)
        .map_err(Status::failed_precondition)?
    {
        fault
            .install(checkpoint)
            .map_err(Status::failed_precondition)?;
    }
    checkpoint
        .begin_finalization(intent)
        .map_err(crate::status_from_state_error)?;
    index
        .project_terminal_finalization(TerminalFinalizationProjectionInput {
            tape_uuid: spec.tape_uuid,
            trigger: spec.trigger,
            operation_id: spec.operation_id,
            progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
            edition_digest: intent.edition_digest,
            layout_digest: intent.layout.layout_digest,
            outcome: TerminalFinalizationOutcome::InProgress,
            updated_at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    Ok(())
}

/// Distinguish an accepted manual intent from the provisional companion left
/// if the process dies before the guarded SQLite transaction commits.
///
/// Both database halves are committed by one transaction. Seeing neither is
/// therefore the sole removable provisional state; seeing exactly one is
/// corruption, while seeing both requires exact equality before recovery.
pub(crate) fn reconcile_manual_terminal_acceptance(
    index: &CatalogIndex,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    intent: &remanence_state::authority::AuthorityFinalization,
) -> Result<bool, Status> {
    let manual = intent.manual.as_ref().ok_or_else(|| {
        Status::failed_precondition("manual acceptance reconciliation has no manual identity")
    })?;
    if intent.trigger != remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        || manual.operation_kind != FINALIZE_TAPE_OPERATION_KIND
    {
        return Err(Status::failed_precondition(
            "manual acceptance reconciliation has invalid operation authority",
        ));
    }
    let operation_id = Uuid::from_bytes(manual.operation_id);
    let idempotency_key = Uuid::from_bytes(manual.idempotency_key);
    let projection = index
        .terminal_finalization(&intent.tape_uuid)
        .map_err(crate::status_from_state_error)?;
    let scope = index
        .idempotency_scope_record(
            manual.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
        )
        .map_err(crate::status_from_state_error)?;
    match (projection, scope) {
        (None, None) => {
            checkpoint
                .retire_preparation(intent, false, false)
                .map_err(crate::status_from_state_error)?;
            Ok(false)
        }
        (Some(_), None) | (None, Some(_)) => Err(Status::failed_precondition(
            "manual terminal acceptance is corrupt: finalization projection and idempotency binding are not both present",
        )),
        (Some(projection), Some(scope)) => {
            if projection.trigger != intent.trigger
                || projection.operation_id != Some(operation_id)
                || projection.edition_digest != intent.edition_digest
                || projection.layout_digest != intent.layout.layout_digest
                || scope.operation_id != operation_id
                || scope.request_fingerprint.as_slice() != manual.request_fingerprint
            {
                return Err(Status::failed_precondition(
                    "manual terminal acceptance database authority differs from its checkpoint companion",
                ));
            }
            checkpoint.accept_preparation().map_err(crate::status_from_state_error)?;
            Ok(true)
        }
    }
}

/// Complete manual close-out admission without possessing any changer or drive
/// capability. Every request-dependent rejection therefore precedes robotics,
/// LOAD, rewind, locate, mode-page configuration, and tape writes.
pub(crate) fn preflight_manual_finalize_tape(
    index: &mut CatalogIndex,
    cfg: ManualFinalizePreflightConfig<'_>,
    barcode: Option<&str>,
    request: &mut ManualFinalizeTapeActorRequest,
) -> Result<Option<ManualFinalizeTapeActorReply>, Status> {
    preflight_manual_finalize_with_prefix(index, cfg, barcode, request, None)
}

/// Accept an immutable terminal plan after an open epoch is reconstructed without writing.
pub(crate) fn preflight_manual_finalize_with_prefix(
    index: &mut CatalogIndex,
    cfg: ManualFinalizePreflightConfig<'_>,
    barcode: Option<&str>,
    request: &mut ManualFinalizeTapeActorRequest,
    prefix: Option<&TerminalPrefixPlan>,
) -> Result<Option<ManualFinalizeTapeActorReply>, Status> {
    preflight_manual_finalize_cancellable(index, cfg, barcode, request, prefix, || false)
}

/// Recheck caller lifetime at the durable admission boundary after slow planning.
pub(crate) fn preflight_manual_finalize_cancellable(
    index: &mut CatalogIndex,
    cfg: ManualFinalizePreflightConfig<'_>,
    barcode: Option<&str>,
    request: &mut ManualFinalizeTapeActorRequest,
    prefix: Option<&TerminalPrefixPlan>,
    cancelled: impl Fn() -> bool,
) -> Result<Option<ManualFinalizeTapeActorReply>, Status> {
    let checkpoint_journal = remanence_state::TapeAuthority::open(
        cfg.checkpoint_journal_dir,
        request.tape_uuid,
        index.path(),
    )
    .map_err(crate::status_from_state_error)?;
    let pending_intent = checkpoint_journal
        .pending_finalization()
        .map_err(crate::status_from_state_error)?;
    let (mut checkpoint, mut existing_intent) = if pending_intent.is_some() {
        let lease = checkpoint_journal
            .acquire_exclusive_for_terminal_recovery()
            .map_err(crate::status_from_state_error)?;
        let intent = lease
            .pending_finalization()
            .map_err(crate::status_from_state_error)?;
        (lease, intent)
    } else {
        let lease = checkpoint_journal
            .acquire_exclusive()
            .map_err(crate::status_from_state_error)?;
        (lease, None)
    };

    #[cfg(test)]
    PREFLIGHT_LEASE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });

    // A durable Decided group reconstructs both acceptance halves; a Prepared
    // group projects neither and remains subject to the retirement guard.
    project_checkpoint_authority_bounded(index, &checkpoint)
        .map_err(crate::status_from_state_error)?;
    if let Some(intent) = existing_intent
        .as_ref()
        .filter(|intent| intent.manual.is_some())
    {
        if !reconcile_manual_terminal_acceptance(index, &mut checkpoint, intent)? {
            existing_intent = None;
        }
    }

    // The checkpoint intent is the crash-recovery authority. If the process
    // stopped after that fsync but before the audit projection, recover the
    // original operation id directly from the exact actor/kind/key binding.
    if let Some(intent) = existing_intent.as_ref() {
        if let Some(manual) = intent.manual.as_ref() {
            if manual.operation_kind == FINALIZE_TAPE_OPERATION_KIND
                && manual.actor_fingerprint == request.actor_fingerprint
                && manual.idempotency_key == *request.idempotency_key.as_bytes()
            {
                if manual.request_fingerprint != request.request_fingerprint {
                    return Err(Status::already_exists(
                        "FinalizeTape idempotency key is already bound to a different request",
                    ));
                }
                request.candidate_operation_id = Uuid::from_bytes(manual.operation_id);
            }
        }
    }
    if let Some(scope) = index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
    {
        if scope.request_fingerprint.as_slice() != request.request_fingerprint {
            return Err(Status::already_exists(
                "FinalizeTape idempotency key is already bound to a different request",
            ));
        }
        request.candidate_operation_id = scope.operation_id;
    }
    let had_existing_intent = existing_intent.is_some();

    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(request.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    if let Some(fault) = tix_fault.as_ref() {
        fault
            .clear_assignment_before_reread(
                index,
                request.tape_uuid,
                request.assignment_generation,
                request.expected_pool_id.as_deref(),
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX assignment-race hook: {error}"))
            })?;
    }
    validate_manual_finalize_owned_request(index, request)?;
    let previous = checkpoint
        .latest_commit()
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| {
            Status::failed_precondition(
                "manual finalization requires at least one durable checkpoint",
            )
        })?;
    if previous.sealed {
        if had_existing_intent {
            project_sealed_checkpoint_then_retire_terminal_intent(
                index,
                &mut checkpoint,
                &previous,
            )?;
        } else {
            index
                .project_tape_log(
                    &checkpoint
                        .freeze()
                        .map_err(crate::status_from_state_error)?,
                )
                .map_err(crate::status_from_state_error)?;
        }
        let completion = previous.finalization.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "sealed checkpoint authority has no terminal-finalization completion",
            )
        })?;
        ensure_tape_sealed_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            request.tape_uuid,
        )?;
        ensure_manual_finalize_finished_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            request.tape_uuid,
            completion,
        )?;
        let projection = index
            .terminal_finalization(&request.tape_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| {
                Status::failed_precondition(
                    "sealed checkpoint authority has no terminal-finalization projection",
                )
            })?;
        if projection.operation_id != Some(request.candidate_operation_id) {
            return Err(Status::already_exists(
                "tape was finalized by a different operation",
            ));
        }
        return Ok(Some(ManualFinalizeTapeActorReply {
            operation_id: request.candidate_operation_id,
            projection,
        }));
    }
    if !had_existing_intent {
        session_open_reject_tape_io_fences(index, &request.tape_uuid, barcode, "finalize tape")?;
    }

    let spec = TerminalFinalizeSpec::operator(request);
    let selected = SelectedTape {
        standalone_endpoint: cfg.mounted_endpoint.cloned(),
        pool_id: request
            .expected_pool_id
            .clone()
            .unwrap_or_else(|| "(unpooled)".to_string()),
        tape_uuid: request.tape_uuid,
        block_size: request.block_size,
        parity_config: request.parity_config.clone(),
    };
    let new_acceptance = existing_intent.is_none();
    let (intent, plan) = match existing_intent {
        Some(intent) => {
            validate_manual_finalize_intent(request, &intent)?;
            match &request.parity_config {
                remanence_parity::ParityConfig::None => {
                    if intent.terminal_prefix.is_some() {
                        return Err(Status::failed_precondition(
                            "parity-off finalization intent unexpectedly has a parity prefix",
                        ));
                    }
                    let mut source =
                        remanence_state::TerminalIndexSource::new_replay_backed_no_parity(
                            &checkpoint,
                        )
                        .map_err(crate::status_from_state_error)?;
                    let plan = source
                        .reconstruct_final_plan(&intent)
                        .map_err(crate::status_from_state_error)?;
                    authorize_terminal_intent_capacity(
                        index,
                        &spec,
                        &selected,
                        &intent,
                        source.summary().counts,
                    )?;
                    (intent, plan)
                }
                remanence_parity::ParityConfig::Scheme(scheme) => {
                    let persisted = intent.terminal_prefix.as_ref().ok_or_else(|| {
                        Status::failed_precondition("parity finalization intent has no prefix plan")
                    })?;
                    let journal = checkpoint
                        .open_structural(request.block_size, scheme.clone())
                        .map_err(|error| {
                            Status::failed_precondition(format!("open parity journal: {error}"))
                        })?;
                    let mut source = if journal
                        .prefix_is_durable(&persisted.publication)
                        .map_err(|error| {
                            Status::failed_precondition(format!(
                                "inspect terminal-prefix journal transition: {error}"
                            ))
                        })?
                    {
                        remanence_state::TerminalIndexSource::new_replay_backed_after_terminal_prefix(
                            &checkpoint,
                            &journal,
                            persisted,
                        )
                    } else {
                        remanence_state::TerminalIndexSource::new_replay_backed_with_planned_terminal_prefix(
                            &checkpoint,
                            &journal,
                            persisted,
                        )
                    }
                    .map_err(crate::status_from_state_error)?;
                    let plan = source
                        .reconstruct_final_plan(&intent)
                        .map_err(crate::status_from_state_error)?;
                    drop(source);
                    checkpoint
                        .retain_structural_publisher(journal)
                        .map_err(crate::status_from_state_error)?;
                    let intent = checkpoint
                        .with_structural(|checkpoint, rows| {
                            checkpoint.finish_structural_scope();
                            reconcile_and_authorize_parity_resume(
                                index,
                                checkpoint,
                                &spec,
                                &selected,
                                intent,
                                &plan,
                                rows.ok_or_else(|| {
                                    Status::internal("missing retained structural capability")
                                })?,
                            )
                        })
                        .map_err(crate::status_from_state_error)??;
                    (intent, plan)
                }
            }
        }
        None => match request.parity_config {
            remanence_parity::ParityConfig::None => {
                let mut source =
                    remanence_state::TerminalIndexSource::new_replay_backed_no_parity(&checkpoint)
                        .map_err(crate::status_from_state_error)?;
                let first_file = source.summary().scope.covered_prefix_tape_file_count;
                build_new_terminal_plan(
                    index,
                    &selected,
                    &spec,
                    &previous,
                    &mut source,
                    TerminalPlanPosition {
                        first_tape_file_number: first_file,
                        first_start_lba: previous.cursor.lba,
                        terminal_prefix: None,
                    },
                )?
            }
            remanence_parity::ParityConfig::Scheme(_) => plan_terminal_prefix_without_motion(
                index,
                &selected,
                &checkpoint,
                cfg.checkpoint_journal_dir,
                &previous,
                request,
                &spec,
                prefix,
            )?,
        },
    };

    if cancelled() && new_acceptance {
        return Err(Status::cancelled(
            "finalization request dropped before acceptance",
        ));
    }
    // Planning may stream a large authority. Re-read at the acceptance edge,
    // then let the immediate catalog transaction repeat the same guard while
    // holding assignment writers out through companion publication.
    validate_manual_finalize_owned_request(index, request)?;
    if let Some(fault) = tix_fault.as_ref() {
        fault
            .clear_assignment_after_reread_before_acceptance(
                index,
                request.tape_uuid,
                request.assignment_generation,
                request.expected_pool_id.as_deref(),
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX assignment-race hook: {error}"))
            })?;
    }
    let projected_recovery_required = index
        .terminal_finalization(&request.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .is_some_and(|projection| {
            projection.progress == intent.progress
                && projection.outcome == TerminalFinalizationOutcome::RecoveryRequired
        });
    let projection = TerminalFinalizationProjectionInput {
        tape_uuid: request.tape_uuid,
        trigger: intent.trigger,
        operation_id: Some(request.candidate_operation_id),
        progress: intent.progress,
        edition_digest: intent.edition_digest,
        layout_digest: intent.layout.layout_digest,
        outcome: if intent.recovery_required || projected_recovery_required {
            TerminalFinalizationOutcome::RecoveryRequired
        } else {
            TerminalFinalizationOutcome::InProgress
        },
        updated_at_utc: None,
    };
    if intent.progress == remanence_state::TerminalFinalizationProgress::BeforeReplicaA {
        index
            .accept_manual_terminal_finalization_with(
                ManualTerminalFinalizationAcceptanceInput {
                    tape_uuid: request.tape_uuid,
                    expected_pool_id: request.expected_pool_id.clone(),
                    expected_assignment_generation: request.assignment_generation,
                    actor_fingerprint: request.actor_fingerprint.clone(),
                    operation_kind: FINALIZE_TAPE_OPERATION_KIND.to_string(),
                    idempotency_key: request.idempotency_key,
                    request_fingerprint: request.request_fingerprint,
                    operation_id: request.candidate_operation_id,
                    projection,
                },
                || {
                    if let Some(fault) = tix_fault.as_ref() {
                        fault
                            .install(&checkpoint)
                            .map_err(remanence_state::StateError::JournalReplayFailed)?;
                    }
                    checkpoint.begin_finalization(&intent)?;
                    #[cfg(test)]
                    PREFLIGHT_INTENT_HOOK.with(|hook| {
                        if let Some(hook) = hook.borrow_mut().take() {
                            hook();
                        }
                    });
                    if new_acceptance && cancelled() {
                        checkpoint.retire_preparation(&intent, false, false)?;
                        return Err(remanence_state::StateError::ConfigInvalid(
                            "finalization request dropped before acceptance".into(),
                        ));
                    }
                    Ok(())
                },
            )
            .map_err(|error| match error {
                remanence_state::StateError::ConfigInvalid(detail) if cancelled() => {
                    Status::cancelled(detail)
                }
                remanence_state::StateError::TapePoolAssignmentConflict(detail) => {
                    Status::failed_precondition(detail)
                }
                other => crate::status_from_state_error(other),
            })?;
    } else {
        index
            .project_terminal_finalization(projection)
            .map_err(crate::status_from_state_error)?;
    }
    checkpoint
        .accept_preparation()
        .map_err(crate::status_from_state_error)?;
    // A crash after acceptance but before this append is repaired from the
    // accepted companion before any physical dispatch.
    record_manual_finalize_request_with(
        index,
        cfg.audit_dir,
        cfg.audit_fsync,
        cfg.audit_append_lock,
        request,
    )?;
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        let result = complete_terminal_finalization_host_only(
            index,
            TerminalFinalizeAuditConfig {
                audit_dir: cfg.audit_dir,
                audit_fsync: cfg.audit_fsync,
                audit_append_lock: cfg.audit_append_lock,
            },
            &mut checkpoint,
            &previous,
            &spec,
            intent,
            &plan,
            tix_fault.as_ref(),
        )?;
        return Ok(Some(ManualFinalizeTapeActorReply {
            operation_id: request.candidate_operation_id,
            projection: result.projection,
        }));
    }
    session_open_reject_tape_io_fences(index, &request.tape_uuid, barcode, "finalize tape")?;
    Ok(None)
}

/// Complete an automatic finalization whose terminal media tail is already
/// durable, without acquiring any drive or changer capability.
pub(crate) fn preflight_automatic_terminal_completion(
    index: &mut CatalogIndex,
    cfg: ManualFinalizePreflightConfig<'_>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
) -> Result<bool, Status> {
    let checkpoint_journal = remanence_state::TapeAuthority::open(
        cfg.checkpoint_journal_dir,
        selected.tape_uuid,
        index.path(),
    )
    .map_err(crate::status_from_state_error)?;
    let pending = checkpoint_journal
        .pending_finalization()
        .map_err(crate::status_from_state_error)?;
    if pending
        .as_ref()
        .is_some_and(|intent| intent.manual.is_some())
    {
        return Ok(false);
    }
    let (mut checkpoint, intent) = if pending.is_some() {
        let checkpoint = checkpoint_journal
            .acquire_exclusive_for_terminal_recovery()
            .map_err(crate::status_from_state_error)?;
        let intent = checkpoint
            .pending_finalization()
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| {
                Status::internal("automatic terminal intent disappeared in preflight")
            })?;
        (checkpoint, Some(intent))
    } else {
        (
            checkpoint_journal
                .acquire_exclusive()
                .map_err(crate::status_from_state_error)?,
            None,
        )
    };
    let previous = checkpoint
        .latest_commit()
        .map_err(crate::status_from_state_error)?;
    let Some(previous) = previous else {
        if intent.is_none() {
            return Ok(false);
        }
        return Err(Status::failed_precondition(
            "automatic terminal completion requires checkpoint authority",
        ));
    };
    if previous.sealed {
        let completion = previous.finalization.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "sealed checkpoint authority has no terminal-finalization completion",
            )
        })?;
        if completion.manual.is_some() {
            return Ok(false);
        }
        if intent.is_some() {
            project_sealed_checkpoint_then_retire_terminal_intent(
                index,
                &mut checkpoint,
                &previous,
            )?;
        } else {
            // An uninterrupted owner may retire the companion immediately
            // after the sealed checkpoint fsync, then fail before SQLite or
            // audit repair. The sealed record remains sufficient host-only
            // authority; ordinary journal ownership excludes a concurrent
            // writer while it is projected.
            index
                .project_tape_log(
                    &checkpoint
                        .freeze()
                        .map_err(crate::status_from_state_error)?,
                )
                .map_err(crate::status_from_state_error)?;
        }
        ensure_tape_sealed_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            selected.tape_uuid,
        )?;
        ensure_manual_finalize_finished_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            selected.tape_uuid,
            completion,
        )?;
        return Ok(true);
    }
    let Some(intent) = intent else {
        return Ok(false);
    };
    let spec = TerminalFinalizeSpec::resume(&intent, selected.block_size, pool_cfg);
    let (intent, plan) = match &selected.parity_config {
        remanence_parity::ParityConfig::None => {
            if intent.terminal_prefix.is_some() {
                return Err(Status::failed_precondition(
                    "parity-off finalization intent unexpectedly has a parity prefix",
                ));
            }
            let mut source =
                remanence_state::TerminalIndexSource::new_replay_backed_no_parity(&checkpoint)
                    .map_err(crate::status_from_state_error)?;
            let plan = source
                .reconstruct_final_plan(&intent)
                .map_err(crate::status_from_state_error)?;
            authorize_terminal_intent_capacity(
                index,
                &spec,
                selected,
                &intent,
                source.summary().counts,
            )?;
            (intent, plan)
        }
        remanence_parity::ParityConfig::Scheme(scheme) => {
            let persisted = intent.terminal_prefix.as_ref().ok_or_else(|| {
                Status::failed_precondition("parity finalization intent has no prefix plan")
            })?;
            let journal = checkpoint
                .open_structural(selected.block_size, scheme.clone())
                .map_err(|error| {
                    Status::failed_precondition(format!("open parity journal: {error}"))
                })?;
            let prefix =
                TerminalPrefixPlan::try_from(persisted).map_err(crate::status_from_state_error)?;
            let prefix_is_durable = journal
                .prefix_is_durable(
                    &prefix
                        .media_publication()
                        .map_err(|e| Status::failed_precondition(e.to_string()))?,
                )
                .map_err(|error| {
                    Status::failed_precondition(format!(
                        "inspect terminal-prefix journal transition: {error}"
                    ))
                })?;
            let mut source = if prefix_is_durable {
                remanence_state::TerminalIndexSource::new_replay_backed_after_terminal_prefix(
                    &checkpoint,
                    &journal,
                    persisted,
                )
            } else {
                remanence_state::TerminalIndexSource::new_replay_backed_with_planned_terminal_prefix(
                    &checkpoint,
                    &journal,
                    persisted,
                )
            }
            .map_err(crate::status_from_state_error)?;
            let plan = source
                .reconstruct_final_plan(&intent)
                .map_err(crate::status_from_state_error)?;
            drop(source);
            checkpoint
                .retain_structural_publisher(journal)
                .map_err(crate::status_from_state_error)?;
            let intent = checkpoint
                .with_structural(|checkpoint, rows| {
                    checkpoint.finish_structural_scope();
                    reconcile_and_authorize_parity_resume(
                        index,
                        checkpoint,
                        &spec,
                        selected,
                        intent,
                        &plan,
                        rows.ok_or_else(|| {
                            Status::internal("missing retained structural capability")
                        })?,
                    )
                })
                .map_err(crate::status_from_state_error)??;
            (intent, plan)
        }
    };
    if intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        return Ok(false);
    }
    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(selected.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    complete_terminal_finalization_host_only(
        index,
        TerminalFinalizeAuditConfig {
            audit_dir: cfg.audit_dir,
            audit_fsync: cfg.audit_fsync,
            audit_append_lock: cfg.audit_append_lock,
        },
        &mut checkpoint,
        &previous,
        &spec,
        intent,
        &plan,
        tix_fault.as_ref(),
    )?;
    Ok(true)
}

/// Project sealed authority before retiring its matching recovery companion.
///
/// The recovery lease acquisition has already proved that the sealed
/// completion exactly matches the normalized companion. Keeping the companion
/// through projection preserves host-only retry routing if SQLite rejects the
/// update; replay removes it only after projection succeeds.
pub(super) fn project_sealed_checkpoint_then_retire_terminal_intent(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    _previous: &remanence_state::authority::AuthorityCommit,
) -> Result<(), Status> {
    index
        .project_tape_log(
            &checkpoint
                .freeze()
                .map_err(crate::status_from_state_error)?,
        )
        .map_err(crate::status_from_state_error)?;
    checkpoint
        .recover_finalization()
        .map_err(crate::status_from_state_error)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_terminal_no_parity(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    previous: &remanence_state::authority::AuthorityCommit,
    existing_intent: Option<remanence_state::authority::AuthorityFinalization>,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    rewritable: bool,
) -> Result<TerminalFinalizeResult, Status> {
    drive.clear_append_admission();
    let cfg = &cfg.terminal_config();
    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(spec.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    let mut source = remanence_state::TerminalIndexSource::new_replay_backed_no_parity(checkpoint)
        .map_err(crate::status_from_state_error)?;
    let (intent, plan) = match existing_intent {
        Some(intent) => {
            if intent.terminal_prefix.is_some() {
                return Err(Status::failed_precondition(
                    "parity-off finalization intent unexpectedly has a parity prefix",
                ));
            }
            let plan = source
                .reconstruct_final_plan(&intent)
                .map_err(crate::status_from_state_error)?;
            authorize_terminal_intent_capacity(
                index,
                spec,
                selected,
                &intent,
                source.summary().counts,
            )?;
            (intent, plan)
        }
        None => {
            let first_file = source.summary().scope.covered_prefix_tape_file_count;
            let planned = build_new_terminal_plan(
                index,
                selected,
                spec,
                previous,
                &mut source,
                TerminalPlanPosition {
                    first_tape_file_number: first_file,
                    first_start_lba: previous.cursor.lba,
                    terminal_prefix: None,
                },
            )?;
            publish_terminal_intent(index, checkpoint, spec, &planned.0)?;
            planned
        }
    };
    if let Some(request) = manual_request {
        record_manual_finalize_request(index, cfg, request)?;
    }
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        let _admission = admit_terminal_write(
            &spec.actor,
            index,
            cfg,
            drive,
            previous,
            selected,
            &plan,
            None,
        )?;
        drop(source);
        return complete_terminal_finalization_host_only(
            index,
            TerminalFinalizeAuditConfig::from(cfg),
            checkpoint,
            previous,
            spec,
            intent,
            &plan,
            tix_fault.as_ref(),
        );
    }
    drive
        .locate(previous.cursor.lba)
        .map_err(|error| Status::unavailable(format!("locate checkpoint EOD: {error}")))?;
    finish_terminal_tail(
        index,
        cfg,
        drive,
        checkpoint,
        previous,
        spec,
        selected,
        manual_request,
        intent,
        plan,
        source,
        None,
        rewritable,
        tix_fault.as_ref(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_terminal_with_parity(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    previous: &remanence_state::authority::AuthorityCommit,
    existing_intent: Option<remanence_state::authority::AuthorityFinalization>,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    rewritable: bool,
    carried: Option<remanence_parity::ParitySinkSessionState>,
) -> Result<TerminalFinalizeResult, Status> {
    let cfg = &cfg.terminal_config();
    let scheme = match &selected.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity finalization called for parity-off tape",
            ));
        }
    };
    let mut journal = checkpoint
        .open_structural(spec.block_size, scheme)
        .map_err(|error| Status::failed_precondition(format!("open parity journal: {error}")))?;
    finalize_terminal_with_parity_journal(
        index,
        cfg,
        drive,
        checkpoint,
        previous,
        existing_intent,
        spec,
        selected,
        manual_request,
        journal.as_mut(),
        rewritable,
        carried,
    )
}

/// Finalize with the caller's already-exclusive parity journal.
///
/// An open write session owns this handle for its whole append lifetime. Its
/// automatic terminal transition must transfer that handle here instead of
/// opening the same journal a second time and contending with itself.
#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_terminal_with_parity_journal(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    previous: &remanence_state::authority::AuthorityCommit,
    existing_intent: Option<remanence_state::authority::AuthorityFinalization>,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    mut journal: &mut dyn remanence_parity::authority::StructuralAuthority,
    rewritable: bool,
    carried: Option<remanence_parity::ParitySinkSessionState>,
) -> Result<TerminalFinalizeResult, Status> {
    checkpoint.finish_structural_scope();
    drive.clear_append_admission();
    let cfg = &cfg.terminal_config();
    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(spec.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    if let Some(fault) = tix_fault.as_ref() {
        fault
            .install(checkpoint)
            .map_err(Status::failed_precondition)?;
    }
    let mut carried = carried;
    let mut epoch_state = None;
    let resuming_existing_intent = existing_intent.is_some();
    let (prefix, intent, plan, source, prefix_snapshot, mut journal) = match existing_intent {
        Some(intent) => {
            let persisted = intent.terminal_prefix.as_ref().ok_or_else(|| {
                Status::failed_precondition("parity finalization intent has no prefix plan")
            })?;
            let prefix =
                TerminalPrefixPlan::try_from(persisted).map_err(crate::status_from_state_error)?;
            let prefix_is_durable = journal
                .prefix_is_durable(
                    &prefix
                        .media_publication()
                        .map_err(|e| Status::failed_precondition(e.to_string()))?,
                )
                .map_err(|error| {
                    Status::failed_precondition(format!(
                        "inspect terminal-prefix journal transition: {error}"
                    ))
                })?;
            let mut source = if prefix_is_durable {
                remanence_state::TerminalIndexSource::new_replay_backed_after_terminal_prefix(
                    checkpoint,
                    journal,
                    persisted,
                )
            } else {
                remanence_state::TerminalIndexSource::new_replay_backed_with_planned_terminal_prefix(
                    checkpoint,
                    journal,
                    persisted,
                )
            }
            .map_err(crate::status_from_state_error)?;
            let plan = source
                .reconstruct_final_plan(&intent)
                .map_err(crate::status_from_state_error)?;
            let (prefix_snapshot, journal) = if prefix_is_durable {
                (None, journal)
            } else {
                let snapshot = journal
                    .freeze_before_terminal_prefix(
                        &prefix
                            .media_publication()
                            .map_err(|e| Status::failed_precondition(e.to_string()))?,
                    )
                    .map_err(|error| {
                        Status::failed_precondition(format!("freeze terminal-prefix base: {error}"))
                    })?;
                epoch_state =
                    terminal_epoch_state(drive, journal, &snapshot, selected, carried.take())?;
                let reconstructed = if let Some(state) = epoch_state.take() {
                    let mut raw = DriveHandleRawSink::new(drive);
                    let sink = remanence_parity::ParitySink::from_session_state_for_terminal_close(
                        &mut raw,
                        &mut journal,
                        state,
                        &prefix,
                        TerminalPrefixReconcileEvidence::Absent,
                    )
                    .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
                    let plan = sink
                        .plan_terminal_index_close(prefix.diagnostics.clone())
                        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
                    epoch_state = Some(
                        sink.into_session_state()
                            .map_err(|e| status_from_parity_error(&e, e.to_string()))?,
                    );
                    plan
                } else {
                    remanence_parity::plan_checkpointed_terminal_index_close(
                        &snapshot,
                        prefix.diagnostics.clone(),
                    )
                    .map_err(|e| status_from_parity_error(&e, e.to_string()))?
                };
                if reconstructed != prefix {
                    return Err(Status::failed_precondition(
                        "bounded terminal close seed conflicts with persisted prefix plan",
                    ));
                }
                (Some(snapshot), journal)
            };
            (prefix, intent, plan, source, prefix_snapshot, journal)
        }
        None => {
            // Validate the checkpoint/parity bijection before the resume
            // session performs even positioning motion.
            let base_authority =
                remanence_state::TerminalIndexSource::new_replay_backed(checkpoint, journal)
                    .map_err(crate::status_from_state_error)?;
            if base_authority
                .summary()
                .scope
                .covered_prefix_tape_file_count
                == 0
            {
                return Err(Status::failed_precondition(
                    "checkpointed parity tape has no replayable committed prefix",
                ));
            }
            let snapshot = journal.freeze().map_err(|error| {
                Status::failed_precondition(format!("freeze parity journal: {error}"))
            })?;
            epoch_state =
                terminal_epoch_state(drive, journal, &snapshot, selected, carried.take())?;
            let diagnostics = remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
                .capture()
                .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
            let prefix = if let Some(state) = epoch_state.take() {
                let mut raw = DriveHandleRawSink::new(drive);
                let sink =
                    remanence_parity::ParitySink::from_session_state(&mut raw, &mut journal, state)
                        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
                let plan = sink
                    .plan_terminal_index_close(diagnostics)
                    .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
                epoch_state = Some(
                    sink.into_session_state()
                        .map_err(|e| status_from_parity_error(&e, e.to_string()))?,
                );
                plan
            } else {
                remanence_parity::plan_checkpointed_terminal_index_close(&snapshot, diagnostics)
                    .map_err(|e| status_from_parity_error(&e, e.to_string()))?
            };
            let expected_start_file = previous.next_file;
            if prefix.start_tape_file_number != expected_start_file
                || prefix.start_lba != previous.cursor.lba
            {
                return Err(Status::failed_precondition(format!(
                    "bounded terminal close seed starts at tape file {}/LBA {}, expected {expected_start_file}/{}",
                    prefix.start_tape_file_number, prefix.start_lba, previous.cursor.lba
                )));
            }
            let persisted = remanence_state::authority::AuthorityPrefixPlan::from(&prefix);
            let mut source = remanence_state::TerminalIndexSource::new_replay_backed_with_planned_terminal_prefix(
                checkpoint,
                journal,
                &persisted,
            )
            .map_err(crate::status_from_state_error)?;
            let planned = build_new_terminal_plan(
                index,
                selected,
                spec,
                previous,
                &mut source,
                TerminalPlanPosition {
                    first_tape_file_number: prefix.tail_start_tape_file_number,
                    first_start_lba: prefix.tail_start_lba,
                    terminal_prefix: Some(&prefix),
                },
            )?;
            publish_terminal_intent(index, checkpoint, spec, &planned.0)?;
            (
                prefix,
                planned.0,
                planned.1,
                source,
                Some(snapshot),
                journal,
            )
        }
    };
    // Before any terminal-prefix/component positioning or write, require the
    // checkpoint intent and sink journal to name the same canonical component
    // history. The only admissible crash skew is one exact barrier-proved sink
    // transition ahead, which is reconciled entirely in host durability first.
    let intent = if resuming_existing_intent {
        reconcile_and_authorize_parity_resume(
            index, checkpoint, spec, selected, intent, &plan, journal,
        )?
    } else {
        reconcile_terminal_component_host_authority(
            index, checkpoint, spec, intent, &plan, journal,
        )?
    };
    if let Some(request) = manual_request {
        record_manual_finalize_request(index, cfg, request)?;
    }
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        let _admission = admit_terminal_write(
            &spec.actor,
            index,
            cfg,
            drive,
            previous,
            selected,
            &plan,
            Some(&prefix),
        )?;
        drop(source);
        return complete_terminal_finalization_host_only(
            index,
            TerminalFinalizeAuditConfig::from(cfg),
            checkpoint,
            previous,
            spec,
            intent,
            &plan,
            tix_fault.as_ref(),
        );
    }

    let prefix_already_journaled = journal
        .prefix_is_durable(
            &prefix
                .media_publication()
                .map_err(|e| Status::failed_precondition(e.to_string()))?,
        )
        .map_err(|error| {
            Status::failed_precondition(format!(
                "inspect terminal-prefix journal transition: {error}"
            ))
        })?;

    // The immutable intent precedes the terminal sidecar and final ParityMap.
    // A durable prefix skips this media boundary on recovery.
    if let Some(fault) = tix_fault.as_ref().filter(|_| !prefix_already_journaled) {
        let position = drive.position().map_err(|error| {
            Status::unavailable(format!(
                "read position before terminal-prefix fault boundary: {error}"
            ))
        })?;
        fault
            .abort_prefix_if_matches(
                crate::terminal_fault::TerminalFaultCut::BeforeTerminalPrefix,
                Some(PhysicalPositionHint {
                    partition: position.partition,
                    lba: position.lba,
                }),
                &prefix,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }

    let prefix_evidence = {
        let mut raw = DriveHandleRawSource::new(drive);
        reconcile_terminal_prefix(
            &mut raw,
            &prefix,
            &spec.tape_uuid,
            spec.block_size,
            rewritable,
        )
    };
    if prefix_already_journaled {
        if prefix_evidence != TerminalPrefixReconcileEvidence::Complete {
            persist_terminal_recovery_required(index, checkpoint, spec)?;
            return Err(Status::failed_precondition(format!(
                "parity journal records a complete terminal prefix, but media reconciliation found {prefix_evidence:?}"
            )));
        }
    } else {
        let snapshot = prefix_snapshot.as_ref().ok_or_else(|| {
            Status::internal("bounded parity prefix authority snapshot is missing")
        })?;
        // A complete, unpublished prefix is adopted without rewriting, but its
        // zero-count barrier is still a media write at `tail_start_lba`, where
        // reconciliation left the cursor. The admission ends with this block.
        let _admission = if matches!(
            prefix_evidence,
            TerminalPrefixReconcileEvidence::Absent
                | TerminalPrefixReconcileEvidence::TornRewritable
                | TerminalPrefixReconcileEvidence::Complete
        ) {
            Some(admit_terminal_write(
                &spec.actor,
                index,
                cfg,
                drive,
                previous,
                selected,
                &plan,
                Some(&prefix),
            )?)
        } else {
            None
        };
        let dispatches_before = drive.media_write_dispatch_count();
        let mut raw_write_attempted = false;
        let close_result = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut tracked = super::checkpoint::ActivityTrackingRawTapeSink::new(
                &mut raw,
                &mut raw_write_attempted,
            );
            let mut faulting = crate::terminal_fault::TerminalPrefixFaultSink::new(
                &mut tracked,
                tix_fault.as_ref(),
                &prefix,
            );
            if let Some(state) = epoch_state.take() {
                remanence_parity::ParitySink::from_session_state_for_terminal_close(
                    &mut faulting,
                    &mut journal,
                    state,
                    &prefix,
                    prefix_evidence,
                )
                .and_then(|sink| sink.close_for_terminal_index(&prefix, prefix_evidence))
                .map(|result| {
                    remanence_parity::CheckpointedTerminalPrefixCloseResult {
                        publication: result.publication,
                        used_tape_blocks: result.used_tape_blocks,
                    }
                })
            } else {
                remanence_parity::close_checkpointed_terminal_index_prefix(
                    &mut faulting,
                    &mut journal,
                    snapshot,
                    &prefix,
                    prefix_evidence,
                )
            }
        };
        let raw_write_attempted = super::checkpoint::raw_write_reached_drive(
            drive,
            dispatches_before,
            raw_write_attempted,
        );
        let prefix_result = match close_result {
            Ok(result) => result,
            Err(error) => {
                return Err(terminal_prefix_write_failure(
                    index,
                    cfg,
                    checkpoint,
                    spec,
                    selected,
                    raw_write_attempted,
                    &error,
                ));
            }
        };
        checkpoint
            .publish_terminal_prefix(
                &prefix
                    .media_publication()
                    .map_err(|e| Status::failed_precondition(e.to_string()))?,
            )
            .map_err(|error| {
                if matches!(error, remanence_parity::JournalError::AfterDurable(_)) {
                    return Status::internal(format!(
                        "terminal prefix is durable; retry after tape-log replay: {error}"
                    ));
                }
                terminal_prefix_write_failure(
                    index,
                    cfg,
                    checkpoint,
                    spec,
                    selected,
                    true,
                    &error.into(),
                )
            })?;
        index
            .project_tape_log(
                &checkpoint
                    .freeze()
                    .map_err(crate::status_from_state_error)?,
            )
            .map_err(crate::status_from_state_error)?;
        if let Some(fault) = tix_fault.as_ref() {
            fault
                .abort_prefix_if_matches(
                    crate::terminal_fault::TerminalFaultCut::AfterTerminalPrefix,
                    Some(PhysicalPositionHint::new(prefix_result.used_tape_blocks)),
                    &prefix,
                )
                .map_err(|error| {
                    Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
                })?;
        }
    }

    finish_terminal_tail(
        index,
        cfg,
        drive,
        checkpoint,
        previous,
        spec,
        selected,
        manual_request,
        intent,
        plan,
        source,
        Some(journal),
        rewritable,
        tix_fault.as_ref(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn complete_terminal_finalization_host_only(
    index: &mut CatalogIndex,
    audit: TerminalFinalizeAuditConfig<'_>,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    previous: &remanence_state::authority::AuthorityCommit,
    spec: &TerminalFinalizeSpec,
    intent: remanence_state::authority::AuthorityFinalization,
    plan: &TerminalTripleWritePlan,
    tix_fault: Option<&crate::terminal_fault::TerminalFaultPlan>,
) -> Result<TerminalFinalizeResult, Status> {
    if let Some(fault) = tix_fault {
        fault
            .install(checkpoint)
            .map_err(Status::failed_precondition)?;
    }
    if intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        return Err(Status::internal(
            "host-only terminal completion requires durable replica C progress",
        ));
    }
    // Keep a recovery-required companion fail-closed until the sealed
    // checkpoint itself is fsynced. The checkpoint transition validates a
    // normalized completion copy and retires the companion only afterwards.
    let mut completed_intent = intent.clone();
    completed_intent.recovery_required = false;
    let replica_c = plan.edition.descriptor.terminal_layout.components[4];
    let final_bundle = remanence_parity::terminal_component_publication(plan, replica_c)
        .map_err(|error| Status::internal(format!("build final C authority: {error}")))?;
    let final_record =
        remanence_state::authority::complete_commit(remanence_state::authority::AuthorityCommit {
            ordinal: previous
                .ordinal
                .checked_add(1)
                .ok_or_else(|| Status::internal("terminal checkpoint ordinal overflows u64"))?,
            object_count: previous.object_count,
            tape_uuid: spec.tape_uuid,
            operation_id: intent.edition_id,
            next_file: replica_c
                .planned_tape_file_number
                .checked_add(1)
                .ok_or_else(|| Status::internal("terminal next tape-file number overflows u64"))?,
            block_size: spec.block_size,
            payloads: Vec::new(),
            scheme: previous.scheme.clone(),
            object_files: Vec::new(),
            barrier: Some(final_bundle),
            finalization: Some(completed_intent),
            sealed: true,
            seal_due: None,
            cursor: remanence_parity::CommitCursor {
                partition: plan.edition.descriptor.terminal_layout.partition,
                lba: plan.edition.descriptor.terminal_layout.expected_eod_lba,
            },
            protected: 0,
            total: 0,
            check: None,
        });
    checkpoint
        .publish_sealed_reports(std::slice::from_ref(&final_record), || Ok(()))
        .map_err(crate::status_from_state_error)?;
    if let Some(fault) = tix_fault {
        fault
            .abort_if_matches(
                "final_projection",
                crate::terminal_fault::TerminalFaultCut::BeforeFinalSqliteProjection,
                Some(PhysicalPositionHint {
                    partition: final_record.cursor.partition,
                    lba: final_record.cursor.lba,
                }),
                None,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }
    index
        .project_tape_log(
            &checkpoint
                .freeze()
                .map_err(crate::status_from_state_error)?,
        )
        .map_err(crate::status_from_state_error)?;
    if let Some(fault) = tix_fault {
        fault
            .abort_if_matches(
                "final_projection",
                crate::terminal_fault::TerminalFaultCut::AfterFinalSqliteProjection,
                Some(PhysicalPositionHint {
                    partition: final_record.cursor.partition,
                    lba: final_record.cursor.lba,
                }),
                None,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }
    ensure_tape_sealed_audit(
        index,
        audit.audit_dir,
        audit.audit_append_lock,
        spec.tape_uuid,
    )?;
    let completion = final_record.finalization.as_ref().ok_or_else(|| {
        Status::internal("sealed terminal checkpoint omitted its completion authority")
    })?;
    ensure_manual_finalize_finished_audit(
        index,
        audit.audit_dir,
        audit.audit_append_lock,
        spec.tape_uuid,
        completion,
    )?;
    let projection = index
        .terminal_finalization(&spec.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::internal("terminal projection disappeared after completion"))?;
    Ok(TerminalFinalizeResult {
        projection,
        final_record: Some(final_record),
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finish_terminal_tail(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    previous: &remanence_state::authority::AuthorityCommit,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    intent: remanence_state::authority::AuthorityFinalization,
    plan: TerminalTripleWritePlan,
    mut source: remanence_state::TerminalIndexSource,
    _parity_journal: Option<&mut dyn remanence_parity::authority::StructuralAuthority>,
    rewritable: bool,
    tix_fault: Option<&crate::terminal_fault::TerminalFaultPlan>,
) -> Result<TerminalFinalizeResult, Status> {
    if let Some(fault) = tix_fault {
        fault
            .install(checkpoint)
            .map_err(Status::failed_precondition)?;
    }
    let cfg = &cfg.terminal_config();
    let prefix_plan = intent
        .terminal_prefix
        .as_ref()
        .map(TerminalPrefixPlan::try_from)
        .transpose()
        .map_err(crate::status_from_state_error)?;
    let mut authority = TerminalTailCatalogAuthority {
        checkpoint,
        index,
        spec,
        intent,
        tix_fault,
        reconciliation: None,
        durable_projection_pending: false,
    };
    let mut admission = None;
    loop {
        let progress = parity_progress_from_state(authority.intent.progress);
        let Some(component_index) = progress.next_component_index() else {
            break;
        };
        let component = plan.edition.descriptor.terminal_layout.components[component_index];
        let evidence = {
            let mut raw = DriveHandleRawSource::new(drive);
            reconcile_terminal_tail_next(&mut raw, &plan, progress, rewritable)
        };
        // A complete, unpublished component is adopted without rewriting, but
        // its zero-count barrier is still a media write after its filemark,
        // where reconciliation left the cursor.
        if matches!(
            evidence,
            remanence_parity::TerminalComponentReconcileEvidence::Absent
                | remanence_parity::TerminalComponentReconcileEvidence::TornRewritable
                | remanence_parity::TerminalComponentReconcileEvidence::Complete
        ) && admission.is_none()
        {
            admission = Some(admit_terminal_write(
                &authority.spec.actor,
                authority.index,
                cfg,
                drive,
                previous,
                selected,
                &plan,
                prefix_plan.as_ref(),
            )?);
        }
        authority.set_reconciliation(progress, component, evidence);
        let dispatches_before = drive.media_write_dispatch_count();
        let step_result = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut faulting = crate::terminal_fault::TerminalFaultSink::new(
                &mut raw,
                authority.tix_fault,
                component,
            );
            remanence_parity::write_terminal_tail_step(
                &mut faulting,
                &mut source,
                &mut authority,
                &plan,
            )
        };
        let step = match step_result {
            Ok(step) => step,
            Err(error) => {
                if authority.durable_projection_pending {
                    return Err(Status::internal(format!(
                        "terminal component is durable; retry after tape-log replay: {error}"
                    )));
                }
                let raw_write_reached_drive =
                    super::checkpoint::raw_write_reached_drive(drive, dispatches_before, true);
                return Err(terminal_tail_write_failure(
                    authority.index,
                    cfg,
                    authority.checkpoint,
                    authority.spec,
                    selected,
                    manual_request,
                    raw_write_reached_drive,
                    &error,
                ));
            }
        };
        match step {
            TerminalTailStepOutcome::Advanced(_) => {}
            TerminalTailStepOutcome::AlreadyComplete => break,
            TerminalTailStepOutcome::RecoveryRequired {
                progress,
                component,
                evidence,
            } => {
                let state_progress = state_progress_from_parity(progress);
                authority.intent = authority
                    .checkpoint
                    .require_finalization_recovery()
                    .map_err(crate::status_from_state_error)?;
                if authority.intent.progress != state_progress {
                    return Err(Status::internal(
                        "terminal recovery evidence disagrees with durable checkpoint progress",
                    ));
                }
                let outcome = terminal_reconciliation_outcome(state_progress, evidence);
                authority
                    .index
                    .project_terminal_finalization(
                        authority.projection_input(state_progress, outcome),
                    )
                    .map_err(crate::status_from_state_error)?;
                record_terminal_finalize_event(
                    authority.index,
                    cfg,
                    manual_request,
                    AuditEvent::CompletionUnknown,
                    state_progress,
                    Some((
                        "recovery_detail",
                        format!(
                            "component {:?}/{} at file {} requires recovery: {evidence:?}",
                            component.kind, component.ordinal, component.planned_tape_file_number
                        ),
                    )),
                )?;
                let projection = authority
                    .index
                    .terminal_finalization(&spec.tape_uuid)
                    .map_err(crate::status_from_state_error)?
                    .ok_or_else(|| Status::internal("recovery projection disappeared"))?;
                return Ok(TerminalFinalizeResult {
                    projection,
                    final_record: None,
                });
            }
        }
    }

    if drive.check_append_admission().is_err() {
        let _admission = admit_terminal_write(
            &authority.spec.actor,
            authority.index,
            cfg,
            drive,
            previous,
            selected,
            &plan,
            prefix_plan.as_ref(),
        )?;
    }
    let completed_intent = authority
        .checkpoint
        .pending_finalization()
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::internal("completed terminal tail lost its durable intent"))?;
    if completed_intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        return Err(Status::internal(
            "terminal writer returned complete before replica C became durable",
        ));
    }
    drop(authority);
    complete_terminal_finalization_host_only(
        index,
        TerminalFinalizeAuditConfig::from(cfg),
        checkpoint,
        previous,
        spec,
        completed_intent,
        &plan,
        tix_fault,
    )
}

pub(super) fn status_from_terminal_tail_error(error: &TerminalTailWriteError) -> Status {
    let message = format!("terminal tape write failed: {error}");
    match error {
        TerminalTailWriteError::Media(parity) => status_from_parity_error(parity, message),
        TerminalTailWriteError::Layout(_)
        | TerminalTailWriteError::Replica(_)
        | TerminalTailWriteError::Separation(_)
        | TerminalTailWriteError::PlanMismatch(_) => Status::failed_precondition(message),
        TerminalTailWriteError::Authority(_)
        | TerminalTailWriteError::PositionMismatch { .. }
        | TerminalTailWriteError::ShortWrite { .. }
        | TerminalTailWriteError::EndOfMedium => Status::unavailable(message),
    }
}

pub(super) fn validate_manual_finalize_owned_request(
    index: &CatalogIndex,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let assignment = index
        .get_tape_assignment_snapshot(&request.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape disappeared while finalization owner was held"))?;
    if assignment.assignment_generation != request.assignment_generation
        || assignment.pool_id != request.expected_pool_id
    {
        return Err(Status::failed_precondition(format!(
            "tape assignment changed before terminal finalization: expected generation {} pool {:?}, found generation {} pool {:?}",
            request.assignment_generation,
            request.expected_pool_id,
            assignment.assignment_generation,
            assignment.pool_id
        )));
    }
    match (&request.expected_pool_id, &request.pool_config) {
        (Some(expected), Some(pool_config)) if pool_config.id == *expected => {}
        (None, None) => {}
        _ => {
            return Err(Status::failed_precondition(
                "manual terminal finalization pool policy does not match the guarded assignment",
            ));
        }
    }

    if let Some(scope) = index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
    {
        if scope.request_fingerprint.as_slice() != request.request_fingerprint {
            return Err(Status::already_exists(
                "FinalizeTape idempotency key is already bound to a different request",
            ));
        }
        if scope.operation_id != request.candidate_operation_id {
            return Err(Status::failed_precondition(format!(
                "FinalizeTape owner received operation {}, but durable idempotency authority names {}",
                request.candidate_operation_id, scope.operation_id
            )));
        }
    }
    Ok(())
}

pub(super) fn require_manual_finalize_preflight_binding(
    index: &CatalogIndex,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let scope = index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| {
            Status::failed_precondition(
                "FinalizeTape drive dispatch is missing its durable preflight binding",
            )
        })?;
    if scope.operation_id != request.candidate_operation_id
        || scope.request_fingerprint.as_slice() != request.request_fingerprint
    {
        return Err(Status::already_exists(
            "FinalizeTape drive dispatch does not match its durable preflight binding",
        ));
    }
    Ok(())
}

pub(super) fn record_manual_finalize_request(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let cfg = &cfg.terminal_config();
    record_manual_finalize_request_with(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        request,
    )
}

pub(super) fn record_manual_finalize_request_with(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    _audit_fsync: bool,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(request.tape_uuid.to_vec()),
        ),
        (
            "actor_fingerprint".to_string(),
            CborValue::Text(request.actor_fingerprint.clone()),
        ),
        (
            "request_fingerprint".to_string(),
            CborValue::Bytes(request.request_fingerprint.to_vec()),
        ),
        (
            "assignment_generation".to_string(),
            CborValue::Integer(request.assignment_generation.into()),
        ),
        (
            "reason".to_string(),
            CborValue::Text(request.reason.clone()),
        ),
    ]);
    if let Some(pool_id) = request.expected_pool_id.as_ref() {
        detail.insert(
            "expected_pool_id".to_string(),
            CborValue::Text(pool_id.clone()),
        );
    }
    detail.insert(
        "operation_kind".to_string(),
        CborValue::Text(FINALIZE_TAPE_OPERATION_KIND.to_string()),
    );
    let subject = AuditSubject {
        kind: "tape".to_string(),
        id: Some(Uuid::from_bytes(request.tape_uuid).to_string()),
    };
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("audit append lock poisoned"))?;
    std::fs::create_dir_all(audit_dir).map_err(|error| {
        Status::internal(format!(
            "create audit directory {}: {error}",
            audit_dir.display()
        ))
    })?;
    let mut exact = None;
    let mut exact_count = 0usize;
    let mut conflict = None;
    FileAuditLog::replay_incremental(audit_dir, |record| {
        if record.operation_id != Some(request.candidate_operation_id)
            || record.event != AuditEvent::RequestReceived
        {
            return ControlFlow::Continue(());
        }
        if record.source_layer == SourceLayer::Layer5
            && record.session_id.is_none()
            && record.idempotency_key == Some(request.idempotency_key)
            && record.subject == subject
            && record.detail == detail
        {
            exact_count += 1;
            exact.get_or_insert(record);
        } else {
            conflict.get_or_insert(record);
        }
        ControlFlow::Continue(())
    })
    .map_err(crate::status_from_state_error)?;
    if conflict.is_some() {
        return Err(Status::already_exists(
            "FinalizeTape operation has a conflicting durable RequestReceived audit record",
        ));
    }
    if exact_count > 1 {
        return Err(Status::failed_precondition(format!(
            "FinalizeTape operation has {exact_count} duplicate durable RequestReceived audit records"
        )));
    }
    let record = if let Some(record) = exact {
        // Admission replay must not overwrite the current attempt outcome.
        if index
            .get_operation(&request.candidate_operation_id.to_string())
            .map_err(crate::status_from_state_error)?
            .is_some()
        {
            return Ok(());
        }
        record
    } else {
        let mut audit =
            FileAuditLog::open(audit_dir, true).map_err(crate::status_from_state_error)?;
        audit
            .append_and_return_record(AuditEventRecord {
                actor: request.actor.clone(),
                source_layer: SourceLayer::Layer5,
                operation_id: Some(request.candidate_operation_id),
                session_id: None,
                idempotency_key: Some(request.idempotency_key),
                event: AuditEvent::RequestReceived,
                subject,
                detail,
            })
            .map_err(crate::status_from_state_error)?
            .1
    };
    index
        .project_audit_record(&record)
        .map_err(crate::status_from_state_error)
}

pub(super) fn record_terminal_finalize_event(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    event: AuditEvent,
    progress: remanence_state::TerminalFinalizationProgress,
    detail_text: Option<(&str, String)>,
) -> Result<(), Status> {
    let cfg = &cfg.terminal_config();
    record_terminal_finalize_event_with(
        index,
        TerminalFinalizeAuditConfig::from(cfg),
        manual_request,
        event,
        progress,
        detail_text,
    )
}

pub(super) fn record_terminal_finalize_event_with(
    index: &mut CatalogIndex,
    audit: TerminalFinalizeAuditConfig<'_>,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    event: AuditEvent,
    progress: remanence_state::TerminalFinalizationProgress,
    detail_text: Option<(&str, String)>,
) -> Result<(), Status> {
    let Some(request) = manual_request else {
        return Ok(());
    };
    record_manual_finalize_event_with(index, audit, request, event, progress, detail_text)
}

pub(super) fn record_manual_finalize_event_with(
    index: &mut CatalogIndex,
    audit: TerminalFinalizeAuditConfig<'_>,
    request: &ManualFinalizeTapeActorRequest,
    event: AuditEvent,
    progress: remanence_state::TerminalFinalizationProgress,
    detail_text: Option<(&str, String)>,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(request.tape_uuid.to_vec()),
        ),
        (
            "actor_fingerprint".to_string(),
            CborValue::Text(request.actor_fingerprint.clone()),
        ),
        (
            "finalization_progress".to_string(),
            CborValue::Text(manual_finalize_progress_name(progress).to_string()),
        ),
    ]);
    if let Some((key, value)) = detail_text {
        detail.insert(key.to_string(), CborValue::Text(value));
    }
    append_operation_audit(
        index,
        audit.audit_dir,
        audit.audit_fsync,
        audit.audit_append_lock,
        OperationAuditInput {
            actor: AuditActor::System,
            operation_id: request.candidate_operation_id,
            operation_kind: FINALIZE_TAPE_OPERATION_KIND,
            event,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(request.tape_uuid).to_string()),
            idempotency_key: Some(request.idempotency_key),
            detail,
        },
    )
}

const fn manual_finalize_progress_name(
    progress: remanence_state::TerminalFinalizationProgress,
) -> &'static str {
    use remanence_state::TerminalFinalizationProgress as Progress;
    match progress {
        Progress::BeforeReplicaA => "before_replica_a",
        Progress::AfterReplicaA => "after_replica_a",
        Progress::AfterSeparationAb => "after_separation_ab",
        Progress::AfterReplicaB => "after_replica_b",
        Progress::AfterSeparationBc => "after_separation_bc",
        Progress::AfterReplicaC => "after_replica_c",
    }
}

/// Preserve the terminal reconciler's position while checking the entire suffix
/// for complete foreign objects before its first destructive operation.
#[allow(clippy::too_many_arguments)]
fn admit_terminal_write(
    actor: &AuditActor,
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    drive: &mut DriveHandle,
    previous: &remanence_state::authority::AuthorityCommit,
    selected: &SelectedTape,
    plan: &TerminalTripleWritePlan,
    prefix: Option<&TerminalPrefixPlan>,
) -> Result<crate::append_admission::AppendAdmission, Status> {
    let mut ctx = crate::append_admission::context(
        selected.tape_uuid,
        selected.block_size,
        previous.cursor.partition,
        previous.cursor.lba,
    );
    ctx.terminal_prefix = prefix.cloned();
    ctx.write_position = drive
        .position()
        .map_err(|e| Status::unavailable(e.to_string()))?;
    crate::append_admission::admit_append_boundary(
        drive,
        crate::append_admission::AppendAuthority::Terminal { plan },
        ctx,
    )
    .map_err(|error| {
        crate::append_admission::daemon_admission_error(
            actor.clone(),
            index,
            &cfg.terminal_config(),
            selected.tape_uuid,
            error,
        )
    })
}

/// Rebuild only the open epoch needed by the terminal funnel, or take its live state.
fn terminal_epoch_state(
    drive: &mut DriveHandle,
    journal: &mut dyn remanence_parity::authority::StructuralAuthority,
    snapshot: &remanence_parity::FrozenFileRows,
    selected: &SelectedTape,
    carried: Option<remanence_parity::ParitySinkSessionState>,
) -> Result<Option<remanence_parity::ParitySinkSessionState>, Status> {
    use remanence_parity::{checked_bounded_resume_summary, BoundedResumeWriterSeed, ParitySink};
    let summary = checked_bounded_resume_summary(snapshot)
        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    if summary.highest_protected_ordinal == summary.total_committed_ordinals {
        return Ok(None);
    }
    if let Some(state) = carried.filter(|s| s.matches_committed_summary(&summary)) {
        remanence_parity::RawTapeSource::locate_physical(
            &mut DriveHandleRawSource::new(drive),
            summary.append_position,
        )
        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        return Ok(Some(state));
    }
    let scheme = summary.scheme().clone();
    let rebuilt = remanence_parity::rebuild_open_epoch_from_bounded_summary(
        &mut DriveHandleRawSource::new(drive),
        &summary,
        &scheme,
        selected.tape_uuid,
        selected.block_size,
    )
    .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    let resume_result = rebuilt
        .plan
        .complete(Vec::new())
        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    let mut raw = DriveHandleRawSink::new(drive);
    let sink = ParitySink::new_sidecar_only_from_bounded_resume(
        &mut raw,
        journal,
        scheme,
        selected.tape_uuid,
        selected.block_size,
        BoundedResumeWriterSeed {
            committed_prefix_snapshot: snapshot
                .try_clone()
                .map_err(|e| Status::internal(e.to_string()))?,
            committed_prefix_summary: summary,
            resume_result: &resume_result,
            live_epoch: rebuilt.live_epoch,
        },
        remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
    )
    .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    sink.into_session_state()
        .map(Some)
        .map_err(|e| status_from_parity_error(&e, e.to_string()))
}

/// Plan an open epoch's terminal sidecar without writing media or either journal.
/// Loading and calibration use the finalization drive preparation policy.
pub(crate) fn plan_unretained_epoch_finalization(
    index: &mut CatalogIndex,
    cfg: &super::terminal_types::TerminalWriteConfig,
    drive: &mut DriveHandle,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<
    (
        TerminalPrefixPlan,
        Option<remanence_parity::ParitySinkSessionState>,
    ),
    Status,
> {
    validate_manual_finalize_owned_request(index, request)?;
    let tape = index
        .get_tape(&request.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    session_open_reject_tape_io_fences(
        index,
        &request.tape_uuid,
        tape.voltag.as_deref(),
        "plan finalization",
    )?;
    let checkpoint = remanence_state::TapeAuthority::open(
        &cfg.checkpoint_journal_dir,
        request.tape_uuid,
        index.path(),
    )
    .map_err(crate::status_from_state_error)?;
    let lease = checkpoint
        .acquire_exclusive()
        .map_err(crate::status_from_state_error)?;
    project_checkpoint_authority_bounded(index, &lease).map_err(crate::status_from_state_error)?;
    let scheme = match &request.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        _ => return Err(Status::internal("open epoch requires parity")),
    };
    let mut journal = lease
        .open_structural(request.block_size, scheme)
        .map_err(|e| Status::failed_precondition(format!("open parity journal: {e}")))?;
    remanence_state::TerminalIndexSource::new_replay_backed(&lease, &journal)
        .map_err(crate::status_from_state_error)?;
    let snapshot = journal
        .freeze()
        .map_err(|e| Status::failed_precondition(format!("freeze parity journal: {e}")))?;
    prepare_drive_for_write(
        drive,
        &request.tape_uuid,
        request.block_size,
        request.candidate_operation_id,
        WriteMediaPolicy::TerminalAppend,
    )?;
    let selected = SelectedTape {
        standalone_endpoint: cfg.mounted_endpoint.clone().or_else(|| {
            drive
                .endpoint()
                .durable_key()
                .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. }))
                .cloned()
        }),
        tape_uuid: request.tape_uuid,
        block_size: request.block_size,
        parity_config: request.parity_config.clone(),
        pool_id: request
            .expected_pool_id
            .clone()
            .unwrap_or_else(|| "(unpooled)".into()),
    };
    let state = terminal_epoch_state(drive, &mut journal, &snapshot, &selected, None)?;
    let diagnostics = remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
        .capture()
        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    if let Some(state) = state {
        let mut raw = DriveHandleRawSink::new(drive);
        let sink = remanence_parity::ParitySink::from_session_state(&mut raw, &mut journal, state)
            .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        let plan = sink
            .plan_terminal_index_close(diagnostics)
            .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        let state = sink
            .into_session_state()
            .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        Ok((plan, Some(state)))
    } else {
        remanence_parity::plan_checkpointed_terminal_index_close(&snapshot, diagnostics)
            .map(|plan| (plan, None))
            .map_err(|e| status_from_parity_error(&e, e.to_string()))
    }
}

/// Plan from a seated accumulator when its ownership and journal keys match.
pub(super) fn plan_mounted_epoch_finalization(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    bay: u16,
    request: &ManualFinalizeTapeActorRequest,
    needs_drive_load: bool,
    retained: &mut Option<super::checkpoint::RetainedParityEpoch>,
) -> Result<TerminalPrefixPlan, Status> {
    validate_manual_finalize_owned_request(index, request)?;
    let tape = index
        .get_tape(&request.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    session_open_reject_tape_io_fences(
        index,
        &request.tape_uuid,
        tape.voltag.as_deref(),
        "finalize tape",
    )?;
    prepare_drive_for_write(
        drive,
        &request.tape_uuid,
        request.block_size,
        request.candidate_operation_id,
        WriteMediaPolicy::TerminalAppend,
    )?;
    if needs_drive_load {
        run_load_calibration_harvest(
            index,
            drive,
            cfg,
            &request.tape_uuid,
            tape.voltag.as_deref(),
        );
    }
    let checkpoint = remanence_state::TapeAuthority::open(
        &cfg.checkpoint_journal_dir,
        request.tape_uuid,
        index.path(),
    )
    .map_err(crate::status_from_state_error)?;
    let mut lease = checkpoint
        .acquire_exclusive()
        .map_err(crate::status_from_state_error)?;
    let records = project_checkpoint_authority_bounded(index, &lease)
        .map_err(crate::status_from_state_error)?;
    let selected = SelectedTape {
        standalone_endpoint: None,
        tape_uuid: request.tape_uuid,
        block_size: request.block_size,
        parity_config: request.parity_config.clone(),
        pool_id: request
            .expected_pool_id
            .clone()
            .unwrap_or_else(|| "(unpooled)".into()),
    };
    let authority =
        super::checkpoint::validate_parity_actor_authority(cfg, &selected, &mut lease, &records)?;
    let mut journal = lease
        .structural_mut()
        .ok_or_else(|| Status::internal("missing validated structural capability"))?;
    let epoch = retained.take().filter(|e| {
        e.selected.tape_uuid == selected.tape_uuid
            && e.identity_generation == drive.identity_generation()
            && e.state.matches_committed_summary(&authority.summary)
            && cfg.lifecycle.as_ref().is_some_and(|l| {
                l.parked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .by_drive
                    .get(&crate::drive_pool::DriveKey::new(
                        &cfg.library().serial,
                        bay,
                    ))
                    .map_or(e.parked_generation == 0, |p| {
                        p.generation == e.parked_generation
                    })
            })
    });
    let (carried, owner) = if let Some(e) = epoch {
        (Some(e.state), (e.permit, e.parked_generation))
    } else {
        let bytes = super::checkpoint::retained_epoch_bytes(&authority.scheme, selected.block_size);
        let permit = cfg
            .io_memory
            .try_reserve_epoch(
                bytes,
                format!(
                    "tape {} drive {:?}",
                    Uuid::from_bytes(selected.tape_uuid),
                    drive.endpoint().durable_key()
                ),
            )
            .ok_or_else(|| {
                Status::resource_exhausted("terminal planning cannot retain its rebuilt epoch")
            })?;
        let generation = cfg
            .lifecycle
            .as_ref()
            .and_then(|l| {
                l.parked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .by_drive
                    .get(&crate::drive_pool::DriveKey::new(
                        &cfg.library().serial,
                        bay,
                    ))
                    .map(|p| p.generation)
            })
            .unwrap_or(0);
        (None, (permit, generation))
    };
    let state = terminal_epoch_state(drive, journal, &authority.snapshot, &selected, carried)?;
    let diagnostics = remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
        .capture()
        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    if let Some(state) = state {
        let mut raw = DriveHandleRawSink::new(drive);
        let sink = remanence_parity::ParitySink::from_session_state(&mut raw, &mut journal, state)
            .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        let plan = sink
            .plan_terminal_index_close(diagnostics)
            .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        let state = sink
            .into_session_state()
            .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
        let (permit, parked_generation) = owner;
        {
            *retained = Some(super::checkpoint::RetainedParityEpoch {
                state,
                permit,
                selected,
                parked_generation,
                identity_generation: drive.identity_generation(),
                head_at_eod: false,
            });
        }
        Ok(plan)
    } else {
        remanence_parity::plan_checkpointed_terminal_index_close(&authority.snapshot, diagnostics)
            .map_err(|e| status_from_parity_error(&e, e.to_string()))
    }
}

/// Fence a dirty terminal prefix before attempting fallible recovery persistence.
pub(super) fn terminal_prefix_write_failure(
    index: &mut CatalogIndex,
    cfg: &impl TerminalConfig,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    raw_write_attempted: bool,
    error: &remanence_parity::ParityError,
) -> Status {
    let status = status_from_parity_error(error, error.to_string());
    let status = if raw_write_attempted {
        fence_failed_parity_raw_write(
            index,
            cfg,
            selected,
            "terminal_prefix",
            None,
            None,
            &error.to_string(),
            status,
        )
        .0
    } else {
        status
    };
    if let Err(persist_error) = persist_terminal_recovery_required(index, checkpoint, spec) {
        return Status::internal(format!(
            "{}; persist terminal recovery: {}",
            status.message(),
            persist_error.message()
        ));
    }
    status
}

/// Fence a failed terminal tail before recovery marking, projection, or audit can fail.
/// A failure that dispatched no media-modifying command leaves the medium
/// unchanged and is a plain error.
#[allow(clippy::too_many_arguments)]
pub(super) fn terminal_tail_write_failure(
    index: &mut CatalogIndex,
    cfg: &impl TerminalConfig,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    raw_write_reached_drive: bool,
    error: &TerminalTailWriteError,
) -> Status {
    let detail = error.to_string();
    let status = status_from_terminal_tail_error(error);
    let status = if raw_write_reached_drive {
        fence_failed_parity_raw_write(
            index,
            cfg,
            selected,
            "terminal_tail",
            None,
            None,
            &detail,
            status,
        )
        .0
    } else {
        status
    };
    let recovery = persist_terminal_recovery_required(index, checkpoint, spec).and_then(|intent| {
        record_terminal_finalize_event(
            index,
            cfg,
            manual_request,
            AuditEvent::CompletionUnknown,
            intent.progress,
            Some(("recovery_detail", detail)),
        )
    });
    if let Err(error) = recovery {
        return Status::internal(format!(
            "{}; persist terminal recovery: {}",
            status.message(),
            error.message()
        ));
    }
    status
}

#[cfg(test)]
thread_local! {
    /// Pause real finalization preflight while it owns the checkpoint lease.
    pub(super) static PREFLIGHT_INTENT_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
    pub(super) static PREFLIGHT_LEASE_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = Default::default();
}

/// Install a one-shot preflight pause on the current test task's thread.
#[cfg(test)]
pub(crate) fn set_manual_preflight_hook(after_intent: bool, hook: impl FnOnce() + 'static) {
    let slot = if after_intent {
        &PREFLIGHT_INTENT_HOOK
    } else {
        &PREFLIGHT_LEASE_HOOK
    };
    slot.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}
