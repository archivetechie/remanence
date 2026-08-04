//! Acceptance tests for the §6.4 mapping: wrap boundaries, reverse
//! wraps, band fill, the EOD-wrap denominator, and the descriptor
//! validation failure paths.

mod common;

use common::uniform_map;
use remanence_order::{
    band_rank, lower_median, EodDenominatorBasis, Ratio, ReowpDescriptor, TapeDirection, WrapMap,
    WrapMapError,
};

// ---------------------------------------------------------------------
// Reverse wraps. An earlier draft of the design gave reverse wraps the
// forward longitudinal coordinate; these tests exist to keep that fixed.
// ---------------------------------------------------------------------

/// A block early in an *odd* wrap is physically far from the load point,
/// not near it.
#[test]
fn early_block_on_odd_wrap_is_far_from_load_point() {
    let span = 1000;
    let map = uniform_map(6, span, 500);
    // Ten blocks into wrap 1.
    let pos = map.locate(span + 10).unwrap();
    assert_eq!(pos.wrap_index, 1);
    assert_eq!(pos.direction, TapeDirection::Reverse);
    // frac = 10/1000, physically at 1 - 10/1000 = 99/100 of the tape.
    assert_eq!(pos.physical_lpos, Ratio::new(99, 100).unwrap());
    assert!(
        pos.physical_lpos > Ratio::new(1, 2).unwrap(),
        "an early odd-wrap block must sit in the far half of the tape"
    );
}

/// `physical_lpos` on both sides of every wrap boundary in the map:
/// exact values per parity, and the two boundary-adjacent blocks are
/// physically adjacent (1/span apart) even though the serpentine
/// reverses direction.
#[test]
fn physical_lpos_on_both_sides_of_every_wrap_boundary() {
    let span: u64 = 1000;
    let wraps = 6u32;
    let map = uniform_map(wraps, span, 500);
    let s = span as i128;
    for w in 1..wraps {
        let boundary = u64::from(w) * span;
        let last_before = map.locate(boundary - 1).unwrap();
        let first_after = map.locate(boundary).unwrap();
        assert_eq!(last_before.wrap_index, w - 1);
        assert_eq!(first_after.wrap_index, w);

        // Exact longitudinal positions by wrap parity.
        let expected_before = if (w - 1) % 2 == 0 {
            Ratio::new(s - 1, s).unwrap() // forward: near the far end
        } else {
            Ratio::new(1, s).unwrap() // reverse: near the load point
        };
        let expected_after = if w % 2 == 0 {
            Ratio::ZERO // forward wrap starts at the load-point end
        } else {
            Ratio::ONE // reverse wrap starts at the far end
        };
        assert_eq!(last_before.physical_lpos, expected_before, "wrap {w} left side");
        assert_eq!(first_after.physical_lpos, expected_after, "wrap {w} right side");

        // Boundary-adjacent blocks are physically adjacent: 1/span apart.
        let gap = last_before
            .physical_lpos
            .checked_abs_diff(first_after.physical_lpos)
            .unwrap();
        assert_eq!(gap, Ratio::new(1, s).unwrap(), "wrap {w} physical gap");
    }
}

/// Direction alternates with wrap parity: even forward, odd reverse.
#[test]
fn direction_follows_wrap_parity() {
    let map = uniform_map(8, 100, 50);
    for w in 0u64..8 {
        let pos = map.locate(w * 100 + 5).unwrap();
        let expected = if w % 2 == 0 {
            TapeDirection::Forward
        } else {
            TapeDirection::Reverse
        };
        assert_eq!(pos.direction, expected, "wrap {w}");
    }
}

// ---------------------------------------------------------------------
// Band mapping. Layout and fill order are different facts and the design
// once conflated them.
// ---------------------------------------------------------------------

/// Logical bands fill numerically: ascending wraps map to bands
/// 0, 1, 2, 3 in that order (fill order), while band 0 sits at physical
/// rank 2 (layout). Two different facts, asserted separately.
#[test]
fn bands_fill_numerically_and_band0_sits_at_rank_2() {
    // LTO-9-like: 70 wraps per band.
    let wpb = 70u32;
    // Fill order: division by wraps_per_band, ascending.
    let fill: Vec<u32> = [0u32, 69, 70, 139, 140, 209, 210, 279]
        .iter()
        .map(|w| w / wpb)
        .collect();
    assert_eq!(fill, [0, 0, 1, 1, 2, 2, 3, 3], "bands must fill numerically");
    // Layout: a separate lookup, not the fill order.
    assert_eq!(band_rank(0), Some(2), "band 0 sits at physical rank 2");
    assert_eq!(
        [band_rank(0), band_rank(1), band_rank(2), band_rank(3)],
        [Some(2), Some(1), Some(3), Some(0)]
    );
    // Bands 0 and 1 are physically adjacent (ranks 2 and 1); bands 0 and
    // 2 are not (ranks 2 and 3 vs a two-step from 1).
    assert_eq!(band_rank(0).unwrap().abs_diff(band_rank(1).unwrap()), 1);
    assert_eq!(band_rank(0).unwrap().abs_diff(band_rank(3).unwrap()), 2);
}

