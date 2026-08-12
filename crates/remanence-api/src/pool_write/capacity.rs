//! Pool geometry, tape selection inputs, watermark admission, and close-capacity proofs.

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::Path;
use std::sync::Arc;

use remanence_parity::{
    CapacityReserveCause, ParityConfig, ParityError, ParityScheme, SchemeId,
    TerminalTripleCapacityRuntimeState, TerminalTripleCloseInput,
};
use remanence_state::{
    effective_tape_pool_capacity_bytes, validate_tape_pool_capacity_invariant,
    watermark_floor_bytes, CatalogIndex, TapePoolConfig, TapeRecord,
};
use remanence_stream::StreamingObjectWriteReport;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::pool_selection::{
    capacity_admission_disposition, AdmissionDisposition, CapacityAdmissionInput, TapeFitState,
};

use super::media::{lto_generation_from_voltag, raw_capacity_bytes};
use super::model::seal_decision_after_write;
use super::model::{
    BatchedAppendPosition, BatchedNoParityAppendContext, NoParityAppendContext,
    ParityCapacityReservation, PoolWriteError, PoolWriteResult, SelectTapeError, SelectedTape,
    TapePositionAfterWrite, TapeSealReason, TapeUuid, WritabilityError, WriteObjectToPoolRequest,
};
use super::{
    HASH_BUFFER_BYTES, NO_PARITY_BOOTSTRAP_BLOCKS, PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS,
    TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS, UNPOOLED_TERMINAL_HIGH_WATERMARK_BLOCKS,
    UNPOOLED_TERMINAL_LOW_WATERMARK_BLOCKS,
};

pub(crate) fn validate_scheme_columns(tape: &TapeRecord) -> Result<(), WritabilityError> {
    match (
        tape.scheme_id.as_deref(),
        tape.data_blocks_per_stripe,
        tape.parity_blocks_per_stripe,
        tape.stripes_per_neighborhood,
    ) {
        (None, None, None, None) => Ok(()),
        (Some(scheme_id), Some(data), Some(parity), Some(stripes)) => {
            let scheme = ParityScheme {
                id: SchemeId::new_owned(scheme_id.to_string()),
                data_blocks_per_stripe: u16::try_from(data)
                    .map_err(|_| missing_geometry("data_blocks_per_stripe overflows u16"))?,
                parity_blocks_per_stripe: u16::try_from(parity)
                    .map_err(|_| missing_geometry("parity_blocks_per_stripe overflows u16"))?,
                stripes_per_neighborhood: stripes,
            };
            scheme
                .validate()
                .map_err(|err| missing_geometry(format!("invalid parity scheme: {err}")))?;
            Ok(())
        }
        _ => Err(missing_geometry(
            "parity scheme columns must be either all present or all null",
        )),
    }
}

pub(crate) fn validate_pool_capacity_invariant_for_tapes(
    pool_cfg: &TapePoolConfig,
    tapes: &[TapeRecord],
) -> Result<(), SelectTapeError> {
    // Pools may contain mixed LTO generations. The invariant is guaranteed
    // against the smallest known cartridge capacity so every known member has
    // at least the configured low/high band width. If no member capacity is
    // known yet, candidate projection will reject unknown media at first write.
    if let Some(capacity_bytes) = tapes
        .iter()
        .filter_map(|tape| {
            tape.voltag
                .as_deref()
                .and_then(lto_generation_from_voltag)
                .map(raw_capacity_bytes)
        })
        .min()
    {
        validate_tape_pool_capacity_invariant(pool_cfg, capacity_bytes)?;
    }
    Ok(())
}

pub(crate) fn tape_fit_state_from_record(
    tape: &TapeRecord,
    pool_cfg: &TapePoolConfig,
    pool_id: &str,
    barcode_order: u64,
) -> Result<TapeFitState, WritabilityError> {
    let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id)
        .map_err(|err| missing_geometry(err.to_string()))?;
    let raw_capacity = tape_capacity_bytes(tape)?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| missing_geometry(error.to_string()))?;
    let block_size = tape_block_size(tape)?;
    let used_bytes = tape_physical_used_bytes(tape, block_size)?;
    let usable_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_high)
        .map_err(|err| missing_geometry(err.to_string()))?;
    let low_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_low)
        .map_err(|err| missing_geometry(err.to_string()))?;

    Ok(TapeFitState {
        tape_uuid,
        barcode_order,
        // TODO(2b): project drive occupancy from resolve_load_target/session state.
        already_loaded: false,
        used_bytes,
        usable_bytes,
        low_bytes,
    })
}

