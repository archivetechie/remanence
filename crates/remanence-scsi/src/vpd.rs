//! Vital Product Data (VPD) — INQUIRY responses returned when the EVPD bit
//! is set in the CDB. Each VPD page shares the same 4-byte header (SPC-5
//! §7.7.2) followed by a page-specific payload.
//!
//! Pages currently supported:
//!   - 0x80 — Unit Serial Number ([`UnitSerial`])
//!   - 0x83 — Device Identification ([`DeviceIdentification`])
//!
//! More pages (0xB0 *Block Limits*, 0xC0 / 0xCC / 0xD0 vendor-specific, …)
//! will land here as Layer 1 needs them. Build the matching CDB with
//! [`crate::inquiry::build_cdb_vpd`].

use crate::error::ScsiError;
use crate::inquiry::DeviceType;

/// Allocation length we ask for when fetching a VPD page. SPC-5 caps the
/// payload at 252 bytes for fixed-format pages and the 4-byte header brings
/// the total to 256 — which fits nicely in a single transfer.
pub const ALLOC_LEN: u16 = 256;

/// VPD page code 0x80 — Unit Serial Number (SPC-5 §7.7.17).
pub const PAGE_UNIT_SERIAL: u8 = 0x80;

/// VPD page code 0x83 — Device Identification (SPC-5 §7.7.7).
pub const PAGE_DEVICE_ID: u8 = 0x83;

/// 4-byte header shared by every VPD page response (SPC-5 §7.7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VpdHeader {
    /// Peripheral device type (low 5 bits of byte 0). Useful for double-
    /// checking that the target we queried is the kind we expected.
    pub device_type: DeviceType,
    /// Peripheral qualifier (high 3 bits of byte 0).
    pub peripheral_qualifier: u8,
    /// Page code (byte 1) echoed back from the CDB.
    pub page_code: u8,
    /// Length of the page payload that follows the header, in bytes
    /// (big-endian u16, bytes 2–3). Does **not** include the header itself.
    pub page_length: u16,
}

impl VpdHeader {
    /// Parse the header and return it alongside a slice of the payload.
    ///
    /// `buf` must contain at least 4 bytes (the header) plus `page_length`
    /// bytes of payload. If `expected_page` is provided, the page code in
    /// the response is checked against it and a mismatch yields
    /// [`ScsiError::InvalidResponse`].
    pub fn parse_with_payload(
        buf: &[u8],
        expected_page: Option<u8>,
    ) -> Result<(Self, &[u8]), ScsiError> {
        if buf.len() < 4 {
            return Err(ScsiError::Truncated {
                got: buf.len(),
                need: 4,
            });
        }
        let page_code = buf[1];
        if let Some(want) = expected_page {
            if page_code != want {
                return Err(ScsiError::InvalidResponse {
                    offset: 1,
                    detail: "VPD page code does not match what we requested",
                });
            }
        }
        let page_length = u16::from_be_bytes([buf[2], buf[3]]);
        let need = 4 + page_length as usize;
        if buf.len() < need {
            return Err(ScsiError::Truncated {
                got: buf.len(),
                need,
            });
        }
        let header = VpdHeader {
            device_type: DeviceType::from_code(buf[0] & 0x1f),
            peripheral_qualifier: (buf[0] >> 5) & 0x07,
            page_code,
            page_length,
        };
        let payload = &buf[4..need];
        Ok((header, payload))
    }
}

/// Parsed Unit Serial Number page (VPD page 0x80).
///
/// Borrows from the original response buffer so we don't allocate. Call
/// [`UnitSerial::as_str`] to get a trimmed UTF-8 view, or
/// [`UnitSerial::as_bytes`] for the raw payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitSerial<'a> {
    /// The 4-byte VPD header that prefixed this serial number.
    pub header: VpdHeader,
    serial: &'a [u8],
}

impl<'a> UnitSerial<'a> {
    /// Parse a VPD page 0x80 response from `buf`.
    pub fn parse(buf: &'a [u8]) -> Result<Self, ScsiError> {
        let (header, payload) = VpdHeader::parse_with_payload(buf, Some(PAGE_UNIT_SERIAL))?;
        if payload.is_empty() {
            return Err(ScsiError::InvalidResponse {
                offset: 2,
                detail: "VPD page 0x80 has zero-length payload",
            });
        }
        let serial = core::str::from_utf8(payload).map_err(|_| ScsiError::InvalidResponse {
            offset: 4,
            detail: "VPD page 0x80 unit serial is not valid UTF-8/ASCII",
        })?;
        if serial
            .trim_matches(|c: char| c == ' ' || c == '\0')
            .is_empty()
        {
            return Err(ScsiError::InvalidResponse {
                offset: 4,
                detail: "VPD page 0x80 unit serial is empty after trimming padding",
            });
        }
        Ok(Self {
            header,
            serial: payload,
        })
    }

