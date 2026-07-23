//! Reader and forward-scan parser for `rao-v1` objects.

use std::collections::{BTreeMap, BTreeSet};

use remanence_aead::{open_to_vec, OpenReport, RecipientPrivateKey};
use remanence_library::BlockRead;
use sha2::{Digest, Sha256};

use crate::error::{FormatError, FormatGate};
use crate::manifest::{manifest_preservation_metadata, validate_manifest};
use crate::model::{
    BodyLba, RemTarEntryType, RemTarExtensions, FORMAT_ID, MANIFEST_PATH, SCHEMA_VERSION,
    TAR_RECORD_SIZE,
};
use crate::pax::{tar_padding_len, validate_chunk_size};
use crate::tar::{
    TYPE_DIRECTORY, TYPE_HARDLINK, TYPE_PAX_EXTENDED, TYPE_PAX_GLOBAL, TYPE_REGULAR, TYPE_SYMLINK,
};

/// Reader integrity mode for `rao-v1` objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadMode {
    /// Verify every complete regular-file payload and fail on mismatch.
    #[default]
    Restore,
    /// Deliver complete payloads while recording file digest mismatches.
    Salvage,
}

/// Per-entry SHA-256 mismatch observed while reading in salvage mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarDigestMismatch {
    /// Effective path after pax processing.
    pub path: String,
    /// Expected lowercase hex SHA-256 from `REMANENCE.file_sha256`.
    pub expected: String,
    /// Actual lowercase hex SHA-256 over delivered bytes.
    pub actual: String,
}

/// Non-fatal conformance issue reported by a restore-mode RAO reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemTarReadWarning {
    /// Tar EOF was reached without the required final manifest entry.
    MissingManifest,
}

/// One entry recovered from a `rao-v1` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarReadEntry {
    /// Entry kind encoded by the tar typeflag.
    pub entry_type: RemTarEntryType,
    /// Effective UTF-8 path after applying pax `path`.
    pub path: String,
    /// Exact data size after applying pax `size`.
    pub size_bytes: u64,
    /// First body LBA containing file data. Absent for zero-length files.
    pub first_chunk_lba: Option<BodyLba>,
    /// Number of body chunks containing file data.
    pub chunk_count: u64,
    /// Byte offset where file payload begins within the object archive.
    pub data_offset: u64,
    /// Per-file pax records attached to this entry.
    pub pax_records: BTreeMap<String, String>,
    /// Link target. Present only for symbolic-link and hardlink entries.
    pub link_target: Option<String>,
    /// Preserved extended attributes decoded from the manifest when available.
    pub xattrs: crate::model::RemTarXattrs,
    /// Carry-only extension members decoded from the manifest when available.
    pub extensions: RemTarExtensions,
    /// File payload bytes.
    pub data: Vec<u8>,
}

/// One entry encountered by the streaming reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarStreamEntry {
    /// Entry kind encoded by the tar typeflag.
    pub entry_type: RemTarEntryType,
    /// Effective UTF-8 path after applying pax `path`.
    pub path: String,
    /// Exact data size after applying pax `size`.
    pub size_bytes: u64,
    /// First body LBA containing file data. Absent for zero-length files.
    pub first_chunk_lba: Option<BodyLba>,
    /// Number of body chunks containing file data.
    pub chunk_count: u64,
    /// Byte offset where file payload begins within the object archive.
    pub data_offset: u64,
    /// Per-file pax records attached to this entry.
    pub pax_records: BTreeMap<String, String>,
    /// Link target. Present only for symbolic-link and hardlink entries.
    pub link_target: Option<String>,
    /// Preserved extended attributes decoded from the manifest when available.
    pub xattrs: crate::model::RemTarXattrs,
    /// Carry-only extension members decoded from the manifest when available.
    pub extensions: RemTarExtensions,
}

/// Streaming callbacks for restoring a `rao-v1` object.
pub trait RemTarEntrySink {
    /// Called after the entry header has been parsed, before payload bytes.
    fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError>;

    /// Called with one or more contiguous payload bytes for the active entry.
    fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError>;

    /// Called after all payload bytes for the active entry were delivered.
    fn end_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError>;
}

/// Summary returned by the streaming reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarStreamReport {
    /// Global pax records in effect for the object.
    pub global_pax: BTreeMap<String, String>,
    /// Regular-file entries seen in stream order.
    pub entries: Vec<RemTarStreamEntry>,
    /// Raw generated manifest CBOR bytes, if present.
    pub manifest_cbor: Option<Vec<u8>>,
    /// Carry-only object-level extension members decoded from the manifest.
    pub object_extensions: RemTarExtensions,
    /// File digest mismatches observed in [`ReadMode::Salvage`].
    pub digest_mismatches: Vec<RemTarDigestMismatch>,
    /// Non-fatal conformance issues observed in restore mode.
    pub warnings: Vec<RemTarReadWarning>,
}

/// Parsed `rao-v1` object archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarReadObject {
    /// Global pax records in effect for the object.
    pub global_pax: BTreeMap<String, String>,
    /// All regular-file entries, including the generated manifest.
    pub entries: Vec<RemTarReadEntry>,
    /// Raw generated manifest CBOR bytes, if present.
    pub manifest_cbor: Option<Vec<u8>>,
    /// Carry-only object-level extension members decoded from the manifest.
    pub object_extensions: RemTarExtensions,
    /// File digest mismatches observed in [`ReadMode::Salvage`].
    pub digest_mismatches: Vec<RemTarDigestMismatch>,
    /// Non-fatal conformance issues observed in restore mode.
    pub warnings: Vec<RemTarReadWarning>,
}

/// Parsed encrypted RAO object and authenticated envelope report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedRaoReadObject {
    /// Decrypted and parsed canonical plaintext RAO object.
    pub object: RemTarReadObject,
    /// RAO envelope report from keyed open.
    pub envelope: OpenReport,
}

impl RemTarReadObject {
    /// Return the entry for `path`, if present.
    pub fn entry(&self, path: &str) -> Option<&RemTarReadEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }
}

/// Read and parse one object archive from a block source.
///
/// `block_count` is the object-local block count supplied by the catalog or
/// Layer 3c bootstrap metadata. The source is expected to already be
/// positioned at object `BodyLba(0)`.
///
/// This compatibility reader materializes the full object archive and every
/// file payload in memory. Prefer [`stream_rem_tar_object`] for restore paths
/// that should avoid full-object buffering.
pub fn read_rem_tar_object<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
) -> Result<RemTarReadObject, FormatError> {
    read_rem_tar_object_with_mode(source, chunk_size, block_count, ReadMode::Restore)
}

/// Read and parse one object archive with an explicit reader integrity mode.
pub fn read_rem_tar_object_with_mode<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    mode: ReadMode,
) -> Result<RemTarReadObject, FormatError> {
    read_rem_tar_object_with_mode_and_manifest_anchor(source, chunk_size, block_count, mode, None)
}

/// Read and parse one object archive with an optional external manifest anchor.
pub fn read_rem_tar_object_with_manifest_anchor<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<RemTarReadObject, FormatError> {
    read_rem_tar_object_with_mode_and_manifest_anchor(
        source,
        chunk_size,
        block_count,
        ReadMode::Restore,
        manifest_sha256,
    )
}

/// Read and parse one object archive with explicit reader mode and manifest anchor.
pub fn read_rem_tar_object_with_mode_and_manifest_anchor<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    mode: ReadMode,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<RemTarReadObject, FormatError> {
    validate_chunk_size(chunk_size)?;
    let byte_count = block_count
        .checked_mul(chunk_size as u64)
        .ok_or_else(|| FormatError::parse("object byte count overflow"))?;
    let capacity = usize::try_from(byte_count)
        .map_err(|_| FormatError::parse("object byte count does not fit this host"))?;
    let mut archive = Vec::new();
    archive
        .try_reserve_exact(capacity)
        .map_err(|_| FormatError::parse("object archive too large to materialize"))?;
    let mut block = vec![0u8; chunk_size];
    for _ in 0..block_count {
        let read = source.read_block(&mut block)?;
        if read != chunk_size {
            return Err(FormatError::parse(format!(
                "short object block: expected {chunk_size}, got {read}"
            )));
        }
        archive.extend_from_slice(&block);
    }
    parse_rem_tar_bytes_with_mode_and_manifest_anchor(&archive, chunk_size, mode, manifest_sha256)
}

/// Read, decrypt, and parse one recipient-envelope RAO object.
pub fn read_encrypted_rao_object<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    recipient: &RecipientPrivateKey,
) -> Result<EncryptedRaoReadObject, FormatError> {
    read_encrypted_rao_object_with_mode(
        source,
        chunk_size,
        block_count,
        recipient,
        ReadMode::Restore,
    )
}

