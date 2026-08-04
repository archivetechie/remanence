//! The per-volume wrap map and the block-to-position mapping —
//! design-read-ordering.md §6.4, implemented exactly.
//!
//! The map is built from long-form REOWP descriptors kept exactly as
//! harvested: `(partition, wrap_number, end_loi)`. Descriptors with
//! `partition != 0` are ignored during harvest — v1 plans only partition
//! zero — and are not a validation failure. Partition-zero descriptors
//! must be contiguous from wrap zero through the EOD wrap, and wrap zero
//! begins at logical object identifier zero.
//!
//! For every completed wrap, `end_loi` is the *inclusive* last logical
//! object identifier on that wrap; its successor starts at
//! `end_loi + 1`, checked. The EOD-wrap descriptor is different: its
//! `end_loi` is the logical position of EOD, copied to a separate
//! *exclusive* `mapped_extent_lba` and never treated as the start of
//! another wrap. The EOD wrap's longitudinal denominator is a separately
//! labelled estimate (`EodDenominator`), never a boundary, and is not
//! materialised into the measured boundary list.

use crate::rational::Ratio;
use std::fmt;

/// One long-form REOWP descriptor, exactly as harvested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReowpDescriptor {
    /// Tape partition the descriptor reports on. Only partition zero is
    /// accepted in v1; other partitions are ignored, not rejected.
    pub partition: u32,
    /// Wrap the descriptor reports on.
    pub wrap_number: u32,
    /// Last logical object identifier on the wrap — inclusive for
    /// completed wraps, the logical position of EOD (exclusive) for the
    /// wrap holding EOD.
    pub end_loi: u64,
}

/// How the EOD wrap's longitudinal denominator was derived — design §6.4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EodDenominatorBasis {
    /// Lower median of the completed-wrap spans.
    MedianCompletedWrap,
    /// The EOD wrap's own observed written span — the no-completed-wrap
    /// case.
    EodObservedSpan,
}

/// The estimated denominator for longitudinal fractions inside the EOD
/// wrap. A derived record, not a boundary; it is never inserted into the
/// measured boundary list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EodDenominator {
    /// The denominator, in logical blocks. Always positive for a valid
    /// map.
    pub span_lba: u64,
    /// How the value was derived.
    pub basis: EodDenominatorBasis,
    /// How many completed-wrap spans the median was taken over; zero for
    /// [`EodDenominatorBasis::EodObservedSpan`].
    pub completed_span_sample_count: u32,
}

/// Travel direction of the head along a wrap.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TapeDirection {
    /// Even wraps: beginning of tape toward the far end.
    Forward,
    /// Odd wraps: far end back toward the beginning.
    Reverse,
}

/// Why a set of descriptors could not become a wrap map.
///
/// Harvest validation failures leave the volume uncalibrated (design
/// §6.5); this enum is the pure-core expression of "descriptor
/// parse/validation failure".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WrapMapError {
    /// No partition-zero descriptor at all.
    NoPartitionZeroDescriptors,
    /// The first partition-zero descriptor does not report wrap zero.
    FirstWrapNotZero {
        /// Wrap number actually found first.
        found: u32,
    },
    /// Partition-zero wrap numbers are not contiguous ascending.
    NonContiguousWrapNumbers {
        /// Wrap number expected at this posn.
        expected: u32,
        /// Wrap number actually found.
        found: u32,
    },
    /// `end_loi + 1` overflowed while deriving a successor wrap start.
    EndLoiOverflow {
        /// Wrap whose `end_loi` overflowed.
        wrap: u32,
    },
    /// A completed wrap's `end_loi` precedes its derived start.
    NonPositiveCompletedSpan {
        /// The offending wrap.
        wrap: u32,
    },
    /// `mapped_extent_lba` does not exceed the EOD wrap's start, so the
    /// EOD wrap has no positive written span.
    NonPositiveEodSpan {
        /// Derived start of the EOD wrap.
        eod_wrap_start: u64,
        /// The reported EOD position.
        mapped_extent_lba: u64,
    },
}

impl fmt::Display for WrapMapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WrapMapError::NoPartitionZeroDescriptors => {
                write!(f, "no partition-zero REOWP descriptors")
            }
            WrapMapError::FirstWrapNotZero { found } => {
                write!(
                    f,
                    "first partition-zero descriptor reports wrap {found}, not wrap 0"
                )
            }
            WrapMapError::NonContiguousWrapNumbers { expected, found } => {
                write!(
                    f,
                    "non-contiguous wrap numbers: expected {expected}, found {found}"
                )
            }
            WrapMapError::EndLoiOverflow { wrap } => {
                write!(f, "end_loi + 1 overflows u64 at wrap {wrap}")
            }
            WrapMapError::NonPositiveCompletedSpan { wrap } => {
                write!(
                    f,
                    "completed wrap {wrap} has end_loi before its derived start"
                )
            }
            WrapMapError::NonPositiveEodSpan {
                eod_wrap_start,
                mapped_extent_lba,
            } => write!(
                f,
                "EOD wrap starting at {eod_wrap_start} has no positive written span \
                 (mapped_extent_lba {mapped_extent_lba})"
            ),
        }
    }
}

