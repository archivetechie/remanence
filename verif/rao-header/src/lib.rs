//! Verification extraction of the RAO AEAD header scalar layout.
//!
//! This crate is a standalone, dependency-free model of the fixed scalar
//! checks and field movement in `crates/remanence-aead/src/header.rs`:
//! `RAO1` magic, header length, format version, suite id, chunk size,
//! reserved-zero fields, metadata-frame bounds, and the validity of the
//! key-id, salt, and object-id fields. The production code still owns the exact
//! byte arrays, UTF-8 string reconstruction, hashing, and allocation behavior.
//! The `drift_guard` test pins the production snippets this extraction mirrors;
//! if it fails, the extraction and Lean proofs must be re-synced.

pub const RAO_HEADER_LEN_U16: u16 = 128;
pub const FORMAT_VERSION: u8 = 1;
pub const FORMAT_VERSION_V2: u8 = 2;
pub const WRAP_SUITE_HPKE_V1: u8 = 1;
pub const KEY_FRAME_MAX_LEN: u32 = 4096;
pub const SUITE_ID_HKDF_SHA256_CHACHA20POLY1305: u8 = 0x01;
pub const CHUNK_SIZE_GRANULARITY: u32 = 512;
pub const RAO_METADATA_FRAME_MIN_LEN: u64 = 17;
pub const RAO_MAX_METADATA_FRAME_LEN: u64 = 16_777_216;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaoHeaderError {
    InvalidMagicBytes,
    InvalidHeaderLength,
    UnsupportedFormatVersion,
    InvalidSuite,
    InvalidChunkSize,
    ReservedBytesNotZero,
    InvalidKeyIdentifier,
    InvalidSalt,
    MetadataFrameLengthInvalid,
    InvalidObjectIdField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderCore {
    pub chunk_size: u32,
    pub key_id_nonzero: bool,
    pub hkdf_salt_nonzero: bool,
    pub metadata_frame_len: u64,
    pub object_id_field_valid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderWire {
    pub magic_rao1: bool,
    pub header_len: u16,
    pub format_version: u8,
    pub suite_id: u8,
    pub chunk_size: u32,
    pub flags: u32,
    pub key_id_nonzero: bool,
    pub hkdf_salt_nonzero: bool,
    pub metadata_frame_len: u64,
    pub reserved_0x38_0x40_zero: bool,
    pub object_id_field_valid: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2HeaderCore {
    pub chunk_size: u32,
    pub key_id_zero: bool,
    pub hkdf_salt_nonzero: bool,
    pub metadata_frame_len: u64,
    pub object_id_field_valid: bool,
    pub wrap_suite: u8,
    pub key_frame_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct V2HeaderWire {
    pub header_len: u16,
    pub format_version: u8,
    pub reserved_0x39_0x3c_zero: bool,
    pub core: V2HeaderCore,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyFrameSlotCore {
    pub slot_index: u8,
    pub label_len: u8,
    pub label_printable: bool,
}

pub fn validate_v2_header_core(header: V2HeaderCore) -> Result<(), RaoHeaderError> {
    validate_chunk_size(header.chunk_size)?;
    if !header.key_id_zero {
        return Err(RaoHeaderError::InvalidKeyIdentifier);
    }
    if !header.hkdf_salt_nonzero {
        return Err(RaoHeaderError::InvalidSalt);
    }
    validate_metadata_frame_len(header.metadata_frame_len)?;
    if !header.object_id_field_valid
        || header.wrap_suite != WRAP_SUITE_HPKE_V1
        || !(103..=KEY_FRAME_MAX_LEN).contains(&header.key_frame_len)
    {
        return Err(RaoHeaderError::InvalidObjectIdField);
    }
    Ok(())
}

pub fn serialize_v2_header_core(header: V2HeaderCore) -> Result<V2HeaderWire, RaoHeaderError> {
    validate_v2_header_core(header)?;
    Ok(V2HeaderWire {
        header_len: RAO_HEADER_LEN_U16,
        format_version: FORMAT_VERSION_V2,
        reserved_0x39_0x3c_zero: true,
        core: header,
    })
}

pub fn parse_v2_header_core(wire: V2HeaderWire) -> Result<V2HeaderCore, RaoHeaderError> {
    if wire.header_len != RAO_HEADER_LEN_U16 {
        return Err(RaoHeaderError::InvalidHeaderLength);
    }
    if wire.format_version != FORMAT_VERSION_V2 {
        return Err(RaoHeaderError::UnsupportedFormatVersion);
    }
    if !wire.reserved_0x39_0x3c_zero {
        return Err(RaoHeaderError::ReservedBytesNotZero);
    }
    validate_v2_header_core(wire.core)?;
    Ok(wire.core)
}

pub fn key_frame_round_trip(
    slots: Vec<KeyFrameSlotCore>,
) -> Result<Vec<KeyFrameSlotCore>, RaoHeaderError> {
    if slots.is_empty() || slots.len() > 8 {
        return Err(RaoHeaderError::InvalidObjectIdField);
    }
    let mut previous = None;
    for slot in &slots {
        if previous.is_some_and(|index| slot.slot_index <= index)
            || slot.label_len > 32
            || !slot.label_printable
        {
            return Err(RaoHeaderError::InvalidObjectIdField);
        }
        previous = Some(slot.slot_index);
    }
    Ok(slots)
}

pub fn validate_chunk_size(chunk_size: u32) -> Result<(), RaoHeaderError> {
    if chunk_size == 0 || chunk_size % CHUNK_SIZE_GRANULARITY != 0 {
        return Err(RaoHeaderError::InvalidChunkSize);
    }
    Ok(())
}

pub fn validate_metadata_frame_len(metadata_frame_len: u64) -> Result<(), RaoHeaderError> {
    if metadata_frame_len < RAO_METADATA_FRAME_MIN_LEN {
        return Err(RaoHeaderError::MetadataFrameLengthInvalid);
    }
    if metadata_frame_len > RAO_MAX_METADATA_FRAME_LEN {
        return Err(RaoHeaderError::MetadataFrameLengthInvalid);
    }
    Ok(())
}

pub fn validate_header_core(header: HeaderCore) -> Result<(), RaoHeaderError> {
    validate_chunk_size(header.chunk_size)?;
    if !header.key_id_nonzero {
        return Err(RaoHeaderError::InvalidKeyIdentifier);
    }
    if !header.hkdf_salt_nonzero {
        return Err(RaoHeaderError::InvalidSalt);
    }
    validate_metadata_frame_len(header.metadata_frame_len)?;
    if !header.object_id_field_valid {
        return Err(RaoHeaderError::InvalidObjectIdField);
    }
    Ok(())
}

pub fn serialize_header_core(header: HeaderCore) -> Result<HeaderWire, RaoHeaderError> {
    validate_header_core(header)?;
    Ok(HeaderWire {
        magic_rao1: true,
        header_len: RAO_HEADER_LEN_U16,
        format_version: FORMAT_VERSION,
        suite_id: SUITE_ID_HKDF_SHA256_CHACHA20POLY1305,
        chunk_size: header.chunk_size,
        flags: 0,
        key_id_nonzero: header.key_id_nonzero,
        hkdf_salt_nonzero: header.hkdf_salt_nonzero,
        metadata_frame_len: header.metadata_frame_len,
        reserved_0x38_0x40_zero: true,
        object_id_field_valid: header.object_id_field_valid,
    })
}

pub fn parse_header_core(wire: HeaderWire) -> Result<HeaderCore, RaoHeaderError> {
    if !wire.magic_rao1 {
        return Err(RaoHeaderError::InvalidMagicBytes);
    }
    if wire.header_len != RAO_HEADER_LEN_U16 {
        return Err(RaoHeaderError::InvalidHeaderLength);
    }
    if wire.format_version != FORMAT_VERSION {
        return Err(RaoHeaderError::UnsupportedFormatVersion);
    }
    if wire.suite_id != SUITE_ID_HKDF_SHA256_CHACHA20POLY1305 {
        return Err(RaoHeaderError::InvalidSuite);
    }
    if wire.flags != 0 {
        return Err(RaoHeaderError::ReservedBytesNotZero);
    }
    if !wire.reserved_0x38_0x40_zero {
        return Err(RaoHeaderError::ReservedBytesNotZero);
    }

    let header = HeaderCore {
        chunk_size: wire.chunk_size,
        key_id_nonzero: wire.key_id_nonzero,
        hkdf_salt_nonzero: wire.hkdf_salt_nonzero,
        metadata_frame_len: wire.metadata_frame_len,
        object_id_field_valid: wire.object_id_field_valid,
    };
    validate_header_core(header)?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_header() -> HeaderCore {
        HeaderCore {
            chunk_size: 262_144,
            key_id_nonzero: true,
            hkdf_salt_nonzero: true,
            metadata_frame_len: 64,
            object_id_field_valid: true,
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-aead/src/header.rs"
        ))
        .expect("production header.rs must be readable from verif/rao-header");
        let key_frame = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-aead/src/key_frame.rs"
        ))
        .expect("production key_frame.rs must be readable from verif/rao-header");

        let snippets: &[&str] = &[
            "pub const RAO_HEADER_LEN: usize = 128;",
            "pub const RAO_MAX_METADATA_FRAME_LEN: u64 = 16 * 1024 * 1024;",
            "pub const RAO_METADATA_FRAME_MIN_LEN: u64 = 17;",
            "const MAGIC: &[u8; 4] = b\"RAO1\";",
            "const SUITE_ID_HKDF_SHA256_CHACHA20POLY1305: u8 = 0x01;",
            "if &bytes[0..4] != MAGIC",
            "let header_len = u16::from_be_bytes([bytes[4], bytes[5]]);",
            "if header_len != RAO_HEADER_LEN as u16",
            "if !matches!(format_version, 1 | 2)",
            "if bytes[7] != SUITE_ID_HKDF_SHA256_CHACHA20POLY1305",
            "let chunk_size = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);",
            "let flags = u32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);",
            "if flags != 0",
            "let metadata_frame_len = u64::from_be_bytes([",
            "if bytes[0x39..0x3c].iter().any(|byte| *byte != 0)",
            "bytes[0x38] = self.wrap_suite;",
            "bytes[0x3c..0x40].copy_from_slice(&self.key_frame_len.to_be_bytes());",
            "header.validate()?;",
            "bytes[0..4].copy_from_slice(MAGIC);",
            "bytes[4..6].copy_from_slice(&(RAO_HEADER_LEN as u16).to_be_bytes());",
            "bytes[6] = self.format_version;",
            "bytes[7] = SUITE_ID_HKDF_SHA256_CHACHA20POLY1305;",
            "bytes[8..12].copy_from_slice(&self.chunk_size.to_be_bytes());",
            "bytes[12..16].copy_from_slice(&0u32.to_be_bytes());",
            "bytes[0x30..0x38].copy_from_slice(&self.metadata_frame_len.to_be_bytes());",
            "if self.key_id == ZERO_16",
            "if self.hkdf_salt == ZERO_16",
            "validate_metadata_frame_len(self.metadata_frame_len)?;",
            "if !(RAO_METADATA_FRAME_MIN_LEN..=RAO_MAX_METADATA_FRAME_LEN).contains(&metadata_frame_len)",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-aead header.rs -- production changed; \
                 re-sync this extraction and its Lean proofs"
            );
        }

        for (i, snippet) in [
            "const MAGIC: &[u8; 4] = b\"RAOK\";",
            "if !(1..=RAO_KEY_FRAME_MAX_SLOTS).contains(&count)",
            "if cursor != bytes.len()",
            "slot.slot_index <= value",
        ]
        .iter()
        .enumerate()
        {
            assert!(
                key_frame.contains(snippet),
                "key-frame snippet {i} changed; re-sync RAO header extraction"
            );
        }

        let extraction_snippets: &[&str] = &[
            "pub fn validate_chunk_size(chunk_size: u32)",
            "pub fn validate_metadata_frame_len(metadata_frame_len: u64)",
            "pub fn validate_header_core(header: HeaderCore)",
            "pub fn serialize_header_core(header: HeaderCore)",
            "pub fn parse_header_core(wire: HeaderWire)",
            "magic_rao1: true,",
            "header_len: RAO_HEADER_LEN_U16,",
            "flags: 0,",
            "reserved_0x38_0x40_zero: true,",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif RAO header model"
            );
        }
    }

    #[test]
    fn header_core_round_trips() {
        let header = valid_header();
        let wire = serialize_header_core(header).unwrap();
        assert_eq!(wire.header_len, 128);
        assert_eq!(wire.format_version, 1);
        assert_eq!(wire.suite_id, 1);
        assert_eq!(wire.flags, 0);
        assert_eq!(parse_header_core(wire).unwrap(), header);
    }

    #[test]
    fn parse_rejects_frozen_field_mismatch() {
        let wire = serialize_header_core(valid_header()).unwrap();

        let mut bad = wire;
        bad.magic_rao1 = false;
        assert_eq!(
            parse_header_core(bad).unwrap_err(),
            RaoHeaderError::InvalidMagicBytes
        );

        let mut bad = wire;
        bad.header_len = 127;
        assert_eq!(
            parse_header_core(bad).unwrap_err(),
            RaoHeaderError::InvalidHeaderLength
        );

        let mut bad = wire;
        bad.flags = 1;
        assert_eq!(
            parse_header_core(bad).unwrap_err(),
            RaoHeaderError::ReservedBytesNotZero
        );

        let mut bad = wire;
        bad.reserved_0x38_0x40_zero = false;
        assert_eq!(
            parse_header_core(bad).unwrap_err(),
            RaoHeaderError::ReservedBytesNotZero
        );
    }

    #[test]
    fn validation_rejects_invalid_header_core_fields() {
        let mut header = valid_header();
        header.chunk_size = 0;
        assert_eq!(
            validate_header_core(header).unwrap_err(),
            RaoHeaderError::InvalidChunkSize
        );

        let mut header = valid_header();
        header.key_id_nonzero = false;
        assert_eq!(
            validate_header_core(header).unwrap_err(),
            RaoHeaderError::InvalidKeyIdentifier
        );

        let mut header = valid_header();
        header.hkdf_salt_nonzero = false;
        assert_eq!(
            validate_header_core(header).unwrap_err(),
            RaoHeaderError::InvalidSalt
        );

        let mut header = valid_header();
        header.metadata_frame_len = 16;
        assert_eq!(
            validate_header_core(header).unwrap_err(),
            RaoHeaderError::MetadataFrameLengthInvalid
        );

        let mut header = valid_header();
        header.object_id_field_valid = false;
        assert_eq!(
            validate_header_core(header).unwrap_err(),
            RaoHeaderError::InvalidObjectIdField
        );
    }

    #[test]
    fn v2_header_and_key_frame_models_round_trip_disjointly() {
        let header = V2HeaderCore {
            chunk_size: 4096,
            key_id_zero: true,
            hkdf_salt_nonzero: true,
            metadata_frame_len: 64,
            object_id_field_valid: true,
            wrap_suite: WRAP_SUITE_HPKE_V1,
            key_frame_len: 211,
        };
        let wire = serialize_v2_header_core(header).unwrap();
        assert_eq!(wire.format_version, 2);
        assert_eq!(parse_v2_header_core(wire).unwrap(), header);
        assert_eq!(
            parse_header_core(HeaderWire {
                magic_rao1: true,
                header_len: 128,
                format_version: 2,
                suite_id: 1,
                chunk_size: 4096,
                flags: 0,
                key_id_nonzero: false,
                hkdf_salt_nonzero: true,
                metadata_frame_len: 64,
                reserved_0x38_0x40_zero: false,
                object_id_field_valid: true,
            })
            .unwrap_err(),
            RaoHeaderError::UnsupportedFormatVersion
        );

        let slots = vec![
            KeyFrameSlotCore {
                slot_index: 0,
                label_len: 4,
                label_printable: true,
            },
            KeyFrameSlotCore {
                slot_index: 7,
                label_len: 6,
                label_printable: true,
            },
        ];
        assert_eq!(key_frame_round_trip(slots.clone()).unwrap(), slots);
    }
}
