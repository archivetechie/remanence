//! WRITE FILEMARKS(6) (CDB `0x10`) — SSC-5 §6.16.
//!
//! Writes N file marks to tape at the current position. File marks
//! are the coarse navigational separators between successive object
//! archives on a rem tape (per the spec v0.3 §5.1 layout). Each
//! file mark consumes one block of tape space and bumps a counter
//! visible via READ POSITION's long-form `file_number`. A zero count
//! is legal; with IMMED=0 IBM LTO treats it as a synchronization of
//! buffered data and filemarks without writing a new filemark.
//!
//! Two flag bits in CDB byte 1:
//! - **WSMK** (Write SetMark): write setmarks instead of file marks.
//!   rem does not use setmarks — they are a niche SSC feature; LTO
//!   firmware varies in how it handles them. WSMK=0 always.
//! - **IMMED**: return as soon as the command starts rather than
//!   waiting for media commit. Per-object commits and zero-count barriers use
//!   IMMED=0. Opt-in parity-off checkpoint batches use IMMED=1 only for
//!   provisional object delimiters; a later synchronous barrier establishes
//!   durability and owns any deferred error.
//!
//! WRITE FILEMARKS(6) carries a 24-bit unsigned count
//! (max 16,777,215 file marks). No tape rem writes will get close
//! to this — typical use is 1-2 marks per object plus end-of-tape
//! marks; tens of thousands of total marks on a maxed-out tape at
//! the smallest sensible object size.
//!
//! **Why no WRITE FILEMARKS(16) here.** SSC's 16-byte WRITE
//! FILEMARKS variant (opcode 0x80) is *not* a 64-bit-count form —
//! it's an explicit-LBA-addressed form with partition + logical
//! object identifier + a 24-bit transfer-length field, intended
//! for FCS / LCS (First/Last-cycle stage) staged-write workflows.
//! An earlier draft of this module incorrectly treated 0x80 as a
//! 64-bit count CDB (a SPACE(16)-style layout); codex review
//! cb91b17b caught it. Since rem doesn't need to address file
//! marks by logical-object identifier, the 16-byte form is
//! dropped entirely.

/// SCSI opcode for WRITE FILEMARKS(6).
pub const OPCODE_WRITE_FILEMARKS_6: u8 = 0x10;

/// Maximum count expressible in a WRITE FILEMARKS(6) CDB
/// (24-bit unsigned).
pub const WRITE_FILEMARKS_6_MAX: u32 = 0x00FF_FFFF;

/// Build the 6-byte WRITE FILEMARKS(6) CDB. `count` must fit in
/// 24 bits unsigned.
pub fn build_cdb_6(count: u32) -> [u8; 6] {
    build_cdb_6_with_immed(count, false)
}

/// Build WRITE FILEMARKS(6) with IMMED set for non-durable delimiters.
pub fn build_cdb_6_immediate(count: u32) -> [u8; 6] {
    build_cdb_6_with_immed(count, true)
}

fn build_cdb_6_with_immed(count: u32, immediate: bool) -> [u8; 6] {
    assert!(
        count <= WRITE_FILEMARKS_6_MAX,
        "WRITE FILEMARKS(6) count {count} exceeds 24-bit max"
    );
    let count_be = count.to_be_bytes();
    [
        OPCODE_WRITE_FILEMARKS_6,
        u8::from(immediate), // WSMK=0, IMMED as requested
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
    fn wf6_single_filemark() {
        let cdb = build_cdb_6(1);
        assert_eq!(cdb, [0x10, 0x00, 0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn wf6_zero_count_is_legal_per_ssc() {
        // SSC allows count=0 (synchronise without writing a mark).
        let cdb = build_cdb_6(0);
        assert_eq!(cdb, [0x10, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn wf6_immediate_sets_only_immed_bit() {
        assert_eq!(
            build_cdb_6_immediate(1),
            [0x10, 0x01, 0x00, 0x00, 0x01, 0x00]
        );
    }

    #[test]
    fn wf6_max_count() {
        let cdb = build_cdb_6(WRITE_FILEMARKS_6_MAX);
        assert_eq!(&cdb[2..5], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    #[should_panic(expected = "WRITE FILEMARKS(6) count")]
    fn wf6_rejects_over_24_bit_count() {
        let _ = build_cdb_6(WRITE_FILEMARKS_6_MAX + 1);
    }
}
