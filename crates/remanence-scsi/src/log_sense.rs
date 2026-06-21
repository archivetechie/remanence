//! LOG SENSE (CDB `0x4D`) helpers for TapeAlert page `0x2E`.
//!
//! Remanence currently uses this module for the current-cumulative TapeAlert
//! log page. The parser is deliberately length-driven: each log parameter's
//! length byte controls how far the parser advances, so short or vendor-shaped
//! pages fail closed instead of assuming the canonical five-byte TapeAlert
//! parameter stride.

use std::collections::BTreeSet;

use crate::error::ScsiError;

/// SCSI opcode for LOG SENSE(10).
pub const OPCODE: u8 = 0x4D;
/// LOG SENSE page code for the TapeAlert page.
pub const PAGE_TAPE_ALERT: u8 = 0x2E;
/// LOG SENSE page-control value for current cumulative values.
pub const PAGE_CONTROL_CURRENT_CUMULATIVE: u8 = 0x01;
/// Canonical TapeAlert flag count.
pub const TAPE_ALERT_FLAG_COUNT: u8 = 64;
/// Canonical byte count for 64 one-byte TapeAlert parameters.
pub const TAPE_ALERT_PARAMETER_BYTES: u16 = 320;
/// Canonical TapeAlert LOG SENSE page response length.
pub const TAPE_ALERT_RESPONSE_LEN: u16 = 4 + TAPE_ALERT_PARAMETER_BYTES;

/// Build a LOG SENSE(10) CDB for `page_code`.
///
/// Byte 2 carries PC=01b (current cumulative) in bits 7..6 and the page code
/// in bits 5..0. Bytes 7..8 carry the allocation length in big-endian order.
pub fn build_cdb(page_code: u8, alloc_len: u16) -> [u8; 10] {
    let [alloc_hi, alloc_lo] = alloc_len.to_be_bytes();
    [
        OPCODE,
        0x00,
        (PAGE_CONTROL_CURRENT_CUMULATIVE << 6) | (page_code & 0x3f),
        0x00,
        0x00,
        0x00,
        0x00,
        alloc_hi,
        alloc_lo,
        0x00,
    ]
}

/// Build a LOG SENSE(10) CDB for the TapeAlert page.
pub fn build_tape_alert_cdb(alloc_len: u16) -> [u8; 10] {
    build_cdb(PAGE_TAPE_ALERT, alloc_len)
}

/// Decoded active TapeAlert flags.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TapeAlerts {
    active: BTreeSet<u8>,
}

impl TapeAlerts {
    /// Create an empty TapeAlert set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a TapeAlert set from flag numbers.
    pub fn from_flags(flags: impl IntoIterator<Item = u8>) -> Self {
        let active = flags
            .into_iter()
            .filter(|flag| (1..=TAPE_ALERT_FLAG_COUNT).contains(flag))
            .collect();
        Self { active }
    }

    /// Whether `flag` is currently active.
    pub fn is_set(&self, flag: u8) -> bool {
        self.active.contains(&flag)
    }

    /// Active flag numbers.
    pub fn active(&self) -> &BTreeSet<u8> {
        &self.active
    }
}

/// Human-readable name for a known LTO TapeAlert flag.
pub fn flag_name(flag: u8) -> Option<&'static str> {
    Some(match flag {
        7 => "media life",
        15 => "cartridge memory chip failure",
        16 => "forced eject",
        18 => "tape directory corrupted in cartridge memory",
        19 => "near media life",
        20 => "clean now",
        21 => "clean periodic",
        22 => "expired cleaning cartridge",
        26 => "cooling fan failure",
        30 => "hardware A",
        31 => "hardware B",
        34 => "firmware download failure",
        36 => "drive temperature",
        37 => "drive voltage",
        38 => "predictive failure",
        51 => "tape directory invalid at unload",
        52 => "tape system area write failure",
        53 => "tape system area read failure",
        58 => "microcode panic",
        61 => "encryption policy violation",
        _ => return None,
    })
}

