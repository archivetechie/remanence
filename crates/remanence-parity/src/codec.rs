//! Reed-Solomon codec for the Layer 3c `rs-cauchy-gf256-v1` scheme.
//!
//! This module implements the normative Appendix A definition directly:
//! GF(2^8) with polynomial `0x11D`, Cauchy generator
//! `G[j][i] = 1 / ((k + j) XOR i)`, systematic shard ordering, and parity
//! rows ordered by ascending parity index. The implementation keeps a small
//! ergonomic API for the writer and recovery path, and exposes accumulator
//! helpers so the streaming writer can build parity incrementally without
//! depending on a third-party crate's matrix choices.

use crate::error::ParityError;
use crate::model::ParityScheme;

/// Codec tied to one validated [`ParityScheme`].
///
/// The generator rows are stored as `m` rows of `k` coefficients from
/// Appendix A's Cauchy construction. Encoding is a linear XOR-accumulation
/// over those rows; reconstruction inverts the survivor submatrix of the
/// systematic generator `[I_k ; G]`.
#[derive(Debug)]
pub struct ReedSolomonCodec {
    k: usize,
    m: usize,
    generator: Vec<Vec<u8>>,
}

impl ReedSolomonCodec {
    /// Construct a codec from a parity scheme. The scheme is
    /// validated; invalid schemes return `ParityError::InvalidScheme`.
    pub fn new(scheme: &ParityScheme) -> Result<Self, ParityError> {
        scheme.validate()?;
        let k = scheme.data_blocks_per_stripe as usize;
        let m = scheme.parity_blocks_per_stripe as usize;
        let generator = cauchy_generator(k, m)?;
        Ok(Self { k, m, generator })
    }

    /// Data blocks per stripe (`k`).
    pub fn data_blocks(&self) -> usize {
        self.k
    }

    /// Parity blocks per stripe (`m`).
    pub fn parity_blocks(&self) -> usize {
        self.m
    }

    /// Encode parity blocks for one stripe using Appendix A's Cauchy matrix.
    ///
    /// `data` must contain `k` slices of identical length
    /// `block_size`. The function allocates `m` zero-filled
    /// `Vec<u8>`s of the same length, runs the RS encode, and
    /// returns them.
    ///
    /// Errors if `data.len() != k` or any data block has the
    /// wrong length (returns `ParityError::Invariant`).
    pub fn encode(&self, data: &[Vec<u8>]) -> Result<Vec<Vec<u8>>, ParityError> {
        if data.len() != self.k {
            return Err(ParityError::Invariant(
                "encode: data row count != k (stripe accounting bug)",
            ));
        }
        let block_size = match data.first() {
            Some(b) => b.len(),
            None => 0,
        };
        if data.iter().any(|b| b.len() != block_size) {
            return Err(ParityError::Invariant(
                "encode: data blocks have heterogeneous sizes",
            ));
        }

        let mut parity = self.new_parity_accumulators(block_size);
        for (data_index, shard) in data.iter().enumerate() {
            self.accumulate(data_index, shard, &mut parity)?;
        }
        Ok(parity)
    }

    /// Allocate zeroed parity accumulators for one stripe.
    ///
    /// Call [`Self::accumulate`] once for each real data shard. Missing final
    /// partial-epoch data shards are implicit zeros, so callers do not need to
    /// accumulate anything for those positions.
    pub fn new_parity_accumulators(&self, block_size: usize) -> Vec<Vec<u8>> {
        vec![vec![0u8; block_size]; self.m]
    }

    /// Add one data shard into existing parity accumulators.
    ///
    /// The order of calls is immaterial because Appendix A's encoding is a
    /// fixed linear combination over GF(2^8). This is the primitive the
    /// streaming writer can use to make incremental parity byte-identical to
    /// batch encoding.
    pub fn accumulate(
        &self,
        data_index: usize,
        shard: &[u8],
        parity: &mut [Vec<u8>],
    ) -> Result<(), ParityError> {
        if data_index >= self.k {
            return Err(ParityError::Invariant(
                "accumulate: data_index outside 0..k",
            ));
        }
        if parity.len() != self.m {
            return Err(ParityError::Invariant(
                "accumulate: parity accumulator count != m",
            ));
        }
        if parity.iter().any(|p| p.len() != shard.len()) {
            return Err(ParityError::Invariant(
                "accumulate: accumulator block size mismatch",
            ));
        }

        for (parity_index, out) in parity.iter_mut().enumerate() {
            let coefficient = self.generator[parity_index][data_index];
            gf_mul_slice_xor(coefficient, shard, out);
        }
        Ok(())
    }

