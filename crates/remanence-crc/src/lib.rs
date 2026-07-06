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

/// Compute CRC-64/XZ over `bytes`.
pub fn crc64_xz(bytes: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for &byte in bytes {
        crc = crc64_xz_update(crc, byte);
    }
    crc ^ u64::MAX
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
