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
