//! RAO 128-byte plaintext scalar envelope header.

use sha2::{Digest, Sha256};

use crate::error::{RaoAeadError, Result};

/// Length in bytes of a RAO encrypted-envelope header.
pub const RAO_HEADER_LEN: usize = 128;
/// Maximum encrypted metadata frame length, including the AEAD tag.
pub const RAO_MAX_METADATA_FRAME_LEN: u64 = 16 * 1024 * 1024;
/// Minimum encrypted metadata frame length, including the AEAD tag.
pub const RAO_METADATA_FRAME_MIN_LEN: u64 = 17;
/// Completion footer for a successfully sealed encrypted RAO object.
pub const RAO_FOOTER: &[u8; 16] = b"RAO1_STREAM_END.";

const MAGIC: &[u8; 4] = b"RAO1";
/// RAO 2.0 HPKE Base X-Wing/HKDF-SHA256/ChaCha20-Poly1305 wrapping suite.
pub const RAO_WRAP_SUITE_XWING: u8 = 0x02;
const SUITE_ID_HKDF_SHA256_CHACHA20POLY1305: u8 = 0x01;
const ZERO_16: [u8; 16] = [0; 16];

/// Parsed RAO encrypted-envelope header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaoHeader {
    /// Envelope format version (always 2 for accepted objects).
    pub format_version: u8,
    /// Object body block size and AEAD plaintext chunk size.
    pub chunk_size: u32,
    /// Deterministically derived per-object HKDF salt.
    pub hkdf_salt: [u8; 16],
    /// Encrypted metadata frame length, including its 16-byte AEAD tag.
    pub metadata_frame_len: u64,
    /// Inner canonical RAO object id.
    pub object_id: String,
    /// DEK wrapping suite.
    pub wrap_suite: u8,
    /// Plaintext key-frame length.
    pub key_frame_len: u32,
}

impl RaoHeader {
    /// Construct and validate an envelope-mode scalar header.
    pub fn new_envelope(
        chunk_size: u32,
        hkdf_salt: [u8; 16],
        metadata_frame_len: u64,
        object_id: impl Into<String>,
        key_frame_len: u32,
    ) -> Result<Self> {
        let header = Self {
            format_version: 2,
            chunk_size,
            hkdf_salt,
            metadata_frame_len,
            object_id: object_id.into(),
            wrap_suite: RAO_WRAP_SUITE_XWING,
            key_frame_len,
        };
        header.validate()?;
        Ok(header)
    }

    /// Parse a serialized 128-byte header.
    pub fn parse(bytes: &[u8; RAO_HEADER_LEN]) -> Result<Self> {
        if &bytes[0..4] != MAGIC {
            return Err(RaoAeadError::InvalidMagicBytes);
        }
        let header_len = u16::from_be_bytes([bytes[4], bytes[5]]);
        if header_len != RAO_HEADER_LEN as u16 {
            return Err(RaoAeadError::InvalidHeaderLength);
        }
        let format_version = bytes[6];
        if format_version != 2 {
            return Err(RaoAeadError::UnsupportedFormatVersion);
        }
        if bytes[7] != SUITE_ID_HKDF_SHA256_CHACHA20POLY1305 {
            return Err(RaoAeadError::InvalidSuite);
        }

        let chunk_size = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let flags = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        if flags != 0 {
            return Err(RaoAeadError::ReservedBytesNotZero);
        }

        if bytes[0x10..0x20].iter().any(|byte| *byte != 0) {
            return Err(RaoAeadError::ReservedBytesNotZero);
        }
        let mut hkdf_salt = [0u8; 16];
        hkdf_salt.copy_from_slice(&bytes[0x20..0x30]);
        let metadata_frame_len = u64::from_be_bytes([
            bytes[0x30],
            bytes[0x31],
            bytes[0x32],
            bytes[0x33],
            bytes[0x34],
            bytes[0x35],
            bytes[0x36],
            bytes[0x37],
        ]);
        if bytes[0x39..0x3c].iter().any(|byte| *byte != 0) {
            return Err(RaoAeadError::ReservedBytesNotZero);
        }
        let wrap_suite = bytes[0x38];
        validate_wrap_suite(wrap_suite)?;
        let key_frame_len = u32::from_be_bytes(bytes[0x3c..0x40].try_into().expect("fixed slice"));

        let object_id = decode_object_id_field(&bytes[0x40..0x80])?;
        let header = Self {
            format_version,
            chunk_size,
            hkdf_salt,
            metadata_frame_len,
            object_id,
            wrap_suite,
            key_frame_len,
        };
        header.validate()?;
        Ok(header)
    }

