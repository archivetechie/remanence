//! REWIND (CDB `0x01`) — SSC-5 §6.4.
//!
//! Repositions the tape to beginning-of-partition (BOT) on partition 0.
//! Issued to a tape drive's `/dev/sgN`. The CDB has no data phase.
//!
//! The `IMMED` bit is deliberately *not* set: rem's callers want
//! completion before they trust the position state to be BOT — the
//! point of rewinding is usually to position before a write session
//! starts at LBA 0.

/// SCSI opcode for REWIND.
pub const OPCODE: u8 = 0x01;

/// Build the 6-byte REWIND CDB.
pub fn build_cdb() -> [u8; 6] {
    [
        OPCODE, 0x00, // IMMED=0 — wait for BOT
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdb_shape() {
        assert_eq!(build_cdb(), [0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }
}