pub(crate) fn tape_capacity_bytes(tape: &TapeRecord) -> Result<u64, WritabilityError> {
    let voltag = tape
        .voltag
        .as_deref()
        .ok_or_else(|| missing_geometry("voltag is null"))?;
    let generation = lto_generation_from_voltag(voltag)
        .ok_or_else(|| missing_geometry("voltag does not end in a known LTO suffix"))?;
    Ok(raw_capacity_bytes(generation))
}

pub(crate) fn tape_block_size(tape: &TapeRecord) -> Result<u64, WritabilityError> {
    let block_size = tape
        .block_size
        .ok_or_else(|| missing_geometry("block_size is null"))?;
    if block_size == 0 {
        return Err(missing_geometry("block_size is zero"));
    }
    Ok(block_size)
}

pub(crate) fn tape_physical_used_bytes(
    tape: &TapeRecord,
    block_size: u64,
) -> Result<u64, WritabilityError> {
    let used_lba = tape_physical_used_blocks(tape)?;
    used_lba
        .checked_mul(block_size)
        .ok_or_else(|| missing_geometry("physical used capacity overflows u64"))
}

pub(crate) fn tape_physical_used_blocks(tape: &TapeRecord) -> Result<u64, WritabilityError> {
    if let Some(lba) = tape.written_extent_lba {
        return Ok(lba);
    }
    let Some(last_tape_file) = tape.last_committed_tape_file else {
        if tape.total_committed_ordinals == 0 {
            return Ok(0);
        }
        // Compatibility projection for pre-checkpoint no-parity catalog rows:
        // each Object contains at least one ordinal, so at most one Object
        // filemark exists per ordinal, plus the initial one-block Bootstrap
        // and its filemark. This is deliberately an upper bound; live parity
        // admission never relies on it.
        return tape
            .total_committed_ordinals
            .checked_mul(2)
            .and_then(|blocks| blocks.checked_add(2))
            .ok_or_else(|| missing_geometry("legacy physical extent estimate overflows u64"));
    };
    let tape_file_count = last_tape_file
        .checked_add(1)
        .ok_or_else(|| missing_geometry("legacy tape-file count overflows u64"))?;
    // A dense legacy no-parity prefix contains the Object blocks plus at most
    // one single-block control body and one filemark per tape file. Parity
    // prefixes with Object data are rejected before this coarse selector and
    // are admitted only through checkpoint/session position proof.
    tape.total_committed_ordinals
        .checked_add(
            tape_file_count
                .checked_mul(2)
                .ok_or_else(|| missing_geometry("legacy control estimate overflows u64"))?,
        )
        .ok_or_else(|| missing_geometry("legacy physical extent estimate overflows u64"))
}

pub(crate) fn ensure_request_pool_matches_config(
    request: &WriteObjectToPoolRequest,
    pool_cfg: &TapePoolConfig,
) -> Result<(), PoolWriteError> {
    if request.pool_id.trim() == pool_cfg.id.trim() {
        Ok(())
    } else {
        Err(PoolWriteError::InvalidInput(format!(
            "request pool_id {} does not match pool config id {}",
            request.pool_id.trim(),
            pool_cfg.id.trim()
        )))
    }
}

pub(crate) fn ensure_selected_tape_accepts_write(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<(), PoolWriteError> {
    ensure_selected_tape_accepts_write_inner(state, pool_cfg, selected, false)
}

pub(crate) fn ensure_selected_tape_accepts_session_write(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<(), PoolWriteError> {
    ensure_selected_tape_accepts_write_inner(state, pool_cfg, selected, true)
}

/// An empty checkpoint authority is admissible only for catalog-fresh media.
/// This check belongs before any load, position, or write preparation because
/// treating a known written prefix as fresh could rewrite BOT.
pub(crate) fn ensure_empty_checkpoint_matches_catalog_freshness(
    state: &CatalogIndex,
    selected: &SelectedTape,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
) -> Result<(), PoolWriteError> {
    if !checkpoints.is_empty() {
        return Ok(());
    }
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.total_committed_ordinals != 0
        || tape.last_committed_tape_file.is_some()
        || tape.written_extent_lba.is_some()
    {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "checkpoint journal is empty but catalog records a written tape prefix; physical-tail reconciliation is required before append (total_committed_ordinals={}, last_committed_tape_file={:?}, written_extent_lba={:?})",
            tape.total_committed_ordinals,
            tape.last_committed_tape_file,
            tape.written_extent_lba,
        )));
    }
    Ok(())
}