    /// Reconstruct missing shards in a stripe.
    ///
    /// `shards` must have exactly `k + m` entries (data shards
    /// first, then parity shards). Missing shards are `None`;
    /// surviving shards are `Some(payload)`. On success every
    /// `None` slot has been filled with the reconstructed
    /// shard.
    ///
    /// At least `k` surviving shards are required; fewer is returned as the
    /// same `ParityError::ReedSolomon(TooFewShardsPresent)` compatibility
    /// surface the prior wrapper exposed. Higher-level recovery code normally
    /// detects that case before calling this method and emits its structured
    /// `Unrecoverable` audit event.
    pub fn reconstruct(&self, shards: &mut [Option<Vec<u8>>]) -> Result<(), ParityError> {
        if shards.len() != self.k + self.m {
            return Err(ParityError::Invariant("reconstruct: shard count != k + m"));
        }

        let Some(shard_len) = shard_len(shards)? else {
            return Err(ParityError::ReedSolomon(
                reed_solomon_erasure::Error::TooFewShardsPresent,
            ));
        };
        if shards.iter().all(Option::is_some) {
            return Ok(());
        }

        let mut survivor_indices = Vec::with_capacity(self.k);
        let mut survivor_shards = Vec::with_capacity(self.k);
        for (index, shard) in shards.iter().enumerate() {
            if let Some(shard) = shard {
                survivor_indices.push(index);
                survivor_shards.push(shard.as_slice());
                if survivor_indices.len() == self.k {
                    break;
                }
            }
        }
        if survivor_indices.len() < self.k {
            return Err(ParityError::ReedSolomon(
                reed_solomon_erasure::Error::TooFewShardsPresent,
            ));
        }

        let survivor_matrix = survivor_indices
            .iter()
            .map(|&index| self.systematic_row(index))
            .collect::<Result<Vec<_>, _>>()?;
        let decode_matrix = invert_matrix(survivor_matrix)?;
        let mut data = vec![vec![0u8; shard_len]; self.k];
        for data_index in 0..self.k {
            for survivor_index in 0..self.k {
                let coefficient = decode_matrix[data_index][survivor_index];
                gf_mul_slice_xor(
                    coefficient,
                    survivor_shards[survivor_index],
                    &mut data[data_index],
                );
            }
        }

        for data_index in 0..self.k {
            if shards[data_index].is_none() {
                shards[data_index] = Some(data[data_index].clone());
            }
        }

        if shards[self.k..].iter().any(Option::is_none) {
            let parity = self.encode(&data)?;
            for (parity_index, parity_shard) in parity.iter().enumerate() {
                let shard_index = self.k + parity_index;
                if shards[shard_index].is_none() {
                    shards[shard_index] = Some(parity_shard.clone());
                }
            }
        }

        Ok(())
    }

    fn systematic_row(&self, shard_index: usize) -> Result<Vec<u8>, ParityError> {
        if shard_index < self.k {
            let mut row = vec![0u8; self.k];
            row[shard_index] = 1;
            return Ok(row);
        }
        let parity_index = shard_index
            .checked_sub(self.k)
            .ok_or(ParityError::Invariant("systematic row underflow"))?;
        self.generator
            .get(parity_index)
            .cloned()
            .ok_or(ParityError::Invariant("systematic parity row outside 0..m"))
    }
}

fn shard_len(shards: &[Option<Vec<u8>>]) -> Result<Option<usize>, ParityError> {
    let mut shard_len = None;
    for shard in shards.iter().flatten() {
        if shard.is_empty() {
            return Err(ParityError::ReedSolomon(
                reed_solomon_erasure::Error::EmptyShard,
            ));
        }
        match shard_len {
            Some(expected) if shard.len() != expected => {
                return Err(ParityError::ReedSolomon(
                    reed_solomon_erasure::Error::IncorrectShardSize,
                ));
            }
            Some(_) => {}
            None => shard_len = Some(shard.len()),
        }
    }
    Ok(shard_len)
}