    /// Raw serial-number bytes (as they came off the wire — usually ASCII).
    pub fn as_bytes(&self) -> &'a [u8] {
        self.serial
    }

    /// Serial number as a `&str`, with leading and trailing spaces / NULs
    /// trimmed. Parsing rejects non-UTF-8 and empty-after-trim serials, so
    /// callers never receive a synthetic empty identity for malformed input.
    pub fn as_str(&self) -> &str {
        let s = core::str::from_utf8(self.serial)
            .expect("UnitSerial::parse validated the payload as UTF-8");
        s.trim_matches(|c: char| c == ' ' || c == '\0')
    }
}

// =================================================================
//  VPD page 0x83 — Device Identification (SPC-5 §7.7.7)
// =================================================================

/// Code set of a Designator's identifier bytes (SPC-5 Table 460).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum CodeSet {
    /// Identifier is binary; only meaningful with [`DeviceDesignator::as_naa`]
    /// (NAA) or callers willing to read raw bytes.
    Binary,
    /// Identifier is printable ASCII.
    Ascii,
    /// Identifier is UTF-8 — rare in tape devices but spec-allowed.
    Utf8,
    /// Anything we don't recognize, preserved.
    Other(u8),
}

impl CodeSet {
    fn from_byte(b: u8) -> Self {
        match b & 0x0f {
            1 => Self::Binary,
            2 => Self::Ascii,
            3 => Self::Utf8,
            x => Self::Other(x),
        }
    }
}

/// Where in the device the designator applies (SPC-5 Table 462).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Association {
    /// 0b00 — applies to the addressed logical unit.
    LogicalUnit,
    /// 0b01 — applies to the target port we sent the command through.
    TargetPort,
    /// 0b10 — applies to the target device that contains the logical unit.
    TargetDevice,
    /// 0b11 — reserved.
    Reserved,
}

impl Association {
    fn from_byte1(b: u8) -> Self {
        match (b >> 4) & 0x03 {
            0 => Self::LogicalUnit,
            1 => Self::TargetPort,
            2 => Self::TargetDevice,
            _ => Self::Reserved,
        }
    }
}

/// Designator type — what kind of identifier the bytes represent
/// (SPC-5 Table 463). We name the types we care about; everything else
/// is preserved as a raw code via [`DesignatorType::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum DesignatorType {
    /// 0x0 — opaque vendor-specific identifier.
    VendorSpecific,
    /// 0x1 — 8-byte vendor ID + vendor-specific bytes (T10-style).
    T10VendorId,
    /// 0x2 — EUI-64.
    Eui64,
    /// 0x3 — World-Wide Name (NAA). HPE chassis identity lives here.
    Naa,
    /// 0x4 — Relative target port identifier.
    RelativeTargetPort,
    /// 0x5 — Target Port Group identifier.
    TargetPortGroup,
    /// 0x6 — Logical Unit Group identifier.
    LogicalUnitGroup,
    /// 0x7 — MD5 logical-unit identifier.
    Md5LogicalUnitId,
    /// 0x8 — SCSI name string (UTF-8).
    ScsiNameString,
    /// Anything else, preserved.
    Other(u8),
}

impl DesignatorType {
    fn from_byte1(b: u8) -> Self {
        match b & 0x0f {
            0 => Self::VendorSpecific,
            1 => Self::T10VendorId,
            2 => Self::Eui64,
            3 => Self::Naa,
            4 => Self::RelativeTargetPort,
            5 => Self::TargetPortGroup,
            6 => Self::LogicalUnitGroup,
            7 => Self::Md5LogicalUnitId,
            8 => Self::ScsiNameString,
            x => Self::Other(x),
        }
    }
}

/// One designation descriptor from a VPD 0x83 response.
///
/// `raw` holds the on-wire identifier bytes; helpers
/// ([`as_naa`](Self::as_naa), [`as_str`](Self::as_str), [`as_hex`](Self::as_hex))
/// surface the most useful interpretations without committing the caller
/// to one. The opaque-bytes approach matches the SPC spec, which
/// deliberately leaves room for vendor-specific blobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDesignator {
    /// Encoding of the identifier bytes.
    pub code_set: CodeSet,
    /// What the identifier names (logical unit vs target port vs ...).
    pub association: Association,
    /// What kind of identifier this is.
    pub designator_type: DesignatorType,
    /// Raw identifier bytes as the device returned them.
    pub raw: Vec<u8>,
}