pub(crate) fn ensure_selected_tape_accepts_write_inner(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    session_has_resume_authority: bool,
) -> Result<(), PoolWriteError> {
    let tape = ensure_selected_tape_binding(state, pool_cfg, selected)?;
    if tape.state != "ready" {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape is not writable in state {}",
            tape.state
        )));
    }
    let conflicts =
        state.tape_io_admission_conflicts(&selected.tape_uuid, tape.voltag.as_deref())?;
    if let Some(conflict) = conflicts.first() {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape is blocked by active tape-I/O fence {}: {}",
            conflict.quarantine_id, conflict.reason
        )));
    }
    let tape_block_size = u64::from(selected.block_size);
    if tape_block_size != pool_cfg.block_size_bytes {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape block size {tape_block_size} does not match pool configured block size {}",
            pool_cfg.block_size_bytes
        )));
    }
    if tape.total_committed_ordinals > 0 {
        return match selected.parity_config {
            ParityConfig::None => Ok(()),
            ParityConfig::Scheme(_) if session_has_resume_authority => Ok(()),
            ParityConfig::Scheme(_) => Err(PoolWriteError::ParityAppendUnsupported {
                tape_uuid: uuid_text(selected.tape_uuid),
                total_committed_ordinals: tape.total_committed_ordinals,
            }),
        };
    }
    Ok(())
}

pub(crate) fn ensure_selected_tape_binding(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<TapeRecord, PoolWriteError> {
    if selected.pool_id.trim() != pool_cfg.id.trim() {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected pool_id {} does not match pool config id {}",
            selected.pool_id.trim(),
            pool_cfg.id.trim()
        )));
    }
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.kind != "data" {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape kind {} is not data",
            tape.kind
        )));
    }
    let actual_pool_id = tape
        .pool_id
        .as_deref()
        .map(str::trim)
        .filter(|pool_id| !pool_id.is_empty());
    if actual_pool_id != Some(pool_cfg.id.trim()) {
        return Err(PoolWriteError::InvalidInput(format!(
            "selected tape catalog pool {:?} does not match required pool {}",
            actual_pool_id,
            pool_cfg.id.trim()
        )));
    }
    let (catalog_block_size, catalog_parity) = selected_tape_geometry(&tape, &pool_cfg.id)
        .map_err(|error| PoolWriteError::MissingTapeGeometry(error.to_string()))?;
    if selected.block_size != catalog_block_size {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "selected block size {} does not match catalog tape block_size {catalog_block_size}",
            selected.block_size
        )));
    }
    if selected.parity_config != catalog_parity {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "selected parity geometry {:?} does not match catalog parity geometry {catalog_parity:?}",
            selected.parity_config
        )));
    }
    Ok(tape)
}

#[cfg(test)]
pub(crate) fn no_parity_append_context(
    state: &CatalogIndex,
    selected: &SelectedTape,
) -> Result<NoParityAppendContext, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.scheme_id.is_some() {
        return Err(PoolWriteError::InvalidInput(
            "no-parity append context requested for parity tape".to_string(),
        ));
    }
    let previous_total_committed_ordinals = tape.total_committed_ordinals;
    if previous_total_committed_ordinals > 0 && tape.last_committed_tape_file.is_none() {
        return Err(PoolWriteError::MissingTapeGeometry(
            "no-parity tape has committed ordinals but no last_committed_tape_file".to_string(),
        ));
    }
    let tape_file_number = match tape.last_committed_tape_file {
        Some(last) => last.checked_add(1).ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "next no-parity tape file overflows u64".to_string(),
            )
        })?,
        None => 1,
    };
    Ok(NoParityAppendContext {
        tape_file_number,
        previous_total_committed_ordinals,
        fresh_tape: previous_total_committed_ordinals == 0
            && tape.last_committed_tape_file.is_none(),
        expected_append_lba: None,
    })
}