/// Read a recipient-envelope RAO object with an explicit integrity mode.
pub fn read_encrypted_rao_object_with_mode<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    recipient: &RecipientPrivateKey,
    mode: ReadMode,
) -> Result<EncryptedRaoReadObject, FormatError> {
    read_encrypted_rao_object_with_mode_and_manifest_anchor(
        source,
        chunk_size,
        block_count,
        recipient,
        mode,
        None,
    )
}

/// Read a recipient-envelope RAO object with an external manifest anchor.
pub fn read_encrypted_rao_object_with_manifest_anchor<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    recipient: &RecipientPrivateKey,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<EncryptedRaoReadObject, FormatError> {
    read_encrypted_rao_object_with_mode_and_manifest_anchor(
        source,
        chunk_size,
        block_count,
        recipient,
        ReadMode::Restore,
        manifest_sha256,
    )
}

/// Read a recipient-envelope RAO object with explicit mode and manifest anchor.
pub fn read_encrypted_rao_object_with_mode_and_manifest_anchor<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    recipient: &RecipientPrivateKey,
    mode: ReadMode,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<EncryptedRaoReadObject, FormatError> {
    validate_chunk_size(chunk_size)?;
    let encrypted = read_object_bytes(source, chunk_size, block_count)?;
    let (plaintext, envelope) = open_to_vec(&encrypted, recipient)?;
    parse_opened_encrypted_object(plaintext, envelope, chunk_size, mode, manifest_sha256)
}

fn parse_opened_encrypted_object(
    plaintext: Vec<u8>,
    envelope: OpenReport,
    chunk_size: usize,
    mode: ReadMode,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<EncryptedRaoReadObject, FormatError> {
    let header_chunk_size = usize::try_from(envelope.header.chunk_size)
        .map_err(|_| FormatError::unsupported_feature("encrypted header chunk_size too large"))?;
    if header_chunk_size != chunk_size {
        return Err(FormatError::ChunkSizeMismatch {
            advertised: header_chunk_size,
            supplied: chunk_size,
        });
    }
    if plaintext.len() % chunk_size != 0 {
        return Err(FormatError::parse(
            "decrypted RAO plaintext is not chunk aligned",
        ));
    }
    let plaintext_block_count = u64::try_from(plaintext.len() / chunk_size)
        .map_err(|_| FormatError::parse("plaintext block count overflow"))?;
    let object = parse_rem_tar_bytes_with_mode_and_manifest_anchor(
        &plaintext,
        chunk_size,
        mode,
        manifest_sha256,
    )
    .map_err(|err| map_inner_plaintext_error(err, &envelope.header.object_id))?;
    let inner_object_id = object
        .global_pax
        .get("REMANENCE.object_id")
        .ok_or_else(|| {
            FormatError::inner_object_mismatch(
                FormatGate::ObjectId,
                "missing inner REMANENCE.object_id",
            )
        })?;
    if inner_object_id != &envelope.header.object_id {
        return Err(FormatError::inner_object_mismatch(
            FormatGate::ObjectId,
            format!(
                "inner object_id {inner_object_id:?} does not match encrypted header {:?}",
                envelope.header.object_id
            ),
        ));
    }
    if plaintext_block_count == 0 {
        return Err(FormatError::parse(
            "decrypted RAO plaintext has zero blocks",
        ));
    }
    Ok(EncryptedRaoReadObject { object, envelope })
}

fn map_inner_plaintext_error(error: FormatError, header_object_id: &str) -> FormatError {
    match error {
        FormatError::ChunkSizeMismatch {
            advertised,
            supplied,
        } => FormatError::inner_object_mismatch(
            FormatGate::ChunkSize,
            format!(
                "inner chunk_size {advertised} does not match encrypted header chunk_size {supplied}"
            ),
        ),
        FormatError::UnsupportedFormatGate { gate, message } => {
            FormatError::inner_object_mismatch(
                gate,
                format!(
                    "inner format gate failed for encrypted header object_id {header_object_id:?}: {message}"
                ),
            )
        }
        other => other,
    }
}

/// Stream one object archive from a block source into an entry sink.
///
/// This is the production-oriented restore path for Layer 3b: it reads fixed
/// object blocks from `source`, parses tar/pax headers in order, and delivers
/// each regular-file payload to `entry_sink` as bytes arrive. It does not
/// materialize the whole object archive in memory.
pub fn stream_rem_tar_object<S, T>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    entry_sink: &mut T,
) -> Result<RemTarStreamReport, FormatError>
where
    S: BlockRead + ?Sized,
    T: RemTarEntrySink + ?Sized,
{
    stream_rem_tar_object_with_mode(
        source,
        chunk_size,
        block_count,
        entry_sink,
        ReadMode::Restore,
    )
}

/// Stream one object archive into an entry sink with an optional external manifest anchor.
pub fn stream_rem_tar_object_with_manifest_anchor<S, T>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    entry_sink: &mut T,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<RemTarStreamReport, FormatError>
where
    S: BlockRead + ?Sized,
    T: RemTarEntrySink + ?Sized,
{
    stream_rem_tar_object_with_mode_and_manifest_anchor(
        source,
        chunk_size,
        block_count,
        entry_sink,
        ReadMode::Restore,
        manifest_sha256,
    )
}

/// Stream one object archive into an entry sink with an explicit reader mode.
pub fn stream_rem_tar_object_with_mode<S, T>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    entry_sink: &mut T,
    mode: ReadMode,
) -> Result<RemTarStreamReport, FormatError>
where
    S: BlockRead + ?Sized,
    T: RemTarEntrySink + ?Sized,
{
    stream_rem_tar_object_with_mode_and_manifest_anchor(
        source,
        chunk_size,
        block_count,
        entry_sink,
        mode,
        None,
    )
}

/// Stream one object archive with explicit reader mode and optional manifest anchor.
pub fn stream_rem_tar_object_with_mode_and_manifest_anchor<S, T>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    entry_sink: &mut T,
    mode: ReadMode,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<RemTarStreamReport, FormatError>
where
    S: BlockRead + ?Sized,
    T: RemTarEntrySink + ?Sized,
{
    validate_chunk_size(chunk_size)?;
    let mut reader = BlockByteReader::new(source, chunk_size, block_count);
    let mut global_pax = BTreeMap::new();
    let mut pending_pax = BTreeMap::new();
    let mut entries = Vec::new();
    let mut digest_mismatches = Vec::new();
    let mut manifest_cbor = None;
    let mut global_contract_validated = false;
    let mut seen_manifest = false;
    let mut seen_regular_paths = BTreeSet::new();

    loop {
        let header = reader.read_record()?;
        if header.iter().all(|&byte| byte == 0) {
            let second = reader.read_record()?;
            if !second.iter().all(|&byte| byte == 0) {
                return Err(FormatError::parse("single zero tar EOF record"));
            }
            break;
        }
        verify_checksum(&header)?;

        let typeflag = header[156];
        let header_size = parse_octal(&header[124..136], "size")?;

        match typeflag {
            TYPE_PAX_GLOBAL | TYPE_PAX_EXTENDED => {
                let body = reader.read_payload_vec(header_size)?;
                let records = parse_pax_records(&body)?;
                if typeflag == TYPE_PAX_GLOBAL {
                    global_pax.extend(records);
                    global_contract_validated = false;
                } else {
                    pending_pax = records;
                }
                reader.skip_padding(header_size)?;
            }
            TYPE_REGULAR | 0 | TYPE_HARDLINK | TYPE_SYMLINK | TYPE_DIRECTORY => {
                if seen_manifest {
                    return Err(FormatError::parse("entry after the manifest"));
                }
                validate_global_contract_once(
                    chunk_size,
                    &global_pax,
                    &mut global_contract_validated,
                )?;
                let entry_type = entry_type_from_typeflag(typeflag)?;
                let size = pending_pax
                    .get("size")
                    .map(|value| parse_decimal_u64(value, "pax size"))
                    .transpose()?
                    .unwrap_or(header_size);
                let path = match pending_pax.get("path") {
                    Some(path) => path.clone(),
                    None => header_path(&header)?,
                };
                if entry_type != RemTarEntryType::Regular && size != 0 {
                    return Err(FormatError::parse(format!(
                        "non-regular entry {path} has nonzero size {size}"
                    )));
                }
                validate_entry_contract(&path, entry_type, &pending_pax)?;
                let link_target = entry_link_target(entry_type, &pending_pax, &header)?;
                validate_hardlink_reference(
                    entry_type,
                    &path,
                    link_target.as_deref(),
                    &seen_regular_paths,
                )?;
                let entry_chunk_count =
                    validate_declared_chunk_count(&path, &pending_pax, size, chunk_size)?;
                let data_offset = reader.offset();
                if size > 0 && data_offset % chunk_size as u64 != 0 {
                    return Err(FormatError::ChunkAlignmentViolation { path, data_offset });
                }
                let entry = RemTarStreamEntry {
                    entry_type,
                    path,
                    size_bytes: size,
                    first_chunk_lba: if size == 0 {
                        None
                    } else {
                        Some(BodyLba(data_offset / chunk_size as u64))
                    },
                    chunk_count: entry_chunk_count,
                    data_offset,
                    pax_records: std::mem::take(&mut pending_pax),
                    link_target,
                    xattrs: Default::default(),
                    extensions: Default::default(),
                };
                let mut manifest_data = (entry.path == MANIFEST_PATH).then(Vec::new);
                let expected_digest =
                    regular_file_sha256(&entry.path, entry.entry_type, &entry.pax_records)?;
                let mut payload_hasher = expected_digest.as_ref().map(|_| Sha256::new());
                entry_sink.begin_file(&entry)?;
                reader.stream_payload(size, |chunk| {
                    if let Some(hasher) = payload_hasher.as_mut() {
                        hasher.update(chunk);
                    }
                    if let Some(data) = manifest_data.as_mut() {
                        data.extend_from_slice(chunk);
                    }
                    entry_sink.write_file_data(chunk)
                })?;
                if let (Some(expected), Some(hasher)) =
                    (expected_digest.as_ref(), payload_hasher.take())
                {
                    let digest = hasher.finalize();
                    let mut actual = [0u8; 32];
                    actual.copy_from_slice(&digest);
                    handle_file_digest_result(
                        mode,
                        &mut digest_mismatches,
                        &entry.path,
                        expected,
                        &actual,
                    )?;
                }
                entry_sink.end_file(&entry)?;
                reader.skip_padding(size)?;
                if let Some(data) = manifest_data {
                    manifest_cbor = Some(data);
                    seen_manifest = true;
                }
                if entry.entry_type == RemTarEntryType::Regular && entry.path != MANIFEST_PATH {
                    seen_regular_paths.insert(entry.path.clone());
                }
                entries.push(entry);
            }
            other => {
                return Err(FormatError::UnsupportedTarTypeflag { typeflag: other });
            }
        }
    }
    validate_global_contract_once(chunk_size, &global_pax, &mut global_contract_validated)?;

    validate_stream_manifest(
        &manifest_cbor,
        &entries,
        &global_pax,
        chunk_size,
        manifest_sha256,
    )?;
    let object_extensions = manifest_cbor
        .as_deref()
        .map(|manifest| hydrate_stream_entry_metadata(&mut entries, manifest))
        .transpose()?
        .unwrap_or_default();
    let warnings = reader_warnings(mode, manifest_cbor.is_some());

    Ok(RemTarStreamReport {
        global_pax,
        entries,
        manifest_cbor,
        object_extensions,
        digest_mismatches,
        warnings,
    })
}

