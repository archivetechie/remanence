//! Verification extraction of the exact terminal-triple capacity arithmetic.
//!
//! This crate is a standalone, dependency-free model of
//! `crates/remanence-parity/src/capacity.rs`'s Object-admission/manual-close
//! calculation. It preserves the production arithmetic and branch ordering but
//! replaces the full production `ParityError` payloads with compact proof-facing
//! variants. The `drift_guard` test pins the production formulas this extraction
//! mirrors; if it fails, the extraction and Lean proofs must be re-synced.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapacityError {
    BlockSizeZero,
    UnsupportedBlockSize,
    DataShardsPerEpochZero,
    ParityOffHasState,
    ProfileNeighborhoodTooLarge,
    CurrentEpochFillOutsideOpenEpoch,
    ObjectRowsExceedStructuralEntries,
    SidecarRowsExceedStructuralEntries,
    RecoveryRowsExceedStructuralEntries,
    StructuralEntriesExceedCapacity,
    MissingBotBootstrap,
    ProjectedObjectPresenceMismatch,
    GapExtentSizeMismatch,
    UnsafeCapacityProfile,
    CapacityProfileCloseExceedsCapacity,
    CapacityPolicyInvalid,
    SidecarDirectoryExceedsCapacity,
    SidecarEntryDoesNotFit,
    ReplicatedControlHeaderTooLarge,
    ArithmeticOverflow,
    CapacityReserveExceededTape,
    CapacityReserveExceededSpool,
}

/// Proof-facing inputs for Object admission and operator terminal close-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalTripleCloseInput {
    pub projected_object_present: bool,
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
    pub replica_filemark_blocks: u64,
    pub gap_filemark_blocks: u64,
    pub gap_nominal_bytes: u64,
    pub safety_margin_blocks: u64,
    pub remaining_tape_blocks: u64,
    pub capacity_basis_blocks: u64,
    pub low_watermark_blocks: u64,
    pub high_watermark_blocks: u64,
    pub pending_completed_epoch_parity_bytes: u64,
    pub remaining_spool_bytes: u64,
}

