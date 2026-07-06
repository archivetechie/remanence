//! READ ELEMENT STATUS (CDB `0xB8`) — SMC-3 §6.13.
//!
//! Builds the 12-byte CDB and parses the response into an
//! [`ElementStatusData`] value that flattens the on-wire structure (header
//! → one page per element type → N descriptors per page) into a single
//! `Vec<Element>` keyed by element address. Each [`Element`] carries its
//! type, full/empty state, the primary volume tag (if the page header set
//! the PVOLTAG bit), the source-address (if SVALID), and the drive serial
//! pulled from a DVCID identifier descriptor (if the target included one).
//!
//! On-wire layout summary (SMC-3 Table 33 / Table 35 / Table 36):
//!
//! ```text
//! Element Status Data header (8 bytes)
//!   byte 0..1  : first element address reported     (u16 BE)
//!   byte 2..3  : number of elements reported        (u16 BE)
//!   byte 4     : reserved
//!   byte 5..7  : byte count of report data          (u24 BE)
//! [for each element-type that has elements:]
//!   Element Status Page header (8 bytes)
//!     byte 0     : element type (1=transport, 2=storage, 3=ie, 4=data)
//!     byte 1     : bit 7 PVOLTAG, bit 6 AVOLTAG, rest reserved
//!     byte 2..3  : element-descriptor length (per descriptor)        (u16 BE)
//!     byte 4     : reserved
//!     byte 5..7  : byte count of all descriptors in this page         (u24 BE)
//!   [for each element in this page:]
//!     element descriptor (variable length, length given in page header):
//!       byte 0..1  : element address                                    (u16 BE)
//!       byte 2     : flags: bit 0 FULL, bit 1 IMPEXP, bit 2 EXCEPT,
//!                            bit 3 ACCESS, bit 4 EXENAB, bit 5 INENAB
//!       byte 3     : reserved
//!       byte 4..5  : ASC / ASCQ
//!       byte 6..8  : reserved (some bits depend on element type)
//!       byte 9     : bit 7 SVALID, bit 6 INVERT (transport/storage),
//!                    bit 7 SVALID, bit 1 ED (drive), depending on type
//!       byte 10..11: source storage element address (valid if SVALID)
//!       byte 12..43: primary volume tag, ASCII             (if PVOLTAG=1)
//!       byte 44..75: alternate volume tag, ASCII           (if AVOLTAG=1)
//!       remainder  : vendor-specific data and/or identifier descriptors
//!                    (DVCID — see [`parse_dvcid_block`])
//! ```
//!
//! Build the CDB with [`build_cdb`]. The recommended "safe" arguments
//! ([`SAFE_NUM_ELEMENTS`], [`SAFE_ALLOC_LEN`]) work on every device tested
//! including QuadStor. Real HPE firmware also accepts the larger plan-doc
//! variant (`0xFFFF` elements, ~16 MB alloc).

use crate::error::ScsiError;

/// SCSI opcode for READ ELEMENT STATUS.
pub const OPCODE: u8 = 0xB8;

/// Conservative element-count for **single-chassis development** use.
/// Sufficient for QuadStor (49 elements) and a single-module MSL3040
/// (≈49 elements), but **insufficient** for a fully populated 7-module
/// MSL3040 stack (up to ≈300 elements). Production discovery (Layer 2)
/// uses [`FULL_NUM_ELEMENTS`] together with a two-phase allocation
/// probe — see `docs/layer2-design.md` §4.2.
pub const SAFE_NUM_ELEMENTS: u16 = 0x0100;

/// "Give me everything" element count. Pair with the
/// [`PROBE_ALLOC_LEN`] / `byte_count+8` two-phase allocation pattern
/// from `docs/layer2-design.md` §4.2 to read arbitrarily large
/// libraries without truncating.
pub const FULL_NUM_ELEMENTS: u16 = 0xFFFF;

/// Conservative allocation length (64 KiB). Comfortable for a
/// 40-slot library; for production-scale discovery use the two-phase
/// probe (PROBE_ALLOC_LEN → exact `byte_count + 8`) instead.
pub const SAFE_ALLOC_LEN: u32 = 0x0001_0000;

/// Maximum allocation length expressible in READ ELEMENT STATUS.
pub const MAX_ALLOC_LEN: u32 = 0x00FF_FFFF;

/// Probe allocation length — just enough room for the 8-byte
/// Element Status Data header. Used as the *first* RES call in the
/// two-phase discovery probe; the target reports its required
/// `byte_count`, and a follow-up call uses `byte_count + 8` to read
/// the full descriptor stream regardless of library size.
pub const PROBE_ALLOC_LEN: u32 = 8;

/// SMC-3 element type codes (CDB byte 1 low nibble; also the first byte of
/// each Element Status Page header).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum ElementType {
    /// 1 — robot / picker arm.
    MediumTransport,
    /// 2 — storage slot.
    Storage,
    /// 3 — import/export port (mail slot).
    ImportExport,
    /// 4 — tape drive bay.
    DataTransfer,
    /// Anything else, preserved as raw code.
    Other(u8),
}

impl ElementType {
    /// Map a raw element-type byte to its [`ElementType`].
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Self::MediumTransport,
            2 => Self::Storage,
            3 => Self::ImportExport,
            4 => Self::DataTransfer,
            other => Self::Other(other),
        }
    }
}