// ---------------------------------------------------------------------
// Wrap-boundary derivation and the search.
// ---------------------------------------------------------------------

/// Wrap starts derive from inclusive completed ends with the checked
/// `+1`; the harvested descriptors are kept exactly; `mapped_extent_lba`
/// is stored separately and is not a boundary.
#[test]
fn wrap_starts_derive_from_inclusive_ends() {
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 2199 }, // span 1200
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 2999 }, // span 800
        ReowpDescriptor { partition: 0, wrap_number: 3, end_loi: 3500 }, // EOD position
    ];
    let map = WrapMap::from_descriptors(&descriptors).unwrap();
    assert_eq!(map.wrap_starts(), &[0, 1000, 2200, 3000]);
    assert_eq!(map.completed_spans(), &[1000, 1200, 800]);
    assert_eq!(map.mapped_extent_lba(), 3500);
    assert_eq!(map.eod_wrap(), 3);
    // Harvested tuples unchanged.
    assert_eq!(map.descriptors(), &descriptors);
    // The EOD position is not the start of another wrap: the boundary
    // list ends at the EOD wrap's start.
    assert_eq!(map.wrap_starts().len(), 4);
    assert!(!map.wrap_starts().contains(&3500));
}

/// Upper-bound-minus-one agrees with a simple linear reference over the
/// whole covered range, uniform and jittered maps both.
#[test]
fn binary_search_matches_linear_reference() {
    for map in [uniform_map(7, 137, 60), common::jitter_map(9, 200, 90, 0xB0A7)] {
        let starts = map.wrap_starts();
        for block in 0..map.mapped_extent_lba() {
            let expected = starts
                .iter()
                .rposition(|&s| s <= block)
                .expect("wrap 0 starts at block 0");
            let got = map.locate(block).unwrap();
            assert_eq!(got.wrap_index as usize, expected, "block {block}");
        }
    }
}

/// Descriptors with a non-zero partition are ignored during harvest —
/// not a validation failure — since v1 plans only partition zero.
#[test]
fn non_zero_partition_descriptors_are_ignored() {
    let descriptors = [
        ReowpDescriptor { partition: 1, wrap_number: 0, end_loi: 77 },
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 1, wrap_number: 1, end_loi: 555 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 1500 },
    ];
    let map = WrapMap::from_descriptors(&descriptors).unwrap();
    assert_eq!(map.wrap_count(), 2);
    assert_eq!(map.descriptors().len(), 2);
    assert!(map.descriptors().iter().all(|d| d.partition == 0));
}

// ---------------------------------------------------------------------
// Coverage.
// ---------------------------------------------------------------------

/// Coverage is checked against the exclusive `mapped_extent_lba` before
/// any wrap arithmetic: at-extent and beyond-extent blocks fail, the
/// last covered block succeeds.
#[test]
fn coverage_is_exclusive_at_mapped_extent() {
    let map = uniform_map(4, 1000, 250);
    let extent = map.mapped_extent_lba();
    assert_eq!(extent, 3250);
    assert!(map.locate(extent - 1).is_ok());
    let err = map.locate(extent).unwrap_err();
    assert_eq!(err.block_lba, extent);
    assert_eq!(err.mapped_extent_lba, extent);
    assert!(map.locate(u64::MAX).is_err());
}

// ---------------------------------------------------------------------
// The EOD wrap.
// ---------------------------------------------------------------------

/// A block inside the EOD wrap uses the lower median of completed spans
/// as its denominator. The denominator is a labelled derived record —
/// basis, sample count — and is not inserted into the boundary list.
/// Wrap index and parity for the EOD wrap remain exact.
#[test]
fn eod_wrap_uses_median_completed_span_denominator() {
    // Completed spans 1000, 1200, 800, 1100 -> sorted [800, 1000, 1100,
    // 1200], lower median = element (4-1)/2 = index 1 = 1000.
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 2199 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 2999 },
        ReowpDescriptor { partition: 0, wrap_number: 3, end_loi: 4099 },
        ReowpDescriptor { partition: 0, wrap_number: 4, end_loi: 4600 }, // EOD
    ];
    let map = WrapMap::from_descriptors(&descriptors).unwrap();
    let den = map.eod_denominator();
    assert_eq!(den.basis, EodDenominatorBasis::MedianCompletedWrap);
    assert_eq!(den.span_lba, 1000);
    assert_eq!(den.completed_span_sample_count, 4);
    // Not materialised as a boundary.
    assert_eq!(map.wrap_starts().len(), 5);

    // A block 250 into the EOD wrap: exact index and parity, fraction
    // over the median denominator.
    let pos = map.locate(4100 + 250).unwrap();
    assert_eq!(pos.wrap_index, 4);
    assert_eq!(pos.direction, TapeDirection::Forward);
    assert!(pos.uses_eod_denominator);
    assert_eq!(pos.physical_lpos, Ratio::new(250, 1000).unwrap());
    // A completed-wrap block does not carry the flag.
    assert!(!map.locate(500).unwrap().uses_eod_denominator);
}