/// Checked nonoverlapping terms in the terminal-triple close guarantee.
#[derive(Clone, Copy, Debug)]
pub struct TerminalTripleCloseReport {
    pub projected_object_present: bool,
    pub epochs_completed_by_object: u64,
    pub final_partial_sidecar_needed: bool,
    pub sidecar_index_block_count: u64,
    pub sidecar_blocks_before_filemark: u64,
    pub sidecar_tape_file_blocks: u64,
    pub sidecars_emitted_by_commit: u64,
    pub sidecar_blocks_emitted_by_commit: u64,
    pub object_tape_file_blocks: u64,
    pub prefix_commit_charge_blocks: u64,
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
    pub replica_payload_bytes: u64,
    pub replica_payload_record_count: u64,
    pub replica_records_before_filemark: u64,
    pub replica_tape_file_blocks: u64,
    pub triple_replica_blocks: u64,
    pub gap_nominal_bytes: u64,
    pub gap_records_before_filemark: u64,
    pub gap_tape_file_blocks: u64,
    pub double_gap_blocks: u64,
    pub parity_closeout_charge_blocks: u64,
    pub terminal_tail_charge_blocks: u64,
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

pub fn terminal_structural_slot_bytes() -> u64 {
    64
}

pub fn terminal_object_row_slot_bytes() -> u64 {
    256
}

pub fn parity_map_header_bytes() -> u64 {
    200
}

pub fn sidecar_header_bytes() -> u64 {
    200
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityMapCapacityLayout {
    pub payload_bound_bytes: u64,
    pub blocks_before_filemark: u64,
    pub tape_file_blocks: u64,
}

pub fn checked_parity_map_capacity_layout(
    block_size_bytes: u64,
    sidecar_entry_count: u64,
    filemark_blocks: u64,
) -> Result<ParityMapCapacityLayout, CapacityError> {
    let payload_bound_bytes = parity_map_payload_len_upper_bound(sidecar_entry_count)?;
    let blocks_before_filemark = replicated_control_total_blocks(
        block_size_bytes,
        parity_map_header_bytes(),
        payload_bound_bytes,
    )?;
    let tape_file_blocks = checked_add(blocks_before_filemark, filemark_blocks)?;
    Ok(ParityMapCapacityLayout {
        payload_bound_bytes,
        blocks_before_filemark,
        tape_file_blocks,
    })
}

pub fn supported_terminal_block_size(block_size_bytes: u64) -> bool {
    block_size_bytes == 262_144 || block_size_bytes == 524_288 || block_size_bytes == 1_048_576
}

/// Checked `copy 1 + copy 2 + footer` geometry used by external ParityMaps.
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

pub fn terminal_payload_bytes(
    structural_entry_count: u64,
    object_row_count: u64,
) -> Result<u64, CapacityError> {
    if object_row_count > structural_entry_count {
        return Err(CapacityError::ObjectRowsExceedStructuralEntries);
    }
    let structural = checked_mul(structural_entry_count, terminal_structural_slot_bytes())?;
    let rows = checked_mul(object_row_count, terminal_object_row_slot_bytes())?;
    checked_add(structural, rows)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSidecarTerms {
    pub index_block_count: u64,
    pub blocks_before_filemark: u64,
    pub tape_file_blocks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalProjectionTerms {
    pub epochs_completed_by_object: u64,
    pub final_partial_sidecar_needed: bool,
    pub sidecars_emitted_by_commit: u64,
    pub sidecar_blocks_emitted_by_commit: u64,
    pub object_tape_file_blocks: u64,
    pub prefix_commit_charge_blocks: u64,
    pub object_rows_after: u64,
    pub sidecar_entries_after_closeout: u64,
    pub maximum_sidecar_entries_for_capacity: u64,
    pub structural_entries_after_closeout: u64,
    pub final_parity_map_needed: bool,
    pub final_parity_map_directory_bound_bytes: u64,
    pub final_parity_map_payload_bound_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalControlTerms {
    pub final_partial_sidecar_blocks: u64,
    pub final_parity_map_blocks_before_filemark: u64,
    pub final_parity_map_tape_file_blocks: u64,
    pub replica_payload_bytes: u64,
    pub replica_payload_record_count: u64,
    pub replica_records_before_filemark: u64,
    pub replica_tape_file_blocks: u64,
    pub triple_replica_blocks: u64,
    pub gap_records_before_filemark: u64,
    pub gap_tape_file_blocks: u64,
    pub double_gap_blocks: u64,
    pub parity_closeout_charge_blocks: u64,
    pub terminal_tail_charge_blocks: u64,
    pub close_bound_blocks: u64,
}

pub fn terminal_replica_layout(
    block_size_bytes: u64,
    structural_entry_count: u64,
    object_row_count: u64,
) -> Result<(u64, u64, u64), CapacityError> {
    if !supported_terminal_block_size(block_size_bytes) {
        return Err(CapacityError::UnsupportedBlockSize);
    }
    let payload_bytes = terminal_payload_bytes(structural_entry_count, object_row_count)?;
    let adjusted = checked_add(payload_bytes, checked_sub(block_size_bytes, 1)?)?;
    let payload_record_count = adjusted / block_size_bytes;
    let records_before_filemark = checked_add(payload_record_count, 2)?;
    Ok((payload_bytes, payload_record_count, records_before_filemark))
}

pub fn index_separation_records(
    block_size_bytes: u64,
    extent_bytes: u64,
) -> Result<u64, CapacityError> {
    if !supported_terminal_block_size(block_size_bytes) {
        return Err(CapacityError::UnsupportedBlockSize);
    }
    let adjusted = checked_add(extent_bytes, checked_sub(block_size_bytes, 1)?)?;
    let records = adjusted / block_size_bytes;
    if records < 2 {
        return Err(CapacityError::UnsafeCapacityProfile);
    }
    Ok(records)
}

pub fn validate_terminal_close_input(input: TerminalTripleCloseInput) -> Result<(), CapacityError> {
    if input.remaining_tape_blocks > input.capacity_basis_blocks {
        return Err(CapacityError::CapacityPolicyInvalid);
    }
    if input.low_watermark_blocks >= input.high_watermark_blocks
        || input.high_watermark_blocks > input.capacity_basis_blocks
    {
        return Err(CapacityError::CapacityPolicyInvalid);
    }
    if !supported_terminal_block_size(input.block_size_bytes) {
        return Err(CapacityError::UnsupportedBlockSize);
    }
    if input.projected_object_present != (input.projected_object_blocks != 0) {
        return Err(CapacityError::ProjectedObjectPresenceMismatch);
    }
    if input.gap_nominal_bytes != 1_073_741_824 {
        return Err(CapacityError::GapExtentSizeMismatch);
    }
    if input.data_shards_per_epoch == 0 {
        return Err(CapacityError::DataShardsPerEpochZero);
    }
    let parity_enabled = input.parity_shards_per_epoch != 0;
    if !parity_enabled
        && (input.current_epoch_fill_blocks != 0
            || input.pending_completed_sidecars != 0
            || input.sidecar_entries_before_object != 0
            || input.pending_completed_epoch_parity_bytes != 0)
    {
        return Err(CapacityError::ParityOffHasState);
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
    if input.structural_entries_before_object > input.capacity_basis_blocks {
        return Err(CapacityError::StructuralEntriesExceedCapacity);
    }
    if input.structural_entries_before_object == 0 {
        return Err(CapacityError::MissingBotBootstrap);
    }
    Ok(())
}

pub fn compute_terminal_sidecar_terms(
    input: TerminalTripleCloseInput,
) -> Result<TerminalSidecarTerms, CapacityError> {
    if input.parity_shards_per_epoch == 0 {
        return Ok(TerminalSidecarTerms {
            index_block_count: 0,
            blocks_before_filemark: 0,
            tape_file_blocks: 0,
        });
    }
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
    Ok(TerminalSidecarTerms {
        index_block_count: layout.block_count,
        blocks_before_filemark,
        tape_file_blocks,
    })
}

/// Exact terminal partial-sidecar charge for the projected epoch remainder.
/// The parity shard count is fixed by the scheme, while the CRC index contains
/// only the data shards actually present in the partial epoch.
pub fn final_partial_sidecar_tape_file_blocks(
    input: TerminalTripleCloseInput,
) -> Result<u64, CapacityError> {
    if input.data_shards_per_epoch == 0 {
        return Err(CapacityError::DataShardsPerEpochZero);
    }
    let projected_epoch_fill = checked_add(
        input.current_epoch_fill_blocks,
        input.projected_object_blocks,
    )?;
    let data_crc_entry_count = projected_epoch_fill % input.data_shards_per_epoch;
    let layout = checked_sidecar_index_capacity_layout(
        input.block_size_bytes,
        input.parity_shards_per_epoch,
        data_crc_entry_count,
    )?;
    let replicated_index_blocks = checked_mul(2, layout.block_count)?;
    let blocks_before_filemark = checked_add(
        checked_add(replicated_index_blocks, input.parity_shards_per_epoch)?,
        1,
    )?;
    checked_add(blocks_before_filemark, input.sidecar_filemark_blocks)
}

/// Validate capacity-derived worst-case control counts without allocating any
/// hypothetical rows and return the physical sidecar-directory ceiling.
pub fn validate_capacity_derived_profile_bounds(
    input: TerminalTripleCloseInput,
    parity_enabled: bool,
    maximum_complete_sidecar_tape_file_blocks: u64,
) -> Result<u64, CapacityError> {
    let closeout_budget_blocks =
        checked_sub(input.capacity_basis_blocks, input.high_watermark_blocks)?;
    if parity_enabled && maximum_complete_sidecar_tape_file_blocks > input.capacity_basis_blocks {
        return Err(CapacityError::CapacityProfileCloseExceedsCapacity);
    }
    let (maximum_sidecar_entries_for_capacity, maximum_parity_map_tape_file_blocks) =
        if parity_enabled {
            let minimum_sidecar_tape_file_blocks = checked_add(
                checked_add(input.parity_shards_per_epoch, 3)?,
                input.sidecar_filemark_blocks,
            )?;
            let maximum_sidecar_entries_for_capacity =
                input.capacity_basis_blocks / minimum_sidecar_tape_file_blocks;
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
            (
                maximum_sidecar_entries_for_capacity,
                maximum_parity_map_tape_file_blocks,
            )
        } else {
            (0, 0)
        };
    let (_, _, maximum_replica_records_before_filemark) = terminal_replica_layout(
        input.block_size_bytes,
        input.capacity_basis_blocks,
        input.capacity_basis_blocks,
    )?;
    let maximum_replica_tape_file_blocks = checked_add(
        maximum_replica_records_before_filemark,
        input.replica_filemark_blocks,
    )?;
    let maximum_triple_replica_blocks = checked_mul(3, maximum_replica_tape_file_blocks)?;
    let gap_records = index_separation_records(input.block_size_bytes, input.gap_nominal_bytes)?;
    let gap_tape_file_blocks = checked_add(gap_records, input.gap_filemark_blocks)?;
    let double_gap_blocks = checked_mul(2, gap_tape_file_blocks)?;
    let maximum_close_step1 = checked_add(
        maximum_complete_sidecar_tape_file_blocks,
        maximum_parity_map_tape_file_blocks,
    )?;
    let maximum_close_step2 = checked_add(maximum_close_step1, maximum_triple_replica_blocks)?;
    let maximum_close_step3 = checked_add(maximum_close_step2, double_gap_blocks)?;
    let maximum_close_bound_blocks = checked_add(maximum_close_step3, input.safety_margin_blocks)?;
    if maximum_close_bound_blocks > closeout_budget_blocks {
        return Err(CapacityError::CapacityProfileCloseExceedsCapacity);
    }
    Ok(maximum_sidecar_entries_for_capacity)
}

pub fn compute_terminal_projection_terms(
    input: TerminalTripleCloseInput,
    sidecar: TerminalSidecarTerms,
    maximum_sidecar_entries_for_capacity: u64,
) -> Result<TerminalProjectionTerms, CapacityError> {
    let parity_enabled = input.parity_shards_per_epoch != 0;
    let (epochs_completed_by_object, final_partial_sidecar_needed) = if parity_enabled {
        let projected_epoch_fill = checked_add(
            input.current_epoch_fill_blocks,
            input.projected_object_blocks,
        )?;
        (
            projected_epoch_fill / input.data_shards_per_epoch,
            projected_epoch_fill % input.data_shards_per_epoch != 0,
        )
    } else {
        (0, false)
    };
    let sidecars_emitted_by_commit =
        checked_add(input.pending_completed_sidecars, epochs_completed_by_object)?;
    let sidecar_blocks_emitted_by_commit =
        checked_mul(sidecars_emitted_by_commit, sidecar.tape_file_blocks)?;
    let object_tape_file_blocks = if input.projected_object_present {
        checked_add(input.projected_object_blocks, input.object_filemark_blocks)?
    } else {
        0
    };
    let prefix_commit_charge_blocks =
        checked_add(object_tape_file_blocks, sidecar_blocks_emitted_by_commit)?;
    let projected_object_count = if input.projected_object_present { 1 } else { 0 };
    let object_rows_after = checked_add(input.object_rows_before_object, projected_object_count)?;
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
        checked_add(
            input.structural_entries_before_object,
            projected_object_count,
        )?,
        sidecars_emitted_by_commit,
    )?;
    let structural_entries_after_closeout = checked_add(
        checked_add(structural_entries_after_commit, final_partial_sidecar_count)?,
        final_parity_map_count,
    )?;
    Ok(TerminalProjectionTerms {
        epochs_completed_by_object,
        final_partial_sidecar_needed,
        sidecars_emitted_by_commit,
        sidecar_blocks_emitted_by_commit,
        object_tape_file_blocks,
        prefix_commit_charge_blocks,
        object_rows_after,
        sidecar_entries_after_closeout,
        maximum_sidecar_entries_for_capacity,
        structural_entries_after_closeout,
        final_parity_map_needed,
        final_parity_map_directory_bound_bytes,
        final_parity_map_payload_bound_bytes,
    })
}

pub fn compute_terminal_control_terms(
    input: TerminalTripleCloseInput,
    _sidecar: TerminalSidecarTerms,
    projection: TerminalProjectionTerms,
) -> Result<TerminalControlTerms, CapacityError> {
    let (replica_payload_bytes, replica_payload_record_count, replica_records_before_filemark) =
        terminal_replica_layout(
            input.block_size_bytes,
            projection.structural_entries_after_closeout,
            projection.object_rows_after,
        )?;
    let replica_tape_file_blocks = checked_add(
        replica_records_before_filemark,
        input.replica_filemark_blocks,
    )?;
    let triple_replica_blocks = checked_mul(3, replica_tape_file_blocks)?;
    let gap_records_before_filemark =
        index_separation_records(input.block_size_bytes, input.gap_nominal_bytes)?;
    let gap_tape_file_blocks = checked_add(gap_records_before_filemark, input.gap_filemark_blocks)?;
    let double_gap_blocks = checked_mul(2, gap_tape_file_blocks)?;
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
        final_partial_sidecar_tape_file_blocks(input)?
    } else {
        0
    };
    let parity_closeout_charge_blocks = checked_add(
        final_partial_sidecar_blocks,
        final_parity_map_tape_file_blocks,
    )?;
    let terminal_tail_charge_blocks = checked_add(triple_replica_blocks, double_gap_blocks)?;
    let close_step1 = checked_add(parity_closeout_charge_blocks, terminal_tail_charge_blocks)?;
    let close_bound_blocks = checked_add(close_step1, input.safety_margin_blocks)?;
    Ok(TerminalControlTerms {
        final_partial_sidecar_blocks,
        final_parity_map_blocks_before_filemark,
        final_parity_map_tape_file_blocks,
        replica_payload_bytes,
        replica_payload_record_count,
        replica_records_before_filemark,
        replica_tape_file_blocks,
        triple_replica_blocks,
        gap_records_before_filemark,
        gap_tape_file_blocks,
        double_gap_blocks,
        parity_closeout_charge_blocks,
        terminal_tail_charge_blocks,
        close_bound_blocks,
    })
}

/// Evaluate one optional Object commit plus the complete terminal close bound.
pub fn evaluate_terminal_close(
    input: TerminalTripleCloseInput,
) -> Result<TerminalTripleCloseReport, CapacityError> {
    validate_terminal_close_input(input)?;
    let parity_enabled = input.parity_shards_per_epoch != 0;
    let sidecar = compute_terminal_sidecar_terms(input)?;
    let maximum_sidecar_entries_for_capacity = match validate_capacity_derived_profile_bounds(
        input,
        parity_enabled,
        sidecar.tape_file_blocks,
    ) {
        Ok(value) => value,
        Err(_) => return Err(CapacityError::UnsafeCapacityProfile),
    };
    let projection =
        compute_terminal_projection_terms(input, sidecar, maximum_sidecar_entries_for_capacity)?;
    let control = compute_terminal_control_terms(input, sidecar, projection)?;
    let required_tape_blocks = checked_add(
        projection.prefix_commit_charge_blocks,
        control.close_bound_blocks,
    )?;
    if input.remaining_tape_blocks < required_tape_blocks {
        return Err(CapacityError::CapacityReserveExceededTape);
    }
    let required_spool_bytes = if parity_enabled {
        let sidecar_tape_file_bytes =
            checked_mul(sidecar.blocks_before_filemark, input.block_size_bytes)?;
        let newly_completed_sidecar_bytes = checked_mul(
            projection.epochs_completed_by_object,
            sidecar_tape_file_bytes,
        )?;
        checked_add(
            input.pending_completed_epoch_parity_bytes,
            newly_completed_sidecar_bytes,
        )?
    } else {
        0
    };
    if input.remaining_spool_bytes < required_spool_bytes {
        return Err(CapacityError::CapacityReserveExceededSpool);
    }
    Ok(TerminalTripleCloseReport {
        projected_object_present: input.projected_object_present,
        epochs_completed_by_object: projection.epochs_completed_by_object,
        final_partial_sidecar_needed: projection.final_partial_sidecar_needed,
        sidecar_index_block_count: sidecar.index_block_count,
        sidecar_blocks_before_filemark: sidecar.blocks_before_filemark,
        sidecar_tape_file_blocks: sidecar.tape_file_blocks,
        sidecars_emitted_by_commit: projection.sidecars_emitted_by_commit,
        sidecar_blocks_emitted_by_commit: projection.sidecar_blocks_emitted_by_commit,
        object_tape_file_blocks: projection.object_tape_file_blocks,
        prefix_commit_charge_blocks: projection.prefix_commit_charge_blocks,
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
        replica_payload_bytes: control.replica_payload_bytes,
        replica_payload_record_count: control.replica_payload_record_count,
        replica_records_before_filemark: control.replica_records_before_filemark,
        replica_tape_file_blocks: control.replica_tape_file_blocks,
        triple_replica_blocks: control.triple_replica_blocks,
        gap_nominal_bytes: input.gap_nominal_bytes,
        gap_records_before_filemark: control.gap_records_before_filemark,
        gap_tape_file_blocks: control.gap_tape_file_blocks,
        double_gap_blocks: control.double_gap_blocks,
        parity_closeout_charge_blocks: control.parity_closeout_charge_blocks,
        terminal_tail_charge_blocks: control.terminal_tail_charge_blocks,
        safety_margin_blocks: input.safety_margin_blocks,
        close_bound_blocks: control.close_bound_blocks,
        required_tape_blocks,
        required_spool_bytes,
    })
}

/// Proof-facing five-component terminal progress. Replica count is derived,
/// never used as the state authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalTailProgress {
    BeforeReplicaA,
    AfterReplicaA,
    AfterSeparationAb,
    AfterReplicaB,
    AfterSeparationBc,
    AfterReplicaC,
}

/// Number of complete replicas implied by authoritative component progress.
pub fn completed_terminal_replicas(progress: TerminalTailProgress) -> u64 {
    match progress {
        TerminalTailProgress::BeforeReplicaA => 0,
        TerminalTailProgress::AfterReplicaA | TerminalTailProgress::AfterSeparationAb => 1,
        TerminalTailProgress::AfterReplicaB | TerminalTailProgress::AfterSeparationBc => 2,
        TerminalTailProgress::AfterReplicaC => 3,
    }
}

/// Advance exactly one component only after its synchronous barrier succeeds.
pub fn advance_terminal_progress(
    progress: TerminalTailProgress,
    barrier_succeeded: bool,
) -> TerminalTailProgress {
    if !barrier_succeeded {
        return progress;
    }
    match progress {
        TerminalTailProgress::BeforeReplicaA => TerminalTailProgress::AfterReplicaA,
        TerminalTailProgress::AfterReplicaA => TerminalTailProgress::AfterSeparationAb,
        TerminalTailProgress::AfterSeparationAb => TerminalTailProgress::AfterReplicaB,
        TerminalTailProgress::AfterReplicaB => TerminalTailProgress::AfterSeparationBc,
        TerminalTailProgress::AfterSeparationBc => TerminalTailProgress::AfterReplicaC,
        TerminalTailProgress::AfterReplicaC => TerminalTailProgress::AfterReplicaC,
    }
}

/// Finalizing is irreversible and therefore excludes later Object admission.
pub fn object_admission_allowed(finalizing: bool) -> bool {
    !finalizing
}

/// Ordinary sealed projection is possible only after replica C is durable.
pub fn sealed_projection_allowed(progress: TerminalTailProgress) -> bool {
    match progress {
        TerminalTailProgress::AfterReplicaC => true,
        TerminalTailProgress::BeforeReplicaA
        | TerminalTailProgress::AfterReplicaA
        | TerminalTailProgress::AfterSeparationAb
        | TerminalTailProgress::AfterReplicaB
        | TerminalTailProgress::AfterSeparationBc => false,
    }
}

/// Proof-facing latest-valid replica selection outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalReplicaSelection {
    ReplicaA,
    ReplicaB,
    ReplicaC,
    FullBotScan,
    Conflict,
}

/// Choose C, then B, then A. Two or more valid survivors must agree on their
/// common edition/scope/payload facts or selection fails closed as conflict.
pub fn select_terminal_replica(
    replica_a_valid: bool,
    replica_b_valid: bool,
    replica_c_valid: bool,
    surviving_replicas_agree: bool,
) -> TerminalReplicaSelection {
    let survivor_count =
        (replica_a_valid as u8) + (replica_b_valid as u8) + (replica_c_valid as u8);
    if survivor_count > 1 && !surviving_replicas_agree {
        TerminalReplicaSelection::Conflict
    } else if replica_c_valid {
        TerminalReplicaSelection::ReplicaC
    } else if replica_b_valid {
        TerminalReplicaSelection::ReplicaB
    } else if replica_a_valid {
        TerminalReplicaSelection::ReplicaA
    } else {
        TerminalReplicaSelection::FullBotScan
    }
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
        let tape_index_replica = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/tape_index_replica.rs"
        ))
        .expect("tape_index_replica.rs must be readable from verif/parity-capacity");
        let index_separation = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/index_separation.rs"
        ))
        .expect("index_separation.rs must be readable from verif/parity-capacity");
        let replicated_control = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/replicated_control.rs"
        ))
        .expect("replicated_control.rs must be readable from verif/parity-capacity");

        let candidate_sources: &[(&str, &str, &[&str])] = &[
            (
                "capacity.rs",
                &capacity,
                &[
                    "pub struct TerminalTripleCloseInput",
                    "pub struct TerminalTripleCloseReport",
                    "checked_sidecar_index_capacity_layout(",
                    "parity_map_directory_len_upper_bound(sidecar_entries_after_closeout)?",
                    "parity_map_payload_len_upper_bound(sidecar_entries_after_closeout)?",
                    "let triple_replica_blocks = checked_mul(3, replica_tape_file_blocks)?;",
                    "let double_gap_blocks = checked_mul(2, gap_tape_file_blocks)?;",
                    "let parity_closeout_charge_blocks = checked_add(",
                    "let terminal_tail_charge_blocks = checked_add(triple_replica_blocks, double_gap_blocks)?;",
                    "let required_tape_blocks = checked_add(prefix_commit_charge_blocks, close_bound_blocks)?;",
                    "self.capacity_basis_blocks",
                    "self.low_watermark_blocks",
                    "self.high_watermark_blocks",
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
                    "Ok((layout.block_count, layout.inline_entry_bytes))",
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
                "tape_index_replica.rs",
                &tape_index_replica,
                &[
                    "pub fn checked_tape_index_payload_len(",
                    "pub fn checked_tape_index_replica_layout(",
                    "let replica_record_count =",
                    "payload_record_count",
                ],
            ),
            (
                "index_separation.rs",
                &index_separation,
                &["pub fn index_separation_records(", "let adjusted = extent_bytes.checked_add("],
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
                "impl TerminalTripleCloseInput {",
                "\nfn checked_add(",
                20_542,
                0x5f89_ec53_8caa_3d97,
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
                594,
                0x8829_827e_de38_c4be,
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
            "pub fn checked_sidecar_index_capacity_layout(",
            "pub fn parity_map_directory_len_upper_bound(",
            "pub fn parity_map_payload_len_upper_bound(",
            "pub fn checked_parity_map_capacity_layout(",
            "pub fn replicated_control_total_blocks(",
            "pub fn terminal_payload_bytes(",
            "pub fn terminal_replica_layout(",
            "pub fn index_separation_records(",
            "pub fn evaluate_terminal_close(",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif capacity model"
            );
        }
    }

    fn production_terminal_input(
        input: TerminalTripleCloseInput,
    ) -> production::TerminalTripleCloseInput {
        production::TerminalTripleCloseInput {
            projected_object_present: input.projected_object_present,
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
            replica_filemark_blocks: input.replica_filemark_blocks,
            gap_filemark_blocks: input.gap_filemark_blocks,
            gap_nominal_bytes: input.gap_nominal_bytes,
            safety_margin_blocks: input.safety_margin_blocks,
            remaining_tape_blocks: input.remaining_tape_blocks,
            capacity_basis_blocks: input.capacity_basis_blocks,
            low_watermark_blocks: input.low_watermark_blocks,
            high_watermark_blocks: input.high_watermark_blocks,
            pending_completed_epoch_parity_bytes: input.pending_completed_epoch_parity_bytes,
            remaining_spool_bytes: input.remaining_spool_bytes,
        }
    }

    fn localize_production_report(
        report: production::TerminalTripleCloseReport,
    ) -> TerminalTripleCloseReport {
        TerminalTripleCloseReport {
            projected_object_present: report.projected_object_present,
            epochs_completed_by_object: report.epochs_completed_by_object,
            final_partial_sidecar_needed: report.final_partial_sidecar_needed,
            sidecar_index_block_count: report.sidecar_index_block_count,
            sidecar_blocks_before_filemark: report.sidecar_blocks_before_filemark,
            sidecar_tape_file_blocks: report.sidecar_tape_file_blocks,
            sidecars_emitted_by_commit: report.sidecars_emitted_by_commit,
            sidecar_blocks_emitted_by_commit: report.sidecar_blocks_emitted_by_commit,
            object_tape_file_blocks: report.object_tape_file_blocks,
            prefix_commit_charge_blocks: report.prefix_commit_charge_blocks,
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
            replica_payload_bytes: report.replica_payload_bytes,
            replica_payload_record_count: report.replica_payload_record_count,
            replica_records_before_filemark: report.replica_records_before_filemark,
            replica_tape_file_blocks: report.replica_tape_file_blocks,
            triple_replica_blocks: report.triple_replica_blocks,
            gap_nominal_bytes: report.gap_nominal_bytes,
            gap_records_before_filemark: report.gap_records_before_filemark,
            gap_tape_file_blocks: report.gap_tape_file_blocks,
            double_gap_blocks: report.double_gap_blocks,
            parity_closeout_charge_blocks: report.parity_closeout_charge_blocks,
            terminal_tail_charge_blocks: report.terminal_tail_charge_blocks,
            safety_margin_blocks: report.safety_margin_blocks,
            close_bound_blocks: report.close_bound_blocks,
            required_tape_blocks: report.required_tape_blocks,
            required_spool_bytes: report.required_spool_bytes,
        }
    }

    fn production_terminal_report(input: TerminalTripleCloseInput) -> TerminalTripleCloseReport {
        production_terminal_input(input)
            .evaluate()
            .map(localize_production_report)
            .unwrap_or_else(|error| {
                panic!("production rejected valid matrix input {input:?}: {error}")
            })
    }

    fn assert_terminal_reports_equal(
        actual: TerminalTripleCloseReport,
        expected: TerminalTripleCloseReport,
    ) {
        macro_rules! assert_fields_equal {
            ($($field:ident),+ $(,)?) => {
                $(assert_eq!(actual.$field, expected.$field, stringify!($field));)+
            };
        }
        assert_fields_equal!(
            projected_object_present,
            epochs_completed_by_object,
            final_partial_sidecar_needed,
            sidecar_index_block_count,
            sidecar_blocks_before_filemark,
            sidecar_tape_file_blocks,
            sidecars_emitted_by_commit,
            sidecar_blocks_emitted_by_commit,
            object_tape_file_blocks,
            prefix_commit_charge_blocks,
            object_rows_after,
            sidecar_entries_after_closeout,
            maximum_sidecar_entries_for_capacity,
            structural_entries_after_closeout,
            final_partial_sidecar_blocks,
            final_parity_map_needed,
            final_parity_map_directory_bound_bytes,
            final_parity_map_payload_bound_bytes,
            final_parity_map_blocks_before_filemark,
            final_parity_map_tape_file_blocks,
            replica_payload_bytes,
            replica_payload_record_count,
            replica_records_before_filemark,
            replica_tape_file_blocks,
            triple_replica_blocks,
            gap_nominal_bytes,
            gap_records_before_filemark,
            gap_tape_file_blocks,
            double_gap_blocks,
            parity_closeout_charge_blocks,
            terminal_tail_charge_blocks,
            safety_margin_blocks,
            close_bound_blocks,
            required_tape_blocks,
            required_spool_bytes,
        );
    }

    fn local_gate(error: CapacityError) -> &'static str {
        match error {
            CapacityError::CapacityReserveExceededTape => "current-tape",
            CapacityError::CapacityReserveExceededSpool => "spool",
            other => panic!("unexpected extraction error in gate matrix: {other:?}"),
        }
    }

    fn production_gate(error: production::ParityError) -> &'static str {
        match error {
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
                if message.starts_with("unsafe terminal close capacity profile:") =>
            {
                "unsafe-capacity-profile"
            }
            other => panic!("unexpected production profile error: {other:?}"),
        }
    }

    fn terminal_close_input() -> TerminalTripleCloseInput {
        TerminalTripleCloseInput {
            projected_object_present: true,
            projected_object_blocks: 100,
            block_size_bytes: 256 * 1024,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 512 * 128,
            parity_shards_per_epoch: 512 * 4,
            pending_completed_sidecars: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 1,
            object_rows_before_object: 0,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            parity_map_filemark_blocks: 1,
            replica_filemark_blocks: 1,
            gap_filemark_blocks: 1,
            gap_nominal_bytes: 1 << 30,
            safety_margin_blocks: 4,
            remaining_tape_blocks: 10_371,
            capacity_basis_blocks: 10_371,
            low_watermark_blocks: 0,
            high_watermark_blocks: 65,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: u64::MAX,
        }
    }

    #[test]
    fn terminal_progress_and_selection_contract() {
        let states = [
            TerminalTailProgress::BeforeReplicaA,
            TerminalTailProgress::AfterReplicaA,
            TerminalTailProgress::AfterSeparationAb,
            TerminalTailProgress::AfterReplicaB,
            TerminalTailProgress::AfterSeparationBc,
            TerminalTailProgress::AfterReplicaC,
        ];
        for state in states {
            assert_eq!(advance_terminal_progress(state, false), state);
            assert!(
                completed_terminal_replicas(advance_terminal_progress(state, true))
                    >= completed_terminal_replicas(state)
            );
        }
        assert!(!object_admission_allowed(true));
        assert!(object_admission_allowed(false));
        assert!(sealed_projection_allowed(
            TerminalTailProgress::AfterReplicaC
        ));
        assert!(!sealed_projection_allowed(
            TerminalTailProgress::AfterSeparationBc
        ));

        assert_eq!(
            select_terminal_replica(true, true, true, true),
            TerminalReplicaSelection::ReplicaC
        );
        assert_eq!(
            select_terminal_replica(true, true, false, true),
            TerminalReplicaSelection::ReplicaB
        );
        assert_eq!(
            select_terminal_replica(true, false, false, false),
            TerminalReplicaSelection::ReplicaA
        );
        assert_eq!(
            select_terminal_replica(false, false, false, false),
            TerminalReplicaSelection::FullBotScan
        );
        assert_eq!(
            select_terminal_replica(true, false, true, false),
            TerminalReplicaSelection::Conflict
        );
    }

    #[test]
    fn no_parity_terminal_close_matches_production_without_sidecar_terms() {
        let mut input = terminal_close_input();
        input.parity_shards_per_epoch = 0;
        input.data_shards_per_epoch = 1;
        let local = evaluate_terminal_close(input).unwrap();
        let production = production_terminal_report(input);
        assert_terminal_reports_equal(local, production);
        assert_eq!(local.sidecar_index_block_count, 0);
        assert_eq!(local.sidecar_blocks_before_filemark, 0);
        assert_eq!(local.sidecar_tape_file_blocks, 0);
        assert_eq!(local.parity_closeout_charge_blocks, 0);
        assert_eq!(local.required_spool_bytes, 0);
        assert!(!local.final_parity_map_needed);
    }

    #[test]
    fn drift_guard_terminal_helpers_match_compiled_production_boundaries() {
        let counts = [1, 2, 4_095, 4_096, 4_097];
        for block_size in [256 * 1024, 512 * 1024, 1024 * 1024] {
            for structural in counts {
                for object_rows in [0, 1, structural] {
                    let extracted =
                        terminal_replica_layout(block_size, structural, object_rows).unwrap();
                    let compiled = production::checked_tape_index_replica_layout(
                        u32::try_from(block_size).unwrap(),
                        production::TapeIndexReplicaCounts {
                            structural_entry_count: structural,
                            object_row_count: object_rows,
                        },
                    )
                    .unwrap();
                    assert_eq!(
                        extracted,
                        (
                            compiled.payload_len,
                            compiled.payload_record_count,
                            compiled.replica_record_count,
                        ),
                        "replica geometry drift for B={block_size}, S={structural}, R={object_rows}"
                    );
                }
            }

            for extent_bytes in [2 * block_size - 1, 2 * block_size, (1u64 << 30) + 1] {
                assert_eq!(
                    index_separation_records(block_size, extent_bytes).ok(),
                    production::index_separation_records(
                        u32::try_from(block_size).unwrap(),
                        extent_bytes,
                    )
                    .ok(),
                    "gap geometry drift for B={block_size}, E={extent_bytes}"
                );
            }
        }

        let sidecar_counts = [0, 1, 7, 8, 15, 16, 17, 255, 256, 257];
        for block_size in [192, 200, 256, 512, 256 * 1024] {
            for parity_count in sidecar_counts {
                for data_count in sidecar_counts {
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
                    assert_eq!(extracted, compiled);
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
                production::parity_map_directory_len_upper_bound(entry_count).ok()
            );
            assert_eq!(
                parity_map_payload_len_upper_bound(entry_count).ok(),
                production::parity_map_payload_len_upper_bound(entry_count).ok()
            );
        }

        for block_size in [256 * 1024, 512 * 1024, 1024 * 1024] {
            for (data_shards, parity_shards) in [(1u64, 1u64), (2, 1), (65_536, 2_048)] {
                for (projected_object_present, projected_object_blocks) in [
                    (false, 0),
                    (true, 1),
                    (true, data_shards),
                    (true, data_shards + 1),
                ] {
                    let input = TerminalTripleCloseInput {
                        projected_object_present,
                        projected_object_blocks,
                        block_size_bytes: block_size,
                        current_epoch_fill_blocks: 0,
                        data_shards_per_epoch: data_shards,
                        parity_shards_per_epoch: parity_shards,
                        pending_completed_sidecars: 0,
                        sidecar_entries_before_object: 1,
                        structural_entries_before_object: 4,
                        object_rows_before_object: 2,
                        object_filemark_blocks: 1,
                        sidecar_filemark_blocks: 1,
                        parity_map_filemark_blocks: 1,
                        replica_filemark_blocks: 1,
                        gap_filemark_blocks: 1,
                        gap_nominal_bytes: 1 << 30,
                        safety_margin_blocks: 4,
                        remaining_tape_blocks: 10_000_000,
                        capacity_basis_blocks: 10_000_000,
                        low_watermark_blocks: 9_200_000,
                        high_watermark_blocks: 9_800_000,
                        pending_completed_epoch_parity_bytes: 17,
                        remaining_spool_bytes: u64::MAX,
                    };
                    let extracted = evaluate_terminal_close(input)
                        .unwrap_or_else(|error| panic!("extraction rejected {input:?}: {error:?}"));
                    assert_terminal_reports_equal(extracted, production_terminal_report(input));
                }
            }
        }
    }

    #[test]
    fn terminal_close_fixture_and_manual_close_match_production() {
        let input = terminal_close_input();
        let report = evaluate_terminal_close(input).expect("close fits");
        assert_terminal_reports_equal(report, production_terminal_report(input));
        assert_eq!(report.prefix_commit_charge_blocks, 101);
        assert_eq!(report.replica_payload_bytes, 512);
        assert_eq!(report.triple_replica_blocks, 12);
        assert_eq!(report.double_gap_blocks, 8_194);
        assert_eq!(report.parity_closeout_charge_blocks, 2_056);
        assert_eq!(report.close_bound_blocks, 10_266);
        assert_eq!(report.required_tape_blocks, 10_367);

        let manual = TerminalTripleCloseInput {
            projected_object_present: false,
            projected_object_blocks: 0,
            current_epoch_fill_blocks: 100,
            structural_entries_before_object: 2,
            object_rows_before_object: 1,
            remaining_tape_blocks: 999_900,
            capacity_basis_blocks: 1_000_000,
            low_watermark_blocks: 920_000,
            high_watermark_blocks: 980_000,
            ..input
        };
        let report = evaluate_terminal_close(manual).expect("manual close fits");
        assert_terminal_reports_equal(report, production_terminal_report(manual));
        assert_eq!(report.object_tape_file_blocks, 0);
        assert_eq!(report.object_rows_after, 1);
        assert_eq!(report.required_tape_blocks, report.close_bound_blocks);
    }

    #[test]
    fn terminal_close_gate_order_and_fail_closed_profile_match_production() {
        let baseline = TerminalTripleCloseInput {
            projected_object_blocks: 65_536,
            remaining_tape_blocks: 1_000_000,
            capacity_basis_blocks: 1_000_000,
            low_watermark_blocks: 0,
            high_watermark_blocks: 1,
            ..terminal_close_input()
        };
        let report = evaluate_terminal_close(baseline).expect("baseline close fits");

        for input in [
            TerminalTripleCloseInput {
                capacity_basis_blocks: report.required_tape_blocks - 1,
                remaining_tape_blocks: report.required_tape_blocks - 1,
                low_watermark_blocks: 0,
                high_watermark_blocks: 1,
                ..baseline
            },
            TerminalTripleCloseInput {
                capacity_basis_blocks: report.required_tape_blocks,
                remaining_tape_blocks: report.required_tape_blocks - 1,
                low_watermark_blocks: 0,
                high_watermark_blocks: 1,
                ..baseline
            },
            TerminalTripleCloseInput {
                capacity_basis_blocks: report.required_tape_blocks,
                remaining_tape_blocks: report.required_tape_blocks,
                low_watermark_blocks: 0,
                high_watermark_blocks: 1,
                remaining_spool_bytes: report.required_spool_bytes - 1,
                ..baseline
            },
        ] {
            assert_eq!(
                local_gate(evaluate_terminal_close(input).unwrap_err()),
                production_gate(production_terminal_input(input).evaluate().unwrap_err())
            );
        }

        for input in [
            TerminalTripleCloseInput {
                capacity_basis_blocks: u64::MAX,
                remaining_tape_blocks: u64::MAX,
                low_watermark_blocks: 0,
                high_watermark_blocks: 1,
                ..baseline
            },
            TerminalTripleCloseInput {
                data_shards_per_epoch: 2_000,
                parity_shards_per_epoch: 1_000,
                capacity_basis_blocks: 1_000,
                remaining_tape_blocks: 1_000,
                low_watermark_blocks: 0,
                high_watermark_blocks: 1,
                ..baseline
            },
            TerminalTripleCloseInput {
                high_watermark_blocks: terminal_close_input().high_watermark_blocks + 1,
                ..terminal_close_input()
            },
        ] {
            assert_eq!(
                local_profile_failure(evaluate_terminal_close(input).unwrap_err()),
                production_profile_failure(
                    production_terminal_input(input).evaluate().unwrap_err()
                )
            );
        }

        assert_eq!(
            terminal_payload_bytes(1, 2),
            Err(CapacityError::ObjectRowsExceedStructuralEntries)
        );
        assert!(matches!(
            evaluate_terminal_close(TerminalTripleCloseInput {
                projected_object_present: false,
                projected_object_blocks: 1,
                ..terminal_close_input()
            }),
            Err(CapacityError::ProjectedObjectPresenceMismatch)
        ));
        assert!(matches!(
            evaluate_terminal_close(TerminalTripleCloseInput {
                structural_entries_before_object: 0,
                ..terminal_close_input()
            }),
            Err(CapacityError::MissingBotBootstrap)
        ));
        let wrong_gap = TerminalTripleCloseInput {
            gap_nominal_bytes: (1 << 30) - 1,
            ..terminal_close_input()
        };
        assert!(matches!(
            evaluate_terminal_close(wrong_gap),
            Err(CapacityError::GapExtentSizeMismatch)
        ));
        assert!(matches!(
            production_terminal_input(wrong_gap).evaluate(),
            Err(production::ParityError::InvalidScheme(message))
                if message.starts_with("terminal close separation extent is ")
        ));
    }

    #[test]
    fn external_parity_map_boundary_and_count_are_checked() {
        let within = checked_parity_map_capacity_layout(4096, 30, 1).unwrap();
        assert_eq!(within.payload_bound_bytes, 3805);
        assert_eq!(within.blocks_before_filemark, 3);
        assert_eq!(within.tape_file_blocks, 4);

        let crossed = checked_parity_map_capacity_layout(4096, 31, 1).unwrap();
        assert_eq!(crossed.payload_bound_bytes, 3921);
        assert_eq!(crossed.blocks_before_filemark, 5);
        assert_eq!(crossed.tape_file_blocks, 6);
    }

    #[test]
    fn external_parity_map_overflow_fails_closed() {
        assert_eq!(
            checked_parity_map_capacity_layout(4096, u64::MAX / 116 + 1, 1),
            Err(CapacityError::ArithmeticOverflow)
        );
    }
}
