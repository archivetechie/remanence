//! The positioning cost model — design-read-ordering.md §7.1.
//!
//! Physical decomposition with fixed priors:
//!
//! ```text
//! cost(a -> b) = t_reposition(delta wrap, band change)
//!              + t_longitudinal(|lpos_b - lpos_a_end|)
//!              + t_reversal(direction mismatch)
//!              + t_fixed
//! ```
//!
//! The decomposition is kept physical rather than fitted so that when
//! the model is wrong it shows *which term* is wrong. v1 measures
//! nothing and fits nothing: every estimate derives from the priors.
//!
//! All stored and compared values are integers or exact rationals.
//! Longitudinal positions are exact [`Ratio`]s; costs are integer
//! nanoseconds ([`ElapsedNs`]), derived from the exact fractions with a
//! documented floor at nanosecond resolution — the same resolution the
//! wire contract carries (`estimated_locate_ns`). Overflow beyond any
//! physically plausible input saturates deterministically instead of
//! panicking.

use crate::rational::{mul_div_floor_u128, Ratio};
use crate::wrap_map::TapeDirection;

/// A duration in integer nanoseconds. The unit is in the name; the wire
/// contract's `estimated_locate_ns` and `estimated_total_ns` carry the
/// same unit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ElapsedNs(pub u64);

impl ElapsedNs {
    /// Zero nanoseconds.
    pub const ZERO: ElapsedNs = ElapsedNs(0);
    /// The saturation ceiling.
    pub const MAX: ElapsedNs = ElapsedNs(u64::MAX);

    /// Saturating addition; the cost model never wraps.
    pub fn saturating_add(self, other: ElapsedNs) -> ElapsedNs {
        ElapsedNs(self.0.saturating_add(other.0))
    }

    /// The raw nanosecond count.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

/// The fixed cost priors, all in integer nanoseconds.
///
/// `full_traverse_ns`, `mean_reposition_ns` and `wrap_turnaround_ns` are
/// published manufacturer figures (HP LTO-6 Technical Reference Manual:
/// maximum access 99 s, mean reposition 2.50 s, end-of-wrap turnaround
/// 1.5 s). `wrap_step_ns`, `band_step_ns` and `fixed_overhead_ns` are
/// not published; they are the values the design's reference simulation
/// (`geom-error-sim-v2.py`) adopts for its primary "published" regime
/// and sweeps for sensitivity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CostPriors {
    /// Full end-to-end tape traverse — scales the longitudinal term.
    pub full_traverse_ns: u64,
    /// Mean stop/reposition — charged on a direction mismatch.
    pub mean_reposition_ns: u64,
    /// End-of-wrap turnaround — charged on a wrap change.
    pub wrap_turnaround_ns: u64,
    /// Per-wrap-change step cost.
    pub wrap_step_ns: u64,
    /// Vertical head step per unit of band-rank distance.
    pub band_step_ns: u64,
    /// Fixed per-hop overhead.
    pub fixed_overhead_ns: u64,
}

/// The v1 priors — design §7.1 and the reference simulation's
/// "published" coefficient regime.
pub const PUBLISHED_PRIORS: CostPriors = CostPriors {
    full_traverse_ns: 99_000_000_000,
    mean_reposition_ns: 2_500_000_000,
    wrap_turnaround_ns: 1_500_000_000,
    wrap_step_ns: 300_000_000,
    band_step_ns: 800_000_000,
    fixed_overhead_ns: 500_000_000,
};

/// A block's full physical position: wrap, direction, exact longitudinal
/// position, and the physical rank of its band under the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalPosition {
    /// The wrap holding the block.
    pub wrap_index: u32,
    /// Head travel direction on that wrap.
    pub direction: TapeDirection,
    /// Exact longitudinal position; only this reaches the cost model,
    /// never the raw fraction.
    pub physical_lpos: Ratio,
    /// Physical rank of the block's logical band under `BAND_LAYOUT`.
    pub band_rank: u32,
}

/// The longitudinal term: `full_traverse_ns * |lpos_b - lpos_a|`,
/// floored to integer nanoseconds. Saturates on arithmetic overflow
/// beyond physically plausible spans.
pub fn longitudinal_ns(priors: &CostPriors, from_lpos: Ratio, to_lpos: Ratio) -> ElapsedNs {
    let Some(delta) = from_lpos.checked_abs_diff(to_lpos) else {
        return ElapsedNs::MAX;
    };
    debug_assert!(delta.num() >= 0);
    match mul_div_floor_u128(
        u128::from(priors.full_traverse_ns),
        delta.num() as u128,
        delta.den() as u128,
    ) {
        Some(ns) => ElapsedNs(u64::try_from(ns).unwrap_or(u64::MAX)),
        None => ElapsedNs::MAX,
    }
}

/// The wrap/band reposition term: zero within a wrap; on a wrap change,
/// one wrap step plus the band step scaled by band-rank distance plus
/// the published end-of-wrap turnaround.
pub fn wrap_reposition_ns(
    priors: &CostPriors,
    from_wrap: u32,
    to_wrap: u32,
    from_rank: u32,
    to_rank: u32,
) -> ElapsedNs {
    if from_wrap == to_wrap {
        return ElapsedNs::ZERO;
    }
    let rank_delta = u64::from(from_rank.abs_diff(to_rank));
    ElapsedNs(priors.wrap_step_ns)
        .saturating_add(ElapsedNs(priors.band_step_ns.saturating_mul(rank_delta)))
        .saturating_add(ElapsedNs(priors.wrap_turnaround_ns))
}

/// The reversal term: the published mean reposition on a direction
/// mismatch, zero otherwise.
pub fn reversal_ns(priors: &CostPriors, from: TapeDirection, to: TapeDirection) -> ElapsedNs {
    if from == to {
        ElapsedNs::ZERO
    } else {
        ElapsedNs(priors.mean_reposition_ns)
    }
}

/// Full hop cost from one physical position to another — the §7.1 sum.
pub fn hop_ns(priors: &CostPriors, from: &PhysicalPosition, to: &PhysicalPosition) -> ElapsedNs {
    ElapsedNs(priors.fixed_overhead_ns)
        .saturating_add(longitudinal_ns(
            priors,
            from.physical_lpos,
            to.physical_lpos,
        ))
        .saturating_add(wrap_reposition_ns(
            priors,
            from.wrap_index,
            to.wrap_index,
            from.band_rank,
            to.band_rank,
        ))
        .saturating_add(reversal_ns(priors, from.direction, to.direction))
}
