//! Shared CRC-64/XZ implementation for Remanence.
//!
//! The parity sidecar format and the append-only audit log both use CRC-64/XZ:
//! width 64, polynomial `0x42F0E1EBA9EA3693`, reflected input/output
//! implemented with reflected polynomial `0xC96C5795D7870F42`, initial value
//! `0xFFFF_FFFF_FFFF_FFFF`, and final XOR `0xFFFF_FFFF_FFFF_FFFF`.

/// CRC-64/XZ check value for ASCII `123456789`.
pub const CRC64_XZ_CHECK_VALUE: u64 = 0x995D_C9BB_DF19_39FA;

/// Reflected CRC-64/XZ polynomial used by the right-shifting update loop.
pub const CRC64_XZ_REFLECTED_POLY: u64 = 0xC96C_5795_D787_0F42;

const CRC64_XZ_TABLE: [u64; 256] = build_crc64_xz_table();
static CRC64_XZ_SLICES: [[u64; 256]; 8] = build_crc64_xz_slices();

/// Advance one reflected CRC-64/XZ bit step.
pub const fn crc64_xz_bit_step(crc: u64) -> u64 {
    if crc & 1 == 1 {
        (crc >> 1) ^ CRC64_XZ_REFLECTED_POLY
    } else {
        crc >> 1
    }
}

/// Build the table entry for one low byte by applying eight reflected bit steps.
pub const fn crc64_xz_table_entry(byte: u8) -> u64 {
    let mut crc = byte as u64;
    let mut bit = 0;
    while bit < 8 {
        crc = crc64_xz_bit_step(crc);
        bit += 1;
    }
    crc
}

/// Advance the internal, pre-final-XOR CRC-64/XZ state by one byte.
pub fn crc64_xz_update(crc: u64, byte: u8) -> u64 {
    let index = ((crc ^ u64::from(byte)) & 0xFF) as usize;
    (crc >> 8) ^ CRC64_XZ_TABLE[index]
}

/// Advance the internal, pre-final-XOR CRC-64/XZ state over a byte slice.
pub fn crc64_xz_update_slice(mut crc: u64, bytes: &[u8]) -> u64 {
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = crc ^ u64::from_le_bytes(chunk.try_into().unwrap());
        crc = CRC64_XZ_SLICES[7][(word & 0xff) as usize]
            ^ CRC64_XZ_SLICES[6][((word >> 8) & 0xff) as usize]
            ^ CRC64_XZ_SLICES[5][((word >> 16) & 0xff) as usize]
            ^ CRC64_XZ_SLICES[4][((word >> 24) & 0xff) as usize]
            ^ CRC64_XZ_SLICES[3][((word >> 32) & 0xff) as usize]
            ^ CRC64_XZ_SLICES[2][((word >> 40) & 0xff) as usize]
            ^ CRC64_XZ_SLICES[1][((word >> 48) & 0xff) as usize]
            ^ CRC64_XZ_SLICES[0][(word >> 56) as usize];
    }
    for &byte in chunks.remainder() {
        crc = crc64_xz_update(crc, byte);
    }
    crc
}

/// Compute CRC-64/XZ over `bytes` using eight table slices.
pub fn crc64_xz(bytes: &[u8]) -> u64 {
    crc64_xz_update_slice(u64::MAX, bytes) ^ u64::MAX
}

/// Each successive slice advances a table entry by one zero byte.
const fn build_crc64_xz_slices() -> [[u64; 256]; 8] {
    let mut tables = [[0; 256]; 8];
    tables[0] = CRC64_XZ_TABLE;
    let mut slice = 1;
    while slice < 8 {
        let mut byte = 0;
        while byte < 256 {
            let crc = tables[slice - 1][byte];
            tables[slice][byte] = (crc >> 8) ^ tables[0][(crc & 0xff) as usize];
            byte += 1;
        }
        slice += 1;
    }
    tables
}

