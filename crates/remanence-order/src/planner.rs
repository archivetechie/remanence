//! The planner: targets in, an order out — design-read-ordering.md §8.
//!
//! The solver is nearest-neighbour construction followed by 2-opt and
//! Or-opt improvement (§8.5). It is deterministic: no randomness
//! anywhere, ties broken by lowest index, so the same input always
//! produces the same output.
//!
//! Two objectives (§8.4): `MIN_TOTAL_TIME` (the default) minimises the
//! whole route; `MIN_TIME_TO_FIRST` reaches the cheapest-to-reach target
//! first, then optimises the remainder. An optional `end_block` is a
//! physical position visited after all targets; its terminal hop is a
//! cost under *both* objectives and is always part of the reported
//! total.
//!
//! This crate is the pure planning core. Request validation that needs
//! `written_extent_lba`, cartridge-fact resolution, statuses, the
//! `MAX_TARGETS` degraded fallback and every other wire concern live in
//! the RPC layer; here a target is a block range, coverage is judged
//! against the map's `mapped_extent_lba` only, and every failure is a
//! typed error.

use crate::cost::{hop_ns, CostPriors, ElapsedNs, PhysicalPosition};
use crate::geometry::{band_rank, StructuralRow};
use crate::wrap_map::WrapMap;
use std::fmt;

/// The contract ceiling on targets per plan, matching the drive's own
/// UDS limit (IBM LTO SCSI Reference GA32-0928-09 §4.6.3.2). The pure
/// core does not enforce it — the wire layer answers a larger batch with
/// its degraded fallback — but the constant lives with the solver so
/// there is exactly one definition.
pub const MAX_TARGETS: u32 = 2730;

/// One read target: an inclusive block range on partition zero.
///
/// The end block is required and load-bearing: the cost of reading B
/// after A depends on where A *ends* (§1.1). The caller-visible opaque
/// tag stays at the wire layer; the pure core identifies targets by
/// their index in the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadTarget {
    /// First block of the target.
    pub start_block: u64,
    /// Last block of the target, inclusive. Must be `>= start_block`.
    pub end_block: u64,
}

/// The planning objective — design §8.4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Objective {
    /// One efficient sweep of the cartridge. The default.
    #[default]
    MinTotalTime,
    /// Reach the first target quickly, then optimise the remainder.
    MinTimeToFirst,
}

/// Everything the pure planning core needs for one plan.
#[derive(Clone, Copy, Debug)]
pub struct PlanInput<'a> {
    /// Structural geometry of the volume's cartridge (bands and wraps
    /// per band feed the band-step cost term).
    pub geometry: &'a StructuralRow,
    /// The volume's wrap map.
    pub map: &'a WrapMap,
    /// The cost priors.
    pub priors: &'a CostPriors,
    /// The read targets, in caller order.
    pub targets: &'a [ReadTarget],
    /// The objective.
    pub objective: Objective,
    /// Where the head starts. `None` means the load point (block zero of
    /// wrap zero) — valid only if the caller actually rewinds (§8.3).
    pub start_block: Option<u64>,
    /// Optional physical position visited after all targets. `None`
    /// means no terminal hop.
    pub end_block: Option<u64>,
}

/// One planned hop: which target to read next and the estimated
/// positioning time to reach it from the previous target's end (from the
/// start position for the first hop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlannedHop {
    /// Index of the target in the request's `targets`.
    pub target_index: usize,
    /// Estimated positioning time for this hop, integer nanoseconds.
    pub estimated_locate_ns: ElapsedNs,
}

/// The plan: an ordering of every input target with per-hop estimates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The targets in recommended read order — always a permutation of
    /// the input.
    pub hops: Vec<PlannedHop>,
    /// The terminal hop to `end_block`, present exactly when the request
    /// carried one. Counted under every objective.
    pub terminal_ns: Option<ElapsedNs>,
    /// Sum of every hop estimate plus the terminal hop when present.
    /// Transfer time is never included.
    pub estimated_total_ns: ElapsedNs,
    /// True when any planned target's position — or a supplied start or
    /// end position that a hop was costed against — used the estimated
    /// EOD-wrap denominator, or when the completed spans are highly
    /// dispersed (§6.4). Means "some estimate was used", not "something
    /// went wrong".
    pub uses_estimated_eod_geometry: bool,
}

/// Why a block could not be given a full physical position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionError {
    /// The block is at or beyond the map's exclusive `mapped_extent_lba`.
    OutOfCoverage {
        /// The uncovered block.
        block_lba: u64,
        /// The map's exclusive coverage bound.
        mapped_extent_lba: u64,
    },
    /// The geometry row cannot band-classify this wrap.
    GeometryInvalid {
        /// What was wrong.
        detail: &'static str,
    },
}

