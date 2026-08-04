//! Acceptance tests for the §7.1 cost model: per-component
//! monotonicity within a fixed wrap and direction, the priors, and the
//! deliberate absence of global monotonicity over block distance.

mod common;

use common::{synthetic_geometry, uniform_map};
use remanence_order::{
    hop_ns, longitudinal_ns, physical_position, reversal_ns, wrap_reposition_ns, ElapsedNs, Ratio,
    TapeDirection, PUBLISHED_PRIORS,
};

/// The priors are the published figures plus the reference simulation's
/// unpublished step coefficients, all integer nanoseconds.
#[test]
fn priors_carry_the_published_figures() {
    let p = PUBLISHED_PRIORS;
    assert_eq!(p.full_traverse_ns, 99_000_000_000); // 99 s maximum access
    assert_eq!(p.mean_reposition_ns, 2_500_000_000); // 2.50 s mean reposition
    assert_eq!(p.wrap_turnaround_ns, 1_500_000_000); // 1.5 s end-of-wrap turnaround
    assert_eq!(p.wrap_step_ns, 300_000_000);
    assert_eq!(p.band_step_ns, 800_000_000);
    assert_eq!(p.fixed_overhead_ns, 500_000_000);
}

/// Longitudinal component: monotone in physical distance, exact at
/// divisor-friendly fractions, zero at zero distance.
#[test]
fn longitudinal_component_is_monotone_in_distance() {
    let p = &PUBLISHED_PRIORS;
    let at = |num: i128, den: i128| Ratio::new(num, den).unwrap();
    assert_eq!(longitudinal_ns(p, at(0, 1), at(0, 1)), ElapsedNs::ZERO);
    // Exact: 99e9 * 1/1000 = 99e6, and symmetric.
    assert_eq!(longitudinal_ns(p, at(0, 1), at(1, 1000)).0, 99_000_000);
    assert_eq!(longitudinal_ns(p, at(1, 1000), at(0, 1)).0, 99_000_000);
    // Nondecreasing over growing distances, strictly here since each
    // step is far above nanosecond resolution.
    let mut prev = ElapsedNs::ZERO;
    for k in 1..=100i128 {
        let c = longitudinal_ns(p, at(0, 1), at(k, 100));
        assert!(
            c > prev,
            "longitudinal cost must grow with distance (k={k})"
        );
        prev = c;
    }
    // Full traverse costs the full published figure.
    assert_eq!(longitudinal_ns(p, at(0, 1), at(1, 1)).0, 99_000_000_000);
}

/// Wrap/band reposition component: zero within a wrap; on a wrap change,
/// monotone in band-rank distance.
#[test]
fn reposition_component_is_monotone_in_band_rank_distance() {
    let p = &PUBLISHED_PRIORS;
    assert_eq!(wrap_reposition_ns(p, 5, 5, 0, 3), ElapsedNs::ZERO);
    let mut prev = ElapsedNs::ZERO;
    for delta in 0u32..=3 {
        let c = wrap_reposition_ns(p, 0, 1, 0, delta);
        // delta 0 still pays the wrap step and turnaround.
        assert_eq!(
            c.0,
            p.wrap_step_ns + p.wrap_turnaround_ns + u64::from(delta) * p.band_step_ns
        );
        assert!(c > prev || delta == 0);
        prev = c;
    }
}

/// Reversal component: zero on matched direction, the published mean
/// reposition on a mismatch, symmetric.
#[test]
fn reversal_component_charges_only_direction_mismatch() {
    let p = &PUBLISHED_PRIORS;
    use TapeDirection::{Forward, Reverse};
    assert_eq!(reversal_ns(p, Forward, Forward), ElapsedNs::ZERO);
    assert_eq!(reversal_ns(p, Reverse, Reverse), ElapsedNs::ZERO);
    assert_eq!(reversal_ns(p, Forward, Reverse).0, 2_500_000_000);
    assert_eq!(reversal_ns(p, Reverse, Forward).0, 2_500_000_000);
}

/// Within a fixed wrap and direction the full hop cost is monotone in
/// block distance (fixed overhead plus a growing longitudinal term).
#[test]
fn hop_cost_is_monotone_within_a_wrap() {
    let map = uniform_map(6, 1000, 500);
    let geom = synthetic_geometry(2);
    let p = &PUBLISHED_PRIORS;
    let pos = |b: u64| physical_position(&map, &geom, b).unwrap().0;
    let origin = pos(100);
    let mut prev = None;
    for b in [101u64, 150, 300, 600, 999] {
        let c = hop_ns(p, &origin, &pos(b));
        if let Some(prev) = prev {
            assert!(c > prev, "hop cost must grow along a wrap (block {b})");
        }
        prev = Some(c);
    }
}

/// Global monotonicity over logical block distance is FALSE on
/// serpentine media, and correct code must exhibit that: a logically
/// distant block just across the wrap turn is physically adjacent and
/// cheaper to reach than a logically nearer block at the other end of
/// the same wrap. Asserting global monotonicity would fail on correct
/// code; this test pins the counterexample instead.
#[test]
fn block_distance_is_not_globally_monotone() {
    let span = 1000u64;
    let map = uniform_map(6, span, 500);
    let geom = synthetic_geometry(2);
    let p = &PUBLISHED_PRIORS;
    let pos = |b: u64| physical_position(&map, &geom, b).unwrap().0;

    let a = 10u64; // wrap 0, near load point
    let same_wrap_far = 990u64; // wrap 0, far end; block distance 980
    let next_wrap_near = 1990u64; // wrap 1, offset 990 -> physically at 10/1000; block distance 1980

    let cost_same_wrap = hop_ns(p, &pos(a), &pos(same_wrap_far));
    let cost_across_turn = hop_ns(p, &pos(a), &pos(next_wrap_near));
    assert!(next_wrap_near - a > same_wrap_far - a);
    assert!(
        cost_across_turn < cost_same_wrap,
        "larger block distance must be able to cost less on serpentine media \
         (across-turn {} ns vs same-wrap {} ns)",
        cost_across_turn.0,
        cost_same_wrap.0
    );
}

/// The full §7.1 sum: fixed + longitudinal + wrap/band + reversal,
/// checked against a hand-computed hop.
#[test]
fn hop_cost_is_the_component_sum() {
    let map = uniform_map(4, 1000, 500);
    // One wrap per band so a wrap change is also a band change.
    let geom = synthetic_geometry(1);
    let p = &PUBLISHED_PRIORS;
    let from = physical_position(&map, &geom, 100).unwrap().0; // wrap 0, band 0, rank 2
    let to = physical_position(&map, &geom, 1100).unwrap().0; // wrap 1, band 1, rank 1
    assert_eq!(from.band_rank, 2);
    assert_eq!(to.band_rank, 1);
    // from lpos = 100/1000; to: reverse wrap, frac 100/1000 -> lpos 900/1000.
    // longitudinal: 99e9 * 800/1000 = 79.2e9; wrap change: 0.3e9 + 1 *
    // 0.8e9 + 1.5e9; reversal: 2.5e9; fixed 0.5e9.
    let expected =
        79_200_000_000 + 300_000_000 + 800_000_000 + 1_500_000_000 + 2_500_000_000 + 500_000_000;
    assert_eq!(hop_ns(p, &from, &to).0, expected);
}