const fn build_crc64_xz_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut byte = 0usize;
    while byte < 256 {
        table[byte] = crc64_xz_table_entry(byte as u8);
        byte += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Byte-at-a-time table implementation serves as the slice reference.
    fn crc64_xz_bytewise(bytes: &[u8]) -> u64 {
        bytes.iter().copied().fold(u64::MAX, crc64_xz_update) ^ u64::MAX
    }

    /// Deterministic pseudorandom input keeps alignment and split coverage repeatable.
    fn random_word(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn slices_match_bytewise_for_lengths_alignments_and_incremental_splits() {
        let mut random = 0x8a51_c742_19db_e630;
        let buffer: Vec<u8> = (0..4104).map(|_| random_word(&mut random) as u8).collect();
        // Each start offset and length covers every end offset modulo eight.
        for start in 0..8 {
            for len in 0..=4096 {
                let bytes = &buffer[start..start + len];
                let expected = crc64_xz_bytewise(bytes);
                assert_eq!(crc64_xz(bytes), expected, "start={start}, len={len}");
                let a = random_word(&mut random) as usize % (len + 1);
                let b = random_word(&mut random) as usize % (len + 1);
                let (a, b) = (a.min(b), a.max(b));
                let crc = crc64_xz_update_slice(u64::MAX, &bytes[..a]);
                let crc = crc64_xz_update_slice(crc, &bytes[a..b]);
                let crc = crc64_xz_update_slice(crc, &bytes[b..]);
                assert_eq!(crc ^ u64::MAX, expected);
            }
        }
    }

    /// Run explicitly with --release --ignored --nocapture; throughput is informational.
    #[test]
    #[ignore = "one-off CRC throughput measurement"]
    fn crc_throughput() {
        use std::{hint::black_box, time::Instant};
        let mut random = 0x8a51_c742_19db_e630;
        let buffer: Vec<u8> = (0..256 * 1024)
            .map(|_| random_word(&mut random) as u8)
            .collect();
        for (name, checksum) in [
            ("bytewise", crc64_xz_bytewise as fn(&[u8]) -> u64),
            ("slice-by-8", crc64_xz as fn(&[u8]) -> u64),
        ] {
            let start = Instant::now();
            for _ in 0..1024 {
                black_box(checksum(black_box(&buffer)));
            }
            let seconds = start.elapsed().as_secs_f64();
            println!(
                "{name}: {:.1} MB/s ({seconds:.3} s)",
                buffer.len() as f64 * 1024.0 / seconds / 1e6
            );
        }
    }

    fn crc64_xz_bitwise(bytes: &[u8]) -> u64 {
        let mut crc = u64::MAX;
        for &byte in bytes {
            crc ^= u64::from(byte);
            for _ in 0..8 {
                crc = crc64_xz_bit_step(crc);
            }
        }
        crc ^ u64::MAX
    }

    #[test]
    fn table_entries_match_reflected_bit_recurrence() {
        for byte in 0u8..=u8::MAX {
            assert_eq!(
                CRC64_XZ_TABLE[usize::from(byte)],
                crc64_xz_table_entry(byte)
            );
        }
    }

    #[test]
    fn table_update_matches_bitwise_reference_for_samples() {
        let samples: &[&[u8]] = &[
            b"",
            b"123456789",
            &[0x00],
            &[0xff],
            &[0x00, 0xff, 0x5a, 0xc3, 0x7e, 0x81],
            b"remanence crc64 xz",
        ];
        for sample in samples {
            assert_eq!(crc64_xz(sample), crc64_xz_bitwise(sample));
        }
    }

    #[test]
    fn normative_vectors_match_rem_parity_spec() {
        assert_eq!(crc64_xz(b""), 0x0000_0000_0000_0000);
        assert_eq!(crc64_xz(b"123456789"), CRC64_XZ_CHECK_VALUE);
        assert_eq!(crc64_xz(&[0x00]), 0x1fad_a173_6467_3f59);
        assert_eq!(crc64_xz(&[0xff]), 0xff00_0000_0000_0000);

        let all_zero = vec![0x00; 256 * 1024];
        let all_ff = vec![0xff; 256 * 1024];
        assert_eq!(crc64_xz(&all_zero), 0x261b_df3d_2998_38fc);
        assert_eq!(crc64_xz(&all_ff), 0x5543_3dd0_f389_08ba);
    }
}
