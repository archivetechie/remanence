//! SPACE(6) / SPACE(16) (CDBs `0x11` / `0x91`) — SSC-5 §6.10, §6.12.
//!
//! Relative tape positioning by blocks, file marks, sequential
//! file-mark hops, or to End-of-Data. SPACE(6) carries a 24-bit
//! signed count (range ±8,388,607); SPACE(16) carries a 64-bit
//! signed count for full-tape skips at small block sizes.
//!
//! CDB byte 1 layout (both forms) per SSC-5:
//! - bits 7-3: reserved
//! - bits 2-0: CODE (motion type — see [`SpaceCode`])
//!
//! Caller is responsible for choosing the right form. The
//! [`fits_in_space6`] helper exists for Layer 3a's `space()` to make
//! that decision; passing an out-of-range count to [`build_cdb_6`]
//! panics.

/// SCSI opcode for SPACE(6).
pub const OPCODE_SPACE_6: u8 = 0x11;

/// SCSI opcode for SPACE(16).
pub const OPCODE_SPACE_16: u8 = 0x91;

/// Maximum positive count expressible in a SPACE(6) CDB.
/// `2^23 - 1 = 8_388_607`.
pub const SPACE6_MAX: i32 = 0x007F_FFFF;

/// Minimum (most negative) count expressible in a SPACE(6) CDB.
/// `-2^23 = -8_388_608`.
pub const SPACE6_MIN: i32 = -0x0080_0000;

/// Motion-type code for SPACE. Encoded in CDB byte 1 bits 2-0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SpaceCode {
    /// Skip N blocks (positive = forward, negative = backward).
    Blocks = 0,
    /// Skip N file marks.
    Filemarks = 1,
    /// "Sequential file marks": skip to the Nth file mark forward;
    /// stops short on EOD. Backward sense per SSC.
    SequentialFilemarks = 2,
    /// Move to End-of-Data. Count field is ignored by the drive.
    EndOfData = 3,
}

/// True iff `count` fits in SPACE(6)'s 24-bit signed count field.
pub fn fits_in_space6(count: i64) -> bool {
    (SPACE6_MIN as i64..=SPACE6_MAX as i64).contains(&count)
}

/// Build the 6-byte SPACE(6) CDB.
///
/// `count` must fit in 24-bit signed (`[-2^23, 2^23 - 1]`) — caller's
/// responsibility; use [`fits_in_space6`] to check before calling.
pub fn build_cdb_6(code: SpaceCode, count: i32) -> [u8; 6] {
    assert!(
        (SPACE6_MIN..=SPACE6_MAX).contains(&count),
        "SPACE(6) count {count} out of 24-bit signed range; \
         use SPACE(16) instead"
    );
    // Two's-complement 24-bit count goes in bytes 2-4.
    let masked = (count as u32) & 0x00FF_FFFF;
    let count_bytes = masked.to_be_bytes();
    [
        OPCODE_SPACE_6,
        (code as u8) & 0x07,
        count_bytes[1],
        count_bytes[2],
        count_bytes[3],
        0x00, // control
    ]
}

/// Build the 16-byte SPACE(16) CDB. Used when the requested count
/// exceeds [`SPACE6_MAX`] / [`SPACE6_MIN`].
pub fn build_cdb_16(code: SpaceCode, count: i64) -> [u8; 16] {
    let count_bytes = count.to_be_bytes();
    [
        OPCODE_SPACE_16,
        (code as u8) & 0x07,
        0x00, // reserved
        0x00, // reserved
        count_bytes[0],
        count_bytes[1],
        count_bytes[2],
        count_bytes[3],
        count_bytes[4],
        count_bytes[5],
        count_bytes[6],
        count_bytes[7],
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn space6_forward_one_block() {
        let cdb = build_cdb_6(SpaceCode::Blocks, 1);
        assert_eq!(cdb, [0x11, 0x00, 0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn space6_backward_one_block_is_24bit_negative_one() {
        let cdb = build_cdb_6(SpaceCode::Blocks, -1);
        assert_eq!(cdb, [0x11, 0x00, 0xFF, 0xFF, 0xFF, 0x00]);
    }

    #[test]
    fn space6_max_positive() {
        let cdb = build_cdb_6(SpaceCode::Blocks, SPACE6_MAX);
        assert_eq!(&cdb[2..5], &[0x7F, 0xFF, 0xFF]);
    }

    #[test]
    fn space6_min_negative() {
        let cdb = build_cdb_6(SpaceCode::Blocks, SPACE6_MIN);
        assert_eq!(&cdb[2..5], &[0x80, 0x00, 0x00]);
    }

    #[test]
    fn space6_code_filemarks_in_byte_1_low_bits() {
        let cdb = build_cdb_6(SpaceCode::Filemarks, 0);
        assert_eq!(cdb[1], 0x01);
    }

    #[test]
    fn space6_code_end_of_data() {
        let cdb = build_cdb_6(SpaceCode::EndOfData, 0);
        assert_eq!(cdb[1], 0x03);
    }

    #[test]
    fn fits_in_space6_boundaries() {
        assert!(fits_in_space6(SPACE6_MAX as i64));
        assert!(fits_in_space6(SPACE6_MIN as i64));
        assert!(fits_in_space6(0));
        assert!(!fits_in_space6((SPACE6_MAX as i64) + 1));
        assert!(!fits_in_space6((SPACE6_MIN as i64) - 1));
    }

    #[test]
    fn space16_zero_count() {
        let cdb = build_cdb_16(SpaceCode::Blocks, 0);
        assert_eq!(cdb[0], 0x91);
        assert_eq!(cdb[1], 0x00);
        assert_eq!(&cdb[4..12], &[0; 8]);
    }

    #[test]
    fn space16_forward_18m_blocks_lto9_full() {
        // ~LTO-9 native at 1 MiB blocks. Exceeds SPACE(6) range.
        let count: i64 = 18_000_000;
        assert!(!fits_in_space6(count));
        let cdb = build_cdb_16(SpaceCode::Blocks, count);
        assert_eq!(&cdb[4..12], &count.to_be_bytes()[..]);
    }

    #[test]
    fn space16_backward_one_block() {
        let cdb = build_cdb_16(SpaceCode::Blocks, -1);
        assert_eq!(&cdb[4..12], &[0xFF; 8]);
    }

    #[test]
    fn space16_filemarks_code() {
        let cdb = build_cdb_16(SpaceCode::Filemarks, 5);
        assert_eq!(cdb[1], 0x01);
        // Last byte of the 8-byte count is the magnitude.
        assert_eq!(cdb[11], 0x05);
    }

    #[test]
    #[should_panic(expected = "SPACE(6) count")]
    fn space6_rejects_over_positive_range() {
        let _ = build_cdb_6(SpaceCode::Blocks, SPACE6_MAX + 1);
    }

    #[test]
    #[should_panic(expected = "SPACE(6) count")]
    fn space6_rejects_under_negative_range() {
        let _ = build_cdb_6(SpaceCode::Blocks, SPACE6_MIN - 1);
    }
}
