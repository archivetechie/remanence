//! Checkpointed direct writer, bootstrap handling, and terminal close.

use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use remanence_library::{
    BlockSink, BlockSource, DriveHandle, DriveHandleSink, DriveHandleSource, TapeIoError,
};
use remanence_parity::{
    bootstrap::{parse_bootstrap_block, write_bootstrap_block},
    checked_bounded_resume_summary, sole_bot_filemark_map_digest, BlockSinkRawTapeSink,
    BootstrapPayload, BoundedResumeWriterSeed, CommittedBundle, CommittedBundleKind,
    FileTapeFileJournal, ParityConfig, ParityError, ParityScheme, ParitySchemeRecord, ParitySink,
    ParitySinkSessionState, RawTapeSink, TapeFileEntry, TapeFileJournal, TapeFileKind,
    TerminalPrefixPlan, TerminalPrefixReconcileEvidence, TerminalTailProgress,
    TerminalTailRunOutcome, TerminalTripleCloseInput, TerminalTripleWritePlan,
};
use remanence_state::{
    CatalogIndex, StateError, StateHandle, TapeJournalIndexInput, TapePoolConfig,
    TerminalFinalizationOutcome, TerminalFinalizationProjectionInput,
};
use remanence_stream::{write_prepared_object_to_parity_from_readers, StreamingObjectWriteReport};
use uuid::Uuid;

use super::capacity::{
    ensure_empty_checkpoint_matches_catalog_freshness, ensure_no_parity_terminal_close_capacity,
    ensure_request_pool_matches_config, ensure_selected_tape_accepts_session_write,
    ensure_selected_tape_accepts_write, ensure_selected_tape_binding,
    ensure_selected_tape_has_capacity, first_batched_append_context, now_rfc3339,
    parity_capacity_basis_blocks, reserve_parity_object_capacity,
    selected_tape_seal_reason_at_barrier, terminal_capacity_basis_blocks,
    terminal_watermark_blocks, uuid_text,
};
use super::model::{
    parity_post_write_projection_gate, require_rewritable_object_media, AppendCommitDiagnostics,
    BatchedNoParityAppendContext, CapacityTrackingRawTapeSink, DirectSequentialTerminalAuthority,
    PoolWriteDurability, PoolWriteError, PoolWriteResources, PoolWriteResult, SelectedTape,
    TapeIdentityError, TapePositionAfterWrite, TapeSealReason, TapeUuid, WriteObjectToPoolRequest,
};
use super::no_parity::{
    checkpoint_projection_for_no_parity_write, fence_after_terminal_motion,
    maybe_replay_pool_write, pool_write_result, write_no_parity_object_to_selected_tape,
};
#[cfg(test)]
use super::overlap::write_parity_object_to_selected_tape;
use super::prepare::{
    open_prepared_readers, parity_label, prepare_pool_object, prepare_stored_object,
    prepared_payload_bytes, stored_footprint_bytes, write_canonical_plaintext_object_to_parity,
    write_encrypted_object_to_parity, PreparedPoolWrite, PreparedStoredObject,
};
#[cfg(test)]
use super::selection::select_tape_in_pool;
use super::staging::{CountingBlockSink, LiveCounterBlockSink};
use super::{TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS, VERIFY_BOOTSTRAP_READ_BYTES};
use crate::bytes_to_hex;
#[cfg(test)]
use std::collections::HashSet;

/// Write one regular file to a caller-named pool using the Phase 1
/// non-hardware-compatible `BlockSink` path, commit catalog rows, and return
/// the resulting object locator.
#[cfg(test)]
pub fn write_object_to_pool(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
        return Ok(result);
    }
    let source_size = request.source.size_bytes()?;
    let reserved_tape_uuids = HashSet::new();
    let selected = select_tape_in_pool(state, pool_cfg, source_size, &reserved_tape_uuids)?;
    write_to_selected_tape_inner(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        false,
        PoolWriteDurability::PerObject,
    )
}

