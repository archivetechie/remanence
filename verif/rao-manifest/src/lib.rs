//! Verification extraction of RAO manifest writer-schema cores.
//!
//! This crate is a standalone, dependency-free model of the scalar writer
//! schema and validation checks in `crates/remanence-format/src/layout.rs` and
//! `crates/remanence-format/src/manifest.rs`.
//!
//! The first core models the original one-entry regular-file manifest: the
//! seven-key manifest root map, regular-file chunk-count arithmetic, the
//! 32-byte `file_sha256` payload, empty object metadata, empty external
//! references, and deterministic writer key order.
//!
//! The second core broadens that surface to a bounded five-entry manifest:
//! nonempty regular file with one xattr, empty regular file, hardlink, symlink,
//! and directory. The array core adds fixed-capacity fold checks for duplicate
//! path/file ids and hardlink target membership in the accumulated regular-file
//! prefix. This is still not the production `Vec`/CBOR parser; text, xattr
//! bytes, and SHA-256 bytes are modeled as opaque scalar words so the proof can
//! preserve identity without extracting `String`, `Vec`, CBOR bytes, tar/pax
//! layout, or hashing. The `drift_guard` test pins the production snippets this
//! extraction mirrors; if it fails, the extraction and Lean proofs must be
//! re-synced.

#![allow(clippy::manual_is_multiple_of)]
// Keep explicit modulo tests: the extraction mirrors production code and the
// Lean proof reasons about the generated remainder branch.

pub const CHUNK_SIZE_GRANULARITY: u64 = 512;
pub const SCHEMA_VERSION: u64 = 1;
pub const ROOT_MAP_LEN: u64 = 7;
pub const FILE_ENTRIES_LEN_ONE: u64 = 1;
pub const FILE_ENTRIES_LEN_BOUNDED: u64 = 5;
pub const FILE_ENTRY_REGULAR_MAP_LEN: u64 = 8;
pub const FILE_ENTRY_LINK_MAP_LEN: u64 = 9;
pub const FILE_ENTRY_DIRECTORY_MAP_LEN: u64 = 8;
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

pub const LINK_KEY_PATH: u64 = 0;
pub const LINK_KEY_FILE_ID: u64 = 1;
pub const LINK_KEY_ENTRY_TYPE: u64 = 2;
pub const LINK_KEY_EXECUTABLE: u64 = 3;
pub const LINK_KEY_SIZE_BYTES: u64 = 4;
pub const LINK_KEY_CHUNK_COUNT: u64 = 5;
pub const LINK_KEY_LINK_TARGET: u64 = 6;
pub const LINK_KEY_FIRST_CHUNK_LBA: u64 = 7;
pub const LINK_KEY_METADATA_PRESERVATION_DATA: u64 = 8;

pub const DIRECTORY_KEY_PATH: u64 = 0;
pub const DIRECTORY_KEY_FILE_ID: u64 = 1;
pub const DIRECTORY_KEY_ENTRY_TYPE: u64 = 2;
pub const DIRECTORY_KEY_EXECUTABLE: u64 = 3;
pub const DIRECTORY_KEY_SIZE_BYTES: u64 = 4;
pub const DIRECTORY_KEY_CHUNK_COUNT: u64 = 5;
pub const DIRECTORY_KEY_FIRST_CHUNK_LBA: u64 = 6;
pub const DIRECTORY_KEY_METADATA_PRESERVATION_DATA: u64 = 7;

pub const METADATA_PRESERVATION_EMPTY_MAP_LEN: u64 = 0;
pub const METADATA_PRESERVATION_XATTRS_MAP_LEN: u64 = 1;
pub const METADATA_KEY_XATTRS: u64 = 0;
pub const XATTRS_ONE_ENTRY_LEN: u64 = 1;

pub const EXECUTABLE_NULL: u8 = 0;
pub const EXECUTABLE_FALSE: u8 = 1;
pub const EXECUTABLE_TRUE: u8 = 2;

