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
    /// Fixed-format top-level VALID bit, or the VALID bit from a
    /// descriptor-format Information descriptor.
    pub valid: bool,
    /// FILEMARK flag.
    pub filemark: bool,
    /// End-of-medium flag.
    pub eom: bool,
    /// Incorrect-length-indicator flag.
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
        0x72 | 0x73 => {
            let (valid, filemark, eom, ili) = decode_descriptor_flags(sense);
            Some(DecodedSense {
                response_code,
                key: sense.get(1).copied()? & 0x0F,
                asc: sense.get(2).copied().unwrap_or(0),
                ascq: sense.get(3).copied().unwrap_or(0),
                valid,
                filemark,
                eom,
                ili,
            })
        }
        _ => None,
    }
}

/// Walk the bounded descriptor list and collect boundary flags shared with
/// fixed-format sense. Malformed trailing descriptors stop the walk while
/// preserving fields decoded from earlier complete descriptors.
fn decode_descriptor_flags(sense: &[u8]) -> (bool, bool, bool, bool) {
    const DESCRIPTOR_LIST_OFFSET: usize = 8;
    const INFORMATION_DESCRIPTOR: u8 = 0x00;
    const STREAM_COMMANDS_DESCRIPTOR: u8 = 0x04;

    let Some(&additional_sense_length) = sense.get(7) else {
        return (false, false, false, false);
    };
    let descriptor_list_end = DESCRIPTOR_LIST_OFFSET
        .saturating_add(usize::from(additional_sense_length))
        .min(sense.len());

    let mut valid = false;
    let mut filemark = false;
    let mut eom = false;
    let mut ili = false;
    let mut offset = DESCRIPTOR_LIST_OFFSET;

    while offset < descriptor_list_end {
        let Some(header) = sense.get(offset..offset.saturating_add(2)) else {
            break;
        };
        let descriptor_type = header[0];
        let descriptor_length = usize::from(header[1]).saturating_add(2);
        let Some(next_offset) = offset.checked_add(descriptor_length) else {
            break;
        };
        if next_offset > descriptor_list_end {
            break;
        }

        match descriptor_type {
            INFORMATION_DESCRIPTOR if descriptor_length >= 12 => {
                valid |= sense[offset + 2] & 0x80 != 0;
            }
            STREAM_COMMANDS_DESCRIPTOR if descriptor_length >= 4 => {
                let flags = sense[offset + 3];
                filemark |= flags & 0x80 != 0;
                eom |= flags & 0x40 != 0;
                ili |= flags & 0x20 != 0;
            }
            _ => {}
        }
        offset = next_offset;
    }

    (valid, filemark, eom, ili)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor_sense(response_code: u8, descriptors: &[u8]) -> Vec<u8> {
        let descriptor_length =
            u8::try_from(descriptors.len()).expect("synthetic descriptor list fits in u8");
        let mut sense = vec![0u8; 8 + descriptors.len()];
        sense[0] = response_code;
        sense[1] = 0x03;
        sense[2] = 0x11;
        sense[3] = 0x04;
        sense[7] = descriptor_length;
        sense[8..].copy_from_slice(descriptors);
        sense
    }

    fn stream_commands_descriptor(flags: u8) -> [u8; 4] {
        [0x04, 0x02, 0x00, flags]
    }

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
    fn descriptor_stream_commands_decodes_filemark_only() {
        let sense = descriptor_sense(0x72, &stream_commands_descriptor(0x80));
        let decoded = decode_sense(&sense).expect("decode descriptor FILEMARK");

        assert!(decoded.filemark);
        assert!(!decoded.eom);
        assert!(!decoded.ili);
    }

    #[test]
    fn descriptor_stream_commands_decodes_eom_only() {
        let sense = descriptor_sense(0x72, &stream_commands_descriptor(0x40));
        let decoded = decode_sense(&sense).expect("decode descriptor EOM");

        assert!(!decoded.filemark);
        assert!(decoded.eom);
        assert!(!decoded.ili);
    }

    #[test]
    fn descriptor_stream_commands_decodes_ili_only() {
        let sense = descriptor_sense(0x72, &stream_commands_descriptor(0x20));
        let decoded = decode_sense(&sense).expect("decode descriptor ILI");

        assert!(!decoded.filemark);
        assert!(!decoded.eom);
        assert!(decoded.ili);
    }

    #[test]
    fn descriptor_stream_commands_decodes_all_boundary_flags() {
        let sense = descriptor_sense(0x72, &stream_commands_descriptor(0xe0));
        let decoded = decode_sense(&sense).expect("decode all descriptor flags");

        assert!(decoded.filemark);
        assert!(decoded.eom);
        assert!(decoded.ili);
    }

    #[test]
    fn descriptor_stream_commands_decodes_no_boundary_flags() {
        let sense = descriptor_sense(0x72, &stream_commands_descriptor(0x00));
        let decoded = decode_sense(&sense).expect("decode empty descriptor flags");

        assert!(!decoded.filemark);
        assert!(!decoded.eom);
        assert!(!decoded.ili);
    }

    #[test]
    fn descriptor_walk_skips_unknown_descriptors_before_and_after_stream_commands() {
        let descriptors = [
            0x7e, 0x03, 0xaa, 0xbb, 0xcc, // unknown
            0x04, 0x02, 0x00, 0xa0, // FILEMARK + ILI
            0x7f, 0x01, 0xdd, // unknown
        ];
        let decoded =
            decode_sense(&descriptor_sense(0x72, &descriptors)).expect("decode descriptor list");

        assert!(decoded.filemark);
        assert!(!decoded.eom);
        assert!(decoded.ili);
    }

    #[test]
    fn descriptor_information_valid_bit_populates_common_valid_field() {
        let information_descriptor = [
            0x00, 0x0a, 0x80, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
        ];
        let decoded = decode_sense(&descriptor_sense(0x72, &information_descriptor))
            .expect("decode Information descriptor");

        assert!(decoded.valid);
        assert!(!decoded.filemark);
    }

    #[test]
    fn descriptor_walk_stops_cleanly_when_buffer_ends_mid_descriptor() {
        let sense = descriptor_sense(0x72, &[0x04, 0x02, 0x00]);
        let decoded = decode_sense(&sense).expect("truncated descriptor remains decodable");

        assert!(!decoded.valid);
        assert!(!decoded.filemark);
        assert!(!decoded.eom);
        assert!(!decoded.ili);
    }

    #[test]
    fn descriptor_walk_stops_cleanly_when_additional_length_overruns_buffer() {
        let descriptors = [
            0x04, 0x02, 0x00, 0x40, // complete EOM descriptor
            0x7e, 0xff, 0xaa, // lying trailing descriptor
        ];
        let decoded =
            decode_sense(&descriptor_sense(0x72, &descriptors)).expect("decode safe prefix");

        assert!(!decoded.filemark);
        assert!(decoded.eom);
        assert!(!decoded.ili);
    }

    #[test]
    fn deferred_descriptor_decodes_boundary_flags_like_current_descriptor() {
        let descriptors = [
            0x00, 0x0a, 0x80, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07,
            0x08, // Information descriptor with VALID
            0x04, 0x02, 0x00, 0xe0, // Stream Commands descriptor
        ];
        let current =
            decode_sense(&descriptor_sense(0x72, &descriptors)).expect("decode current descriptor");
        let deferred = decode_sense(&descriptor_sense(0x73, &descriptors))
            .expect("decode deferred descriptor");

        assert_eq!(deferred.key, current.key);
        assert_eq!(deferred.asc, current.asc);
        assert_eq!(deferred.ascq, current.ascq);
        assert_eq!(deferred.valid, current.valid);
        assert_eq!(deferred.filemark, current.filemark);
        assert_eq!(deferred.eom, current.eom);
        assert_eq!(deferred.ili, current.ili);
        assert!(deferred.is_deferred());
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
