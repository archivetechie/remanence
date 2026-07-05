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
    DataShardsPerEpochZero,
    CurrentEpochFillOutsideOpenEpoch,
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

pub fn block_count_per_bootstrap() -> u64 {
    1
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
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-parity/src/capacity.rs"
        ))
        .expect("original capacity.rs must be readable from verif/parity-capacity");

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
                original.contains(snippet),
                "snippet {i} no longer in remanence-parity capacity.rs -- original \
                 changed; re-sync this extraction and its Lean proofs"
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
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif capacity model"
            );
        }
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
