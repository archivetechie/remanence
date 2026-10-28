//! Differential coverage and opt-in throughput measurement for slice arithmetic.

use super::*;
use crate::model::SchemeId;
use crate::sidecar::data_shard_crc64;
use sha2::{Digest, Sha256};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// Preserve the byte-wise multiply-add as the differential and timing baseline.
fn table_mul_slice_xor(coefficient: u8, input: &[u8], output: &mut [u8]) {
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

/// Generate reproducible random bytes without a new test dependency.
fn random_data(blocks: usize, len: usize, mut state: u64) -> Vec<Vec<u8>> {
    (0..blocks)
        .map(|_| {
            (0..len)
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

/// Compare all coefficients with random input and existing accumulator bytes.
#[test]
fn slice_multiply_add_matches_table() {
    for kernel in kernels::supported() {
        for len in (0..=65).chain([127, 255, 4095, 4096, 4097, 262143, 262144]) {
            let buffers = random_data(2, len + 96, 0x5eed_cafe);
            for (input_offset, output_offset) in [(0, 0), (1, 3), (15, 17), (31, 1)] {
                // Start from a 32-byte boundary so offsets exercise known alignment.
                let input_start = buffers[0].as_ptr().align_offset(32) + input_offset;
                let input = &buffers[0][input_start..input_start + len];
                let mut actual = buffers[1].clone();
                let output_start = actual.as_ptr().align_offset(32) + output_offset;
                for coefficient in 0..=255 {
                    actual.copy_from_slice(&buffers[1]);
                    let mut expected = actual.clone();
                    table_mul_slice_xor(
                        coefficient,
                        input,
                        &mut expected[output_start..output_start + len],
                    );
                    kernel.apply(
                        coefficient,
                        input,
                        &mut actual[output_start..output_start + len],
                    );
                    // Compare the guards too: bytes outside the output stay intact.
                    assert_eq!(
                    actual, expected,
                    "kernel={kernel:?}, coefficient={coefficient}, len={len}, offsets={input_offset}/{output_offset}"
                );
                }
            }
        }
    }
}

/// Mirror accumulate's validation and generator traversal with table arithmetic.
fn table_accumulate(codec: &ReedSolomonCodec, index: usize, data: &[u8], parity: &mut [Vec<u8>]) {
    assert!(index < codec.k);
    assert_eq!(parity.len(), codec.m);
    assert!(parity.iter().all(|out| out.len() == data.len()));
    for (row, out) in parity.iter_mut().enumerate() {
        table_mul_slice_xor(codec.generator[row][index], data, out);
    }
}

/// Time exactly 1 GiB using a reusable 32 MiB stripe; exclude setup and I/O.
fn time_accumulation(
    codec: &ReedSolomonCodec,
    data: &[Vec<u8>],
    kernel: Option<kernels::Kernel>,
) -> (Duration, Vec<Vec<u8>>) {
    let mut parity = codec.new_parity_accumulators(data[0].len());
    let start = Instant::now();
    for _ in 0..32 {
        for out in &mut parity {
            out.fill(0);
        }
        for (index, block) in data.iter().enumerate() {
            if let Some(kernel) = kernel {
                for (row, out) in parity.iter_mut().enumerate() {
                    kernel.apply(
                        black_box(codec.generator[row][index]),
                        black_box(block),
                        out,
                    );
                }
            } else {
                table_accumulate(black_box(codec), index, black_box(block), &mut parity);
            }
        }
        black_box(&parity);
    }
    (start.elapsed(), parity)
}

/// Measure the sink's per-block CRC and a pair of streaming SHA-256 updates.
/// The two hashes are a separate workload; the current sink has no such updates.
fn time_checksums(data: &[Vec<u8>]) -> (Duration, Duration) {
    let start = Instant::now();
    for _ in 0..32 {
        for block in data {
            black_box(data_shard_crc64(black_box(block)));
        }
    }
    let crc = start.elapsed();
    let mut first = Sha256::new();
    let mut second = Sha256::new();
    let start = Instant::now();
    for _ in 0..32 {
        for block in data {
            first.update(black_box(block));
            second.update(black_box(block));
        }
    }
    let sha = start.elapsed();
    black_box((first.finalize(), second.finalize()));
    (crc, sha)
}

/// Report three paired samples without a hardware-dependent speed assertion.
/// Run with `cargo test --release -p remanence-parity --lib
/// benchmark_write_kernels -- --ignored --nocapture --test-threads=1`.
#[test]
#[ignore = "opt-in 1 GiB throughput measurement; run in release mode"]
fn benchmark_write_kernels() {
    if cfg!(debug_assertions) {
        panic!("run this measurement in release mode");
    }
    let codec = ReedSolomonCodec::new(&ParityScheme {
        id: SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 4,
        stripes_per_neighborhood: 1,
    })
    .unwrap();
    let data = random_data(128, 256 * 1024, 0x5eed_cafe);
    println!("1 GiB/sample, k=128, m=4, block=262144; 32 MiB input reused 32 times");
    println!("seconds/GiB: accumulation, CRC-64/XZ, two SHA-256 updates (separate workload)");
    // Warm both implementations and compare a complete stripe before timing.
    let mut reference = codec.new_parity_accumulators(data[0].len());
    let mut accelerated = reference.clone();
    for (index, block) in data.iter().enumerate() {
        table_accumulate(&codec, index, block, &mut reference);
        codec.accumulate(index, block, &mut accelerated).unwrap();
    }
    assert_eq!(reference, accelerated);
    println!("selected kernel: {}", encoding_kernel());
    for kernel in std::iter::once(None).chain(kernels::supported().into_iter().map(Some)) {
        let name = kernel.map_or("table", |k| k.name());
        let mut samples = Vec::new();
        for sample in 1..=3 {
            let (elapsed, parity) = time_accumulation(&codec, &data, kernel);
            assert_eq!(reference, parity);
            samples.push(elapsed);
            println!(
                "sample={sample} kernel={name}: {:.6} seconds/GiB",
                elapsed.as_secs_f64()
            );
        }
        samples.sort();
        println!(
            "median kernel={name}: {:.6} seconds/GiB",
            samples[1].as_secs_f64()
        );
    }
    let (crc, sha) = time_checksums(&data);
    println!(
        "CRC={:.6}, two SHA-256={:.6} seconds/GiB",
        crc.as_secs_f64(),
        sha.as_secs_f64()
    );
}