/// Seed a batched session from the durable checkpoint journal, never SQLite
/// counters. A non-fresh tape without a checkpoint record is not safe to
/// admit because SPACE(EOD) would preserve an uncommitted crash tail.
pub(crate) fn first_batched_append_context(
    state: &CatalogIndex,
    selected: &SelectedTape,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
) -> Result<BatchedNoParityAppendContext, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    if tape.scheme_id.is_some() || !matches!(selected.parity_config, ParityConfig::None) {
        return Err(PoolWriteError::InvalidInput(
            "batched append context requires a parity-off tape".to_string(),
        ));
    }
    match checkpoints.last() {
        Some(checkpoint) => {
            if checkpoint.tape_uuid != selected.tape_uuid {
                return Err(PoolWriteError::MissingTapeGeometry(
                    "checkpoint journal tape UUID does not match selected tape".to_string(),
                ));
            }
            let previous_total_committed_ordinals = checkpoint
                .objects
                .last()
                .map(|object| object.total_committed_ordinals)
                .ok_or_else(|| {
                    PoolWriteError::MissingTapeGeometry(
                        "checkpoint record has no object projection".to_string(),
                    )
                })?;
            let tape_file_number = checkpoint.next_tape_file_number;
            let object_row_count = checkpoint.committed_object_count;
            Ok(BatchedNoParityAppendContext {
                append: NoParityAppendContext {
                    tape_file_number,
                    previous_total_committed_ordinals,
                    fresh_tape: false,
                    expected_append_lba: Some(checkpoint.eod_lba),
                },
                position: BatchedAppendPosition::JournalEod(checkpoint.eod_lba),
                object_row_count,
            })
        }
        None if tape.total_committed_ordinals == 0 && tape.last_committed_tape_file.is_none() => {
            Ok(BatchedNoParityAppendContext {
                append: NoParityAppendContext {
                    tape_file_number: 1,
                    previous_total_committed_ordinals: 0,
                    fresh_tape: true,
                    expected_append_lba: None,
                },
                position: BatchedAppendPosition::FreshTape,
                object_row_count: 0,
            })
        }
        None => Err(PoolWriteError::MissingTapeGeometry(
            "batched append requires a checkpoint journal for a non-fresh tape".to_string(),
        )),
    }
}

/// Advance the session-local append context from the prior object's report.
pub(crate) fn next_batched_append_context(
    previous: &BatchedNoParityAppendContext,
    result: &PoolWriteResult,
) -> Result<BatchedNoParityAppendContext, PoolWriteError> {
    let report = result.write_report().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "cannot derive provisional append context from a replay".to_string(),
        )
    })?;
    let tape_file_number = report
        .catalog
        .object_copy
        .tape_file_number
        .checked_add(1)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "next provisional tape-file number overflows u64".to_string(),
            )
        })?;
    let expected_lba = report.object_close.filemark_outcome.position_after.lba;
    result.checkpoint_projection().ok_or_else(|| {
        PoolWriteError::InvalidInput(
            "batched append result is missing checkpoint projection".to_string(),
        )
    })?;
    let object_row_count = previous.object_row_count.checked_add(1).ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("checkpoint Object-row count overflows u64".to_string())
    })?;
    Ok(BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number,
            previous_total_committed_ordinals: report
                .catalog
                .tape_file_bundle
                .total_committed_ordinals,
            fresh_tape: false,
            expected_append_lba: Some(expected_lba),
        },
        position: BatchedAppendPosition::CurrentBoundary(expected_lba),
        object_row_count,
    })
}

/// Re-anchor an accumulated session-local context at a durable checkpoint.
pub(crate) fn batched_append_context_after_checkpoint(
    previous: &BatchedNoParityAppendContext,
    record: &remanence_state::CheckpointJournalRecord,
) -> Result<BatchedNoParityAppendContext, PoolWriteError> {
    let previous_total_committed_ordinals = record
        .objects
        .last()
        .map(|object| object.total_committed_ordinals)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "checkpoint record has no final object projection".to_string(),
            )
        })?;
    let tape_file_number = record.next_tape_file_number;
    Ok(BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number,
            previous_total_committed_ordinals,
            fresh_tape: false,
            expected_append_lba: Some(record.eod_lba),
        },
        position: BatchedAppendPosition::CurrentBoundary(record.eod_lba),
        object_row_count: previous.object_row_count,
    })
}

