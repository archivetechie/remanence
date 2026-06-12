//! INITIALIZE ELEMENT STATUS (CDB `0x07`) — SMC-3 §6.4.
//!
//! Tells the changer to re-scan every element and re-derive its
//! internal state. Needed when the operator inserts a cartridge via
//! the front panel, or when discovery's snapshot is suspect for any
//! reason. The CDB has no data phase.
//!
//! The "with range" variant (`0x37`) is deliberately not implemented
//! — see `docs/layer2b-design.md` §3.2 for the rationale (single-
//! digit-second full rescan on 40-slot libraries is not worth the
//! extra surface).

/// SCSI opcode for INITIALIZE ELEMENT STATUS.
pub const OPCODE: u8 = 0x07;

/// Build the 6-byte INITIALIZE ELEMENT STATUS CDB.
pub fn build_cdb() -> [u8; 6] {
    [OPCODE, 0x00, 0x00, 0x00, 0x00, 0x00]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdb_matches_spec() {
        // SMC-3 Table 27.
        assert_eq!(build_cdb(), [0x07, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }
}