/// One element in the library: a slot, drive bay, IE port, or the robot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    /// What kind of element this is.
    pub element_type: ElementType,
    /// SCSI element address (the unit the library uses to address it).
    pub address: u16,
    /// True if this element has a medium loaded.
    pub full: bool,
    /// True if the medium currently in this element was placed here via
    /// the import/export port (SMC-3 IMPEXP bit). Only meaningful when
    /// `full` is also set.
    pub impexp: bool,
    /// True if the element raised an exception (ASC/ASCQ would explain why).
    pub except: bool,
    /// Additional Sense Code from the element descriptor common prefix.
    /// Meaningful when [`Self::except`] is set; retained even when clear so
    /// callers can persist the target's exact bytes.
    pub asc: u8,
    /// Additional Sense Code Qualifier from the element descriptor common
    /// prefix. Meaningful when [`Self::except`] is set.
    pub ascq: u8,
    /// True if the element is accessible to the medium transport.
    pub access: bool,
    /// True if the IE port currently accepts exports (SMC-3 EXENAB
    /// flag). Only meaningful for IE-port elements; 0 for everything else.
    pub export_enabled: bool,
    /// True if the IE port currently accepts imports (SMC-3 INENAB
    /// flag). Only meaningful for IE-port elements; 0 for everything else.
    pub import_enabled: bool,
    /// If SVALID was set, the source element address from the last move —
    /// for a drive, this is the slot the loaded tape came from.
    pub source_address: Option<u16>,
    /// Primary volume tag (cartridge barcode) if the page set PVOLTAG.
    /// Trimmed of trailing spaces; `None` when the page didn't include it
    /// or the field was empty.
    pub primary_voltag: Option<String>,
    /// Drive serial number, only present when the target included a DVCID
    /// identifier descriptor for this drive (some firmware skips this
    /// even when DVCID=1 is set in the CDB).
    pub drive_serial: Option<String>,
}

/// Fully parsed READ ELEMENT STATUS response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElementStatusData {
    /// First element address reported, from the response header.
    pub first_element_address: u16,
    /// Number of elements reported, from the response header. Should equal
    /// `elements.len()` after parsing.
    pub num_elements: u16,
    /// All elements, in the order the target returned them. Different
    /// element types are grouped because they come in separate pages.
    pub elements: Vec<Element>,
}

impl ElementStatusData {
    /// Convenience: iterate only the elements matching the given type.
    pub fn by_type(&self, t: ElementType) -> impl Iterator<Item = &Element> {
        self.elements.iter().filter(move |e| e.element_type == t)
    }
}

/// Build the 12-byte READ ELEMENT STATUS CDB.
///
/// - `element_type` — set to 0 to ask for all element types.
/// - `starting_element_address` — 0 means "from the lowest address".
/// - `num_elements` — how many elements to ask for. Use [`SAFE_NUM_ELEMENTS`].
/// - `voltag` — set the VOLTAG bit so the target returns volume tags.
/// - `dvcid` — set the DVCID bit so data-transfer descriptors include the
///   drive's identifier (typically the unit serial number).
/// - `curdata` — set the CurData bit. Production discovery normally asks for
///   both DVCID and CurData so targets can return cached element state plus
///   drive identifiers in one read-only probe.
///
///   Semantically, `curdata=true` tells the target "return cached element
///   state, no device motion" — fast and is the right default for
///   Remanence's read-mostly discovery flow.
/// - `alloc_len` — buffer size we hand the target. Use [`SAFE_ALLOC_LEN`].
pub fn build_cdb(
    element_type: u8,
    starting_element_address: u16,
    num_elements: u16,
    voltag: bool,
    dvcid: bool,
    curdata: bool,
    alloc_len: u32,
) -> [u8; 12] {
    assert!(
        alloc_len <= MAX_ALLOC_LEN,
        "READ ELEMENT STATUS allocation length {alloc_len} exceeds 24-bit max"
    );
    let mut byte1 = element_type & 0x0f;
    if voltag {
        byte1 |= 0x10;
    }
    let mut byte6 = 0u8;
    if dvcid {
        byte6 |= 0x01;
    }
    if curdata {
        byte6 |= 0x02;
    }
    [
        OPCODE,
        byte1,
        (starting_element_address >> 8) as u8,
        (starting_element_address & 0xff) as u8,
        (num_elements >> 8) as u8,
        (num_elements & 0xff) as u8,
        byte6,
        ((alloc_len >> 16) & 0xff) as u8,
        ((alloc_len >> 8) & 0xff) as u8,
        (alloc_len & 0xff) as u8,
        0x00, // reserved
        0x00, // control
    ]
}

// -------------------------------------------------------------- parsing

const HEADER_LEN: usize = 8;
const PAGE_HEADER_LEN: usize = 8;
const COMMON_DESC_LEN: usize = 12; // bytes 0..11 of every element descriptor

// SMC-3 Table 47: each volume-tag information block is 36 bytes —
//   32 bytes Primary Volume Identifier (ASCII, space-padded)
//   2 bytes reserved
//   2 bytes Primary Volume Sequence Number (big-endian u16)
// The cursor must skip the full 36 bytes after a PVOLTAG/AVOLTAG block to
// reach the next field (DVCID descriptors, vendor data), but only the
// first 32 bytes carry the printable identifier we surface.
const VOLTAG_BLOCK_LEN: usize = 36;
const VOLTAG_ID_LEN: usize = 32;