fn parse_rem_tar_bytes_with_mode_and_manifest_anchor(
    bytes: &[u8],
    chunk_size: usize,
    mode: ReadMode,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<RemTarReadObject, FormatError> {
    validate_chunk_size(chunk_size)?;
    let mut offset = 0usize;
    let mut global_pax = BTreeMap::new();
    let mut pending_pax = BTreeMap::new();
    let mut entries = Vec::new();
    let mut digest_mismatches = Vec::new();
    let mut global_contract_validated = false;
    let mut seen_manifest = false;
    let mut seen_regular_paths = BTreeSet::new();

    loop {
        let header = read_record(bytes, offset)?;
        if header.iter().all(|&byte| byte == 0) {
            let next = offset
                .checked_add(TAR_RECORD_SIZE)
                .ok_or_else(|| FormatError::parse("archive offset overflow"))?;
            let second = read_record(bytes, next)?;
            if !second.iter().all(|&byte| byte == 0) {
                return Err(FormatError::parse("single zero tar EOF record"));
            }
            break;
        }
        verify_checksum(header)?;

        let typeflag = header[156];
        let header_name = header_path(header)?;
        let header_size = parse_octal(&header[124..136], "size")?;
        offset = offset
            .checked_add(TAR_RECORD_SIZE)
            .ok_or_else(|| FormatError::parse("archive offset overflow"))?;

        match typeflag {
            TYPE_PAX_GLOBAL | TYPE_PAX_EXTENDED => {
                let body = read_payload(bytes, offset, header_size)?;
                let records = parse_pax_records(body)?;
                if typeflag == TYPE_PAX_GLOBAL {
                    global_pax.extend(records);
                    global_contract_validated = false;
                } else {
                    pending_pax = records;
                }
                offset = skip_payload(offset, header_size)?;
            }
            TYPE_REGULAR | 0 | TYPE_HARDLINK | TYPE_SYMLINK | TYPE_DIRECTORY => {
                if seen_manifest {
                    return Err(FormatError::parse("entry after the manifest"));
                }
                validate_global_contract_once(
                    chunk_size,
                    &global_pax,
                    &mut global_contract_validated,
                )?;
                let entry_type = entry_type_from_typeflag(typeflag)?;
                let size = pending_pax
                    .get("size")
                    .map(|value| parse_decimal_u64(value, "pax size"))
                    .transpose()?
                    .unwrap_or(header_size);
                if entry_type != RemTarEntryType::Regular && size != 0 {
                    return Err(FormatError::parse(format!(
                        "non-regular entry {header_name} has nonzero size {size}"
                    )));
                }
                let path = pending_pax.get("path").cloned().unwrap_or(header_name);
                validate_entry_contract(&path, entry_type, &pending_pax)?;
                let link_target = entry_link_target(entry_type, &pending_pax, header)?;
                validate_hardlink_reference(
                    entry_type,
                    &path,
                    link_target.as_deref(),
                    &seen_regular_paths,
                )?;
                let data_offset = offset as u64;
                if size > 0 && data_offset % chunk_size as u64 != 0 {
                    return Err(FormatError::ChunkAlignmentViolation { path, data_offset });
                }
                let expected_digest = regular_file_sha256(&path, entry_type, &pending_pax)?;
                let data = read_payload(bytes, offset, size)?.to_vec();
                let entry_chunk_count =
                    validate_declared_chunk_count(&path, &pending_pax, size, chunk_size)?;
                if let Some(expected) = expected_digest.as_ref() {
                    let digest = Sha256::digest(&data);
                    let mut actual = [0u8; 32];
                    actual.copy_from_slice(&digest);
                    handle_file_digest_result(
                        mode,
                        &mut digest_mismatches,
                        &path,
                        expected,
                        &actual,
                    )?;
                }
                let is_manifest = path == MANIFEST_PATH;
                let entry = RemTarReadEntry {
                    entry_type,
                    path,
                    size_bytes: size,
                    first_chunk_lba: if size == 0 {
                        None
                    } else {
                        Some(BodyLba(data_offset / chunk_size as u64))
                    },
                    chunk_count: entry_chunk_count,
                    data_offset,
                    pax_records: std::mem::take(&mut pending_pax),
                    link_target,
                    xattrs: Default::default(),
                    extensions: Default::default(),
                    data,
                };
                if entry.entry_type == RemTarEntryType::Regular && entry.path != MANIFEST_PATH {
                    seen_regular_paths.insert(entry.path.clone());
                }
                entries.push(entry);
                if is_manifest {
                    seen_manifest = true;
                }
                offset = skip_payload(offset, size)?;
            }
            other => {
                return Err(FormatError::UnsupportedTarTypeflag { typeflag: other });
            }
        }
    }
    validate_global_contract_once(chunk_size, &global_pax, &mut global_contract_validated)?;

    let manifest_entry = entries.iter().find(|entry| entry.path == MANIFEST_PATH);
    let manifest_cbor = manifest_entry.map(|entry| entry.data.clone());
    if let Some(entry) = manifest_entry {
        validate_manifest_entry(
            &entry.data,
            &entry.pax_records,
            &global_pax,
            chunk_size,
            manifest_sha256,
        )?;
    }
    let object_extensions = manifest_cbor
        .as_deref()
        .map(|manifest| hydrate_read_entry_metadata(&mut entries, manifest))
        .transpose()?
        .unwrap_or_default();
    let warnings = reader_warnings(mode, manifest_cbor.is_some());
    Ok(RemTarReadObject {
        global_pax,
        entries,
        manifest_cbor,
        object_extensions,
        digest_mismatches,
        warnings,
    })
}

fn reader_warnings(mode: ReadMode, has_manifest: bool) -> Vec<RemTarReadWarning> {
    if mode == ReadMode::Restore && !has_manifest {
        vec![RemTarReadWarning::MissingManifest]
    } else {
        Vec::new()
    }
}

fn read_object_bytes<S: BlockRead + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
) -> Result<Vec<u8>, FormatError> {
    let byte_count = block_count
        .checked_mul(chunk_size as u64)
        .ok_or_else(|| FormatError::parse("object byte count overflow"))?;
    let capacity = usize::try_from(byte_count)
        .map_err(|_| FormatError::parse("object byte count does not fit this host"))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| FormatError::parse("object archive too large to materialize"))?;
    let mut block = vec![0u8; chunk_size];
    for _ in 0..block_count {
        let read = source.read_block(&mut block)?;
        if read != chunk_size {
            return Err(FormatError::parse(format!(
                "short object block: expected {chunk_size}, got {read}"
            )));
        }
        bytes.extend_from_slice(&block);
    }
    Ok(bytes)
}

