//! Shared object-append primitives used by the drive actor.

use super::capacity::{
    ensure_no_parity_terminal_close_capacity, ensure_request_pool_matches_config,
    ensure_selected_tape_accepts_session_write, ensure_selected_tape_accepts_write,
    ensure_selected_tape_has_capacity, parity_capacity_basis_blocks,
    reserve_parity_object_capacity, uuid_text,
};
use super::model::require_rewritable_object_media;
use super::model::{
    parity_post_write_projection_gate, AppendCommitDiagnostics, BatchedNoParityAppendContext,
    CapacityTrackingRawTapeSink, PoolWriteDurability, PoolWriteError, PoolWriteResult,
    SelectedTape, WriteObjectToPoolRequest,
};
use super::no_parity::{
    checkpoint_projection_for_no_parity_write, maybe_replay_pool_write, pool_write_result,
    write_no_parity_object_to_selected_tape,
};
#[cfg(test)]
use super::overlap::write_parity_object_to_selected_tape;
use super::prepare::{
    open_prepared_readers, parity_label, prepare_pool_object, prepare_stored_object,
    prepared_payload_bytes, stored_footprint_bytes, write_canonical_plaintext_object_to_parity,
    write_encrypted_object_to_parity, PreparedPoolWrite, PreparedStoredObject,
};
use super::staging::{CountingBlockSink, LiveCounterBlockSink};
use crate::bytes_to_hex;
use crate::verify_tape_identity;
use remanence_library::{BlockSink, DriveHandle, DriveHandleSource};
use remanence_parity::{
    ParityConfig, ParitySink, ParitySinkSessionState, RawTapeSink, TapeReportPublisher,
};
use remanence_state::{CatalogIndex, TapePoolConfig};
use remanence_stream::{write_prepared_object_to_parity_from_readers, StreamingObjectWriteReport};
use std::{cell::Cell, sync::Arc, time::Instant};

/// Write one parity-off object into a server-owned provisional batch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_batched_to_selected_tape_after_replay_check(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
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
        fence_cfg,
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
    journal: &mut dyn TapeReportPublisher,
    session_state: &mut Option<ParitySinkSessionState>,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    io_memory: &Arc<crate::io_memory::IoMemoryReservation>,
    raw_write_attempted: &mut bool,
) -> Result<PoolWriteResult, PoolWriteError> {
    let prepare_started = Instant::now();
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
    let prepare_elapsed = prepare_started.elapsed();
    let payload_bytes = prepared_payload_bytes(&prepared);
    tracing::info!(target: "remanence_write_diag", phase = "prepare",
        pool_id = %selected.pool_id, tape_uuid = %uuid_text(selected.tape_uuid),
        object_id = %prepared.object_uuid, caller_object_id = %request.caller_object_id,
        parity = parity_label(&selected.parity_config), representation = stored.representation_label(),
        payload_bytes, selected_block_size_bytes = selected.block_size,
        projected_object_blocks, elapsed_ms = crate::diagnostics::duration_ms(prepare_elapsed),
        throughput_mib_s = crate::diagnostics::mib_per_s(payload_bytes, prepare_elapsed), "remanence_write_diag");
    let transfer_started = Instant::now();
    let stats = Cell::new(super::staging::BlockSinkStats::default());
    let mut timed = super::staging::TimedRawTapeSink {
        inner: raw,
        stats: &stats,
        last_write_finished: None,
    };
    let write_attempted = Cell::new(false);
    let mut tracked = CapacityTrackingRawTapeSink::new(&mut timed, &write_attempted);
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
    let transfer_elapsed = transfer_started.elapsed();
    let error_text = write_report.as_ref().err().map(ToString::to_string);
    super::prepare::log_transfer_diagnostics(
        &request,
        &selected,
        &prepared,
        projected_object_blocks,
        true,
        super::prepare::TransferDiagnosticOutcome {
            stats: stats.get(),
            elapsed: transfer_elapsed,
            status: if error_text.is_some() { "error" } else { "ok" },
            error: error_text.as_deref(),
        },
    );
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
    let _tail = crate::diagnostics::WriteTailTimer::new(
        prepared.object_uuid,
        &selected.pool_id,
        selected.tape_uuid,
        parity_label(&selected.parity_config),
        payload_bytes,
    );
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
    result.transfer_feed_wait = stats.get().between_write_elapsed;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_tape_with_live_counter_impl(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
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
                fence_cfg,
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
            fence_cfg,
            sink,
            pool_cfg,
            request,
            selected,
            check_replay,
            durability,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn write_to_selected_tape_inner<S: BlockSink + ?Sized>(
    state: &mut CatalogIndex,
    fence_cfg: &crate::write_owner::TerminalWriteConfig,
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
        object_id = %prepared.object_uuid,
        caller_object_id = %request.caller_object_id,
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
            fence_cfg,
            &mut counted_sink,
            pool_cfg,
            request,
            selected,
            prepared_write,
            durability,
        ),
    }
}

/// Re-establish readiness, removal lock, BOT identity, mode, and the exact
/// durable append cursor before any post-checkpoint terminal modification.
pub(crate) fn reprove_checkpoint_boundary(
    drive: &mut DriveHandle,
    record: &remanence_state::authority::AuthorityCommit,
) -> Result<(), PoolWriteError> {
    drive.clear_append_admission();
    drive
        .test_unit_ready()
        .map_err(|e| PoolWriteError::InvalidInput(e.to_string()))?;
    drive
        .prevent_medium_removal()
        .map_err(|e| PoolWriteError::InvalidInput(e.to_string()))?;
    drive.rewind()?;
    let generation = drive.identity_generation();
    verify_tape_identity(&mut DriveHandleSource(drive), &record.tape_uuid)?;
    let config = drive.read_config()?;
    require_rewritable_object_media(config)?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, config, record.block_size)?;
    drive.bind_identity_generation(generation)?;
    let position = drive.locate(record.cursor.lba)?;
    if position.partition != record.cursor.partition || position.lba != record.cursor.lba {
        return Err(PoolWriteError::InvalidInput(
            "checkpoint append position mismatch".into(),
        ));
    }
    Ok(())
}