/// Parse a READ ELEMENT STATUS response.
pub fn parse(buf: &[u8]) -> Result<ElementStatusData, ScsiError> {
    if buf.len() < HEADER_LEN {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: HEADER_LEN,
        });
    }
    let first_element_address = u16::from_be_bytes([buf[0], buf[1]]);
    let num_elements = u16::from_be_bytes([buf[2], buf[3]]);
    let byte_count = ((buf[5] as usize) << 16) | ((buf[6] as usize) << 8) | (buf[7] as usize);
    let need = HEADER_LEN + byte_count;
    if buf.len() < need {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need,
        });
    }

    let mut elements: Vec<Element> = Vec::with_capacity(num_elements as usize);
    let mut cursor = HEADER_LEN;
    let end = need;

    while cursor + PAGE_HEADER_LEN <= end {
        // ---- page header --------------------------------------------
        let p = &buf[cursor..cursor + PAGE_HEADER_LEN];
        let etype = ElementType::from_code(p[0]);
        let pvoltag = (p[1] & 0x80) != 0;
        let avoltag = (p[1] & 0x40) != 0;
        let desc_len = u16::from_be_bytes([p[2], p[3]]) as usize;
        let page_bytes = ((p[5] as usize) << 16) | ((p[6] as usize) << 8) | (p[7] as usize);
        cursor += PAGE_HEADER_LEN;

        if desc_len < COMMON_DESC_LEN {
            return Err(ScsiError::InvalidResponse {
                offset: cursor - PAGE_HEADER_LEN + 2,
                detail: "element descriptor length is smaller than the common-prefix size",
            });
        }
        // A page's byte count must be an exact multiple of the descriptor
        // length. Anything else is malformed framing — a hostile target
        // could exploit "slack" to hide bytes after a descriptor.
        if page_bytes % desc_len != 0 {
            return Err(ScsiError::InvalidResponse {
                offset: cursor - PAGE_HEADER_LEN + 5,
                detail: "page byte count is not a multiple of element-descriptor length",
            });
        }
        if cursor + page_bytes > end {
            return Err(ScsiError::Truncated {
                got: end,
                need: cursor + page_bytes,
            });
        }

        // ---- iterate this page's descriptors ------------------------
        let page_end = cursor + page_bytes;
        while cursor + desc_len <= page_end {
            let d = &buf[cursor..cursor + desc_len];
            let address = u16::from_be_bytes([d[0], d[1]]);
            let flags = d[2];
            let svalid = (d[9] & 0x80) != 0;
            let source_address = if svalid {
                Some(u16::from_be_bytes([d[10], d[11]]))
            } else {
                None
            };

            // Optional PVOLTAG occupies a 36-byte block when set in the page
            // (32-byte identifier + 4 bytes reserved/sequence-number).
            let mut after_voltags = COMMON_DESC_LEN;
            let primary_voltag = if pvoltag {
                if d.len() < after_voltags + VOLTAG_BLOCK_LEN {
                    return Err(ScsiError::Truncated {
                        got: d.len(),
                        need: after_voltags + VOLTAG_BLOCK_LEN,
                    });
                }
                // Read only the 32-byte identifier prefix; the trailing
                // 4 bytes are not printable.
                let voltag =
                    trim_fixed_width_text(&d[after_voltags..after_voltags + VOLTAG_ID_LEN]);
                after_voltags += VOLTAG_BLOCK_LEN;
                voltag.map(str::to_string)
            } else {
                None
            };
            if avoltag {
                // Validate the AVOLTAG space exists in the descriptor before
                // skipping past it; otherwise DVCID parsing reads from a
                // bogus offset for non-data-transfer elements too.
                if d.len() < after_voltags + VOLTAG_BLOCK_LEN {
                    return Err(ScsiError::Truncated {
                        got: d.len(),
                        need: after_voltags + VOLTAG_BLOCK_LEN,
                    });
                }
                after_voltags += VOLTAG_BLOCK_LEN;
            }

            // Anything past the (optional) voltags can be DVCID identifier
            // descriptors. Parse them only for data-transfer elements; the
            // spec lets transports/storage carry vendor data here.
            let drive_serial = if matches!(etype, ElementType::DataTransfer) {
                let extra = if after_voltags < d.len() {
                    &d[after_voltags..]
                } else {
                    &[]
                };
                parse_dvcid_block(extra)?
            } else {
                None
            };

            elements.push(Element {
                element_type: etype,
                address,
                full: (flags & 0x01) != 0,
                impexp: (flags & 0x02) != 0,
                except: (flags & 0x04) != 0,
                asc: d[4],
                ascq: d[5],
                access: (flags & 0x08) != 0,
                export_enabled: (flags & 0x10) != 0,
                import_enabled: (flags & 0x20) != 0,
                source_address,
                primary_voltag,
                drive_serial,
            });
            cursor += desc_len;
        }
        // page_bytes is now divisible by desc_len (checked above), so the
        // descriptor loop consumed exactly page_bytes. Belt-and-braces in
        // case of future refactors:
        debug_assert_eq!(cursor, page_end);
    }

    // Caller asked the target for N elements and the response header said
    // N. If the page parsing produced a different count, the target's
    // header lied or the framing was inconsistent — surface it.
    if elements.len() != num_elements as usize {
        return Err(ScsiError::InvalidResponse {
            offset: 2,
            detail: "header num_elements does not match the count of parsed descriptors",
        });
    }
    // The response header's byte_count must cover all pages exactly. Any
    // trailing slack would let a hostile target hide bytes the parser
    // never inspected.
    if cursor != end {
        return Err(ScsiError::InvalidResponse {
            offset: 5,
            detail: "byte count in header does not match the sum of page-byte-counts",
        });
    }

    Ok(ElementStatusData {
        first_element_address,
        num_elements,
        elements,
    })
}

