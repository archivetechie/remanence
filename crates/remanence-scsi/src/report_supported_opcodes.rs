//! REPORT SUPPORTED OPERATION CODES (`A3h/0Ch`) — SPC-5 §6.34.
//!
//! The minimal probe read ordering needs: ask the device whether one
//! specific `(operation code, service action)` pair is implemented, and
//! decode the one_command parameter data's SUPPORT field. Nothing else
//! from RSOC — the all-commands listing, RCTD timeout descriptors — is
//! built, because nothing in rem consumes it.
//!
//! This exists so capability detection can ask the drive itself instead
//! of inferring support from an INQUIRY-derived generation table
//! (design-read-ordering.md, P2): a drive that surprises us then
//! degrades instead of erroring. First consumer:
//! [`crate::read_end_of_wrap_position::capability_probe_cdb`].

use crate::error::ScsiError;

/// SCSI opcode — MAINTENANCE IN.
pub const OPCODE: u8 = 0xA3;

/// MAINTENANCE IN service action for REPORT SUPPORTED OPERATION CODES.
pub const SERVICE_ACTION: u8 = 0x0C;

/// REPORTING OPTIONS `010b`: return one_command parameter data for the
/// requested operation code *and* service action.
pub const REPORTING_OPTIONS_ONE_COMMAND_SERVICE_ACTION: u8 = 0b010;

/// Length of the one_command parameter data header (reserved byte,
/// CTDP/SUPPORT byte, 2-byte CDB size). CDB usage data follows.
pub const ONE_COMMAND_HEADER_LEN: usize = 4;

/// Recommended allocation length: header plus usage data for the
/// longest CDBs, with slack for a CTDP timeouts descriptor should a
/// device append one.
pub const ALLOC_LEN: u32 = 64;

/// Build a 12-byte RSOC CDB probing one `(opcode, service action)`
/// pair, one_command format, RCTD clear.
///
/// The REQUESTED SERVICE ACTION field is 16 bits on the wire even
/// though MAINTENANCE IN encodes only 5 service-action bits in its own
/// CDBs. IBM's service action *qualifiers* (such as READ END OF WRAP
/// POSITION's `45h`) have no field here and cannot be probed — the
/// pair is the finest granularity RSOC offers.
pub fn build_cdb(opcode: u8, service_action: u16, alloc_len: u32) -> [u8; 12] {
    let sa = service_action.to_be_bytes();
    let alloc = alloc_len.to_be_bytes();
    [
        OPCODE,
        SERVICE_ACTION & 0x1F,
        REPORTING_OPTIONS_ONE_COMMAND_SERVICE_ACTION, // RCTD=0
        opcode,
        sa[0],
        sa[1],
        alloc[0],
        alloc[1],
        alloc[2],
        alloc[3],
        0x00, // reserved
        0x00, // control
    ]
}

/// The device's answer about one `(opcode, service action)` pair —
/// the SUPPORT field of the one_command parameter data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpcodeSupport {
    /// SUPPORT `000b`: data about the command is not currently
    /// available. Indeterminate, not a "no" — the caller decides
    /// whether to retry or degrade.
    DataUnavailable,
    /// SUPPORT `001b`: the device server does not support the command.
    NotSupported,
    /// SUPPORT `011b`: supported in conformance with a standard.
    Supported,
    /// SUPPORT `101b`: supported in a vendor-specific manner.
    SupportedVendorSpecific,
}

impl OpcodeSupport {
    /// Whether the device affirmed support (standard or
    /// vendor-specific).
    pub fn is_supported(self) -> bool {
        matches!(
            self,
            OpcodeSupport::Supported | OpcodeSupport::SupportedVendorSpecific
        )
    }
}

