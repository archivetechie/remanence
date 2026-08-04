//! Acceptance tests for the §8.5 solver: optimality against exhaustive
//! enumeration, permutation, determinism, start and end handling, the
//! objectives-differ counterexample, degenerate geometries, and
//! EOD-denominator scale invariance.

mod common;

use common::{
    exhaustive_optimal, matrices, min_first_hop_set, random_targets, synthetic_geometry,
    uniform_map, jitter_map, SplitMix64,
};
use remanence_order::{
    hop_ns, lookup_geometry, physical_position, plan, GeometryLookup, Objective, Plan, PlanError,
    PlanInput, ReadTarget, ReowpDescriptor, StructuralRow, WrapMap, PUBLISHED_PRIORS,
};

/// The named optimality bound from the design and prompt: the solver
/// must be within **1.10** of the exhaustive optimum for batches up to
/// ten targets. Expressed as an exact integer ratio, asserted, never an
/// adjective.
const OPTIMALITY_BOUND_NUM: u128 = 110;
const OPTIMALITY_BOUND_DEN: u128 = 100;

fn run_plan(
    geometry: &StructuralRow,
    map: &WrapMap,
    targets: &[ReadTarget],
    objective: Objective,
    start_block: Option<u64>,
    end_block: Option<u64>,
) -> Plan {
    plan(&PlanInput {
        geometry,
        map,
        priors: &PUBLISHED_PRIORS,
        targets,
        objective,
        start_block,
        end_block,
    })
    .expect("fixture plans succeed")
}

fn order_of(plan: &Plan) -> Vec<usize> {
    plan.hops.iter().map(|h| h.target_index).collect()
}

fn assert_permutation(order: &[usize], n: usize) {
    let mut seen = order.to_vec();
    seen.sort_unstable();
    let expected: Vec<usize> = (0..n).collect();
    assert_eq!(seen, expected, "output must be a permutation of the input");
}

/// Recompute a plan's route cost through the public cost API and check
/// the reported per-hop and total estimates describe the returned order.
fn assert_estimates_describe_order(
    plan: &Plan,
    geometry: &StructuralRow,
    map: &WrapMap,
    targets: &[ReadTarget],
    start_block: Option<u64>,
    end_block: Option<u64>,
) -> u128 {
    let pos = |b: u64| physical_position(map, geometry, b).unwrap().0;
    let mut current = pos(start_block.unwrap_or(0));
    let mut total: u128 = 0;
    for hop in &plan.hops {
        let t = &targets[hop.target_index];
        let expected = hop_ns(&PUBLISHED_PRIORS, &current, &pos(t.start_block));
        assert_eq!(
            hop.estimated_locate_ns, expected,
            "hop estimate must describe the returned order"
        );
        total += u128::from(expected.0);
        current = pos(t.end_block);
    }
    match (end_block, plan.terminal_ns) {
        (Some(e), Some(term)) => {
            let expected = hop_ns(&PUBLISHED_PRIORS, &current, &pos(e));
            assert_eq!(term, expected, "terminal hop must be costed from the last end");
            total += u128::from(expected.0);
        }
        (None, None) => {}
        (e, t) => panic!("terminal presence mismatch: end_block {e:?}, terminal_ns {t:?}"),
    }
    assert_eq!(
        u128::from(plan.estimated_total_ns.0),
        total,
        "total must be the hop sum plus the terminal hop"
    );
    total
}

// ---------------------------------------------------------------------
// Optimality, permutation, determinism — the generated-batch harness.
// ---------------------------------------------------------------------