/// Space-fill the primary volume tag for one element descriptor.
///
/// This walks the READ ELEMENT STATUS response framing defensively and returns
/// `false` for malformed, truncated, non-PVOLTAG, or missing descriptors. It
/// never panics on medium-sourced bytes.
pub fn blank_element_voltag(page: &mut [u8], element_address: u16) -> bool {
    mutate_element_descriptor(page, element_address, |descriptor, pvoltag| {
        if !pvoltag || descriptor.len() < COMMON_DESC_LEN + VOLTAG_BLOCK_LEN {
            return false;
        }
        let start = COMMON_DESC_LEN;
        descriptor[start..start + VOLTAG_ID_LEN].fill(b' ');
        true
    })
}

/// Mark one element descriptor as inaccessible with an exception.
///
/// The helper sets EXCEPT (`0x04`) and clears ACCESS (`0x08`) in the targeted
/// descriptor's common flags byte. It returns `false` rather than panicking on
/// malformed READ ELEMENT STATUS framing or an absent descriptor.
pub fn set_element_exception(page: &mut [u8], element_address: u16) -> bool {
    mutate_element_descriptor(page, element_address, |descriptor, _pvoltag| {
        if descriptor.len() < COMMON_DESC_LEN {
            return false;
        }
        descriptor[2] |= 0x04;
        descriptor[2] &= !0x08;
        true
    })
}

fn mutate_element_descriptor(
    page: &mut [u8],
    element_address: u16,
    mut mutate: impl FnMut(&mut [u8], bool) -> bool,
) -> bool {
    if page.len() < HEADER_LEN {
        return false;
    }
    let byte_count = ((page[5] as usize) << 16) | ((page[6] as usize) << 8) | (page[7] as usize);
    let Some(end) = HEADER_LEN.checked_add(byte_count) else {
        return false;
    };
    if page.len() < end {
        return false;
    }

    let mut cursor = HEADER_LEN;
    while cursor < end {
        let Some(page_header_end) = cursor.checked_add(PAGE_HEADER_LEN) else {
            return false;
        };
        if page_header_end > end {
            return false;
        }

        let pvoltag = (page[cursor + 1] & 0x80) != 0;
        let desc_len = u16::from_be_bytes([page[cursor + 2], page[cursor + 3]]) as usize;
        let page_bytes = ((page[cursor + 5] as usize) << 16)
            | ((page[cursor + 6] as usize) << 8)
            | page[cursor + 7] as usize;
        if desc_len < COMMON_DESC_LEN || page_bytes % desc_len != 0 {
            return false;
        }
        let Some(desc_start) = cursor.checked_add(PAGE_HEADER_LEN) else {
            return false;
        };
        let Some(desc_end) = desc_start.checked_add(page_bytes) else {
            return false;
        };
        if desc_end > end {
            return false;
        }

        let mut descriptor_offset = desc_start;
        while descriptor_offset < desc_end {
            let Some(next_descriptor) = descriptor_offset.checked_add(desc_len) else {
                return false;
            };
            if next_descriptor > desc_end {
                return false;
            }
            let address =
                u16::from_be_bytes([page[descriptor_offset], page[descriptor_offset + 1]]);
            if address == element_address {
                return mutate(&mut page[descriptor_offset..next_descriptor], pvoltag);
            }
            descriptor_offset = next_descriptor;
        }

        cursor = desc_end;
    }

    false
}

/// Trim trailing ASCII spaces and NULs from a fixed-width text field.
fn trim_fixed_width_text(buf: &[u8]) -> Option<&str> {
    let end = buf
        .iter()
        .rposition(|&b| b != b' ' && b != 0)
        .map_or(0, |i| i + 1);
    if end == 0 {
        return None;
    }
    core::str::from_utf8(&buf[..end]).ok()
}