impl std::error::Error for WrapMapError {}

/// A block outside the map's coverage. Coverage is checked against the
/// exclusive `mapped_extent_lba` before any wrap arithmetic runs — no
/// fraction is computed and no search runs for an uncovered block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageError {
    /// The uncovered block.
    pub block_lba: u64,
    /// The map's exclusive coverage bound.
    pub mapped_extent_lba: u64,
}

impl fmt::Display for CoverageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block {} is at or beyond the map's exclusive mapped extent {}",
            self.block_lba, self.mapped_extent_lba
        )
    }
}

impl std::error::Error for CoverageError {}

/// A block's physical position as derived from the wrap map alone.
/// Band information needs structural geometry and is added by the
/// planner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockPosition {
    /// The wrap holding the block.
    pub wrap_index: u32,
    /// Head travel direction on that wrap.
    pub direction: TapeDirection,
    /// Longitudinal position along the tape: `frac` on forward wraps,
    /// `1 - frac` on reverse wraps. Exact. May leave `[0, 1]` inside the
    /// EOD wrap, whose denominator is an estimate.
    pub physical_lpos: Ratio,
    /// True when the fraction's denominator is the estimated EOD-wrap
    /// denominator rather than a measured completed span.
    pub uses_eod_denominator: bool,
}

/// Lower median of a sample: element `(n - 1) / 2` of the values sorted
/// into nondecreasing order, under zero-based integer division — design
/// §6.4. An even sample uses the lower of its two central values and
/// never averages them. `None` on an empty sample.
pub fn lower_median(values: &[u64]) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Some(sorted[(sorted.len() - 1) / 2])
}

/// The per-volume wrap map: harvested descriptors plus everything §6.4
/// derives from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrapMap {
    /// Partition-zero descriptors exactly as harvested.
    descriptors: Vec<ReowpDescriptor>,
    /// Derived wrap starts; `wrap_starts[0] == 0`. One entry per wrap,
    /// including the EOD wrap. Contains no synthetic boundary.
    wrap_starts: Vec<u64>,
    /// Inclusive spans of the completed wraps; `len == eod_wrap`.
    completed_spans: Vec<u64>,
    /// Exclusive extent of the map, copied from the EOD descriptor's
    /// `end_loi`. The only value coverage is checked against.
    mapped_extent_lba: u64,
    /// The derived EOD-wrap denominator record.
    eod_denominator: EodDenominator,
}

impl WrapMap {
    /// Build a map from harvested descriptors, applying the §6.4
    /// inclusive/exclusive rules with checked arithmetic.
    ///
    /// Descriptors with `partition != 0` are ignored. Partition-zero
    /// descriptors must arrive contiguous ascending from wrap zero; any
    /// other shape is a validation failure that leaves the volume
    /// uncalibrated at the harvest layer.
    pub fn from_descriptors(descriptors: &[ReowpDescriptor]) -> Result<WrapMap, WrapMapError> {
        let p0: Vec<ReowpDescriptor> = descriptors
            .iter()
            .filter(|d| d.partition == 0)
            .copied()
            .collect();
        if p0.is_empty() {
            return Err(WrapMapError::NoPartitionZeroDescriptors);
        }
        for (i, d) in p0.iter().enumerate() {
            let expected = i as u32;
            if d.wrap_number != expected {
                return Err(if i == 0 {
                    WrapMapError::FirstWrapNotZero {
                        found: d.wrap_number,
                    }
                } else {
                    WrapMapError::NonContiguousWrapNumbers {
                        expected,
                        found: d.wrap_number,
                    }
                });
            }
        }

        // Wrap zero begins at logical object identifier zero; each later
        // reported wrap starts one past its predecessor's inclusive end.
        let mut wrap_starts: Vec<u64> = vec![0];
        let mut completed_spans: Vec<u64> = Vec::with_capacity(p0.len() - 1);
        for (w, d) in p0[..p0.len() - 1].iter().enumerate() {
            let start = wrap_starts[w];
            let next_start = d
                .end_loi
                .checked_add(1)
                .ok_or(WrapMapError::EndLoiOverflow { wrap: w as u32 })?;
            if d.end_loi < start {
                return Err(WrapMapError::NonPositiveCompletedSpan { wrap: w as u32 });
            }
            // Inclusive span: end - start + 1, overflow-free because
            // next_start = end + 1 succeeded.
            completed_spans.push(next_start - start);
            wrap_starts.push(next_start);
        }

        let eod = p0[p0.len() - 1];
        let mapped_extent_lba = eod.end_loi; // exclusive; copied, not a boundary
        let eod_wrap_start = *wrap_starts.last().expect("wrap_starts is never empty");
        if mapped_extent_lba <= eod_wrap_start {
            return Err(WrapMapError::NonPositiveEodSpan {
                eod_wrap_start,
                mapped_extent_lba,
            });
        }

        let eod_denominator = if completed_spans.is_empty() {
            EodDenominator {
                span_lba: mapped_extent_lba - eod_wrap_start,
                basis: EodDenominatorBasis::EodObservedSpan,
                completed_span_sample_count: 0,
            }
        } else {
            EodDenominator {
                span_lba: lower_median(&completed_spans)
                    .expect("completed_spans is non-empty in this branch"),
                basis: EodDenominatorBasis::MedianCompletedWrap,
                completed_span_sample_count: completed_spans.len() as u32,
            }
        };

        Ok(WrapMap {
            descriptors: p0,
            wrap_starts,
            completed_spans,
            mapped_extent_lba,
            eod_denominator,
        })
    }