struct BlockByteReader<'a, S: BlockRead + ?Sized> {
    source: &'a mut S,
    chunk_size: usize,
    blocks_remaining: u64,
    block: Vec<u8>,
    position: usize,
    offset: u64,
}

impl<'a, S: BlockRead + ?Sized> BlockByteReader<'a, S> {
    fn new(source: &'a mut S, chunk_size: usize, block_count: u64) -> Self {
        Self {
            source,
            chunk_size,
            blocks_remaining: block_count,
            block: Vec::new(),
            position: 0,
            offset: 0,
        }
    }

    fn offset(&self) -> u64 {
        self.offset
    }

    fn read_record(&mut self) -> Result<[u8; TAR_RECORD_SIZE], FormatError> {
        let mut record = [0u8; TAR_RECORD_SIZE];
        self.read_exact_into(&mut record)?;
        Ok(record)
    }

    fn read_payload_vec(&mut self, size: u64) -> Result<Vec<u8>, FormatError> {
        if size > self.bytes_remaining()? {
            return Err(FormatError::TruncatedPayload);
        }
        let len = usize::try_from(size).map_err(|_| FormatError::parse("payload too large"))?;
        let mut out = Vec::new();
        out.try_reserve_exact(len)
            .map_err(|_| FormatError::parse("payload too large"))?;
        out.resize(len, 0);
        self.read_exact_into(&mut out)?;
        Ok(out)
    }

    fn stream_payload<F>(&mut self, size: u64, mut on_chunk: F) -> Result<(), FormatError>
    where
        F: FnMut(&[u8]) -> Result<(), FormatError>,
    {
        let mut remaining = size;
        while remaining > 0 {
            self.ensure_available()?;
            let available = self.block.len() - self.position;
            let take = available.min(usize::try_from(remaining).unwrap_or(usize::MAX));
            on_chunk(&self.block[self.position..self.position + take])?;
            self.position += take;
            self.offset = self
                .offset
                .checked_add(take as u64)
                .ok_or_else(|| FormatError::parse("stream offset overflow"))?;
            remaining -= take as u64;
        }
        Ok(())
    }

    fn skip_padding(&mut self, size: u64) -> Result<(), FormatError> {
        let padding = tar_padding_len(size);
        let mut scratch = vec![0u8; padding];
        self.read_exact_into(&mut scratch)
    }

    fn read_exact_into(&mut self, mut out: &mut [u8]) -> Result<(), FormatError> {
        while !out.is_empty() {
            self.ensure_available()?;
            let available = self.block.len() - self.position;
            let take = available.min(out.len());
            let (head, tail) = out.split_at_mut(take);
            head.copy_from_slice(&self.block[self.position..self.position + take]);
            self.position += take;
            self.offset = self
                .offset
                .checked_add(take as u64)
                .ok_or_else(|| FormatError::parse("stream offset overflow"))?;
            out = tail;
        }
        Ok(())
    }

    fn ensure_available(&mut self) -> Result<(), FormatError> {
        if self.position < self.block.len() {
            return Ok(());
        }
        if self.blocks_remaining == 0 {
            return Err(FormatError::TruncatedPayload);
        }
        if self.block.len() != self.chunk_size {
            self.block.resize(self.chunk_size, 0);
        }
        let read = self.source.read_block(&mut self.block)?;
        if read != self.chunk_size {
            return Err(FormatError::parse(format!(
                "short object block: expected {}, got {read}",
                self.chunk_size
            )));
        }
        self.blocks_remaining -= 1;
        self.position = 0;
        Ok(())
    }

    fn bytes_remaining(&self) -> Result<u64, FormatError> {
        let buffered = self.block.len().saturating_sub(self.position) as u64;
        let unread = self
            .blocks_remaining
            .checked_mul(self.chunk_size as u64)
            .ok_or_else(|| FormatError::parse("stream remaining byte count overflow"))?;
        buffered
            .checked_add(unread)
            .ok_or_else(|| FormatError::parse("stream remaining byte count overflow"))
    }
}

fn read_record(bytes: &[u8], offset: usize) -> Result<&[u8], FormatError> {
    read_payload(bytes, offset, TAR_RECORD_SIZE as u64)
}

fn read_payload(bytes: &[u8], offset: usize, size: u64) -> Result<&[u8], FormatError> {
    let size = usize::try_from(size).map_err(|_| FormatError::parse("payload too large"))?;
    let end = offset
        .checked_add(size)
        .ok_or_else(|| FormatError::parse("payload offset overflow"))?;
    bytes.get(offset..end).ok_or(FormatError::TruncatedPayload)
}

fn skip_payload(offset: usize, size: u64) -> Result<usize, FormatError> {
    let padding = tar_padding_len(size);
    let size = usize::try_from(size).map_err(|_| FormatError::parse("payload too large"))?;
    offset
        .checked_add(size)
        .and_then(|next| next.checked_add(padding))
        .ok_or_else(|| FormatError::parse("payload skip overflow"))
}

fn parse_pax_records(mut body: &[u8]) -> Result<BTreeMap<String, String>, FormatError> {
    let mut records = BTreeMap::new();
    while !body.is_empty() {
        let space = body.iter().position(|&byte| byte == b' ').ok_or_else(|| {
            FormatError::PaxRecordMalformed("missing length separator".to_string())
        })?;
        let length_text = std::str::from_utf8(&body[..space])
            .map_err(|_| FormatError::PaxRecordMalformed("length is not UTF-8".to_string()))?;
        let length = parse_decimal_usize(length_text, "pax length")?;
        if length == 0 || length > body.len() {
            return Err(FormatError::PaxRecordMalformed(
                "length out of bounds".to_string(),
            ));
        }
        let record = &body[..length];
        if record.last() != Some(&b'\n') {
            return Err(FormatError::PaxRecordMalformed(
                "missing newline".to_string(),
            ));
        }
        let equals = record[space + 1..length - 1]
            .iter()
            .position(|&byte| byte == b'=')
            .map(|pos| space + 1 + pos)
            .ok_or_else(|| FormatError::PaxRecordMalformed("missing equals sign".to_string()))?;
        let key = std::str::from_utf8(&record[space + 1..equals])
            .map_err(|_| FormatError::PaxRecordMalformed("keyword is not UTF-8".to_string()))?;
        validate_pax_keyword(key)?;
        let value_bytes = &record[equals + 1..length - 1];
        if value_bytes.iter().any(|&byte| byte < 0x20) {
            return Err(FormatError::PaxRecordMalformed(
                "value contains control character".to_string(),
            ));
        }
        let value = std::str::from_utf8(value_bytes)
            .map_err(|_| FormatError::PaxRecordMalformed("value is not UTF-8".to_string()))?;
        records.insert(key.to_string(), value.to_string());
        body = &body[length..];
    }
    Ok(records)
}

fn header_path(header: &[u8]) -> Result<String, FormatError> {
    let name = nul_terminated(&header[0..100])?;
    let prefix = nul_terminated(&header[345..500])?;
    if prefix.is_empty() {
        Ok(name)
    } else {
        Ok(format!("{prefix}/{name}"))
    }
}

fn header_linkname(header: &[u8]) -> Result<String, FormatError> {
    nul_terminated(&header[157..257])
}

fn entry_type_from_typeflag(typeflag: u8) -> Result<RemTarEntryType, FormatError> {
    match typeflag {
        TYPE_REGULAR | 0 => Ok(RemTarEntryType::Regular),
        TYPE_HARDLINK => Ok(RemTarEntryType::Hardlink),
        TYPE_SYMLINK => Ok(RemTarEntryType::Symlink),
        TYPE_DIRECTORY => Ok(RemTarEntryType::Directory),
        other => Err(FormatError::UnsupportedTarTypeflag { typeflag: other }),
    }
}

fn entry_link_target(
    entry_type: RemTarEntryType,
    pax: &BTreeMap<String, String>,
    header: &[u8],
) -> Result<Option<String>, FormatError> {
    if !matches!(
        entry_type,
        RemTarEntryType::Hardlink | RemTarEntryType::Symlink
    ) {
        return Ok(None);
    }
    let target = pax
        .get("linkpath")
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| header_linkname(header))?;
    if target.is_empty() {
        return Err(FormatError::parse("link entry missing link target"));
    }
    Ok(Some(target))
}