/// Walk a tail of identifier descriptors (SPC-5 §7.7.7-style) looking for
/// one whose identifier looks like a vendor-specific or T10-vendor-id
/// drive serial. Returns the first plausible one, or `None`.
///
/// Layout of one identifier descriptor:
///
/// ```text
///   byte 0  : bits 0..3 code set (1=binary, 2=ASCII, 3=UTF-8)
///   byte 1  : bit 4..7 PIV/reserved, bits 0..3 identifier type
///             (0=vendor-specific, 1=T10 vendor ID, 2=EUI-64, ...)
///   byte 2  : reserved
///   byte 3  : identifier length N
///   bytes 4..4+N: identifier
/// ```
fn parse_dvcid_block(buf: &[u8]) -> Result<Option<String>, ScsiError> {
    let mut i = 0;
    while i < buf.len() {
        // A descriptor must have a full 4-byte header. A trailing fragment
        // smaller than that is malformed — refuse to silently lose data.
        if i + 4 > buf.len() {
            return Err(ScsiError::InvalidResponse {
                offset: i,
                detail: "DVCID block has a partial identifier descriptor at its tail",
            });
        }
        let code_set = buf[i] & 0x0f;
        let id_len = buf[i + 3] as usize;
        let header = 4;
        if i + header + id_len > buf.len() {
            return Err(ScsiError::InvalidResponse {
                offset: i + 3,
                detail: "DVCID identifier_length overruns the descriptor block",
            });
        }
        let ident = &buf[i + header..i + header + id_len];
        // Accept ASCII (2) and UTF-8 (3); binary (1) is also possible but
        // rarer for drive serials and we don't want to surface raw hex
        // here. Skip empty / all-padding identifiers.
        if (code_set == 2 || code_set == 3) && !ident.is_empty() {
            if let Some(raw) = trim_fixed_width_text(ident) {
                // For T10 vendor-ID format, HPE packs the 34-byte identifier
                // as `<8-byte vendor><16-byte product><10-byte serial>` —
                // whitespace-padded fixed-width fields. SPC-5 only mandates
                // the 8-byte vendor prefix; the rest is vendor-defined.
                // Splitting on whitespace and taking the last token cleanly
                // peels off the serial for HPE drives, and falls back to
                // the whole string for any other vendor whose format we
                // haven't seen yet (callers can still pattern-match later).
                let serial = raw.split_whitespace().last().unwrap_or(raw);
                return Ok(Some(serial.to_string()));
            }
        }
        i += header + id_len;
    }
    Ok(None)
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    // Fixtures captured 2026-05-16:
    //   quadstor-msl-g3.bin            — QuadStor RES without identifier descriptors
    //   quadstor-msl-g3-dvcid.bin      — same target, DVCID=1 + CurData=1 → +identifier
    //   real-msl3040.bin               — real HPE MSL3040 (43 elements, no DVCID)
    //   real-msl3040-dvcid.bin         — real HPE MSL3040, element_type=4 +
    //                                     DVCID=1 + CurData=1 → drive serials inline
    //   real-msl3040-full-dvcid.bin    — real HPE MSL3040, all elements + DVCID
    //                                     (mixed desc_len: 52 for non-drives, 86 for drives)
    const QUADSTOR: &[u8] = include_bytes!("../../../fixtures/element-status/quadstor-msl-g3.bin");
    const QUADSTOR_DVCID: &[u8] =
        include_bytes!("../../../fixtures/element-status/quadstor-msl-g3-dvcid.bin");
    const REAL_MSL: &[u8] = include_bytes!("../../../fixtures/element-status/real-msl3040.bin");
    const REAL_MSL_DVCID: &[u8] =
        include_bytes!("../../../fixtures/element-status/real-msl3040-dvcid.bin");
    const REAL_MSL_FULL_DVCID: &[u8] =
        include_bytes!("../../../fixtures/element-status/real-msl3040-full-dvcid.bin");

    #[test]
    fn cdb_layout_matches_spec() {
        let cdb = build_cdb(0, 0, SAFE_NUM_ELEMENTS, true, true, true, SAFE_ALLOC_LEN);
        assert_eq!(cdb[0], 0xB8);
        assert_eq!(cdb[1] & 0x0f, 0, "element type = 0 (all)");
        assert_eq!(cdb[1] & 0x10, 0x10, "VOLTAG bit set");
        assert_eq!(u16::from_be_bytes([cdb[2], cdb[3]]), 0);
        assert_eq!(u16::from_be_bytes([cdb[4], cdb[5]]), SAFE_NUM_ELEMENTS);
        assert_eq!(cdb[6] & 0x01, 0x01, "DVCID bit set");
        assert_eq!(cdb[6] & 0x02, 0x02, "CurData bit set");
        let alloc = ((cdb[7] as u32) << 16) | ((cdb[8] as u32) << 8) | (cdb[9] as u32);
        assert_eq!(alloc, SAFE_ALLOC_LEN);
    }

    #[test]
    fn cdb_dvcid_and_curdata_bits_are_individually_spec_positioned() {
        let dvcid_only = build_cdb(4, 0, 1, false, true, false, 8);
        let curdata_only = build_cdb(4, 0, 1, false, false, true, 8);

        assert_eq!(dvcid_only[6], 0x01, "byte 6 bit 0 is DVCID");
        assert_eq!(curdata_only[6], 0x02, "byte 6 bit 1 is CurData");
    }

    fn two_slot_response() -> Vec<u8> {
        fn descriptor(address: u16, barcode: &str) -> Vec<u8> {
            let mut desc = vec![0u8; COMMON_DESC_LEN + VOLTAG_BLOCK_LEN];
            desc[0..2].copy_from_slice(&address.to_be_bytes());
            desc[2] = 0x09; // FULL + ACCESS
            desc[COMMON_DESC_LEN..COMMON_DESC_LEN + VOLTAG_ID_LEN].fill(b' ');
            for (dst, src) in desc[COMMON_DESC_LEN..COMMON_DESC_LEN + VOLTAG_ID_LEN]
                .iter_mut()
                .zip(barcode.as_bytes())
            {
                *dst = *src;
            }
            desc
        }

        let desc_len = COMMON_DESC_LEN + VOLTAG_BLOCK_LEN;
        let descs = [descriptor(0x0400, "TAPE001"), descriptor(0x0401, "TAPE002")].concat();
        let page_bytes = descs.len();
        let report_bytes = PAGE_HEADER_LEN + page_bytes;
        let mut response = vec![0u8; HEADER_LEN + report_bytes];
        response[0..2].copy_from_slice(&0x0400u16.to_be_bytes());
        response[2..4].copy_from_slice(&2u16.to_be_bytes());
        response[5] = ((report_bytes >> 16) & 0xff) as u8;
        response[6] = ((report_bytes >> 8) & 0xff) as u8;
        response[7] = (report_bytes & 0xff) as u8;

        let page = HEADER_LEN;
        response[page] = 2;
        response[page + 1] = 0x80; // PVOLTAG
        response[page + 2..page + 4].copy_from_slice(&(desc_len as u16).to_be_bytes());
        response[page + 5] = ((page_bytes >> 16) & 0xff) as u8;
        response[page + 6] = ((page_bytes >> 8) & 0xff) as u8;
        response[page + 7] = (page_bytes & 0xff) as u8;
        response[HEADER_LEN + PAGE_HEADER_LEN..].copy_from_slice(&descs);
        response
    }

    #[test]
    fn blank_element_voltag_mutates_target_only() {
        let mut response = two_slot_response();

        assert!(blank_element_voltag(&mut response, 0x0400));

        let parsed = parse(&response).expect("mutated RES still parses");
        let first = parsed
            .elements
            .iter()
            .find(|element| element.address == 0x0400)
            .expect("first slot");
        let second = parsed
            .elements
            .iter()
            .find(|element| element.address == 0x0401)
            .expect("second slot");
        assert!(first.full);
        assert_eq!(first.primary_voltag, None);
        assert_eq!(second.primary_voltag.as_deref(), Some("TAPE002"));
    }

    #[test]
    fn set_element_exception_mutates_target_only() {
        let mut response = two_slot_response();

        assert!(set_element_exception(&mut response, 0x0401));

        let parsed = parse(&response).expect("mutated RES still parses");
        let first = parsed
            .elements
            .iter()
            .find(|element| element.address == 0x0400)
            .expect("first slot");
        let second = parsed
            .elements
            .iter()
            .find(|element| element.address == 0x0401)
            .expect("second slot");
        assert!(first.access);
        assert!(!first.except);
        assert!(!second.access);
        assert!(second.except);
    }

    #[test]
    fn parses_element_exception_asc_and_ascq() {
        let mut response = two_slot_response();
        let desc = HEADER_LEN + PAGE_HEADER_LEN + COMMON_DESC_LEN + VOLTAG_BLOCK_LEN;
        response[desc + 2] |= 0x04;
        response[desc + 4] = 0x04;
        response[desc + 5] = 0x01;

        let parsed = parse(&response).expect("RES with element exception parses");
        let second = parsed
            .elements
            .iter()
            .find(|element| element.address == 0x0401)
            .expect("second slot");

        assert!(second.except);
        assert_eq!(second.asc, 0x04);
        assert_eq!(second.ascq, 0x01);
    }

    #[test]
    fn element_mutators_fail_closed_on_truncation() {
        let mut response = two_slot_response();
        response.truncate(response.len() - 1);

        assert!(!blank_element_voltag(&mut response, 0x0400));
        assert!(!set_element_exception(&mut response, 0x0400));
    }

    #[test]
    #[should_panic(expected = "READ ELEMENT STATUS allocation length")]
    fn cdb_rejects_over_24_bit_allocation_length() {
        let _ = build_cdb(0, 0, SAFE_NUM_ELEMENTS, true, true, true, MAX_ALLOC_LEN + 1);
    }

    #[test]
    fn parses_ie_port_export_and_import_enable_flags() {
        let mut response = vec![0u8; HEADER_LEN + PAGE_HEADER_LEN + COMMON_DESC_LEN];
        response[2..4].copy_from_slice(&1u16.to_be_bytes());
        response[7] = (PAGE_HEADER_LEN + COMMON_DESC_LEN) as u8;
        let page = HEADER_LEN;
        response[page] = 3;
        response[page + 2..page + 4].copy_from_slice(&(COMMON_DESC_LEN as u16).to_be_bytes());
        response[page + 7] = COMMON_DESC_LEN as u8;
        let desc = HEADER_LEN + PAGE_HEADER_LEN;
        response[desc..desc + 2].copy_from_slice(&0x0101u16.to_be_bytes());
        response[desc + 2] = 0x38;

        let parsed = parse(&response).expect("parse one IE-port descriptor");
        let ie = parsed
            .elements
            .iter()
            .find(|element| element.element_type == ElementType::ImportExport)
            .expect("IE port");

        assert!(ie.access);
        assert!(ie.export_enabled);
        assert!(ie.import_enabled);
    }

    #[test]
    fn invalid_dvcid_text_does_not_become_empty_serial() {
        let block = [0x02, 0x00, 0x00, 0x01, 0xff];

        let serial = parse_dvcid_block(&block).expect("descriptor parses structurally");

        assert_eq!(serial, None);
    }

    #[test]
    fn invalid_primary_voltag_text_does_not_become_empty_barcode() {
        let desc_len = COMMON_DESC_LEN + VOLTAG_BLOCK_LEN;
        let mut response = vec![0u8; HEADER_LEN + PAGE_HEADER_LEN + desc_len];
        response[2..4].copy_from_slice(&1u16.to_be_bytes());
        response[7] = (PAGE_HEADER_LEN + desc_len) as u8;

        let page = HEADER_LEN;
        response[page] = 2; // storage element
        response[page + 1] = 0x80; // PVOLTAG
        response[page + 2..page + 4].copy_from_slice(&(desc_len as u16).to_be_bytes());
        response[page + 7] = desc_len as u8;

        let desc = HEADER_LEN + PAGE_HEADER_LEN;
        response[desc..desc + 2].copy_from_slice(&0x03e9u16.to_be_bytes());
        response[desc + COMMON_DESC_LEN] = 0xff;

        let parsed = parse(&response).expect("parse invalid text as absent voltag");
        let slot = parsed
            .elements
            .iter()
            .find(|element| element.element_type == ElementType::Storage)
            .expect("storage element");

        assert_eq!(slot.primary_voltag, None);
    }

    #[test]
    fn extracts_drive_serials_from_dvcid_block() {
        // dt_only probe → only data-transfer elements requested,
        // CurData=1+DVCID=1 → identifier descriptors emitted in the fixture.
        let r = parse(QUADSTOR_DVCID).expect("parse QuadStor DVCID fixture");
        let drives: Vec<_> = r.by_type(ElementType::DataTransfer).collect();
        assert_eq!(
            drives.len(),
            4,
            "fixture is element_type=4 (data-transfer only)"
        );
        // QuadStor's mainlib drive serials from VPD 0x80 — proves the
        // RES-side and INQUIRY-side mappings agree.
        let serials: Vec<&str> = drives
            .iter()
            .map(|d| d.drive_serial.as_deref().expect("drive_serial populated"))
            .collect();
        assert_eq!(
            serials,
            vec!["11A1D57AD0", "6D71FB6FE6", "2FEB23D41A", "79B07D9D00"]
        );
    }

    #[test]
    fn parses_quadstor_msl_g3() {
        let r = parse(QUADSTOR).expect("parse QuadStor RES");
        // QuadStor: 1 robot + 4 drives + 40 slots + 4 IE = 49 elements.
        assert_eq!(r.num_elements, 49);
        assert_eq!(r.elements.len(), 49);

        let robots: Vec<_> = r.by_type(ElementType::MediumTransport).collect();
        let drives: Vec<_> = r.by_type(ElementType::DataTransfer).collect();
        let slots: Vec<_> = r.by_type(ElementType::Storage).collect();
        let ieports: Vec<_> = r.by_type(ElementType::ImportExport).collect();

        assert_eq!(robots.len(), 1, "MSL G3 has one picker");
        assert_eq!(drives.len(), 4);
        assert_eq!(slots.len(), 40);
        assert_eq!(ieports.len(), 4);

        // No tape loaded in the QuadStor library: every drive should be empty.
        assert!(drives.iter().all(|d| !d.full));
    }

    #[test]
    fn parses_real_msl3040() {
        let r = parse(REAL_MSL).expect("parse real MSL3040 RES");
        // MSL3040 in this config reports 43 elements (4 drives + 1 robot +
        // 0 IE ports configured + 38 slots populated this way).
        assert_eq!(r.num_elements, 43);
        assert_eq!(r.elements.len(), 43);

        let drives: Vec<_> = r.by_type(ElementType::DataTransfer).collect();
        assert_eq!(drives.len(), 2, "datamover changer1 reports 2 drives");

        // Drive 1 in mtx output: Full, loaded from Storage Element 34, voltag S30002L9.
        let d1 = drives
            .iter()
            .find(|d| d.address == 1)
            .expect("drive 1 present");
        assert!(d1.full, "drive 1 should be full");
        assert_eq!(d1.primary_voltag.as_deref(), Some("S30002L9"));
        assert_eq!(
            d1.source_address,
            Some(0x040a),
            "source = 1034 = storage element 34"
        );

        // Slot 1 (address 0x3e9 = 1001) has CLNU01L9 loaded (cleaning tape).
        let slot1 = r
            .elements
            .iter()
            .find(|e| e.element_type == ElementType::Storage && e.address == 0x03e9)
            .expect("first storage slot present");
        assert_eq!(slot1.primary_voltag.as_deref(), Some("CLNU01L9"));
        assert!(slot1.full);
    }

    #[test]
    fn real_msl3040_dvcid_block_works_too() {
        // The primary DVCID=1 + CurData=1 discovery CDB produces the
        // identifier block on the production HP MSL3040 firmware 3350.
        // Confirms the discovery path generalizes from the emulator to the
        // real device.
        let r = parse(REAL_MSL_DVCID).expect("parse real MSL3040 DVCID fixture");
        let drives: Vec<_> = r.by_type(ElementType::DataTransfer).collect();
        assert_eq!(drives.len(), 2, "datamover changer1 sees 2 drives");
        // Both LTO-9. Serials match what VPD 0x80 returns when queried
        // directly via each drive's /dev/sgN — proving RES-DVCID is a
        // trustworthy primary path for bay→serial mapping on this hardware.
        let serials: Vec<&str> = drives
            .iter()
            .map(|d| d.drive_serial.as_deref().expect("drive_serial populated"))
            .collect();
        assert_eq!(serials, vec!["8031BDC7D1", "8031BDC7DB"]);
        // Element addresses match what mtx reports.
        assert_eq!(drives[0].address, 0x0001);
        assert_eq!(drives[1].address, 0x0002);
    }

    #[test]
    fn parses_mixed_descriptor_lengths_across_pages() {
        // The "full DVCID" capture covers every element type in one
        // response. Drives have desc_len=86 (DVCID-extended); slots,
        // transport, and IE elements stay at desc_len=52. Each Element
        // Status Page header carries its own desc_len, so the parser must
        // pick it up fresh on every page.
        let r = parse(REAL_MSL_FULL_DVCID).expect("parse full-DVCID fixture");
        assert_eq!(r.num_elements, 43);

        // Spot-check a slot voltag: storage element 1 (CLNU01L9 per mtx).
        let slot1 = r
            .elements
            .iter()
            .find(|e| e.element_type == ElementType::Storage && e.address == 0x03e9)
            .expect("first storage slot");
        assert_eq!(slot1.primary_voltag.as_deref(), Some("CLNU01L9"));

        // Spot-check drive 1: full, voltag S30002L9, source 0x040a,
        // drive_serial from inline DVCID.
        let d1 = r
            .elements
            .iter()
            .find(|e| e.element_type == ElementType::DataTransfer && e.address == 1)
            .expect("drive 1");
        assert!(d1.full);
        assert_eq!(d1.primary_voltag.as_deref(), Some("S30002L9"));
        assert_eq!(d1.source_address, Some(0x040a));
        assert_eq!(d1.drive_serial.as_deref(), Some("8031BDC7D1"));
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(matches!(
            parse(&[0u8; 4]),
            Err(ScsiError::Truncated { got: 4, need: 8 })
        ));
    }

    #[test]
    fn rejects_truncated_payload() {
        // Header says 1000 bytes of payload but we only give 8.
        let mut b = [0u8; 8];
        b[5] = 0x00;
        b[6] = 0x03;
        b[7] = 0xe8; // 0x0003e8 = 1000
        assert!(matches!(
            parse(&b),
            Err(ScsiError::Truncated { got: 8, need: 1008 })
        ));
    }

    #[test]
    fn rejects_page_bytes_not_multiple_of_descriptor_length() {
        // Header: 1 element, byte_count = 21 (8 page header + 13 = 8+12+1).
        // Page: type=2, no voltag, desc_len=12, page_bytes=13 (NOT a
        // multiple of 12). Parser must reject this rather than silently
        // skipping the trailing 1 byte of "slack".
        let mut b = vec![0u8; 8];
        b[3] = 0x01; // num_elements = 1
        b[5] = 0x00;
        b[6] = 0x00;
        b[7] = 21; // byte_count = 21
        b.extend_from_slice(&[
            0x02, 0x00, 0x00, 0x0c, // type=2, voltag=0, desc_len=12
            0x00, 0x00, 0x00, 13, // page byte count = 13 (bad!)
        ]);
        b.extend_from_slice(&[0u8; 13]); // 13 bytes of descriptor data
        let r = parse(&b);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail: d, .. }) if d.contains("multiple of")),
            "got {r:?}",
        );
    }

    #[test]
    fn rejects_header_num_elements_mismatch() {
        // Header claims 5 elements but body frames 1.
        let mut b = vec![0u8; 8];
        b[3] = 5; // num_elements = 5 (lie)
        b[5] = 0x00;
        b[6] = 0x00;
        b[7] = 20; // byte_count = 20
        b.extend_from_slice(&[
            0x02, 0x00, 0x00, 0x0c, // type=2, desc_len=12
            0x00, 0x00, 0x00, 12, // page_bytes=12 (exactly one descriptor)
        ]);
        b.extend_from_slice(&[0u8; 12]);
        let r = parse(&b);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail, .. })
                     if detail.contains("num_elements")),
            "got {r:?}",
        );
    }

    #[test]
    fn rejects_trailing_slack_after_last_page() {
        // Header claims 25 bytes total, the single page consumes exactly
        // 20 (8 page header + 12 desc). Cursor ends at 28, end is 33,
        // and there's 5 bytes of trailing slack — too few for another
        // page header (8 bytes) so the loop exits with cursor != end.
        let mut b = vec![0u8; 8];
        b[3] = 1; // num_elements = 1
        b[5] = 0x00;
        b[6] = 0x00;
        b[7] = 25; // byte_count = 25 (lie)
        b.extend_from_slice(&[
            0x02, 0x00, 0x00, 0x0c, // type=2, desc_len=12
            0x00, 0x00, 0x00, 12, // page_bytes=12
        ]);
        b.extend_from_slice(&[0u8; 12]); // descriptor
        b.extend_from_slice(&[0u8; 5]); // 5-byte trailing slack
        let r = parse(&b);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail, .. })
                     if detail.contains("byte count")),
            "got {r:?}",
        );
    }

    #[test]
    fn parse_dvcid_block_rejects_overrun_identifier_length() {
        // Identifier descriptor claims a 50-byte identifier but only 4
        // bytes of buffer remain after the header. Must Err, not silently
        // skip.
        let buf = [0x02u8, 0x01, 0x00, 50, 0x41, 0x42, 0x43, 0x44]; // 8 total
        let r = parse_dvcid_block(&buf);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail, .. })
                         if detail.contains("identifier_length")),
            "got {r:?}"
        );
    }

    #[test]
    fn parse_dvcid_block_rejects_partial_tail_descriptor() {
        // After one well-formed descriptor (4 byte header + 0 byte ident),
        // only 2 trailing bytes remain — a partial fragment.
        let buf = [0x02u8, 0x01, 0x00, 0x00, 0xaa, 0xbb];
        let r = parse_dvcid_block(&buf);
        assert!(
            matches!(r, Err(ScsiError::InvalidResponse { detail, .. })
                         if detail.contains("partial")),
            "got {r:?}"
        );
    }
}