    /// The partition-zero descriptors exactly as harvested.
    pub fn descriptors(&self) -> &[ReowpDescriptor] {
        &self.descriptors
    }

    /// Derived wrap starts, one per wrap including the EOD wrap.
    pub fn wrap_starts(&self) -> &[u64] {
        &self.wrap_starts
    }

    /// Inclusive spans of the completed wraps.
    pub fn completed_spans(&self) -> &[u64] {
        &self.completed_spans
    }

    /// Exclusive coverage bound of the map.
    pub fn mapped_extent_lba(&self) -> u64 {
        self.mapped_extent_lba
    }

    /// Number of wraps in the map, including the EOD wrap.
    pub fn wrap_count(&self) -> u32 {
        self.wrap_starts.len() as u32
    }

    /// Wrap number of the wrap holding EOD.
    pub fn eod_wrap(&self) -> u32 {
        (self.wrap_starts.len() - 1) as u32
    }

    /// The derived EOD-wrap denominator record.
    pub fn eod_denominator(&self) -> EodDenominator {
        self.eod_denominator
    }

    /// Whether the completed spans are highly dispersed — design §6.4:
    /// normalised MAD above 0.05, decided by checked integer
    /// cross-multiplication `100 * MAD > 5 * median`, never floats.
    /// A dispersed volume keeps the lower median as its denominator, but
    /// the EOD-wrap estimate is unreliable and the response says so.
    pub fn completed_spans_highly_dispersed(&self) -> bool {
        let Some(median) = lower_median(&self.completed_spans) else {
            return false;
        };
        let deviations: Vec<u64> = self
            .completed_spans
            .iter()
            .map(|&s| s.abs_diff(median))
            .collect();
        let mad = lower_median(&deviations).expect("deviations mirror completed_spans");
        100u128 * u128::from(mad) > 5u128 * u128::from(median)
    }

    /// Map a block to its wrap, direction and exact longitudinal
    /// position — the §6.4 mapping.
    ///
    /// Coverage against the exclusive `mapped_extent_lba` is checked
    /// first: an uncovered block computes no fraction and runs no
    /// search. The wrap is found upper-bound-minus-one: the first
    /// derived start strictly greater than the block, minus one.
    pub fn locate(&self, block_lba: u64) -> Result<BlockPosition, CoverageError> {
        if block_lba >= self.mapped_extent_lba {
            return Err(CoverageError {
                block_lba,
                mapped_extent_lba: self.mapped_extent_lba,
            });
        }
        // upper_bound - 1: partition_point returns the first index whose
        // start exceeds the block; wrap_starts[0] == 0 <= block keeps it
        // >= 1.
        let ub = self.wrap_starts.partition_point(|&s| s <= block_lba);
        let wrap_index = ub - 1;
        let offset = block_lba - self.wrap_starts[wrap_index];
        let is_eod = wrap_index == self.eod_wrap() as usize;
        let denominator = if is_eod {
            self.eod_denominator.span_lba
        } else {
            self.completed_spans[wrap_index]
        };
        let frac = Ratio::new(offset as i128, denominator as i128)
            .expect("map denominators are validated positive");
        let direction = if wrap_index % 2 == 0 {
            TapeDirection::Forward
        } else {
            TapeDirection::Reverse
        };
        let physical_lpos = match direction {
            TapeDirection::Forward => frac,
            TapeDirection::Reverse => frac
                .checked_one_minus()
                .expect("1 - frac cannot overflow for u64-derived fractions"),
        };
        Ok(BlockPosition {
            wrap_index: wrap_index as u32,
            direction,
            physical_lpos,
            uses_eod_denominator: is_eod,
        })
    }
}