fn cauchy_generator(k: usize, m: usize) -> Result<Vec<Vec<u8>>, ParityError> {
    let width = k
        .checked_add(m)
        .ok_or(ParityError::Invariant("Cauchy generator width overflows"))?;
    if width > 255 {
        return Err(ParityError::InvalidScheme(format!(
            "k + m = {width} > 255 — GF(2^8) RS limit"
        )));
    }

    let mut rows = Vec::with_capacity(m);
    for parity_index in 0..m {
        let x = u8::try_from(k + parity_index)
            .map_err(|_| ParityError::Invariant("Cauchy X seed exceeds u8"))?;
        let mut row = Vec::with_capacity(k);
        for data_index in 0..k {
            let y = u8::try_from(data_index)
                .map_err(|_| ParityError::Invariant("Cauchy Y seed exceeds u8"))?;
            row.push(gf_inv(x ^ y)?);
        }
        rows.push(row);
    }
    Ok(rows)
}

fn invert_matrix(mut matrix: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>, ParityError> {
    let n = matrix.len();
    if matrix.iter().any(|row| row.len() != n) {
        return Err(ParityError::Invariant("decode matrix is not square"));
    }

    let mut inverse = vec![vec![0u8; n]; n];
    for (i, row) in inverse.iter_mut().enumerate() {
        row[i] = 1;
    }

    for col in 0..n {
        let pivot = (col..n)
            .find(|&row| matrix[row][col] != 0)
            .ok_or(ParityError::Invariant("Cauchy decode matrix is singular"))?;
        if pivot != col {
            matrix.swap(pivot, col);
            inverse.swap(pivot, col);
        }

        let pivot_inv = gf_inv(matrix[col][col])?;
        for value in &mut matrix[col] {
            *value = gf_mul(*value, pivot_inv);
        }
        for value in &mut inverse[col] {
            *value = gf_mul(*value, pivot_inv);
        }

        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = matrix[row][col];
            if factor == 0 {
                continue;
            }
            for c in 0..n {
                matrix[row][c] ^= gf_mul(factor, matrix[col][c]);
                inverse[row][c] ^= gf_mul(factor, inverse[col][c]);
            }
        }
    }

    Ok(inverse)
}

fn gf_inv(value: u8) -> Result<u8, ParityError> {
    if value == 0 {
        return Err(ParityError::Invariant("attempted GF inverse of zero"));
    }
    Ok(gf_pow(value, 254))
}

fn gf_pow(mut value: u8, mut exponent: u16) -> u8 {
    let mut result = 1u8;
    while exponent > 0 {
        if exponent & 1 == 1 {
            result = gf_mul(result, value);
        }
        value = gf_mul(value, value);
        exponent >>= 1;
    }
    result
}

static GF_MUL_TABLE: [[u8; 256]; 256] = build_gf_mul_table();

const fn build_gf_mul_table() -> [[u8; 256]; 256] {
    let mut table = [[0u8; 256]; 256];
    let mut a = 0usize;
    while a < 256 {
        let mut b = 0usize;
        while b < 256 {
            table[a][b] = gf_mul_bitwise(a as u8, b as u8);
            b += 1;
        }
        a += 1;
    }
    table
}

const fn gf_mul_bitwise(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            product ^= a;
        }
        let carry = a & 0x80 != 0;
        a <<= 1;
        if carry {
            a ^= 0x1d;
        }
        b >>= 1;
    }
    product
}

fn gf_mul(a: u8, b: u8) -> u8 {
    GF_MUL_TABLE[a as usize][b as usize]
}