    /// Serialize this header in canonical big-endian RAO wire form.
    pub fn serialize(&self) -> Result<[u8; RAO_HEADER_LEN]> {
        self.validate()?;

        let mut bytes = [0u8; RAO_HEADER_LEN];
        bytes[0..4].copy_from_slice(MAGIC);
        bytes[4..6].copy_from_slice(&(RAO_HEADER_LEN as u16).to_be_bytes());
        bytes[6] = self.format_version;
        bytes[7] = SUITE_ID_HKDF_SHA256_CHACHA20POLY1305;
        bytes[8..12].copy_from_slice(&self.chunk_size.to_be_bytes());
        bytes[12..16].copy_from_slice(&0u32.to_be_bytes());
        bytes[0x20..0x30].copy_from_slice(&self.hkdf_salt);
        bytes[0x30..0x38].copy_from_slice(&self.metadata_frame_len.to_be_bytes());
        bytes[0x38] = self.wrap_suite;
        bytes[0x3c..0x40].copy_from_slice(&self.key_frame_len.to_be_bytes());
        bytes[0x40..0x80].copy_from_slice(&object_id_field(&self.object_id)?);
        Ok(bytes)
    }

    /// SHA-256 of the exact serialized header bytes.
    pub fn header_hash(&self) -> Result<[u8; 32]> {
        let bytes = self.serialize()?;
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Ok(out)
    }

    /// SHA-256 of the exact scalar header followed by its key frame.
    pub fn header_hash_with_key_frame(&self, key_frame: &[u8]) -> Result<[u8; 32]> {
        if key_frame.len() != self.key_frame_len as usize {
            return Err(RaoAeadError::InvalidKeyFrameLength);
        }
        let mut hasher = Sha256::new();
        hasher.update(self.serialize()?);
        hasher.update(key_frame);
        Ok(hasher.finalize().into())
    }

    /// Return the exact 64-byte object-id field used by salt derivation.
    pub fn object_id_field(&self) -> Result<[u8; 64]> {
        object_id_field(&self.object_id)
    }

    /// Validate this header under the frozen envelope-field rules.
    pub fn validate(&self) -> Result<()> {
        validate_chunk_size(self.chunk_size)?;
        if self.format_version != 2 {
            return Err(RaoAeadError::UnsupportedFormatVersion);
        }
        validate_wrap_suite(self.wrap_suite)?;
        if !(crate::key_frame::RAO_KEY_FRAME_MIN_LEN..=crate::key_frame::RAO_KEY_FRAME_MAX_LEN)
            .contains(&(self.key_frame_len as usize))
        {
            return Err(RaoAeadError::InvalidKeyFrameLength);
        }
        if self.hkdf_salt == ZERO_16 {
            return Err(RaoAeadError::InvalidSalt);
        }
        validate_metadata_frame_len(self.metadata_frame_len)?;
        object_id_field(&self.object_id)?;
        Ok(())
    }
}

fn validate_wrap_suite(wrap_suite: u8) -> Result<()> {
    if wrap_suite != RAO_WRAP_SUITE_XWING {
        // 0x01 was the pre-production X25519-only assignment. It is
        // permanently reserved and intentionally shares the unknown-suite
        // failure path so neither Readers nor Sealers can negotiate it.
        return Err(RaoAeadError::InvalidWrapSuite);
    }
    Ok(())
}

/// Validate a RAO body block / AEAD chunk size.
pub fn validate_chunk_size(chunk_size: u32) -> Result<()> {
    if chunk_size == 0 || chunk_size % 512 != 0 {
        return Err(RaoAeadError::InvalidChunkSize);
    }
    Ok(())
}

/// Validate an encrypted metadata frame length.
pub fn validate_metadata_frame_len(metadata_frame_len: u64) -> Result<()> {
    if !(RAO_METADATA_FRAME_MIN_LEN..=RAO_MAX_METADATA_FRAME_LEN).contains(&metadata_frame_len) {
        return Err(RaoAeadError::MetadataFrameLengthInvalid);
    }
    Ok(())
}

/// Encode an object id into the exact 64-byte NUL-padded header field.
pub fn object_id_field(object_id: &str) -> Result<[u8; 64]> {
    let bytes = object_id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 || bytes.contains(&0) {
        return Err(RaoAeadError::InvalidObjectIdField);
    }
    let mut field = [0u8; 64];
    field[..bytes.len()].copy_from_slice(bytes);
    Ok(field)
}