impl DeviceDesignator {
    /// If this is an 8-byte NAA designator (binary code set), parse it
    /// as a big-endian `u64`. Returns `None` for any other shape.
    pub fn as_naa(&self) -> Option<u64> {
        if !matches!(self.designator_type, DesignatorType::Naa) {
            return None;
        }
        if !matches!(self.code_set, CodeSet::Binary) {
            return None;
        }
        if self.raw.len() != 8 {
            return None;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.raw);
        Some(u64::from_be_bytes(buf))
    }

    /// If the code set is ASCII or UTF-8 and the bytes decode cleanly,
    /// return a trimmed `&str`. Used for T10 vendor IDs and SCSI name
    /// strings.
    pub fn as_str(&self) -> Option<&str> {
        if !matches!(self.code_set, CodeSet::Ascii | CodeSet::Utf8) {
            return None;
        }
        core::str::from_utf8(&self.raw)
            .ok()
            .map(|s| s.trim_matches(|c: char| c == ' ' || c == '\0'))
    }

    /// Lowercase hex of the raw bytes, no separator. Always available;
    /// used by the CLI for diagnostic display of binary designators.
    pub fn as_hex(&self) -> String {
        let mut s = String::with_capacity(self.raw.len() * 2);
        for b in &self.raw {
            use core::fmt::Write;
            let _ = write!(s, "{:02x}", b);
        }
        s
    }
}

/// Parsed VPD page 0x83 — a list of designation descriptors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentification {
    /// The 4-byte VPD header that prefixed the descriptors.
    pub header: VpdHeader,
    /// All designators, in the order the device returned them.
    pub designators: Vec<DeviceDesignator>,
}

impl DeviceIdentification {
    /// Parse a VPD 0x83 response.
    pub fn parse(buf: &[u8]) -> Result<Self, ScsiError> {
        let (header, payload) = VpdHeader::parse_with_payload(buf, Some(PAGE_DEVICE_ID))?;
        let mut designators = Vec::new();
        let mut i = 0;
        while i < payload.len() {
            // Each descriptor has a 4-byte header followed by an identifier.
            if i + 4 > payload.len() {
                return Err(ScsiError::InvalidResponse {
                    offset: 4 + i,
                    detail: "VPD 0x83 has a partial designator-descriptor header at its tail",
                });
            }
            let code_set = CodeSet::from_byte(payload[i]);
            let association = Association::from_byte1(payload[i + 1]);
            let designator_type = DesignatorType::from_byte1(payload[i + 1]);
            let id_len = payload[i + 3] as usize;
            let start = i + 4;
            let end = start + id_len;
            if end > payload.len() {
                return Err(ScsiError::InvalidResponse {
                    offset: 4 + i + 3,
                    detail: "VPD 0x83 designator length overruns the page payload",
                });
            }
            designators.push(DeviceDesignator {
                code_set,
                association,
                designator_type,
                raw: payload[start..end].to_vec(),
            });
            i = end;
        }
        Ok(Self {
            header,
            designators,
        })
    }

    /// Pick the best designator to use as a "chassis identity" hint for
    /// the operator. Preference order: NAA → EUI-64 → SCSI Name String
    /// → T10 Vendor ID. Restricted to designators associated with the
    /// logical unit or the target device (target-port designators
    /// describe *how* we reached the chassis, not the chassis itself).
    /// Each candidate must also be *usable* — a malformed 5-byte NAA
    /// blob doesn't beat a usable T10 string just because NAA is
    /// higher in the order. Returns `None` if no plausible designator
    /// was found.
    pub fn preferred_chassis(&self) -> Option<&DeviceDesignator> {
        const ORDER: &[DesignatorType] = &[
            DesignatorType::Naa,
            DesignatorType::Eui64,
            DesignatorType::ScsiNameString,
            DesignatorType::T10VendorId,
        ];
        for want in ORDER {
            for d in &self.designators {
                if !matches!(
                    d.association,
                    Association::LogicalUnit | Association::TargetDevice
                ) {
                    continue;
                }
                if d.designator_type != *want {
                    continue;
                }
                if !d.is_usable() {
                    continue;
                }
                return Some(d);
            }
        }
        None
    }
}