/// Parse a TapeAlert LOG SENSE page into active flag numbers.
pub fn parse_response(buf: &[u8]) -> Result<TapeAlerts, ScsiError> {
    if buf.len() < 4 {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: 4,
        });
    }
    if buf[0] & 0x3f != PAGE_TAPE_ALERT {
        return Err(ScsiError::InvalidResponse {
            offset: 0,
            detail: "LOG SENSE page is not TapeAlert 0x2e",
        });
    }
    let page_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    let total = 4usize
        .checked_add(page_len)
        .ok_or(ScsiError::InvalidResponse {
            offset: 2,
            detail: "LOG SENSE page length overflow",
        })?;
    if buf.len() < total {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: total,
        });
    }

    let mut active = BTreeSet::new();
    let mut offset = 4usize;
    while offset < total {
        let header_end = offset.checked_add(4).ok_or(ScsiError::InvalidResponse {
            offset,
            detail: "LOG parameter offset overflow",
        })?;
        if header_end > total {
            return Err(ScsiError::Truncated {
                got: total.saturating_sub(offset),
                need: 4,
            });
        }
        let code = u16::from_be_bytes([buf[offset], buf[offset + 1]]);
        let len = buf[offset + 3] as usize;
        let value_end = header_end
            .checked_add(len)
            .ok_or(ScsiError::InvalidResponse {
                offset: offset + 3,
                detail: "LOG parameter length overflow",
            })?;
        if value_end > total {
            return Err(ScsiError::Truncated {
                got: total.saturating_sub(header_end),
                need: len,
            });
        }
        if (1..=u16::from(TAPE_ALERT_FLAG_COUNT)).contains(&code)
            && buf[header_end..value_end].iter().any(|value| *value != 0)
        {
            active.insert(code as u8);
        }
        offset = value_end;
    }

    Ok(TapeAlerts { active })
}

/// Synthesize the canonical 324-byte TapeAlert page.
pub fn synthesize_tape_alert_page(flags: &BTreeSet<u8>) -> Vec<u8> {
    let mut page = Vec::with_capacity(TAPE_ALERT_RESPONSE_LEN as usize);
    page.push(PAGE_TAPE_ALERT);
    page.push(0x00);
    page.extend_from_slice(&TAPE_ALERT_PARAMETER_BYTES.to_be_bytes());
    for flag in 1..=TAPE_ALERT_FLAG_COUNT {
        page.push(0x00);
        page.push(flag);
        page.push(0x00);
        page.push(0x01);
        page.push(u8::from(flags.contains(&flag)));
    }
    page
}

/// Set one flag in a mutable canonical TapeAlert page.
pub fn set_tape_alert_flag(page: &mut [u8], flag: u8) -> bool {
    if !(1..=TAPE_ALERT_FLAG_COUNT).contains(&flag) || page.len() < 4 {
        return false;
    }
    let page_len = u16::from_be_bytes([page[2], page[3]]) as usize;
    let Some(total) = 4usize.checked_add(page_len) else {
        return false;
    };
    if page.len() < total {
        return false;
    }
    let mut offset = 4usize;
    while offset < total {
        let Some(header_end) = offset.checked_add(4) else {
            return false;
        };
        if header_end > total {
            return false;
        }
        let code = u16::from_be_bytes([page[offset], page[offset + 1]]);
        let len = page[offset + 3] as usize;
        let Some(value_end) = header_end.checked_add(len) else {
            return false;
        };
        if value_end > total {
            return false;
        }
        if code == u16::from(flag) && len > 0 {
            page[header_end] = 1;
            return true;
        }
        offset = value_end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tape_alert_cdb_sets_current_cumulative_page() {
        assert_eq!(
            build_tape_alert_cdb(TAPE_ALERT_RESPONSE_LEN),
            [0x4d, 0, 0x6e, 0, 0, 0, 0, 0x01, 0x44, 0]
        );
    }

    #[test]
    fn synthetic_tape_alert_page_round_trips() {
        let flags = BTreeSet::from([7, 19, 58]);
        let page = synthesize_tape_alert_page(&flags);
        assert_eq!(page.len(), TAPE_ALERT_RESPONSE_LEN as usize);

        let parsed = parse_response(&page).expect("parse page");
        assert_eq!(parsed.active(), &flags);
        assert!(parsed.is_set(7));
        assert_eq!(flag_name(20), Some("clean now"));
    }

    #[test]
    fn parse_honors_parameter_length() {
        let page = [PAGE_TAPE_ALERT, 0, 0, 6, 0, 7, 0, 2, 0, 1];
        let parsed = parse_response(&page).expect("parse alternate length");
        assert_eq!(parsed.active(), &BTreeSet::from([7]));
    }

    #[test]
    fn parse_rejects_truncated_parameter_without_panic() {
        let page = [PAGE_TAPE_ALERT, 0, 0, 5, 0, 7, 0, 2, 1];
        assert!(matches!(
            parse_response(&page),
            Err(ScsiError::Truncated { .. })
        ));
    }

    #[test]
    fn parse_rejects_wrong_page() {
        let page = [0x2f, 0, 0, 0];
        assert!(matches!(
            parse_response(&page),
            Err(ScsiError::InvalidResponse { offset: 0, .. })
        ));
    }

    #[test]
    fn set_flag_updates_canonical_page() {
        let mut page = synthesize_tape_alert_page(&BTreeSet::new());
        assert!(set_tape_alert_flag(&mut page, 20));
        let parsed = parse_response(&page).expect("parse page");
        assert_eq!(parsed.active(), &BTreeSet::from([20]));
    }
}