pub(crate) fn ensure_selected_tape_has_capacity(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    object_size: u64,
    provisional_used_lba: Option<u64>,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let catalog_used_lba = tape_physical_used_blocks(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let used_lba = provisional_used_lba.unwrap_or(catalog_used_lba);
    if used_lba < catalog_used_lba {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "provisional physical extent {used_lba} precedes catalog physical extent or conservative estimate {catalog_used_lba}"
        )));
    }
    let used = used_lba.checked_mul(block_size).ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("physical used capacity overflows u64".to_string())
    })?;
    if used > capacity || object_size > capacity - used {
        return Err(PoolWriteError::SelectedTapeInsufficientCapacity {
            object_size,
            raw_capacity: capacity,
            used,
        });
    }
    Ok(())
}

pub(crate) fn ensure_no_parity_terminal_close_capacity(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
    context: &BatchedNoParityAppendContext,
    projected_object_blocks: u64,
) -> Result<(), PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity_bytes = tape_capacity_bytes(&tape)
        .map_err(|error| PoolWriteError::MissingTapeGeometry(error.to_string()))?;
    let capacity_bytes = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity_bytes)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let capacity_blocks = capacity_bytes / u64::from(selected.block_size);
    let fresh_prefix_blocks = if context.append.fresh_tape {
        NO_PARITY_BOOTSTRAP_BLOCKS + 1
    } else {
        0
    };
    let current_used_blocks = context
        .append
        .expected_append_lba()?
        .checked_add(fresh_prefix_blocks)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(
                "no-parity physical cursor plus fresh prefix overflows u64".to_string(),
            )
        })?;
    if current_used_blocks > capacity_blocks {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "no-parity physical cursor {current_used_blocks} exceeds capacity basis {capacity_blocks}"
        )));
    }
    let structural_entries_before_object = context.append.tape_file_number;
    let object_rows_before_object = context.object_row_count;
    let (low_watermark_blocks, high_watermark_blocks) =
        terminal_watermark_blocks(capacity_blocks, Some(pool_cfg))?;
    let exact_input = TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks,
        block_size_bytes: selected.block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 1,
        parity_shards_per_epoch: 0,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object: 0,
        structural_entries_before_object,
        object_rows_before_object,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS,
        remaining_tape_blocks: capacity_blocks - current_used_blocks,
        capacity_basis_blocks: capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 0,
    };
    let exact = match exact_input.evaluate() {
        Ok(report) => report,
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            reserve_blocks: Some(required_reserve_blocks),
            ..
        }) if context.append.fresh_tape => {
            return Err(ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks,
                empty_tape_usable_blocks: capacity_blocks
                    .saturating_sub(NO_PARITY_BOOTSTRAP_BLOCKS + 1),
                required_reserve_blocks,
            }
            .into());
        }
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            ..
        }) => {
            return Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "no-parity exact close reserve requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            });
        }
        Err(error) => return Err(error.into()),
    };
    match capacity_admission_disposition(CapacityAdmissionInput {
        current_used_blocks,
        object_commit_charge_blocks: exact.prefix_commit_charge_blocks,
        close_bound_blocks: exact.close_bound_blocks,
        capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
    }) {
        AdmissionDisposition::AdmitRemainOpen | AdmissionDisposition::AdmitThenFinalize => Ok(()),
        AdmissionDisposition::FinalizePrefixAndRetry => {
            Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "no-parity exact close reserve requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            })
        }
        AdmissionDisposition::RejectInvalidCapacityPolicy => {
            Err(PoolWriteError::InvalidInput(format!(
                "invalid physical capacity policy: low={low_watermark_blocks}, high={high_watermark_blocks}, capacity={capacity_blocks} blocks"
            )))
        }
    }
}