/// Parse one_command parameter data into the SUPPORT answer.
///
/// The buffer must cover the 4-byte header and the CDB usage data the
/// header declares; reserved SUPPORT values are rejected rather than
/// guessed at. Trailing bytes beyond the declared usage data (such as
/// a timeouts descriptor from a device that ignores RCTD=0) are
/// ignored.
pub fn parse_one_command_response(buf: &[u8]) -> Result<OpcodeSupport, ScsiError> {
    if buf.len() < ONE_COMMAND_HEADER_LEN {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: ONE_COMMAND_HEADER_LEN,
        });
    }
    let cdb_size = usize::from(u16::from_be_bytes([buf[2], buf[3]]));
    let total = ONE_COMMAND_HEADER_LEN + cdb_size;
    if buf.len() < total {
        return Err(ScsiError::Truncated {
            got: buf.len(),
            need: total,
        });
    }
    match buf[1] & 0b0000_0111 {
        0b000 => Ok(OpcodeSupport::DataUnavailable),
        0b001 => Ok(OpcodeSupport::NotSupported),
        0b011 => Ok(OpcodeSupport::Supported),
        0b101 => Ok(OpcodeSupport::SupportedVendorSpecific),
        _ => Err(ScsiError::InvalidResponse {
            offset: 1,
            detail: "reserved SUPPORT value in REPORT SUPPORTED OPERATION CODES response",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_command(support: u8, cdb_usage: &[u8]) -> Vec<u8> {
        let mut buf = vec![0x00, support & 0x07];
        buf.extend_from_slice(&(cdb_usage.len() as u16).to_be_bytes());
        buf.extend_from_slice(cdb_usage);
        buf
    }

    #[test]
    fn cdb_shape_for_opcode_and_service_action() {
        let cdb = build_cdb(0xA3, 0x001F, 64);
        assert_eq!(
            cdb,
            [
                0xA3, // MAINTENANCE IN
                0x0C, // RSOC service action
                0x02, // RCTD=0, reporting options 010b
                0xA3, // requested operation code
                0x00, 0x1F, // requested service action, big-endian
                0x00, 0x00, 0x00, 0x40, // allocation length, big-endian
                0x00, // reserved
                0x00, // control
            ]
        );
    }

    #[test]
    fn support_states_decode() {
        let usage = [0u8; 12];
        assert_eq!(
            parse_one_command_response(&one_command(0b011, &usage)).expect("standard"),
            OpcodeSupport::Supported
        );
        assert_eq!(
            parse_one_command_response(&one_command(0b101, &usage)).expect("vendor"),
            OpcodeSupport::SupportedVendorSpecific
        );
        assert_eq!(
            parse_one_command_response(&one_command(0b001, &[])).expect("unsupported"),
            OpcodeSupport::NotSupported
        );
        assert_eq!(
            parse_one_command_response(&one_command(0b000, &[])).expect("unavailable"),
            OpcodeSupport::DataUnavailable
        );
        assert!(OpcodeSupport::Supported.is_supported());
        assert!(OpcodeSupport::SupportedVendorSpecific.is_supported());
        assert!(!OpcodeSupport::NotSupported.is_supported());
        assert!(!OpcodeSupport::DataUnavailable.is_supported());
    }

    #[test]
    fn reserved_support_values_are_rejected() {
        for reserved in [0b010, 0b100, 0b110, 0b111] {
            let err =
                parse_one_command_response(&one_command(reserved, &[])).expect_err("reserved");
            assert!(matches!(err, ScsiError::InvalidResponse { offset: 1, .. }));
        }
    }

    #[test]
    fn truncated_header_is_rejected() {
        let err = parse_one_command_response(&[0x00, 0x03, 0x00]).expect_err("short");
        assert!(matches!(err, ScsiError::Truncated { got: 3, need: 4 }));
    }

    #[test]
    fn declared_cdb_usage_beyond_buffer_is_rejected() {
        // Header says 12 bytes of CDB usage data follow; only 4 do.
        let mut buf = one_command(0b011, &[0u8; 12]);
        buf.truncate(8);
        let err = parse_one_command_response(&buf).expect_err("usage truncated");
        assert!(matches!(err, ScsiError::Truncated { got: 8, need: 16 }));
    }

    #[test]
    fn ctdp_bit_does_not_disturb_support_decode() {
        // Byte 1 bit 7 is CTDP; a device setting it appends a timeouts
        // descriptor after the usage data. The SUPPORT decode must
        // mask it off and tolerate the trailing descriptor.
        let mut buf = one_command(0b011, &[0u8; 12]);
        buf[1] |= 0x80;
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(
            parse_one_command_response(&buf).expect("ctdp tolerated"),
            OpcodeSupport::Supported
        );
    }
}
