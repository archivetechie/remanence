//! remanence-order — the pure batch read-ordering planner for serpentine
//! tape.
//!
//! Design of record: `design-read-ordering.md` §§6.3–6.4, 7 and 8
//! (private journal). The crate holds the whole of the planning logic
//! and none of the integration:
//!
//! - [`geometry`] — the structural cartridge table (§6.3): bands, wraps
//!   per band, channels, data tracks, with unsupported formats
//!   distinguishable from absent ones.
//! - [`wrap_map`] — the §6.4 mapping from harvested REOWP descriptors to
//!   wrap, direction and exact longitudinal position.
//! - [`cost`] — the §7.1 physical cost decomposition on fixed priors.
//! - [`planner`] — the §8.5 deterministic solver (nearest neighbour,
//!   then 2-opt and Or-opt) under the §8.4 objectives.
//!
//! **Purity is a hard boundary.** No I/O, no async, no SCSI, no
//! database, no randomness, and no dependency on any other remanence
//! crate. Every stored and compared value is an integer or an exact
//! rational; longitudinal positions are exact [`rational::Ratio`]s and
//! durations are integer nanoseconds with the unit in the type name.
//! REOWP acquisition, the map cache and its lifecycle, and the
//! `PlanBatchRead` RPC live elsewhere (prompts P2, P4, P5).

#![forbid(unsafe_code)]

pub mod cost;
pub mod geometry;
pub mod planner;
pub mod rational;
pub mod wrap_map;

pub use cost::{
    hop_ns, longitudinal_ns, reversal_ns, wrap_reposition_ns, CostPriors, ElapsedNs,
    PhysicalPosition, PUBLISHED_PRIORS,
};
pub use geometry::{
    band_rank, lookup_geometry, GeometryLookup, StructuralRow, UnsupportedRow, BAND_LAYOUT,
    STRUCTURAL_TABLE, UNSUPPORTED_TABLE,
};
pub use planner::{
    physical_position, plan, Objective, Plan, PlanError, PlanInput, PlannedHop, PositionError,
    ReadTarget, MAX_TARGETS,
};
pub use rational::Ratio;
pub use wrap_map::{
    lower_median, BlockPosition, CoverageError, EodDenominator, EodDenominatorBasis,
    ReowpDescriptor, TapeDirection, WrapMap, WrapMapError,
};
