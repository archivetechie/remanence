//! Standard SCSI INQUIRY command (`opcode 0x12`).
//!
//! Builds the 6-byte CDB and parses the response buffer into a strongly
//! typed [`Inquiry`] value. No allocations, no I/O — just bytes in, struct
//! out. The transport (sending the CDB to a `/dev/sgN`) lives in
//! [`crate::sg_io`].

use crate::error::ScsiError;
use core::fmt;

/// Opcode for SCSI INQUIRY (SPC-5 §6.6).
pub const OPCODE: u8 = 0x12;

/// Default allocation length we ask the target for. Standard INQUIRY data is
/// at most 96 bytes per SPC-5 §6.6.2; 96 fits comfortably.
pub const ALLOC_LEN: u16 = 96;

/// Build a 6-byte CDB for a *standard* INQUIRY (EVPD=0, page code=0).
///
/// Caller supplies `alloc_len`, normally [`ALLOC_LEN`] (96). The CDB layout
/// is SPC-5 Table 142.
pub fn build_cdb(alloc_len: u16) -> [u8; 6] {
    [
        OPCODE,
        0x00,                   // EVPD=0, page code N/A
        0x00,                   // page code = 0 (standard INQUIRY)
        (alloc_len >> 8) as u8, // allocation length (big-endian u16)
        (alloc_len & 0xff) as u8,
        0x00, // control byte
    ]
}

/// Build a 6-byte CDB for a *VPD* INQUIRY (EVPD=1) for a specific page code.
///
/// Same CDB shape as [`build_cdb`], but with the EVPD bit set and the page
/// code in byte 2. Use e.g. `build_cdb_vpd(crate::vpd::PAGE_UNIT_SERIAL, 252)`.
pub fn build_cdb_vpd(page_code: u8, alloc_len: u16) -> [u8; 6] {
    [
        OPCODE,
        0x01, // EVPD = 1
        page_code,
        (alloc_len >> 8) as u8,
        (alloc_len & 0xff) as u8,
        0x00,
    ]
}

/// Parsed standard INQUIRY response.
///
/// Fields preserve the on-wire byte layout where it's natural (the three
/// ASCII fields stay as raw byte arrays so callers can decide whether to
/// trim trailing spaces). The byte-level fields are decoded to typed Rust
/// equivalents where useful.
#[derive(Clone, PartialEq, Eq)]
pub struct Inquiry {
    /// Peripheral device type, low 5 bits of byte 0. See [`DeviceType`].
    pub device_type: DeviceType,
    /// Peripheral qualifier, high 3 bits of byte 0.
    /// 0b000 = device is connected here; 0b001/0b011 = various "not
    /// here / unknown" states.
    pub peripheral_qualifier: u8,
    /// True if the medium is removable (RMB bit, byte 1 bit 7). True for
    /// every tape drive and changer; false for fixed disks.
    pub removable: bool,
    /// SPC version the device claims (byte 2). 0x07 = SPC-5, 0x06 = SPC-4.
    pub version: u8,
    /// Response data format (byte 3, low 4 bits). Should be 2 for any
    /// device that follows SPC-3 or later — which is everything since 2005.
    pub response_data_format: u8,
    /// Additional length (byte 4). Number of bytes beyond byte 4.
    pub additional_length: u8,
    /// Vendor identification, 8 ASCII bytes, space-padded right (bytes 8–15).
    pub vendor: [u8; 8],
    /// Product identification, 16 ASCII bytes, space-padded right (bytes 16–31).
    pub product: [u8; 16],
    /// Product revision level, 4 ASCII bytes (bytes 32–35).
    pub revision: [u8; 4],
}

