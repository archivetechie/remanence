//! SSC LOAD / UNLOAD (CDB `0x1B`) — SSC-5 §6.7.
//!
//! Sent to a tape drive's `/dev/sgN` (not the changer's) to load the
//! cartridge that's been physically inserted into the bay, or to
//! release it before the changer can pluck it back out. The CDB has
//! no data phase.
//!
//! The default builder deliberately leaves the `IMMED` bit clear:
//! existing Layer 2b callers want completion before issuing a follow-up
//! MOVE MEDIUM. Readiness-aware callers may opt into `IMMED=1` and
//! poll TEST UNIT READY instead.

/// SCSI opcode for SSC LOAD / UNLOAD.
pub const OPCODE: u8 = 0x1B;

/// Build the 6-byte SSC LOAD/UNLOAD CDB. `load = true` issues LOAD;
/// `load = false` issues UNLOAD.
///
/// All other flag bits in byte 4 (`RETEN`, `EOT`, `HOLD`) are zero —
/// the defaults the tape archive workflow wants: don't retension,
/// don't seek to end-of-tape, don't hold the cartridge after unload.
pub fn build_cdb(load: bool) -> [u8; 6] {
    build_cdb_with_immed(load, false)
}

/// Build the 6-byte SSC LOAD/UNLOAD CDB with explicit control over
/// byte 1 bit 0 (`IMMED`).
pub fn build_cdb_with_immed(load: bool, immed: bool) -> [u8; 6] {
    [
        OPCODE,
        if immed { 0x01 } else { 0x00 }, // bit 0: IMMED
        0x00,                            // reserved
        0x00,                            // reserved
        if load { 0x01 } else { 0x00 },  // bit 0: LOAD
        0x00,                            // control
    ]
}

/// Build an immediate SSC LOAD CDB for media-readiness workflows.
pub fn build_immediate_load_cdb() -> [u8; 6] {
    build_cdb_with_immed(true, true)
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

    #[test]
    fn immediate_load_sets_byte_1_bit_0() {
        assert_eq!(
            build_immediate_load_cdb(),
            [0x1B, 0x01, 0x00, 0x00, 0x01, 0x00]
        );
    }
}
