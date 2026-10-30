//! Transitional checkpointed direct writer and terminal close.
//! Shared primitives are re-exported here until the caller migration completes.

use super::capacity::{
    ensure_empty_checkpoint_matches_catalog_freshness, ensure_request_pool_matches_config,
    ensure_selected_tape_accepts_session_write, ensure_selected_tape_binding,
    first_batched_append_context, selected_tape_seal_reason_at_barrier, uuid_text,
};
use super::model::{
    require_rewritable_object_media, CapacityTrackingRawTapeSink,
    DirectSequentialTerminalAuthority, PoolWriteError, PoolWriteResources, PoolWriteResult,
    SelectedTape, TapePositionAfterWrite, TapeSealReason, WriteObjectToPoolRequest,
};
use super::no_parity::{fence_after_terminal_motion, maybe_replay_pool_write};
use remanence_library::{BlockSink, DriveHandle, DriveHandleSink, DriveHandleSource, TapeIoError};
use remanence_parity::{
    checked_bounded_resume_summary, resume_record_result, BlockSinkRawTapeSink,
    BoundedResumeWriterSeed, FileTapeFileJournal, ParityConfig, ParityError, ParitySink,
    ParitySinkSessionState, TerminalPrefixPlan, TerminalPrefixReconcileEvidence,
    TerminalTailProgress, TerminalTailRunOutcome, TerminalTripleWritePlan,
};
use remanence_state::{
    CatalogIndex, StateError, StateHandle, TapePoolConfig, TerminalFinalizationOutcome,
    TerminalFinalizationProjectionInput,
};
use std::{cell::Cell, path::Path, sync::Arc};
use uuid::Uuid;