/// `lower_median` is element `(n - 1) / 2` of the sorted sample: an even
/// sample uses the lower central value, never an average.
#[test]
fn lower_median_is_the_lower_central_element() {
    assert_eq!(lower_median(&[800, 1000, 1100, 1200]), Some(1000));
    assert_eq!(lower_median(&[1200, 800, 1100, 1000]), Some(1000)); // unsorted input
    assert_eq!(lower_median(&[5, 3, 9]), Some(5));
    assert_eq!(lower_median(&[7]), Some(7));
    assert_eq!(lower_median(&[4, 8]), Some(4)); // lower of two, not 6
    assert_eq!(lower_median(&[]), None);
}

/// No completed wrap: the EOD wrap's own observed span is the
/// denominator, with basis and a zero sample count saying so.
#[test]
fn no_completed_wrap_uses_observed_span() {
    let descriptors = [ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 5000 }];
    let map = WrapMap::from_descriptors(&descriptors).unwrap();
    let den = map.eod_denominator();
    assert_eq!(den.basis, EodDenominatorBasis::EodObservedSpan);
    assert_eq!(den.span_lba, 5000);
    assert_eq!(den.completed_span_sample_count, 0);
    assert_eq!(map.wrap_count(), 1);
    let pos = map.locate(1250).unwrap();
    assert!(pos.uses_eod_denominator);
    assert_eq!(pos.physical_lpos, Ratio::new(1, 4).unwrap());
}

/// The EOD-wrap estimate can overshoot: written span beyond the median
/// puts `frac` above one, and on a reverse EOD wrap `1 - frac` goes
/// negative. The mapping stays exact, total and ordered — no clamp, no
/// panic. Parity and wrap index remain exact throughout.
#[test]
fn eod_overshoot_beyond_median_is_exact_not_clamped() {
    // Completed wrap span 1000; EOD wrap 1 (reverse) written to 4999,
    // i.e. 3999 blocks against a denominator of 1000.
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 4999 },
    ];
    let map = WrapMap::from_descriptors(&descriptors).unwrap();
    assert_eq!(map.eod_denominator().span_lba, 1000);
    let pos = map.locate(4000).unwrap(); // offset 3000, frac = 3
    assert_eq!(pos.wrap_index, 1);
    assert_eq!(pos.direction, TapeDirection::Reverse);
    assert_eq!(pos.physical_lpos, Ratio::new(-2, 1).unwrap());
    // Still strictly monotone (descending) along the reverse wrap.
    let a = map.locate(1100).unwrap().physical_lpos;
    let b = map.locate(2500).unwrap().physical_lpos;
    let c = map.locate(4600).unwrap().physical_lpos;
    assert!(a > b && b > c);
}

// ---------------------------------------------------------------------
// Dispersion.
// ---------------------------------------------------------------------

/// The dispersion rule: `100 * MAD > 5 * median` in checked integer
/// arithmetic. Strictly-greater — the boundary value is not dispersed.
#[test]
fn dispersion_threshold_is_strict_integer_comparison() {
    // spans [500, 1000, 1600]: median 1000, deviations [500, 0, 600],
    // MAD 500 -> 50000 > 5000: dispersed.
    let dispersed = WrapMap::from_descriptors(&[
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 499 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 1499 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 3099 },
        ReowpDescriptor { partition: 0, wrap_number: 3, end_loi: 3600 },
    ])
    .unwrap();
    assert!(dispersed.completed_spans_highly_dispersed());

    // spans [950, 1000, 1050]: median 1000, deviations [50, 0, 50],
    // MAD 50 -> 5000 == 5000: exactly at the threshold, NOT dispersed.
    let boundary = WrapMap::from_descriptors(&[
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 949 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 1949 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 2999 },
        ReowpDescriptor { partition: 0, wrap_number: 3, end_loi: 3500 },
    ])
    .unwrap();
    assert!(!boundary.completed_spans_highly_dispersed());

    // Near-uniform spans: not dispersed.
    let tight = WrapMap::from_descriptors(&[
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 2000 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 2999 },
        ReowpDescriptor { partition: 0, wrap_number: 3, end_loi: 3400 },
    ])
    .unwrap();
    assert!(!tight.completed_spans_highly_dispersed());

    // No completed wraps: nothing to disperse.
    let single = WrapMap::from_descriptors(&[ReowpDescriptor {
        partition: 0,
        wrap_number: 0,
        end_loi: 400,
    }])
    .unwrap();
    assert!(!single.completed_spans_highly_dispersed());
}