impl Inquiry {
    /// Parse a standard INQUIRY response from a byte slice.
    ///
    /// `buf` must be at least 36 bytes — the minimum span that contains the
    /// vendor, product, and revision strings (SPC-5 Table 144).
    pub fn parse(buf: &[u8]) -> Result<Self, ScsiError> {
        if buf.len() < 36 {
            return Err(ScsiError::Truncated {
                got: buf.len(),
                need: 36,
            });
        }

        let dt_raw = buf[0] & 0x1f;
        let pq = (buf[0] >> 5) & 0x07;
        let rdf = buf[3] & 0x0f;

        // SPC-3 onwards must report 2 here. Anything else points at a
        // pre-SPC-3 device we don't support, or a transport error.
        if rdf != 2 {
            return Err(ScsiError::InvalidResponse {
                offset: 3,
                detail: "response data format must be 2 (SPC-3+)",
            });
        }

        // additional_length is the count of valid bytes beyond byte 4. To
        // trust the vendor (bytes 8-15), product (16-31), and revision
        // (32-35) fields, the device must say it sent at least through
        // byte 35 — i.e. additional_length >= 31. Otherwise we'd be
        // reading uninitialised padding the target told us was not valid.
        let additional_length = buf[4];
        if (additional_length as usize) < 31 {
            return Err(ScsiError::InvalidResponse {
                offset: 4,
                detail: "additional_length < 31 — vendor/product/revision fields are not valid",
            });
        }

        let mut vendor = [0u8; 8];
        let mut product = [0u8; 16];
        let mut revision = [0u8; 4];
        vendor.copy_from_slice(&buf[8..16]);
        product.copy_from_slice(&buf[16..32]);
        revision.copy_from_slice(&buf[32..36]);

        Ok(Self {
            device_type: DeviceType::from_code(dt_raw),
            peripheral_qualifier: pq,
            removable: (buf[1] & 0x80) != 0,
            version: buf[2],
            response_data_format: rdf,
            additional_length,
            vendor,
            product,
            revision,
        })
    }

    /// Vendor as a UTF-8 string with trailing spaces removed.
    pub fn vendor_str(&self) -> &str {
        trim_ascii_right(&self.vendor)
    }
    /// Product as a UTF-8 string with trailing spaces removed.
    pub fn product_str(&self) -> &str {
        trim_ascii_right(&self.product)
    }
    /// Revision as a UTF-8 string with trailing spaces removed.
    pub fn revision_str(&self) -> &str {
        trim_ascii_right(&self.revision)
    }
}

impl fmt::Debug for Inquiry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inquiry")
            .field("device_type", &self.device_type)
            .field("removable", &self.removable)
            .field("version", &format_args!("0x{:02x}", self.version))
            .field("vendor", &self.vendor_str())
            .field("product", &self.product_str())
            .field("revision", &self.revision_str())
            .finish()
    }
}

/// SCSI peripheral device type (SPC-5 Table 145). We name the variants we
/// actually care about for Remanence; everything else gets [`Other`].
///
/// [`Other`]: DeviceType::Other
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum DeviceType {
    /// Direct-access block device (disk). Code 0x00.
    DirectAccess,
    /// Sequential-access device (tape). Code 0x01.
    SequentialAccess,
    /// Medium changer (tape library robot). Code 0x08.
    MediumChanger,
    /// 0x1F — no logical unit is present at this slot.
    Unknown,
    /// Anything else we don't have a friendly name for yet.
    Other(u8),
}

impl DeviceType {
    /// Map a raw 5-bit peripheral-device-type code to its [`DeviceType`].
    pub fn from_code(code: u8) -> Self {
        match code & 0x1f {
            0x00 => DeviceType::DirectAccess,
            0x01 => DeviceType::SequentialAccess,
            0x08 => DeviceType::MediumChanger,
            0x1f => DeviceType::Unknown,
            other => DeviceType::Other(other),
        }
    }
}