/// Why a plan could not be produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanError {
    /// The geometry row is unusable (zero bands or wraps per band, or a
    /// band outside the four-band layout).
    GeometryInvalid {
        /// What was wrong.
        detail: &'static str,
    },
    /// The map reports more wraps than the geometry row allows.
    MapExceedsGeometry {
        /// Wraps in the map.
        map_wraps: u32,
        /// Wraps the geometry row allows.
        geometry_wraps: u32,
    },
    /// A target's `end_block` precedes its `start_block`.
    TargetEndBeforeStart {
        /// Index of the offending target.
        index: usize,
    },
    /// A target block is outside map coverage.
    TargetOutOfCoverage {
        /// Index of the offending target.
        index: usize,
        /// The uncovered block.
        block_lba: u64,
        /// The map's exclusive coverage bound.
        mapped_extent_lba: u64,
    },
    /// The supplied start position is outside map coverage.
    StartOutOfCoverage {
        /// The uncovered block.
        block_lba: u64,
        /// The map's exclusive coverage bound.
        mapped_extent_lba: u64,
    },
    /// The supplied end position is outside map coverage.
    EndOutOfCoverage {
        /// The uncovered block.
        block_lba: u64,
        /// The map's exclusive coverage bound.
        mapped_extent_lba: u64,
    },
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::GeometryInvalid { detail } => write!(f, "unusable geometry row: {detail}"),
            PlanError::MapExceedsGeometry {
                map_wraps,
                geometry_wraps,
            } => write!(
                f,
                "wrap map reports {map_wraps} wraps but the geometry row allows {geometry_wraps}"
            ),
            PlanError::TargetEndBeforeStart { index } => {
                write!(f, "target {index}: end_block precedes start_block")
            }
            PlanError::TargetOutOfCoverage {
                index,
                block_lba,
                mapped_extent_lba,
            } => write!(
                f,
                "target {index}: block {block_lba} is at or beyond mapped extent {mapped_extent_lba}"
            ),
            PlanError::StartOutOfCoverage {
                block_lba,
                mapped_extent_lba,
            } => write!(
                f,
                "start position {block_lba} is at or beyond mapped extent {mapped_extent_lba}"
            ),
            PlanError::EndOutOfCoverage {
                block_lba,
                mapped_extent_lba,
            } => write!(
                f,
                "end position {block_lba} is at or beyond mapped extent {mapped_extent_lba}"
            ),
        }
    }
}

impl std::error::Error for PlanError {}

/// Compose a map lookup with structural geometry into a full physical
/// position: wrap, direction, exact longitudinal position, band rank.
///
/// The logical band follows from the wrap index by division; the
/// physical rank is a separate lookup into the layout. These are
/// different facts (§6.4) and are kept as two steps.
pub fn physical_position(
    map: &WrapMap,
    geometry: &StructuralRow,
    block_lba: u64,
) -> Result<(PhysicalPosition, bool), PositionError> {
    let pos = map
        .locate(block_lba)
        .map_err(|e| PositionError::OutOfCoverage {
            block_lba: e.block_lba,
            mapped_extent_lba: e.mapped_extent_lba,
        })?;
    if geometry.wraps_per_band == 0 {
        return Err(PositionError::GeometryInvalid {
            detail: "wraps_per_band is zero",
        });
    }
    let logical_band = pos.wrap_index / geometry.wraps_per_band;
    if logical_band >= geometry.bands {
        return Err(PositionError::GeometryInvalid {
            detail: "wrap maps to a band beyond the row's band count",
        });
    }
    let rank = band_rank(logical_band).ok_or(PositionError::GeometryInvalid {
        detail: "logical band outside the four-band layout",
    })?;
    Ok((
        PhysicalPosition {
            wrap_index: pos.wrap_index,
            direction: pos.direction,
            physical_lpos: pos.physical_lpos,
            band_rank: rank,
        },
        pos.uses_eod_denominator,
    ))
}

