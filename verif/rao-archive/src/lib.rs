//! Verification extraction of the RAO archive composition core.
//!
//! This crate is a standalone, dependency-free model of the scalar composition
//! checks that tie the already-proved RAO header, metadata, and fixed-capacity
//! manifest-array cores together: one object id, one chunk size, bounded
//! metadata-frame length, metadata plaintext-size arithmetic, manifest
//! duplicate/id/target checks, and deterministic top-level writer shape. The
//! component crates still own their narrower proofs, and production still owns
//! exact CBOR bytes, `Vec`/`String` traversal, tar/pax records, hashing,
//! allocation, encryption, and IO. The `drift_guard` test pins the proof-facing
//! snippets this composition extraction mirrors; if it fails, this extraction
//! and its Lean proof must be re-synced.

#![allow(clippy::manual_is_multiple_of)]
// Keep explicit modulo tests: the extraction mirrors the proof-facing RAO
// component cores and the Lean proof reasons about the generated branches.

pub const RAO_HEADER_LEN_U16: u16 = 128;
pub const FORMAT_VERSION: u8 = 1;
pub const SUITE_ID_HKDF_SHA256_CHACHA20POLY1305: u8 = 0x01;
pub const CHUNK_SIZE_GRANULARITY: u64 = 512;
pub const RAO_METADATA_FRAME_MIN_LEN: u64 = 17;
pub const RAO_MAX_METADATA_FRAME_LEN: u64 = 16_777_216;

pub const METADATA_VERSION: u64 = 1;
pub const METADATA_REQUIRED_MAP_LEN: u64 = 4;
pub const DIGEST_BYTE_LEN: u64 = 32;
pub const CHACHA20POLY1305_TAG_LEN: u64 = 16;

