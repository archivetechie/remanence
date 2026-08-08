//! Pure within-pool tape-selection policies.
//!
//! The policy layer only sees caller-projected fit state. It deliberately has
//! no catalog, session, drive, or hardware access; Tier-0 stickiness and eager
//! sealing live in the session/write path.
#![allow(dead_code)]

use std::sync::Arc;

use crate::pool_write::TapeUuid;

/// Per-tape fit state projected for a selection decision.
///
/// Deliberately decoupled from the catalog `TapeRecord`: the caller (in the
/// write engine) projects each candidate into this value before consulting a
/// policy, so the policy never touches the catalog, hardware, or a session.
/// `already_loaded` is one such projected fact (is this tape currently mounted
/// in a free drive?) — computed by the caller from drive occupancy, not by the
/// policy. `usable_bytes` already bakes in `watermark_high`; `low_bytes` bakes
/// in `watermark_low`. Both are per-tape so a pool may hold mixed capacities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeFitState {
    /// Physical tape identity.
    pub tape_uuid: TapeUuid,
    /// Fill order within the pool (barcode/id sequence).
    pub barcode_order: u64,
    /// Projected fact: already mounted in a free drive (mount-avoidance tie-break).
    pub already_loaded: bool,
    /// Bytes already committed on the tape.
    pub used_bytes: u64,
    /// `capacity * watermark_high` — the usable ceiling.
    pub usable_bytes: u64,
    /// `capacity * watermark_low` — the fill target / seal threshold.
    pub low_bytes: u64,
}

/// Inputs a selection policy may see at a rollover decision. A pure value: no
/// hardware handle, no session, no catalog. `candidates` is pre-filtered by the
/// caller to tapes that fit the object and are not reserved by another live
/// session (design §4, §7).
#[derive(Clone, Debug)]
pub struct PoolSelectionContext<'a> {
    /// Active, fitting, unreserved candidate tapes.
    pub candidates: &'a [TapeFitState],
    /// Projected footprint `P` of the object being placed (incl. sidecars).
    pub projected_footprint: u64,
}

/// A policy's choice at rollover (design §4 Tiers 1–3). Note there is no
/// `seal_after`: the seal decision is taken later, from the tape's actual
/// post-write position, not from the policy (design §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Place the object on this existing active tape.
    UseTape {
        /// Selected tape.
        tape_uuid: TapeUuid,
    },
    /// No active tape fits; the session machinery must promote a blank or fail.
    NeedFreshTape,
}

/// Proof-facing result of applying the existing low/high closing state machine
/// to one candidate-specific physical projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionDisposition {
    /// The Object and its close reserve fit below the low watermark.
    AdmitRemainOpen,
    /// The Object fits at or above low and no later Object may be admitted.
    AdmitThenFinalize,
    /// Write none of this Object; finalize the committed prefix and retry the
    /// whole unchanged Object on another tape.
    FinalizePrefixAndRetry,
    /// The integer low/high/capacity policy itself is inconsistent.
    RejectInvalidCapacityPolicy,
}

/// Candidate-specific physical-block inputs to the low/high admission kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapacityAdmissionInput {
    /// Physical charge of the already committed prefix (`U`).
    pub current_used_blocks: u64,
    /// Complete proposed Object commit charge (`U' - U`).
    pub object_commit_charge_blocks: u64,
    /// Exact final-close reserve after the projected Object.
    pub close_bound_blocks: u64,
    /// Conservative physical capacity basis (`C`).
    pub capacity_blocks: u64,
    /// Existing fill target / eager-seal boundary (`L`).
    pub low_watermark_blocks: u64,
    /// Existing maximum ordinary committed boundary (`H`).
    pub high_watermark_blocks: u64,
}