/// For batches of 2..=10 targets across uniform and jittered maps, both
/// objectives, with and without an end position: the solver's cost is
/// within 1.10 of the exhaustive optimum, the output is a permutation,
/// and repeated runs are identical.
#[test]
fn solver_within_bound_of_exhaustive_optimum_up_to_ten_targets() {
    let geom = lookup_geometry("LTO-9", "L9");
    let GeometryLookup::Supported(geom) = geom else { panic!("LTO-9 supported") };
    for n in 2..=10usize {
        let seeds = if n >= 9 { 3 } else { 6 };
        for seed in 0..seeds {
            let mut rng = SplitMix64(0xC0FF_EE00 + (n as u64) * 1000 + seed);
            let map = if seed % 2 == 0 {
                uniform_map(280, 1000, 500)
            } else {
                jitter_map(120, 900, 400, 0xFEED + seed)
            };
            let extent = map.mapped_extent_lba();
            let targets = random_targets(&mut rng, n, extent);
            let start = if seed % 3 == 0 { None } else { Some(rng.below(extent)) };
            let end = if seed % 2 == 1 { Some(rng.below(extent)) } else { None };

            for objective in [Objective::MinTotalTime, Objective::MinTimeToFirst] {
                let p = run_plan(geom, &map, &targets, objective, start, end);
                let order = order_of(&p);
                assert_permutation(&order, n);
                let got = assert_estimates_describe_order(&p, geom, &map, &targets, start, end);

                // Determinism: same input, same output, repeatedly.
                for _ in 0..3 {
                    let again = run_plan(geom, &map, &targets, objective, start, end);
                    assert_eq!(again, p, "solver must be deterministic (n={n} seed={seed})");
                }

                // Exhaustive reference.
                let (m0, m, term) = matrices(&map, geom, &PUBLISHED_PRIORS, &targets, start, end);
                let firsts: Vec<usize> = match objective {
                    Objective::MinTotalTime => (0..n).collect(),
                    Objective::MinTimeToFirst => min_first_hop_set(&m0),
                };
                let optimal = exhaustive_optimal(&m0, &m, term.as_deref(), &firsts);
                assert!(got >= optimal, "a valid route cannot beat the optimum");
                assert!(
                    got * OPTIMALITY_BOUND_DEN <= optimal * OPTIMALITY_BOUND_NUM,
                    "solver {got} ns exceeds {OPTIMALITY_BOUND_NUM}/{OPTIMALITY_BOUND_DEN} \
                     of optimal {optimal} ns (n={n} seed={seed} objective={objective:?})"
                );

                // MIN_TIME_TO_FIRST reaches a cheapest-first-hop target
                // first, by definition.
                if objective == Objective::MinTimeToFirst {
                    assert!(
                        firsts.contains(&order[0]),
                        "first hop must minimise time to first"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Start and end.
// ---------------------------------------------------------------------

/// The start position is respected: the first hop is costed from it,
/// and moving the start moves the chosen first target.
#[test]
fn start_position_is_respected() {
    let map = uniform_map(6, 1000, 500);
    let geom = &synthetic_geometry(2);
    let targets = [
        ReadTarget { start_block: 100, end_block: 120 },  // wrap 0
        ReadTarget { start_block: 4500, end_block: 4520 }, // wrap 4
    ];
    // From the load point the wrap-0 target is first.
    let from_load = run_plan(geom, &map, &targets, Objective::MinTimeToFirst, None, None);
    assert_eq!(order_of(&from_load)[0], 0);
    // From mid-tape next to the wrap-4 target, that target is first.
    let from_mid = run_plan(geom, &map, &targets, Objective::MinTimeToFirst, Some(4400), None);
    assert_eq!(order_of(&from_mid)[0], 1);
    // The first hop is costed from the supplied start, exactly.
    let pos = |b: u64| physical_position(&map, geom, b).unwrap().0;
    let expected = hop_ns(&PUBLISHED_PRIORS, &pos(4400), &pos(4500));
    assert_eq!(from_mid.hops[0].estimated_locate_ns, expected);
}

/// `end_position`'s terminal hop is included under both objectives: the
/// reported total is the hop sum plus the terminal hop, and the terminal
/// leg participates in optimisation rather than being bolted on.
#[test]
fn end_position_terminal_hop_counts_under_both_objectives() {
    let map = uniform_map(6, 1000, 500);
    let geom = &synthetic_geometry(2);
    let targets = [
        ReadTarget { start_block: 10, end_block: 20 },
        ReadTarget { start_block: 2200, end_block: 2260 },
        ReadTarget { start_block: 3800, end_block: 3810 },
    ];
    for objective in [Objective::MinTotalTime, Objective::MinTimeToFirst] {
        let without = run_plan(geom, &map, &targets, objective, None, None);
        assert_eq!(without.terminal_ns, None);
        assert_estimates_describe_order(&without, geom, &map, &targets, None, None);

        let with = run_plan(geom, &map, &targets, objective, None, Some(30));
        let term = with.terminal_ns.expect("terminal hop present when end_block is");
        assert!(term.0 > 0);
        assert_estimates_describe_order(&with, geom, &map, &targets, None, Some(30));
        assert!(
            with.estimated_total_ns.0
                == with
                    .hops
                    .iter()
                    .map(|h| h.estimated_locate_ns.0)
                    .sum::<u64>()
                    + term.0,
            "total must include the terminal hop under {objective:?}"
        );
    }

    // The terminal hop participates in optimisation rather than being
    // bolted on: an end position near one order's final resting place
    // flips MIN_TOTAL_TIME's choice. Two wraps of 8000; target A spans
    // the wrap turn and ends travelling reverse near the load point,
    // target B is short on wrap 0. Without an end position B-then-A wins
    // (no extra direction crossing); with the end position beside B's
    // end, A-then-B wins because finishing on B avoids a full
    // wrap-and-reversal terminal leg.
    let turn_map = WrapMap::from_descriptors(&[
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 7999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 15999 },
    ])
    .unwrap();
    let a = ReadTarget { start_block: 100, end_block: 15900 }; // ends on reverse wrap 1
    let b = ReadTarget { start_block: 200, end_block: 210 }; // short, wrap 0
    let two = [a, b];
    let without = run_plan(geom, &turn_map, &two, Objective::MinTotalTime, None, None);
    let with_end = run_plan(geom, &turn_map, &two, Objective::MinTotalTime, None, Some(208));
    assert_eq!(order_of(&without), vec![1, 0], "without an end position B-then-A wins");
    assert_eq!(
        order_of(&with_end),
        vec![0, 1],
        "an end position beside B's end must flip the order under MIN_TOTAL_TIME"
    );
}

/// Zero and one targets: returned unchanged; with an end position and no
/// targets the terminal hop runs from the start.
#[test]
fn zero_and_one_target_plans() {
    let map = uniform_map(4, 1000, 500);
    let geom = &synthetic_geometry(1);
    let one = [ReadTarget { start_block: 700, end_block: 720 }];
    let p = run_plan(geom, &map, &one, Objective::MinTotalTime, None, None);
    assert_eq!(order_of(&p), vec![0]);
    assert_estimates_describe_order(&p, geom, &map, &one, None, None);

    let none: [ReadTarget; 0] = [];
    let p0 = run_plan(geom, &map, &none, Objective::MinTotalTime, None, None);
    assert!(p0.hops.is_empty());
    assert_eq!(p0.estimated_total_ns.0, 0);
    let p0e = run_plan(geom, &map, &none, Objective::MinTotalTime, Some(100), Some(900));
    let pos = |b: u64| physical_position(&map, geom, b).unwrap().0;
    let expected = hop_ns(&PUBLISHED_PRIORS, &pos(100), &pos(900));
    assert_eq!(p0e.terminal_ns, Some(expected));
    assert_eq!(p0e.estimated_total_ns, expected);
}

// ---------------------------------------------------------------------
// Objectives differ. The design once claimed all objectives agree at
// small batch sizes; they do not, and this counterexample keeps that
// fixed.
// ---------------------------------------------------------------------

/// §8.4's two-target counterexample on one forward wrap, starting at
/// block 0: `A = [1, 100]`, `B = [2, 2]`. `MIN_TIME_TO_FIRST` takes A
/// first because A begins nearer; `MIN_TOTAL_TIME` takes B then A
/// because A ends at 100 and the return leg costs more than reaching B
/// first. The orders must differ.
#[test]
fn objectives_differ_on_the_two_target_counterexample() {
    // One completed wrap of 10,000 blocks; both targets on forward wrap 0.
    let map = WrapMap::from_descriptors(&[
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 9999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 15000 },
    ])
    .unwrap();
    let geom = &synthetic_geometry(1);
    let a = ReadTarget { start_block: 1, end_block: 100 };
    let b = ReadTarget { start_block: 2, end_block: 2 };
    let targets = [a, b];

    let ttf = run_plan(geom, &map, &targets, Objective::MinTimeToFirst, Some(0), None);
    let total = run_plan(geom, &map, &targets, Objective::MinTotalTime, Some(0), None);

    assert_eq!(order_of(&ttf), vec![0, 1], "MIN_TIME_TO_FIRST takes A first");
    assert_eq!(order_of(&total), vec![1, 0], "MIN_TOTAL_TIME takes B then A");
    assert_ne!(
        order_of(&ttf),
        order_of(&total),
        "the objectives must return different orders on this input"
    );
}

// ---------------------------------------------------------------------
// EOD-wrap denominator behaviour through the planner.
// ---------------------------------------------------------------------

/// No completed wrap: any positive denominator is a monotonic scale
/// factor for every longitudinal distance, so the *order* is unchanged
/// across denominators; only the absolute estimates stretch. The
/// estimated-geometry flag is set.
#[test]
fn no_completed_wrap_order_is_invariant_under_denominator() {
    let geom = &synthetic_geometry(1);
    let targets = [
        ReadTarget { start_block: 100, end_block: 150 },
        ReadTarget { start_block: 2000, end_block: 2100 },
        ReadTarget { start_block: 700, end_block: 750 },
        ReadTarget { start_block: 4000, end_block: 4100 },
        ReadTarget { start_block: 1500, end_block: 1600 },
    ];
    // Two single-wrap maps: same written data, different observed spans,
    // hence different (but both positive) denominators.
    let small = WrapMap::from_descriptors(&[ReowpDescriptor {
        partition: 0,
        wrap_number: 0,
        end_loi: 5000,
    }])
    .unwrap();
    let large = WrapMap::from_descriptors(&[ReowpDescriptor {
        partition: 0,
        wrap_number: 0,
        end_loi: 9000,
    }])
    .unwrap();
    assert_eq!(small.eod_denominator().span_lba, 5000);
    assert_eq!(large.eod_denominator().span_lba, 9000);

    for objective in [Objective::MinTotalTime, Objective::MinTimeToFirst] {
        let p_small = run_plan(geom, &small, &targets, objective, None, None);
        let p_large = run_plan(geom, &large, &targets, objective, None, None);
        assert_eq!(
            order_of(&p_small),
            order_of(&p_large),
            "a positive denominator is a monotonic scale factor; the order must not change"
        );
        assert_ne!(
            p_small.estimated_total_ns, p_large.estimated_total_ns,
            "absolute estimates must stretch with the denominator"
        );
        assert!(p_small.uses_estimated_eod_geometry);
        assert!(p_large.uses_estimated_eod_geometry);
    }
}

/// The estimated-geometry flag: set when a target sits in the EOD wrap
/// (ordinary median case included), clear when none does, and set by
/// high dispersion even for targets on completed wraps.
#[test]
fn uses_estimated_eod_geometry_flag() {
    let geom = &synthetic_geometry(2);
    // Well-behaved map: completed spans tight, EOD wrap 3.
    let map = uniform_map(4, 1000, 500);
    let completed_only = [
        ReadTarget { start_block: 100, end_block: 150 },
        ReadTarget { start_block: 2200, end_block: 2300 },
    ];
    let p = run_plan(geom, &map, &completed_only, Objective::MinTotalTime, None, None);
    assert!(!p.uses_estimated_eod_geometry, "no EOD target, no dispersion: flag clear");

    let with_eod = [
        ReadTarget { start_block: 100, end_block: 150 },
        ReadTarget { start_block: 3100, end_block: 3150 }, // inside EOD wrap 3
    ];
    let p = run_plan(geom, &map, &with_eod, Objective::MinTotalTime, None, None);
    assert!(p.uses_estimated_eod_geometry, "an EOD-wrap target sets the flag");

    // Highly dispersed completed spans set the flag even when every
    // target is on a completed wrap.
    let dispersed = WrapMap::from_descriptors(&[
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 499 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 1499 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 3099 },
        ReowpDescriptor { partition: 0, wrap_number: 3, end_loi: 3600 },
    ])
    .unwrap();
    assert!(dispersed.completed_spans_highly_dispersed());
    let p = run_plan(geom, &dispersed, &completed_only, Objective::MinTotalTime, None, None);
    assert!(
        p.uses_estimated_eod_geometry,
        "high dispersion marks the volume's EOD estimate unreliable"
    );
}

// ---------------------------------------------------------------------
// Degenerate geometries.
// ---------------------------------------------------------------------

/// Single wrap: everything on one forward wrap still plans, is a
/// permutation, and matches the exhaustive optimum.
#[test]
fn degenerate_single_wrap() {
    let geom = &synthetic_geometry(1);
    let map = WrapMap::from_descriptors(&[ReowpDescriptor {
        partition: 0,
        wrap_number: 0,
        end_loi: 8000,
    }])
    .unwrap();
    let mut rng = SplitMix64(0x51_0135);
    let targets = random_targets(&mut rng, 6, map.mapped_extent_lba());
    let p = run_plan(geom, &map, &targets, Objective::MinTotalTime, None, None);
    assert_permutation(&order_of(&p), targets.len());
    let got = assert_estimates_describe_order(&p, geom, &map, &targets, None, None);
    let (m0, m, term) = matrices(&map, geom, &PUBLISHED_PRIORS, &targets, None, None);
    let firsts: Vec<usize> = (0..targets.len()).collect();
    let optimal = exhaustive_optimal(&m0, &m, term.as_deref(), &firsts);
    assert!(got * OPTIMALITY_BOUND_DEN <= optimal * OPTIMALITY_BOUND_NUM);
}

/// A partially written tape: median-denominator EOD wrap, targets both
/// sides of the EOD boundary, exact parity everywhere.
#[test]
fn degenerate_partially_written_tape() {
    let geom = &synthetic_geometry(2);
    // 5 wraps: 4 completed, EOD wrap 4 written 300 of ~1000.
    let map = uniform_map(5, 1000, 300);
    let targets = [
        ReadTarget { start_block: 50, end_block: 80 },
        ReadTarget { start_block: 4100, end_block: 4150 }, // EOD wrap
        ReadTarget { start_block: 2500, end_block: 2550 },
        ReadTarget { start_block: 3999, end_block: 3999 }, // last completed block
    ];
    let p = run_plan(geom, &map, &targets, Objective::MinTotalTime, None, None);
    assert_permutation(&order_of(&p), targets.len());
    assert!(p.uses_estimated_eod_geometry);
    assert_estimates_describe_order(&p, geom, &map, &targets, None, None);
    // The map itself: exact EOD wrap index, median basis.
    assert_eq!(map.eod_wrap(), 4);
    assert_eq!(
        map.eod_denominator().basis,
        remanence_order::EodDenominatorBasis::MedianCompletedWrap
    );
}

/// An LTO-7-like cartridge (112 wraps, 28 per band) and an LTO-9-like
/// one (280 wraps, 70 per band), planned end to end against exhaustive
/// enumeration.
#[test]
fn degenerate_lto7_like_and_lto9_like() {
    for (gen, fmt, wraps) in [("LTO-7", "L7", 112u32), ("LTO-9", "L9", 280u32)] {
        let GeometryLookup::Supported(geom) = lookup_geometry(gen, fmt) else {
            panic!("{gen} supported")
        };
        assert_eq!(geom.wraps, wraps);
        let map = uniform_map(wraps, 500, 250);
        let mut rng = SplitMix64(0x17E0 + u64::from(wraps));
        let targets = random_targets(&mut rng, 7, map.mapped_extent_lba());
        let p = run_plan(geom, &map, &targets, Objective::MinTotalTime, None, None);
        assert_permutation(&order_of(&p), targets.len());
        let got = assert_estimates_describe_order(&p, geom, &map, &targets, None, None);
        let (m0, m, term) = matrices(&map, geom, &PUBLISHED_PRIORS, &targets, None, None);
        let firsts: Vec<usize> = (0..targets.len()).collect();
        let optimal = exhaustive_optimal(&m0, &m, term.as_deref(), &firsts);
        assert!(got * OPTIMALITY_BOUND_DEN <= optimal * OPTIMALITY_BOUND_NUM, "{gen}");
    }
}

// ---------------------------------------------------------------------
// Planner failure paths.
// ---------------------------------------------------------------------

/// A target whose end precedes its start is rejected with its index.
#[test]
fn target_end_before_start_is_rejected() {
    let map = uniform_map(4, 1000, 500);
    let geom = &synthetic_geometry(1);
    let targets = [
        ReadTarget { start_block: 10, end_block: 20 },
        ReadTarget { start_block: 500, end_block: 400 },
    ];
    let err = plan(&PlanInput {
        geometry: geom,
        map: &map,
        priors: &PUBLISHED_PRIORS,
        targets: &targets,
        objective: Objective::MinTotalTime,
        start_block: None,
        end_block: None,
    })
    .unwrap_err();
    assert_eq!(err, PlanError::TargetEndBeforeStart { index: 1 });
}

/// Coverage failures name what was uncovered: the target by index, or
/// the supplied start or end position. The inclusive target end at the
/// exclusive extent is out of coverage.
#[test]
fn coverage_failures_name_the_offender() {
    let map = uniform_map(4, 1000, 500); // extent 3500
    let geom = &synthetic_geometry(1);
    let extent = map.mapped_extent_lba();
    let base = PlanInput {
        geometry: geom,
        map: &map,
        priors: &PUBLISHED_PRIORS,
        targets: &[],
        objective: Objective::MinTotalTime,
        start_block: None,
        end_block: None,
    };

    let targets = [ReadTarget { start_block: 3400, end_block: extent }];
    let err = plan(&PlanInput { targets: &targets, ..base }).unwrap_err();
    assert_eq!(
        err,
        PlanError::TargetOutOfCoverage { index: 0, block_lba: extent, mapped_extent_lba: extent }
    );

    let err = plan(&PlanInput { start_block: Some(extent), ..base }).unwrap_err();
    assert_eq!(
        err,
        PlanError::StartOutOfCoverage { block_lba: extent, mapped_extent_lba: extent }
    );

    let err = plan(&PlanInput { end_block: Some(extent + 7), ..base }).unwrap_err();
    assert_eq!(
        err,
        PlanError::EndOutOfCoverage { block_lba: extent + 7, mapped_extent_lba: extent }
    );
}

/// A map reporting more wraps than the geometry row allows is a
/// mismatch, not an index panic.
#[test]
fn map_exceeding_geometry_is_rejected() {
    let map = uniform_map(6, 1000, 500);
    let geom = &synthetic_geometry(1); // 4 wraps only
    let targets = [ReadTarget { start_block: 10, end_block: 20 }];
    let err = plan(&PlanInput {
        geometry: geom,
        map: &map,
        priors: &PUBLISHED_PRIORS,
        targets: &targets,
        objective: Objective::MinTotalTime,
        start_block: None,
        end_block: None,
    })
    .unwrap_err();
    assert_eq!(err, PlanError::MapExceedsGeometry { map_wraps: 6, geometry_wraps: 4 });
}

/// A geometry row with zero wraps-per-band cannot band-classify
/// anything and is rejected rather than dividing by zero.
#[test]
fn zero_wraps_per_band_is_rejected() {
    let map = uniform_map(2, 1000, 500);
    let mut geom = synthetic_geometry(1);
    geom.wraps_per_band = 0;
    geom.wraps = 4; // keep the wrap-count gate out of the way
    let targets = [ReadTarget { start_block: 10, end_block: 20 }];
    let err = plan(&PlanInput {
        geometry: &geom,
        map: &map,
        priors: &PUBLISHED_PRIORS,
        targets: &targets,
        objective: Objective::MinTotalTime,
        start_block: None,
        end_block: None,
    })
    .unwrap_err();
    assert!(matches!(err, PlanError::GeometryInvalid { .. }));
}