pub const ENTRY_TYPE_HARDLINK: u8 = 1;
pub const ENTRY_TYPE_SYMLINK: u8 = 2;
pub const ENTRY_TYPE_DIRECTORY: u8 = 3;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RichRegularFileCore {
    pub path_id: u64,
    pub file_id: u64,
    pub size_bytes: u64,
    pub file_sha256: DigestWords,
    pub first_chunk_lba_present: bool,
    pub first_chunk_lba: u64,
    pub executable_tag: u8,
    pub xattr_present: bool,
    pub xattr_name_id: u64,
    pub xattr_value_len: u64,
    pub xattr_value_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HardlinkEntryCore {
    pub path_id: u64,
    pub file_id: u64,
    pub link_target_path_id: u64,
    pub executable_tag: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SymlinkEntryCore {
    pub path_id: u64,
    pub file_id: u64,
    pub link_target_id: u64,
    pub executable_tag: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryEntryCore {
    pub path_id: u64,
    pub file_id: u64,
    pub executable_tag: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestEntriesCore {
    pub object_id: u64,
    pub caller_object_id: u64,
    pub chunk_size: u64,
    pub nonempty_regular: RichRegularFileCore,
    pub empty_regular: RichRegularFileCore,
    pub hardlink: HardlinkEntryCore,
    pub symlink: SymlinkEntryCore,
    pub directory: DirectoryEntryCore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RichRegularFileWireCore {
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
    pub metadata_preservation_data_map_len: u64,
    pub metadata_key_xattrs: u64,
    pub xattrs_map_len: u64,
    pub xattr_name_id: u64,
    pub xattr_value_len: u64,
    pub xattr_value_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkEntryWireCore {
    pub map_len: u64,
    pub key_path: u64,
    pub path_id: u64,
    pub key_file_id: u64,
    pub file_id: u64,
    pub key_entry_type: u64,
    pub entry_type: u8,
    pub key_executable: u64,
    pub executable_tag: u8,
    pub key_size_bytes: u64,
    pub size_bytes: u64,
    pub key_chunk_count: u64,
    pub chunk_count: u64,
    pub key_link_target: u64,
    pub link_target_id: u64,
    pub key_first_chunk_lba: u64,
    pub first_chunk_lba_is_null: bool,
    pub key_metadata_preservation_data: u64,
    pub metadata_preservation_data_empty: bool,
    pub file_sha256_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryEntryWireCore {
    pub map_len: u64,
    pub key_path: u64,
    pub path_id: u64,
    pub key_file_id: u64,
    pub file_id: u64,
    pub key_entry_type: u64,
    pub entry_type: u8,
    pub key_executable: u64,
    pub executable_tag: u8,
    pub key_size_bytes: u64,
    pub size_bytes: u64,
    pub key_chunk_count: u64,
    pub chunk_count: u64,
    pub key_first_chunk_lba: u64,
    pub first_chunk_lba_is_null: bool,
    pub key_metadata_preservation_data: u64,
    pub metadata_preservation_data_empty: bool,
    pub file_sha256_present: bool,
    pub link_target_present: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestEntriesWireCore {
    pub root_map_len: u64,
    pub key_object_id: u64,
    pub object_id: u64,
    pub key_chunk_size: u64,
    pub chunk_size: u64,
    pub key_file_entries: u64,
    pub file_entries_len: u64,
    pub nonempty_regular: RichRegularFileWireCore,
    pub empty_regular: RichRegularFileWireCore,
    pub hardlink: LinkEntryWireCore,
    pub symlink: LinkEntryWireCore,
    pub directory: DirectoryEntryWireCore,
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

pub fn regular_file_from_rich(file: RichRegularFileCore) -> RegularFileCore {
    RegularFileCore {
        path_id: file.path_id,
        file_id: file.file_id,
        size_bytes: file.size_bytes,
        file_sha256: file.file_sha256,
        first_chunk_lba_present: file.first_chunk_lba_present,
        first_chunk_lba: file.first_chunk_lba,
        executable_tag: file.executable_tag,
    }
}

pub fn validate_rich_regular_file_core(
    file: RichRegularFileCore,
    chunk_size: u64,
) -> Result<(), RaoManifestError> {
    validate_regular_file_core(regular_file_from_rich(file), chunk_size)?;
    if file.xattr_present {
        if file.xattr_name_id == 0 {
            return Err(RaoManifestError::InvalidManifestField);
        }
    } else if file.xattr_name_id != 0 || file.xattr_value_len != 0 || file.xattr_value_id != 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn validate_hardlink_entry_core(entry: HardlinkEntryCore) -> Result<(), RaoManifestError> {
    if entry.path_id == 0 || entry.file_id == 0 || entry.link_target_path_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if entry.executable_tag > EXECUTABLE_TRUE {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn validate_symlink_entry_core(entry: SymlinkEntryCore) -> Result<(), RaoManifestError> {
    if entry.path_id == 0 || entry.file_id == 0 || entry.link_target_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if entry.executable_tag > EXECUTABLE_TRUE {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn validate_directory_entry_core(entry: DirectoryEntryCore) -> Result<(), RaoManifestError> {
    if entry.path_id == 0 || entry.file_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if entry.executable_tag > EXECUTABLE_TRUE {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn validate_manifest_entries_core(
    manifest: ManifestEntriesCore,
) -> Result<(), RaoManifestError> {
    validate_chunk_size(manifest.chunk_size)?;
    if manifest.object_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if manifest.nonempty_regular.size_bytes == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if manifest.empty_regular.size_bytes != 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    validate_rich_regular_file_core(manifest.nonempty_regular, manifest.chunk_size)?;
    validate_rich_regular_file_core(manifest.empty_regular, manifest.chunk_size)?;
    validate_hardlink_entry_core(manifest.hardlink)?;
    validate_symlink_entry_core(manifest.symlink)?;
    validate_directory_entry_core(manifest.directory)?;
    if manifest.hardlink.link_target_path_id != manifest.nonempty_regular.path_id {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn encode_rich_regular_file_core(
    file: RichRegularFileCore,
    chunk_size: u64,
) -> Result<RichRegularFileWireCore, RaoManifestError> {
    validate_rich_regular_file_core(file, chunk_size)?;
    let chunk_count = chunk_count_core(file.size_bytes, chunk_size)?;
    Ok(RichRegularFileWireCore {
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
        metadata_preservation_data_map_len: if file.xattr_present {
            METADATA_PRESERVATION_XATTRS_MAP_LEN
        } else {
            METADATA_PRESERVATION_EMPTY_MAP_LEN
        },
        metadata_key_xattrs: if file.xattr_present {
            METADATA_KEY_XATTRS
        } else {
            0
        },
        xattrs_map_len: if file.xattr_present {
            XATTRS_ONE_ENTRY_LEN
        } else {
            0
        },
        xattr_name_id: file.xattr_name_id,
        xattr_value_len: file.xattr_value_len,
        xattr_value_id: file.xattr_value_id,
    })
}

pub fn encode_hardlink_entry_core(
    entry: HardlinkEntryCore,
) -> Result<LinkEntryWireCore, RaoManifestError> {
    validate_hardlink_entry_core(entry)?;
    Ok(LinkEntryWireCore {
        map_len: FILE_ENTRY_LINK_MAP_LEN,
        key_path: LINK_KEY_PATH,
        path_id: entry.path_id,
        key_file_id: LINK_KEY_FILE_ID,
        file_id: entry.file_id,
        key_entry_type: LINK_KEY_ENTRY_TYPE,
        entry_type: ENTRY_TYPE_HARDLINK,
        key_executable: LINK_KEY_EXECUTABLE,
        executable_tag: entry.executable_tag,
        key_size_bytes: LINK_KEY_SIZE_BYTES,
        size_bytes: 0,
        key_chunk_count: LINK_KEY_CHUNK_COUNT,
        chunk_count: 0,
        key_link_target: LINK_KEY_LINK_TARGET,
        link_target_id: entry.link_target_path_id,
        key_first_chunk_lba: LINK_KEY_FIRST_CHUNK_LBA,
        first_chunk_lba_is_null: true,
        key_metadata_preservation_data: LINK_KEY_METADATA_PRESERVATION_DATA,
        metadata_preservation_data_empty: true,
        file_sha256_present: false,
    })
}

pub fn encode_symlink_entry_core(
    entry: SymlinkEntryCore,
) -> Result<LinkEntryWireCore, RaoManifestError> {
    validate_symlink_entry_core(entry)?;
    Ok(LinkEntryWireCore {
        map_len: FILE_ENTRY_LINK_MAP_LEN,
        key_path: LINK_KEY_PATH,
        path_id: entry.path_id,
        key_file_id: LINK_KEY_FILE_ID,
        file_id: entry.file_id,
        key_entry_type: LINK_KEY_ENTRY_TYPE,
        entry_type: ENTRY_TYPE_SYMLINK,
        key_executable: LINK_KEY_EXECUTABLE,
        executable_tag: entry.executable_tag,
        key_size_bytes: LINK_KEY_SIZE_BYTES,
        size_bytes: 0,
        key_chunk_count: LINK_KEY_CHUNK_COUNT,
        chunk_count: 0,
        key_link_target: LINK_KEY_LINK_TARGET,
        link_target_id: entry.link_target_id,
        key_first_chunk_lba: LINK_KEY_FIRST_CHUNK_LBA,
        first_chunk_lba_is_null: true,
        key_metadata_preservation_data: LINK_KEY_METADATA_PRESERVATION_DATA,
        metadata_preservation_data_empty: true,
        file_sha256_present: false,
    })
}

pub fn encode_directory_entry_core(
    entry: DirectoryEntryCore,
) -> Result<DirectoryEntryWireCore, RaoManifestError> {
    validate_directory_entry_core(entry)?;
    Ok(DirectoryEntryWireCore {
        map_len: FILE_ENTRY_DIRECTORY_MAP_LEN,
        key_path: DIRECTORY_KEY_PATH,
        path_id: entry.path_id,
        key_file_id: DIRECTORY_KEY_FILE_ID,
        file_id: entry.file_id,
        key_entry_type: DIRECTORY_KEY_ENTRY_TYPE,
        entry_type: ENTRY_TYPE_DIRECTORY,
        key_executable: DIRECTORY_KEY_EXECUTABLE,
        executable_tag: entry.executable_tag,
        key_size_bytes: DIRECTORY_KEY_SIZE_BYTES,
        size_bytes: 0,
        key_chunk_count: DIRECTORY_KEY_CHUNK_COUNT,
        chunk_count: 0,
        key_first_chunk_lba: DIRECTORY_KEY_FIRST_CHUNK_LBA,
        first_chunk_lba_is_null: true,
        key_metadata_preservation_data: DIRECTORY_KEY_METADATA_PRESERVATION_DATA,
        metadata_preservation_data_empty: true,
        file_sha256_present: false,
        link_target_present: false,
    })
}

pub fn encode_manifest_entries_core(
    manifest: ManifestEntriesCore,
) -> Result<ManifestEntriesWireCore, RaoManifestError> {
    validate_manifest_entries_core(manifest)?;
    let nonempty_regular =
        encode_rich_regular_file_core(manifest.nonempty_regular, manifest.chunk_size)?;
    let empty_regular = encode_rich_regular_file_core(manifest.empty_regular, manifest.chunk_size)?;
    let hardlink = encode_hardlink_entry_core(manifest.hardlink)?;
    let symlink = encode_symlink_entry_core(manifest.symlink)?;
    let directory = encode_directory_entry_core(manifest.directory)?;
    Ok(ManifestEntriesWireCore {
        root_map_len: ROOT_MAP_LEN,
        key_object_id: ROOT_KEY_OBJECT_ID,
        object_id: manifest.object_id,
        key_chunk_size: ROOT_KEY_CHUNK_SIZE,
        chunk_size: manifest.chunk_size,
        key_file_entries: ROOT_KEY_FILE_ENTRIES,
        file_entries_len: FILE_ENTRIES_LEN_BOUNDED,
        nonempty_regular,
        empty_regular,
        hardlink,
        symlink,
        directory,
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

pub fn decode_rich_regular_file_core(
    wire: RichRegularFileWireCore,
    chunk_size: u64,
) -> Result<RichRegularFileCore, RaoManifestError> {
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

    let xattr_present =
        if wire.metadata_preservation_data_map_len == METADATA_PRESERVATION_EMPTY_MAP_LEN {
            if wire.metadata_key_xattrs != 0
                || wire.xattrs_map_len != 0
                || wire.xattr_name_id != 0
                || wire.xattr_value_len != 0
                || wire.xattr_value_id != 0
            {
                return Err(RaoManifestError::InvalidManifestField);
            }
            false
        } else if wire.metadata_preservation_data_map_len == METADATA_PRESERVATION_XATTRS_MAP_LEN {
            if wire.metadata_key_xattrs != METADATA_KEY_XATTRS
                || wire.xattrs_map_len != XATTRS_ONE_ENTRY_LEN
                || wire.xattr_name_id == 0
            {
                return Err(RaoManifestError::InvalidManifestField);
            }
            true
        } else {
            return Err(RaoManifestError::InvalidManifestField);
        };

    let file = RichRegularFileCore {
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
        xattr_present,
        xattr_name_id: wire.xattr_name_id,
        xattr_value_len: wire.xattr_value_len,
        xattr_value_id: wire.xattr_value_id,
    };
    validate_rich_regular_file_core(file, chunk_size)?;
    Ok(file)
}

pub fn decode_hardlink_entry_core(
    wire: LinkEntryWireCore,
) -> Result<HardlinkEntryCore, RaoManifestError> {
    if wire.map_len != FILE_ENTRY_LINK_MAP_LEN {
        return Err(RaoManifestError::MissingRequiredManifestField);
    }
    if wire.key_path != LINK_KEY_PATH
        || wire.key_file_id != LINK_KEY_FILE_ID
        || wire.key_entry_type != LINK_KEY_ENTRY_TYPE
        || wire.key_executable != LINK_KEY_EXECUTABLE
        || wire.key_size_bytes != LINK_KEY_SIZE_BYTES
        || wire.key_chunk_count != LINK_KEY_CHUNK_COUNT
        || wire.key_link_target != LINK_KEY_LINK_TARGET
        || wire.key_first_chunk_lba != LINK_KEY_FIRST_CHUNK_LBA
        || wire.key_metadata_preservation_data != LINK_KEY_METADATA_PRESERVATION_DATA
    {
        return Err(RaoManifestError::InvalidCborEncoding);
    }
    if wire.entry_type != ENTRY_TYPE_HARDLINK
        || wire.size_bytes != 0
        || wire.chunk_count != 0
        || !wire.first_chunk_lba_is_null
        || !wire.metadata_preservation_data_empty
        || wire.file_sha256_present
    {
        return Err(RaoManifestError::InvalidManifestField);
    }
    let entry = HardlinkEntryCore {
        path_id: wire.path_id,
        file_id: wire.file_id,
        link_target_path_id: wire.link_target_id,
        executable_tag: wire.executable_tag,
    };
    validate_hardlink_entry_core(entry)?;
    Ok(entry)
}

pub fn decode_symlink_entry_core(
    wire: LinkEntryWireCore,
) -> Result<SymlinkEntryCore, RaoManifestError> {
    if wire.map_len != FILE_ENTRY_LINK_MAP_LEN {
        return Err(RaoManifestError::MissingRequiredManifestField);
    }
    if wire.key_path != LINK_KEY_PATH
        || wire.key_file_id != LINK_KEY_FILE_ID
        || wire.key_entry_type != LINK_KEY_ENTRY_TYPE
        || wire.key_executable != LINK_KEY_EXECUTABLE
        || wire.key_size_bytes != LINK_KEY_SIZE_BYTES
        || wire.key_chunk_count != LINK_KEY_CHUNK_COUNT
        || wire.key_link_target != LINK_KEY_LINK_TARGET
        || wire.key_first_chunk_lba != LINK_KEY_FIRST_CHUNK_LBA
        || wire.key_metadata_preservation_data != LINK_KEY_METADATA_PRESERVATION_DATA
    {
        return Err(RaoManifestError::InvalidCborEncoding);
    }
    if wire.entry_type != ENTRY_TYPE_SYMLINK
        || wire.size_bytes != 0
        || wire.chunk_count != 0
        || !wire.first_chunk_lba_is_null
        || !wire.metadata_preservation_data_empty
        || wire.file_sha256_present
    {
        return Err(RaoManifestError::InvalidManifestField);
    }
    let entry = SymlinkEntryCore {
        path_id: wire.path_id,
        file_id: wire.file_id,
        link_target_id: wire.link_target_id,
        executable_tag: wire.executable_tag,
    };
    validate_symlink_entry_core(entry)?;
    Ok(entry)
}

pub fn decode_directory_entry_core(
    wire: DirectoryEntryWireCore,
) -> Result<DirectoryEntryCore, RaoManifestError> {
    if wire.map_len != FILE_ENTRY_DIRECTORY_MAP_LEN {
        return Err(RaoManifestError::MissingRequiredManifestField);
    }
    if wire.key_path != DIRECTORY_KEY_PATH
        || wire.key_file_id != DIRECTORY_KEY_FILE_ID
        || wire.key_entry_type != DIRECTORY_KEY_ENTRY_TYPE
        || wire.key_executable != DIRECTORY_KEY_EXECUTABLE
        || wire.key_size_bytes != DIRECTORY_KEY_SIZE_BYTES
        || wire.key_chunk_count != DIRECTORY_KEY_CHUNK_COUNT
        || wire.key_first_chunk_lba != DIRECTORY_KEY_FIRST_CHUNK_LBA
        || wire.key_metadata_preservation_data != DIRECTORY_KEY_METADATA_PRESERVATION_DATA
    {
        return Err(RaoManifestError::InvalidCborEncoding);
    }
    if wire.entry_type != ENTRY_TYPE_DIRECTORY
        || wire.size_bytes != 0
        || wire.chunk_count != 0
        || !wire.first_chunk_lba_is_null
        || !wire.metadata_preservation_data_empty
        || wire.file_sha256_present
        || wire.link_target_present
    {
        return Err(RaoManifestError::InvalidManifestField);
    }
    let entry = DirectoryEntryCore {
        path_id: wire.path_id,
        file_id: wire.file_id,
        executable_tag: wire.executable_tag,
    };
    validate_directory_entry_core(entry)?;
    Ok(entry)
}

pub fn decode_manifest_entries_core(
    wire: ManifestEntriesWireCore,
    reader_chunk_size: u64,
) -> Result<ManifestEntriesCore, RaoManifestError> {
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
    if wire.file_entries_len != FILE_ENTRIES_LEN_BOUNDED {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if !wire.object_metadata_empty || !wire.external_references_empty {
        return Err(RaoManifestError::InvalidManifestField);
    }

    let nonempty_regular = decode_rich_regular_file_core(wire.nonempty_regular, reader_chunk_size)?;
    let empty_regular = decode_rich_regular_file_core(wire.empty_regular, reader_chunk_size)?;
    let hardlink = decode_hardlink_entry_core(wire.hardlink)?;
    let symlink = decode_symlink_entry_core(wire.symlink)?;
    let directory = decode_directory_entry_core(wire.directory)?;
    let manifest = ManifestEntriesCore {
        object_id: wire.object_id,
        caller_object_id: wire.caller_object_id,
        chunk_size: wire.chunk_size,
        nonempty_regular,
        empty_regular,
        hardlink,
        symlink,
        directory,
    };
    validate_manifest_entries_core(manifest)?;
    Ok(manifest)
}

pub fn distinct5_core(
    first: u64,
    second: u64,
    third: u64,
    fourth: u64,
    fifth: u64,
) -> Result<(), RaoManifestError> {
    if first == second {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if first == third {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if first == fourth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if first == fifth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if second == third {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if second == fourth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if second == fifth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if third == fourth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if third == fifth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if fourth == fifth {
        return Err(RaoManifestError::InvalidManifestField);
    }
    Ok(())
}

pub fn hardlink_target_seen_regular_prefix_two_core(
    target: u64,
    first_regular_path: u64,
    second_regular_path: u64,
) -> Result<(), RaoManifestError> {
    if target == first_regular_path {
        return Ok(());
    }
    if target == second_regular_path {
        return Ok(());
    }
    Err(RaoManifestError::InvalidManifestField)
}

pub fn validate_manifest_array_core(manifest: ManifestEntriesCore) -> Result<(), RaoManifestError> {
    validate_chunk_size(manifest.chunk_size)?;
    if manifest.object_id == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if manifest.nonempty_regular.size_bytes == 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if manifest.empty_regular.size_bytes != 0 {
        return Err(RaoManifestError::InvalidManifestField);
    }
    validate_rich_regular_file_core(manifest.nonempty_regular, manifest.chunk_size)?;
    validate_rich_regular_file_core(manifest.empty_regular, manifest.chunk_size)?;
    validate_hardlink_entry_core(manifest.hardlink)?;
    validate_symlink_entry_core(manifest.symlink)?;
    validate_directory_entry_core(manifest.directory)?;
    distinct5_core(
        manifest.nonempty_regular.path_id,
        manifest.empty_regular.path_id,
        manifest.hardlink.path_id,
        manifest.symlink.path_id,
        manifest.directory.path_id,
    )?;
    distinct5_core(
        manifest.nonempty_regular.file_id,
        manifest.empty_regular.file_id,
        manifest.hardlink.file_id,
        manifest.symlink.file_id,
        manifest.directory.file_id,
    )?;
    hardlink_target_seen_regular_prefix_two_core(
        manifest.hardlink.link_target_path_id,
        manifest.nonempty_regular.path_id,
        manifest.empty_regular.path_id,
    )?;
    Ok(())
}

pub fn encode_manifest_array_core(
    manifest: ManifestEntriesCore,
) -> Result<ManifestEntriesWireCore, RaoManifestError> {
    validate_manifest_array_core(manifest)?;
    let nonempty_regular =
        encode_rich_regular_file_core(manifest.nonempty_regular, manifest.chunk_size)?;
    let empty_regular = encode_rich_regular_file_core(manifest.empty_regular, manifest.chunk_size)?;
    let hardlink = encode_hardlink_entry_core(manifest.hardlink)?;
    let symlink = encode_symlink_entry_core(manifest.symlink)?;
    let directory = encode_directory_entry_core(manifest.directory)?;
    Ok(ManifestEntriesWireCore {
        root_map_len: ROOT_MAP_LEN,
        key_object_id: ROOT_KEY_OBJECT_ID,
        object_id: manifest.object_id,
        key_chunk_size: ROOT_KEY_CHUNK_SIZE,
        chunk_size: manifest.chunk_size,
        key_file_entries: ROOT_KEY_FILE_ENTRIES,
        file_entries_len: FILE_ENTRIES_LEN_BOUNDED,
        nonempty_regular,
        empty_regular,
        hardlink,
        symlink,
        directory,
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

pub fn decode_manifest_array_core(
    wire: ManifestEntriesWireCore,
    reader_chunk_size: u64,
) -> Result<ManifestEntriesCore, RaoManifestError> {
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
    if wire.file_entries_len != FILE_ENTRIES_LEN_BOUNDED {
        return Err(RaoManifestError::InvalidManifestField);
    }
    if !wire.object_metadata_empty || !wire.external_references_empty {
        return Err(RaoManifestError::InvalidManifestField);
    }

    let nonempty_regular = decode_rich_regular_file_core(wire.nonempty_regular, reader_chunk_size)?;
    let empty_regular = decode_rich_regular_file_core(wire.empty_regular, reader_chunk_size)?;
    let hardlink = decode_hardlink_entry_core(wire.hardlink)?;
    let symlink = decode_symlink_entry_core(wire.symlink)?;
    let directory = decode_directory_entry_core(wire.directory)?;
    let manifest = ManifestEntriesCore {
        object_id: wire.object_id,
        caller_object_id: wire.caller_object_id,
        chunk_size: wire.chunk_size,
        nonempty_regular,
        empty_regular,
        hardlink,
        symlink,
        directory,
    };
    validate_manifest_array_core(manifest)?;
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

    fn rich_regular_file(
        path_id: u64,
        file_id: u64,
        size_bytes: u64,
        xattr_present: bool,
    ) -> RichRegularFileCore {
        RichRegularFileCore {
            path_id,
            file_id,
            size_bytes,
            file_sha256: DIGEST,
            first_chunk_lba_present: size_bytes != 0,
            first_chunk_lba: if size_bytes == 0 { 0 } else { 7 },
            executable_tag: EXECUTABLE_NULL,
            xattr_present,
            xattr_name_id: if xattr_present { 901 } else { 0 },
            xattr_value_len: if xattr_present { 13 } else { 0 },
            xattr_value_id: if xattr_present { 902 } else { 0 },
        }
    }

    fn manifest_entries() -> ManifestEntriesCore {
        ManifestEntriesCore {
            object_id: 133,
            caller_object_id: 144,
            chunk_size: 512,
            nonempty_regular: rich_regular_file(201, 301, 1025, true),
            empty_regular: rich_regular_file(202, 302, 0, false),
            hardlink: HardlinkEntryCore {
                path_id: 203,
                file_id: 303,
                link_target_path_id: 201,
                executable_tag: EXECUTABLE_NULL,
            },
            symlink: SymlinkEntryCore {
                path_id: 204,
                file_id: 304,
                link_target_id: 404,
                executable_tag: EXECUTABLE_NULL,
            },
            directory: DirectoryEntryCore {
                path_id: 205,
                file_id: 305,
                executable_tag: EXECUTABLE_NULL,
            },
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
            "if let Some(entry_type) = layout.entry_type.manifest_value()",
            "map.push((\"entry_type\", CborValue::Text(entry_type.to_string())));",
            "if let Some(link_target) = &layout.link_target",
            "map.push((\"link_target\", CborValue::Text(link_target.clone())));",
            "fn metadata_preservation_data(layout: &RemTarFileLayout) -> CborValue",
            "if layout.xattrs.is_empty()",
            "let xattrs = CborValue::Map(",
            "let mut seen_paths = BTreeSet::new();",
            "let mut seen_file_ids = BTreeSet::new();",
            "let mut seen_regular_paths = BTreeSet::new();",
            "if !seen_paths.insert(spec.path.clone())",
            "duplicate payload path",
            "if !seen_file_ids.insert(spec.file_id.clone())",
            "duplicate file_id",
            "seen_regular_paths.insert(spec.path.clone());",
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
            "let mut seen_regular_paths = BTreeSet::new();",
            "if let Some(path) = validate_file_entry(entry, reader_chunk_size, &seen_regular_paths)?",
            "seen_regular_paths.insert(path);",
            "let expected_chunk_count = if size_bytes == 0 {\n        0\n    } else {\n        (size_bytes - 1) / reader_chunk_size as u64 + 1\n    };",
            "if chunk_count != expected_chunk_count",
            "first_chunk_lba must be null when size_bytes is zero",
            "first_chunk_lba must be unsigned when size_bytes is nonzero",
            "regular entry missing file_sha256",
            "if file_sha256.len() != 32",
            "Some(\"hardlink\")",
            "hardlink entry must not have xattrs",
            "if !seen_regular_paths.contains(target)",
            "Some(\"symlink\")",
            "Some(\"directory\")",
            "xattrs_from_metadata_preservation_data",
            "xattr names must not be empty",
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
            "pub fn encode_manifest_entries_core(",
            "pub fn decode_manifest_entries_core(",
            "pub fn distinct5_core(",
            "pub fn hardlink_target_seen_regular_prefix_two_core(",
            "pub fn validate_manifest_array_core(",
            "pub fn encode_manifest_array_core(",
            "pub fn decode_manifest_array_core(",
            "pub fn validate_hardlink_entry_core(",
            "pub fn validate_symlink_entry_core(",
            "pub fn validate_directory_entry_core(",
            "root_map_len: ROOT_MAP_LEN,",
            "file_entries_len: FILE_ENTRIES_LEN_ONE,",
            "file_entries_len: FILE_ENTRIES_LEN_BOUNDED,",
            "schema_version: SCHEMA_VERSION,",
            "metadata_preservation_data_empty: true,",
            "metadata_preservation_data_map_len: if file.xattr_present",
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

    #[test]
    fn manifest_entries_core_round_trips() {
        let manifest = manifest_entries();
        let wire = encode_manifest_entries_core(manifest).unwrap();
        assert_eq!(wire.file_entries_len, FILE_ENTRIES_LEN_BOUNDED);
        assert_eq!(wire.nonempty_regular.chunk_count, 3);
        assert_eq!(
            wire.nonempty_regular.metadata_preservation_data_map_len,
            METADATA_PRESERVATION_XATTRS_MAP_LEN
        );
        assert_eq!(wire.empty_regular.chunk_count, 0);
        assert!(wire.empty_regular.first_chunk_lba_is_null);
        assert_eq!(wire.hardlink.entry_type, ENTRY_TYPE_HARDLINK);
        assert_eq!(wire.symlink.entry_type, ENTRY_TYPE_SYMLINK);
        assert_eq!(wire.directory.entry_type, ENTRY_TYPE_DIRECTORY);
        assert_eq!(
            decode_manifest_entries_core(wire, manifest.chunk_size).unwrap(),
            manifest
        );
    }

    #[test]
    fn manifest_entries_reject_bad_hardlink_order() {
        let mut manifest = manifest_entries();
        manifest.hardlink.link_target_path_id = manifest.empty_regular.path_id;
        assert_eq!(
            validate_manifest_entries_core(manifest),
            Err(RaoManifestError::InvalidManifestField)
        );

        let manifest = manifest_entries();
        let mut wire = encode_manifest_entries_core(manifest).unwrap();
        wire.hardlink.link_target_id = manifest.empty_regular.path_id;
        assert_eq!(
            decode_manifest_entries_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );
    }

    #[test]
    fn manifest_entries_decode_rejects_bad_xattr_shape() {
        let manifest = manifest_entries();
        let mut wire = encode_manifest_entries_core(manifest).unwrap();
        wire.nonempty_regular.xattr_name_id = 0;
        assert_eq!(
            decode_manifest_entries_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );

        let manifest = manifest_entries();
        let mut wire = encode_manifest_entries_core(manifest).unwrap();
        wire.empty_regular.xattr_name_id = 77;
        assert_eq!(
            decode_manifest_entries_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );
    }

    #[test]
    fn manifest_entries_decode_rejects_nonregular_file_hash() {
        let manifest = manifest_entries();
        let mut wire = encode_manifest_entries_core(manifest).unwrap();
        wire.hardlink.file_sha256_present = true;
        assert_eq!(
            decode_manifest_entries_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );
    }

    #[test]
    fn manifest_array_core_round_trips_with_accumulated_hardlink_target() {
        let mut manifest = manifest_entries();
        manifest.hardlink.link_target_path_id = manifest.empty_regular.path_id;
        let wire = encode_manifest_array_core(manifest).unwrap();
        assert_eq!(wire.file_entries_len, FILE_ENTRIES_LEN_BOUNDED);
        assert_eq!(
            decode_manifest_array_core(wire, manifest.chunk_size).unwrap(),
            manifest
        );
    }

    #[test]
    fn manifest_array_rejects_duplicate_path_and_file_ids() {
        let mut manifest = manifest_entries();
        manifest.directory.path_id = manifest.symlink.path_id;
        assert_eq!(
            validate_manifest_array_core(manifest),
            Err(RaoManifestError::InvalidManifestField)
        );

        let mut manifest = manifest_entries();
        manifest.directory.file_id = manifest.symlink.file_id;
        assert_eq!(
            validate_manifest_array_core(manifest),
            Err(RaoManifestError::InvalidManifestField)
        );
    }

    #[test]
    fn manifest_array_rejects_unseen_hardlink_target() {
        let mut manifest = manifest_entries();
        manifest.hardlink.link_target_path_id = manifest.symlink.path_id;
        assert_eq!(
            validate_manifest_array_core(manifest),
            Err(RaoManifestError::InvalidManifestField)
        );

        let manifest = manifest_entries();
        let mut wire = encode_manifest_array_core(manifest).unwrap();
        wire.hardlink.link_target_id = manifest.directory.path_id;
        assert_eq!(
            decode_manifest_array_core(wire, manifest.chunk_size),
            Err(RaoManifestError::InvalidManifestField)
        );
    }
}
