//! Verification extraction of the pool-selection ranking kernel.
//!
//! This crate is a standalone, dependency-free model of the arithmetic and
//! lexicographic key logic in `crates/remanence-api/src/pool_selection.rs`.
//! The production policy uses slices, `Vec`, iterator adapters, trait objects,
//! and tuple `min_by_key`; this proof-facing crate extracts the stable kernel:
//! fit filtering, completion detection, leftover calculation, and the pairwise
//! ranking predicates for `CompleteOrFill` and `FillOldest`. UUIDs are modeled
//! as ordered `u64`s because the production key only needs deterministic final
//! ordering. The `drift_guard` test pins the ranking snippets and compiles the
//! production admission kernel into the test crate for semantic equivalence
//! checks across its branch and overflow boundaries.

pub type TapeUuid = u64;

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct TapeFitState {
    pub tape_uuid: TapeUuid,
    pub barcode_order: u64,
    pub already_loaded: bool,
    pub used_bytes: u64,
    pub usable_bytes: u64,
    pub low_bytes: u64,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub enum AdmissionDisposition {
    AdmitRemainOpen,
    AdmitThenFinalize,
    FinalizePrefixAndRetry,
    RejectInvalidCapacityPolicy,
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq, Eq))]
pub struct CapacityAdmissionInput {
    pub current_used_blocks: u64,
    pub object_commit_charge_blocks: u64,
    pub close_bound_blocks: u64,
    pub capacity_blocks: u64,
    pub low_watermark_blocks: u64,
    pub high_watermark_blocks: u64,
}

pub fn capacity_admission_disposition(input: CapacityAdmissionInput) -> AdmissionDisposition {
    if input.low_watermark_blocks > input.high_watermark_blocks
        || input.high_watermark_blocks > input.capacity_blocks
    {
        return AdmissionDisposition::RejectInvalidCapacityPolicy;
    }
    let projected_used_blocks = match input
        .current_used_blocks
        .checked_add(input.object_commit_charge_blocks)
    {
        Some(value) => value,
        None => return AdmissionDisposition::FinalizePrefixAndRetry,
    };
    if projected_used_blocks > input.high_watermark_blocks {
        return AdmissionDisposition::FinalizePrefixAndRetry;
    }
    let required_through_close = match projected_used_blocks.checked_add(input.close_bound_blocks) {
        Some(value) => value,
        None => return AdmissionDisposition::FinalizePrefixAndRetry,
    };
    if required_through_close > input.capacity_blocks {
        return AdmissionDisposition::FinalizePrefixAndRetry;
    }
    if projected_used_blocks < input.low_watermark_blocks {
        AdmissionDisposition::AdmitRemainOpen
    } else {
        AdmissionDisposition::AdmitThenFinalize
    }
}

pub fn loaded_key(candidate: TapeFitState) -> u8 {
    if candidate.already_loaded {
        0
    } else {
        1
    }
}

pub fn fits(candidate: TapeFitState, projected_footprint: u64) -> bool {
    match candidate.usable_bytes.checked_sub(candidate.used_bytes) {
        Some(remaining) => remaining >= projected_footprint,
        None => false,
    }
}

pub fn completes_tape(candidate: TapeFitState, projected_footprint: u64) -> bool {
    candidate.used_bytes.saturating_add(projected_footprint) >= candidate.low_bytes
}

pub fn leftover_after_write(candidate: TapeFitState, projected_footprint: u64) -> u64 {
    candidate
        .usable_bytes
        .saturating_sub(candidate.used_bytes)
        .saturating_sub(projected_footprint)
}

pub fn complete_or_fill_completing_precedes_or_ties(
    left: TapeFitState,
    right: TapeFitState,
    projected_footprint: u64,
) -> bool {
    let left_leftover = leftover_after_write(left, projected_footprint);
    let right_leftover = leftover_after_write(right, projected_footprint);
    if left_leftover < right_leftover {
        return true;
    }
    if right_leftover < left_leftover {
        return false;
    }

    let left_loaded_key = loaded_key(left);
    let right_loaded_key = loaded_key(right);
    if left_loaded_key < right_loaded_key {
        return true;
    }
    if right_loaded_key < left_loaded_key {
        return false;
    }

    if left.barcode_order < right.barcode_order {
        return true;
    }
    if right.barcode_order < left.barcode_order {
        return false;
    }

    left.tape_uuid <= right.tape_uuid
}

pub fn complete_or_fill_fill_precedes_or_ties(left: TapeFitState, right: TapeFitState) -> bool {
    let left_loaded_key = loaded_key(left);
    let right_loaded_key = loaded_key(right);
    if left_loaded_key < right_loaded_key {
        return true;
    }
    if right_loaded_key < left_loaded_key {
        return false;
    }

    if left.barcode_order < right.barcode_order {
        return true;
    }
    if right.barcode_order < left.barcode_order {
        return false;
    }

    left.tape_uuid <= right.tape_uuid
}