pub(super) fn terminal_trigger_for_seal_reason(
    reason: TapeSealReason,
) -> Result<remanence_state::TerminalFinalizationTrigger, PoolWriteError> {
    match reason {
        TapeSealReason::ReachedLowWatermark => {
            Ok(remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark)
        }
        TapeSealReason::HardwareEarlyWarning => {
            Ok(remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning)
        }
        TapeSealReason::PoolCloseOut => {
            Ok(remanence_state::TerminalFinalizationTrigger::PoolCloseOut)
        }
        TapeSealReason::NoPendingObjectFits => {
            Ok(remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits)
        }
        TapeSealReason::OperatorCloseOut => Err(PoolWriteError::InvalidInput(
            "direct automatic terminal finalization cannot invent manual operator identity"
                .to_string(),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_direct_terminal_plan(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    previous: &remanence_state::CheckpointJournalRecord,
    source: &mut remanence_state::CheckpointTerminalIndexRecordSource<'_>,
    first_tape_file_number: u64,
    first_start_lba: u64,
    terminal_prefix: Option<&TerminalPrefixPlan>,
    trigger: remanence_state::TerminalFinalizationTrigger,
) -> Result<
    (
        remanence_state::TerminalFinalizationIntent,
        TerminalTripleWritePlan,
    ),
    PoolWriteError,
> {
    let summary = source.summary();
    let replica =
        remanence_parity::checked_tape_index_replica_layout(selected.block_size, summary.counts)
            .map_err(|error| {
                PoolWriteError::InvalidInput(format!("plan direct terminal replica: {error}"))
            })?;
    let separation_records = remanence_parity::index_separation_records(
        selected.block_size,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal separation: {error}"))
    })?;
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        selected.block_size,
        first_tape_file_number,
        first_start_lba,
        replica.replica_record_count,
        separation_records,
    )
    .map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal layout: {error}"))
    })?;
    authorize_terminal_close_only_plan(
        state,
        Some(pool_cfg),
        selected,
        first_start_lba,
        summary.counts,
        layout.expected_eod_lba,
    )?;
    let edition_sequence = previous.ordinal.checked_add(1).ok_or_else(|| {
        PoolWriteError::InvalidInput("terminal edition sequence overflows u64".to_string())
    })?;
    let edition_id = *Uuid::new_v4().as_bytes();
    let diagnostics = match terminal_prefix {
        Some(prefix) => prefix.diagnostics.clone(),
        None => remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")).capture()?,
    };
    let writer_version = diagnostics.writer_version.clone();
    let write_timestamp = diagnostics.write_timestamp.clone();
    let plan = remanence_parity::assemble_terminal_plan(
        selected.tape_uuid,
        selected.block_size,
        false,
        edition_sequence,
        summary.scope,
        summary.counts,
        source,
        layout,
        diagnostics,
        edition_id,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal edition: {error}"))
    })?;
    let intent = remanence_state::TerminalFinalizationIntent {
        tape_uuid: selected.tape_uuid,
        trigger,
        manual: None,
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id,
        edition_sequence,
        edition_digest: plan.edition.edition_digest,
        writer_version,
        write_timestamp,
        terminal_prefix: terminal_prefix.map(remanence_state::TerminalFinalizationPrefixPlan::from),
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)?,
    };
    Ok((intent, plan))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_direct_terminal_tail(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
    sink: &mut dyn BlockSink,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    selected: &SelectedTape,
    intent: remanence_state::TerminalFinalizationIntent,
    plan: &TerminalTripleWritePlan,
    source: &mut remanence_state::CheckpointTerminalIndexRecordSource<'_>,
    mut parity_journal: Option<&mut FileTapeFileJournal>,
    parity_prefix: Option<(ParitySinkSessionState, &TerminalPrefixPlan)>,
) -> Result<remanence_state::CheckpointJournalRecord, PoolWriteError> {
    let _terminal_admission = if let Some(drive) = sink.append_admission_drive() {
        let mut ctx = crate::append_admission::context(
            selected.tape_uuid,
            selected.block_size,
            previous.eod_partition,
            previous.eod_lba,
        );
        ctx.terminal_prefix = parity_prefix.as_ref().map(|(_, prefix)| (*prefix).clone());
        Some(
            crate::append_admission::admit_append_boundary(
                drive,
                crate::append_admission::AppendAuthority::Terminal { plan },
                ctx,
            )
            .map_err(|e| PoolWriteError::InvalidInput(e.to_string()))?,
        )
    } else {
        None
    };
    let write_attempted = Cell::new(false);
    let execution = (|| -> Result<_, PoolWriteError> {
        let mut raw = BlockSinkRawTapeSink::new(sink);
        let mut tracked = CapacityTrackingRawTapeSink::new(&mut raw, &write_attempted);
        if let Some((session_state, prefix)) = parity_prefix {
            let journal = parity_journal.as_deref_mut().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "direct parity terminal prefix has no parity journal".to_string(),
                )
            })?;
            let parity = ParitySink::from_session_state(&mut tracked, journal, session_state)?;
            parity.close_for_terminal_index(prefix, TerminalPrefixReconcileEvidence::Absent)?;
        }
        let completed_intent = {
            let mut authority = DirectSequentialTerminalAuthority {
                checkpoint,
                parity_journal,
                intent,
                cursor_proved_for: TerminalTailProgress::BeforeReplicaA,
            };
            match remanence_parity::write_terminal_tail(&mut tracked, source, &mut authority, plan)
                .map_err(|error| {
                    PoolWriteError::InvalidInput(format!("write direct terminal triple: {error}"))
                })? {
                TerminalTailRunOutcome::Complete => {}
                TerminalTailRunOutcome::RecoveryRequired {
                    progress,
                    component,
                    evidence,
                } => {
                    return Err(PoolWriteError::InvalidInput(format!(
                        "direct terminal continuation requires source-capable reconciliation at {progress:?} for {:?}/{}: {evidence:?}",
                        component.kind, component.ordinal
                    )));
                }
            }
            authority.intent
        };
        if completed_intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC
        {
            return Err(PoolWriteError::InvalidInput(
                "direct terminal writer completed before final replica C became durable"
                    .to_string(),
            ));
        }
        let replica_c = plan.edition.descriptor.terminal_layout.components[4];
        let final_bundle =
            remanence_parity::terminal_component_bundle(plan, replica_c).map_err(|error| {
                PoolWriteError::InvalidInput(format!("build direct final C authority: {error}"))
            })?;
        let final_record = remanence_state::CheckpointJournalRecord {
            ordinal: previous.ordinal.checked_add(1).ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "terminal checkpoint ordinal overflows u64".to_string(),
                )
            })?,
            committed_object_count: previous.committed_object_count,
            eod_partition: plan.edition.descriptor.terminal_layout.partition,
            eod_lba: plan.edition.descriptor.terminal_layout.expected_eod_lba,
            tape_uuid: selected.tape_uuid,
            batch_id: completed_intent.edition_id,
            next_tape_file_number: replica_c
                .planned_tape_file_number
                .checked_add(1)
                .ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "terminal next tape-file number overflows u64".to_string(),
                    )
                })?,
            block_size: selected.block_size,
            objects: Vec::new(),
            scheme: previous.scheme.clone(),
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: Some(final_bundle),
            terminal_finalization: Some(completed_intent),
            sealed_after_write: true,
            seal_due: None,
        };
        checkpoint.append_terminal_finalization(std::slice::from_ref(&final_record))?;
        state.project_checkpoint_record(&final_record)?;
        Ok(final_record)
    })();
    match execution {
        Ok(record) => Ok(record),
        Err(error) if write_attempted.get() => {
            let recovery = (|| -> Result<(), PoolWriteError> {
                let intent = checkpoint.mark_terminal_recovery_required()?;
                state.project_terminal_finalization(TerminalFinalizationProjectionInput {
                    tape_uuid: selected.tape_uuid,
                    trigger: intent.trigger,
                    operation_id: None,
                    progress: intent.progress,
                    edition_digest: intent.edition_digest,
                    layout_digest: intent.layout.layout_digest,
                    outcome: TerminalFinalizationOutcome::RecoveryRequired,
                    updated_at_utc: None,
                })?;
                Ok(())
            })();
            let error = match recovery {
                Ok(()) => error,
                Err(recovery_error) => PoolWriteError::InvalidInput(format!(
                    "{error}; failed to persist terminal recovery-required authority: {recovery_error}"
                )),
            };
            Err(fence_after_terminal_motion(
                state,
                fence_cfg,
                selected,
                "terminal_finalization",
                error,
            ))
        }
        Err(error) => Err(error),
    }
}

