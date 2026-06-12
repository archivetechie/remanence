//! MOVE MEDIUM (CDB `0xA5`) — SMC-3 §6.10.
//!
//! Moves a single piece of media from `src` to `dst`, using the medium
//! transport (robot) at address `robot`. The CDB has no data phase —
//! issue it through [`crate::sg_io::execute_none`] (Layer 1) or
//! `SgTransport::execute_none` (Layer 2).
//!
//! Layer 2b composes higher-level operations (slot↔drive load/unload,
//! IE-port import/export) over this single primitive.

/// SCSI opcode for MOVE MEDIUM.
pub const OPCODE: u8 = 0xA5;

/// Build the 12-byte MOVE MEDIUM CDB.
///
/// - `robot` — medium-transport element address. For every library
///   Remanence supports today this is `0x0000`
///   (`library.layout.robot_address`).
/// - `src` — source element address (slot, IE port, or drive bay).
/// - `dst` — destination element address.
/// - `invert` — flip the cartridge mid-move. For LTO this is always
///   `false`; the bit exists for two-sided media SMC supports but
///   tape libraries don't use.
pub fn build_cdb(robot: u16, src: u16, dst: u16, invert: bool) -> [u8; 12] {
    [
        OPCODE,
        0x00, // reserved
        (robot >> 8) as u8,
        (robot & 0xff) as u8,
        (src >> 8) as u8,
        (src & 0xff) as u8,
        (dst >> 8) as u8,
        (dst & 0xff) as u8,
        0x00, // reserved
        0x00, // reserved
        if invert { 0x01 } else { 0x00 },
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_robot_zero_simple_slot_to_drive() {
        // mtx -f /dev/sch0 load 1 0  →  src=0x03e9 dst=0x0001 (typical HPE)
        let cdb = build_cdb(0x0000, 0x03e9, 0x0001, false);
        assert_eq!(
            cdb,
            [0xA5, 0x00, 0x00, 0x00, 0x03, 0xe9, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn builds_quadstor_layout_bay_to_slot() {
        // QuadStor uses drive bay 0x0100..0x0103, slot 0x0400..0x0427.
        // Round-trip: bay 0x0100 back to slot 0x0400.
        let cdb = build_cdb(0x0000, 0x0100, 0x0400, false);
        assert_eq!(
            cdb,
            [0xA5, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn invert_flag_sets_byte_10_bit_0() {
        let cdb = build_cdb(0x0000, 0x0001, 0x0002, true);
        assert_eq!(cdb[10], 0x01);
        // Other bytes match the non-invert layout.
        let cdb_off = build_cdb(0x0000, 0x0001, 0x0002, false);
        assert_eq!(cdb[..10], cdb_off[..10]);
        assert_eq!(cdb[11], cdb_off[11]);
    }

    #[test]
    fn opcode_is_first_byte() {
        let cdb = build_cdb(0x1234, 0x5678, 0x9abc, false);
        assert_eq!(cdb[0], 0xA5);
        // Big-endian fields land in the right positions.
        assert_eq!(&cdb[2..4], &[0x12, 0x34]);
        assert_eq!(&cdb[4..6], &[0x56, 0x78]);
        assert_eq!(&cdb[6..8], &[0x9a, 0xbc]);
    }
}
