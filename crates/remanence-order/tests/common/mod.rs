//! Shared fixtures for the remanence-order acceptance tests.
#![allow(dead_code)]

use remanence_order::{
    hop_ns, physical_position, CostPriors, ReadTarget, ReowpDescriptor, StructuralRow, WrapMap,
};

/// Deterministic SplitMix64. Tests use it to generate fixtures from
/// fixed seeds; the solver itself has no randomness anywhere.
pub struct SplitMix64(pub u64);

impl SplitMix64 {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, bound)`; `bound > 0`.
    pub fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound
    }
}

/// A map of `wraps` total wraps (the last is the EOD wrap) with uniform
/// completed spans of `span` and `eod_written` blocks written into the
/// EOD wrap.
pub fn uniform_map(wraps: u32, span: u64, eod_written: u64) -> WrapMap {
    assert!(wraps >= 1 && span >= 1 && eod_written >= 1);
    let mut descriptors = Vec::new();
    for w in 0..wraps - 1 {
        descriptors.push(ReowpDescriptor {
            partition: 0,
            wrap_number: w,
            end_loi: u64::from(w + 1) * span - 1,
        });
    }
    descriptors.push(ReowpDescriptor {
        partition: 0,
        wrap_number: wraps - 1,
        end_loi: u64::from(wraps - 1) * span + eod_written,
    });
    WrapMap::from_descriptors(&descriptors).expect("uniform fixture map is valid")
}

/// A map whose completed spans jitter around `base_span` (+/- up to a
/// tenth), from a fixed seed.
pub fn jitter_map(wraps: u32, base_span: u64, eod_written: u64, seed: u64) -> WrapMap {
    assert!(wraps >= 2 && base_span >= 20);
    let mut rng = SplitMix64(seed);
    let mut descriptors = Vec::new();
    let mut next_start = 0u64;
    for w in 0..wraps - 1 {
        let jitter = rng.below(base_span / 10 + 1);
        let span = if rng.below(2) == 0 {
            base_span - jitter
        } else {
            base_span + jitter
        };
        descriptors.push(ReowpDescriptor {
            partition: 0,
            wrap_number: w,
            end_loi: next_start + span - 1,
        });
        next_start += span;
    }
    descriptors.push(ReowpDescriptor {
        partition: 0,
        wrap_number: wraps - 1,
        end_loi: next_start + eod_written,
    });
    WrapMap::from_descriptors(&descriptors).expect("jitter fixture map is valid")
}

/// A synthetic supported-shape geometry row for small maps. Satisfies
/// the table identity so band classification works for any wrap the map
/// can produce.
pub fn synthetic_geometry(wraps_per_band: u32) -> StructuralRow {
    StructuralRow {
        cartridge_generation: "SYN",
        recording_format: "S1",
        bands: 4,
        wraps_per_band,
        wraps: 4 * wraps_per_band,
        channels: 1,
        data_tracks: 4 * wraps_per_band,
        source: "synthetic test row",
    }
}

/// Random targets inside the map's coverage.
pub fn random_targets(rng: &mut SplitMix64, n: usize, mapped_extent: u64) -> Vec<ReadTarget> {
    assert!(mapped_extent > 200);
    (0..n)
        .map(|_| {
            let start = rng.below(mapped_extent - 101);
            let len = 1 + rng.below(100.min(mapped_extent - start - 1));
            ReadTarget {
                start_block: start,
                end_block: start + len - 1,
            }
        })
        .collect()
}

/// Cost matrices built through the public API, for feeding the
/// exhaustive reference: `(m0, m, term)`.
pub fn matrices(
    map: &WrapMap,
    geometry: &StructuralRow,
    priors: &CostPriors,
    targets: &[ReadTarget],
    start_block: Option<u64>,
    end_block: Option<u64>,
) -> (Vec<u64>, Vec<Vec<u64>>, Option<Vec<u64>>) {
    let pos = |b: u64| {
        physical_position(map, geometry, b)
            .expect("test fixture blocks are covered")
            .0
    };
    let start = pos(start_block.unwrap_or(0));
    let starts: Vec<_> = targets.iter().map(|t| pos(t.start_block)).collect();
    let ends: Vec<_> = targets.iter().map(|t| pos(t.end_block)).collect();
    let n = targets.len();
    let m0: Vec<u64> = (0..n).map(|j| hop_ns(priors, &start, &starts[j]).0).collect();
    let m: Vec<Vec<u64>> = (0..n)
        .map(|i| (0..n).map(|j| hop_ns(priors, &ends[i], &starts[j]).0).collect())
        .collect();
    let term = end_block.map(|e| {
        let ep = pos(e);
        (0..n).map(|i| hop_ns(priors, &ends[i], &ep).0).collect()
    });
    (m0, m, term)
}

/// Exact optimum over every admissible permutation, by depth-first
/// enumeration with a partial-cost cut (costs are nonnegative, so a
/// partial sum already at or above the best complete route cannot
/// improve; the cut never excludes an optimal route).
pub fn exhaustive_optimal(
    m0: &[u64],
    m: &[Vec<u64>],
    term: Option<&[u64]>,
    admissible_firsts: &[usize],
) -> u128 {
    let n = m0.len();
    assert!(n >= 1);
    let mut best = u128::MAX;
    let mut used = vec![false; n];
    for &f in admissible_firsts {
        used[f] = true;
        dfs(f, 1, u128::from(m0[f]), n, m, term, &mut used, &mut best);
        used[f] = false;
    }
    best
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    current: usize,
    depth: usize,
    acc: u128,
    n: usize,
    m: &[Vec<u64>],
    term: Option<&[u64]>,
    used: &mut [bool],
    best: &mut u128,
) {
    if acc >= *best {
        return;
    }
    if depth == n {
        let total = acc + term.map_or(0, |t| u128::from(t[current]));
        if total < *best {
            *best = total;
        }
        return;
    }
    for j in 0..n {
        if !used[j] {
            used[j] = true;
            dfs(j, depth + 1, acc + u128::from(m[current][j]), n, m, term, used, best);
            used[j] = false;
        }
    }
}

/// The indices minimising the first-hop cost — the admissible firsts
/// under `MIN_TIME_TO_FIRST`.
pub fn min_first_hop_set(m0: &[u64]) -> Vec<usize> {
    let min = *m0.iter().min().expect("non-empty");
    (0..m0.len()).filter(|&j| m0[j] == min).collect()
}