fn validate_hardlink_reference(
    entry_type: RemTarEntryType,
    path: &str,
    target: Option<&str>,
    seen_regular_paths: &BTreeSet<String>,
) -> Result<(), FormatError> {
    if entry_type != RemTarEntryType::Hardlink {
        return Ok(());
    }
    let target = target.ok_or_else(|| FormatError::parse("hardlink entry missing link target"))?;
    validate_canonical_entry_path(target, RemTarEntryType::Regular)?;
    if !seen_regular_paths.contains(target) {
        return Err(FormatError::InvalidHardlinkTarget {
            path: path.to_string(),
            target: target.to_string(),
        });
    }
    Ok(())
}

fn verify_checksum(header: &[u8]) -> Result<(), FormatError> {
    let stored = parse_octal(&header[148..156], "checksum")?;
    let computed: u64 = header
        .iter()
        .enumerate()
        .map(|(idx, &byte)| {
            if (148..156).contains(&idx) {
                b' ' as u64
            } else {
                byte as u64
            }
        })
        .sum();
    if stored != computed {
        return Err(FormatError::UstarChecksumMismatch { stored, computed });
    }
    Ok(())
}

fn nul_terminated(field: &[u8]) -> Result<String, FormatError> {
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    std::str::from_utf8(&field[..end])
        .map(str::to_string)
        .map_err(|_| FormatError::parse("ustar header field is not UTF-8"))
}

fn parse_octal(field: &[u8], name: &str) -> Result<u64, FormatError> {
    let start = field
        .iter()
        .position(|&byte| byte != 0 && byte != b' ')
        .unwrap_or(field.len());
    let rest = &field[start..];
    let end = rest
        .iter()
        .position(|&byte| byte == 0 || byte == b' ')
        .unwrap_or(rest.len());
    let text = std::str::from_utf8(&rest[..end])
        .map_err(|_| FormatError::parse(format!("{name} field is not UTF-8")))?;
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8)
        .map_err(|_| FormatError::parse(format!("{name} field is not octal")))
}

fn validate_pax_keyword(key: &str) -> Result<(), FormatError> {
    if key.is_empty() {
        return Err(FormatError::PaxRecordMalformed(
            "keyword is empty".to_string(),
        ));
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii() && !byte.is_ascii_control() && byte != b'=')
    {
        return Err(FormatError::PaxRecordMalformed(
            "keyword contains a disallowed byte".to_string(),
        ));
    }
    Ok(())
}

fn parse_decimal_u64(value: &str, name: &str) -> Result<u64, FormatError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(FormatError::parse(format!(
            "{name} is not a plain decimal integer"
        )));
    }
    value
        .parse::<u64>()
        .map_err(|_| FormatError::parse(format!("{name} is not a decimal integer")))
}

fn parse_decimal_usize(value: &str, name: &str) -> Result<usize, FormatError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(FormatError::parse(format!(
            "{name} is not a plain decimal integer"
        )));
    }
    value
        .parse::<usize>()
        .map_err(|_| FormatError::parse(format!("{name} is not a decimal integer")))
}

fn validate_global_contract_once(
    chunk_size: usize,
    global_pax: &BTreeMap<String, String>,
    validated: &mut bool,
) -> Result<(), FormatError> {
    if *validated {
        return Ok(());
    }
    let format_id = required_pax(global_pax, "REMANENCE.format_id")?;
    if format_id != FORMAT_ID {
        return Err(FormatError::unsupported_format_gate(
            FormatGate::FormatId,
            format!("format_id {format_id:?} is not {FORMAT_ID:?}"),
        ));
    }
    let schema_version = required_pax(global_pax, "REMANENCE.schema_version")?;
    let expected_major = schema_major(SCHEMA_VERSION)?;
    let actual_major = schema_major(schema_version)?;
    if actual_major != expected_major {
        return Err(FormatError::unsupported_format_gate(
            FormatGate::SchemaVersion,
            format!(
                "schema_version {schema_version:?} has major version {actual_major}, expected {expected_major}"
            ),
        ));
    }
    if let Some(encryption) = global_pax.get("REMANENCE.encryption") {
        if encryption != "none" {
            return Err(FormatError::unsupported_format_gate(
                FormatGate::Encryption,
                format!("inner RAO stream encryption must be \"none\", got {encryption:?}"),
            ));
        }
    }
    if let Some(advertised_chunk_size) = global_pax.get("REMANENCE.chunk_size") {
        let advertised_chunk_size =
            parse_decimal_usize(advertised_chunk_size, "REMANENCE.chunk_size")?;
        if advertised_chunk_size != chunk_size {
            return Err(FormatError::ChunkSizeMismatch {
                advertised: advertised_chunk_size,
                supplied: chunk_size,
            });
        }
    }
    *validated = true;
    Ok(())
}

fn validate_entry_contract(
    path: &str,
    entry_type: RemTarEntryType,
    pax: &BTreeMap<String, String>,
) -> Result<(), FormatError> {
    validate_canonical_entry_path(path, entry_type)?;
    let compression = required_pax(pax, "REMANENCE.compression")?;
    if compression != "none" {
        return Err(FormatError::unsupported_feature(format!(
            "file {path} uses compression {compression:?}"
        )));
    }
    Ok(())
}

fn regular_file_sha256(
    path: &str,
    entry_type: RemTarEntryType,
    pax: &BTreeMap<String, String>,
) -> Result<Option<[u8; 32]>, FormatError> {
    if entry_type != RemTarEntryType::Regular {
        return Ok(None);
    }
    let expected = required_pax(pax, "REMANENCE.file_sha256").and_then(parse_sha256_hex)?;
    if path.is_empty() {
        return Err(FormatError::invalid_path("entry path must not be empty"));
    }
    Ok(Some(expected))
}

fn handle_file_digest_result(
    mode: ReadMode,
    digest_mismatches: &mut Vec<RemTarDigestMismatch>,
    path: &str,
    expected: &[u8; 32],
    actual: &[u8; 32],
) -> Result<(), FormatError> {
    if actual == expected {
        return Ok(());
    }
    let mismatch = RemTarDigestMismatch {
        path: path.to_string(),
        expected: hex_lower(expected),
        actual: hex_lower(actual),
    };
    match mode {
        ReadMode::Restore => Err(mismatch.into_error()),
        ReadMode::Salvage => {
            digest_mismatches.push(mismatch);
            Ok(())
        }
    }
}

impl RemTarDigestMismatch {
    fn into_error(self) -> FormatError {
        FormatError::FileDigestMismatch {
            path: self.path,
            expected: self.expected,
            actual: self.actual,
        }
    }
}

fn validate_canonical_entry_path(
    path: &str,
    entry_type: RemTarEntryType,
) -> Result<(), FormatError> {
    if path.is_empty() {
        return Err(FormatError::invalid_path("entry path must not be empty"));
    }
    if path.as_bytes().contains(&0) {
        return Err(FormatError::invalid_path("entry path must not contain NUL"));
    }
    if path.bytes().any(|byte| byte < 0x20) {
        return Err(FormatError::invalid_path(
            "entry path must not contain ASCII control characters",
        ));
    }
    if path.starts_with('/') {
        return Err(FormatError::invalid_path("entry path must be relative"));
    }
    let component_path = if entry_type == RemTarEntryType::Directory {
        path.strip_suffix('/')
            .ok_or_else(|| FormatError::invalid_path("directory entry path must end with '/'"))?
    } else {
        if path.ends_with('/') {
            return Err(FormatError::invalid_path(
                "non-directory entry path must not end with '/'",
            ));
        }
        path
    };
    if component_path.is_empty() {
        return Err(FormatError::invalid_path("entry path must not be empty"));
    }
    for component in component_path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(FormatError::invalid_path(format!(
                "entry path contains non-canonical component {component:?}"
            )));
        }
    }
    Ok(())
}

fn required_pax<'a>(pax: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, FormatError> {
    pax.get(key)
        .map(String::as_str)
        .ok_or_else(|| FormatError::parse(format!("missing required pax key {key}")))
}

