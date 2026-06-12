//! READ BLOCK LIMITS (CDB `0x05`) — SSC-5 §6.3 / IBM LTO SCSI
//! Reference §5.2.17, Tables 77-79.
//!
//! **Scope of this module: MLOI=0 only — the "block-length data"
//! variant (Table 78).** READ BLOCK LIMITS has a `MLOI` bit in
//! CDB byte 1 bit 0 per Table 77:
//!
//! - `MLOI=0` returns the 6-byte block-length data of **Table 78**
//!   — granularity, maximum block length limit, minimum block
//!   length limit. This is what `build_cdb` emits and
//!   `parse_response` decodes. Layer 3a's `DriveHandle::read_config`
//!   in the `remanence-library` crate depends on it.
//! - `MLOI=1` returns the 20-byte maximum-logical-object-identifier
//!   data of **Table 79**. Not used by rem today; not implemented
//!   here. A future caller wanting the MLOI=1 variant should add
//!   `build_cdb_mloi(true)` + a separate parser rather than try
//!   to fold the two layouts into one type. Codex 20:17
//!   (idref=447d7348 Low) caught the earlier ambiguous naming
//!   that suggested the command was variant-free.
//!
//! Table 78 layout (the MLOI=0 response decoded here):
//!
//! - Byte 0 bits 0..4: `Granularity` — `2^G` is the minimum
//!   alignment for variable-block transfers (always 0 on LTO).
//! - Bytes 1..4: `MAXIMUM BLOCK LENGTH LIMIT` (3-byte big-endian).
//!   On LTO-9 Table 78 documents this as `0x80_0000` (8 MiB), even
//!   though §4.11 Note 15 says the drive may accept larger
//!   unencrypted block lengths up to `0xFF_FFFF` without reporting
//!   it. Layer 3a's `read_config()` stores the reported value
//!   verbatim.
//! - Bytes 4..6: `MINIMUM BLOCK LENGTH LIMIT` (2-byte big-endian).
//!
//! The CDB has no host-side data phase — the response is read back
//! via `execute_in` with a 6-byte buffer.

/// SCSI opcode for READ BLOCK LIMITS.
pub const OPCODE: u8 = 0x05;

/// Response length in bytes for the **MLOI=0** (block-length data)
/// variant — fixed by Table 78. The MLOI=1 variant returns 20
/// bytes per Table 79; that's a separate CDB this module does not
/// build.
pub const RESPONSE_LEN: u16 = 6;

/// Build the 6-byte READ BLOCK LIMITS CDB with `MLOI=0` — returns
/// Table 78 block-length data. Caller passes the response buffer
/// to [`parse_response`] for decoding. For MLOI=1 (Table 79
/// maximum-logical-object-identifier data) a separate CDB builder
/// will need to be added — not used by Layer 3a today.
pub fn build_cdb() -> [u8; 6] {
    [
        OPCODE, 0x00, // MLOI=0 (Table 78 block-length data)
        0x00, // reserved
        0x00, // reserved
        0x00, // reserved
        0x00, // control
    ]
}

/// Decoded READ BLOCK LIMITS response. All three fields come
/// straight from the on-wire bytes; the caller decides how to
/// interpret them against the drive's `INQUIRY` device type and
/// the spec-supported maximum from §4.11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLimits {
    /// Granularity (`2^G` minimum alignment for variable-block
    /// transfers). Always 0 on LTO.
    pub granularity: u8,
    /// Maximum logical block length the drive **reports** in
    /// READ BLOCK LIMITS. Note this is the *reported* value
    /// (Table 78 field); §4.11 Note 15 says the drive may accept
    /// larger unencrypted blocks up to `0xFF_FFFF` without
    /// reporting them.
    pub max_block_length: u32,
    /// Minimum logical block length the drive will accept.
    pub min_block_length: u16,
}

/// Parse the 6-byte READ BLOCK LIMITS response. Short responses
/// return `None`; callers map that to a transport-level error.
pub fn parse_response(buf: &[u8]) -> Option<BlockLimits> {
    if buf.len() < RESPONSE_LEN as usize {
        return None;
    }
    let granularity = buf[0] & 0x1F;
    let max_block_length = ((buf[1] as u32) << 16) | ((buf[2] as u32) << 8) | (buf[3] as u32);
    let min_block_length = ((buf[4] as u16) << 8) | (buf[5] as u16);
    Some(BlockLimits {
        granularity,
        max_block_length,
        min_block_length,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdb_is_six_zero_bytes_after_opcode() {
        assert_eq!(build_cdb(), [0x05, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn response_len_constant_is_six() {
        assert_eq!(RESPONSE_LEN, 6);
    }

    #[test]
    fn parse_response_lto9_default_max_block_length() {
        // IBM Table 78 documents the LTO-9 reported max as
        // 0x80_0000 (8 MiB).
        let mut buf = [0u8; 6];
        buf[0] = 0; // granularity 0
        buf[1] = 0x80;
        buf[2] = 0x00;
        buf[3] = 0x00;
        buf[4] = 0x00;
        buf[5] = 0x01;
        let parsed = parse_response(&buf).expect("parse ok");
        assert_eq!(parsed.granularity, 0);
        assert_eq!(parsed.max_block_length, 0x0080_0000);
        assert_eq!(parsed.min_block_length, 1);
    }

    #[test]
    fn parse_response_decodes_full_24bit_max() {
        // Verify the 24-bit MAX field decodes correctly at the
        // upper bound (codex 19:57 catch: §4.11 supported max).
        let buf = [0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x04];
        let parsed = parse_response(&buf).expect("parse ok");
        assert_eq!(parsed.max_block_length, 0x00FF_FFFF);
        assert_eq!(parsed.min_block_length, 4);
    }

    #[test]
    fn parse_response_returns_none_for_short_buffer() {
        let buf = [0u8; 5];
        assert!(parse_response(&buf).is_none());
    }

    #[test]
    fn parse_response_masks_granularity_to_5_bits() {
        // Per SSC-5: byte 0 bits 0..4 are GRANULARITY; bits 5..7
        // are reserved. Tolerate non-zero reserved bits.
        let buf = [0xFF, 0x00, 0x10, 0x00, 0x00, 0x01];
        let parsed = parse_response(&buf).expect("parse ok");
        assert_eq!(parsed.granularity, 0x1F);
    }
}
