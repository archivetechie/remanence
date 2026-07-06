//! Verification extraction of Remanence CRC-64/XZ byte-step and slice-fold logic.
//!
//! This crate is a standalone, dependency-free model of
//! `crates/remanence-crc/src/lib.rs`. It mirrors the shared production
//! CRC-64/XZ implementation used by parity sidecars and audit records, while
//! keeping the byte-step and public slice-loop arithmetic small enough for
//! Lean/Aeneas verification.
//! The `drift_guard` test pins the production formulas this extraction mirrors;
//! if it fails, the extraction and Lean proofs must be re-synced.

pub const CRC64_XZ_CHECK_VALUE: u64 = 0x995D_C9BB_DF19_39FA;
pub const CRC64_XZ_REFLECTED_POLY: u64 = 0xC96C_5795_D787_0F42;

pub fn crc64_xz_bit_step(crc: u64) -> u64 {
    if crc & 1 == 1 {
        (crc >> 1) ^ CRC64_XZ_REFLECTED_POLY
    } else {
        crc >> 1
    }
}

pub fn crc64_xz_table_entry(byte: u8) -> u64 {
    let mut crc = byte as u64;
    crc = crc64_xz_bit_step(crc);
    crc = crc64_xz_bit_step(crc);
    crc = crc64_xz_bit_step(crc);
    crc = crc64_xz_bit_step(crc);
    crc = crc64_xz_bit_step(crc);
    crc = crc64_xz_bit_step(crc);
    crc = crc64_xz_bit_step(crc);
    crc64_xz_bit_step(crc)
}

pub fn crc64_xz_update(crc: u64, byte: u8) -> u64 {
    let index = ((crc ^ u64::from(byte)) & 0xFF) as u8;
    (crc >> 8) ^ crc64_xz_table_entry(index)
}

pub fn crc64_xz_one(byte: u8) -> u64 {
    crc64_xz_update(u64::MAX, byte) ^ u64::MAX
}

pub fn crc64_xz_two(first: u8, second: u8) -> u64 {
    let crc = crc64_xz_update(u64::MAX, first);
    crc64_xz_update(crc, second) ^ u64::MAX
}

#[allow(clippy::too_many_arguments)]
pub fn crc64_xz_nine(
    b0: u8,
    b1: u8,
    b2: u8,
    b3: u8,
    b4: u8,
    b5: u8,
    b6: u8,
    b7: u8,
    b8: u8,
) -> u64 {
    let crc = crc64_xz_update(u64::MAX, b0);
    let crc = crc64_xz_update(crc, b1);
    let crc = crc64_xz_update(crc, b2);
    let crc = crc64_xz_update(crc, b3);
    let crc = crc64_xz_update(crc, b4);
    let crc = crc64_xz_update(crc, b5);
    let crc = crc64_xz_update(crc, b6);
    let crc = crc64_xz_update(crc, b7);
    crc64_xz_update(crc, b8) ^ u64::MAX
}

pub fn crc64_xz(bytes: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for &byte in bytes {
        crc = crc64_xz_update(crc, byte);
    }
    crc ^ u64::MAX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-crc/src/lib.rs"
        ))
        .expect("production remanence-crc lib.rs must be readable");

        let snippets: &[&str] = &[
            "pub const CRC64_XZ_CHECK_VALUE: u64 = 0x995D_C9BB_DF19_39FA;",
            "pub const CRC64_XZ_REFLECTED_POLY: u64 = 0xC96C_5795_D787_0F42;",
            "if crc & 1 == 1 {\n        (crc >> 1) ^ CRC64_XZ_REFLECTED_POLY\n    } else {\n        crc >> 1\n    }",
            "table[byte] = crc64_xz_table_entry(byte as u8);",
            "let index = ((crc ^ u64::from(byte)) & 0xFF) as usize;",
            "(crc >> 8) ^ CRC64_XZ_TABLE[index]",
            "let mut crc = u64::MAX;\n    for &byte in bytes {\n        crc = crc64_xz_update(crc, byte);\n    }\n    crc ^ u64::MAX",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-crc lib.rs -- production changed; \
                 re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "pub fn crc64_xz_bit_step(crc: u64) -> u64",
            "crc = crc64_xz_bit_step(crc);",
            "let index = ((crc ^ u64::from(byte)) & 0xFF) as u8;",
            "(crc >> 8) ^ crc64_xz_table_entry(index)",
            "pub fn crc64_xz(bytes: &[u8]) -> u64",
            "let mut crc = u64::MAX;\n    for &byte in bytes {\n        crc = crc64_xz_update(crc, byte);\n    }\n    crc ^ u64::MAX",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif CRC model"
            );
        }
    }

    #[test]
    fn byte_vectors_match_spec() {
        assert_eq!(crc64_xz_one(0x00), 0x1fad_a173_6467_3f59);
        assert_eq!(crc64_xz_one(0xff), 0xff00_0000_0000_0000);
        assert_eq!(crc64_xz(b""), 0);
        assert_eq!(crc64_xz(b"123456789"), CRC64_XZ_CHECK_VALUE);
        assert_eq!(
            crc64_xz_nine(b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9'),
            crc64_xz(b"123456789")
        );
    }
}