pub(super) fn publish_direct_terminal_intent(
    state: &mut CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    selected: &SelectedTape,
    intent: &remanence_state::TerminalFinalizationIntent,
) -> Result<(), PoolWriteError> {
    checkpoint.begin_terminal_finalization(intent)?;
    state.project_terminal_finalization(TerminalFinalizationProjectionInput {
        tape_uuid: selected.tape_uuid,
        trigger: intent.trigger,
        operation_id: None,
        progress: intent.progress,
        edition_digest: intent.edition_digest,
        layout_digest: intent.layout.layout_digest,
        outcome: TerminalFinalizationOutcome::InProgress,
        updated_at_utc: None,
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_direct_checkpoint_prefix(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
    pool_cfg: &TapePoolConfig,
    sink: &mut dyn BlockSink,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    records: &[remanence_state::CheckpointJournalRecord],
    selected: &SelectedTape,
    trigger: remanence_state::TerminalFinalizationTrigger,
    mut parity_journal: Option<&mut FileTapeFileJournal>,
    parity_state: Option<ParitySinkSessionState>,
) -> Result<remanence_state::CheckpointJournalRecord, PoolWriteError> {
    let previous = records.last().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "direct terminal finalization has no committed checkpoint".to_string(),
        )
    })?;
    let observed = sink.position()?;
    if observed.partition != previous.eod_partition || observed.lba != previous.eod_lba {
        return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(format!(
            "direct terminal cursor is partition {} lba {}, expected checkpoint partition {} lba {}",
            observed.partition, observed.lba, previous.eod_partition, previous.eod_lba
        ))));
    }
    match (&selected.parity_config, parity_state) {
        (ParityConfig::None, None) => {
            let mut source =
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(
                    checkpoint,
                )?;
            let first_file = source.summary().scope.covered_prefix_tape_file_count;
            let (intent, plan) = build_direct_terminal_plan(
                state,
                pool_cfg,
                selected,
                previous,
                &mut source,
                first_file,
                observed.lba,
                None,
                trigger,
            )?;
            publish_direct_terminal_intent(state, checkpoint, selected, &intent)?;
            execute_direct_terminal_tail(
                state,
                fence_cfg,
                sink,
                checkpoint,
                previous,
                selected,
                intent,
                &plan,
                &mut source,
                None,
                None,
            )
        }
        (ParityConfig::Scheme(_), Some(session_state)) => {
            let journal = parity_journal.as_deref_mut().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "direct parity terminal finalization has no parity journal".to_string(),
                )
            })?;
            let (prefix, session_state) = {
                let mut raw = BlockSinkRawTapeSink::new(sink);
                let parity = ParitySink::from_session_state(&mut raw, journal, session_state)?;
                let prefix =
                    parity.plan_terminal_index_close(parity.writer_identity().capture()?)?;
                let state = parity.into_session_state()?;
                (prefix, state)
            };
            let persisted = remanence_state::TerminalFinalizationPrefixPlan::from(&prefix);
            let mut source = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                checkpoint,
                journal,
                &persisted,
            )?;
            let (intent, plan) = build_direct_terminal_plan(
                state,
                pool_cfg,
                selected,
                previous,
                &mut source,
                prefix.tail_start_tape_file_number,
                prefix.tail_start_lba,
                Some(&prefix),
                trigger,
            )?;
            publish_direct_terminal_intent(state, checkpoint, selected, &intent)?;
            execute_direct_terminal_tail(
                state,
                fence_cfg,
                sink,
                checkpoint,
                previous,
                selected,
                intent,
                &plan,
                &mut source,
                parity_journal,
                Some((session_state, &prefix)),
            )
        }
        (ParityConfig::None, Some(_)) => Err(PoolWriteError::InvalidInput(
            "parity-off direct finalization unexpectedly has parity session state".to_string(),
        )),
        (ParityConfig::Scheme(_), None) => Err(PoolWriteError::InvalidInput(
            "parity direct finalization is missing its terminal session state".to_string(),
        )),
    }
}