// ---------------------------------------------------------------------
// Harvest validation failure paths. These carry more weight than the
// happy path: a failure here must leave the volume uncalibrated, never
// produce a best-effort map.
// ---------------------------------------------------------------------

/// An empty descriptor set, or one with no partition-zero descriptors,
/// is a validation failure, not an empty map.
#[test]
fn no_partition_zero_descriptors_is_rejected() {
    assert_eq!(
        WrapMap::from_descriptors(&[]),
        Err(WrapMapError::NoPartitionZeroDescriptors)
    );
    let only_p1 = [ReowpDescriptor { partition: 1, wrap_number: 0, end_loi: 100 }];
    assert_eq!(
        WrapMap::from_descriptors(&only_p1),
        Err(WrapMapError::NoPartitionZeroDescriptors)
    );
}

/// The map must start at wrap zero — wrap zero begins at logical object
/// identifier zero, and a map missing it has no derivable starts.
#[test]
fn first_descriptor_must_report_wrap_zero() {
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 1999 },
    ];
    assert_eq!(
        WrapMap::from_descriptors(&descriptors),
        Err(WrapMapError::FirstWrapNotZero { found: 1 })
    );
}

/// Wrap numbers must be contiguous through the EOD wrap; gaps,
/// duplicates and reordering are all validation failures.
#[test]
fn non_contiguous_wrap_numbers_are_rejected() {
    let gap = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 2999 },
    ];
    assert_eq!(
        WrapMap::from_descriptors(&gap),
        Err(WrapMapError::NonContiguousWrapNumbers { expected: 1, found: 2 })
    );
    let dup = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 1999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 2999 },
    ];
    assert_eq!(
        WrapMap::from_descriptors(&dup),
        Err(WrapMapError::NonContiguousWrapNumbers { expected: 1, found: 0 })
    );
    let reordered = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 2999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 1999 },
    ];
    assert_eq!(
        WrapMap::from_descriptors(&reordered),
        Err(WrapMapError::NonContiguousWrapNumbers { expected: 1, found: 2 })
    );
}

/// The checked `+1` on a completed wrap's inclusive end: `u64::MAX`
/// cannot gain a successor, and the harvest is rejected rather than
/// wrapped.
#[test]
fn end_loi_overflow_rejects_the_harvest() {
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: u64::MAX },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 100 },
    ];
    assert_eq!(
        WrapMap::from_descriptors(&descriptors),
        Err(WrapMapError::EndLoiOverflow { wrap: 0 })
    );
}

/// A completed wrap whose end precedes its derived start has no positive
/// span and rejects the harvest.
#[test]
fn non_positive_completed_span_is_rejected() {
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 500 }, // before start 1000
        ReowpDescriptor { partition: 0, wrap_number: 2, end_loi: 3000 },
    ];
    assert_eq!(
        WrapMap::from_descriptors(&descriptors),
        Err(WrapMapError::NonPositiveCompletedSpan { wrap: 1 })
    );
}

/// A single-block completed wrap (end == start) is valid: span one.
#[test]
fn single_block_completed_wrap_is_valid() {
    let descriptors = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 0 }, // span 1
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 10 },
    ];
    let map = WrapMap::from_descriptors(&descriptors).unwrap();
    assert_eq!(map.completed_spans(), &[1]);
}

/// The EOD wrap must have a positive written span:
/// `mapped_extent_lba > wrap_start[eod_wrap]`, else harvest fails
/// descriptor validation and the volume stays uncalibrated (§6.5).
#[test]
fn non_positive_eod_span_is_rejected() {
    let at_start = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 1000 }, // == start
    ];
    assert_eq!(
        WrapMap::from_descriptors(&at_start),
        Err(WrapMapError::NonPositiveEodSpan { eod_wrap_start: 1000, mapped_extent_lba: 1000 })
    );
    let before_start = [
        ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 999 },
        ReowpDescriptor { partition: 0, wrap_number: 1, end_loi: 42 },
    ];
    assert!(matches!(
        WrapMap::from_descriptors(&before_start),
        Err(WrapMapError::NonPositiveEodSpan { .. })
    ));
    // Degenerate single-wrap map with EOD at zero: no positive span.
    let empty_volume = [ReowpDescriptor { partition: 0, wrap_number: 0, end_loi: 0 }];
    assert_eq!(
        WrapMap::from_descriptors(&empty_volume),
        Err(WrapMapError::NonPositiveEodSpan { eod_wrap_start: 0, mapped_extent_lba: 0 })
    );
}