fn decode_object_id_field(field: &[u8]) -> Result<String> {
    debug_assert_eq!(field.len(), 64);
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if end == 0 || field[end..].iter().any(|byte| *byte != 0) {
        return Err(RaoAeadError::InvalidObjectIdField);
    }
    std::str::from_utf8(&field[..end])
        .map(|value| value.to_string())
        .map_err(|_| RaoAeadError::InvalidObjectIdField)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header() -> RaoHeader {
        RaoHeader::new_envelope(
            262_144,
            [2; 16],
            64,
            "object-1",
            crate::RAO_KEY_FRAME_MIN_LEN as u32,
        )
        .unwrap()
    }

    #[test]
    fn header_round_trips_and_matches_offsets() {
        let header = valid_header();
        let bytes = header.serialize().unwrap();
        assert_eq!(&bytes[0x00..0x04], b"RAO1");
        assert_eq!(u16::from_be_bytes([bytes[0x04], bytes[0x05]]), 128);
        assert_eq!(bytes[0x06], 2);
        assert_eq!(bytes[0x07], 1);
        assert_eq!(
            u32::from_be_bytes(bytes[0x08..0x0c].try_into().unwrap()),
            262_144
        );
        assert_eq!(&bytes[0x10..0x20], &[0; 16]);
        assert_eq!(&bytes[0x20..0x30], &[2; 16]);
        assert_eq!(
            u64::from_be_bytes(bytes[0x30..0x38].try_into().unwrap()),
            64
        );
        assert_eq!(&bytes[0x40..0x48], b"object-1");

        let parsed = RaoHeader::parse(&bytes).unwrap();
        assert_eq!(parsed, header);
        assert_ne!(header.header_hash().unwrap(), [0; 32]);
        assert_eq!(bytes[0x38], RAO_WRAP_SUITE_XWING);
        assert_eq!(
            u32::from_be_bytes(bytes[0x3c..0x40].try_into().unwrap()),
            1191
        );
    }

    #[test]
    fn header_rejects_unsupported_version_legacy_and_unknown_wrap_suites() {
        let header = RaoHeader::new_envelope(
            4096,
            [2; 16],
            64,
            "object-2",
            crate::RAO_KEY_FRAME_MIN_LEN as u32,
        )
        .unwrap();
        let bytes = header.serialize().unwrap();
        assert_eq!(bytes[0x06], 2);
        assert_eq!(bytes[0x07], 1);
        assert_eq!(&bytes[0x10..0x20], &[0; 16]);
        assert_eq!(bytes[0x38], RAO_WRAP_SUITE_XWING);
        assert_eq!(&bytes[0x39..0x3c], &[0; 3]);
        assert_eq!(
            u32::from_be_bytes(bytes[0x3c..0x40].try_into().unwrap()),
            1191
        );
        assert_eq!(RaoHeader::parse(&bytes).unwrap(), header);

        let mut legacy_suite = bytes;
        legacy_suite[0x38] = 0x01;
        assert!(matches!(
            RaoHeader::parse(&legacy_suite),
            Err(RaoAeadError::InvalidWrapSuite)
        ));
        let mut unknown_suite = bytes;
        unknown_suite[0x38] = 0xff;
        assert!(matches!(
            RaoHeader::parse(&unknown_suite),
            Err(RaoAeadError::InvalidWrapSuite)
        ));
        let mut noncanonical_sealer_header = header.clone();
        noncanonical_sealer_header.wrap_suite = 0x01;
        assert!(matches!(
            noncanonical_sealer_header.serialize(),
            Err(RaoAeadError::InvalidWrapSuite)
        ));
        let mut version_flip = bytes;
        version_flip[6] = 1;
        assert!(matches!(
            RaoHeader::parse(&version_flip),
            Err(RaoAeadError::UnsupportedFormatVersion)
        ));
    }

    #[test]
    fn header_enforces_xwing_key_frame_bounds() {
        for invalid in [1190u32, 16_385] {
            let mut bytes = valid_header().serialize().unwrap();
            bytes[0x3c..0x40].copy_from_slice(&invalid.to_be_bytes());
            assert!(matches!(
                RaoHeader::parse(&bytes),
                Err(RaoAeadError::InvalidKeyFrameLength)
            ));
        }

        for valid in [1191, 16_384] {
            let header = RaoHeader::new_envelope(512, [3; 16], 17, "bounds", valid).unwrap();
            assert_eq!(header.key_frame_len, valid);
        }
    }

    #[test]
    fn object_id_rejects_empty_long_and_interior_nul() {
        assert!(matches!(
            object_id_field(""),
            Err(RaoAeadError::InvalidObjectIdField)
        ));
        assert!(matches!(
            object_id_field(&"x".repeat(65)),
            Err(RaoAeadError::InvalidObjectIdField)
        ));
        assert!(matches!(
            object_id_field("x\0y"),
            Err(RaoAeadError::InvalidObjectIdField)
        ));
    }

    #[test]
    fn header_rejects_reserved_and_bad_magic() {
        let mut bytes = valid_header().serialize().unwrap();
        bytes[0] = b'A';
        assert!(matches!(
            RaoHeader::parse(&bytes),
            Err(RaoAeadError::InvalidMagicBytes)
        ));

        let mut bytes = valid_header().serialize().unwrap();
        bytes[0x10] = 1;
        assert!(matches!(
            RaoHeader::parse(&bytes),
            Err(RaoAeadError::ReservedBytesNotZero)
        ));
    }
}