fn validate_stream_manifest(
    manifest_cbor: &Option<Vec<u8>>,
    entries: &[RemTarStreamEntry],
    global_pax: &BTreeMap<String, String>,
    chunk_size: usize,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<(), FormatError> {
    if let Some(bytes) = manifest_cbor {
        let entry = entries
            .iter()
            .find(|entry| entry.path == MANIFEST_PATH)
            .ok_or_else(|| FormatError::parse("manifest data captured without manifest entry"))?;
        validate_manifest_entry(
            bytes,
            &entry.pax_records,
            global_pax,
            chunk_size,
            manifest_sha256,
        )?;
    }
    Ok(())
}

fn hydrate_stream_entry_metadata(
    entries: &mut [RemTarStreamEntry],
    manifest: &[u8],
) -> Result<RemTarExtensions, FormatError> {
    let mut metadata = manifest_preservation_metadata(manifest)?;
    for entry in entries {
        if let Some(preservation) = metadata.entries.remove(&entry.path) {
            entry.xattrs = preservation.xattrs;
            entry.extensions = preservation.extensions;
        }
    }
    Ok(metadata.object_extensions)
}

fn hydrate_read_entry_metadata(
    entries: &mut [RemTarReadEntry],
    manifest: &[u8],
) -> Result<RemTarExtensions, FormatError> {
    let mut metadata = manifest_preservation_metadata(manifest)?;
    for entry in entries {
        if let Some(preservation) = metadata.entries.remove(&entry.path) {
            entry.xattrs = preservation.xattrs;
            entry.extensions = preservation.extensions;
        }
    }
    Ok(metadata.object_extensions)
}

fn validate_manifest_entry(
    bytes: &[u8],
    pax: &BTreeMap<String, String>,
    global_pax: &BTreeMap<String, String>,
    chunk_size: usize,
    manifest_sha256: Option<[u8; 32]>,
) -> Result<(), FormatError> {
    let anchor = required_pax(pax, "REMANENCE.file_sha256").and_then(parse_sha256_hex)?;
    if let Some(external_anchor) = manifest_sha256 {
        if external_anchor != anchor {
            return Err(FormatError::ManifestDigestMismatch);
        }
    }
    validate_manifest(bytes, &anchor, global_pax, chunk_size)
}

fn parse_sha256_hex(value: &str) -> Result<[u8; 32], FormatError> {
    if value.len() != 64 {
        return Err(FormatError::parse(
            "REMANENCE.file_sha256 must be 64 lowercase hex digits",
        ));
    }
    let mut out = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let hi = hex_digit(chunk[0])?;
        let lo = hex_digit(chunk[1])?;
        out[index] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_digit(byte: u8) -> Result<u8, FormatError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(FormatError::parse(
            "REMANENCE.file_sha256 must be lowercase hex",
        )),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn schema_major(value: &str) -> Result<u64, FormatError> {
    let major = value.split_once('.').map_or(value, |(major, _)| major);
    parse_decimal_u64(major, "schema_version major")
}

fn chunk_count(size_bytes: u64, chunk_size: usize) -> Result<u64, FormatError> {
    if size_bytes == 0 {
        return Ok(0);
    }
    let chunk = chunk_size as u64;
    Ok((size_bytes - 1) / chunk + 1)
}

fn validate_declared_chunk_count(
    path: &str,
    pax: &BTreeMap<String, String>,
    size_bytes: u64,
    chunk_size: usize,
) -> Result<u64, FormatError> {
    let computed = chunk_count(size_bytes, chunk_size)?;
    if let Some(declared) = pax.get("REMANENCE.chunk_count") {
        let declared = parse_decimal_u64(declared, "REMANENCE.chunk_count")?;
        if declared != computed {
            return Err(FormatError::parse(format!(
                "file {path} declares chunk_count {declared}, computed {computed}"
            )));
        }
    }
    Ok(computed)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use remanence_aead::{
        inspect_bytes, seal_to_vec, EnvelopeSealOptions, RecipientPrivateKey, RecipientPublicKey,
        SealOptions,
    };
    use remanence_library::{FileBlockSink, FileBlockSource, VecBlockSink, VecBlockSource};

    use super::*;
    use crate::tar::encode_header;
    use crate::{
        write_encrypted_rao_object, write_rem_tar_object, RemTarFile, RemTarObjectOptions,
        FORMAT_ID,
    };

    fn options(chunk_size: usize) -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            "55555555-5555-5555-5555-555555555555",
            "caller-reader",
            "2026-05-27T22:10:00+05:30",
            "66666666-6666-6666-6666-666666666666",
        );
        opts.chunk_size = chunk_size;
        opts
    }

    fn recipient_pair() -> (RecipientPrivateKey, Vec<RecipientPublicKey>) {
        let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
        let recipients = vec![
            primary.public_key(0).unwrap(),
            recovery.public_key(1).unwrap(),
        ];
        (primary, recipients)
    }

    fn pax_record(key: &str, value: &str) -> Vec<u8> {
        let mut length = key.len() + value.len() + 4;
        loop {
            let record = format!("{length} {key}={value}\n");
            if record.len() == length {
                return record.into_bytes();
            }
            length = record.len();
        }
    }

    fn replace_once_in_blocks(blocks: &mut [Vec<u8>], needle: &[u8], replacement: &[u8]) {
        assert_eq!(needle.len(), replacement.len());
        for block in blocks {
            if let Some(offset) = block
                .windows(needle.len())
                .position(|window| window == needle)
            {
                block[offset..offset + needle.len()].copy_from_slice(replacement);
                return;
            }
        }
        panic!("needle {:?} not found in archive blocks", needle);
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn temp_object_path(test_name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "remanence-format-{test_name}-{}-{nanos}.rao",
            std::process::id()
        ))
    }

    #[test]
    fn global_contract_accepts_optional_encryption_and_chunk_size() {
        let mut pax = BTreeMap::from([
            ("REMANENCE.format_id".to_string(), FORMAT_ID.to_string()),
            (
                "REMANENCE.schema_version".to_string(),
                SCHEMA_VERSION.to_string(),
            ),
        ]);
        let mut validated = false;
        validate_global_contract_once(4096, &pax, &mut validated).unwrap();
        assert!(validated);

        pax.insert("REMANENCE.encryption".to_string(), "none".to_string());
        pax.insert("REMANENCE.chunk_size".to_string(), "4096".to_string());
        let mut validated = false;
        validate_global_contract_once(4096, &pax, &mut validated).unwrap();
        assert!(validated);
    }

    #[test]
    fn parse_octal_accepts_leading_space_padding() {
        assert_eq!(parse_octal(b"   644 \0\0", "mode").unwrap(), 0o644);
        assert_eq!(parse_octal(b"\0\0  17 ", "size").unwrap(), 0o17);
    }

    #[test]
    fn parse_pax_records_rejects_invalid_keywords() {
        for key in ["", "bad\nkey", "bad\tkey", "vid\u{e9}o"] {
            let record = pax_record(key, "value");
            let err = parse_pax_records(&record).expect_err("invalid key should fail");
            assert!(matches!(err, FormatError::PaxRecordMalformed(_)), "{err}");
        }
        let records = parse_pax_records(&pax_record("REMANENCE.file_sha256", "abc")).unwrap();
        assert_eq!(records.get("REMANENCE.file_sha256").unwrap(), "abc");
    }

    #[test]
    fn reads_writer_output_through_block_source() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "vidéo/clip.txt",
                file_id: "file-b",
                data: b"utf8 path",
                mtime: None,
                executable: Some(true),
            },
        ];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let mut source = VecBlockSource::new(sink.blocks);
        let read = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .unwrap();

        assert_eq!(
            read.global_pax.get("REMANENCE.format_id").unwrap(),
            FORMAT_ID
        );
        assert_eq!(read.entry("a.txt").unwrap().data, b"hello");
        assert_eq!(read.entry("vidéo/clip.txt").unwrap().data, b"utf8 path");
        assert_eq!(
            read.entry("vidéo/clip.txt").unwrap().first_chunk_lba,
            layout.files[1].first_chunk_lba
        );
        assert!(read.entry(MANIFEST_PATH).is_some());
        assert_eq!(read.manifest_cbor.as_ref().unwrap(), &layout.manifest_cbor);
    }

    #[test]
    fn reader_rejects_manifest_digest_mismatch() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let offset = layout.manifest.data_offset as usize;
        sink.blocks[offset / opts.chunk_size][offset % opts.chunk_size] ^= 1;
        let mut source = VecBlockSource::new(sink.blocks);

        let err = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::FileDigestMismatch { ref path, .. } if path == MANIFEST_PATH
            ),
            "{err}"
        );
    }

    #[test]
    fn restore_readers_report_missing_manifest() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let mut bytes = sink.blocks.into_iter().flatten().collect::<Vec<_>>();
        bytes[layout.manifest.pax_header_offset as usize..].fill(0);
        let blocks = bytes
            .chunks_exact(opts.chunk_size)
            .map(Vec::from)
            .collect::<Vec<_>>();

        let mut source = VecBlockSource::new(blocks.clone());
        let read = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .expect("restore-mode materializing reader reports rather than hides absence");
        assert_eq!(read.manifest_cbor, None);
        assert_eq!(read.warnings, vec![RemTarReadWarning::MissingManifest]);

        let mut source = VecBlockSource::new(blocks);
        let mut entries = CollectingEntrySink::default();
        let report = stream_rem_tar_object(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            &mut entries,
        )
        .expect("restore-mode streaming reader reports rather than hides absence");
        assert_eq!(report.manifest_cbor, None);
        assert_eq!(report.warnings, vec![RemTarReadWarning::MissingManifest]);
    }

    #[test]
    fn reader_rejects_external_manifest_anchor_mismatch() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let mut wrong_anchor = layout.manifest_sha256;
        wrong_anchor[0] ^= 1;

        let mut source = VecBlockSource::new(sink.blocks.clone());
        let err = read_rem_tar_object_with_manifest_anchor(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            Some(wrong_anchor),
        )
        .unwrap_err();
        assert!(matches!(err, FormatError::ManifestDigestMismatch), "{err}");

        let mut source = VecBlockSource::new(sink.blocks);
        let mut entries = CollectingEntrySink::default();
        let err = stream_rem_tar_object_with_manifest_anchor(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            &mut entries,
            Some(wrong_anchor),
        )
        .unwrap_err();
        assert!(matches!(err, FormatError::ManifestDigestMismatch), "{err}");
    }

    #[test]
    fn reader_rejects_payload_digest_mismatch() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let offset = layout.files[0].data_offset as usize;
        sink.blocks[offset / opts.chunk_size][offset % opts.chunk_size] ^= 1;
        let mut source = VecBlockSource::new(sink.blocks);

        let err = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::FileDigestMismatch { ref path, .. } if path == "a.txt"
            ),
            "{err}"
        );
    }

    #[test]
    fn reader_rejects_declared_chunk_count_mismatch() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        replace_once_in_blocks(
            &mut sink.blocks,
            b"REMANENCE.chunk_count=1\n",
            b"REMANENCE.chunk_count=2\n",
        );
        let mut source = VecBlockSource::new(sink.blocks);

        let err = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .unwrap_err();

        assert!(err.to_string().contains("chunk_count"), "{err}");
    }

    #[test]
    fn streaming_reader_rejects_payload_digest_mismatch_before_end_file() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let offset = layout.files[0].data_offset as usize;
        sink.blocks[offset / opts.chunk_size][offset % opts.chunk_size] ^= 1;
        let mut source = VecBlockSource::new(sink.blocks);
        let mut entries = CollectingEntrySink::default();

        let err = stream_rem_tar_object(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            &mut entries,
        )
        .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::FileDigestMismatch { ref path, .. } if path == "a.txt"
            ),
            "{err}"
        );
        assert_eq!(entries.active.as_deref(), Some("a.txt"));
    }

    #[test]
    fn salvage_mode_reports_payload_digest_mismatch_and_continues() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let offset = layout.files[0].data_offset as usize;
        sink.blocks[offset / opts.chunk_size][offset % opts.chunk_size] ^= 1;
        let mut source = VecBlockSource::new(sink.blocks);
        let mut entries = CollectingEntrySink::default();

        let report = stream_rem_tar_object_with_mode(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            &mut entries,
            ReadMode::Salvage,
        )
        .unwrap();

        assert_eq!(report.digest_mismatches.len(), 1);
        assert_eq!(report.digest_mismatches[0].path, "a.txt");
        assert!(entries.active.is_none());
        assert_eq!(entries.data.get("a.txt").unwrap()[0], b'h' ^ 1);
    }

    #[test]
    fn reader_rejects_noncanonical_entry_paths() {
        let mut pax = BTreeMap::new();
        pax.insert("REMANENCE.compression".to_string(), "none".to_string());

        for path in [
            "/abs.bin",
            "a//b.bin",
            "a/./b.bin",
            "a/../b.bin",
            "trailing/",
        ] {
            let err = validate_entry_contract(path, RemTarEntryType::Regular, &pax)
                .expect_err("noncanonical path should fail");
            assert!(err.to_string().contains("path"), "{path}: {err}");
        }

        let err = validate_entry_contract("bad/../empty/", RemTarEntryType::Directory, &pax)
            .expect_err("directory traversal component should fail");
        assert!(err.to_string().contains("path"), "{err}");
    }

    #[test]
    fn plaintext_rao_round_trips_through_file_block_adapters() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "dir/b.bin",
                file_id: "file-b",
                data: &[0xA5u8; 7000],
                mtime: None,
                executable: Some(true),
            },
        ];
        let path = temp_object_path("plain");
        let layout = {
            let mut sink = FileBlockSink::create_truncate(&path, opts.chunk_size).unwrap();
            let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
            sink.flush().unwrap();
            layout
        };

        let mut source = FileBlockSource::open(&path, opts.chunk_size).unwrap();
        assert_eq!(source.block_count(), layout.projected_size_blocks);
        let block_count = source.block_count();
        let read = read_rem_tar_object(&mut source, opts.chunk_size, block_count).unwrap();
        assert_eq!(read.entry("a.txt").unwrap().data, b"hello");
        assert_eq!(read.entry("dir/b.bin").unwrap().data, &[0xA5u8; 7000]);
        assert_eq!(source.cursor(), layout.projected_size_blocks);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn streaming_reader_delivers_entries_without_full_object_buffer() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "vidéo/clip.txt",
                file_id: "file-b",
                data: b"utf8 path",
                mtime: None,
                executable: Some(true),
            },
        ];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        let mut source = VecBlockSource::new(sink.blocks);
        let mut entries = CollectingEntrySink::default();

        let report = stream_rem_tar_object(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            &mut entries,
        )
        .unwrap();

        assert_eq!(
            report.global_pax.get("REMANENCE.format_id").unwrap(),
            FORMAT_ID
        );
        assert_eq!(entries.data.get("a.txt").unwrap(), &b"hello".to_vec());
        assert_eq!(
            entries.data.get("vidéo/clip.txt").unwrap(),
            &b"utf8 path".to_vec()
        );
        assert_eq!(
            entries
                .entries
                .iter()
                .find(|entry| entry.path == "vidéo/clip.txt")
                .unwrap()
                .first_chunk_lba,
            layout.files[1].first_chunk_lba
        );
        assert_eq!(report.entries, entries.entries);
        assert_eq!(
            report.manifest_cbor.as_ref().unwrap(),
            &layout.manifest_cbor
        );
        assert_eq!(source.cursor(), layout.projected_size_blocks);
    }

    #[test]
    fn reader_rejects_wrong_format_id_before_payload_success() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        replace_once_in_blocks(&mut sink.blocks, b"rao-v1", b"rao-v0");
        let mut source = VecBlockSource::new(sink.blocks);

        let err = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::UnsupportedFormatGate {
                    gate: FormatGate::FormatId,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn encrypted_rao_round_trips_and_keyless_inspect_hides_inner_names() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "dir/b.bin",
                file_id: "file-b",
                data: &[0x5Au8; 5000],
                mtime: None,
                executable: Some(true),
            },
        ];
        let (primary, recipients) = recipient_pair();
        let mut sink = VecBlockSink::new();
        let write_report =
            write_encrypted_rao_object(&mut sink, &opts, &files, &recipients).unwrap();
        let encrypted: Vec<u8> = sink.blocks.iter().flatten().copied().collect();

        let inspected = inspect_bytes(&encrypted).unwrap();
        assert_eq!(inspected.header.object_id, opts.object_id);
        assert_eq!(inspected.key_frame.slots.len(), 2);
        assert_eq!(
            inspected.stored_size_bytes,
            write_report.envelope.stored_size_bytes
        );
        assert!(!contains_bytes(&encrypted, b"a.txt"));
        assert!(!contains_bytes(&encrypted, b"dir/b.bin"));
        assert!(!contains_bytes(&encrypted, MANIFEST_PATH.as_bytes()));

        let mut source = VecBlockSource::new(sink.blocks);
        let read = read_encrypted_rao_object(
            &mut source,
            opts.chunk_size,
            write_report.envelope.stored_size_blocks,
            &primary,
        )
        .unwrap();

        assert_eq!(
            read.object.global_pax.get("REMANENCE.format_id").unwrap(),
            FORMAT_ID
        );
        assert_eq!(read.object.entry("a.txt").unwrap().data, b"hello");
        assert_eq!(
            read.object.entry("dir/b.bin").unwrap().data,
            &[0x5Au8; 5000]
        );
        assert_eq!(
            read.envelope.metadata.plaintext_digest,
            write_report.envelope.plaintext.digest
        );
    }

    #[test]
    fn encrypted_rao_round_trips_through_file_block_adapters() {
        let opts = options(4096);
        let files = [
            RemTarFile {
                path: "a.txt",
                file_id: "file-a",
                data: b"hello",
                mtime: Some("0"),
                executable: Some(false),
            },
            RemTarFile {
                path: "secret/name.bin",
                file_id: "file-b",
                data: &[0x3Cu8; 9000],
                mtime: None,
                executable: Some(false),
            },
        ];
        let (primary, recipients) = recipient_pair();
        let path = temp_object_path("encrypted");
        let write_report = {
            let mut sink = FileBlockSink::create_truncate(&path, opts.chunk_size).unwrap();
            let report = write_encrypted_rao_object(&mut sink, &opts, &files, &recipients).unwrap();
            sink.flush().unwrap();
            report
        };
        let encrypted = fs::read(&path).unwrap();
        assert!(!contains_bytes(&encrypted, b"secret/name.bin"));
        assert_eq!(inspect_bytes(&encrypted).unwrap().key_frame.slots.len(), 2);

        let mut source = FileBlockSource::open(&path, opts.chunk_size).unwrap();
        assert_eq!(
            source.block_count(),
            write_report.envelope.stored_size_blocks
        );
        let block_count = source.block_count();
        let read =
            read_encrypted_rao_object(&mut source, opts.chunk_size, block_count, &primary).unwrap();
        assert_eq!(read.object.entry("a.txt").unwrap().data, b"hello");
        assert_eq!(
            read.object.entry("secret/name.bin").unwrap().data,
            &[0x3Cu8; 9000]
        );
        let _ = fs::remove_file(path);
    }

    #[test]
    fn encrypted_rao_wrong_key_fails_before_inner_plaintext_parse() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let (_primary, recipients) = recipient_pair();
        let wrong_key = RecipientPrivateKey::new([0x33; 16], "wrong-2026", [0x43; 32]).unwrap();
        let mut sink = VecBlockSink::new();
        let report = write_encrypted_rao_object(&mut sink, &opts, &files, &recipients).unwrap();
        let mut source = VecBlockSource::new(sink.blocks);

        let err = read_encrypted_rao_object(
            &mut source,
            opts.chunk_size,
            report.envelope.stored_size_blocks,
            &wrong_key,
        )
        .unwrap_err();

        assert!(matches!(err, FormatError::Aead(_)), "{err}");
    }

    #[test]
    fn encrypted_rao_rejects_inner_payload_digest_mismatch() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let (primary, recipients) = recipient_pair();
        let mut plaintext_sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut plaintext_sink, &opts, &files).unwrap();
        let mut plaintext: Vec<u8> = plaintext_sink.blocks.iter().flatten().copied().collect();
        let offset = layout.files[0].data_offset as usize;
        plaintext[offset] ^= 1;
        let digest = Sha256::digest(&plaintext);
        let mut plaintext_digest = [0u8; 32];
        plaintext_digest.copy_from_slice(&digest);
        let seal_options = EnvelopeSealOptions {
            allow_single_recipient: false,
            common: SealOptions {
                chunk_size: opts.chunk_size as u32,
                object_id: opts.object_id.clone(),
                plaintext_size: plaintext.len() as u64,
                plaintext_digest,
            },
            recipients,
        };
        let (sealed, report) = seal_to_vec(&plaintext, &seal_options).unwrap();
        let mut source = VecBlockSource::new(
            sealed
                .chunks_exact(opts.chunk_size)
                .map(Vec::from)
                .collect(),
        );

        let err = read_encrypted_rao_object(
            &mut source,
            opts.chunk_size,
            report.stored_size_blocks,
            &primary,
        )
        .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::FileDigestMismatch { ref path, .. } if path == "a.txt"
            ),
            "{err}"
        );
    }

    #[test]
    fn encrypted_rao_maps_inner_format_gate_without_string_matching() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let (primary, recipients) = recipient_pair();
        let mut plaintext_sink = VecBlockSink::new();
        let _layout = write_rem_tar_object(&mut plaintext_sink, &opts, &files).unwrap();
        replace_once_in_blocks(
            &mut plaintext_sink.blocks,
            b"REMANENCE.schema_version=1.0",
            b"REMANENCE.schema_version=2.0",
        );
        let plaintext: Vec<u8> = plaintext_sink.blocks.iter().flatten().copied().collect();
        let digest = Sha256::digest(&plaintext);
        let mut plaintext_digest = [0u8; 32];
        plaintext_digest.copy_from_slice(&digest);
        let seal_options = EnvelopeSealOptions {
            allow_single_recipient: false,
            common: SealOptions {
                chunk_size: opts.chunk_size as u32,
                object_id: opts.object_id.clone(),
                plaintext_size: plaintext.len() as u64,
                plaintext_digest,
            },
            recipients,
        };
        let (sealed, report) = seal_to_vec(&plaintext, &seal_options).unwrap();
        let mut source = VecBlockSource::new(
            sealed
                .chunks_exact(opts.chunk_size)
                .map(Vec::from)
                .collect(),
        );

        let err = read_encrypted_rao_object(
            &mut source,
            opts.chunk_size,
            report.stored_size_blocks,
            &primary,
        )
        .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::InnerObjectMismatch {
                    gate: FormatGate::SchemaVersion,
                    ..
                }
            ),
            "{err}"
        );
        assert!(!err.to_string().contains("format_id"));
    }

    #[test]
    fn reader_rejects_unsupported_schema_major_before_payload_success() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        replace_once_in_blocks(
            &mut sink.blocks,
            b"REMANENCE.schema_version=1.0",
            b"REMANENCE.schema_version=2.0",
        );
        let mut source = VecBlockSource::new(sink.blocks);

        let err = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
            .unwrap_err();

        assert!(
            matches!(
                err,
                FormatError::UnsupportedFormatGate {
                    gate: FormatGate::SchemaVersion,
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn streaming_reader_rejects_unsupported_file_compression() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        replace_once_in_blocks(
            &mut sink.blocks,
            b"REMANENCE.compression=none",
            b"REMANENCE.compression=gzip",
        );
        let mut source = VecBlockSource::new(sink.blocks);
        let mut entries = CollectingEntrySink::default();

        let err = stream_rem_tar_object(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            &mut entries,
        )
        .unwrap_err();

        assert!(matches!(err, FormatError::UnsupportedFeature(_)), "{err}");
        assert!(entries.data.is_empty());
    }

    #[test]
    fn rejects_single_zero_eof_record() {
        let mut bytes = vec![0u8; TAR_RECORD_SIZE];
        bytes.extend_from_slice(&[1u8; TAR_RECORD_SIZE]);
        let err = parse_rem_tar_bytes_with_mode_and_manifest_anchor(
            &bytes,
            1024,
            ReadMode::Restore,
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("single zero"));
    }

    #[test]
    fn streaming_reader_rejects_truncated_pax_body_before_allocation() {
        let header = encode_header(
            "GlobalHead.0/PaxHeaders/remanence",
            4096,
            TYPE_PAX_GLOBAL,
            0o644,
        )
        .unwrap();
        let mut source = VecBlockSource::new(vec![header.to_vec()]);
        let mut entries = CollectingEntrySink::default();

        let err = stream_rem_tar_object(&mut source, 512, 1, &mut entries).unwrap_err();

        assert!(matches!(err, FormatError::TruncatedPayload));
    }

    #[test]
    fn rejects_bad_header_checksum() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "a.txt",
            file_id: "file-a",
            data: b"hello",
            mtime: None,
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        write_rem_tar_object(&mut sink, &opts, &files).unwrap();
        sink.blocks[0][0] ^= 0x01;
        let mut source = VecBlockSource::new(sink.blocks);
        let err = read_rem_tar_object(&mut source, opts.chunk_size, 2).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
    }

    #[test]
    fn rejects_control_character_in_external_pax_value() {
        let mut records = std::collections::BTreeMap::new();
        records.insert("path".to_string(), "bad\nname".to_string());
        let body = b"17 path=bad\nname\n";
        let err = parse_pax_records(body).unwrap_err();
        assert!(err.to_string().contains("control character"));
    }

    #[test]
    fn rejects_leading_plus_in_decimal_fields() {
        let err = parse_decimal_u64("+1", "pax size").unwrap_err();
        assert!(matches!(err, FormatError::Parse(_)), "{err}");

        let mut global = BTreeMap::new();
        global.insert("REMANENCE.format_id".to_string(), FORMAT_ID.to_string());
        global.insert("REMANENCE.schema_version".to_string(), "+1.0".to_string());
        let mut validated = false;
        let err = validate_global_contract_once(4096, &global, &mut validated).unwrap_err();
        assert!(matches!(err, FormatError::Parse(_)), "{err}");
    }

    #[derive(Default)]
    struct CollectingEntrySink {
        active: Option<String>,
        entries: Vec<RemTarStreamEntry>,
        data: BTreeMap<String, Vec<u8>>,
    }

    impl RemTarEntrySink for CollectingEntrySink {
        fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
            assert!(self.active.is_none(), "nested begin_file");
            self.active = Some(entry.path.clone());
            self.entries.push(entry.clone());
            self.data.insert(entry.path.clone(), Vec::new());
            Ok(())
        }

        fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
            let active = self.active.as_ref().expect("active entry");
            self.data
                .get_mut(active)
                .expect("active data")
                .extend(bytes);
            Ok(())
        }

        fn end_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
            assert_eq!(self.active.as_deref(), Some(entry.path.as_str()));
            self.active = None;
            Ok(())
        }
    }
}