#[cfg(test)]
pub(crate) fn seal_selected_tape_if_needed(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    hardware_early_warning: bool,
) -> Result<bool, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let used_bytes = tape
        .total_committed_ordinals
        .checked_mul(block_size)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry("used capacity overflows u64".to_string())
        })?;
    let low_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_low)?;
    if seal_decision_after_write(
        TapePositionAfterWrite {
            used_bytes,
            early_warning: hardware_early_warning,
        },
        low_bytes,
        None,
    )
    .is_some()
    {
        state.seal_tape(selected.tape_uuid)?;
        return Ok(true);
    }
    Ok(false)
}

/// Evaluate tape sealing at a shared checkpoint boundary without projecting it.
///
/// Callers that receive a seal reason must finalize terminal media, append its
/// terminal-only checkpoint authority, and then project that record.
pub(crate) fn selected_tape_seal_reason_at_barrier(
    state: &CatalogIndex,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    position: TapePositionAfterWrite,
) -> Result<Option<TapeSealReason>, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity = effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity)
        .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?;
    let low_bytes = watermark_floor_bytes(capacity, pool_cfg.watermark_low)?;
    Ok(seal_decision_after_write(position, low_bytes, None))
}

pub(crate) fn missing_geometry(reason: impl Into<String>) -> WritabilityError {
    WritabilityError::MissingGeometry {
        reason: reason.into(),
    }
}

pub(crate) fn selected_tape_from_record(
    tape: TapeRecord,
    pool_id: &str,
) -> Result<SelectedTape, SelectTapeError> {
    let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id)?;
    let (block_size, parity_config) = selected_tape_geometry(&tape, pool_id)?;
    Ok(SelectedTape {
        pool_id: pool_id.to_string(),
        tape_uuid,
        block_size,
        parity_config,
    })
}

pub(crate) fn compare_tapes_for_pool_selection(
    left: &TapeRecord,
    right: &TapeRecord,
) -> std::cmp::Ordering {
    left.voltag
        .as_deref()
        .unwrap_or("")
        .cmp(right.voltag.as_deref().unwrap_or(""))
        .then_with(|| left.tape_uuid.cmp(&right.tape_uuid))
}

pub(crate) fn tape_uuid_from_vec(
    value: Vec<u8>,
    pool_id: &str,
) -> Result<TapeUuid, SelectTapeError> {
    value
        .try_into()
        .map_err(|value: Vec<u8>| SelectTapeError::InvalidTapeUuid {
            pool_id: pool_id.to_string(),
            actual_len: value.len(),
        })
}

pub(crate) fn selected_tape_geometry(
    tape: &TapeRecord,
    pool_id: &str,
) -> Result<(u32, ParityConfig), SelectTapeError> {
    let block_size = tape
        .block_size
        .ok_or_else(|| invalid_geometry(pool_id, "block_size is null"))
        .and_then(|value| {
            u32::try_from(value).map_err(|_| invalid_geometry(pool_id, "block_size overflows u32"))
        })?;
    let (scheme_id, data_blocks_per_stripe, parity_blocks_per_stripe, stripes_per_neighborhood) =
        match (
            tape.scheme_id.clone(),
            tape.data_blocks_per_stripe,
            tape.parity_blocks_per_stripe,
            tape.stripes_per_neighborhood,
        ) {
            (None, None, None, None) => return Ok((block_size, ParityConfig::None)),
            (Some(scheme_id), Some(data), Some(parity), Some(stripes)) => (
                scheme_id,
                u16::try_from(data).map_err(|_| {
                    invalid_geometry(pool_id, "data_blocks_per_stripe overflows u16")
                })?,
                u16::try_from(parity).map_err(|_| {
                    invalid_geometry(pool_id, "parity_blocks_per_stripe overflows u16")
                })?,
                stripes,
            ),
            _ => {
                return Err(invalid_geometry(
                    pool_id,
                    "parity scheme columns must be either all present or all null",
                ));
            }
        };
    let scheme = ParityScheme {
        id: SchemeId::new_owned(scheme_id),
        data_blocks_per_stripe,
        parity_blocks_per_stripe,
        stripes_per_neighborhood,
    };
    scheme
        .validate()
        .map_err(|err| invalid_geometry(pool_id, err.to_string()))?;
    Ok((block_size, ParityConfig::Scheme(scheme)))
}

pub(crate) fn invalid_geometry(pool_id: &str, reason: impl Into<String>) -> SelectTapeError {
    SelectTapeError::InvalidTapeGeometry {
        pool_id: pool_id.to_string(),
        reason: reason.into(),
    }
}

