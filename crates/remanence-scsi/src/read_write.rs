//! READ(6) / WRITE(6) (CDBs `0x08` / `0x0A`) — SSC-5 §6.5, §6.19.
//!
//! Block-level data transfer to/from tape. Six-byte CDBs with a
//! 24-bit transfer length, which covers any block size LTO drives
//! support (LTO-9 caps at 16 MiB ≈ 2^24, which is the SSC max).
//!
//! Layer 3a does NOT expose READ(16) / WRITE(16). T10 marks them
//! *optional* for sequential-access devices (the opcodes `0x88` /
//! `0x8A` are best-known from SBC for disks), but HPE LTO drives
//! do not implement them — only the 6-byte forms — and rem does not
//! need them, since READ(6) / WRITE(6)'s 24-bit transfer length
//! covers any LTO block size (max 16 MiB). The earlier
//! `docs/layer3a-design.md` SCSI table mentioned 0x88 / 0x8A as a
//! design option; that row is now a strikethrough explaining the
//! drop with this rationale.
//!
//! Mode bits in byte 1:
//! - **FIXED** (bit 0): 0 = variable-block mode (transfer length =
//!   bytes), 1 = fixed-block mode (transfer length = N times the
//!   configured block size). rem-chunked-v1 uses variable.
//! - **SILI** (bit 1, READ only — "Suppress Incorrect Length
//!   Indicator"): 0 = the drive sets ILI in sense data when the
//!   on-tape block size differs from the host buffer. rem's READ
//!   path WANTS this signal so it can detect short reads and
//!   recover position. SILI=0 always.

/// SCSI opcode for READ(6).
pub const OPCODE_READ_6: u8 = 0x08;

/// SCSI opcode for WRITE(6).
pub const OPCODE_WRITE_6: u8 = 0x0A;

/// Maximum 24-bit transfer length the CDB can carry.
pub const MAX_TRANSFER_LEN: u32 = 0x00FF_FFFF;

/// Build the 6-byte READ(6) CDB in variable-block mode.
///
/// `len_bytes` is the host-buffer size — the drive will return one
/// block of up to that many bytes and signal ILI if the on-tape
/// block was a different size. Must fit in 24 bits.
pub fn build_read_variable_cdb(len_bytes: u32) -> [u8; 6] {
    assert!(
        len_bytes <= MAX_TRANSFER_LEN,
        "READ(6) length {len_bytes} exceeds 24-bit max"
    );
    let len_bytes_be = len_bytes.to_be_bytes();
    [
        OPCODE_READ_6,
        0x00, // SILI=0, FIXED=0 — variable-block, surface ILI on short reads
        len_bytes_be[1],
        len_bytes_be[2],
        len_bytes_be[3],
        0x00, // control
    ]
}

/// Build the 6-byte READ(6) CDB in fixed-block mode.
///
/// `count` is the number of fixed-size blocks to read (block size
/// set previously via MODE SELECT). Must fit in 24 bits.
pub fn build_read_fixed_cdb(count: u32) -> [u8; 6] {
    assert!(
        count <= MAX_TRANSFER_LEN,
        "READ(6) fixed-block count {count} exceeds 24-bit max"
    );
    let count_be = count.to_be_bytes();
    [
        OPCODE_READ_6,
        0x01, // SILI=0, FIXED=1
        count_be[1],
        count_be[2],
        count_be[3],
        0x00, // control
    ]
}

/// Build the 6-byte WRITE(6) CDB in variable-block mode.
///
/// `len_bytes` is the host-buffer size — the drive writes exactly
/// that many bytes as one block. Must fit in 24 bits.
pub fn build_write_variable_cdb(len_bytes: u32) -> [u8; 6] {
    assert!(
        len_bytes <= MAX_TRANSFER_LEN,
        "WRITE(6) length {len_bytes} exceeds 24-bit max"
    );
    let len_bytes_be = len_bytes.to_be_bytes();
    [
        OPCODE_WRITE_6,
        0x00, // FIXED=0 — variable-block
        len_bytes_be[1],
        len_bytes_be[2],
        len_bytes_be[3],
        0x00, // control
    ]
}

/// Build the 6-byte WRITE(6) CDB in fixed-block mode.
///
/// `count` is the number of fixed-size blocks to write. Must fit
/// in 24 bits.
pub fn build_write_fixed_cdb(count: u32) -> [u8; 6] {
    assert!(
        count <= MAX_TRANSFER_LEN,
        "WRITE(6) fixed-block count {count} exceeds 24-bit max"
    );
    let count_be = count.to_be_bytes();
    [
        OPCODE_WRITE_6,
        0x01, // FIXED=1
        count_be[1],
        count_be[2],
        count_be[3],
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_variable_short_read() {
        let cdb = build_read_variable_cdb(1024);
        assert_eq!(cdb[0], 0x08);
        assert_eq!(cdb[1], 0x00);
        assert_eq!(&cdb[2..5], &[0x00, 0x04, 0x00]); // 1024 = 0x000400
    }

    #[test]
    fn read_variable_max_length() {
        let cdb = build_read_variable_cdb(MAX_TRANSFER_LEN);
        assert_eq!(&cdb[2..5], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn read_fixed_sets_byte_1_bit_0() {
        let cdb = build_read_fixed_cdb(5);
        assert_eq!(cdb[1], 0x01);
        assert_eq!(&cdb[2..5], &[0x00, 0x00, 0x05]);
    }

    #[test]
    fn write_variable_one_mib() {
        let cdb = build_write_variable_cdb(1_048_576);
        assert_eq!(cdb[0], 0x0A);
        assert_eq!(cdb[1], 0x00);
        assert_eq!(&cdb[2..5], &[0x10, 0x00, 0x00]); // 1 MiB = 0x100000
    }

    #[test]
    fn write_fixed_sets_byte_1_bit_0() {
        let cdb = build_write_fixed_cdb(7);
        assert_eq!(cdb[0], 0x0A);
        assert_eq!(cdb[1], 0x01);
        assert_eq!(&cdb[2..5], &[0x00, 0x00, 0x07]);
    }

    #[test]
    fn max_transfer_len_is_24_bit_max() {
        assert_eq!(MAX_TRANSFER_LEN, 0x00FF_FFFF);
        assert_eq!(MAX_TRANSFER_LEN, (1u32 << 24) - 1);
    }

    #[test]
    #[should_panic(expected = "READ(6) length")]
    fn read_variable_rejects_over_24_bit_length() {
        let _ = build_read_variable_cdb(MAX_TRANSFER_LEN + 1);
    }

    #[test]
    #[should_panic(expected = "READ(6) fixed-block count")]
    fn read_fixed_rejects_over_24_bit_count() {
        let _ = build_read_fixed_cdb(MAX_TRANSFER_LEN + 1);
    }

    #[test]
    #[should_panic(expected = "WRITE(6) length")]
    fn write_variable_rejects_over_24_bit_length() {
        let _ = build_write_variable_cdb(MAX_TRANSFER_LEN + 1);
    }

    #[test]
    #[should_panic(expected = "WRITE(6) fixed-block count")]
    fn write_fixed_rejects_over_24_bit_count() {
        let _ = build_write_fixed_cdb(MAX_TRANSFER_LEN + 1);
    }
}
