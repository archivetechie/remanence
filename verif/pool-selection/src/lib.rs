//! Verification extraction of the pool-selection ranking kernel.
//!
//! This crate is a standalone, dependency-free model of the arithmetic and
//! lexicographic key logic in `crates/remanence-api/src/pool_selection.rs`.
//! The production policy uses slices, `Vec`, iterator adapters, trait objects,
//! and tuple `min_by_key`; this proof-facing crate extracts the stable kernel:
//! fit filtering, completion detection, leftover calculation, and the pairwise
//! ranking predicates for `CompleteOrFill` and `FillOldest`. UUIDs are modeled
//! as ordered `u64`s because the production key only needs deterministic final
//! ordering. The `drift_guard` test pins the production snippets this mirrors.

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

    #[test]
    fn fits_matches_remaining_capacity() {
        assert!(fits(tape(1, 1, false, 10, 200, 150), P));
        assert!(!fits(tape(1, 1, false, 180, 200, 190), P));
        assert!(!fits(tape(1, 1, false, 201, 200, 190), P));
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