/// Write one regular file to a previously selected tape without re-running
/// pool tape selection.
///
/// This is the select-once entrypoint for callers that already opened a write
/// session against a concrete tape. The selected tape's [`ParityConfig`]
/// controls whether the write uses the existing parity path or the direct
/// no-parity bootstrap/body/filemark path.
#[cfg(test)]
pub fn write_to_selected_tape(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
    write_to_selected_tape_with_live_counter(state, sink, pool_cfg, request, selected, None)
}

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

/// Validate one already-planned close-only terminal tail through the shared
/// exact capacity authority.
///
/// Parity finalization must emit its pending sidecar/ParityMap prefix before
/// `first_start_lba`; the barrier-proved cursor therefore charges that prefix
/// without modeling it a second time. The returned report covers exactly the
/// remaining A/gap/B/gap/C tail plus the shared safety allowance under the
/// selected pool/cartridge C/L/H basis.
pub(crate) fn authorize_terminal_close_only_plan(
    state: &CatalogIndex,
    pool_cfg: Option<&TapePoolConfig>,
    selected: &SelectedTape,
    first_start_lba: u64,
    counts: remanence_parity::TapeIndexReplicaCounts,
    planned_eod_lba: u64,
) -> Result<remanence_parity::TerminalTripleCloseReport, PoolWriteError> {
    let capacity_blocks = terminal_capacity_basis_blocks(state, pool_cfg, selected)?;
    let (low_watermark_blocks, high_watermark_blocks) =
        terminal_watermark_blocks(capacity_blocks, pool_cfg)?;
    let remaining_tape_blocks = capacity_blocks
        .checked_sub(first_start_lba)
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(format!(
                "terminal cursor {first_start_lba} exceeds capacity basis {capacity_blocks}"
            ))
        })?;
    let report = TerminalTripleCloseInput {
        projected_object_present: false,
        projected_object_blocks: 0,
        block_size_bytes: selected.block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 1,
        parity_shards_per_epoch: 0,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object: 0,
        structural_entries_before_object: counts.structural_entry_count,
        object_rows_before_object: counts.object_row_count,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS,
        remaining_tape_blocks,
        capacity_basis_blocks: capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 0,
    }
    .evaluate()?;
    let planned_tail_charge = planned_eod_lba
        .checked_sub(first_start_lba)
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(
                "terminal layout ends before its starting cursor".to_string(),
            )
        })?;
    if report.terminal_tail_charge_blocks != planned_tail_charge {
        return Err(PoolWriteError::InvalidInput(format!(
            "terminal close authority/layout mismatch: calculator={} planned={planned_tail_charge}",
            report.terminal_tail_charge_blocks
        )));
    }
    Ok(report)
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
    let writer_version = format!("remanence-api/{}", env!("CARGO_PKG_VERSION"));
    let write_timestamp = now_rfc3339()?;
    let edition = remanence_parity::plan_tape_index_edition(
        remanence_parity::TapeIndexEditionDescriptor {
            tape_uuid: selected.tape_uuid,
            edition_id,
            edition_sequence,
            scope: summary.scope,
            counts: summary.counts,
            block_size: selected.block_size,
            compression_enabled: false,
            writer_version: writer_version.clone(),
            write_timestamp: write_timestamp.clone(),
            terminal_layout: layout,
        },
        source,
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
        edition_digest: edition.edition_digest,
        writer_version,
        write_timestamp,
        terminal_prefix: terminal_prefix.map(remanence_state::TerminalFinalizationPrefixPlan::from),
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)?,
    };
    let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
        PoolWriteError::InvalidInput(format!("plan direct terminal writer: {error}"))
    })?;
    Ok((intent, plan))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_direct_terminal_tail(
    state: &mut CatalogIndex,
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
                let prefix = parity.plan_terminal_index_close()?;
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
    let pool_cfg = configured_direct_pool(state, &request.pool_id)?;
    let checkpoint_journal_dir = state.paths().journal_dir.join("checkpoints");
    let parity_journal_path = state.journal_path(selected.tape_uuid);
    let resources = PoolWriteResources::new(state.config().daemon.io_memory_ceiling)
        .map_err(PoolWriteError::InvalidInput)?;
    let result = write_to_selected_drive_checkpointed_with_catalog(
        state.catalog_index(),
        drive,
        &pool_cfg,
        request,
        selected,
        checkpoint_journal_dir.as_path(),
        parity_journal_path.as_path(),
        &resources,
    )?;
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

/// Resolve an already-committed direct write without selecting or moving tape.
///
/// This globally reconciles configured checkpoint journals first, then checks
/// the exact operator-owned pool/caller replay key and content guards. The
/// drive-bound writer repeats the same checks after selection to close races.
pub fn replay_committed_pool_write_from_state(
    state: &mut StateHandle,
    request: &WriteObjectToPoolRequest,
) -> Result<Option<PoolWriteResult>, PoolWriteError> {
    crate::reconcile_checkpoint_journal_projections(state)
        .map_err(|status| PoolWriteError::CheckpointReconciliation(status.message().to_string()))?;
    let pool_cfg = configured_direct_pool(state, &request.pool_id)?;
    maybe_replay_pool_write(state.catalog_index(), &pool_cfg, request)
}

pub(super) fn configured_direct_pool(
    state: &StateHandle,
    requested_pool_id: &str,
) -> Result<TapePoolConfig, PoolWriteError> {
    state
        .config()
        .tape_pools
        .iter()
        .find(|pool| pool.id.trim() == requested_pool_id.trim())
        .cloned()
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(format!(
                "request names unconfigured tape pool {}",
                requested_pool_id.trim()
            ))
        })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_drive_checkpointed_with_catalog(
    state: &mut CatalogIndex,
    drive: &mut DriveHandle,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    checkpoint_journal_dir: &Path,
    parity_journal_path: &Path,
    resources: &PoolWriteResources,
) -> Result<PoolWriteResult, PoolWriteError> {
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
    {
        let mut source = DriveHandleSource(drive);
        verify_tape_identity(&mut source, &selected.tape_uuid)?;
    }
    drive.rewind()?;
    let current_cfg = drive.read_config()?;
    require_rewritable_object_media(current_cfg)?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, current_cfg, selected.block_size)?;
    let mut sink = DriveHandleSink(drive);
    write_to_selected_tape_checkpointed_after_preflight(
        state,
        &mut sink,
        pool_cfg,
        request,
        selected,
        parity_journal_path,
        resources,
        checkpoint_lease,
        prior_records,
        Some(&mut write_admission),
    )
}

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
    let preflight =
        prepare_checkpointed_write(state, pool_cfg, &request, &selected, checkpoint_journal_dir)?;
    let (checkpoint_lease, prior_records) = match preflight {
        CheckpointedWritePreflight::Replay(result) => return Ok(*result),
        CheckpointedWritePreflight::Ready {
            checkpoint_lease,
            prior_records,
        } => (checkpoint_lease, prior_records),
    };
    write_to_selected_tape_checkpointed_after_preflight(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        parity_journal_path,
        resources,
        checkpoint_lease,
        prior_records,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_tape_checkpointed_after_preflight(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    parity_journal_path: &Path,
    resources: &PoolWriteResources,
    mut checkpoint_lease: remanence_state::FileCheckpointJournalLease,
    prior_records: Vec<remanence_state::CheckpointJournalRecord>,
    write_admission: Option<&mut crate::write_admission::WriteAdmissionReservation>,
) -> Result<PoolWriteResult, PoolWriteError> {
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
        mut result,
        next_tape_file_number,
        sync,
        scheme,
        object_bundles,
        barrier_bundle,
        mut terminal_parity_state,
    ) = match &selected.parity_config {
        ParityConfig::None => {
            let append = first_batched_append_context(state, &selected, &prior_records)?;
            let result = write_batched_to_selected_tape_after_replay_check(
                state,
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
            let mut parity_journal = FileTapeFileJournal::open(
                parity_journal_path,
                selected.tape_uuid,
                selected.block_size,
                parity_scheme.clone(),
            )
            .map_err(ParityError::from)?;
            if parity_journal.orphaned_bundles_preserved_on_open() != 0 {
                tracing::warn!(
                    tape_uuid = %uuid_text(selected.tape_uuid),
                    orphaned_bundle_count = parity_journal.orphaned_bundles_preserved_on_open(),
                    "preserved sink-journal bundles beyond the last checkpoint watermark; reconciliation required"
                );
            }
            let snapshot = parity_journal
                .committed_snapshot_bounded()
                .map_err(ParityError::from)?;
            let summary = checked_bounded_resume_summary(&snapshot)?;
            if prior_records.is_empty() {
                if summary.committed_tape_file_count != 0 {
                    return Err(PoolWriteError::InvalidInput(
                        "parity journal has a committed prefix but checkpoint authority is empty"
                            .to_string(),
                    ));
                }
            } else {
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed(
                    &checkpoint_lease,
                    &parity_journal,
                )?;
            }
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
                if !plan.sidecars_to_emit.is_empty()
                    || plan.highest_protected_ordinal_before_rebuild != plan.next_data_ordinal
                {
                    return Err(PoolWriteError::InvalidInput(
                        "checkpointed parity tape retains an open epoch".to_string(),
                    ));
                }
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
                            live_epoch: None,
                        },
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
                    let closed = parity.close_open_epoch(remanence_parity::CloseReason::Barrier)?;
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
    if let Some(seal_reason) = seal_reason {
        let mut authority_records = prior_records;
        authority_records.push(record);
        let trigger = terminal_trigger_for_seal_reason(seal_reason)?;
        match &selected.parity_config {
            ParityConfig::None => {
                finalize_direct_checkpoint_prefix(
                    state,
                    pool_cfg,
                    sink,
                    &mut checkpoint_lease,
                    &authority_records,
                    &selected,
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
                    pool_cfg,
                    sink,
                    &mut checkpoint_lease,
                    &authority_records,
                    &selected,
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

pub(crate) fn project_fresh_parity_bootstrap_bundle(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    scheme: &ParityScheme,
) -> Result<(), PoolWriteError> {
    // The caller holds the exclusive journal handle and invokes this only
    // after `ParitySink::write_bootstrap` returned success at the proved BOT
    // cursor. Project that exact sole-BOT write without asking ordinary
    // bounded-resume replay to accept its intentionally uncheckpointed state.
    let bootstrap_bundle = CommittedBundle {
        kind: CommittedBundleKind::BotBootstrap,
        entries: vec![TapeFileEntry {
            tape_file_number: 0,
            kind: TapeFileKind::Bootstrap,
            block_count: 1,
            physical_start_hint: Some(0),
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        }],
        highest_protected_ordinal: 0,
        total_committed_ordinals: 0,
    };
    state.project_committed_tape_file_bundle(
        TapeJournalIndexInput {
            tape_uuid: selected.tape_uuid,
            block_size: selected.block_size,
            scheme: Some(scheme.clone()),
            journal_offset_bytes: 0,
        },
        &bootstrap_bundle,
    )?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn write_to_selected_tape_with_live_counter(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
) -> Result<PoolWriteResult, PoolWriteError> {
    write_to_selected_tape_with_live_counter_impl(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        live_write_counter,
        true,
        PoolWriteDurability::PerObject,
    )
}

/// Write one parity-off object into a server-owned provisional batch.
pub(crate) fn write_batched_to_selected_tape_after_replay_check(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
    append: BatchedNoParityAppendContext,
) -> Result<PoolWriteResult, PoolWriteError> {
    if !matches!(selected.parity_config, ParityConfig::None) {
        return Err(PoolWriteError::InvalidInput(
            "batched checkpointing is not admitted on parity-enabled tapes".to_string(),
        ));
    }
    write_to_selected_tape_with_live_counter_impl(
        state,
        sink,
        pool_cfg,
        request,
        selected,
        live_write_counter,
        false,
        PoolWriteDurability::Batched(append),
    )
}

/// Write one parity-protected object through the actor-carried logical sink.
///
/// On success, `session_state` contains the same session advanced to the next
/// Object boundary. Every failure before raw motion leaves the original state
/// available to the owner; a write-path failure after reattachment consumes it
/// so the owner fences the uncertain session.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_batched_parity_to_selected_tape_after_replay_check(
    state: &CatalogIndex,
    raw: &mut dyn RawTapeSink,
    journal: &mut dyn TapeFileJournal,
    session_state: &mut Option<ParitySinkSessionState>,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    io_memory: &Arc<crate::io_memory::IoMemoryReservation>,
    raw_write_attempted: &mut bool,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    ensure_selected_tape_accepts_session_write(state, pool_cfg, &selected)?;
    if matches!(selected.parity_config, ParityConfig::None) {
        return Err(PoolWriteError::InvalidInput(
            "parity session append requested for a parity-off tape".to_string(),
        ));
    }
    let prepared = prepare_pool_object(&request, selected.block_size)?;
    if let Some(expected) = request.expected_content_sha256 {
        if prepared.content_sha256 != expected {
            return Err(PoolWriteError::ContentHashMismatch {
                expected: bytes_to_hex(&expected),
                actual: bytes_to_hex(&prepared.content_sha256),
            });
        }
    }
    let stored = prepare_stored_object(&prepared, &request.representation)?;
    let capacity_blocks = parity_capacity_basis_blocks(state, pool_cfg, &selected)?;
    let projected_object_blocks = match &stored {
        PreparedStoredObject::Plaintext | PreparedStoredObject::CanonicalPlaintext => {
            prepared.plan.layout.projected_size_blocks
        }
        PreparedStoredObject::Encrypted(encrypted) => encrypted.envelope.stored_size_blocks,
    };
    let mut plaintext_readers = if matches!(&stored, PreparedStoredObject::Plaintext) {
        Some(open_prepared_readers(&prepared)?)
    } else {
        None
    };
    let detached = session_state.as_ref().ok_or_else(|| {
        PoolWriteError::InvalidInput("parity sink session state is unavailable".to_string())
    })?;
    // The detached session is the live, exclusive append authority between
    // shared barriers.  In particular, a fresh session contains the sole BOT
    // Bootstrap before the first checkpoint marker exists, so the ordinary
    // bounded-prefix loader must not be asked to interpret that intentional
    // one-bundle suffix.  Resumed sessions were themselves constructed from a
    // bounded snapshot, and reattachment below checks any currently available
    // bounded authority against this carried state.
    let runtime = detached.terminal_triple_capacity_runtime_state()?;
    let structural_entries = runtime.structural_entries_before_object;
    let object_rows = runtime.object_rows_before_object;
    let capacity = reserve_parity_object_capacity(
        runtime,
        detached.scheme(),
        &selected,
        (pool_cfg, structural_entries, object_rows),
        capacity_blocks,
        projected_object_blocks,
        io_memory,
    )?;
    let (capacity, _spool_permit) = capacity.into_parts();
    let detached = session_state.take().ok_or_else(|| {
        PoolWriteError::InvalidInput("parity sink session state is unavailable".to_string())
    })?;
    let write_attempted = Cell::new(false);
    let mut tracked = CapacityTrackingRawTapeSink::new(raw, &write_attempted);
    let mut parity = match ParitySink::try_from_session_state(&mut tracked, journal, detached) {
        Ok(parity) => parity,
        Err((error, detached)) => {
            *session_state = Some(*detached);
            *raw_write_attempted |= write_attempted.get();
            return Err(error.into());
        }
    };
    let write_report: Result<StreamingObjectWriteReport, PoolWriteError> = match &stored {
        PreparedStoredObject::Plaintext => {
            let readers = plaintext_readers.as_mut().ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "prepared plaintext Object is missing its source readers".to_string(),
                )
            })?;
            write_prepared_object_to_parity_from_readers(
                &mut parity,
                selected.tape_uuid,
                &prepared.options,
                &prepared.files,
                readers,
                capacity,
            )
            .map_err(PoolWriteError::from)
        }
        PreparedStoredObject::CanonicalPlaintext => write_canonical_plaintext_object_to_parity(
            &mut parity,
            selected.tape_uuid,
            &prepared,
            capacity,
        ),
        PreparedStoredObject::Encrypted(encrypted) => write_encrypted_object_to_parity(
            &mut parity,
            selected.tape_uuid,
            &prepared,
            encrypted,
            capacity,
        ),
    };
    let write_report = match write_report {
        Ok(report) => report,
        Err(error) => {
            let attempted = write_attempted.get();
            *raw_write_attempted |= attempted;
            if !attempted {
                parity.rollback_unwritten_object()?;
                *session_state = Some(parity.into_session_state()?);
            }
            return Err(error);
        }
    };
    // From this point onward the Object writer has completed one or more raw
    // commands. Publish that fact before any projection or detach operation
    // can fail so the owner cannot mistake a post-motion error for a safe
    // retry.
    *raw_write_attempted |= write_attempted.get();
    parity_post_write_projection_gate()?;
    let mut checkpoint_projection = checkpoint_projection_for_no_parity_write(
        &selected,
        &prepared,
        &write_report,
        stored.copy_representation(),
    )?;
    checkpoint_projection.copy.first_parity_data_ordinal =
        write_report.catalog.object_copy.first_parity_data_ordinal;
    checkpoint_projection.copy.protected_until_ordinal =
        write_report.catalog.object_copy.protected_until_ordinal;
    checkpoint_projection.fresh_tape = false;
    let mut result = pool_write_result(
        request,
        selected,
        prepared,
        stored.copy_representation(),
        write_report,
        AppendCommitDiagnostics::default(),
        false,
        Some(checkpoint_projection),
    )?;
    let detached = parity.into_session_state()?;
    result.hardware_early_warning |= detached.hardware_early_warning_seen();
    *session_state = Some(detached);
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_tape_with_live_counter_impl(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
    check_replay: bool,
    durability: PoolWriteDurability,
) -> Result<PoolWriteResult, PoolWriteError> {
    match live_write_counter {
        Some(counter) => {
            let mut live_counted_sink =
                LiveCounterBlockSink::new(sink, counter, selected.block_size);
            write_to_selected_tape_inner(
                state,
                &mut live_counted_sink,
                pool_cfg,
                request,
                selected,
                check_replay,
                durability,
            )
        }
        None => write_to_selected_tape_inner(
            state,
            sink,
            pool_cfg,
            request,
            selected,
            check_replay,
            durability,
        ),
    }
}

pub(super) fn write_to_selected_tape_inner<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    sink: &mut S,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    check_replay: bool,
    durability: PoolWriteDurability,
) -> Result<PoolWriteResult, PoolWriteError> {
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if check_replay {
        if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
            return Ok(result);
        }
    }
    ensure_selected_tape_accepts_write(state, pool_cfg, &selected)?;
    let block_size = selected.block_size;
    let prepare_started = Instant::now();
    let prepared = prepare_pool_object(&request, block_size)?;
    if let Some(expected) = request.expected_content_sha256 {
        if prepared.content_sha256 != expected {
            return Err(PoolWriteError::ContentHashMismatch {
                expected: bytes_to_hex(&expected),
                actual: bytes_to_hex(&prepared.content_sha256),
            });
        }
    }
    let stored = prepare_stored_object(&prepared, &request.representation)?;
    let stored_projected_blocks = stored.projected_size_blocks(&prepared);
    let mut stored_size_bytes = stored_footprint_bytes(&stored, &prepared, selected.block_size)?;
    if matches!(&durability, PoolWriteDurability::Batched(_)) {
        stored_size_bytes = stored_size_bytes
            .checked_add(u64::from(selected.block_size).saturating_mul(3))
            .ok_or_else(|| {
                PoolWriteError::InvalidInput(
                    "batched barrier capacity reservation overflows u64".to_string(),
                )
            })?;
    }
    let provisional_used_lba = match &durability {
        #[cfg(test)]
        PoolWriteDurability::PerObject => None,
        PoolWriteDurability::Batched(context) => Some(context.append.expected_append_lba()?),
    };
    if let (ParityConfig::None, PoolWriteDurability::Batched(context)) =
        (&selected.parity_config, &durability)
    {
        ensure_no_parity_terminal_close_capacity(
            state,
            pool_cfg,
            &selected,
            context,
            stored_projected_blocks,
        )?;
    }
    ensure_selected_tape_has_capacity(
        state,
        pool_cfg,
        &selected,
        stored_size_bytes,
        provisional_used_lba,
    )?;
    let prepare_elapsed = prepare_started.elapsed();
    let payload_bytes = prepared_payload_bytes(&prepared);
    tracing::info!(
        target: "remanence_write_diag",
        phase = "prepare",
        pool_id = %selected.pool_id,
        tape_uuid = %uuid_text(selected.tape_uuid),
        parity = parity_label(&selected.parity_config),
        representation = stored.representation_label(),
        payload_bytes,
        selected_block_size_bytes = selected.block_size,
        projected_object_blocks = stored_projected_blocks,
        elapsed_ms = crate::diagnostics::duration_ms(prepare_elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, prepare_elapsed),
        "remanence_write_diag",
    );

    // Only the hardware-backed tape transfer below is counted live. The spool
    // write already finished in mount.rs, and parity/object replay only reads
    // the prepared in-memory object.
    let mut counted_sink = CountingBlockSink::new(sink, selected.block_size);
    let prepared_write = PreparedPoolWrite { prepared, stored };
    match selected.parity_config.clone() {
        ParityConfig::Scheme(scheme) => {
            #[cfg(test)]
            {
                write_parity_object_to_selected_tape(
                    state,
                    &mut counted_sink,
                    pool_cfg,
                    request,
                    selected,
                    prepared_write,
                    scheme,
                )
            }
            #[cfg(not(test))]
            {
                let _ = (scheme, prepared_write);
                Err(PoolWriteError::InvalidInput(
                    "the legacy per-object parity path is test-only; use the checkpointed batch core"
                        .to_string(),
                ))
            }
        }
        ParityConfig::None => write_no_parity_object_to_selected_tape(
            state,
            &mut counted_sink,
            pool_cfg,
            request,
            selected,
            prepared_write,
            durability,
        ),
    }
}

/// Verify that the block at BOT is a bootstrap for the expected tape UUID.
///
/// The helper uses only the generic [`BlockSource`] surface so tests can run
/// against [`remanence_library::VecBlockSource`]. It leaves the source
/// positioned immediately after the bootstrap block on success.
pub fn verify_tape_identity(
    source: &mut dyn BlockSource,
    expected_tape_uuid: &[u8; 16],
) -> Result<(), TapeIdentityError> {
    source
        .locate(0)
        .map_err(|err| TapeIdentityError::AbsentBootstrap(format!("locate BOT: {err}")))?;
    let mut block = vec![0u8; VERIFY_BOOTSTRAP_READ_BYTES];
    let read = source
        .read_block(&mut block)
        .map_err(|err| TapeIdentityError::AbsentBootstrap(format!("read BOT: {err}")))?;
    let payload = parse_bootstrap_block(&block[..read])
        .map_err(|err| TapeIdentityError::AbsentBootstrap(err.to_string()))?;
    if &payload.tape_uuid != expected_tape_uuid {
        return Err(TapeIdentityError::Mismatch {
            expected: uuid_text(*expected_tape_uuid),
            actual: uuid_text(payload.tape_uuid),
        });
    }
    Ok(())
}

/// Build the bootstrap payload for a newly provisioned tape.
pub fn build_tape_bootstrap(
    tape_uuid: TapeUuid,
    block_size: u32,
    parity: ParityConfig,
    written_at: impl Into<String>,
    written_by_version: impl Into<String>,
) -> BootstrapPayload {
    match parity {
        ParityConfig::None => BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: written_by_version.into(),
            written_at: written_at.into(),
            sequence: 0,
            block_size_bytes: block_size,
            drive_compression: false,
        },
        ParityConfig::Scheme(scheme) => BootstrapPayload {
            scheme: Some(ParitySchemeRecord {
                id: scheme.id.as_str().to_string(),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(
                sole_bot_filemark_map_digest()
                    .expect("canonical sole-BOT filemark map is structurally valid"),
            ),
            tape_uuid,
            written_by_version: written_by_version.into(),
            written_at: written_at.into(),
            sequence: 0,
            block_size_bytes: block_size,
            drive_compression: false,
        },
    }
}

/// Write one bootstrap tape file through a generic block sink.
pub fn write_tape_bootstrap(
    sink: &mut dyn BlockSink,
    payload: &BootstrapPayload,
) -> Result<(), PoolWriteError> {
    let mut block = vec![0u8; payload.block_size_bytes as usize];
    write_bootstrap_block(payload, &mut block)?;
    sink.write_block(&block)?;
    sink.write_filemarks(1)?;
    Ok(())
}