/// Trim trailing 0x20 (space) and 0x00 bytes from an ASCII field and return
/// a `&str`. Non-UTF-8 input produces a degenerate empty string rather than
/// panicking — INQUIRY fields are spec'd to be ASCII so this should never
/// happen on real hardware, but we don't want a malicious target to crash us.
fn trim_ascii_right(buf: &[u8]) -> &str {
    let end = buf
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    core::str::from_utf8(&buf[..end]).unwrap_or("")
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    // QuadStor virtual library (akash) — what's in the dev environment.
    const DRIVE_LTO9: &[u8] = include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin");
    const CHANGER_MSL: &[u8] = include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin");
    // Real HP MSL3040 + real Ultrium 7/9 drives, captured from datamover.
    const REAL_CHANGER: &[u8] =
        include_bytes!("../../../fixtures/inquiry/real/changer-msl3040.bin");
    const REAL_DRIVE_LTO9: &[u8] = include_bytes!("../../../fixtures/inquiry/real/drive-lto9.bin");
    const REAL_DRIVE_LTO7: &[u8] = include_bytes!("../../../fixtures/inquiry/real/drive-lto7.bin");

    #[test]
    fn build_cdb_has_correct_shape() {
        let cdb = build_cdb(ALLOC_LEN);
        assert_eq!(cdb[0], 0x12, "opcode is INQUIRY");
        assert_eq!(cdb[1] & 0x01, 0, "EVPD must be clear for standard INQUIRY");
        assert_eq!(cdb[2], 0x00, "page code is 0 for standard INQUIRY");
        assert_eq!(u16::from_be_bytes([cdb[3], cdb[4]]), ALLOC_LEN);
        assert_eq!(cdb[5], 0x00, "control byte is 0");
    }

    #[test]
    fn parses_lto9_drive_fixture() {
        let i = Inquiry::parse(DRIVE_LTO9).expect("parse drive");
        assert_eq!(i.device_type, DeviceType::SequentialAccess);
        assert!(i.removable, "tape drives are always removable-media");
        assert_eq!(i.vendor_str(), "HPE");
        assert_eq!(i.product_str(), "Ultrium 9-SCSI");
        assert_eq!(i.revision_str(), "HH90");
    }

    #[test]
    fn parses_msl_changer_fixture() {
        let i = Inquiry::parse(CHANGER_MSL).expect("parse changer");
        assert_eq!(i.device_type, DeviceType::MediumChanger);
        assert!(i.removable);
        assert_eq!(i.vendor_str(), "HP");
        assert_eq!(i.product_str(), "MSL G3 Series");
        assert_eq!(i.revision_str(), "D.00");
    }

    #[test]
    fn rejects_truncated_buffer() {
        let short = &DRIVE_LTO9[..20];
        match Inquiry::parse(short) {
            Err(ScsiError::Truncated { got: 20, need: 36 }) => (),
            other => panic!("expected Truncated, got {:?}", other),
        }
    }

    #[test]
    fn parses_real_msl3040_changer() {
        let i = Inquiry::parse(REAL_CHANGER).expect("parse real MSL3040");
        assert_eq!(i.device_type, DeviceType::MediumChanger);
        assert!(i.removable);
        assert_eq!(i.vendor_str(), "HPE");
        assert_eq!(i.product_str(), "MSL3040");
        assert_eq!(i.revision_str(), "3350");
    }

    #[test]
    fn parses_real_lto9_drive() {
        let i = Inquiry::parse(REAL_DRIVE_LTO9).expect("parse real LTO-9");
        assert_eq!(i.device_type, DeviceType::SequentialAccess);
        assert!(i.removable);
        assert_eq!(i.vendor_str(), "HPE");
        assert_eq!(i.product_str(), "Ultrium 9-SCSI");
        // S2S1 / R3G3 etc. — firmware varies across drives; just sanity-check length
        assert_eq!(i.revision_str().len(), 4);
    }

    #[test]
    fn parses_real_lto7_drive() {
        let i = Inquiry::parse(REAL_DRIVE_LTO7).expect("parse real LTO-7");
        assert_eq!(i.device_type, DeviceType::SequentialAccess);
        assert!(i.removable);
        // Note: HP rebranded as HPE around LTO-8; LTO-7 drives still report "HP".
        assert_eq!(i.vendor_str(), "HP");
        assert_eq!(i.product_str(), "Ultrium 7-SCSI");
    }

    #[test]
    fn rejects_wrong_response_data_format() {
        // Forge a response whose byte 3 (response data format) is not 2.
        let mut bad = DRIVE_LTO9.to_vec();
        bad[3] = (bad[3] & 0xf0) | 0x01;
        match Inquiry::parse(&bad) {
            Err(ScsiError::InvalidResponse { offset: 3, .. }) => (),
            other => panic!("expected InvalidResponse@3, got {:?}", other),
        }
    }

    #[test]
    fn rejects_too_short_additional_length() {
        // additional_length lies and says only 20 valid bytes follow byte 4
        // (i.e., bytes 5..25). Vendor/product/revision live at bytes 8..35
        // — we must not trust them when the target says they aren't valid.
        let mut bad = DRIVE_LTO9.to_vec();
        bad[4] = 20;
        match Inquiry::parse(&bad) {
            Err(ScsiError::InvalidResponse { offset: 4, .. }) => (),
            other => panic!("expected InvalidResponse@4, got {:?}", other),
        }
    }
}