/// Verify, configure, and write one Object through the same loaded drive.
///
/// The wrapper owns the safety-critical sequence: while the supplied
/// [`StateHandle`] holds this deployment's exclusive state lock, it verifies
/// the selected tape's current catalog binding and BOT identity, reads media
/// state from the same drive handle, requires positive rewritable evidence,
/// applies fixed-block configuration, and writes through that handle.
/// Abstract checkpointed cores remain private, so a downstream caller cannot
/// pair drive-A evidence with a drive-B sink or fabricate selection geometry.
///
/// The state lock coordinates Remanence processes that use the same state
/// directory. It is not a SCSI persistent reservation: operators must exclude
/// unrelated tape software and Remanence instances configured with another
/// state directory from the drive for this call's duration.
pub fn write_to_selected_drive_checkpointed(
    state: &mut StateHandle,
    drive: &mut DriveHandle,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
    crate::reconcile_checkpoint_journal_projections(state)
        .map_err(|status| PoolWriteError::CheckpointReconciliation(status.message().to_string()))?;
    write_to_selected_drive_checkpointed_reconciled(state, drive, request, selected)
}

/// Continue the same admitted writer under an already reconciled exclusive state lock.
pub(crate) fn write_to_selected_drive_checkpointed_reconciled(
    state: &mut StateHandle,
    drive: &mut DriveHandle,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
    let fence_cfg = &crate::write_owner::TerminalWriteConfig {
        checkpoint_journal_dir: state.paths().journal_dir.join("checkpoints"),
        audit_dir: state.paths().audit_dir.clone(),
        audit_fsync: true,
        audit_append_lock: Arc::new(std::sync::Mutex::new(())),
    };
    let pool_cfg = configured_direct_pool(state, &request.pool_id)?;
    let checkpoint_journal_dir = state.paths().journal_dir.join("checkpoints");
    let parity_journal_path = state.journal_path(selected.tape_uuid);
    let resources = PoolWriteResources::for_state(
        state.config().daemon.io_memory_ceiling,
        &state.paths().state_dir,
    )
    .map_err(PoolWriteError::InvalidInput)?;
    let result = write_to_selected_drive_checkpointed_with_catalog(
        state.catalog_index(),
        fence_cfg,
        drive,
        &pool_cfg,
        request,
        selected,
        checkpoint_journal_dir.as_path(),
        parity_journal_path.as_path(),
        &resources,
    );
    state.refresh_audit_append_cursor()?;
    let result = result?;
    finish_direct_write_host_suffix(state, &result)?;
    Ok(result)
}

pub(super) fn finish_direct_write_host_suffix(
    state: &mut StateHandle,
    result: &PoolWriteResult,
) -> Result<(), PoolWriteError> {
    if !result.sealed_after_write() {
        return Ok(());
    }
    // The sealed checkpoint, not the in-memory result flag, is the authority
    // for this host-only suffix. Replaying it publishes the exactly-once
    // TapeSealed audit fact without touching media and repairs the same crash
    // cut on the next invocation.
    crate::reconcile_checkpoint_journal_projections(state)
        .map_err(|status| PoolWriteError::CheckpointReconciliation(status.message().to_string()))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_drive_checkpointed_with_catalog(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
    drive: &mut DriveHandle,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    checkpoint_journal_dir: &Path,
    parity_journal_path: &Path,
    resources: &PoolWriteResources,
) -> Result<PoolWriteResult, PoolWriteError> {
    let mut selected = selected;
    selected.standalone_endpoint = drive
        .endpoint()
        .durable_key()
        .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. }))
        .cloned();
    let mut write_admission = resources
        .write_admissions
        .reserve(
            &request.pool_id,
            &request.caller_object_id,
            request.expected_object_id,
        )
        .map_err(|status| PoolWriteError::WriteAdmissionConflict(status.message().to_string()))?;
    let preflight =
        prepare_checkpointed_write(state, pool_cfg, &request, &selected, checkpoint_journal_dir)?;
    let (checkpoint_lease, prior_records) = match preflight {
        CheckpointedWritePreflight::Replay(result) => return Ok(*result),
        CheckpointedWritePreflight::Ready {
            checkpoint_lease,
            prior_records,
        } => (checkpoint_lease, prior_records),
    };

    drive.rewind()?;
    let identity_generation = drive.identity_generation();
    {
        let mut source = DriveHandleSource(drive);
        verify_tape_identity(&mut source, &selected.tape_uuid)?;
    }
    drive.rewind()?;
    let current_cfg = drive.read_config()?;
    require_rewritable_object_media(current_cfg)?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, current_cfg, selected.block_size)?;
    drive.bind_identity_generation(identity_generation)?;
    let last = prior_records.last();
    let ctx = crate::append_admission::context(
        selected.tape_uuid,
        selected.block_size,
        last.map_or(0, |r| r.eod_partition),
        last.map_or(0, |r| r.eod_lba),
    );
    let authority = last.map_or(crate::append_admission::AppendAuthority::Fresh, |_| {
        crate::append_admission::AppendAuthority::Checkpoint { eod: ctx.boundary }
    });
    let _admission = crate::append_admission::admit_append_boundary(drive, authority, ctx)
        .map_err(|e| PoolWriteError::InvalidInput(e.to_string()))?;
    let appended = write_to_selected_tape_checkpointed_after_preflight(
        state,
        fence_cfg,
        &mut DriveHandleSink(drive),
        pool_cfg,
        request,
        selected.clone(),
        parity_journal_path,
        resources,
        checkpoint_lease,
        prior_records,
        Some(&mut write_admission),
    )?;
    if appended.seal_reason.is_some() {
        reprove_checkpoint_boundary(drive, &appended.record)?;
    }
    finish_checkpointed_append(
        state,
        fence_cfg,
        &mut DriveHandleSink(drive),
        pool_cfg,
        &selected,
        parity_journal_path,
        appended,
    )
}

