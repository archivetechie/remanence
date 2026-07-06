//! Verification extraction of the RAO metadata deterministic-CBOR core.
//!
//! This crate is a standalone, dependency-free model of the v1 metadata writer
//! schema and validation arithmetic in `crates/remanence-aead/src/metadata.rs`:
//! the required four integer keys, version `1`, digest algorithm `sha256`,
//! 32-byte digest payload, chunk-size validation, plaintext-size alignment, and
//! payload tag-overflow checks. It models the SHA-256 digest as four opaque
//! scalar words so the proof can preserve digest identity without extracting
//! byte slices, allocation, UTF-8, or recursive extension skipping. The
//! `drift_guard` test pins the production snippets this extraction mirrors; if
//! it fails, the extraction and Lean proofs must be re-synced.

#![allow(clippy::manual_is_multiple_of)]
// Keep explicit modulo tests: the extraction mirrors production code and the
// Lean proof reasons about the generated remainder branch.

pub const KEY_METADATA_VERSION: u64 = 0;
pub const KEY_PLAINTEXT_SIZE: u64 = 1;
pub const KEY_PLAINTEXT_DIGEST_ALG: u64 = 2;
pub const KEY_PLAINTEXT_DIGEST: u64 = 3;

pub const METADATA_VERSION: u64 = 1;
pub const REQUIRED_MAP_LEN: u64 = 4;
pub const DIGEST_BYTE_LEN: u64 = 32;
pub const CHACHA20POLY1305_TAG_LEN: u64 = 16;
pub const CHUNK_SIZE_GRANULARITY: u64 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaoMetadataError {
    InvalidChunkSize,
    InvalidMetadataField,
    InvalidCborEncoding,
    MissingRequiredMetadataField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestWords {
    pub w0: u64,
    pub w1: u64,
    pub w2: u64,
    pub w3: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataCore {
    pub plaintext_size: u64,
    pub digest: DigestWords,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataCborCore {
    pub map_len: u64,
    pub key_metadata_version: u64,
    pub metadata_version: u64,
    pub key_plaintext_size: u64,
    pub plaintext_size: u64,
    pub key_plaintext_digest_alg: u64,
    pub digest_alg_sha256: bool,
    pub key_plaintext_digest: u64,
    pub digest_byte_len: u64,
    pub digest: DigestWords,
    pub trailing_data: bool,
}

pub fn checked_add(a: u64, b: u64) -> Result<u64, RaoMetadataError> {
    match a.checked_add(b) {
        Some(sum) => Ok(sum),
        None => Err(RaoMetadataError::InvalidMetadataField),
    }
}

pub fn checked_mul(a: u64, b: u64) -> Result<u64, RaoMetadataError> {
    match a.checked_mul(b) {
        Some(product) => Ok(product),
        None => Err(RaoMetadataError::InvalidMetadataField),
    }
}

pub fn validate_chunk_size(chunk_size: u64) -> Result<(), RaoMetadataError> {
    if chunk_size == 0 || chunk_size % CHUNK_SIZE_GRANULARITY != 0 {
        return Err(RaoMetadataError::InvalidChunkSize);
    }
    Ok(())
}

pub fn validate_metadata_core(
    metadata: MetadataCore,
    chunk_size: u64,
) -> Result<(), RaoMetadataError> {
    validate_chunk_size(chunk_size)?;
    if metadata.plaintext_size == 0 || metadata.plaintext_size % chunk_size != 0 {
        return Err(RaoMetadataError::InvalidMetadataField);
    }
    let chunk_count = metadata.plaintext_size / chunk_size;
    let tag_bytes = checked_mul(CHACHA20POLY1305_TAG_LEN, chunk_count)?;
    checked_add(metadata.plaintext_size, tag_bytes)?;
    Ok(())
}

pub fn encode_metadata_core(
    metadata: MetadataCore,
    chunk_size: u64,
) -> Result<MetadataCborCore, RaoMetadataError> {
    validate_metadata_core(metadata, chunk_size)?;
    Ok(MetadataCborCore {
        map_len: REQUIRED_MAP_LEN,
        key_metadata_version: KEY_METADATA_VERSION,
        metadata_version: METADATA_VERSION,
        key_plaintext_size: KEY_PLAINTEXT_SIZE,
        plaintext_size: metadata.plaintext_size,
        key_plaintext_digest_alg: KEY_PLAINTEXT_DIGEST_ALG,
        digest_alg_sha256: true,
        key_plaintext_digest: KEY_PLAINTEXT_DIGEST,
        digest_byte_len: DIGEST_BYTE_LEN,
        digest: metadata.digest,
        trailing_data: false,
    })
}

pub fn decode_metadata_core(
    wire: MetadataCborCore,
    chunk_size: u64,
) -> Result<MetadataCore, RaoMetadataError> {
    if wire.trailing_data {
        return Err(RaoMetadataError::InvalidCborEncoding);
    }
    if wire.map_len != REQUIRED_MAP_LEN {
        return Err(RaoMetadataError::MissingRequiredMetadataField);
    }
    if wire.key_metadata_version != KEY_METADATA_VERSION
        || wire.key_plaintext_size != KEY_PLAINTEXT_SIZE
        || wire.key_plaintext_digest_alg != KEY_PLAINTEXT_DIGEST_ALG
        || wire.key_plaintext_digest != KEY_PLAINTEXT_DIGEST
    {
        return Err(RaoMetadataError::InvalidCborEncoding);
    }
    if wire.metadata_version != METADATA_VERSION {
        return Err(RaoMetadataError::InvalidMetadataField);
    }
    if !wire.digest_alg_sha256 {
        return Err(RaoMetadataError::InvalidMetadataField);
    }
    if wire.digest_byte_len != DIGEST_BYTE_LEN {
        return Err(RaoMetadataError::InvalidMetadataField);
    }

    let metadata = MetadataCore {
        plaintext_size: wire.plaintext_size,
        digest: wire.digest,
    };
    validate_metadata_core(metadata, chunk_size)?;
    Ok(metadata)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: DigestWords = DigestWords {
        w0: 0x0102_0304_0506_0708,
        w1: 0x1112_1314_1516_1718,
        w2: 0x2122_2324_2526_2728,
        w3: 0x3132_3334_3536_3738,
    };

    fn valid_metadata() -> MetadataCore {
        MetadataCore {
            plaintext_size: 262_144,
            digest: DIGEST,
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let original = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-aead/src/metadata.rs"
        ))
        .expect("production metadata.rs must be readable from verif/rao-metadata");

        let snippets: &[&str] = &[
            "const KEY_METADATA_VERSION: u64 = 0;",
            "const KEY_PLAINTEXT_SIZE: u64 = 1;",
            "const KEY_PLAINTEXT_DIGEST_ALG: u64 = 2;",
            "const KEY_PLAINTEXT_DIGEST: u64 = 3;",
            "encode_type_len(5, 4, &mut out);",
            "encode_unsigned(KEY_METADATA_VERSION, &mut out);\n        encode_unsigned(1, &mut out);",
            "encode_unsigned(KEY_PLAINTEXT_SIZE, &mut out);\n        encode_unsigned(self.plaintext_size, &mut out);",
            "encode_unsigned(KEY_PLAINTEXT_DIGEST_ALG, &mut out);\n        encode_text(\"sha256\", &mut out);",
            "encode_unsigned(KEY_PLAINTEXT_DIGEST, &mut out);\n        encode_bytes(&self.plaintext_digest, &mut out);",
            "if decoder.pos != bytes.len()",
            "if metadata_version.ok_or(RaoAeadError::MissingRequiredMetadataField)? != 1",
            "if plaintext_digest_alg.ok_or(RaoAeadError::MissingRequiredMetadataField)? != \"sha256\"",
            "if bytes.len() != 32",
            "validate_chunk_size(chunk_size)?;",
            "if self.plaintext_size == 0 || self.plaintext_size % chunk != 0",
            "let chunk_count = self.plaintext_size / chunk;",
            "let tag_bytes = CHACHA20POLY1305_TAG_LEN\n            .checked_mul(chunk_count)",
            "self.plaintext_size\n            .checked_add(tag_bytes)",
        ];
        for (i, snippet) in snippets.iter().enumerate() {
            assert!(
                original.contains(snippet),
                "snippet {i} no longer in remanence-aead metadata.rs -- production changed; \
                 re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "pub fn validate_metadata_core(",
            "metadata.plaintext_size == 0 || metadata.plaintext_size % chunk_size != 0",
            "let chunk_count = metadata.plaintext_size / chunk_size;",
            "let tag_bytes = checked_mul(CHACHA20POLY1305_TAG_LEN, chunk_count)?;",
            "checked_add(metadata.plaintext_size, tag_bytes)?;",
            "pub fn encode_metadata_core(",
            "map_len: REQUIRED_MAP_LEN,",
            "digest_alg_sha256: true,",
            "trailing_data: false,",
            "pub fn decode_metadata_core(",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif RAO metadata model"
            );
        }
    }

    #[test]
    fn metadata_core_round_trips() {
        let metadata = valid_metadata();
        let wire = encode_metadata_core(metadata, 262_144).unwrap();
        assert_eq!(wire.map_len, 4);
        assert_eq!(wire.metadata_version, 1);
        assert!(wire.digest_alg_sha256);
        assert_eq!(wire.digest_byte_len, 32);
        assert_eq!(decode_metadata_core(wire, 262_144).unwrap(), metadata);
    }

    #[test]
    fn validation_rejects_bad_plaintext_size() {
        let mut metadata = valid_metadata();
        metadata.plaintext_size = 0;
        assert_eq!(
            validate_metadata_core(metadata, 512).unwrap_err(),
            RaoMetadataError::InvalidMetadataField
        );

        let mut metadata = valid_metadata();
        metadata.plaintext_size = 513;
        assert_eq!(
            validate_metadata_core(metadata, 512).unwrap_err(),
            RaoMetadataError::InvalidMetadataField
        );

        let mut metadata = valid_metadata();
        metadata.plaintext_size = u64::MAX - 511;
        assert_eq!(
            validate_metadata_core(metadata, 512).unwrap_err(),
            RaoMetadataError::InvalidMetadataField
        );
    }

    #[test]
    fn decode_rejects_bad_writer_shape() {
        let wire = encode_metadata_core(valid_metadata(), 262_144).unwrap();

        let mut bad = wire;
        bad.trailing_data = true;
        assert_eq!(
            decode_metadata_core(bad, 262_144).unwrap_err(),
            RaoMetadataError::InvalidCborEncoding
        );

        let mut bad = wire;
        bad.map_len = 3;
        assert_eq!(
            decode_metadata_core(bad, 262_144).unwrap_err(),
            RaoMetadataError::MissingRequiredMetadataField
        );

        let mut bad = wire;
        bad.key_plaintext_size = 0;
        assert_eq!(
            decode_metadata_core(bad, 262_144).unwrap_err(),
            RaoMetadataError::InvalidCborEncoding
        );

        let mut bad = wire;
        bad.metadata_version = 2;
        assert_eq!(
            decode_metadata_core(bad, 262_144).unwrap_err(),
            RaoMetadataError::InvalidMetadataField
        );

        let mut bad = wire;
        bad.digest_alg_sha256 = false;
        assert_eq!(
            decode_metadata_core(bad, 262_144).unwrap_err(),
            RaoMetadataError::InvalidMetadataField
        );

        let mut bad = wire;
        bad.digest_byte_len = 31;
        assert_eq!(
            decode_metadata_core(bad, 262_144).unwrap_err(),
            RaoMetadataError::InvalidMetadataField
        );
    }
}
