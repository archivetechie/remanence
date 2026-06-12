//! MODE SELECT(6) / MODE SENSE(6) (CDBs `0x15` / `0x1A`) — SPC-5
//! §6.21, §6.22.
//!
//! Configuration CDBs used at write-session open to set block size +
//! compression, and queried at session open to confirm. Mode pages
//! that Layer 3a / rem-chunked-v1 cares about:
//!
//! - **0x0F — Data Compression Mode Page.** Toggles the drive's
//!   hardware compression (DCE bit). rem defaults to `compression =
//!   false` because format-level zstd already compresses the
//!   payload (see `docs/pfr-reference.md` §6.3).
//! - **0x10 — Device Configuration Mode Page.** Block-size sense
//!   live in the block descriptor (not the page itself), but the
//!   page exposes other knobs (active partition, BIS, etc.) that
//!   Layer 3a touches indirectly.
//!
//! Block size is controlled by the **block descriptor** in the mode
//! parameter list — a **3-byte** BLOCK LENGTH at descriptor bytes
//! 5..8 (bytes 5, 6, 7), big-endian, per IBM LTO SCSI Reference
//! Tables 332/337. Block length 0 → variable-block mode; non-zero
//! → fixed-block of that size. Codex 20:22 (idref=01cf3e76 Low)
//! caught the earlier "4-byte" wording.

/// SCSI opcode for MODE SELECT(6).
pub const OPCODE_MODE_SELECT_6: u8 = 0x15;

/// SCSI opcode for MODE SENSE(6).
pub const OPCODE_MODE_SENSE_6: u8 = 0x1A;

/// Mode page code for Data Compression. Page contains the DCE bit
/// that toggles hardware compression.
pub const PAGE_DATA_COMPRESSION: u8 = 0x0F;

/// Mode page code for Device Configuration. Companion to 0x0F for
/// LTO tape drives.
pub const PAGE_DEVICE_CONFIGURATION: u8 = 0x10;

/// Special page code: return all supported pages.
pub const PAGE_ALL: u8 = 0x3F;

/// Page-Control field (CDB byte 2 bits 7-6):
/// `00 = current`, `01 = changeable mask`, `10 = default`,
/// `11 = saved`. Layer 3a queries with `00 = current` at session
/// open, then issues MODE SELECT with the desired values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageControl {
    /// Return the values currently in effect on the drive.
    Current = 0b00,
    /// Return a bit-mask of which fields the drive accepts MODE
    /// SELECT changes to. Useful for capability probing.
    Changeable = 0b01,
    /// Return the manufacturer-default values for the page.
    Default = 0b10,
    /// Return the last-saved (non-volatile) values for the page.
    Saved = 0b11,
}

/// Build the 6-byte MODE SENSE(6) CDB.
///
/// - `page_control`: which view of the page values to read.
/// - `page_code`: which mode page (e.g.
///   [`PAGE_DATA_COMPRESSION`]). Use [`PAGE_ALL`] for the
///   all-pages view.
/// - `alloc_length`: host buffer size; drive returns at most this
///   many bytes. The mode-page header is 4 bytes; rem typically
///   passes 64-255 to cover header + page + block descriptor.
pub fn build_mode_sense6_cdb(
    page_control: PageControl,
    page_code: u8,
    alloc_length: u8,
) -> [u8; 6] {
    let pc = (page_control as u8) << 6;
    let pc_page = pc | (page_code & 0x3F);
    [
        OPCODE_MODE_SENSE_6,
        0x00, // DBD=0 — include block descriptor in response
        pc_page,
        0x00, // subpage = 0
        alloc_length,
        0x00, // control
    ]
}

/// Build the 6-byte MODE SELECT(6) CDB.
///
/// The actual mode-page payload is passed as the CDB's data-out
/// buffer; this builder just produces the CDB header.
///
/// - `pf`: Page Format bit (CDB byte 1 bit 4). Always set to 1 for
///   anything written this century — `0` is the SCSI-1 legacy
///   format which LTO drives do not accept.
/// - `save_pages`: SP bit (byte 1 bit 0). `true` writes the page to
///   non-volatile drive memory so it persists across power cycles;
///   `false` is volatile-only. rem's write sessions write
///   volatile-only because the format crate sets its own values at
///   session open.
/// - `param_length`: length of the data-out parameter list (the
///   page payload the caller will provide separately).
pub fn build_mode_select6_cdb(pf: bool, save_pages: bool, param_length: u8) -> [u8; 6] {
    let pf_bit = if pf { 0x10 } else { 0x00 };
    let sp_bit = if save_pages { 0x01 } else { 0x00 };
    [
        OPCODE_MODE_SELECT_6,
        pf_bit | sp_bit,
        0x00, // reserved
        0x00, // reserved
        param_length,
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sense_current_data_compression_page() {
        let cdb = build_mode_sense6_cdb(PageControl::Current, PAGE_DATA_COMPRESSION, 64);
        assert_eq!(cdb, [0x1A, 0x00, 0x0F, 0x00, 0x40, 0x00]);
    }

    #[test]
    fn sense_changeable_mask_sets_pc_bits_to_01() {
        let cdb = build_mode_sense6_cdb(PageControl::Changeable, PAGE_DEVICE_CONFIGURATION, 64);
        assert_eq!(cdb[2], 0b0100_0000 | PAGE_DEVICE_CONFIGURATION);
    }

    #[test]
    fn sense_default_page_sets_pc_bits_to_10() {
        let cdb = build_mode_sense6_cdb(PageControl::Default, PAGE_DATA_COMPRESSION, 64);
        assert_eq!(cdb[2], 0b1000_0000 | PAGE_DATA_COMPRESSION);
    }

    #[test]
    fn sense_all_pages() {
        let cdb = build_mode_sense6_cdb(PageControl::Current, PAGE_ALL, 255);
        assert_eq!(cdb[2], PAGE_ALL);
        assert_eq!(cdb[4], 255);
    }

    #[test]
    fn select_pf1_sp0_default() {
        let cdb = build_mode_select6_cdb(true, false, 32);
        assert_eq!(cdb, [0x15, 0x10, 0x00, 0x00, 0x20, 0x00]);
    }

    #[test]
    fn select_pf1_sp1_persisted() {
        let cdb = build_mode_select6_cdb(true, true, 32);
        assert_eq!(cdb[1], 0x11);
    }

    #[test]
    fn select_pf0_legacy_format() {
        // Provided for completeness — LTO drives reject this in
        // practice, but the builder doesn't second-guess.
        let cdb = build_mode_select6_cdb(false, false, 16);
        assert_eq!(cdb[1] & 0x10, 0x00);
    }
}