use super::append::reprove_checkpoint_boundary;

pub(super) enum CheckpointedWritePreflight {
    Replay(Box<PoolWriteResult>),
    Ready {
        checkpoint_lease: remanence_state::FileCheckpointJournalLease,
        prior_records: Vec<remanence_state::CheckpointJournalRecord>,
    },
}

pub(super) fn prepare_checkpointed_write(
    state: &mut CatalogIndex,
    pool_cfg: &TapePoolConfig,
    request: &WriteObjectToPoolRequest,
    selected: &SelectedTape,
    checkpoint_journal_dir: &Path,
) -> Result<CheckpointedWritePreflight, PoolWriteError> {
    ensure_request_pool_matches_config(request, pool_cfg)?;
    ensure_selected_tape_binding(state, pool_cfg, selected)?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, request)? {
        return Ok(CheckpointedWritePreflight::Replay(Box::new(result)));
    }
    let checkpoint_journal =
        remanence_state::FileCheckpointJournal::open(checkpoint_journal_dir, selected.tape_uuid)?;
    let checkpoint_lease = checkpoint_journal.acquire_exclusive()?;
    let mut last_checkpoint = None;
    checkpoint_lease.for_each_record_bounded(|record| {
        state.project_checkpoint_record(record)?;
        last_checkpoint = Some(record.clone());
        Ok(())
    })?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, request)? {
        return Ok(CheckpointedWritePreflight::Replay(Box::new(result)));
    }
    let prior_records: Vec<_> = last_checkpoint.into_iter().collect();
    ensure_empty_checkpoint_matches_catalog_freshness(state, selected, &prior_records)?;
    ensure_selected_tape_accepts_session_write(state, pool_cfg, selected)?;
    Ok(CheckpointedWritePreflight::Ready {
        checkpoint_lease,
        prior_records,
    })
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_tape_checkpointed(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    checkpoint_journal_dir: &Path,
    parity_journal_path: &Path,
    resources: &PoolWriteResources,
) -> Result<PoolWriteResult, PoolWriteError> {
    let fence_cfg = &super::test_fence_config(state.path().parent().expect("catalog parent"));
    let preflight =
        prepare_checkpointed_write(state, pool_cfg, &request, &selected, checkpoint_journal_dir)?;
    let (checkpoint_lease, prior_records) = match preflight {
        CheckpointedWritePreflight::Replay(result) => return Ok(*result),
        CheckpointedWritePreflight::Ready {
            checkpoint_lease,
            prior_records,
        } => (checkpoint_lease, prior_records),
    };
    let appended = write_to_selected_tape_checkpointed_after_preflight(
        state,
        fence_cfg,
        sink,
        pool_cfg,
        request,
        selected.clone(),
        parity_journal_path,
        resources,
        checkpoint_lease,
        prior_records,
        None,
    )?;
    finish_checkpointed_append(
        state,
        fence_cfg,
        sink,
        pool_cfg,
        &selected,
        parity_journal_path,
        appended,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_tape_checkpointed_after_preflight(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    parity_journal_path: &Path,
    resources: &PoolWriteResources,
    mut checkpoint_lease: remanence_state::FileCheckpointJournalLease,
    prior_records: Vec<remanence_state::CheckpointJournalRecord>,
    write_admission: Option<&mut crate::write_admission::WriteAdmissionReservation>,
) -> Result<CheckpointedAppend, PoolWriteError> {
    let dispatches = sink
        .append_admission_drive()
        .map(|d| d.media_write_dispatch_count());
    let execution = (|| {
        let direct_replay_fault =
            crate::direct_replay_fault::DirectReplayFaultPlan::from_env_for_object(
                selected.tape_uuid,
                &request.caller_object_id,
            )
            .map_err(PoolWriteError::InvalidInput)?;
        let next_ordinal = prior_records.last().map_or(Ok(1), |record| {
            record.ordinal.checked_add(1).ok_or_else(|| {
                PoolWriteError::InvalidInput("checkpoint ordinal overflows u64".to_string())
            })
        })?;
        let next_committed_count = prior_records.last().map_or(Ok(1), |record| {
            record.committed_object_count.checked_add(1).ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "checkpoint committed object count overflows u64".to_string(),
                )
            })
        })?;
        let batch_id = *Uuid::new_v4().as_bytes();

        let (
            result,
            next_tape_file_number,
            sync,
            scheme,
            object_bundles,
            barrier_bundle,
            terminal_parity_state,
        ) = match &selected.parity_config {
            ParityConfig::None => {
                let append = first_batched_append_context(state, &selected, &prior_records)?;
                let result = write_batched_to_selected_tape_after_replay_check(
                    state,
                    fence_cfg,
                    sink,
                    pool_cfg,
                    request,
                    selected.clone(),
                    None,
                    append,
                )?;
                let projection = result.checkpoint_projection().cloned().ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "batch-of-one object is missing checkpoint projection".to_string(),
                    )
                })?;
                let sync = sink.write_filemarks(0)?;
                let next_tape_file_number = projection
                    .copy
                    .tape_file_number
                    .checked_add(1)
                    .ok_or_else(|| {
                        PoolWriteError::InvalidInput(
                            "checkpoint next tape-file number overflows u64".to_string(),
                        )
                    })?;
                (
                    result,
                    next_tape_file_number,
                    sync,
                    None,
                    Vec::new(),
                    None,
                    None,
                )
            }
            ParityConfig::Scheme(parity_scheme) => {
                let mut parity_journal = resume_record_result(FileTapeFileJournal::open(
                    parity_journal_path,
                    selected.tape_uuid,
                    selected.block_size,
                    parity_scheme.clone(),
                ))?;
                if parity_journal.orphaned_bundles_preserved_on_open() != 0 {
                    tracing::warn!(
                        tape_uuid = %uuid_text(selected.tape_uuid),
                        orphaned_bundle_count = parity_journal.orphaned_bundles_preserved_on_open(),
                        "preserved sink-journal bundles beyond the last checkpoint watermark; reconciliation required"
                    );
                }
                let snapshot = resume_record_result(parity_journal.committed_snapshot_bounded())?;
                let summary = checked_bounded_resume_summary(&snapshot)?;
                validate_parity_resume_checkpoint_authority(
                    &checkpoint_lease,
                    &parity_journal,
                    prior_records.is_empty(),
                    summary.committed_tape_file_count,
                )
                .map_err(|refusal| match refusal {
                    ResumeAuthorityRefusal::Records(error) => PoolWriteError::from(error),
                    ResumeAuthorityRefusal::State(error) => PoolWriteError::from(error),
                })?;
                let session_state = if summary.committed_tape_file_count == 0 {
                    let located = sink.locate(0)?;
                    if located.partition != 0 || located.lba != 0 {
                        return Err(PoolWriteError::InvalidInput(format!(
                        "fresh parity BOT locate reported partition {} lba {}, expected partition 0 lba 0",
                        located.partition, located.lba
                    )));
                    }
                    let fresh_session_state = {
                        let mut raw = BlockSinkRawTapeSink::new(sink);
                        let mut parity = ParitySink::new_with_journal(
                            &mut raw,
                            &mut parity_journal,
                            parity_scheme.clone(),
                            selected.tape_uuid,
                            selected.block_size,
                            remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
                        )?;
                        parity.write_bootstrap()?;
                        parity.into_session_state()?
                    };
                    project_fresh_parity_bootstrap_bundle(state, &selected, parity_scheme)?;
                    fresh_session_state
                } else {
                    let checkpoint = prior_records.last().ok_or_else(|| {
                        PoolWriteError::InvalidInput(
                            "non-fresh parity tape has no checkpoint journal".to_string(),
                        )
                    })?;
                    sink.locate(checkpoint.eod_lba)?;
                    let plan = summary.append_plan(parity_scheme)?;
                    let live_epoch =
                        if plan.highest_protected_ordinal_before_rebuild < plan.next_data_ordinal {
                            let drive = sink.append_admission_drive().ok_or_else(|| {
                                PoolWriteError::InvalidInput(
                                    "open epoch rebuild requires a readable admitted drive".into(),
                                )
                            })?;
                            let mut source = remanence_parity::DriveHandleRawSource::new(drive);
                            remanence_parity::rebuild_open_epoch_from_bounded_summary(
                                &mut source,
                                &summary,
                                parity_scheme,
                                selected.tape_uuid,
                                selected.block_size,
                            )?
                            .live_epoch
                        } else {
                            None
                        };
                    sink.locate(checkpoint.eod_lba)?;
                    let resume_result = plan.complete(Vec::new())?;
                    let resume_session_state = {
                        let mut raw = BlockSinkRawTapeSink::new(sink);
                        let parity = ParitySink::new_sidecar_only_from_bounded_resume(
                            &mut raw,
                            &mut parity_journal,
                            parity_scheme.clone(),
                            selected.tape_uuid,
                            selected.block_size,
                            BoundedResumeWriterSeed {
                                committed_prefix_snapshot: snapshot,
                                committed_prefix_summary: summary,
                                resume_result: &resume_result,
                                live_epoch,
                            },
                            remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
                        )?;
                        parity.into_session_state()?
                    };
                    resume_session_state
                };
                let mut raw_write_attempted = false;
                let parity_append = (|| -> Result<_, PoolWriteError> {
                    let (session_state, mut result) = {
                        let mut raw = BlockSinkRawTapeSink::new(sink);
                        let mut session_state = Some(session_state);
                        let result = write_batched_parity_to_selected_tape_after_replay_check(
                            state,
                            &mut raw,
                            &mut parity_journal,
                            &mut session_state,
                            pool_cfg,
                            request,
                            selected.clone(),
                            resources.io_memory(),
                            &mut raw_write_attempted,
                        )?;
                        let session_state = session_state.ok_or_else(|| {
                            PoolWriteError::InvalidInput(
                                "successful parity batch-of-one lost its session state".to_string(),
                            )
                        })?;
                        (session_state, result)
                    };
                    let object_bundle = result
                        .write_report()
                        .ok_or_else(|| {
                            PoolWriteError::InvalidInput(
                                "parity batch-of-one is missing its write report".to_string(),
                            )
                        })?
                        .catalog
                        .tape_file_bundle
                        .clone();
                    let (closed, session_state) = {
                        let mut raw = BlockSinkRawTapeSink::new(sink);
                        let mut parity = ParitySink::from_session_state(
                            &mut raw,
                            &mut parity_journal,
                            session_state,
                        )?;
                        let closed =
                            parity.close_open_epoch(remanence_parity::CloseReason::Barrier)?;
                        let state = parity.into_session_state()?;
                        (closed, state)
                    };
                    result.hardware_early_warning |= session_state.hardware_early_warning_seen();
                    Ok((
                        result,
                        closed.next_tape_file_number,
                        closed.barrier_outcome,
                        Some(parity_scheme.clone()),
                        vec![object_bundle],
                        closed.committed_bundle,
                        Some(session_state),
                    ))
                })();
                match parity_append {
                    Ok(result) => result,
                    Err(error) if raw_write_attempted => {
                        return Err(fence_after_terminal_motion(
                            state,
                            fence_cfg,
                            &selected,
                            "parity_append",
                            error,
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let captured = sink.position()?;
        if captured.partition != sync.position_after.partition
            || captured.lba != sync.position_after.lba
        {
            return Err(PoolWriteError::TapeIo(TapeIoError::OperationFailed(
                "batch-of-one barrier position proof mismatch".to_string(),
            )));
        }
        let projection = result.checkpoint_projection().cloned().ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "batch-of-one object is missing checkpoint projection".to_string(),
            )
        })?;
        let record = remanence_state::CheckpointJournalRecord {
            ordinal: next_ordinal,
            committed_object_count: next_committed_count,
            eod_partition: captured.partition,
            eod_lba: captured.lba,
            tape_uuid: selected.tape_uuid,
            batch_id,
            next_tape_file_number,
            block_size: selected.block_size,
            objects: vec![projection],
            scheme,
            object_tape_file_bundles: object_bundles,
            barrier_bundle,
            terminal_finalization: None,
            sealed_after_write: false,
            seal_due: None,
        };
        let used_bytes = captured
            .lba
            .checked_mul(u64::from(selected.block_size))
            .ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "batch-of-one post-barrier used-byte count overflows u64".to_string(),
                )
            })?;
        let seal_reason = selected_tape_seal_reason_at_barrier(
            state,
            &selected,
            pool_cfg,
            TapePositionAfterWrite {
                used_bytes,
                early_warning: result.hardware_early_warning || sync.early_warning,
            },
        )?;
        if let Err(error) = checkpoint_lease.append(&record) {
            quarantine_direct_admission_on_uncertain_append(&error, write_admission);
            return Err(error.into());
        }
        if let Some(fault) = direct_replay_fault.as_ref() {
            fault
                .abort_after_checkpoint_append(&record)
                .map_err(PoolWriteError::InvalidInput)?;
        }
        if let Err(error) = state.project_checkpoint_record(&record) {
            if let Some(admission) = write_admission {
                admission.quarantine_until_restart();
            }
            return Err(error.into());
        }
        Ok((result, record, terminal_parity_state, seal_reason))
    })();
    let (result, record, terminal_parity_state, seal_reason) = match execution {
        Ok(value) => value,
        Err(error) => {
            if let Some(drive) = sink.append_admission_drive() {
                if dispatches != Some(drive.media_write_dispatch_count()) {
                    if let Err(marker) = crate::append_admission::abandon_pending_append(
                        drive,
                        &checkpoint_lease,
                        None,
                    ) {
                        return Err(PoolWriteError::InvalidInput(format!(
                            "{error}; abandonment marker failed: {marker}"
                        )));
                    }
                    if let ParityConfig::Scheme(scheme) = &selected.parity_config {
                        let mut journal = FileTapeFileJournal::open(
                            parity_journal_path,
                            selected.tape_uuid,
                            selected.block_size,
                            scheme.clone(),
                        )
                        .map_err(ParityError::from)?;
                        crate::append_admission::discard_reconciled_parity_suffix(
                            &mut journal,
                            &checkpoint_lease,
                        )
                        .map_err(PoolWriteError::InvalidInput)?;
                    }
                }
            }
            return Err(error);
        }
    };
    Ok(CheckpointedAppend {
        result,
        record,
        checkpoint_lease,
        prior_records,
        terminal_parity_state,
        seal_reason,
    })
}