/// Produce a plan. The returned order is always a permutation of
/// `input.targets`; estimates always describe the order actually
/// returned.
pub fn plan(input: &PlanInput<'_>) -> Result<Plan, PlanError> {
    let geometry = input.geometry;
    let map = input.map;
    if geometry.bands == 0 || geometry.wraps_per_band == 0 {
        return Err(PlanError::GeometryInvalid {
            detail: "zero bands or wraps_per_band",
        });
    }
    if map.wrap_count() > geometry.wraps {
        return Err(PlanError::MapExceedsGeometry {
            map_wraps: map.wrap_count(),
            geometry_wraps: geometry.wraps,
        });
    }

    for (index, t) in input.targets.iter().enumerate() {
        if t.end_block < t.start_block {
            return Err(PlanError::TargetEndBeforeStart { index });
        }
    }

    // Positions. An absent start means the load point: block zero, which
    // every valid map covers. A position in the EOD wrap depends on the
    // estimated denominator exactly as a target does (§6.4), so the
    // uses-EOD flags are kept, not discarded — but the start position's
    // flag only counts when some hop is actually costed from it.
    let (start_pos, start_uses_eod) =
        match physical_position(map, geometry, input.start_block.unwrap_or(0)) {
            Ok((p, uses_eod)) => (p, uses_eod),
            Err(PositionError::OutOfCoverage {
                block_lba,
                mapped_extent_lba,
            }) => {
                return Err(PlanError::StartOutOfCoverage {
                    block_lba,
                    mapped_extent_lba,
                })
            }
            Err(PositionError::GeometryInvalid { detail }) => {
                return Err(PlanError::GeometryInvalid { detail })
            }
        };
    let mut end_uses_eod = false;
    let end_pos = match input.end_block {
        None => None,
        Some(block) => match physical_position(map, geometry, block) {
            Ok((p, uses_eod)) => {
                end_uses_eod = uses_eod;
                Some(p)
            }
            Err(PositionError::OutOfCoverage {
                block_lba,
                mapped_extent_lba,
            }) => {
                return Err(PlanError::EndOutOfCoverage {
                    block_lba,
                    mapped_extent_lba,
                })
            }
            Err(PositionError::GeometryInvalid { detail }) => {
                return Err(PlanError::GeometryInvalid { detail })
            }
        },
    };
    // The start position feeds an estimate whenever any hop leaves it —
    // the first target's hop, or the degenerate zero-target terminal hop.
    let start_feeds_an_estimate = !input.targets.is_empty() || end_pos.is_some();

    let mut starts = Vec::with_capacity(input.targets.len());
    let mut ends = Vec::with_capacity(input.targets.len());
    let mut any_target_uses_eod = false;
    for (index, t) in input.targets.iter().enumerate() {
        for (block_lba, out) in [(t.start_block, &mut starts), (t.end_block, &mut ends)] {
            match physical_position(map, geometry, block_lba) {
                Ok((p, uses_eod)) => {
                    any_target_uses_eod |= uses_eod;
                    out.push(p);
                }
                Err(PositionError::OutOfCoverage {
                    block_lba,
                    mapped_extent_lba,
                }) => {
                    return Err(PlanError::TargetOutOfCoverage {
                        index,
                        block_lba,
                        mapped_extent_lba,
                    })
                }
                Err(PositionError::GeometryInvalid { detail }) => {
                    return Err(PlanError::GeometryInvalid { detail })
                }
            }
        }
    }

    let n = input.targets.len();
    let priors = input.priors;

    // Cost matrices. m0[j]: start -> start of j. m[i][j]: end of i ->
    // start of j. term[i]: end of i -> end position.
    let m0: Vec<u64> = (0..n)
        .map(|j| hop_ns(priors, &start_pos, &starts[j]).0)
        .collect();
    let m: Vec<Vec<u64>> = (0..n)
        .map(|i| {
            (0..n)
                .map(|j| hop_ns(priors, &ends[i], &starts[j]).0)
                .collect()
        })
        .collect();
    let term: Option<Vec<u64>> = end_pos
        .as_ref()
        .map(|e| (0..n).map(|i| hop_ns(priors, &ends[i], e).0).collect());

    let order: Vec<usize> = match n {
        0 => Vec::new(),
        1 => vec![0],
        _ => solve(&m0, &m, term.as_deref(), n, input.objective),
    };

    // Assemble hops with estimates describing the returned order.
    let mut hops = Vec::with_capacity(n);
    let mut total: u128 = 0;
    for (k, &idx) in order.iter().enumerate() {
        let ns = if k == 0 {
            m0[idx]
        } else {
            m[order[k - 1]][idx]
        };
        total += u128::from(ns);
        hops.push(PlannedHop {
            target_index: idx,
            estimated_locate_ns: ElapsedNs(ns),
        });
    }
    let terminal_ns = match (&end_pos, order.last()) {
        (Some(_), Some(&last)) => {
            let t = term.as_ref().expect("term exists when end_pos does")[last];
            total += u128::from(t);
            Some(ElapsedNs(t))
        }
        // No targets at all: the terminal hop runs from the start
        // position, so the reported total is still the cost of honouring
        // end_block.
        (Some(e), None) => {
            let t = hop_ns(priors, &start_pos, e).0;
            total += u128::from(t);
            Some(ElapsedNs(t))
        }
        (None, _) => None,
    };

    Ok(Plan {
        hops,
        terminal_ns,
        estimated_total_ns: ElapsedNs(u64::try_from(total).unwrap_or(u64::MAX)),
        uses_estimated_eod_geometry: any_target_uses_eod
            || (start_feeds_an_estimate && start_uses_eod)
            || end_uses_eod
            || map.completed_spans_highly_dispersed(),
    })
}