pub const MANIFEST_ROOT_MAP_LEN: u64 = 7;
pub const MANIFEST_FILE_ENTRIES_LEN: u64 = 5;
pub const MANIFEST_SCHEMA_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaoArchiveError {
    InvalidHeaderField,
    InvalidMetadataField,
    InvalidManifestField,
    InvalidArchiveField,
    InvalidWireShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestWords {
    pub w0: u64,
    pub w1: u64,
    pub w2: u64,
    pub w3: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderCore {
    pub object_id: u64,
    pub chunk_size: u64,
    pub key_id_nonzero: bool,
    pub hkdf_salt_nonzero: bool,
    pub metadata_frame_len: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataCore {
    pub plaintext_size: u64,
    pub digest: DigestWords,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestArrayCore {
    pub object_id: u64,
    pub caller_object_id: u64,
    pub chunk_size: u64,
    pub nonempty_regular_path_id: u64,
    pub nonempty_regular_file_id: u64,
    pub nonempty_regular_size_bytes: u64,
    pub nonempty_regular_sha256: DigestWords,
    pub empty_regular_path_id: u64,
    pub empty_regular_file_id: u64,
    pub hardlink_path_id: u64,
    pub hardlink_file_id: u64,
    pub hardlink_target_path_id: u64,
    pub symlink_path_id: u64,
    pub symlink_file_id: u64,
    pub symlink_target_id: u64,
    pub directory_path_id: u64,
    pub directory_file_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveCore {
    pub header: HeaderCore,
    pub metadata: MetadataCore,
    pub manifest: ManifestArrayCore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderWireCore {
    pub magic_rao1: bool,
    pub header_len: u16,
    pub format_version: u8,
    pub suite_id: u8,
    pub object_id: u64,
    pub chunk_size: u64,
    pub flags: u32,
    pub key_id_nonzero: bool,
    pub hkdf_salt_nonzero: bool,
    pub metadata_frame_len: u64,
    pub reserved_zero: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataWireCore {
    pub map_len: u64,
    pub metadata_version: u64,
    pub plaintext_size: u64,
    pub digest_alg_sha256: bool,
    pub digest_byte_len: u64,
    pub digest: DigestWords,
    pub trailing_data: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestArrayWireCore {
    pub root_map_len: u64,
    pub object_id: u64,
    pub caller_object_id: u64,
    pub chunk_size: u64,
    pub file_entries_len: u64,
    pub schema_version: u64,
    pub object_metadata_empty: bool,
    pub external_references_empty: bool,
    pub nonempty_regular_path_id: u64,
    pub nonempty_regular_file_id: u64,
    pub nonempty_regular_size_bytes: u64,
    pub nonempty_regular_sha256: DigestWords,
    pub empty_regular_path_id: u64,
    pub empty_regular_file_id: u64,
    pub hardlink_path_id: u64,
    pub hardlink_file_id: u64,
    pub hardlink_target_path_id: u64,
    pub symlink_path_id: u64,
    pub symlink_file_id: u64,
    pub symlink_target_id: u64,
    pub directory_path_id: u64,
    pub directory_file_id: u64,
    pub trailing_data: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArchiveWireCore {
    pub header: HeaderWireCore,
    pub metadata: MetadataWireCore,
    pub manifest: ManifestArrayWireCore,
}

pub fn checked_add(a: u64, b: u64) -> Result<u64, RaoArchiveError> {
    match a.checked_add(b) {
        Some(sum) => Ok(sum),
        None => Err(RaoArchiveError::InvalidMetadataField),
    }
}

pub fn checked_mul(a: u64, b: u64) -> Result<u64, RaoArchiveError> {
    match a.checked_mul(b) {
        Some(product) => Ok(product),
        None => Err(RaoArchiveError::InvalidMetadataField),
    }
}

pub fn validate_chunk_size(chunk_size: u64) -> Result<(), RaoArchiveError> {
    if chunk_size == 0 || chunk_size % CHUNK_SIZE_GRANULARITY != 0 {
        return Err(RaoArchiveError::InvalidHeaderField);
    }
    Ok(())
}

pub fn validate_header_core(header: HeaderCore) -> Result<(), RaoArchiveError> {
    validate_chunk_size(header.chunk_size)?;
    if header.object_id == 0 {
        return Err(RaoArchiveError::InvalidHeaderField);
    }
    if !header.key_id_nonzero || !header.hkdf_salt_nonzero {
        return Err(RaoArchiveError::InvalidHeaderField);
    }
    if header.metadata_frame_len < RAO_METADATA_FRAME_MIN_LEN {
        return Err(RaoArchiveError::InvalidHeaderField);
    }
    if header.metadata_frame_len > RAO_MAX_METADATA_FRAME_LEN {
        return Err(RaoArchiveError::InvalidHeaderField);
    }
    Ok(())
}

pub fn validate_metadata_core(
    metadata: MetadataCore,
    chunk_size: u64,
) -> Result<(), RaoArchiveError> {
    validate_chunk_size(chunk_size)?;
    if metadata.plaintext_size == 0 || metadata.plaintext_size % chunk_size != 0 {
        return Err(RaoArchiveError::InvalidMetadataField);
    }
    let chunk_count = metadata.plaintext_size / chunk_size;
    let tag_bytes = checked_mul(CHACHA20POLY1305_TAG_LEN, chunk_count)?;
    checked_add(metadata.plaintext_size, tag_bytes)?;
    Ok(())
}

pub fn distinct5_core(
    first: u64,
    second: u64,
    third: u64,
    fourth: u64,
    fifth: u64,
) -> Result<(), RaoArchiveError> {
    if first == second {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if first == third {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if first == fourth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if first == fifth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if second == third {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if second == fourth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if second == fifth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if third == fourth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if third == fifth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if fourth == fifth {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    Ok(())
}

pub fn hardlink_target_seen_regular_prefix_two_core(
    target: u64,
    first_regular_path: u64,
    second_regular_path: u64,
) -> Result<(), RaoArchiveError> {
    if target == first_regular_path {
        return Ok(());
    }
    if target == second_regular_path {
        return Ok(());
    }
    Err(RaoArchiveError::InvalidManifestField)
}

pub fn validate_manifest_array_core(manifest: ManifestArrayCore) -> Result<(), RaoArchiveError> {
    validate_chunk_size(manifest.chunk_size)?;
    if manifest.object_id == 0 {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if manifest.nonempty_regular_path_id == 0
        || manifest.nonempty_regular_file_id == 0
        || manifest.empty_regular_path_id == 0
        || manifest.empty_regular_file_id == 0
        || manifest.hardlink_path_id == 0
        || manifest.hardlink_file_id == 0
        || manifest.symlink_path_id == 0
        || manifest.symlink_file_id == 0
        || manifest.symlink_target_id == 0
        || manifest.directory_path_id == 0
        || manifest.directory_file_id == 0
    {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if manifest.nonempty_regular_size_bytes == 0 {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    if manifest.nonempty_regular_size_bytes % manifest.chunk_size != 0 {
        return Err(RaoArchiveError::InvalidManifestField);
    }
    distinct5_core(
        manifest.nonempty_regular_path_id,
        manifest.empty_regular_path_id,
        manifest.hardlink_path_id,
        manifest.symlink_path_id,
        manifest.directory_path_id,
    )?;
    distinct5_core(
        manifest.nonempty_regular_file_id,
        manifest.empty_regular_file_id,
        manifest.hardlink_file_id,
        manifest.symlink_file_id,
        manifest.directory_file_id,
    )?;
    hardlink_target_seen_regular_prefix_two_core(
        manifest.hardlink_target_path_id,
        manifest.nonempty_regular_path_id,
        manifest.empty_regular_path_id,
    )?;
    Ok(())
}

pub fn validate_archive_core(archive: ArchiveCore) -> Result<(), RaoArchiveError> {
    validate_header_core(archive.header)?;
    validate_metadata_core(archive.metadata, archive.header.chunk_size)?;
    validate_manifest_array_core(archive.manifest)?;
    if archive.header.chunk_size != archive.manifest.chunk_size {
        return Err(RaoArchiveError::InvalidArchiveField);
    }
    if archive.header.object_id != archive.manifest.object_id {
        return Err(RaoArchiveError::InvalidArchiveField);
    }
    Ok(())
}

pub fn encode_archive_core(archive: ArchiveCore) -> Result<ArchiveWireCore, RaoArchiveError> {
    validate_archive_core(archive)?;
    Ok(ArchiveWireCore {
        header: HeaderWireCore {
            magic_rao1: true,
            header_len: RAO_HEADER_LEN_U16,
            format_version: FORMAT_VERSION,
            suite_id: SUITE_ID_HKDF_SHA256_CHACHA20POLY1305,
            object_id: archive.header.object_id,
            chunk_size: archive.header.chunk_size,
            flags: 0,
            key_id_nonzero: archive.header.key_id_nonzero,
            hkdf_salt_nonzero: archive.header.hkdf_salt_nonzero,
            metadata_frame_len: archive.header.metadata_frame_len,
            reserved_zero: true,
        },
        metadata: MetadataWireCore {
            map_len: METADATA_REQUIRED_MAP_LEN,
            metadata_version: METADATA_VERSION,
            plaintext_size: archive.metadata.plaintext_size,
            digest_alg_sha256: true,
            digest_byte_len: DIGEST_BYTE_LEN,
            digest: archive.metadata.digest,
            trailing_data: false,
        },
        manifest: ManifestArrayWireCore {
            root_map_len: MANIFEST_ROOT_MAP_LEN,
            object_id: archive.manifest.object_id,
            caller_object_id: archive.manifest.caller_object_id,
            chunk_size: archive.manifest.chunk_size,
            file_entries_len: MANIFEST_FILE_ENTRIES_LEN,
            schema_version: MANIFEST_SCHEMA_VERSION,
            object_metadata_empty: true,
            external_references_empty: true,
            nonempty_regular_path_id: archive.manifest.nonempty_regular_path_id,
            nonempty_regular_file_id: archive.manifest.nonempty_regular_file_id,
            nonempty_regular_size_bytes: archive.manifest.nonempty_regular_size_bytes,
            nonempty_regular_sha256: archive.manifest.nonempty_regular_sha256,
            empty_regular_path_id: archive.manifest.empty_regular_path_id,
            empty_regular_file_id: archive.manifest.empty_regular_file_id,
            hardlink_path_id: archive.manifest.hardlink_path_id,
            hardlink_file_id: archive.manifest.hardlink_file_id,
            hardlink_target_path_id: archive.manifest.hardlink_target_path_id,
            symlink_path_id: archive.manifest.symlink_path_id,
            symlink_file_id: archive.manifest.symlink_file_id,
            symlink_target_id: archive.manifest.symlink_target_id,
            directory_path_id: archive.manifest.directory_path_id,
            directory_file_id: archive.manifest.directory_file_id,
            trailing_data: false,
        },
    })
}

pub fn decode_archive_core(wire: ArchiveWireCore) -> Result<ArchiveCore, RaoArchiveError> {
    if !wire.header.magic_rao1
        || wire.header.header_len != RAO_HEADER_LEN_U16
        || wire.header.format_version != FORMAT_VERSION
        || wire.header.suite_id != SUITE_ID_HKDF_SHA256_CHACHA20POLY1305
        || wire.header.flags != 0
        || !wire.header.reserved_zero
    {
        return Err(RaoArchiveError::InvalidWireShape);
    }
    if wire.metadata.trailing_data
        || wire.metadata.map_len != METADATA_REQUIRED_MAP_LEN
        || wire.metadata.metadata_version != METADATA_VERSION
        || !wire.metadata.digest_alg_sha256
        || wire.metadata.digest_byte_len != DIGEST_BYTE_LEN
    {
        return Err(RaoArchiveError::InvalidWireShape);
    }
    if wire.manifest.trailing_data
        || wire.manifest.root_map_len != MANIFEST_ROOT_MAP_LEN
        || wire.manifest.file_entries_len != MANIFEST_FILE_ENTRIES_LEN
        || wire.manifest.schema_version != MANIFEST_SCHEMA_VERSION
        || !wire.manifest.object_metadata_empty
        || !wire.manifest.external_references_empty
    {
        return Err(RaoArchiveError::InvalidWireShape);
    }

    let archive = ArchiveCore {
        header: HeaderCore {
            object_id: wire.header.object_id,
            chunk_size: wire.header.chunk_size,
            key_id_nonzero: wire.header.key_id_nonzero,
            hkdf_salt_nonzero: wire.header.hkdf_salt_nonzero,
            metadata_frame_len: wire.header.metadata_frame_len,
        },
        metadata: MetadataCore {
            plaintext_size: wire.metadata.plaintext_size,
            digest: wire.metadata.digest,
        },
        manifest: ManifestArrayCore {
            object_id: wire.manifest.object_id,
            caller_object_id: wire.manifest.caller_object_id,
            chunk_size: wire.manifest.chunk_size,
            nonempty_regular_path_id: wire.manifest.nonempty_regular_path_id,
            nonempty_regular_file_id: wire.manifest.nonempty_regular_file_id,
            nonempty_regular_size_bytes: wire.manifest.nonempty_regular_size_bytes,
            nonempty_regular_sha256: wire.manifest.nonempty_regular_sha256,
            empty_regular_path_id: wire.manifest.empty_regular_path_id,
            empty_regular_file_id: wire.manifest.empty_regular_file_id,
            hardlink_path_id: wire.manifest.hardlink_path_id,
            hardlink_file_id: wire.manifest.hardlink_file_id,
            hardlink_target_path_id: wire.manifest.hardlink_target_path_id,
            symlink_path_id: wire.manifest.symlink_path_id,
            symlink_file_id: wire.manifest.symlink_file_id,
            symlink_target_id: wire.manifest.symlink_target_id,
            directory_path_id: wire.manifest.directory_path_id,
            directory_file_id: wire.manifest.directory_file_id,
        },
    };
    validate_archive_core(archive)?;
    Ok(archive)
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

    fn valid_archive() -> ArchiveCore {
        ArchiveCore {
            header: HeaderCore {
                object_id: 33,
                chunk_size: 512,
                key_id_nonzero: true,
                hkdf_salt_nonzero: true,
                metadata_frame_len: 64,
            },
            metadata: MetadataCore {
                plaintext_size: 1024,
                digest: DIGEST,
            },
            manifest: ManifestArrayCore {
                object_id: 33,
                caller_object_id: 44,
                chunk_size: 512,
                nonempty_regular_path_id: 11,
                nonempty_regular_file_id: 21,
                nonempty_regular_size_bytes: 512,
                nonempty_regular_sha256: DIGEST,
                empty_regular_path_id: 12,
                empty_regular_file_id: 22,
                hardlink_path_id: 13,
                hardlink_file_id: 23,
                hardlink_target_path_id: 11,
                symlink_path_id: 14,
                symlink_file_id: 24,
                symlink_target_id: 91,
                directory_path_id: 15,
                directory_file_id: 25,
            },
        }
    }

    #[test]
    fn archive_core_round_trips() {
        let archive = valid_archive();
        let wire = encode_archive_core(archive).expect("valid archive encodes");
        assert_eq!(decode_archive_core(wire), Ok(archive));
    }

    #[test]
    fn archive_rejects_chunk_size_mismatch() {
        let mut archive = valid_archive();
        archive.manifest.chunk_size = 1024;
        archive.manifest.nonempty_regular_size_bytes = 1024;
        assert_eq!(
            validate_archive_core(archive),
            Err(RaoArchiveError::InvalidArchiveField)
        );
    }

    #[test]
    fn archive_rejects_object_id_mismatch() {
        let mut archive = valid_archive();
        archive.manifest.object_id = 34;
        assert_eq!(
            validate_archive_core(archive),
            Err(RaoArchiveError::InvalidArchiveField)
        );
    }

    #[test]
    fn archive_rejects_bad_wire_shape() {
        let mut wire = encode_archive_core(valid_archive()).expect("valid archive encodes");
        wire.manifest.trailing_data = true;
        assert_eq!(
            decode_archive_core(wire),
            Err(RaoArchiveError::InvalidWireShape)
        );
    }

    #[test]
    fn archive_rejects_duplicate_manifest_paths() {
        let mut archive = valid_archive();
        archive.manifest.directory_path_id = archive.manifest.symlink_path_id;
        assert_eq!(
            validate_archive_core(archive),
            Err(RaoArchiveError::InvalidManifestField)
        );
    }

    #[test]
    fn drift_guard() {
        let header = include_str!("../../rao-header/src/lib.rs");
        let metadata = include_str!("../../rao-metadata/src/lib.rs");
        let manifest = include_str!("../../rao-manifest/src/lib.rs");
        let manifest_spec = include_str!("../../rao-manifest/lean/RaoManifest/Spec.lean");
        let this = include_str!("lib.rs");

        for snippet in [
            "pub fn validate_header_core(header: HeaderCore)",
            "pub fn validate_metadata_core(",
            "pub fn validate_manifest_array_core(manifest: ManifestEntriesCore)",
            "pub fn encode_manifest_array_core(",
            "pub fn decode_manifest_array_core(",
        ] {
            assert!(
                header.contains(snippet)
                    || metadata.contains(snippet)
                    || manifest.contains(snippet),
                "component extraction drifted: {snippet}"
            );
        }

        assert!(manifest_spec.contains("decode_encode_manifest_array_core_round_trip"));
        assert!(this.contains("pub fn validate_archive_core("));
        assert!(this.contains("pub fn encode_archive_core("));
        assert!(this.contains("pub fn decode_archive_core("));
    }
}
