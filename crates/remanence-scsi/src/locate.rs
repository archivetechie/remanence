//! LOCATE(16) (CDB `0x92`) — SSC-5 §6.6.
//!
//! Seeks the tape to a given Logical Block Address. The 16-byte form
//! is rem's exclusive choice — LTO-9 native capacity (18 TB ≈ 18 M
//! blocks at 1 MiB) exceeds LOCATE(10)'s 32-bit LBA field when
//! addressing small-block formats.
//!
//! The IMMED bit is **not** set: the caller wants the seek to be
//! complete before the next data CDB is issued.
//! Destination Type (DEST_TYPE) is fixed to `0` (logical block
//! address). Layer 3a does not expose physical-block addressing
//! because positions are not portable across drives or read passes.
//! Change-Partition (CP) is fixed to `0` — rem operates in
//! partition 0 only (LTFS uses partition 1 for its index, one of
//! the reasons rem does not use LTFS).
//!
//! CDB byte 1 layout per SSC-5:
//! - bits 7-6: reserved
//! - bits 5-3: DEST_TYPE  (0 = logical block address)
//! - bit 2:    CP         (0 = stay in current partition)
//! - bit 1:    reserved
//! - bit 0:    IMMED      (0 = wait for completion)
//!
//! With every relevant bit zero, byte 1 is `0x00`.

/// SCSI opcode for LOCATE(16).
pub const OPCODE: u8 = 0x92;

/// Build the 16-byte LOCATE(16) CDB. Seeks to the given LBA in
/// partition 0, blocking until the drive reports the seek complete.
pub fn build_cdb(lba: u64) -> [u8; 16] {
    let lba_be = lba.to_be_bytes();
    [
        OPCODE, 0x00, // DEST_TYPE=0, CP=0, IMMED=0
        0x00, // reserved
        0x00, // partition (only used if CP=1; rem always stays in partition 0)
        lba_be[0], lba_be[1], lba_be[2], lba_be[3], lba_be[4], lba_be[5], lba_be[6], lba_be[7],
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
    fn locate_to_bot_is_all_zero_lba() {
        let cdb = build_cdb(0);
        assert_eq!(cdb[0], 0x92);
        assert_eq!(cdb[1], 0x00);
        assert_eq!(&cdb[4..12], &[0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(cdb[15], 0x00);
    }

    #[test]
    fn locate_lba_big_endian_in_bytes_4_through_11() {
        let cdb = build_cdb(0x0123_4567_89AB_CDEF);
        assert_eq!(
            &cdb[4..12],
            &[0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]
        );
    }

    #[test]
    fn locate_max_lba() {
        let cdb = build_cdb(u64::MAX);
        assert_eq!(&cdb[4..12], &[0xFF; 8]);
    }

    #[test]
    fn locate_small_lba_padded_with_high_zeros() {
        // 18 M blocks ≈ LTO-9 native at 1 MiB blocks; well within u32.
        let cdb = build_cdb(18_000_000);
        // High bytes are zero, low 4 bytes carry the value.
        assert_eq!(&cdb[4..8], &[0x00, 0x00, 0x00, 0x00]);
        assert_eq!(&cdb[8..12], &18_000_000_u32.to_be_bytes()[..]);
    }
}
