//! Verification extraction of the RAO manifest regular-file core.
//!
//! This crate is a standalone, dependency-free model of the scalar writer
//! schema and validation checks in `crates/remanence-format/src/layout.rs` and
//! `crates/remanence-format/src/manifest.rs`: the seven-key manifest root map,
//! the one-entry regular `file_entries` array, regular-file chunk-count
//! arithmetic, the 32-byte `file_sha256` payload, empty object metadata, empty
//! external references, and deterministic writer key order. Text fields and
//! SHA-256 bytes are modeled as opaque scalar words so the proof can preserve
//! identity without extracting `String`, `Vec`, CBOR bytes, tar/pax layout, or
//! hashing. The `drift_guard` test pins the production snippets this extraction
//! mirrors; if it fails, the extraction and Lean proofs must be re-synced.

#![allow(clippy::manual_is_multiple_of)]
// Keep explicit modulo tests: the extraction mirrors production code and the
// Lean proof reasons about the generated remainder branch.

pub const CHUNK_SIZE_GRANULARITY: u64 = 512;
pub const SCHEMA_VERSION: u64 = 1;
pub const ROOT_MAP_LEN: u64 = 7;
pub const FILE_ENTRIES_LEN_ONE: u64 = 1;
pub const FILE_ENTRY_REGULAR_MAP_LEN: u64 = 8;
pub const DIGEST_BYTE_LEN: u64 = 32;

pub const ROOT_KEY_OBJECT_ID: u64 = 0;
pub const ROOT_KEY_CHUNK_SIZE: u64 = 1;
pub const ROOT_KEY_FILE_ENTRIES: u64 = 2;
pub const ROOT_KEY_SCHEMA_VERSION: u64 = 3;
pub const ROOT_KEY_OBJECT_METADATA: u64 = 4;
pub const ROOT_KEY_CALLER_OBJECT_ID: u64 = 5;
pub const ROOT_KEY_EXTERNAL_REFERENCES: u64 = 6;

pub const FILE_KEY_PATH: u64 = 0;
pub const FILE_KEY_FILE_ID: u64 = 1;
pub const FILE_KEY_EXECUTABLE: u64 = 2;
pub const FILE_KEY_SIZE_BYTES: u64 = 3;
pub const FILE_KEY_CHUNK_COUNT: u64 = 4;
pub const FILE_KEY_FILE_SHA256: u64 = 5;
pub const FILE_KEY_FIRST_CHUNK_LBA: u64 = 6;
pub const FILE_KEY_METADATA_PRESERVATION_DATA: u64 = 7;

