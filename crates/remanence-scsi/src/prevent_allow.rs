//! PREVENT / ALLOW MEDIUM REMOVAL (CDB `0x1E`) — SPC-5 §6.16.
//!
//! Locks (`prevent=true`) or unlocks (`prevent=false`) operator
//! removal of media. On a tape library this gates the front-panel
//! eject button and any operator-initiated mailslot eject. Used by
//! Layer 2b's `RemovalLockGuard` (see `docs/layer2b-design.md` §3.3)
//! around multi-step sequences.

/// SCSI opcode for PREVENT/ALLOW MEDIUM REMOVAL.
pub const OPCODE: u8 = 0x1E;

/// Build the 6-byte PREVENT/ALLOW MEDIUM REMOVAL CDB.
///
/// `prevent = true` sets byte-4 bit-0 (LU-only lock — the most
/// permissive form most targets accept). `prevent = false` clears the
/// byte, asking the target to allow removal again.
pub fn build_cdb(prevent: bool) -> [u8; 6] {
    [
        OPCODE,
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        if prevent { 0x01 } else { 0x00 },
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prevent_sets_byte_4_bit_0() {
        assert_eq!(build_cdb(true), [0x1E, 0x00, 0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn allow_clears_byte_4() {
        assert_eq!(build_cdb(false), [0x1E, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }
}
