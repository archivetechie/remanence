//! SSC LOAD / UNLOAD (CDB `0x1B`) — SSC-5 §6.7.
//!
//! Sent to a tape drive's `/dev/sgN` (not the changer's) to load the
//! cartridge that's been physically inserted into the bay, or to
//! release it before the changer can pluck it back out. The CDB has
//! no data phase.
//!
//! The `IMMED` bit (return as soon as the operation starts, rather
//! than when it completes) is deliberately *not* set: Layer 2b's
//! callers want completion before issuing the follow-up MOVE MEDIUM.

/// SCSI opcode for SSC LOAD / UNLOAD.
pub const OPCODE: u8 = 0x1B;

/// Build the 6-byte SSC LOAD/UNLOAD CDB. `load = true` issues LOAD;
/// `load = false` issues UNLOAD.
///
/// All other flag bits in byte 4 (`RETEN`, `EOT`, `HOLD`) are zero —
/// the defaults the tape archive workflow wants: don't retension,
/// don't seek to end-of-tape, don't hold the cartridge after unload.
pub fn build_cdb(load: bool) -> [u8; 6] {
    [
        OPCODE,
        0x00,                           // IMMED=0 — wait for completion
        0x00,                           // reserved
        0x00,                           // reserved
        if load { 0x01 } else { 0x00 }, // bit 0: LOAD
        0x00,                           // control
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_sets_byte_4_bit_0() {
        assert_eq!(build_cdb(true), [0x1B, 0x00, 0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn unload_clears_byte_4() {
        assert_eq!(build_cdb(false), [0x1B, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }
}