/// Total cost of a route under the matrices: start hop, inter-target
/// hops, and the terminal hop when present. `u128` accumulator so
/// saturated entries cannot wrap the sum.
fn route_cost(order: &[usize], m0: &[u64], m: &[Vec<u64>], term: Option<&[u64]>) -> u128 {
    let mut total = u128::from(m0[order[0]]);
    for w in order.windows(2) {
        total += u128::from(m[w[0]][w[1]]);
    }
    if let Some(t) = term {
        total += u128::from(t[*order.last().expect("route is non-empty")]);
    }
    total
}

/// Deterministic nearest-neighbour construction. Ties break to the
/// lowest index because iteration is ascending and improvement is
/// strict.
fn nearest_neighbour(
    m0: &[u64],
    m: &[Vec<u64>],
    n: usize,
    pinned_first: Option<usize>,
) -> Vec<usize> {
    let mut visited = vec![false; n];
    let mut order = Vec::with_capacity(n);
    let mut current: Option<usize> = None;
    if let Some(f) = pinned_first {
        visited[f] = true;
        order.push(f);
        current = Some(f);
    }
    while order.len() < n {
        let mut best: Option<(usize, u64)> = None;
        for j in 0..n {
            if visited[j] {
                continue;
            }
            let c = match current {
                None => m0[j],
                Some(i) => m[i][j],
            };
            if best.is_none_or(|(_, bc)| c < bc) {
                best = Some((j, c));
            }
        }
        let (j, _) = best.expect("an unvisited target always exists here");
        visited[j] = true;
        order.push(j);
        current = Some(j);
    }
    order
}

/// 2-opt and Or-opt improvement, first-improvement order, strict
/// integer decrease only — deterministic. When `pinned` the first
/// element never moves (the `MIN_TIME_TO_FIRST` remainder optimisation).
fn improve(order: &mut Vec<usize>, m0: &[u64], m: &[Vec<u64>], term: Option<&[u64]>, pinned: bool) {
    let n = order.len();
    let lo = usize::from(pinned);
    let mut best = route_cost(order, m0, m, term);
    let mut improved = true;
    while improved {
        improved = false;
        // 2-opt: reverse order[i..=j].
        for i in lo..n.saturating_sub(1) {
            for j in (i + 1)..n {
                let mut cand = order.clone();
                cand[i..=j].reverse();
                let cc = route_cost(&cand, m0, m, term);
                if cc < best {
                    *order = cand;
                    best = cc;
                    improved = true;
                }
            }
        }
        // Or-opt: relocate a chunk of 1..=3 consecutive targets.
        for seg in 1..=3usize {
            if n < seg + lo {
                continue;
            }
            for i in lo..=(n - seg) {
                let chunk: Vec<usize> = order[i..i + seg].to_vec();
                let mut rest: Vec<usize> = Vec::with_capacity(n - seg);
                rest.extend_from_slice(&order[..i]);
                rest.extend_from_slice(&order[i + seg..]);
                for j in lo..=rest.len() {
                    if j == i {
                        continue; // reinsertion at the same place
                    }
                    let mut cand = Vec::with_capacity(n);
                    cand.extend_from_slice(&rest[..j]);
                    cand.extend_from_slice(&chunk);
                    cand.extend_from_slice(&rest[j..]);
                    let cc = route_cost(&cand, m0, m, term);
                    if cc < best {
                        *order = cand;
                        best = cc;
                        improved = true;
                    }
                }
            }
        }
    }
}

/// The §8.5 solver over prebuilt matrices.
fn solve(
    m0: &[u64],
    m: &[Vec<u64>],
    term: Option<&[u64]>,
    n: usize,
    objective: Objective,
) -> Vec<usize> {
    match objective {
        Objective::MinTotalTime => {
            let mut order = nearest_neighbour(m0, m, n, None);
            improve(&mut order, m0, m, term, false);
            order
        }
        Objective::MinTimeToFirst => {
            // Reach the first target quickly: the cheapest first hop,
            // lowest index on a tie. Then optimise the remainder —
            // terminal hop included — with the first pinned.
            let first = (0..n).min_by_key(|&j| (m0[j], j)).expect("n >= 2 in solve");
            let mut order = nearest_neighbour(m0, m, n, Some(first));
            improve(&mut order, m0, m, term, true);
            order
        }
    }
}