pub const EXECUTABLE_NULL: u8 = 0;
pub const EXECUTABLE_FALSE: u8 = 1;
pub const EXECUTABLE_TRUE: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaoManifestError {
    InvalidChunkSize,
    InvalidManifestField,
    InvalidCborEncoding,
    MissingRequiredManifestField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestWords {
    pub w0: u64,
    pub w1: u64,
    pub w2: u64,
    pub w3: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegularFileCore {
    pub path_id: u64,
    pub file_id: u64,
    pub size_bytes: u64,
    pub file_sha256: DigestWords,
    pub first_chunk_lba_present: bool,
    pub first_chunk_lba: u64,
    pub executable_tag: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestCore {
    pub object_id: u64,
    pub caller_object_id: u64,
    pub chunk_size: u64,
    pub file: RegularFileCore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegularFileWireCore {
    pub map_len: u64,
    pub key_path: u64,
    pub path_id: u64,
    pub key_file_id: u64,
    pub file_id: u64,
    pub key_executable: u64,
    pub executable_tag: u8,
    pub key_size_bytes: u64,
    pub size_bytes: u64,
    pub key_chunk_count: u64,
    pub chunk_count: u64,
    pub key_file_sha256: u64,
    pub file_sha256_len: u64,
    pub file_sha256: DigestWords,
    pub key_first_chunk_lba: u64,
    pub first_chunk_lba_is_null: bool,
    pub first_chunk_lba: u64,
    pub key_metadata_preservation_data: u64,
    pub metadata_preservation_data_empty: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestWireCore {
    pub root_map_len: u64,
    pub key_object_id: u64,
    pub object_id: u64,
    pub key_chunk_size: u64,
    pub chunk_size: u64,
    pub key_file_entries: u64,
    pub file_entries_len: u64,
    pub file: RegularFileWireCore,
    pub key_schema_version: u64,
    pub schema_version: u64,
    pub key_object_metadata: u64,
    pub object_metadata_empty: bool,
    pub key_caller_object_id: u64,
    pub caller_object_id: u64,
    pub key_external_references: u64,
    pub external_references_empty: bool,
    pub trailing_data: bool,
}

pub fn checked_add(a: u64, b: u64) -> Result<u64, RaoManifestError> {
    match a.checked_add(b) {
        Some(sum) => Ok(sum),
        None => Err(RaoManifestError::InvalidManifestField),
    }
}

pub fn validate_chunk_size(chunk_size: u64) -> Result<(), RaoManifestError> {
    if chunk_size == 0 || chunk_size % CHUNK_SIZE_GRANULARITY != 0 {
        return Err(RaoManifestError::InvalidChunkSize);
    }
    Ok(())
}

pub fn chunk_count_core(size_bytes: u64, chunk_size: u64) -> Result<u64, RaoManifestError> {
    validate_chunk_size(chunk_size)?;
    if size_bytes == 0 {
        return Ok(0);
    }
    let chunks_minus_one = (size_bytes - 1) / chunk_size;
    checked_add(chunks_minus_one, 1)
}

pub fn validate_regular_file_core(
    file: RegularFileCore,
    chunk_size: u64,
) -> Result<(), RaoManifestError> {
    validate_chunk_size(chunk_size)?;
    if file.path_id == 0 || file.file_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if file.executable_tag > EXECUTABLE_TRUE {
        return Err(RaoManifestError::InvalidManifestField);
    }
    chunk_count_core(file.size_bytes, chunk_size)?;
    if file.size_bytes == 0 {
        if file.first_chunk_lba_present || file.first_chunk_lba != 0 {
            return Err(RaoManifestError::InvalidManifestField);
        }
    } else if !file.first_chunk_lba_present {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn validate_manifest_core(manifest: ManifestCore) -> Result<(), RaoManifestError> {
    validate_chunk_size(manifest.chunk_size)?;
    if manifest.object_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    validate_regular_file_core(manifest.file, manifest.chunk_size)?;
    Ok(())
}

pub fn encode_regular_file_core(
    file: RegularFileCore,
    chunk_size: u64,
) -> Result<RegularFileWireCore, RaoManifestError> {
    validate_regular_file_core(file, chunk_size)?;
    let chunk_count = chunk_count_core(file.size_bytes, chunk_size)?;
    Ok(RegularFileWireCore {
        map_len: FILE_ENTRY_REGULAR_MAP_LEN,
        key_path: FILE_KEY_PATH,
        path_id: file.path_id,
        key_file_id: FILE_KEY_FILE_ID,
        file_id: file.file_id,
        key_executable: FILE_KEY_EXECUTABLE,
        executable_tag: file.executable_tag,
        key_size_bytes: FILE_KEY_SIZE_BYTES,
        size_bytes: file.size_bytes,
        key_chunk_count: FILE_KEY_CHUNK_COUNT,
        chunk_count,
        key_file_sha256: FILE_KEY_FILE_SHA256,
        file_sha256_len: DIGEST_BYTE_LEN,
        file_sha256: file.file_sha256,
        key_first_chunk_lba: FILE_KEY_FIRST_CHUNK_LBA,
        first_chunk_lba_is_null: !file.first_chunk_lba_present,
        first_chunk_lba: file.first_chunk_lba,
        key_metadata_preservation_data: FILE_KEY_METADATA_PRESERVATION_DATA,
        metadata_preservation_data_empty: true,
    })
}

pub fn encode_manifest_core(manifest: ManifestCore) -> Result<ManifestWireCore, RaoManifestError> {
    validate_manifest_core(manifest)?;
    let file = encode_regular_file_core(manifest.file, manifest.chunk_size)?;
    Ok(ManifestWireCore {
        root_map_len: ROOT_MAP_LEN,
        key_object_id: ROOT_KEY_OBJECT_ID,
        object_id: manifest.object_id,
        key_chunk_size: ROOT_KEY_CHUNK_SIZE,
        chunk_size: manifest.chunk_size,
        key_file_entries: ROOT_KEY_FILE_ENTRIES,
        file_entries_len: FILE_ENTRIES_LEN_ONE,
        file,
        key_schema_version: ROOT_KEY_SCHEMA_VERSION,
        schema_version: SCHEMA_VERSION,
        key_object_metadata: ROOT_KEY_OBJECT_METADATA,
        object_metadata_empty: true,
        key_caller_object_id: ROOT_KEY_CALLER_OBJECT_ID,
        caller_object_id: manifest.caller_object_id,
        key_external_references: ROOT_KEY_EXTERNAL_REFERENCES,
        external_references_empty: true,
        trailing_data: false,
    })
}

pub fn decode_regular_file_core(
    wire: RegularFileWireCore,
    chunk_size: u64,
) -> Result<RegularFileCore, RaoManifestError> {
    if wire.map_len != FILE_ENTRY_REGULAR_MAP_LEN {
        return Err(RaoManifestError::MissingRequiredManifestField);
    }
    if wire.key_path != FILE_KEY_PATH
        || wire.key_file_id != FILE_KEY_FILE_ID
        || wire.key_executable != FILE_KEY_EXECUTABLE
        || wire.key_size_bytes != FILE_KEY_SIZE_BYTES
        || wire.key_chunk_count != FILE_KEY_CHUNK_COUNT
        || wire.key_file_sha256 != FILE_KEY_FILE_SHA256
        || wire.key_first_chunk_lba != FILE_KEY_FIRST_CHUNK_LBA
        || wire.key_metadata_preservation_data != FILE_KEY_METADATA_PRESERVATION_DATA
    {
        return Err(RaoManifestError::InvalidCborEncoding);
    }
    if wire.file_sha256_len != DIGEST_BYTE_LEN {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if !wire.metadata_preservation_data_empty {
        return Err(RaoManifestError::InvalidManifestField);
    }

    let expected_chunk_count = chunk_count_core(wire.size_bytes, chunk_size)?;
    if wire.chunk_count != expected_chunk_count {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if wire.size_bytes == 0 {
        if !wire.first_chunk_lba_is_null {
            return Err(RaoManifestError::InvalidManifestField);
        }
    } else if wire.first_chunk_lba_is_null {
        return Err(RaoManifestError::InvalidManifestField);
    }

    let file = RegularFileCore {
        path_id: wire.path_id,
        file_id: wire.file_id,
        size_bytes: wire.size_bytes,
        file_sha256: wire.file_sha256,
        first_chunk_lba_present: !wire.first_chunk_lba_is_null,
        first_chunk_lba: if wire.first_chunk_lba_is_null {
            0
        } else {
            wire.first_chunk_lba
        },
        executable_tag: wire.executable_tag,
    };
    validate_regular_file_core(file, chunk_size)?;
    Ok(file)
}

pub fn decode_manifest_core(
    wire: ManifestWireCore,
    reader_chunk_size: u64,
) -> Result<ManifestCore, RaoManifestError> {
    if wire.trailing_data {
        return Err(RaoManifestError::InvalidCborEncoding);
    }
    if wire.root_map_len != ROOT_MAP_LEN {
        return Err(RaoManifestError::MissingRequiredManifestField);
    }
    if wire.key_object_id != ROOT_KEY_OBJECT_ID
        || wire.key_chunk_size != ROOT_KEY_CHUNK_SIZE
        || wire.key_file_entries != ROOT_KEY_FILE_ENTRIES
        || wire.key_schema_version != ROOT_KEY_SCHEMA_VERSION
        || wire.key_object_metadata != ROOT_KEY_OBJECT_METADATA
        || wire.key_caller_object_id != ROOT_KEY_CALLER_OBJECT_ID
        || wire.key_external_references != ROOT_KEY_EXTERNAL_REFERENCES
    {
        return Err(RaoManifestError::InvalidCborEncoding);
    }
    if wire.schema_version != SCHEMA_VERSION {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if wire.chunk_size != reader_chunk_size {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if wire.file_entries_len != FILE_ENTRIES_LEN_ONE {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if !wire.object_metadata_empty || !wire.external_references_empty {
        return Err(RaoManifestError::InvalidManifestField);
    }

    let file = decode_regular_file_core(wire.file, reader_chunk_size)?;
    let manifest = ManifestCore {
        object_id: wire.object_id,
        caller_object_id: wire.caller_object_id,
        chunk_size: wire.chunk_size,
        file,
    };
    validate_manifest_core(manifest)?;
    Ok(manifest)
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

    fn regular_file(size_bytes: u64) -> RegularFileCore {
        RegularFileCore {
            path_id: 11,
            file_id: 22,
            size_bytes,
            file_sha256: DIGEST,
            first_chunk_lba_present: size_bytes != 0,
            first_chunk_lba: if size_bytes == 0 { 0 } else { 5 },
            executable_tag: EXECUTABLE_NULL,
        }
    }

    fn manifest(size_bytes: u64) -> ManifestCore {
        ManifestCore {
            object_id: 33,
            caller_object_id: 44,
            chunk_size: 512,
            file: regular_file(size_bytes),
        }
    }

    #[test]
    fn drift_guard() {
        let this_file = include_str!("lib.rs");
        let layout = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-format/src/layout.rs"
        ))
        .expect("production layout.rs must be readable from verif/rao-manifest");
        let manifest = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../crates/remanence-format/src/manifest.rs"
        ))
        .expect("production manifest.rs must be readable from verif/rao-manifest");

        let layout_snippets: &[&str] = &[
            "pub(crate) fn encode_manifest(",
            "\"caller_object_id\"",
            "\"chunk_size\"",
            "\"external_references\", CborValue::Array(Vec::new())",
            "\"file_entries\"",
            "CborValue::Array(files.iter().map(file_manifest_entry).collect())",
            "\"object_id\"",
            "\"object_metadata\", CborValue::Map(Vec::new())",
            "\"schema_version\", CborValue::Integer(1u64.into())",
            "map.sort_by_key(|entry| canonical_text_key(entry.0));",
            "fn file_manifest_entry(layout: &RemTarFileLayout) -> CborValue",
            "\"chunk_count\", CborValue::Integer(layout.chunk_count.into())",
            "\"executable\"",
            "\"file_id\", CborValue::Text(layout.file_id.clone())",
            "\"first_chunk_lba\"",
            "\"metadata_preservation_data\"",
            "\"path\", CborValue::Text(layout.path.clone())",
            "\"size_bytes\", CborValue::Integer(layout.size_bytes.into())",
            "map.push((\"file_sha256\", CborValue::Bytes(file_sha256.to_vec())));",
            "pub(crate) fn chunk_count(size_bytes: u64, chunk_size: usize)",
            "if size_bytes == 0 {\n        return Ok(0);\n    }",
            "Ok((size_bytes - 1) / chunk + 1)",
        ];
        for (i, snippet) in layout_snippets.iter().enumerate() {
            assert!(
                layout.contains(snippet),
                "layout snippet {i} no longer in remanence-format layout.rs -- production changed; \
                 re-sync this extraction and its Lean proofs"
            );
        }

        let manifest_snippets: &[&str] = &[
            "let schema_version = required_u64(map, \"schema_version\")?;",
            "if schema_version != 1",
            "let manifest_chunk_size = required_u64(map, \"chunk_size\")?;",
            "if manifest_chunk_size != reader_chunk_size as u64",
            "let file_entries = required_array(map, \"file_entries\")?;",
            "if file_entries.len() > MAX_FILE_ENTRIES",
            "let expected_chunk_count = if size_bytes == 0 {\n        0\n    } else {\n        (size_bytes - 1) / reader_chunk_size as u64 + 1\n    };",
            "if chunk_count != expected_chunk_count",
            "first_chunk_lba must be null when size_bytes is zero",
            "first_chunk_lba must be unsigned when size_bytes is nonzero",
            "regular entry missing file_sha256",
            "if file_sha256.len() != 32",
        ];
        for (i, snippet) in manifest_snippets.iter().enumerate() {
            assert!(
                manifest.contains(snippet),
                "manifest snippet {i} no longer in remanence-format manifest.rs -- production changed; \
                 re-sync this extraction and its Lean proofs"
            );
        }

        let extraction_snippets: &[&str] = &[
            "pub fn chunk_count_core(",
            "pub fn validate_regular_file_core(",
            "pub fn encode_regular_file_core(",
            "pub fn encode_manifest_core(",
            "pub fn decode_regular_file_core(",
            "pub fn decode_manifest_core(",
            "root_map_len: ROOT_MAP_LEN,",
            "file_entries_len: FILE_ENTRIES_LEN_ONE,",
            "schema_version: SCHEMA_VERSION,",
            "metadata_preservation_data_empty: true,",
            "file_sha256_len: DIGEST_BYTE_LEN,",
        ];
        for (i, snippet) in extraction_snippets.iter().enumerate() {
            assert!(
                this_file.contains(snippet),
                "extraction snippet {i} missing from verif RAO manifest model"
            );
        }
    }

    #[test]
    fn manifest_core_round_trips() {
        let manifest = manifest(1025);
        let wire = encode_manifest_core(manifest).unwrap();
        assert_eq!(wire.file.chunk_count, 3);
        assert_eq!(
            decode_manifest_core(wire, manifest.chunk_size).unwrap(),
            manifest
        );
    }

    #[test]
    fn empty_regular_file_round_trips() {
        let manifest = manifest(0);
        let wire = encode_manifest_core(manifest).unwrap();
        assert_eq!(wire.file.chunk_count, 0);
        assert!(wire.file.first_chunk_lba_is_null);
        assert_eq!(
            decode_manifest_core(wire, manifest.chunk_size).unwrap(),
            manifest
        );
    }

    #[test]
    fn validation_rejects_bad_file_shape() {
        let mut file = regular_file(0);
        file.first_chunk_lba_present = true;
        assert_eq!(
            validate_regular_file_core(file, 512),
            Err(RaoManifestError::InvalidManifestField)
        );

        let mut file = regular_file(1);
        file.first_chunk_lba_present = false;
        assert_eq!(
            validate_regular_file_core(file, 512),
            Err(RaoManifestError::InvalidManifestField)
        );
    }

    #[test]
    fn decode_rejects_bad_writer_shape() {
        let manifest = manifest(1);
        let mut wire = encode_manifest_core(manifest).unwrap();
        wire.file.file_sha256_len = 31;
        assert_eq!(
            decode_manifest_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );

        let mut wire = encode_manifest_core(manifest).unwrap();
        wire.file.chunk_count = 2;
        assert_eq!(
            decode_manifest_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );
    }
}
