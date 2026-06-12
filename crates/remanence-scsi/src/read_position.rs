//! READ POSITION (CDB `0x34`) — SSC-5 §6.7.
//!
//! Returns the drive's current tape position. SSC defines several
//! service actions; the ones rem cares about:
//!
//! - 0 — short form (returns 20 bytes; 32-bit block address).
//! - 1 — short-extended form (returns 32 bytes with file-number +
//!   set-number; uses 32-bit block address). Layer 3a does not use
//!   this variant.
//! - 6 — long form (returns 32 bytes with 64-bit LBA + file/set
//!   numbers + BPEW + BOP/EOP flags). **This is what Layer 3a
//!   uses.**
//! - 8 — extended form (returns up to 64 bytes; uses allocation
//!   length field).
//!
//! Layer 3a uses **service action 6** so the full 64-bit LBA fits,
//! plus BPEW (block-position end-of-warning) and file-number
//! metadata, without a second CDB. The drive returns a fixed-size
//! 32-byte response; the allocation-length field (bytes 7-8)
//! **must be 0** for service actions 0, 1, and 6 per SSC and the
//! IBM LTO SCSI
//! Reference — non-zero is treated as INVALID FIELD IN CDB. Only
//! the extended form (service action 8) uses allocation length.

/// SCSI opcode for READ POSITION.
pub const OPCODE: u8 = 0x34;

/// Service action: long form (32-byte response).
pub const SERVICE_ACTION_LONG: u8 = 0x06;

/// Service action: short form (20-byte response). Provided for
/// completeness; Layer 3a does not use it.
pub const SERVICE_ACTION_SHORT: u8 = 0x00;

/// Long-form response length in bytes. Fixed by the standard; the
/// caller's host buffer must be at least this large.
pub const LONG_FORM_RESPONSE_LEN: u16 = 32;

/// Build a READ POSITION long-form CDB.
///
/// Uses service action 6. Allocation length is required to be 0
/// for this service action — the drive returns a fixed 32 bytes
/// regardless. Setting alloc length non-zero is INVALID FIELD IN
/// CDB on real LTO drives (codex review cb91b17b caught an
/// earlier version of this builder that incorrectly set it to
/// `0x0020`).
pub fn build_cdb_long() -> [u8; 10] {
    [
        OPCODE,
        SERVICE_ACTION_LONG & 0x1F,
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // alloc length MSB — must be 0 for service action 6
        0x00, // alloc length LSB — must be 0 for service action 6
        0x00, // control
    ]
}

/// Build a READ POSITION short-form CDB. Provided for completeness;
/// Layer 3a always uses [`build_cdb_long`]. Allocation length is
/// also zero for short form (service action 0 returns a fixed 20
/// bytes).
pub fn build_cdb_short() -> [u8; 10] {
    [
        OPCODE,
        SERVICE_ACTION_SHORT & 0x1F,
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // alloc length not used by short form
        0x00,
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_form_sets_service_action_6() {
        let cdb = build_cdb_long();
        assert_eq!(cdb[0], 0x34);
        assert_eq!(cdb[1] & 0x1F, 0x06);
    }

    #[test]
    fn long_form_alloc_length_is_zero() {
        // SSC requires alloc length = 0 for service action 6. Any
        // non-zero value gets INVALID FIELD IN CDB from the drive.
        let cdb = build_cdb_long();
        assert_eq!(&cdb[7..9], &[0x00, 0x00]);
    }

    #[test]
    fn long_form_full_cdb_shape() {
        let cdb = build_cdb_long();
        assert_eq!(
            cdb,
            [0x34, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn short_form_sets_service_action_zero() {
        let cdb = build_cdb_short();
        assert_eq!(cdb[0], 0x34);
        assert_eq!(cdb[1] & 0x1F, 0x00);
    }
}