/// A published checkpoint retains its lease until terminal close is finished.
pub(super) struct CheckpointedAppend {
    result: PoolWriteResult,
    record: remanence_state::CheckpointJournalRecord,
    checkpoint_lease: remanence_state::FileCheckpointJournalLease,
    prior_records: Vec<remanence_state::CheckpointJournalRecord>,
    terminal_parity_state: Option<ParitySinkSessionState>,
    seal_reason: Option<TapeSealReason>,
}
fn finish_checkpointed_append(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    parity_journal_path: &Path,
    appended: CheckpointedAppend,
) -> Result<PoolWriteResult, PoolWriteError> {
    let CheckpointedAppend {
        mut result,
        record,
        mut checkpoint_lease,
        prior_records,
        mut terminal_parity_state,
        seal_reason,
    } = appended;
    if let Some(seal_reason) = seal_reason {
        let mut authority_records = prior_records;
        authority_records.push(record);
        let trigger = terminal_trigger_for_seal_reason(seal_reason)?;
        match &selected.parity_config {
            ParityConfig::None => {
                finalize_direct_checkpoint_prefix(
                    state,
                    fence_cfg,
                    pool_cfg,
                    sink,
                    &mut checkpoint_lease,
                    &authority_records,
                    selected,
                    trigger,
                    None,
                    None,
                )?;
            }
            ParityConfig::Scheme(parity_scheme) => {
                let parity_state = terminal_parity_state.take().ok_or_else(|| {
                    PoolWriteError::InvalidInput(
                        "sealed parity batch has no terminal session state".to_string(),
                    )
                })?;
                let mut parity_journal = FileTapeFileJournal::open(
                    parity_journal_path,
                    selected.tape_uuid,
                    selected.block_size,
                    parity_scheme.clone(),
                )
                .map_err(ParityError::from)?;
                finalize_direct_checkpoint_prefix(
                    state,
                    fence_cfg,
                    pool_cfg,
                    sink,
                    &mut checkpoint_lease,
                    &authority_records,
                    selected,
                    trigger,
                    Some(&mut parity_journal),
                    Some(parity_state),
                )?;
            }
        }
        result.sealed_after_write = true;
    }
    Ok(result)
}