fn gf_mul_slice_xor(coefficient: u8, input: &[u8], output: &mut [u8]) {
    debug_assert_eq!(input.len(), output.len());
    match coefficient {
        0 => {}
        1 => {
            for (out, byte) in output.iter_mut().zip(input) {
                *out ^= *byte;
            }
        }
        _ => {
            let row = &GF_MUL_TABLE[coefficient as usize];
            for (out, byte) in output.iter_mut().zip(input) {
                *out ^= row[*byte as usize];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SchemeId;

    fn small_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        }
    }

    fn appendix_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("rs-cauchy-gf256-v1"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        }
    }

    fn patterned_data(k: usize, block_size: usize) -> Vec<Vec<u8>> {
        (0..k)
            .map(|i| {
                (0..block_size)
                    .map(|j| {
                        let value = (i as u32 * 73 + j as u32 * 29 + (i * j) as u32 * 11) & 0xff;
                        value as u8
                    })
                    .collect()
            })
            .collect()
    }

    fn deterministic_random_data(k: usize, block_size: usize, mut state: u64) -> Vec<Vec<u8>> {
        (0..k)
            .map(|_| {
                (0..block_size)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state >> 32) as u8
                    })
                    .collect()
            })
            .collect()
    }

    fn slow_gf_mul(a: u8, b: u8) -> u8 {
        let mut product = 0u16;
        for bit in 0..8 {
            if (b >> bit) & 1 != 0 {
                product ^= (a as u16) << bit;
            }
        }
        for degree in (8..=14).rev() {
            if product & (1u16 << degree) != 0 {
                product ^= 0x11d_u16 << (degree - 8);
            }
        }
        debug_assert!(product <= 0xff);
        product as u8
    }

    fn slow_gf_inv(value: u8) -> u8 {
        assert_ne!(value, 0, "zero has no GF inverse");
        (1u16..=255)
            .map(|candidate| candidate as u8)
            .find(|candidate| slow_gf_mul(value, *candidate) == 1)
            .expect("every non-zero GF(2^8) element has an inverse")
    }

    fn slow_appendix_a_encode(data: &[Vec<u8>], m: usize) -> Vec<Vec<u8>> {
        let k = data.len();
        let block_size = data.first().map_or(0, Vec::len);
        assert!(k + m <= 255, "GF(2^8) supports at most 255 Cauchy seeds");
        assert!(
            data.iter().all(|block| block.len() == block_size),
            "test data must use homogeneous block sizes"
        );

        let mut parity = vec![vec![0u8; block_size]; m];
        for (parity_index, out) in parity.iter_mut().enumerate() {
            let x = (k + parity_index) as u8;
            for (data_index, shard) in data.iter().enumerate() {
                let y = data_index as u8;
                let coefficient = slow_gf_inv(x ^ y);
                for (out_byte, data_byte) in out.iter_mut().zip(shard) {
                    *out_byte ^= slow_gf_mul(coefficient, *data_byte);
                }
            }
        }
        parity
    }

    fn combinations_of_size(n: usize, size: usize) -> Vec<Vec<usize>> {
        fn visit(
            n: usize,
            size: usize,
            start: usize,
            current: &mut Vec<usize>,
            out: &mut Vec<Vec<usize>>,
        ) {
            if current.len() == size {
                out.push(current.clone());
                return;
            }
            let remaining = size - current.len();
            for index in start..=n - remaining {
                current.push(index);
                visit(n, size, index + 1, current, out);
                current.pop();
            }
        }

        let mut out = Vec::new();
        let mut current = Vec::new();
        visit(n, size, 0, &mut current, &mut out);
        out
    }

    fn assert_reconstructs_erasure_set(
        codec: &ReedSolomonCodec,
        data: &[Vec<u8>],
        parity: &[Vec<u8>],
        missing: &[usize],
    ) {
        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        shards.extend(parity.iter().cloned().map(Some));
        for &index in missing {
            shards[index] = None;
        }

        codec
            .reconstruct(&mut shards)
            .expect("selected erasure set reconstructs");

        for (index, expected) in data.iter().enumerate() {
            assert_eq!(
                shards[index].as_ref(),
                Some(expected),
                "data shard {index} was not reconstructed byte-identically"
            );
        }
        for (index, expected) in parity.iter().enumerate() {
            let shard_index = codec.data_blocks() + index;
            assert_eq!(
                shards[shard_index].as_ref(),
                Some(expected),
                "parity shard {index} was not reconstructed byte-identically"
            );
        }
    }

    #[test]
    fn encode_and_decode_roundtrip_with_no_erasures() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap();
        let data: Vec<Vec<u8>> = (0..codec.data_blocks())
            .map(|i| vec![i as u8 + 1; 64])
            .collect();
        let parity = codec.encode(&data).expect("encode ok");
        assert_eq!(parity.len(), codec.parity_blocks());
        // Every parity block matches the block size.
        for p in &parity {
            assert_eq!(p.len(), 64);
        }
    }

    #[test]
    fn reconstruct_one_lost_data_block() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap();
        let data: Vec<Vec<u8>> = (0..codec.data_blocks())
            .map(|i| vec![(i + 1) as u8; 64])
            .collect();
        let parity = codec.encode(&data).unwrap();

        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        shards.extend(parity.iter().cloned().map(Some));
        // Lose data block 1.
        shards[1] = None;
        codec.reconstruct(&mut shards).expect("reconstruct ok");
        let recovered = shards[1].as_ref().unwrap();
        assert_eq!(recovered, &data[1]);
    }

    #[test]
    fn reconstruct_two_lost_blocks_at_m_equals_two() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap(); // m=2
        let data: Vec<Vec<u8>> = (0..codec.data_blocks())
            .map(|i| vec![(i + 1) as u8; 32])
            .collect();
        let parity = codec.encode(&data).unwrap();
        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        shards.extend(parity.iter().cloned().map(Some));
        // Lose two — a data block + a parity block (within m=2).
        shards[0] = None;
        shards[5] = None;
        codec.reconstruct(&mut shards).expect("reconstruct ok");
        assert_eq!(shards[0].as_ref().unwrap(), &data[0]);
        assert_eq!(shards[5].as_ref().unwrap(), &parity[1]);
    }

    #[test]
    fn reconstruct_three_lost_at_m_equals_two_fails() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap(); // m=2
        let data: Vec<Vec<u8>> = (0..codec.data_blocks())
            .map(|i| vec![(i + 1) as u8; 32])
            .collect();
        let parity = codec.encode(&data).unwrap();
        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        shards.extend(parity.iter().cloned().map(Some));
        shards[0] = None;
        shards[1] = None;
        shards[2] = None;
        let err = codec.reconstruct(&mut shards).unwrap_err();
        assert!(matches!(err, ParityError::ReedSolomon(_)));
    }

    #[test]
    fn encode_rejects_wrong_data_count() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap();
        let data = vec![vec![0u8; 4]; codec.data_blocks() - 1];
        let err = codec.encode(&data).unwrap_err();
        assert!(matches!(err, ParityError::Invariant(_)));
    }

    #[test]
    fn encode_rejects_heterogeneous_block_sizes() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap();
        let mut data: Vec<Vec<u8>> = (0..codec.data_blocks()).map(|_| vec![0u8; 32]).collect();
        data[2] = vec![0u8; 33];
        let err = codec.encode(&data).unwrap_err();
        assert!(matches!(err, ParityError::Invariant(_)));
    }

    #[test]
    fn reconstruct_rejects_wrong_shard_count() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap();
        let mut shards: Vec<Option<Vec<u8>>> =
            vec![None; codec.data_blocks() + codec.parity_blocks() - 1];
        let err = codec.reconstruct(&mut shards).unwrap_err();
        assert!(matches!(err, ParityError::Invariant(_)));
    }

    #[test]
    fn new_propagates_invalid_scheme_error() {
        let bad = ParityScheme {
            id: SchemeId::new_static("bad"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 5, // m > k
            stripes_per_neighborhood: 1,
        };
        let err = ReedSolomonCodec::new(&bad).unwrap_err();
        assert!(matches!(err, ParityError::InvalidScheme(_)));
    }

    #[test]
    fn encode_decode_at_default_scheme_parameters_smoke() {
        // k=128, m=4 — the production default. Use a tiny
        // block size (16 bytes) to keep the test fast.
        let codec = ReedSolomonCodec::new(&crate::default_scheme()).unwrap();
        let data: Vec<Vec<u8>> = (0..codec.data_blocks())
            .map(|i| {
                let mut v = vec![0u8; 16];
                v[0] = (i % 251) as u8;
                v[1] = ((i >> 8) % 251) as u8;
                v
            })
            .collect();
        let parity = codec.encode(&data).unwrap();
        let mut shards: Vec<Option<Vec<u8>>> = data.iter().cloned().map(Some).collect();
        shards.extend(parity.iter().cloned().map(Some));
        // Lose 4 (the full m) — should still recover.
        shards[5] = None;
        shards[42] = None;
        shards[100] = None;
        shards[127] = None;
        codec.reconstruct(&mut shards).expect("reconstruct at m");
        assert_eq!(shards[5].as_ref().unwrap(), &data[5]);
        assert_eq!(shards[42].as_ref().unwrap(), &data[42]);
        assert_eq!(shards[100].as_ref().unwrap(), &data[100]);
        assert_eq!(shards[127].as_ref().unwrap(), &data[127]);
    }

    #[test]
    fn randomized_small_scheme_encode_reconstruct_matrix() {
        for (k, m, block_size, seed, missing_sets) in [
            (
                2u16,
                2u16,
                257usize,
                0x0202_0000_0000_0001u64,
                vec![vec![0], vec![1, 2], vec![2, 3]],
            ),
            (
                4u16,
                2u16,
                509usize,
                0x0402_0000_0000_0002u64,
                vec![vec![0], vec![1, 4], vec![4, 5]],
            ),
            (
                8u16,
                3u16,
                769usize,
                0x0803_0000_0000_0003u64,
                vec![vec![0], vec![2, 7], vec![1, 8, 10]],
            ),
        ] {
            let scheme = ParityScheme {
                id: SchemeId::new_static("rs-cauchy-gf256-v1"),
                data_blocks_per_stripe: k,
                parity_blocks_per_stripe: m,
                stripes_per_neighborhood: 1,
            };
            let codec = ReedSolomonCodec::new(&scheme).unwrap();
            let data = deterministic_random_data(k as usize, block_size, seed);
            let parity = codec.encode(&data).unwrap();

            for missing in missing_sets {
                assert!(
                    missing.len() <= m as usize,
                    "test erasure set must stay inside parity capacity"
                );
                assert_reconstructs_erasure_set(&codec, &data, &parity, &missing);
            }
        }
    }

    #[test]
    fn randomized_encode_matches_independent_slow_appendix_a_codec() {
        for (k, m, block_size, seed) in [
            (2u16, 2u16, 257usize, 0x5a11_0202_0000_0001u64),
            (4u16, 2u16, 509usize, 0x5a11_0402_0000_0002u64),
            (8u16, 3u16, 769usize, 0x5a11_0803_0000_0003u64),
            (16u16, 4u16, 127usize, 0x5a11_1004_0000_0004u64),
        ] {
            let scheme = ParityScheme {
                id: SchemeId::new_static("rs-cauchy-gf256-v1"),
                data_blocks_per_stripe: k,
                parity_blocks_per_stripe: m,
                stripes_per_neighborhood: 1,
            };
            let codec = ReedSolomonCodec::new(&scheme).unwrap();
            let data = deterministic_random_data(k as usize, block_size, seed);
            let expected = slow_appendix_a_encode(&data, m as usize);

            assert_eq!(
                codec.encode(&data).unwrap(),
                expected,
                "Appendix A encode mismatch for k={k}, m={m}, block_size={block_size}"
            );
        }
    }

    #[test]
    fn reconstructs_every_small_erasure_pattern_up_to_m() {
        for (k, m, block_size, seed) in [
            (3u16, 2u16, 67usize, 0xe4a5_0302_0000_0001u64),
            (4u16, 2u16, 73usize, 0xe4a5_0402_0000_0002u64),
            (5u16, 3u16, 79usize, 0xe4a5_0503_0000_0003u64),
        ] {
            let scheme = ParityScheme {
                id: SchemeId::new_static("rs-cauchy-gf256-v1"),
                data_blocks_per_stripe: k,
                parity_blocks_per_stripe: m,
                stripes_per_neighborhood: 1,
            };
            let codec = ReedSolomonCodec::new(&scheme).unwrap();
            let data = deterministic_random_data(k as usize, block_size, seed);
            let parity = codec.encode(&data).unwrap();
            let shard_count = k as usize + m as usize;

            for missing_count in 0..=m as usize {
                for missing in combinations_of_size(shard_count, missing_count) {
                    assert_reconstructs_erasure_set(&codec, &data, &parity, &missing);
                }
            }
        }
    }

    #[test]
    fn default_scheme_full_block_deterministic_reconstruction_smoke() {
        let codec = ReedSolomonCodec::new(&crate::default_scheme()).unwrap();
        let block_size = 256 * 1024;
        let data =
            deterministic_random_data(codec.data_blocks(), block_size, 0xdefa_0170_cafe_f00d);
        let parity = codec.encode(&data).unwrap();

        assert_eq!(codec.data_blocks(), 128);
        assert_eq!(codec.parity_blocks(), 4);
        assert_eq!(data[0].len(), block_size);
        assert_eq!(parity[0].len(), block_size);

        assert_reconstructs_erasure_set(&codec, &data, &parity, &[5, 42, 100, 127]);
    }

    #[test]
    fn appendix_a_gf_inverses_match_design() {
        assert_eq!(gf_inv(0x02).unwrap(), 0x8e);
        assert_eq!(gf_inv(0x03).unwrap(), 0xf4);
    }

    #[test]
    fn appendix_a_generator_and_encoding_vector_match_design() {
        let codec = ReedSolomonCodec::new(&appendix_scheme()).unwrap();
        assert_eq!(codec.generator, vec![vec![0x8e, 0xf4], vec![0xf4, 0x8e]]);

        let data = vec![vec![0x01, 0x02, 0x03, 0x04], vec![0x10, 0x20, 0x30, 0x40]];
        let parity = codec.encode(&data).unwrap();
        assert_eq!(parity[0], vec![0x75, 0xea, 0x9f, 0xc9]);
        assert_eq!(parity[1], vec![0xfc, 0xe5, 0x19, 0xd7]);
    }

    #[test]
    fn appendix_a_reconstruction_vector_recovers_d1_from_d0_and_p0() {
        let codec = ReedSolomonCodec::new(&appendix_scheme()).unwrap();
        let data = vec![vec![0x01, 0x02, 0x03, 0x04], vec![0x10, 0x20, 0x30, 0x40]];
        let parity = codec.encode(&data).unwrap();
        let mut shards = vec![Some(data[0].clone()), None, Some(parity[0].clone()), None];

        codec.reconstruct(&mut shards).unwrap();

        assert_eq!(shards[0].as_ref().unwrap(), &data[0]);
        assert_eq!(shards[1].as_ref().unwrap(), &data[1]);
        assert_eq!(shards[3].as_ref().unwrap(), &parity[1]);
    }

    #[test]
    fn incremental_accumulation_matches_batch_encoding_in_any_order() {
        for (k, m) in [(2u16, 2u16), (4, 2), (8, 3)] {
            let scheme = ParityScheme {
                id: SchemeId::new_static("rs-cauchy-gf256-v1"),
                data_blocks_per_stripe: k,
                parity_blocks_per_stripe: m,
                stripes_per_neighborhood: 1,
            };
            let codec = ReedSolomonCodec::new(&scheme).unwrap();
            let data = patterned_data(k as usize, 31);
            let batch = codec.encode(&data).unwrap();

            let mut reversed = codec.new_parity_accumulators(31);
            for data_index in (0..k as usize).rev() {
                codec
                    .accumulate(data_index, &data[data_index], &mut reversed)
                    .unwrap();
            }
            assert_eq!(reversed, batch);

            let mut permuted = codec.new_parity_accumulators(31);
            let mut order: Vec<usize> = (0..k as usize).collect();
            order.sort_by_key(|idx| (idx * 37 + 11) % 97);
            for data_index in order {
                codec
                    .accumulate(data_index, &data[data_index], &mut permuted)
                    .unwrap();
            }
            assert_eq!(permuted, batch);
        }
    }

    #[test]
    fn implicit_zero_final_epoch_shards_match_explicit_zero_blocks() {
        let codec = ReedSolomonCodec::new(&small_scheme()).unwrap();
        let mut explicit = patterned_data(codec.data_blocks(), 23);
        explicit[2].fill(0);
        explicit[3].fill(0);
        let batch_with_zeros = codec.encode(&explicit).unwrap();

        let mut incremental = codec.new_parity_accumulators(23);
        codec.accumulate(0, &explicit[0], &mut incremental).unwrap();
        codec.accumulate(1, &explicit[1], &mut incremental).unwrap();

        assert_eq!(incremental, batch_with_zeros);
    }
}