impl DeviceDesignator {
    /// Sanity-check the raw bytes against the designator type. Filters
    /// out malformed descriptors (e.g., a NAA that isn't 8 bytes long)
    /// so callers don't pick them in preference rankings. Unknown
    /// designator types pass (we don't know enough to reject them).
    pub fn is_usable(&self) -> bool {
        match self.designator_type {
            DesignatorType::Naa => self.as_naa().is_some(),
            DesignatorType::Eui64 => {
                matches!(self.code_set, CodeSet::Binary) && self.raw.len() == 8
            }
            DesignatorType::T10VendorId
            | DesignatorType::ScsiNameString
            | DesignatorType::VendorSpecific => {
                self.as_str().map(|s| !s.is_empty()).unwrap_or(false)
            }
            DesignatorType::RelativeTargetPort
            | DesignatorType::TargetPortGroup
            | DesignatorType::LogicalUnitGroup => self.raw.len() == 4,
            DesignatorType::Md5LogicalUnitId => self.raw.len() == 16,
            DesignatorType::Other(_) => true,
        }
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const DRIVE1_UNIT_SERIAL: &[u8] = include_bytes!("../../../fixtures/vpd-80/drive1-lto9.bin");
    const CHANGER_UNIT_SERIAL: &[u8] =
        include_bytes!("../../../fixtures/vpd-80/changer-msl-g3.bin");
    // Real-hardware captures from datamover (HP MSL3040 with mixed LTO-7/9 drives).
    const REAL_CHANGER_SERIAL: &[u8] =
        include_bytes!("../../../fixtures/vpd-80/real/changer-msl3040.bin");
    const REAL_LTO9_SERIAL: &[u8] = include_bytes!("../../../fixtures/vpd-80/real/drive-lto9.bin");
    const REAL_LTO7_SERIAL: &[u8] = include_bytes!("../../../fixtures/vpd-80/real/drive-lto7.bin");

    #[test]
    fn parses_drive_unit_serial() {
        let us = UnitSerial::parse(DRIVE1_UNIT_SERIAL).expect("parse drive serial");
        assert_eq!(us.header.page_code, PAGE_UNIT_SERIAL);
        assert_eq!(us.header.device_type, DeviceType::SequentialAccess);
        assert_eq!(us.header.page_length, 10);
        assert_eq!(us.as_str(), "11A1D57AD0");
    }

    #[test]
    fn parses_changer_unit_serial() {
        let us = UnitSerial::parse(CHANGER_UNIT_SERIAL).expect("parse changer serial");
        assert_eq!(us.header.page_code, PAGE_UNIT_SERIAL);
        assert_eq!(us.header.device_type, DeviceType::MediumChanger);
        assert_eq!(us.header.page_length, 10);
        assert_eq!(us.as_str(), "7CBAD9CF74");
    }

    #[test]
    fn parses_real_msl3040_serial() {
        let us = UnitSerial::parse(REAL_CHANGER_SERIAL).expect("parse real MSL3040 serial");
        assert_eq!(us.header.device_type, DeviceType::MediumChanger);
        // The MSL3040 chassis returns a 15-char "logical library" serial.
        assert_eq!(us.header.page_length, 15);
        assert_eq!(us.as_str(), "DEC418146K_LL02");
    }

    #[test]
    fn parses_real_lto9_serial() {
        let us = UnitSerial::parse(REAL_LTO9_SERIAL).expect("parse real LTO-9 serial");
        assert_eq!(us.header.device_type, DeviceType::SequentialAccess);
        assert_eq!(us.header.page_length, 10);
        // 10-char alphanumeric — exact value depends on the specific drive
        // we captured, but the prefix is stable for this library.
        assert!(us.as_str().starts_with("8031BDC7"));
        assert_eq!(us.as_str().len(), 10);
    }

    #[test]
    fn parses_real_lto7_serial() {
        let us = UnitSerial::parse(REAL_LTO7_SERIAL).expect("parse real LTO-7 serial");
        assert_eq!(us.header.device_type, DeviceType::SequentialAccess);
        assert_eq!(us.header.page_length, 10);
        assert!(us.as_str().starts_with("8031BDC7"));
    }

    #[test]
    fn rejects_wrong_page_code() {
        // Forge a response with a page code that isn't 0x80.
        let mut bad = DRIVE1_UNIT_SERIAL.to_vec();
        bad[1] = 0x83;
        match UnitSerial::parse(&bad) {
            Err(ScsiError::InvalidResponse { offset: 1, .. }) => (),
            other => panic!("expected InvalidResponse@1, got {:?}", other),
        }
    }

    #[test]
    fn rejects_non_utf8_unit_serial() {
        let bad = [0x01, PAGE_UNIT_SERIAL, 0x00, 0x02, 0xff, 0xfe];
        match UnitSerial::parse(&bad) {
            Err(ScsiError::InvalidResponse { offset: 4, .. }) => (),
            other => panic!("expected InvalidResponse@4, got {:?}", other),
        }
    }

    #[test]
    fn rejects_padding_only_unit_serial() {
        let bad = [0x01, PAGE_UNIT_SERIAL, 0x00, 0x04, b' ', b' ', 0x00, 0x00];
        match UnitSerial::parse(&bad) {
            Err(ScsiError::InvalidResponse { offset: 4, .. }) => (),
            other => panic!("expected InvalidResponse@4, got {:?}", other),
        }
    }

    #[test]
    fn rejects_truncated_header() {
        match UnitSerial::parse(&[0x01, 0x80, 0x00]) {
            Err(ScsiError::Truncated { got: 3, need: 4 }) => (),
            other => panic!("expected Truncated@4, got {:?}", other),
        }
    }

    #[test]
    fn rejects_truncated_payload() {
        // Header claims 100 bytes of payload but only 4 bytes provided.
        match UnitSerial::parse(&[0x01, 0x80, 0x00, 0x64]) {
            Err(ScsiError::Truncated { got: 4, need: 104 }) => (),
            other => panic!("expected Truncated@104, got {:?}", other),
        }
    }

    // -------- VPD 0x83 fixtures (Device Identification) ------------------

    const REAL_CHANGER_DEVICE_ID: &[u8] =
        include_bytes!("../../../fixtures/real-hardware/remanence-fixtures-datamover-20260516T172906Z/inquiry/vpd-83/changer1.bin");
    const REAL_DRIVE_DEVICE_ID: &[u8] =
        include_bytes!("../../../fixtures/real-hardware/remanence-fixtures-datamover-20260516T172906Z/inquiry/vpd-83/drive1.bin");

    #[test]
    fn parses_real_changer_device_identification() {
        let did =
            DeviceIdentification::parse(REAL_CHANGER_DEVICE_ID).expect("parse changer VPD 0x83");
        assert_eq!(did.header.page_code, PAGE_DEVICE_ID);
        assert_eq!(did.header.device_type, DeviceType::MediumChanger);
        // The MSL3040 changer returns two designators: an 8-byte NAA on
        // the logical unit, and a 39-byte T10 vendor ID (also LU).
        assert_eq!(did.designators.len(), 2);

        let naa = &did.designators[0];
        assert!(matches!(naa.designator_type, DesignatorType::Naa));
        assert!(matches!(naa.association, Association::LogicalUnit));
        assert_eq!(naa.as_naa(), Some(0x5001_4380_31bd_c7d4));

        let t10 = &did.designators[1];
        assert!(matches!(t10.designator_type, DesignatorType::T10VendorId));
        // Trimmed T10 identifier is vendor + product + library serial,
        // each padded with spaces — split_whitespace pulls the last
        // token, which is the partition serial.
        let s = t10.as_str().expect("ASCII");
        assert!(s.starts_with("HPE"));
        assert!(s.ends_with("DEC418146K_LL02"));
    }

    #[test]
    fn parses_real_drive_device_identification() {
        let did = DeviceIdentification::parse(REAL_DRIVE_DEVICE_ID).expect("parse drive VPD 0x83");
        assert_eq!(did.header.device_type, DeviceType::SequentialAccess);
        // The LTO-9 drive returns five descriptors:
        //   1. T10 vendor ID (LU)
        //   2. NAA (LU)
        //   3. Relative target port (target port)
        //   4. NAA (target port)
        //   5. NAA (target device)
        assert_eq!(did.designators.len(), 5);

        // The drive's NAA at LU association.
        let lu_naa = did
            .designators
            .iter()
            .find(|d| {
                matches!(d.designator_type, DesignatorType::Naa)
                    && matches!(d.association, Association::LogicalUnit)
            })
            .expect("LU NAA");
        assert_eq!(lu_naa.as_naa(), Some(0x5001_4380_31bd_c7db));

        // Relative target port — always 1 on our hardware.
        let rtp = did
            .designators
            .iter()
            .find(|d| matches!(d.designator_type, DesignatorType::RelativeTargetPort))
            .expect("relative target port");
        assert_eq!(rtp.raw, vec![0, 0, 0, 1]);
    }

    #[test]
    fn preferred_chassis_picks_naa_when_present() {
        let did = DeviceIdentification::parse(REAL_CHANGER_DEVICE_ID).unwrap();
        let chosen = did
            .preferred_chassis()
            .expect("expected a chassis designator");
        assert!(matches!(chosen.designator_type, DesignatorType::Naa));
        assert_eq!(chosen.as_naa(), Some(0x5001_4380_31bd_c7d4));
    }

    #[test]
    fn preferred_chassis_ignores_target_port_descriptors() {
        // A drive's VPD 0x83 has NAA descriptors at target_port and
        // target_device associations too. preferred_chassis() should
        // not pick the target_port one (that names the port, not the
        // device).
        let did = DeviceIdentification::parse(REAL_DRIVE_DEVICE_ID).unwrap();
        let chosen = did.preferred_chassis().expect("expected a designator");
        assert!(matches!(
            chosen.association,
            Association::LogicalUnit | Association::TargetDevice
        ));
    }

    #[test]
    fn preferred_chassis_skips_malformed_naa_in_favor_of_usable_t10() {
        // Construct a VPD 0x83 payload where the NAA designator has the
        // wrong length (5 bytes instead of 8) and a T10 vendor ID
        // designator carries a usable string. preferred_chassis() must
        // walk past the bogus NAA rather than pick it just because NAA
        // sits higher in the priority order.
        let mut payload = Vec::new();
        // bad NAA: code_set=binary, assoc=LU, type=NAA, length=5
        payload.extend_from_slice(&[0x01, 0x03, 0x00, 0x05, 0xde, 0xad, 0xbe, 0xef, 0x00]);
        // good T10: code_set=ASCII, assoc=LU, type=T10, length=8 "HPE VND1"
        payload.extend_from_slice(&[0x02, 0x01, 0x00, 0x08]);
        payload.extend_from_slice(b"HPE VND1");

        let mut buf = vec![0x08, 0x83, 0x00, payload.len() as u8];
        buf.extend(payload);

        let did = DeviceIdentification::parse(&buf).expect("parse");
        let chosen = did.preferred_chassis().expect("expected a designator");
        assert!(matches!(
            chosen.designator_type,
            DesignatorType::T10VendorId
        ));
        assert_eq!(chosen.as_str(), Some("HPE VND1"));
    }

    #[test]
    fn is_usable_rejects_short_naa_and_eui64() {
        // Standalone unit test for the shape filter so the regression
        // is pinned independent of preferred_chassis().
        let short_naa = DeviceDesignator {
            code_set: CodeSet::Binary,
            association: Association::LogicalUnit,
            designator_type: DesignatorType::Naa,
            raw: vec![0xde, 0xad, 0xbe, 0xef], // 4 bytes (must be 8)
        };
        assert!(!short_naa.is_usable());

        let short_eui = DeviceDesignator {
            code_set: CodeSet::Binary,
            association: Association::LogicalUnit,
            designator_type: DesignatorType::Eui64,
            raw: vec![0xde, 0xad, 0xbe, 0xef],
        };
        assert!(!short_eui.is_usable());

        let empty_t10 = DeviceDesignator {
            code_set: CodeSet::Ascii,
            association: Association::LogicalUnit,
            designator_type: DesignatorType::T10VendorId,
            raw: vec![b' '; 8], // all-space, trims to empty
        };
        assert!(!empty_t10.is_usable());
    }

    #[test]
    fn rejects_partial_designator_header() {
        // Header says 1 byte of designator payload, which is less than
        // the 4-byte designator header.
        let buf = [0x08, 0x83, 0x00, 0x01, 0x99];
        let r = DeviceIdentification::parse(&buf);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail, .. })
                         if detail.contains("partial designator")),
            "got {r:?}"
        );
    }

    #[test]
    fn rejects_overrun_designator_length() {
        // Designator header says identifier is 50 bytes; only 4 bytes
        // of payload remain.
        let buf = [
            0x08, 0x83, 0x00, 0x08, 0x02, 0x01, 0x00, 50, 0x41, 0x42, 0x43, 0x44,
        ];
        let r = DeviceIdentification::parse(&buf);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail, .. })
                         if detail.contains("overruns")),
            "got {r:?}"
        );
    }
}