pub(super) fn quarantine_direct_admission_on_uncertain_append(
    error: &StateError,
    admission: Option<&mut crate::write_admission::WriteAdmissionReservation>,
) {
    if error.is_checkpoint_append_authority_uncertain() {
        if let Some(admission) = admission {
            admission.quarantine_until_restart();
        }
    }
}

// Compatibility paths for existing callers during the staged actor migration.
pub(crate) use super::append::{
    project_fresh_parity_bootstrap_bundle,
    write_batched_parity_to_selected_tape_after_replay_check,
    write_batched_to_selected_tape_after_replay_check,
};
pub(crate) use super::capacity::authorize_terminal_close_only_plan;
use super::replay::configured_direct_pool;
pub use super::replay::replay_committed_pool_write_from_state;
pub(crate) use super::replay::replay_committed_pool_write_reconciled;
pub(crate) use super::resume_authority::{
    validate_parity_resume_checkpoint_authority, ResumeAuthorityRefusal,
};
#[cfg(test)]
pub(crate) use super::test_support::write_to_selected_tape_with_live_counter;
#[cfg(test)]
pub use super::test_support::{write_object_to_pool, write_to_selected_tape};
pub use crate::tape_identity::verify_tape_identity;
pub use crate::tape_init::{build_tape_bootstrap, write_tape_bootstrap};