pub(crate) fn first_payload_body_lba(report: &StreamingObjectWriteReport) -> u64 {
    report
        .catalog
        .files
        .iter()
        .filter_map(|file| file.first_chunk_lba.map(|lba| lba.0))
        .min()
        .unwrap_or(0)
}

pub(crate) fn source_file_size(path: &Path) -> Result<u64, PoolWriteError> {
    let metadata = fs::metadata(path).map_err(|source| PoolWriteError::Io {
        context: "stat source file",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_file() {
        return Err(PoolWriteError::InvalidInput(format!(
            "source path is not a regular file: {}",
            path.display()
        )));
    }
    Ok(metadata.len())
}

pub(crate) fn sha256_file(path: &Path) -> Result<[u8; 32], PoolWriteError> {
    let file = File::open(path).map_err(|source| PoolWriteError::Io {
        context: "open source file for hashing",
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::with_capacity(HASH_BUFFER_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = reader.read(&mut buf).map_err(|source| PoolWriteError::Io {
            context: "read source file for hashing",
            path: path.to_path_buf(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

pub(crate) fn parity_capacity_basis_blocks(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    selected: &SelectedTape,
) -> Result<u64, PoolWriteError> {
    terminal_capacity_basis_blocks(state, Some(pool_cfg), selected)
}

pub(crate) fn terminal_capacity_basis_blocks(
    state: &CatalogIndex,
    pool_cfg: Option<&TapePoolConfig>,
    selected: &SelectedTape,
) -> Result<u64, PoolWriteError> {
    let tape = state.get_tape(&selected.tape_uuid)?.ok_or_else(|| {
        PoolWriteError::MissingTapeGeometry("selected tape row is missing".into())
    })?;
    let raw_capacity_bytes = tape_capacity_bytes(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    let capacity_bytes = match pool_cfg {
        Some(pool_cfg) => effective_tape_pool_capacity_bytes(pool_cfg, raw_capacity_bytes)
            .map_err(|error| PoolWriteError::InvalidInput(error.to_string()))?,
        None => raw_capacity_bytes,
    };
    let catalog_block_size = tape_block_size(&tape)
        .map_err(|err| PoolWriteError::MissingTapeGeometry(err.to_string()))?;
    if catalog_block_size != u64::from(selected.block_size) {
        return Err(PoolWriteError::MissingTapeGeometry(format!(
            "selected block size {} does not match catalog tape block_size {catalog_block_size}",
            selected.block_size
        )));
    }
    let capacity_blocks = capacity_bytes / catalog_block_size;
    if capacity_blocks == 0 {
        return Err(PoolWriteError::MissingTapeGeometry(
            "tape capacity basis is smaller than one fixed block".to_string(),
        ));
    }
    Ok(capacity_blocks)
}

pub(crate) fn terminal_watermark_blocks(
    capacity_blocks: u64,
    pool_cfg: Option<&TapePoolConfig>,
) -> Result<(u64, u64), PoolWriteError> {
    match pool_cfg {
        Some(pool_cfg) => Ok((
            watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)?,
            watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_high)?,
        )),
        None => Ok((
            UNPOOLED_TERMINAL_LOW_WATERMARK_BLOCKS,
            UNPOOLED_TERMINAL_HIGH_WATERMARK_BLOCKS,
        )),
    }
}

pub(crate) fn reserve_parity_object_capacity(
    runtime: TerminalTripleCapacityRuntimeState,
    scheme: &ParityScheme,
    selected: &SelectedTape,
    terminal_authority: (&TapePoolConfig, u64, u64),
    capacity_blocks: u64,
    projected_object_blocks: u64,
    io_memory: &Arc<crate::io_memory::IoMemoryReservation>,
) -> Result<ParityCapacityReservation, PoolWriteError> {
    let remaining_tape_blocks = capacity_blocks
        .checked_sub(runtime.used_tape_blocks)
        .ok_or_else(|| {
            PoolWriteError::MissingTapeGeometry(format!(
                "physical tape position {} exceeds capacity basis {capacity_blocks}",
                runtime.used_tape_blocks
            ))
        })?;
    let data_shards_per_epoch = u64::from(scheme.data_blocks_per_stripe)
        .checked_mul(u64::from(scheme.stripes_per_neighborhood))
        .ok_or(ParityError::Invariant(
            "capacity reserve data-shard count overflows",
        ))?;
    let parity_shards_per_epoch = u64::from(scheme.parity_blocks_per_stripe)
        .checked_mul(u64::from(scheme.stripes_per_neighborhood))
        .ok_or(ParityError::Invariant(
            "capacity reserve parity-shard count overflows",
        ))?;
    let (pool_cfg, structural_entries_before_object, object_rows_before_object) =
        terminal_authority;
    if runtime.structural_entries_before_object != structural_entries_before_object
        || runtime.object_rows_before_object != object_rows_before_object
    {
        return Err(PoolWriteError::InvalidInput(format!(
            "terminal capacity journal/sink authority mismatch: journal S/R={structural_entries_before_object}/{object_rows_before_object}, sink S/R={}/{}",
            runtime.structural_entries_before_object, runtime.object_rows_before_object
        )));
    }
    let (low_watermark_blocks, high_watermark_blocks) =
        terminal_watermark_blocks(capacity_blocks, Some(pool_cfg))?;
    let mut input = TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks,
        block_size_bytes: selected.block_size,
        current_epoch_fill_blocks: runtime.current_epoch_fill_blocks,
        data_shards_per_epoch,
        parity_shards_per_epoch,
        pending_completed_sidecars: runtime.pending_completed_sidecars,
        sidecar_entries_before_object: runtime.sidecar_entries_before_object,
        structural_entries_before_object,
        object_rows_before_object,
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
        pending_completed_epoch_parity_bytes: runtime.pending_completed_epoch_parity_bytes,
        remaining_spool_bytes: u64::MAX,
    };
    let report = match input.evaluate() {
        Ok(report) => report,
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            reserve_blocks: Some(required_reserve_blocks),
            ..
        }) if runtime.used_tape_blocks == PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS => {
            return Err(ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks,
                empty_tape_usable_blocks: capacity_blocks
                    .saturating_sub(PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS),
                required_reserve_blocks,
            }
            .into());
        }
        Err(ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::TapeCapacity,
            ..
        }) => {
            return Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "exact terminal-close admission requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            });
        }
        Err(error) => return Err(error.into()),
    };
    match capacity_admission_disposition(CapacityAdmissionInput {
        current_used_blocks: runtime.used_tape_blocks,
        object_commit_charge_blocks: report.prefix_commit_charge_blocks,
        close_bound_blocks: report.close_bound_blocks,
        capacity_blocks,
        low_watermark_blocks,
        high_watermark_blocks,
    }) {
        AdmissionDisposition::AdmitRemainOpen | AdmissionDisposition::AdmitThenFinalize => {}
        AdmissionDisposition::FinalizePrefixAndRetry => {
            return Err(PoolWriteError::TerminalCloseRequired {
                detail: format!(
                    "exact terminal-close admission requires finalizing the current prefix before writing the proposed {projected_object_blocks}-block Object"
                ),
            });
        }
        AdmissionDisposition::RejectInvalidCapacityPolicy => {
            return Err(PoolWriteError::InvalidInput(format!(
                "invalid physical capacity policy: low={low_watermark_blocks}, high={high_watermark_blocks}, capacity={capacity_blocks} blocks"
            )));
        }
    }
    let required_spool_bytes = report.required_spool_bytes;
    let spool_permit = io_memory
        .try_reserve_with_available(required_spool_bytes)
        .map_err(|available| ParityError::CapacityReserveExceeded {
            cause: CapacityReserveCause::ParitySpoolCapacity,
            projected_object_blocks,
            remaining_blocks: None,
            reserve_blocks: None,
            remaining_spool_bytes: Some(available),
            required_spool_bytes: Some(required_spool_bytes),
        })?;
    input.remaining_spool_bytes = required_spool_bytes;
    let reservation = input.reserve_object()?;
    debug_assert_eq!(reservation.report(), &report);
    Ok(ParityCapacityReservation {
        reservation,
        _spool_permit: spool_permit,
    })
}

pub(crate) fn now_rfc3339() -> Result<String, PoolWriteError> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

pub(crate) fn uuid_text(value: [u8; 16]) -> String {
    Uuid::from_bytes(value).to_string()
}