pub fn fill_oldest_precedes_or_ties(left: TapeFitState, right: TapeFitState) -> bool {
    if left.barcode_order < right.barcode_order {
        return true;
    }
    if right.barcode_order < left.barcode_order {
        return false;
    }

    let left_loaded_key = loaded_key(left);
    let right_loaded_key = loaded_key(right);
    if left_loaded_key < right_loaded_key {
        return true;
    }
    if right_loaded_key < left_loaded_key {
        return false;
    }

    left.tape_uuid <= right.tape_uuid
}

// Compile the production policy source directly into the test crate. Its only
// crate-local dependency is the tape UUID alias; using the production `[u8; 16]`
// shape also keeps the production module's own tests type-correct. This makes
// admission drift a behavioral test failure instead of a substring mismatch.
#[cfg(test)]
mod pool_write {
    pub type TapeUuid = [u8; 16];
}

#[cfg(test)]
#[path = "../../../crates/remanence-api/src/pool_selection.rs"]
mod production_pool_selection;

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 50;

    fn tape(
        tape_uuid: TapeUuid,
        barcode_order: u64,
        already_loaded: bool,
        used_bytes: u64,
        usable_bytes: u64,
        low_bytes: u64,
    ) -> TapeFitState {
        TapeFitState {
            tape_uuid,
            barcode_order,
            already_loaded,
            used_bytes,
            usable_bytes,
            low_bytes,
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-api/src/pool_selection.rs"
        ))
        .expect("original pool_selection.rs must be readable from verif/pool-selection");

        let snippets: &[&str] = &[
            ".filter(|candidate| fits(candidate, ctx.projected_footprint))",
            ".filter(|candidate| completes_tape(candidate, ctx.projected_footprint))",
            "leftover_after_write(candidate, ctx.projected_footprint),\n                    !candidate.already_loaded,\n                    candidate.barcode_order,\n                    candidate.tape_uuid,",
            "!candidate.already_loaded,\n                    candidate.barcode_order,\n                    candidate.tape_uuid,",
            "candidate.barcode_order,\n                    !candidate.already_loaded,\n                    candidate.tape_uuid,",
            "candidate\n        .usable_bytes\n        .checked_sub(candidate.used_bytes)\n        .is_some_and(|remaining| remaining >= projected_footprint)",
            "candidate.used_bytes.saturating_add(projected_footprint) >= candidate.low_bytes",
            "candidate\n        .usable_bytes\n        .saturating_sub(candidate.used_bytes)\n        .saturating_sub(projected_footprint)",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-api pool_selection.rs -- original \
                 changed; re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "pub fn fits(candidate: TapeFitState, projected_footprint: u64) -> bool",
            "pub fn completes_tape(candidate: TapeFitState, projected_footprint: u64) -> bool",
            "pub fn leftover_after_write(candidate: TapeFitState, projected_footprint: u64) -> u64",
            "pub fn complete_or_fill_completing_precedes_or_ties(",
            "pub fn complete_or_fill_fill_precedes_or_ties(",
            "pub fn fill_oldest_precedes_or_ties(left: TapeFitState, right: TapeFitState) -> bool",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif pool-selection model"
            );
        }
    }

    fn production_admission(input: CapacityAdmissionInput) -> AdmissionDisposition {
        use production_pool_selection as production;

        match production::capacity_admission_disposition(production::CapacityAdmissionInput {
            current_used_blocks: input.current_used_blocks,
            object_commit_charge_blocks: input.object_commit_charge_blocks,
            close_bound_blocks: input.close_bound_blocks,
            capacity_blocks: input.capacity_blocks,
            low_watermark_blocks: input.low_watermark_blocks,
            high_watermark_blocks: input.high_watermark_blocks,
        }) {
            production::AdmissionDisposition::AdmitRemainOpen => {
                AdmissionDisposition::AdmitRemainOpen
            }
            production::AdmissionDisposition::AdmitThenFinalize => {
                AdmissionDisposition::AdmitThenFinalize
            }
            production::AdmissionDisposition::FinalizePrefixAndRetry => {
                AdmissionDisposition::FinalizePrefixAndRetry
            }
            production::AdmissionDisposition::RejectInvalidCapacityPolicy => {
                AdmissionDisposition::RejectInvalidCapacityPolicy
            }
        }
    }

    #[test]
    fn drift_guard_admission_semantics_match_production_at_branch_boundaries() {
        let values = [0, 1, 2, 9, 10, u64::MAX - 1, u64::MAX];
        for current_used_blocks in values {
            for object_commit_charge_blocks in values {
                for close_bound_blocks in values {
                    for capacity_blocks in values {
                        for low_watermark_blocks in values {
                            for high_watermark_blocks in values {
                                let input = CapacityAdmissionInput {
                                    current_used_blocks,
                                    object_commit_charge_blocks,
                                    close_bound_blocks,
                                    capacity_blocks,
                                    low_watermark_blocks,
                                    high_watermark_blocks,
                                };
                                assert_eq!(
                                    capacity_admission_disposition(input),
                                    production_admission(input),
                                    "admission drift for input {input:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fits_matches_remaining_capacity() {
        assert!(fits(tape(1, 1, false, 10, 200, 150), P));
        assert!(!fits(tape(1, 1, false, 180, 200, 190), P));
        assert!(!fits(tape(1, 1, false, 201, 200, 190), P));
    }

    fn admission(projected_used_blocks: u64, close_bound_blocks: u64) -> AdmissionDisposition {
        capacity_admission_disposition(CapacityAdmissionInput {
            current_used_blocks: 10,
            object_commit_charge_blocks: projected_used_blocks - 10,
            close_bound_blocks,
            capacity_blocks: 140,
            low_watermark_blocks: 100,
            high_watermark_blocks: 120,
        })
    }

    #[test]
    fn admission_pins_low_high_close_and_retry_boundaries() {
        assert_eq!(admission(99, 20), AdmissionDisposition::AdmitRemainOpen);
        assert_eq!(admission(100, 20), AdmissionDisposition::AdmitThenFinalize);
        assert_eq!(admission(120, 20), AdmissionDisposition::AdmitThenFinalize);
        assert_eq!(
            admission(121, 0),
            AdmissionDisposition::FinalizePrefixAndRetry
        );
        assert_eq!(
            admission(99, 42),
            AdmissionDisposition::FinalizePrefixAndRetry
        );
    }

    #[test]
    fn admission_overflow_retries_and_invalid_policy_rejects() {
        assert_eq!(
            capacity_admission_disposition(CapacityAdmissionInput {
                current_used_blocks: u64::MAX,
                object_commit_charge_blocks: 1,
                close_bound_blocks: 0,
                capacity_blocks: u64::MAX,
                low_watermark_blocks: 0,
                high_watermark_blocks: u64::MAX,
            }),
            AdmissionDisposition::FinalizePrefixAndRetry
        );
        assert_eq!(
            capacity_admission_disposition(CapacityAdmissionInput {
                current_used_blocks: 0,
                object_commit_charge_blocks: 0,
                close_bound_blocks: 0,
                capacity_blocks: 100,
                low_watermark_blocks: 91,
                high_watermark_blocks: 90,
            }),
            AdmissionDisposition::RejectInvalidCapacityPolicy
        );
    }

    #[test]
    fn completing_and_leftover_match_policy_arithmetic() {
        let not_complete = tape(1, 1, false, 10, 200, 150);
        let complete = tape(2, 2, false, 110, 200, 150);

        assert!(!completes_tape(not_complete, P));
        assert!(completes_tape(complete, P));
        assert_eq!(leftover_after_write(complete, P), 40);
    }

    #[test]
    fn complete_or_fill_completing_rank_minimizes_leftover_first() {
        let left = tape(1, 1, false, 130, 190, 150);
        let right = tape(2, 2, false, 130, 240, 150);

        assert!(complete_or_fill_completing_precedes_or_ties(left, right, P));
        assert!(!complete_or_fill_completing_precedes_or_ties(
            right, left, P
        ));
    }

    #[test]
    fn complete_or_fill_completing_rank_uses_loaded_then_barcode_then_uuid() {
        let unloaded = tape(1, 1, false, 130, 200, 150);
        let loaded = tape(2, 2, true, 130, 200, 150);
        let lower_barcode = tape(3, 1, false, 130, 200, 150);
        let higher_barcode = tape(4, 2, false, 130, 200, 150);
        let lower_uuid = tape(5, 1, false, 130, 200, 150);
        let higher_uuid = tape(6, 1, false, 130, 200, 150);

        assert!(complete_or_fill_completing_precedes_or_ties(
            loaded, unloaded, P
        ));
        assert!(complete_or_fill_completing_precedes_or_ties(
            lower_barcode,
            higher_barcode,
            P
        ));
        assert!(complete_or_fill_completing_precedes_or_ties(
            lower_uuid,
            higher_uuid,
            P
        ));
    }

    #[test]
    fn fill_ranks_match_policy_tie_breaks() {
        let loaded = tape(3, 30, true, 10, 200, 150);
        let lower_barcode = tape(1, 10, false, 10, 200, 150);
        let higher_barcode = tape(2, 20, false, 10, 200, 150);

        assert!(complete_or_fill_fill_precedes_or_ties(
            loaded,
            lower_barcode
        ));
        assert!(fill_oldest_precedes_or_ties(lower_barcode, loaded));
        assert!(fill_oldest_precedes_or_ties(lower_barcode, higher_barcode));
    }
}
