//! REQUEST SENSE data decoding helpers.
//!
//! Remanence mostly receives fixed-format sense from LTO drives, but Linux
//! hosts and devices can be configured for descriptor-format sense. Keep the
//! key/ASC/ASCQ offset rules here so upper layers do not duplicate byte math.

/// Parsed sense fields common to fixed-format and descriptor-format sense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedSense {
    /// Response code with the VALID bit masked off.
    pub response_code: u8,
    /// Sense key.
    pub key: u8,
    /// Additional Sense Code.
    pub asc: u8,
    /// Additional Sense Code Qualifier.
    pub ascq: u8,
    /// Fixed-format sense VALID bit. Descriptor-format sense has no
    /// equivalent top-level bit, so this is false there.
    pub valid: bool,
    /// Fixed-format FILEMARK flag.
    pub filemark: bool,
    /// Fixed-format EOM flag.
    pub eom: bool,
    /// Fixed-format ILI flag.
    pub ili: bool,
}

impl DecodedSense {
    /// True when the sense payload uses current fixed format (0x70).
    pub fn is_fixed_format(self) -> bool {
        self.response_code == 0x70
    }

    /// True when the sense payload uses current descriptor format (0x72).
    pub fn is_descriptor_format(self) -> bool {
        self.response_code == 0x72
    }

    /// True when the sense payload reports deferred sense (0x71/0x73).
    pub fn is_deferred(self) -> bool {
        matches!(self.response_code, 0x71 | 0x73)
    }
}

/// Decode common sense fields from fixed-format (0x70/0x71) or
/// descriptor-format (0x72/0x73) sense bytes.
pub fn decode_sense(sense: &[u8]) -> Option<DecodedSense> {
    let byte0 = *sense.first()?;
    let response_code = byte0 & 0x7F;
    match response_code {
        0x70 | 0x71 => {
            let byte2 = *sense.get(2)?;
            Some(DecodedSense {
                response_code,
                key: byte2 & 0x0F,
                asc: sense.get(12).copied().unwrap_or(0),
                ascq: sense.get(13).copied().unwrap_or(0),
                valid: (byte0 & 0x80) != 0,
                filemark: (byte2 & 0x80) != 0,
                eom: (byte2 & 0x40) != 0,
                ili: (byte2 & 0x20) != 0,
            })
        }
        0x72 | 0x73 => Some(DecodedSense {
            response_code,
            key: sense.get(1).copied()? & 0x0F,
            asc: sense.get(2).copied().unwrap_or(0),
            ascq: sense.get(3).copied().unwrap_or(0),
            valid: false,
            filemark: false,
            eom: false,
            ili: false,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fixed_format_sense() {
        let mut sense = vec![0u8; 18];
        sense[0] = 0xF0;
        sense[2] = 0xE3;
        sense[12] = 0x11;
        sense[13] = 0x22;

        let decoded = decode_sense(&sense).expect("decode fixed");

        assert_eq!(decoded.response_code, 0x70);
        assert_eq!(decoded.key, 0x03);
        assert_eq!(decoded.asc, 0x11);
        assert_eq!(decoded.ascq, 0x22);
        assert!(decoded.valid);
        assert!(decoded.filemark);
        assert!(decoded.eom);
        assert!(decoded.ili);
        assert!(decoded.is_fixed_format());
    }

    #[test]
    fn decodes_descriptor_format_sense() {
        let sense = [0x72, 0x03, 0x11, 0x04];

        let decoded = decode_sense(&sense).expect("decode descriptor");

        assert_eq!(decoded.response_code, 0x72);
        assert_eq!(decoded.key, 0x03);
        assert_eq!(decoded.asc, 0x11);
        assert_eq!(decoded.ascq, 0x04);
        assert!(decoded.is_descriptor_format());
        assert!(!decoded.valid);
        assert!(!decoded.filemark);
    }

    #[test]
    fn deferred_fixed_sense_is_not_current_fixed_format() {
        let mut sense = vec![0u8; 18];
        sense[0] = 0xF1;
        sense[2] = 0x40;

        let decoded = decode_sense(&sense).expect("decode deferred fixed");

        assert_eq!(decoded.response_code, 0x71);
        assert!(!decoded.is_fixed_format());
        assert!(decoded.is_deferred());
    }

    #[test]
    fn deferred_descriptor_sense_is_not_current_descriptor_format() {
        let sense = [0x73, 0x00, 0x00, 0x00];

        let decoded = decode_sense(&sense).expect("decode deferred descriptor");

        assert_eq!(decoded.response_code, 0x73);
        assert!(!decoded.is_descriptor_format());
        assert!(decoded.is_deferred());
    }

    #[test]
    fn rejects_unknown_response_code() {
        assert!(decode_sense(&[0x7f, 0, 0, 0]).is_none());
    }
}
