//! Verification extraction of the v0.4.4 parity capacity-reserve arithmetic.
//!
//! This crate is a standalone, dependency-free model of
//! `crates/remanence-parity/src/capacity.rs`'s pure object-start reserve
//! calculation. It preserves the production arithmetic and branch ordering but
//! replaces the full production `ParityError` payloads with compact proof-facing
//! variants. The `drift_guard` test pins the production formulas this extraction
//! mirrors; if it fails, the extraction and Lean proofs must be re-synced.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityReserveCause {
    TapeCapacity,
    ParitySpoolCapacity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityError {
    BlockSizeZero,
    UnsupportedBlockSize,
    DataShardsPerEpochZero,
    ParityShardsPerEpochZero,
    ProfileNeighborhoodTooLarge,
    CurrentEpochFillOutsideOpenEpoch,
    ObjectRowsExceedStructuralEntries,
    SidecarRowsExceedStructuralEntries,
    RecoveryRowsExceedStructuralEntries,
    StructuralEntriesExceedCapacity,
    UnsafeCapacityProfile,
    CapacityProfileCloseExceedsCapacity,
    SidecarDirectoryExceedsCapacity,
    SidecarEntryDoesNotFit,
    ReplicatedControlHeaderTooLarge,
    ArithmeticOverflow,
    ObjectTooLargeForEmptyTape,
    CapacityReserveExceededTape,
    CapacityReserveExceededSpool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityReserveInput {
    pub projected_object_blocks: u64,
    pub block_size_bytes: u64,
    pub current_epoch_fill_blocks: u64,
    pub data_shards_per_epoch: u64,
    pub parity_shards_per_epoch: u64,
    pub sidecar_index_block_count: u64,
    pub object_filemark_blocks: u64,
    pub sidecar_filemark_blocks: u64,
    pub bootstrap_filemark_blocks: u64,
    pub pending_completed_sidecars: u64,
    pub remaining_bootstrap_count: u64,
    pub safety_margin_blocks: u64,
    pub remaining_tape_blocks: u64,
    pub empty_tape_usable_blocks: u64,
    pub pending_completed_epoch_parity_bytes: u64,
    pub remaining_spool_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeReserveReport {
    pub epochs_completed_by_object: u64,
    pub final_partial_sidecar_needed: bool,
    pub sidecar_tape_file_blocks: u64,
    pub bootstrap_tape_file_blocks: u64,
    pub reserve_after_object_blocks: u64,
    pub required_tape_blocks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityReserveReport {
    pub epochs_completed_by_object: u64,
    pub final_partial_sidecar_needed: bool,
    pub sidecar_tape_file_blocks: u64,
    pub bootstrap_tape_file_blocks: u64,
    pub reserve_after_object_blocks: u64,
    pub required_tape_blocks: u64,
    pub required_spool_bytes: u64,
}

/// Proof-facing inputs for the no-policy-snapshot final-close base kernel.
///
/// Counts describe committed authority before the proposed Object. Sidecar,
/// ParityMap, and snapshot bounds are derived rather than supplied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCloseInput {
    pub projected_object_blocks: u64,
    pub block_size_bytes: u64,
    pub current_epoch_fill_blocks: u64,
    pub data_shards_per_epoch: u64,
    pub parity_shards_per_epoch: u64,
    pub pending_completed_sidecars: u64,
    pub sidecar_entries_before_object: u64,
    pub structural_entries_before_object: u64,
    pub object_rows_before_object: u64,
    pub object_filemark_blocks: u64,
    pub sidecar_filemark_blocks: u64,
    pub parity_map_filemark_blocks: u64,
    pub snapshot_filemark_blocks: u64,
    pub bootstrap_filemark_blocks: u64,
    pub safety_margin_blocks: u64,
    pub remaining_tape_blocks: u64,
    pub empty_tape_usable_blocks: u64,
    pub high_watermark_blocks: u64,
    pub pending_completed_epoch_parity_bytes: u64,
    pub remaining_spool_bytes: u64,
}

/// Checked terms in the draft.4 close guarantee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCloseReport {
    pub epochs_completed_by_object: u64,
    pub final_partial_sidecar_needed: bool,
    pub sidecar_index_block_count: u64,
    pub sidecar_blocks_before_filemark: u64,
    pub sidecar_tape_file_blocks: u64,
    pub sidecars_emitted_by_commit: u64,
    pub sidecar_blocks_emitted_by_commit: u64,
    pub object_tape_file_blocks: u64,
    pub object_commit_charge_blocks: u64,
    pub object_rows_after: u64,
    pub sidecar_entries_after_closeout: u64,
    pub maximum_sidecar_entries_for_capacity: u64,
    pub structural_entries_after_closeout: u64,
    pub final_partial_sidecar_blocks: u64,
    pub final_parity_map_needed: bool,
    pub final_parity_map_directory_bound_bytes: u64,
    pub final_parity_map_payload_bound_bytes: u64,
    pub final_parity_map_blocks_before_filemark: u64,
    pub final_parity_map_tape_file_blocks: u64,
    pub snapshot_payload_bytes: u64,
    pub snapshot_blocks_before_filemark: u64,
    pub snapshot_tape_file_blocks: u64,
    pub final_bootstrap_tape_file_blocks: u64,
    pub safety_margin_blocks: u64,
    pub close_bound_blocks: u64,
    pub required_tape_blocks: u64,
    pub required_spool_bytes: u64,
}

pub fn checked_add(a: u64, b: u64) -> Result<u64, CapacityError> {
    match a.checked_add(b) {
        Some(sum) => Ok(sum),
        None => Err(CapacityError::ArithmeticOverflow),
    }
}

pub fn checked_mul(a: u64, b: u64) -> Result<u64, CapacityError> {
    match a.checked_mul(b) {
        Some(product) => Ok(product),
        None => Err(CapacityError::ArithmeticOverflow),
    }
}

pub fn checked_sub(a: u64, b: u64) -> Result<u64, CapacityError> {
    match a.checked_sub(b) {
        Some(difference) => Ok(difference),
        None => Err(CapacityError::ArithmeticOverflow),
    }
}

pub fn block_count_per_bootstrap() -> u64 {
    1
}

pub fn snapshot_header_bytes() -> u64 {
    512
}

pub fn snapshot_structural_slot_bytes() -> u64 {
    64
}

pub fn snapshot_object_row_slot_bytes() -> u64 {
    256
}

pub fn parity_map_header_bytes() -> u64 {
    184
}

pub fn sidecar_header_bytes() -> u64 {
    184
}

pub fn sidecar_trailing_crc_bytes() -> u64 {
    8
}

pub fn parity_index_entry_bytes() -> u64 {
    16
}

pub fn data_crc_entry_bytes() -> u64 {
    8
}

pub fn parity_map_fixed_bound_bytes() -> u64 {
    325
}

pub fn parity_map_directory_fixed_bound_bytes() -> u64 {
    43
}

pub fn parity_map_directory_entry_bound_bytes() -> u64 {
    116
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexPackingState {
    pub block_count: u64,
    pub remaining_bytes: u64,
    pub inline_entry_bytes: u64,
    pub current_block_is_empty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SidecarIndexCapacityLayout {
    pub block_count: u64,
    pub inline_entry_bytes: u64,
}

pub fn pack_index_segment(
    state: IndexPackingState,
    spill_capacity: u64,
    entry_count: u64,
    entry_len: u64,
) -> Result<IndexPackingState, CapacityError> {
    if entry_count == 0 {
        return Ok(state);
    }
    if entry_len == 0 || spill_capacity / entry_len == 0 {
        return Err(CapacityError::SidecarEntryDoesNotFit);
    }
    let available_entries = state.remaining_bytes / entry_len;
    let entries_here = if entry_count < available_entries {
        entry_count
    } else {
        available_entries
    };
    if entries_here == 0 && state.current_block_is_empty {
        return Err(CapacityError::SidecarEntryDoesNotFit);
    }
    let bytes_here = checked_mul(entries_here, entry_len)?;
    let remaining_count = checked_sub(entry_count, entries_here)?;
    let remaining_bytes = checked_sub(state.remaining_bytes, bytes_here)?;
    let inline_entry_bytes = if state.block_count == 1 {
        checked_add(state.inline_entry_bytes, bytes_here)?
    } else {
        state.inline_entry_bytes
    };
    if remaining_count == 0 {
        return Ok(IndexPackingState {
            block_count: state.block_count,
            remaining_bytes,
            inline_entry_bytes,
            current_block_is_empty: false,
        });
    }

    let entries_per_spill = spill_capacity / entry_len;
    let complete_spill_blocks = remaining_count / entries_per_spill;
    let partial_spill_block = if remaining_count % entries_per_spill == 0 {
        0
    } else {
        1
    };
    let added_blocks = checked_add(complete_spill_blocks, partial_spill_block)?;
    let block_count = checked_add(state.block_count, added_blocks)?;
    let preceding_blocks = checked_sub(added_blocks, 1)?;
    let entries_before_last = checked_mul(preceding_blocks, entries_per_spill)?;
    let entries_in_last = checked_sub(remaining_count, entries_before_last)?;
    let bytes_in_last = checked_mul(entries_in_last, entry_len)?;
    let remaining_bytes = checked_sub(spill_capacity, bytes_in_last)?;
    Ok(IndexPackingState {
        block_count,
        remaining_bytes,
        inline_entry_bytes,
        current_block_is_empty: false,
    })
}

pub fn checked_sidecar_index_capacity_layout(
    block_size_bytes: u64,
    parity_entry_count: u64,
    data_crc_entry_count: u64,
) -> Result<SidecarIndexCapacityLayout, CapacityError> {
    let minimum_block_size = checked_add(sidecar_header_bytes(), sidecar_trailing_crc_bytes())?;
    if block_size_bytes < minimum_block_size {
        return Err(CapacityError::SidecarEntryDoesNotFit);
    }
    let spill_capacity = checked_sub(block_size_bytes, sidecar_trailing_crc_bytes())?;
    let first_capacity = checked_sub(spill_capacity, sidecar_header_bytes())?;
    let initial = IndexPackingState {
        block_count: 1,
        remaining_bytes: first_capacity,
        inline_entry_bytes: 0,
        current_block_is_empty: true,
    };
    let after_parity = pack_index_segment(
        initial,
        spill_capacity,
        parity_entry_count,
        parity_index_entry_bytes(),
    )?;
    let after_data = pack_index_segment(
        after_parity,
        spill_capacity,
        data_crc_entry_count,
        data_crc_entry_bytes(),
    )?;
    Ok(SidecarIndexCapacityLayout {
        block_count: after_data.block_count,
        inline_entry_bytes: after_data.inline_entry_bytes,
    })
}

pub fn parity_map_directory_len_upper_bound(
    directory_entry_count: u64,
) -> Result<u64, CapacityError> {
    let rows = checked_mul(
        directory_entry_count,
        parity_map_directory_entry_bound_bytes(),
    )?;
    checked_add(parity_map_directory_fixed_bound_bytes(), rows)
}

pub fn parity_map_payload_len_upper_bound(
    directory_entry_count: u64,
) -> Result<u64, CapacityError> {
    let rows = checked_mul(
        directory_entry_count,
        parity_map_directory_entry_bound_bytes(),
    )?;
    checked_add(parity_map_fixed_bound_bytes(), rows)
}

pub fn supported_snapshot_block_size(block_size_bytes: u64) -> bool {
    block_size_bytes == 262_144 || block_size_bytes == 524_288 || block_size_bytes == 1_048_576
}

/// Checked `copy 1 + copy 2 + footer` geometry shared by ParityMap and the
/// candidate TapeIndexSnapshot.
pub fn replicated_control_total_blocks(
    block_size_bytes: u64,
    header_bytes: u64,
    payload_bytes: u64,
) -> Result<u64, CapacityError> {
    if block_size_bytes == 0 {
        return Err(CapacityError::BlockSizeZero);
    }
    if header_bytes > block_size_bytes {
        return Err(CapacityError::ReplicatedControlHeaderTooLarge);
    }
    let copy_bytes = checked_add(header_bytes, payload_bytes)?;
    let quotient = copy_bytes / block_size_bytes;
    let remainder = copy_bytes % block_size_bytes;
    let copy_blocks = checked_add(quotient, if remainder == 0 { 0 } else { 1 })?;
    checked_add(checked_mul(2, copy_blocks)?, 1)
}

pub fn snapshot_payload_bytes(
    structural_entry_count: u64,
    object_row_count: u64,
) -> Result<u64, CapacityError> {
    if object_row_count > structural_entry_count {
        return Err(CapacityError::ObjectRowsExceedStructuralEntries);
    }
    let structural = checked_mul(structural_entry_count, snapshot_structural_slot_bytes())?;
    let rows = checked_mul(object_row_count, snapshot_object_row_slot_bytes())?;
    checked_add(structural, rows)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotSidecarTerms {
    pub index_block_count: u64,
    pub blocks_before_filemark: u64,
    pub tape_file_blocks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotProjectionTerms {
    pub epochs_completed_by_object: u64,
    pub final_partial_sidecar_needed: bool,
    pub sidecars_emitted_by_commit: u64,
    pub sidecar_blocks_emitted_by_commit: u64,
    pub object_tape_file_blocks: u64,
    pub object_commit_charge_blocks: u64,
    pub object_rows_after: u64,
    pub sidecar_entries_after_closeout: u64,
    pub maximum_sidecar_entries_for_capacity: u64,
    pub structural_entries_after_closeout: u64,
    pub final_parity_map_needed: bool,
    pub final_parity_map_directory_bound_bytes: u64,
    pub final_parity_map_payload_bound_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotControlTerms {
    pub final_partial_sidecar_blocks: u64,
    pub final_parity_map_blocks_before_filemark: u64,
    pub final_parity_map_tape_file_blocks: u64,
    pub snapshot_payload_bytes: u64,
    pub snapshot_blocks_before_filemark: u64,
    pub snapshot_tape_file_blocks: u64,
    pub final_bootstrap_tape_file_blocks: u64,
    pub close_bound_blocks: u64,
}

pub fn validate_snapshot_close_input(input: SnapshotCloseInput) -> Result<(), CapacityError> {
    if !supported_snapshot_block_size(input.block_size_bytes) {
        return Err(CapacityError::UnsupportedBlockSize);
    }
    if input.data_shards_per_epoch == 0 {
        return Err(CapacityError::DataShardsPerEpochZero);
    }
    if input.parity_shards_per_epoch == 0 {
        return Err(CapacityError::ParityShardsPerEpochZero);
    }
    let profile_neighborhood_blocks =
        checked_add(input.data_shards_per_epoch, input.parity_shards_per_epoch)?;
    if profile_neighborhood_blocks > 4_294_967_295 {
        return Err(CapacityError::ProfileNeighborhoodTooLarge);
    }
    if input.current_epoch_fill_blocks >= input.data_shards_per_epoch {
        return Err(CapacityError::CurrentEpochFillOutsideOpenEpoch);
    }
    if input.object_rows_before_object > input.structural_entries_before_object {
        return Err(CapacityError::ObjectRowsExceedStructuralEntries);
    }
    if input.sidecar_entries_before_object > input.structural_entries_before_object {
        return Err(CapacityError::SidecarRowsExceedStructuralEntries);
    }
    let recovery_rows_before_object = checked_add(
        input.object_rows_before_object,
        input.sidecar_entries_before_object,
    )?;
    if recovery_rows_before_object > input.structural_entries_before_object {
        return Err(CapacityError::RecoveryRowsExceedStructuralEntries);
    }
    if input.structural_entries_before_object > input.empty_tape_usable_blocks {
        return Err(CapacityError::StructuralEntriesExceedCapacity);
    }
    Ok(())
}

pub fn compute_snapshot_sidecar_terms(
    input: SnapshotCloseInput,
) -> Result<SnapshotSidecarTerms, CapacityError> {
    let layout = checked_sidecar_index_capacity_layout(
        input.block_size_bytes,
        input.parity_shards_per_epoch,
        input.data_shards_per_epoch,
    )?;
    let replicated_index_blocks = checked_mul(2, layout.block_count)?;
    let blocks_before_filemark = checked_add(
        checked_add(replicated_index_blocks, input.parity_shards_per_epoch)?,
        1,
    )?;
    let tape_file_blocks = checked_add(blocks_before_filemark, input.sidecar_filemark_blocks)?;
    Ok(SnapshotSidecarTerms {
        index_block_count: layout.block_count,
        blocks_before_filemark,
        tape_file_blocks,
    })
}

/// Validate capacity-derived worst-case control counts without allocating any
/// hypothetical rows and return the physical sidecar-directory ceiling.
pub fn validate_capacity_derived_profile_bounds(
    input: SnapshotCloseInput,
    maximum_complete_sidecar_tape_file_blocks: u64,
) -> Result<u64, CapacityError> {
    let closeout_budget_blocks =
        checked_sub(input.empty_tape_usable_blocks, input.high_watermark_blocks)?;
    if maximum_complete_sidecar_tape_file_blocks > input.empty_tape_usable_blocks {
        return Err(CapacityError::CapacityProfileCloseExceedsCapacity);
    }
    let minimum_sidecar_tape_file_blocks = checked_add(
        checked_add(input.parity_shards_per_epoch, 3)?,
        input.sidecar_filemark_blocks,
    )?;
    let maximum_sidecar_entries_for_capacity =
        input.empty_tape_usable_blocks / minimum_sidecar_tape_file_blocks;
    let _maximum_directory_bytes =
        parity_map_directory_len_upper_bound(maximum_sidecar_entries_for_capacity)?;
    let maximum_parity_map_payload_bytes =
        parity_map_payload_len_upper_bound(maximum_sidecar_entries_for_capacity)?;
    let maximum_parity_map_blocks_before_filemark = replicated_control_total_blocks(
        input.block_size_bytes,
        parity_map_header_bytes(),
        maximum_parity_map_payload_bytes,
    )?;
    let maximum_parity_map_tape_file_blocks = if maximum_sidecar_entries_for_capacity == 0 {
        0
    } else {
        checked_add(
            maximum_parity_map_blocks_before_filemark,
            input.parity_map_filemark_blocks,
        )?
    };
    let maximum_snapshot_payload_bytes = snapshot_payload_bytes(
        input.empty_tape_usable_blocks,
        input.empty_tape_usable_blocks,
    )?;
    let maximum_snapshot_blocks_before_filemark = replicated_control_total_blocks(
        input.block_size_bytes,
        snapshot_header_bytes(),
        maximum_snapshot_payload_bytes,
    )?;
    let maximum_snapshot_tape_file_blocks = checked_add(
        maximum_snapshot_blocks_before_filemark,
        input.snapshot_filemark_blocks,
    )?;
    let final_bootstrap_tape_file_blocks =
        checked_add(block_count_per_bootstrap(), input.bootstrap_filemark_blocks)?;
    let maximum_close_step1 = checked_add(
        maximum_complete_sidecar_tape_file_blocks,
        maximum_parity_map_tape_file_blocks,
    )?;
    let maximum_close_step2 = checked_add(maximum_close_step1, maximum_snapshot_tape_file_blocks)?;
    let maximum_close_step3 = checked_add(maximum_close_step2, final_bootstrap_tape_file_blocks)?;
    let maximum_close_bound_blocks = checked_add(maximum_close_step3, input.safety_margin_blocks)?;
    if maximum_close_bound_blocks > closeout_budget_blocks {
        return Err(CapacityError::CapacityProfileCloseExceedsCapacity);
    }
    Ok(maximum_sidecar_entries_for_capacity)
}

pub fn compute_snapshot_projection_terms(
    input: SnapshotCloseInput,
    sidecar: SnapshotSidecarTerms,
    maximum_sidecar_entries_for_capacity: u64,
) -> Result<SnapshotProjectionTerms, CapacityError> {
    let projected_epoch_fill = checked_add(
        input.current_epoch_fill_blocks,
        input.projected_object_blocks,
    )?;
    let epochs_completed_by_object = projected_epoch_fill / input.data_shards_per_epoch;
    let final_partial_sidecar_needed = projected_epoch_fill % input.data_shards_per_epoch != 0;
    let sidecars_emitted_by_commit =
        checked_add(input.pending_completed_sidecars, epochs_completed_by_object)?;
    let sidecar_blocks_emitted_by_commit =
        checked_mul(sidecars_emitted_by_commit, sidecar.tape_file_blocks)?;
    let object_tape_file_blocks =
        checked_add(input.projected_object_blocks, input.object_filemark_blocks)?;
    let object_commit_charge_blocks =
        checked_add(object_tape_file_blocks, sidecar_blocks_emitted_by_commit)?;
    let object_rows_after = checked_add(input.object_rows_before_object, 1)?;
    let sidecar_entries_after_commit = checked_add(
        input.sidecar_entries_before_object,
        sidecars_emitted_by_commit,
    )?;
    let final_partial_sidecar_count = if final_partial_sidecar_needed { 1 } else { 0 };
    let sidecar_entries_after_closeout =
        checked_add(sidecar_entries_after_commit, final_partial_sidecar_count)?;
    if input.sidecar_entries_before_object > maximum_sidecar_entries_for_capacity {
        return Err(CapacityError::SidecarDirectoryExceedsCapacity);
    }
    let final_parity_map_directory_bound_bytes =
        parity_map_directory_len_upper_bound(sidecar_entries_after_closeout)?;
    let final_parity_map_payload_bound_bytes =
        parity_map_payload_len_upper_bound(sidecar_entries_after_closeout)?;
    let final_parity_map_needed = sidecar_entries_after_closeout != 0;
    let final_parity_map_count = if final_parity_map_needed { 1 } else { 0 };
    let structural_entries_after_commit = checked_add(
        checked_add(input.structural_entries_before_object, 1)?,
        sidecars_emitted_by_commit,
    )?;
    let structural_entries_after_closeout = checked_add(
        checked_add(structural_entries_after_commit, final_partial_sidecar_count)?,
        final_parity_map_count,
    )?;
    Ok(SnapshotProjectionTerms {
        epochs_completed_by_object,
        final_partial_sidecar_needed,
        sidecars_emitted_by_commit,
        sidecar_blocks_emitted_by_commit,
        object_tape_file_blocks,
        object_commit_charge_blocks,
        object_rows_after,
        sidecar_entries_after_closeout,
        maximum_sidecar_entries_for_capacity,
        structural_entries_after_closeout,
        final_parity_map_needed,
        final_parity_map_directory_bound_bytes,
        final_parity_map_payload_bound_bytes,
    })
}

pub fn compute_snapshot_control_terms(
    input: SnapshotCloseInput,
    sidecar: SnapshotSidecarTerms,
    projection: SnapshotProjectionTerms,
) -> Result<SnapshotControlTerms, CapacityError> {
    let snapshot_payload_bytes = snapshot_payload_bytes(
        projection.structural_entries_after_closeout,
        projection.object_rows_after,
    )?;
    let snapshot_blocks_before_filemark = replicated_control_total_blocks(
        input.block_size_bytes,
        snapshot_header_bytes(),
        snapshot_payload_bytes,
    )?;
    let snapshot_tape_file_blocks = checked_add(
        snapshot_blocks_before_filemark,
        input.snapshot_filemark_blocks,
    )?;
    let final_parity_map_blocks_before_filemark = if projection.final_parity_map_needed {
        replicated_control_total_blocks(
            input.block_size_bytes,
            parity_map_header_bytes(),
            projection.final_parity_map_payload_bound_bytes,
        )?
    } else {
        0
    };
    let final_parity_map_tape_file_blocks = if projection.final_parity_map_needed {
        checked_add(
            final_parity_map_blocks_before_filemark,
            input.parity_map_filemark_blocks,
        )?
    } else {
        0
    };
    let final_partial_sidecar_blocks = if projection.final_partial_sidecar_needed {
        sidecar.tape_file_blocks
    } else {
        0
    };
    let final_bootstrap_tape_file_blocks =
        checked_add(block_count_per_bootstrap(), input.bootstrap_filemark_blocks)?;
    let close_step1 = checked_add(
        final_partial_sidecar_blocks,
        final_parity_map_tape_file_blocks,
    )?;
    let close_step2 = checked_add(close_step1, snapshot_tape_file_blocks)?;
    let close_step3 = checked_add(close_step2, final_bootstrap_tape_file_blocks)?;
    let close_bound_blocks = checked_add(close_step3, input.safety_margin_blocks)?;
    Ok(SnapshotControlTerms {
        final_partial_sidecar_blocks,
        final_parity_map_blocks_before_filemark,
        final_parity_map_tape_file_blocks,
        snapshot_payload_bytes,
        snapshot_blocks_before_filemark,
        snapshot_tape_file_blocks,
        final_bootstrap_tape_file_blocks,
        close_bound_blocks,
    })
}

/// Evaluate the no-policy Object commit plus conservative final-close bound
/// before any Object block is written.
pub fn evaluate_snapshot_close(
    input: SnapshotCloseInput,
) -> Result<SnapshotCloseReport, CapacityError> {
    validate_snapshot_close_input(input)?;
    let sidecar = compute_snapshot_sidecar_terms(input)?;
    let maximum_sidecar_entries_for_capacity =
        match validate_capacity_derived_profile_bounds(input, sidecar.tape_file_blocks) {
            Ok(value) => value,
            Err(_) => return Err(CapacityError::UnsafeCapacityProfile),
        };
    let projection =
        compute_snapshot_projection_terms(input, sidecar, maximum_sidecar_entries_for_capacity)?;
    let control = compute_snapshot_control_terms(input, sidecar, projection)?;
    let required_tape_blocks = checked_add(
        projection.object_commit_charge_blocks,
        control.close_bound_blocks,
    )?;
    if input.empty_tape_usable_blocks < required_tape_blocks {
        return Err(CapacityError::ObjectTooLargeForEmptyTape);
    }
    if input.remaining_tape_blocks < required_tape_blocks {
        return Err(CapacityError::CapacityReserveExceededTape);
    }
    let sidecar_tape_file_bytes =
        checked_mul(sidecar.blocks_before_filemark, input.block_size_bytes)?;
    let newly_completed_sidecar_bytes = checked_mul(
        projection.epochs_completed_by_object,
        sidecar_tape_file_bytes,
    )?;
    let required_spool_bytes = checked_add(
        input.pending_completed_epoch_parity_bytes,
        newly_completed_sidecar_bytes,
    )?;
    if input.remaining_spool_bytes < required_spool_bytes {
        return Err(CapacityError::CapacityReserveExceededSpool);
    }
    Ok(SnapshotCloseReport {
        epochs_completed_by_object: projection.epochs_completed_by_object,
        final_partial_sidecar_needed: projection.final_partial_sidecar_needed,
        sidecar_index_block_count: sidecar.index_block_count,
        sidecar_blocks_before_filemark: sidecar.blocks_before_filemark,
        sidecar_tape_file_blocks: sidecar.tape_file_blocks,
        sidecars_emitted_by_commit: projection.sidecars_emitted_by_commit,
        sidecar_blocks_emitted_by_commit: projection.sidecar_blocks_emitted_by_commit,
        object_tape_file_blocks: projection.object_tape_file_blocks,
        object_commit_charge_blocks: projection.object_commit_charge_blocks,
        object_rows_after: projection.object_rows_after,
        sidecar_entries_after_closeout: projection.sidecar_entries_after_closeout,
        maximum_sidecar_entries_for_capacity: projection.maximum_sidecar_entries_for_capacity,
        structural_entries_after_closeout: projection.structural_entries_after_closeout,
        final_partial_sidecar_blocks: control.final_partial_sidecar_blocks,
        final_parity_map_needed: projection.final_parity_map_needed,
        final_parity_map_directory_bound_bytes: projection.final_parity_map_directory_bound_bytes,
        final_parity_map_payload_bound_bytes: projection.final_parity_map_payload_bound_bytes,
        final_parity_map_blocks_before_filemark: control.final_parity_map_blocks_before_filemark,
        final_parity_map_tape_file_blocks: control.final_parity_map_tape_file_blocks,
        snapshot_payload_bytes: control.snapshot_payload_bytes,
        snapshot_blocks_before_filemark: control.snapshot_blocks_before_filemark,
        snapshot_tape_file_blocks: control.snapshot_tape_file_blocks,
        final_bootstrap_tape_file_blocks: control.final_bootstrap_tape_file_blocks,
        safety_margin_blocks: input.safety_margin_blocks,
        close_bound_blocks: control.close_bound_blocks,
        required_tape_blocks,
        required_spool_bytes,
    })
}

pub fn compute_tape_reserve(
    input: CapacityReserveInput,
) -> Result<TapeReserveReport, CapacityError> {
    if input.block_size_bytes == 0 {
        return Err(CapacityError::BlockSizeZero);
    }
    if input.data_shards_per_epoch == 0 {
        return Err(CapacityError::DataShardsPerEpochZero);
    }
    if input.current_epoch_fill_blocks >= input.data_shards_per_epoch {
        return Err(CapacityError::CurrentEpochFillOutsideOpenEpoch);
    }

    let sidecar_metadata_blocks = checked_add(checked_mul(2, input.sidecar_index_block_count)?, 1)?;
    let sidecar_plus_parity = checked_add(sidecar_metadata_blocks, input.parity_shards_per_epoch)?;
    let sidecar_tape_file_blocks = checked_add(sidecar_plus_parity, input.sidecar_filemark_blocks)?;
    let bootstrap_tape_file_blocks =
        checked_add(block_count_per_bootstrap(), input.bootstrap_filemark_blocks)?;

    let projected_epoch_fill = checked_add(
        input.current_epoch_fill_blocks,
        input.projected_object_blocks,
    )?;
    let epochs_completed_by_object = projected_epoch_fill / input.data_shards_per_epoch;
    let final_partial_sidecar_needed = projected_epoch_fill % input.data_shards_per_epoch != 0;

    let pending_sidecar_blocks =
        checked_mul(input.pending_completed_sidecars, sidecar_tape_file_blocks)?;
    let completed_by_object_sidecar_blocks =
        checked_mul(epochs_completed_by_object, sidecar_tape_file_blocks)?;
    let final_partial_sidecar_blocks = if final_partial_sidecar_needed {
        sidecar_tape_file_blocks
    } else {
        0
    };
    let remaining_bootstrap_blocks =
        checked_mul(input.remaining_bootstrap_count, bootstrap_tape_file_blocks)?;

    let reserve_step1 = checked_add(input.object_filemark_blocks, pending_sidecar_blocks)?;
    let reserve_step2 = checked_add(reserve_step1, completed_by_object_sidecar_blocks)?;
    let reserve_step3 = checked_add(reserve_step2, final_partial_sidecar_blocks)?;
    let reserve_step4 = checked_add(reserve_step3, remaining_bootstrap_blocks)?;
    let reserve_after_object_blocks = checked_add(reserve_step4, input.safety_margin_blocks)?;
    let required_tape_blocks =
        checked_add(input.projected_object_blocks, reserve_after_object_blocks)?;

    Ok(TapeReserveReport {
        epochs_completed_by_object,
        final_partial_sidecar_needed,
        sidecar_tape_file_blocks,
        bootstrap_tape_file_blocks,
        reserve_after_object_blocks,
        required_tape_blocks,
    })
}

pub fn compute_spool_reserve(
    input: CapacityReserveInput,
    epochs_completed_by_object: u64,
    sidecar_tape_file_blocks: u64,
) -> Result<u64, CapacityError> {
    let sidecar_tape_file_bytes = checked_mul(sidecar_tape_file_blocks, input.block_size_bytes)?;
    let completed_by_object_spool_bytes =
        checked_mul(epochs_completed_by_object, sidecar_tape_file_bytes)?;
    checked_add(
        input.pending_completed_epoch_parity_bytes,
        completed_by_object_spool_bytes,
    )
}

pub fn evaluate(input: CapacityReserveInput) -> Result<CapacityReserveReport, CapacityError> {
    let tape = compute_tape_reserve(input)?;

    if input.empty_tape_usable_blocks < tape.required_tape_blocks {
        return Err(CapacityError::ObjectTooLargeForEmptyTape);
    }

    if input.remaining_tape_blocks < tape.required_tape_blocks {
        return Err(CapacityError::CapacityReserveExceededTape);
    }

    let required_spool_bytes = compute_spool_reserve(
        input,
        tape.epochs_completed_by_object,
        tape.sidecar_tape_file_blocks,
    )?;

    if input.remaining_spool_bytes < required_spool_bytes {
        return Err(CapacityError::CapacityReserveExceededSpool);
    }

    Ok(CapacityReserveReport {
        epochs_completed_by_object: tape.epochs_completed_by_object,
        final_partial_sidecar_needed: tape.final_partial_sidecar_needed,
        sidecar_tape_file_blocks: tape.sidecar_tape_file_blocks,
        bootstrap_tape_file_blocks: tape.bootstrap_tape_file_blocks,
        reserve_after_object_blocks: tape.reserve_after_object_blocks,
        required_tape_blocks: tape.required_tape_blocks,
        required_spool_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_parity as production;

    /// FNV-1a is deliberately implemented locally so the proof extraction stays
    /// dependency-free while the test can pin exact source-region bytes.
    fn source_fingerprint(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn assert_source_region(
        source_name: &str,
        source: &str,
        start_marker: &str,
        end_marker: &str,
        expected_len: usize,
        expected_fingerprint: u64,
    ) {
        let start = source.find(start_marker).unwrap_or_else(|| {
            panic!("{source_name}: candidate region start marker missing: {start_marker:?}")
        });
        let after_start = &source[start..];
        let relative_end = after_start.find(end_marker).unwrap_or_else(|| {
            panic!("{source_name}: candidate region end marker missing: {end_marker:?}")
        });
        assert!(
            relative_end > 0,
            "{source_name}: candidate region markers are reversed or identical"
        );
        let region = &after_start.as_bytes()[..relative_end];
        let actual_fingerprint = source_fingerprint(region);
        assert_eq!(
            (region.len(), actual_fingerprint),
            (expected_len, expected_fingerprint),
            "{source_name}: exact candidate region drifted between {start_marker:?} and \
             {end_marker:?}; got len={} fingerprint=0x{actual_fingerprint:016x}. \
             Re-sync the extraction, generated Lean, and proofs after reviewing the full body",
            region.len()
        );
    }

    fn sample_input() -> CapacityReserveInput {
        CapacityReserveInput {
            projected_object_blocks: 20,
            block_size_bytes: 1024,
            current_epoch_fill_blocks: 5,
            data_shards_per_epoch: 12,
            parity_shards_per_epoch: 6,
            sidecar_index_block_count: 2,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 1,
            remaining_bootstrap_count: 2,
            safety_margin_blocks: 3,
            remaining_tape_blocks: 76,
            empty_tape_usable_blocks: u64::MAX,
            pending_completed_epoch_parity_bytes: 7 * 1024,
            remaining_spool_bytes: 31 * 1024,
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let capacity = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/capacity.rs"
        ))
        .expect("original capacity.rs must be readable from verif/parity-capacity");
        let sidecar = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/sidecar.rs"
        ))
        .expect("sidecar.rs must be readable from verif/parity-capacity");
        let parity_map = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/parity_map.rs"
        ))
        .expect("parity_map.rs must be readable from verif/parity-capacity");
        let tape_index = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/tape_index.rs"
        ))
        .expect("tape_index.rs must be readable from verif/parity-capacity");
        let replicated_control = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/replicated_control.rs"
        ))
        .expect("replicated_control.rs must be readable from verif/parity-capacity");

        let snippets: &[&str] = &[
            "if self.block_size_bytes == 0 {",
            "if self.data_shards_per_epoch == 0 {",
            "if self.current_epoch_fill_blocks >= self.data_shards_per_epoch {",
            "checked_mul(2, self.sidecar_index_block_count)?",
            "1, // footer locator",
            "self.parity_shards_per_epoch,\n            self.sidecar_filemark_blocks,",
            "checked_add(\n            self.block_count_per_bootstrap(),\n            self.bootstrap_filemark_blocks,",
            "let projected_epoch_fill =\n            checked_add(self.current_epoch_fill_blocks, self.projected_object_blocks)?;",
            "let epochs_completed_by_object = projected_epoch_fill / self.data_shards_per_epoch;",
            "let final_partial_sidecar_needed = projected_epoch_fill % self.data_shards_per_epoch != 0;",
            "checked_mul(self.pending_completed_sidecars, sidecar_tape_file_blocks)?;",
            "checked_mul(epochs_completed_by_object, sidecar_tape_file_blocks)?;",
            "checked_mul(self.remaining_bootstrap_count, bootstrap_tape_file_blocks)?;",
            "self.empty_tape_usable_blocks < required_tape_blocks",
            "self.remaining_tape_blocks < required_tape_blocks",
            "checked_mul(sidecar_tape_file_blocks, self.block_size_bytes)?;",
            "checked_mul(epochs_completed_by_object, sidecar_tape_file_bytes)?;",
            "self.remaining_spool_bytes < required_spool_bytes",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                capacity.contains(snippet),
                "snippet {i} no longer in remanence-parity capacity.rs -- original \
                 changed; re-sync this extraction and its Lean proofs"
            );
        }

        let candidate_sources: &[(&str, &str, &[&str])] = &[
            (
                "capacity.rs",
                &capacity,
                &[
                    "pub struct SnapshotCloseInput",
                    "pub struct SnapshotCloseReport",
                    "checked_sidecar_index_capacity_layout(",
                    "parity_map_directory_len_upper_bound(sidecar_entries_after_closeout)?",
                    "parity_map_payload_len_upper_bound(sidecar_entries_after_closeout)?",
                    "structural_entry_count: structural_entries_after_closeout",
                    "object_row_count: object_rows_after",
                    "let required_tape_blocks = checked_add(object_commit_charge_blocks, close_bound_blocks)?;",
                    "if self.empty_tape_usable_blocks < required_tape_blocks",
                    "if self.remaining_tape_blocks < required_tape_blocks",
                    "if self.remaining_spool_bytes < required_spool_bytes",
                ],
            ),
            (
                "sidecar.rs",
                &sidecar,
                &[
                    "pub fn checked_sidecar_index_capacity_layout(",
                    "let after_parity = pack_index_segment(",
                    "let after_data = pack_index_segment(",
                    "let layout = checked_sidecar_index_capacity_layout(",
                    "u32::try_from(layout.block_count)",
                ],
            ),
            (
                "parity_map.rs",
                &parity_map,
                &[
                    "pub const PARITY_MAP_CBOR_FIXED_UPPER_BOUND_BYTES: u64 = 325;",
                    "pub const PARITY_MAP_CBOR_DIRECTORY_FIXED_UPPER_BOUND_BYTES: u64 = 43;",
                    "pub const PARITY_MAP_CBOR_DIRECTORY_ENTRY_UPPER_BOUND_BYTES: u64 = 116;",
                    "pub fn parity_map_directory_len_upper_bound(",
                    "pub fn parity_map_payload_len_upper_bound(",
                ],
            ),
            (
                "tape_index.rs",
                &tape_index,
                &[
                    "pub const TAPE_INDEX_SNAPSHOT_HEADER_LEN: usize = 0x200;",
                    "pub const TAPE_INDEX_STRUCTURAL_SLOT_LEN: u64 = 64;",
                    "pub const TAPE_INDEX_OBJECT_ROW_SLOT_LEN: u64 = 256;",
                    "if counts.object_row_count > counts.structural_entry_count",
                    "pub fn tape_index_snapshot_layout(",
                ],
            ),
            (
                "replicated_control.rs",
                &replicated_control,
                &[
                    "let copy_bytes = header_len",
                    "let copy_block_count = copy_bytes.div_ceil(block_size);",
                    "let footer_block_index = copy_block_count",
                    "let total_block_count = footer_block_index",
                ],
            ),
        ];
        for (source_name, source, snippets) in candidate_sources {
            for (i, snippet) in snippets.iter().enumerate() {
                assert!(
                    source.contains(snippet),
                    "candidate snippet {i} no longer in {source_name} -- re-sync the extraction and proofs"
                );
            }
        }

        let candidate_regions: &[(&str, &str, &str, &str, usize, u64)] = &[
            (
                "capacity.rs",
                &capacity,
                "impl SnapshotCloseInput {",
                "\nimpl CapacityReserveInput {",
                15_766,
                0xc983_0f45_f6cf_c77e,
            ),
            (
                "sidecar.rs scalar packing",
                &sidecar,
                "pub fn checked_sidecar_index_capacity_layout(",
                "\nconst SIDECAR_MAGIC_MESSAGE",
                4_642,
                0x8883_d6d8_8e03_fe5c,
            ),
            (
                "sidecar.rs encoder delegation",
                &sidecar,
                "fn compute_index_layout(",
                "\nfn entry_kinds(",
                830,
                0x4663_278b_d1a1_d2dc,
            ),
            (
                "parity_map.rs bounds",
                &parity_map,
                "pub const PARITY_MAP_CBOR_FIXED_UPPER_BOUND_BYTES",
                "\n/// Directory flag:",
                1_665,
                0xbaef_a316_1912_c258,
            ),
            (
                "tape_index.rs constants",
                &tape_index,
                "pub const TAPE_INDEX_SNAPSHOT_HEADER_LEN",
                "\nconst TAPE_INDEX_HEADER_MAGIC_MESSAGE",
                1_309,
                0x1449_9f0e_bd15_9869,
            ),
            (
                "tape_index.rs layout",
                &tape_index,
                "pub fn tape_index_snapshot_layout(",
                "\n/// Derive the HMAC-domain header magic",
                883,
                0xcd46_e2af_d46c_bdaf,
            ),
            (
                "tape_index.rs payload formula",
                &tape_index,
                "fn validate_snapshot_counts(",
                "\nfn validate_snapshot_descriptor(",
                1_031,
                0xe217_55e0_93ea_a776,
            ),
            (
                "replicated_control.rs layout",
                &replicated_control,
                "pub(crate) fn checked_replicated_control_layout(",
                "\n/// Validate persisted locator fields",
                1_241,
                0xd4c8_71ee_5793_86dc,
            ),
        ];
        for (source_name, source, start, end, expected_len, expected_fingerprint) in
            candidate_regions
        {
            assert_source_region(
                source_name,
                source,
                start,
                end,
                *expected_len,
                *expected_fingerprint,
            );
        }

        let extraction_snippets: &[&str] = &[
            "checked_add(checked_mul(2, input.sidecar_index_block_count)?, 1)?",
            "let sidecar_plus_parity = checked_add(sidecar_metadata_blocks, input.parity_shards_per_epoch)?;",
            "let sidecar_tape_file_blocks = checked_add(sidecar_plus_parity, input.sidecar_filemark_blocks)?;",
            "let bootstrap_tape_file_blocks =\n        checked_add(block_count_per_bootstrap(), input.bootstrap_filemark_blocks)?;",
            "let epochs_completed_by_object = projected_epoch_fill / input.data_shards_per_epoch;",
            "let final_partial_sidecar_needed = projected_epoch_fill % input.data_shards_per_epoch != 0;",
            "let reserve_step1 = checked_add(input.object_filemark_blocks, pending_sidecar_blocks)?;",
            "let reserve_step2 = checked_add(reserve_step1, completed_by_object_sidecar_blocks)?;",
            "let reserve_step3 = checked_add(reserve_step2, final_partial_sidecar_blocks)?;",
            "let reserve_step4 = checked_add(reserve_step3, remaining_bootstrap_blocks)?;",
            "let tape = compute_tape_reserve(input)?;",
            "let sidecar_tape_file_bytes = checked_mul(sidecar_tape_file_blocks, input.block_size_bytes)?;",
            "let completed_by_object_spool_bytes =\n        checked_mul(epochs_completed_by_object, sidecar_tape_file_bytes)?;",
            "pub fn checked_sidecar_index_capacity_layout(",
            "pub fn parity_map_directory_len_upper_bound(",
            "pub fn parity_map_payload_len_upper_bound(",
            "pub fn replicated_control_total_blocks(",
            "pub fn snapshot_payload_bytes(",
            "pub fn evaluate_snapshot_close(",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif capacity model"
            );
        }
    }

    fn production_snapshot_input(input: SnapshotCloseInput) -> production::SnapshotCloseInput {
        production::SnapshotCloseInput {
            projected_object_blocks: input.projected_object_blocks,
            block_size_bytes: u32::try_from(input.block_size_bytes)
                .expect("behavior matrix block size fits u32"),
            current_epoch_fill_blocks: input.current_epoch_fill_blocks,
            data_shards_per_epoch: input.data_shards_per_epoch,
            parity_shards_per_epoch: input.parity_shards_per_epoch,
            pending_completed_sidecars: input.pending_completed_sidecars,
            sidecar_entries_before_object: input.sidecar_entries_before_object,
            structural_entries_before_object: input.structural_entries_before_object,
            object_rows_before_object: input.object_rows_before_object,
            object_filemark_blocks: input.object_filemark_blocks,
            sidecar_filemark_blocks: input.sidecar_filemark_blocks,
            parity_map_filemark_blocks: input.parity_map_filemark_blocks,
            snapshot_filemark_blocks: input.snapshot_filemark_blocks,
            bootstrap_filemark_blocks: input.bootstrap_filemark_blocks,
            safety_margin_blocks: input.safety_margin_blocks,
            remaining_tape_blocks: input.remaining_tape_blocks,
            empty_tape_usable_blocks: input.empty_tape_usable_blocks,
            high_watermark_blocks: input.high_watermark_blocks,
            pending_completed_epoch_parity_bytes: input.pending_completed_epoch_parity_bytes,
            remaining_spool_bytes: input.remaining_spool_bytes,
        }
    }

    fn localize_production_report(report: production::SnapshotCloseReport) -> SnapshotCloseReport {
        SnapshotCloseReport {
            epochs_completed_by_object: report.epochs_completed_by_object,
            final_partial_sidecar_needed: report.final_partial_sidecar_needed,
            sidecar_index_block_count: report.sidecar_index_block_count,
            sidecar_blocks_before_filemark: report.sidecar_blocks_before_filemark,
            sidecar_tape_file_blocks: report.sidecar_tape_file_blocks,
            sidecars_emitted_by_commit: report.sidecars_emitted_by_commit,
            sidecar_blocks_emitted_by_commit: report.sidecar_blocks_emitted_by_commit,
            object_tape_file_blocks: report.object_tape_file_blocks,
            object_commit_charge_blocks: report.object_commit_charge_blocks,
            object_rows_after: report.object_rows_after,
            sidecar_entries_after_closeout: report.sidecar_entries_after_closeout,
            maximum_sidecar_entries_for_capacity: report.maximum_sidecar_entries_for_capacity,
            structural_entries_after_closeout: report.structural_entries_after_closeout,
            final_partial_sidecar_blocks: report.final_partial_sidecar_blocks,
            final_parity_map_needed: report.final_parity_map_needed,
            final_parity_map_directory_bound_bytes: report.final_parity_map_directory_bound_bytes,
            final_parity_map_payload_bound_bytes: report.final_parity_map_payload_bound_bytes,
            final_parity_map_blocks_before_filemark: report.final_parity_map_blocks_before_filemark,
            final_parity_map_tape_file_blocks: report.final_parity_map_tape_file_blocks,
            snapshot_payload_bytes: report.snapshot_payload_bytes,
            snapshot_blocks_before_filemark: report.snapshot_blocks_before_filemark,
            snapshot_tape_file_blocks: report.snapshot_tape_file_blocks,
            final_bootstrap_tape_file_blocks: report.final_bootstrap_tape_file_blocks,
            safety_margin_blocks: report.safety_margin_blocks,
            close_bound_blocks: report.close_bound_blocks,
            required_tape_blocks: report.required_tape_blocks,
            required_spool_bytes: report.required_spool_bytes,
        }
    }

    fn production_snapshot_report(input: SnapshotCloseInput) -> SnapshotCloseReport {
        production_snapshot_input(input)
            .evaluate()
            .map(localize_production_report)
            .unwrap_or_else(|error| {
                panic!("production rejected valid matrix input {input:?}: {error}")
            })
    }

    fn local_gate(error: CapacityError) -> &'static str {
        match error {
            CapacityError::ObjectTooLargeForEmptyTape => "empty-tape",
            CapacityError::CapacityReserveExceededTape => "current-tape",
            CapacityError::CapacityReserveExceededSpool => "spool",
            other => panic!("unexpected extraction error in gate matrix: {other:?}"),
        }
    }

    fn production_gate(error: production::ParityError) -> &'static str {
        match error {
            production::ParityError::ObjectTooLargeForEmptyTape { .. } => "empty-tape",
            production::ParityError::CapacityReserveExceeded {
                cause: production::CapacityReserveCause::TapeCapacity,
                ..
            } => "current-tape",
            production::ParityError::CapacityReserveExceeded {
                cause: production::CapacityReserveCause::ParitySpoolCapacity,
                ..
            } => "spool",
            other => panic!("unexpected production error in gate matrix: {other:?}"),
        }
    }

    fn local_profile_failure(error: CapacityError) -> &'static str {
        match error {
            CapacityError::UnsafeCapacityProfile => "unsafe-capacity-profile",
            other => panic!("unexpected extraction profile error: {other:?}"),
        }
    }

    fn production_profile_failure(error: production::ParityError) -> &'static str {
        match error {
            production::ParityError::InvalidScheme(message)
                if message.starts_with("unsafe snapshot close capacity profile:") =>
            {
                "unsafe-capacity-profile"
            }
            other => panic!("unexpected production profile error: {other:?}"),
        }
    }

    fn local_structural_capacity_failure(error: CapacityError) -> &'static str {
        match error {
            CapacityError::StructuralEntriesExceedCapacity => "structural-capacity",
            other => panic!("unexpected extraction structural-capacity error: {other:?}"),
        }
    }

    fn production_structural_capacity_failure(error: production::ParityError) -> &'static str {
        match error {
            production::ParityError::Invariant(
                "snapshot close committed structural entries exceed physical capacity bound",
            ) => "structural-capacity",
            other => panic!("unexpected production structural-capacity error: {other:?}"),
        }
    }

    #[test]
    fn drift_guard_snapshot_helpers_match_compiled_production_boundaries() {
        let counts = [0, 1, 7, 8, 15, 16, 17, 255, 256, 257];
        for block_size in [192, 200, 256, 512, 256 * 1024] {
            for parity_count in counts {
                for data_count in counts {
                    let extracted =
                        checked_sidecar_index_capacity_layout(block_size, parity_count, data_count)
                            .map(|layout| (layout.block_count, layout.inline_entry_bytes));
                    let compiled = production::checked_sidecar_index_capacity_layout(
                        block_size,
                        parity_count,
                        data_count,
                    )
                    .map(|layout| (layout.block_count, layout.inline_entry_bytes))
                    .map_err(|_| CapacityError::SidecarEntryDoesNotFit);
                    assert_eq!(
                        extracted, compiled,
                        "sidecar layout drift for B={block_size}, P={parity_count}, D={data_count}"
                    );
                }
            }
        }

        for entry_count in [
            0,
            1,
            23,
            u64::from(u32::MAX),
            (u64::MAX - 325) / 116,
            u64::MAX,
        ] {
            assert_eq!(
                parity_map_directory_len_upper_bound(entry_count).ok(),
                production::parity_map_directory_len_upper_bound(entry_count).ok(),
                "ParityMap directory bound drift for N={entry_count}"
            );
            assert_eq!(
                parity_map_payload_len_upper_bound(entry_count).ok(),
                production::parity_map_payload_len_upper_bound(entry_count).ok(),
                "ParityMap payload bound drift for N={entry_count}"
            );
        }

        for block_size in [256 * 1024, 512 * 1024, 1024 * 1024] {
            for (data_shards, parity_shards) in [(1u64, 1u64), (2, 1), (65_536, 2_048)] {
                for projected_object_blocks in [
                    0,
                    1,
                    data_shards.saturating_sub(1),
                    data_shards,
                    data_shards + 1,
                ] {
                    for current_epoch_fill_blocks in [0, data_shards - 1] {
                        for pending_completed_sidecars in [0, 1] {
                            let input = SnapshotCloseInput {
                                projected_object_blocks,
                                block_size_bytes: block_size,
                                current_epoch_fill_blocks,
                                data_shards_per_epoch: data_shards,
                                parity_shards_per_epoch: parity_shards,
                                pending_completed_sidecars,
                                sidecar_entries_before_object: 1,
                                structural_entries_before_object: 4,
                                object_rows_before_object: 2,
                                object_filemark_blocks: 1,
                                sidecar_filemark_blocks: 1,
                                parity_map_filemark_blocks: 1,
                                snapshot_filemark_blocks: 1,
                                bootstrap_filemark_blocks: 1,
                                safety_margin_blocks: 4,
                                remaining_tape_blocks: 10_000_000,
                                empty_tape_usable_blocks: 10_000_000,
                                high_watermark_blocks: 9_800_000,
                                pending_completed_epoch_parity_bytes: 17,
                                remaining_spool_bytes: u64::MAX,
                            };
                            let extracted =
                                evaluate_snapshot_close(input).unwrap_or_else(|error| {
                                    panic!("extraction rejected {input:?}: {error:?}")
                                });
                            assert_eq!(
                                extracted,
                                production_snapshot_report(input),
                                "snapshot-close report drift for input {input:?}"
                            );
                        }
                    }
                }
            }
        }

        let baseline = SnapshotCloseInput {
            projected_object_blocks: 65_536,
            block_size_bytes: 256 * 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 65_536,
            parity_shards_per_epoch: 2_048,
            pending_completed_sidecars: 1,
            sidecar_entries_before_object: 1,
            structural_entries_before_object: 4,
            object_rows_before_object: 2,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            parity_map_filemark_blocks: 1,
            snapshot_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            safety_margin_blocks: 4,
            remaining_tape_blocks: 10_000_000,
            empty_tape_usable_blocks: 10_000_000,
            high_watermark_blocks: 9_800_000,
            pending_completed_epoch_parity_bytes: 17,
            remaining_spool_bytes: u64::MAX,
        };
        let report = evaluate_snapshot_close(baseline).expect("baseline close fits");
        let gate_inputs = [
            SnapshotCloseInput {
                empty_tape_usable_blocks: report.required_tape_blocks - 1,
                remaining_tape_blocks: report.required_tape_blocks - 1,
                high_watermark_blocks: 0,
                ..baseline
            },
            SnapshotCloseInput {
                empty_tape_usable_blocks: report.required_tape_blocks,
                remaining_tape_blocks: report.required_tape_blocks - 1,
                high_watermark_blocks: 0,
                ..baseline
            },
            SnapshotCloseInput {
                empty_tape_usable_blocks: report.required_tape_blocks,
                remaining_tape_blocks: report.required_tape_blocks,
                high_watermark_blocks: 0,
                remaining_spool_bytes: report.required_spool_bytes - 1,
                ..baseline
            },
        ];
        for input in gate_inputs {
            let extracted = local_gate(evaluate_snapshot_close(input).unwrap_err());
            let compiled =
                production_gate(production_snapshot_input(input).evaluate().unwrap_err());
            assert_eq!(extracted, compiled, "gate-order drift for input {input:?}");
        }

        let unsafe_profile = SnapshotCloseInput {
            empty_tape_usable_blocks: u64::MAX,
            remaining_tape_blocks: u64::MAX,
            ..baseline
        };
        assert_eq!(
            local_profile_failure(evaluate_snapshot_close(unsafe_profile).unwrap_err()),
            production_profile_failure(
                production_snapshot_input(unsafe_profile)
                    .evaluate()
                    .unwrap_err()
            )
        );

        let physically_unusable_profile = SnapshotCloseInput {
            data_shards_per_epoch: 2_000,
            parity_shards_per_epoch: 1_000,
            empty_tape_usable_blocks: 1_000,
            remaining_tape_blocks: 1_000,
            high_watermark_blocks: 0,
            ..baseline
        };
        assert_eq!(
            local_profile_failure(
                evaluate_snapshot_close(physically_unusable_profile).unwrap_err()
            ),
            production_profile_failure(
                production_snapshot_input(physically_unusable_profile)
                    .evaluate()
                    .unwrap_err()
            )
        );

        let physically_unusable_close_profile = SnapshotCloseInput {
            empty_tape_usable_blocks: 2_060,
            remaining_tape_blocks: 2_060,
            high_watermark_blocks: 0,
            ..snapshot_close_input()
        };
        assert_eq!(
            local_profile_failure(
                evaluate_snapshot_close(physically_unusable_close_profile).unwrap_err()
            ),
            production_profile_failure(
                production_snapshot_input(physically_unusable_close_profile)
                    .evaluate()
                    .unwrap_err()
            )
        );

        let closeout_band_too_narrow = SnapshotCloseInput {
            high_watermark_blocks: 100,
            ..snapshot_close_input()
        };
        assert_eq!(
            local_profile_failure(evaluate_snapshot_close(closeout_band_too_narrow).unwrap_err()),
            production_profile_failure(
                production_snapshot_input(closeout_band_too_narrow)
                    .evaluate()
                    .unwrap_err()
            )
        );

        let invalid_high_watermark = SnapshotCloseInput {
            high_watermark_blocks: 2_172,
            ..snapshot_close_input()
        };
        assert_eq!(
            local_profile_failure(evaluate_snapshot_close(invalid_high_watermark).unwrap_err()),
            production_profile_failure(
                production_snapshot_input(invalid_high_watermark)
                    .evaluate()
                    .unwrap_err()
            )
        );

        let structurally_impossible_prefix = SnapshotCloseInput {
            structural_entries_before_object: 10_000_001,
            ..baseline
        };
        assert_eq!(
            local_structural_capacity_failure(
                evaluate_snapshot_close(structurally_impossible_prefix).unwrap_err()
            ),
            production_structural_capacity_failure(
                production_snapshot_input(structurally_impossible_prefix)
                    .evaluate()
                    .unwrap_err()
            )
        );
    }

    fn snapshot_close_input() -> SnapshotCloseInput {
        SnapshotCloseInput {
            projected_object_blocks: 100,
            block_size_bytes: 256 * 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 512 * 128,
            parity_shards_per_epoch: 512 * 4,
            pending_completed_sidecars: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 0,
            object_rows_before_object: 0,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            parity_map_filemark_blocks: 1,
            snapshot_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            safety_margin_blocks: 4,
            remaining_tape_blocks: 2_171,
            empty_tape_usable_blocks: 2_171,
            high_watermark_blocks: 0,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: u64::MAX,
        }
    }

    #[test]
    fn snapshot_close_fixture_matches_production_model() {
        let input = snapshot_close_input();
        let report = evaluate_snapshot_close(input).expect("close fits");
        assert_eq!(report.sidecar_index_block_count, 3);
        assert_eq!(report.sidecar_blocks_before_filemark, 2_055);
        assert_eq!(report.object_commit_charge_blocks, 101);
        assert_eq!(report.structural_entries_after_closeout, 3);
        assert_eq!(report.snapshot_payload_bytes, 448);
        assert_eq!(report.final_parity_map_payload_bound_bytes, 441);
        assert_eq!(report.close_bound_blocks, 2_070);
        assert_eq!(report.required_tape_blocks, 2_171);

        evaluate_snapshot_close(SnapshotCloseInput {
            high_watermark_blocks: 97,
            ..input
        })
        .expect("worst-close equality with C-H must succeed");
    }

    #[test]
    fn snapshot_close_rejects_row_mismatch_and_capacity_shortfall() {
        assert_eq!(
            snapshot_payload_bytes(1, 2),
            Err(CapacityError::ObjectRowsExceedStructuralEntries)
        );
        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                remaining_tape_blocks: 2_170,
                ..snapshot_close_input()
            }),
            Err(CapacityError::CapacityReserveExceededTape)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                object_rows_before_object: 1,
                sidecar_entries_before_object: 1,
                structural_entries_before_object: 1,
                ..snapshot_close_input()
            }),
            Err(CapacityError::RecoveryRowsExceedStructuralEntries)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                empty_tape_usable_blocks: u64::MAX,
                remaining_tape_blocks: u64::MAX,
                ..snapshot_close_input()
            }),
            Err(CapacityError::UnsafeCapacityProfile)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                data_shards_per_epoch: 2_000,
                parity_shards_per_epoch: 1_000,
                empty_tape_usable_blocks: 1_000,
                remaining_tape_blocks: 1_000,
                high_watermark_blocks: 0,
                ..snapshot_close_input()
            }),
            Err(CapacityError::UnsafeCapacityProfile)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                empty_tape_usable_blocks: 2_060,
                remaining_tape_blocks: 2_060,
                high_watermark_blocks: 0,
                ..snapshot_close_input()
            }),
            Err(CapacityError::UnsafeCapacityProfile)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                high_watermark_blocks: 100,
                ..snapshot_close_input()
            }),
            Err(CapacityError::UnsafeCapacityProfile)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                high_watermark_blocks: 2_172,
                ..snapshot_close_input()
            }),
            Err(CapacityError::UnsafeCapacityProfile)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                structural_entries_before_object: 2_172,
                ..snapshot_close_input()
            }),
            Err(CapacityError::StructuralEntriesExceedCapacity)
        );

        assert_eq!(
            evaluate_snapshot_close(SnapshotCloseInput {
                projected_object_blocks: 512 * 128 * 3,
                empty_tape_usable_blocks: 4_111,
                remaining_tape_blocks: 4_111,
                ..snapshot_close_input()
            }),
            Err(CapacityError::ObjectTooLargeForEmptyTape)
        );
    }

    #[test]
    fn sample_report_matches_production_fixture() {
        let report = evaluate(sample_input()).expect("reserve fits");
        assert_eq!(report.epochs_completed_by_object, 2);
        assert!(report.final_partial_sidecar_needed);
        assert_eq!(report.sidecar_tape_file_blocks, 12);
        assert_eq!(report.bootstrap_tape_file_blocks, 2);
        assert_eq!(report.reserve_after_object_blocks, 56);
        assert_eq!(report.required_tape_blocks, 76);
        assert_eq!(report.required_spool_bytes, 31 * 1024);
    }

    #[test]
    fn gate_order_matches_production_capacity_distinctions() {
        assert_eq!(
            evaluate(CapacityReserveInput {
                empty_tape_usable_blocks: 75,
                remaining_tape_blocks: 75,
                ..sample_input()
            })
            .unwrap_err(),
            CapacityError::ObjectTooLargeForEmptyTape
        );
        assert_eq!(
            evaluate(CapacityReserveInput {
                remaining_tape_blocks: 75,
                ..sample_input()
            })
            .unwrap_err(),
            CapacityError::CapacityReserveExceededTape
        );
        assert_eq!(
            evaluate(CapacityReserveInput {
                remaining_spool_bytes: 31 * 1024 - 1,
                ..sample_input()
            })
            .unwrap_err(),
            CapacityError::CapacityReserveExceededSpool
        );
    }
}