/// Decide admission before any block of the proposed Object is written.
///
/// The close proof deliberately precedes the low-watermark branch. Checked
/// overflow is a conservative retry, while an invalid `L <= H <= C` policy is
/// a terminal configuration rejection rather than a tape-rollover loop.
pub fn capacity_admission_disposition(input: CapacityAdmissionInput) -> AdmissionDisposition {
    if input.low_watermark_blocks > input.high_watermark_blocks
        || input.high_watermark_blocks > input.capacity_blocks
    {
        return AdmissionDisposition::RejectInvalidCapacityPolicy;
    }
    let Some(projected_used_blocks) = input
        .current_used_blocks
        .checked_add(input.object_commit_charge_blocks)
    else {
        return AdmissionDisposition::FinalizePrefixAndRetry;
    };
    if projected_used_blocks > input.high_watermark_blocks {
        return AdmissionDisposition::FinalizePrefixAndRetry;
    }
    let Some(required_through_close) = projected_used_blocks.checked_add(input.close_bound_blocks)
    else {
        return AdmissionDisposition::FinalizePrefixAndRetry;
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

/// A pluggable within-pool selection policy (design §10).
///
/// Object-safe and `Send + Sync` so the daemon can hold one as
/// `Arc<dyn PoolSelectionPolicy>` shared across async request handlers.
pub trait PoolSelectionPolicy: Send + Sync {
    /// Choose the next tape for one object at a rollover. Pure function of the
    /// context; must not block, touch hardware, or mutate state.
    fn select(&self, ctx: &PoolSelectionContext<'_>) -> Selection;

    /// Stable policy name (matches the config `selection_policy` value).
    fn name(&self) -> &'static str;
}

/// Default policy: the two-tier "complete-or-fill" rule (design §4).
#[derive(Clone, Copy, Debug, Default)]
pub struct CompleteOrFill;

impl PoolSelectionPolicy for CompleteOrFill {
    fn select(&self, ctx: &PoolSelectionContext<'_>) -> Selection {
        let candidates = fitting_candidates(ctx);
        let completing = candidates
            .iter()
            .copied()
            .filter(|candidate| completes_tape(candidate, ctx.projected_footprint))
            .min_by_key(|candidate| {
                (
                    leftover_after_write(candidate, ctx.projected_footprint),
                    !candidate.already_loaded,
                    candidate.barcode_order,
                    candidate.tape_uuid,
                )
            });
        if let Some(candidate) = completing {
            return Selection::UseTape {
                tape_uuid: candidate.tape_uuid,
            };
        }

        candidates
            .iter()
            .copied()
            .min_by_key(|candidate| {
                (
                    !candidate.already_loaded,
                    candidate.barcode_order,
                    candidate.tape_uuid,
                )
            })
            .map(|candidate| Selection::UseTape {
                tape_uuid: candidate.tape_uuid,
            })
            .unwrap_or(Selection::NeedFreshTape)
    }

    fn name(&self) -> &'static str {
        "complete-or-fill"
    }
}

/// Pure d2 first-fit-by-barcode (Tier 2 only) — the trivial fallback policy.
#[derive(Clone, Copy, Debug, Default)]
pub struct FillOldest;

impl PoolSelectionPolicy for FillOldest {
    fn select(&self, ctx: &PoolSelectionContext<'_>) -> Selection {
        fitting_candidates(ctx)
            .iter()
            .copied()
            .min_by_key(|candidate| {
                (
                    candidate.barcode_order,
                    !candidate.already_loaded,
                    candidate.tape_uuid,
                )
            })
            .map(|candidate| Selection::UseTape {
                tape_uuid: candidate.tape_uuid,
            })
            .unwrap_or(Selection::NeedFreshTape)
    }

    fn name(&self) -> &'static str {
        "fill-oldest"
    }
}

/// Resolve a configured `selection_policy` name to a shared policy object.
/// Demonstrates the trait-object storage path the daemon will use.
pub fn resolve_policy(name: &str) -> Option<Arc<dyn PoolSelectionPolicy>> {
    let policy: Arc<dyn PoolSelectionPolicy> = match name {
        "complete-or-fill" => Arc::new(CompleteOrFill),
        "fill-oldest" => Arc::new(FillOldest),
        _ => return None,
    };
    Some(policy)
}

// Compile-time checks of the two Rust-specific constraints this design relies
// on (rust-design-verification categories 2 and 5): the trait object is
// object-safe AND `Send + Sync`, so it is storable in the daemon's shared
// state and movable across async/thread boundaries.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<dyn PoolSelectionPolicy>>();
    assert_send_sync::<TapeFitState>();
    assert_send_sync::<Selection>();
};

fn fitting_candidates<'a>(ctx: &'a PoolSelectionContext<'a>) -> Vec<&'a TapeFitState> {
    ctx.candidates
        .iter()
        .filter(|candidate| fits(candidate, ctx.projected_footprint))
        .collect()
}

fn fits(candidate: &TapeFitState, projected_footprint: u64) -> bool {
    candidate
        .usable_bytes
        .checked_sub(candidate.used_bytes)
        .is_some_and(|remaining| remaining >= projected_footprint)
}

fn completes_tape(candidate: &TapeFitState, projected_footprint: u64) -> bool {
    candidate.used_bytes.saturating_add(projected_footprint) >= candidate.low_bytes
}

fn leftover_after_write(candidate: &TapeFitState, projected_footprint: u64) -> u64 {
    candidate
        .usable_bytes
        .saturating_sub(candidate.used_bytes)
        .saturating_sub(projected_footprint)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: u64 = 50;

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
    fn capacity_admission_pins_low_and_high_equalities() {
        assert_eq!(admission(99, 20), AdmissionDisposition::AdmitRemainOpen);
        assert_eq!(admission(100, 20), AdmissionDisposition::AdmitThenFinalize);
        assert_eq!(admission(120, 20), AdmissionDisposition::AdmitThenFinalize);
        assert_eq!(
            admission(121, 0),
            AdmissionDisposition::FinalizePrefixAndRetry
        );
    }

    #[test]
    fn capacity_admission_checks_close_before_remaining_open() {
        assert_eq!(
            admission(99, 42),
            AdmissionDisposition::FinalizePrefixAndRetry
        );
    }

    #[test]
    fn capacity_admission_fails_closed_on_overflow() {
        let overflow = CapacityAdmissionInput {
            current_used_blocks: u64::MAX,
            object_commit_charge_blocks: 1,
            close_bound_blocks: 0,
            capacity_blocks: u64::MAX,
            low_watermark_blocks: 0,
            high_watermark_blocks: u64::MAX,
        };
        assert_eq!(
            capacity_admission_disposition(overflow),
            AdmissionDisposition::FinalizePrefixAndRetry
        );

        let invalid = CapacityAdmissionInput {
            current_used_blocks: 0,
            object_commit_charge_blocks: 0,
            close_bound_blocks: 0,
            capacity_blocks: 100,
            low_watermark_blocks: 91,
            high_watermark_blocks: 90,
        };
        assert_eq!(
            capacity_admission_disposition(invalid),
            AdmissionDisposition::RejectInvalidCapacityPolicy
        );
    }

    fn tape(
        tape_uuid_byte: u8,
        barcode_order: u64,
        already_loaded: bool,
        used_bytes: u64,
        usable_bytes: u64,
        low_bytes: u64,
    ) -> TapeFitState {
        TapeFitState {
            tape_uuid: [tape_uuid_byte; 16],
            barcode_order,
            already_loaded,
            used_bytes,
            usable_bytes,
            low_bytes,
        }
    }

    fn ctx(candidates: &[TapeFitState], projected_footprint: u64) -> PoolSelectionContext<'_> {
        PoolSelectionContext {
            candidates,
            projected_footprint,
        }
    }

    fn selected(selection: Selection) -> TapeUuid {
        match selection {
            Selection::UseTape { tape_uuid } => tape_uuid,
            Selection::NeedFreshTape => panic!("expected tape selection"),
        }
    }

    #[test]
    fn complete_or_fill_empty_candidates_needs_fresh_tape() {
        assert_eq!(
            CompleteOrFill.select(&ctx(&[], P)),
            Selection::NeedFreshTape
        );
    }

    #[test]
    fn complete_or_fill_tier_one_beats_tier_two() {
        let candidates = [
            tape(1, 1, false, 10, 200, 150),
            tape(2, 2, false, 110, 200, 150),
        ];

        assert_eq!(
            selected(CompleteOrFill.select(&ctx(&candidates, P))),
            [2; 16]
        );
    }

    #[test]
    fn complete_or_fill_tier_one_best_fit_minimizes_leftover() {
        let candidates = [
            tape(1, 1, false, 130, 240, 150),
            tape(2, 2, false, 130, 190, 150),
            tape(3, 3, false, 130, 260, 150),
        ];

        assert_eq!(
            selected(CompleteOrFill.select(&ctx(&candidates, P))),
            [2; 16]
        );
    }

    #[test]
    fn complete_or_fill_already_loaded_tie_break_wins() {
        let candidates = [
            tape(1, 1, false, 130, 200, 150),
            tape(2, 2, true, 130, 200, 150),
        ];

        assert_eq!(
            selected(CompleteOrFill.select(&ctx(&candidates, P))),
            [2; 16]
        );
    }

    #[test]
    fn complete_or_fill_lowest_barcode_final_tie_break_is_deterministic() {
        let candidates = [
            tape(3, 30, false, 130, 200, 150),
            tape(1, 10, false, 130, 200, 150),
            tape(2, 20, false, 130, 200, 150),
        ];

        assert_eq!(
            selected(CompleteOrFill.select(&ctx(&candidates, P))),
            [1; 16]
        );
    }

    #[test]
    fn complete_or_fill_used_plus_projected_equal_low_counts_as_complete() {
        let candidates = [
            tape(1, 1, false, 10, 200, 100),
            tape(2, 2, false, 50, 200, 100),
        ];

        assert_eq!(
            selected(CompleteOrFill.select(&ctx(&candidates, P))),
            [2; 16]
        );
    }

    #[test]
    fn complete_or_fill_skips_non_fitting_candidates_defensively() {
        let candidates = [
            tape(1, 1, false, 180, 200, 190),
            tape(2, 2, false, 10, 200, 150),
        ];

        assert_eq!(
            selected(CompleteOrFill.select(&ctx(&candidates, P))),
            [2; 16]
        );
    }

    #[test]
    fn fill_oldest_first_fitting_barcode_wins() {
        let candidates = [
            tape(3, 30, true, 10, 200, 150),
            tape(1, 10, false, 10, 200, 150),
            tape(2, 20, false, 190, 200, 150),
        ];

        assert_eq!(selected(FillOldest.select(&ctx(&candidates, P))), [1; 16]);
    }

    #[test]
    fn fill_oldest_empty_candidates_needs_fresh_tape() {
        assert_eq!(FillOldest.select(&ctx(&[], P)), Selection::NeedFreshTape);
    }

    #[test]
    fn resolve_policy_accepts_v1_policy_names() {
        assert_eq!(
            resolve_policy("complete-or-fill").expect("policy").name(),
            "complete-or-fill"
        );
        assert_eq!(
            resolve_policy("fill-oldest").expect("policy").name(),
            "fill-oldest"
        );
        assert!(resolve_policy("most-free").is_none());
    }
}
