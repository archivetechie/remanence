//! Streaming orchestration primitives above Layers 3b and 3c.
//!
//! This crate is intentionally small: it does not own the daemon API, library
//! robotics, or persistent SQLite schema. It composes the lower layers into the
//! first real write/restore surface:
//!
//! - prepass regular files into [`remanence_format::RemTarFileSpec`] values;
//! - stream those files through `rao-v1` and [`remanence_parity::ParitySink`];
//! - return catalog/audit projection rows for Layer 4/5 to commit atomically;
//! - stream an object-local block source back to a filesystem destination.

mod recovery;

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use remanence_format::{
    plan_rem_tar_object, stream_rem_tar_object, write_rem_tar_object_from_readers,
    ArchiveEventSink, ArchiveGapRange, ArchiveReader, BodyLba, DamageRange, DamageStatus,
    EntryCatalogSink, EntryKind, FormatError, NormalizedEntry, RemTarEntrySink, RemTarEntryType,
    RemTarFileLayout, RemTarFileSpec, RemTarFileStream, RemTarObjectLayout, RemTarObjectOptions,
    RemTarStreamEntry, RemTarStreamReport, ScanReport, StreamReport, FORMAT_ID, MANIFEST_PATH,
};
use remanence_library::{
    BlockSink, BlockSource, TapeIoError, TapePosition, WriteFilemarksOutcome, WriteOutcome,
};
use remanence_parity::{
    BootstrapObjectRow, BootstrapObjectRowAdmission, CapacityReserveInput, CommittedBundle,
    ObjectParityState, ObjectWriteSummary, ParityError, ParitySink, TapeFileKind,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicase::UniCase;
use unicode_normalization::UnicodeNormalization;
#[cfg(unix)]
use xattr::FileExt;

pub use recovery::{
    recover_archive_reader_to_directory, ArchiveRecoveryReport, RecoveryArchiveGapRecord,
    RecoveryByteRange, RecoveryDamageRecord, RecoveryFileRecord, RecoveryManifestRecord,
    RecoveryOptions, RecoveryStatus, RECOVERY_MANIFEST_RELATIVE_PATH,
};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;

/// Errors returned by streaming orchestration helpers.
#[derive(Debug, Error)]
pub enum StreamingError {
    /// Caller input was invalid before lower layers were invoked.
    #[error("invalid streaming input: {0}")]
    InvalidInput(String),

    /// An xattr namespace prefix could broaden the restore allow-list.
    #[error(
        "invalid xattr namespace prefix {prefix:?}: expected <name>. with a non-empty name, exactly one trailing dot, and no other dots or control characters"
    )]
    InvalidXattrNamespacePrefix {
        /// Rejected caller-supplied prefix.
        prefix: String,
    },

    /// Filesystem I/O failed at the named path.
    #[error("{context} at {}: {source}", path.display())]
    Io {
        /// Operation being performed.
        context: String,
        /// Path involved in the operation.
        path: PathBuf,
        /// Underlying I/O error.
        source: io::Error,
    },

    /// Layer 3b format error.
    #[error(transparent)]
    Format(#[from] FormatError),

    /// Layer 3c parity error.
    #[error(transparent)]
    Parity(#[from] ParityError),
}

/// One regular file prepared for a streaming `rao-v1` write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFile {
    /// Source path that will be opened again for the write pass.
    pub source_path: PathBuf,
    /// Archive metadata and precomputed content hash.
    pub spec: RemTarFileSpec,
}

/// Planned streaming object before tape admission.
#[derive(Debug, Clone)]
pub struct StreamingObjectPlan {
    /// File specs in write order.
    pub file_specs: Vec<RemTarFileSpec>,
    /// Exact `rao-v1` layout projected by Layer 3b.
    pub layout: RemTarObjectLayout,
}

/// Catalog-facing object row derived from a streaming write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCatalogProjection {
    /// Remanence object UUID.
    pub object_id: String,
    /// Opaque caller/orchestrator object identifier.
    pub caller_object_id: String,
    /// Body format identifier.
    pub body_format: String,
    /// Sum of payload file sizes, excluding the generated manifest.
    pub logical_size_bytes: u64,
    /// SHA-256 of the generated `rao-v1` manifest CBOR bytes.
    pub manifest_sha256: [u8; 32],
}

/// Catalog-facing file row derived from a streaming write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCatalogProjection {
    /// Remanence object UUID containing this file.
    pub object_id: String,
    /// Stable file identifier inside the object.
    pub file_id: String,
    /// UTF-8 path inside the object.
    pub path: String,
    /// Exact file payload size.
    pub size_bytes: u64,
    /// SHA-256 of the exact file payload bytes.
    pub file_sha256: [u8; 32],
    /// First object-local body LBA containing file data.
    pub first_chunk_lba: Option<BodyLba>,
    /// Number of body chunks containing file data.
    pub chunk_count: u64,
    /// Optional mtime pax value.
    pub mtime: Option<String>,
    /// Optional executable flag.
    pub executable: Option<bool>,
}

/// Catalog-facing object-copy row derived from a streaming write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectCopyProjection {
    /// Remanence object UUID.
    pub object_id: String,
    /// Tape UUID that received the object copy.
    pub tape_uuid: [u8; 16],
    /// Filemark-delimited tape-file number of the object.
    pub tape_file_number: u32,
    /// First parity data ordinal assigned by Layer 3c, absent for no-parity copies.
    pub first_parity_data_ordinal: Option<u64>,
    /// Number of object-data ordinals in the copy.
    pub data_block_count: u64,
    /// Highest protected ordinal after this object-close bundle, absent for no-parity copies.
    pub protected_until_ordinal: Option<u64>,
    /// Object-level parity state at that watermark, absent for no-parity copies.
    pub parity_state: Option<ObjectParityState>,
    /// SHA-256 of the canonical plaintext RAO object bytes.
    pub plaintext_digest: [u8; 32],
    /// SHA-256 of the stored representation bytes for this copy.
    pub stored_digest: [u8; 32],
}

/// Projection bundle Layer 5 can commit with the 3c journal bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingCatalogProjection {
    /// Object row.
    pub object: ObjectCatalogProjection,
    /// Per-file rows.
    pub files: Vec<FileCatalogProjection>,
    /// Object-copy row for this tape.
    pub object_copy: ObjectCopyProjection,
    /// 3c tape-file rows for the object-close commit bundle.
    pub tape_file_bundle: CommittedBundle,
}

/// Minimal audit event emitted by this orchestration surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingAuditEvent {
    /// Stable event kind.
    pub kind: &'static str,
    /// Remanence object UUID.
    pub object_id: String,
    /// Human-readable summary.
    pub summary: String,
}

/// Result of a complete streaming object write.
#[derive(Debug, Clone)]
pub struct StreamingObjectWriteReport {
    /// Planned and emitted Layer 3b layout.
    pub layout: RemTarObjectLayout,
    /// Layer 3c object-close summary.
    pub object_close: ObjectWriteSummary,
    /// Catalog projection rows ready for an atomic Layer 5 commit.
    pub catalog: StreamingCatalogProjection,
    /// Audit events ready for the Layer 4 audit log.
    pub audit_events: Vec<StreamingAuditEvent>,
}

/// Filesystem restore policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemRestoreOptions {
    /// Replace an existing destination file.
    pub overwrite: bool,
    /// Also restore Remanence's generated manifest file.
    pub include_manifest: bool,
    /// Xattr namespace prefixes that may be restored.
    pub xattr_allowed_prefixes: Vec<String>,
}

impl Default for FilesystemRestoreOptions {
    fn default() -> Self {
        Self {
            overwrite: false,
            include_manifest: false,
            xattr_allowed_prefixes: vec!["user.".to_string()],
        }
    }
}

/// Report from streaming an object to a filesystem directory.
#[derive(Debug, Clone)]
pub struct FilesystemRestoreReport {
    /// Layer 3b streaming parse report.
    pub stream: RemTarStreamReport,
    /// User-visible files written.
    pub files_written: u64,
    /// Directory entries materialized or confirmed.
    pub directories_seen: u64,
    /// Symbolic-link entries created.
    pub symlinks_written: u64,
    /// Hardlink entries created.
    pub hardlinks_written: u64,
    /// User-visible payload bytes written.
    pub bytes_written: u64,
    /// Archived xattr names skipped for each entry path; values are never reported.
    pub skipped_xattrs: BTreeMap<String, Vec<String>>,
    /// Applied xattr names outside `user.` for each entry path; values are never reported.
    pub applied_privileged_xattrs: BTreeMap<String, Vec<String>>,
}

/// Validate one xattr namespace prefix before it is used for restore matching.
///
/// A prefix is exactly one non-empty namespace followed by one dot, such as
/// `user.` or `security.`. Restricting the shape prevents `starts_with` from
/// turning short or unterminated prefixes into a broader allow-list.
pub fn validate_xattr_namespace_prefix(prefix: &str) -> Result<(), StreamingError> {
    let valid = prefix.strip_suffix('.').is_some_and(|namespace| {
        !namespace.is_empty()
            && !namespace.contains('.')
            && !namespace.chars().any(char::is_control)
    });
    if valid {
        Ok(())
    } else {
        Err(StreamingError::InvalidXattrNamespacePrefix {
            prefix: prefix.to_string(),
        })
    }
}

/// Report from scanning a normalized archive reader.
#[derive(Debug, Clone)]
pub struct ArchiveScanReport {
    /// Format-driver scan summary.
    pub scan: ScanReport,
    /// Entries returned in archive order.
    pub entries: Vec<NormalizedEntry>,
    /// Non-fatal damage events discovered while scanning.
    pub damages: Vec<DamageRange>,
    /// Source gaps discovered while scanning that are not tied to one file.
    pub archive_gaps: Vec<ArchiveGapRange>,
}

/// Report from restoring a normalized archive reader to a directory.
#[derive(Debug, Clone)]
pub struct ArchiveFilesystemRestoreReport {
    /// Format-driver streaming summary.
    pub stream: StreamReport,
    /// Regular files written.
    pub files_written: u64,
    /// Directory entries materialized or confirmed.
    pub directories_seen: u64,
    /// File payload bytes written.
    pub bytes_written: u64,
    /// Non-fatal damage events reported by the format driver.
    pub damages: Vec<DamageRange>,
    /// Source gaps reported by the format driver that are not tied to one file.
    pub archive_gaps: Vec<ArchiveGapRange>,
    /// Archived xattr names skipped for each entry path; normalized entries
    /// currently carry no xattr data, so this is empty by construction.
    pub skipped_xattrs: BTreeMap<String, Vec<String>>,
    /// Applied xattr names outside `user.` for each entry path; normalized
    /// entries currently carry no xattr data, so this is empty by construction.
    pub applied_privileged_xattrs: BTreeMap<String, Vec<String>>,
}

/// Parse and validate one operator-supplied xattr namespace prefix.
///
/// This is the shared clap-facing parser used by every restore binary.
pub fn parse_xattr_namespace_prefix(prefix: &str) -> Result<String, String> {
    validate_xattr_namespace_prefix(prefix)
        .map(|()| prefix.to_string())
        .map_err(|error| error.to_string())
}

/// Count the xattr names recorded in a restore report.
pub fn skipped_xattr_count(skipped: &BTreeMap<String, Vec<String>>) -> usize {
    skipped
        .values()
        .fold(0usize, |total, names| total.saturating_add(names.len()))
}

/// Build the single warning emitted from a restore report with skipped xattrs.
pub fn xattr_policy_skip_warning(skipped: &BTreeMap<String, Vec<String>>) -> Option<String> {
    xattr_policy_skip_warning_for_count(skipped_xattr_count(skipped))
}

/// Build the single warning emitted when a restore skips a known xattr count.
pub fn xattr_policy_skip_warning_for_count(skipped_count: usize) -> Option<String> {
    (skipped_count != 0).then(|| {
        format!(
            "{skipped_count} extended attribute(s) skipped by namespace policy; see skipped_xattrs in the report"
        )
    })
}

/// Prepass one regular file and produce a streaming write spec.
///
/// `archive_path` is the relative UTF-8 path that will be recorded inside the
/// object. This function rejects absolute paths, `..`, non-UTF-8 components,
/// and non-regular sources.
pub fn prepare_regular_file(
    source_path: impl AsRef<Path>,
    archive_path: impl AsRef<Path>,
    file_id: impl Into<String>,
) -> Result<PreparedFile, StreamingError> {
    let source_path = source_path.as_ref().to_path_buf();
    let archive_path = normalize_archive_path(archive_path.as_ref())?;
    let metadata = fs::metadata(&source_path)
        .map_err(|err| io_error("stat source file", &source_path, err))?;
    if !metadata.file_type().is_file() {
        return Err(StreamingError::InvalidInput(format!(
            "source path is not a regular file: {}",
            source_path.display()
        )));
    }

    let mut spec = RemTarFileSpec::new(
        archive_path,
        file_id.into(),
        metadata.len(),
        sha256_file(&source_path)?,
    );
    spec.mtime = metadata_mtime(&metadata);
    spec.executable = Some(is_executable(&metadata));
    Ok(PreparedFile { source_path, spec })
}

/// Plan a prepared object without touching tape.
pub fn plan_prepared_object(
    options: &RemTarObjectOptions,
    files: &[PreparedFile],
) -> Result<StreamingObjectPlan, StreamingError> {
    for file in files {
        if file.spec.entry_type != RemTarEntryType::Regular {
            return Err(StreamingError::InvalidInput(format!(
                "catalog projection supports regular files only, got {:?} for {}",
                file.spec.entry_type, file.spec.path
            )));
        }
    }
    let file_specs: Vec<RemTarFileSpec> = files.iter().map(|file| file.spec.clone()).collect();
    let layout = plan_rem_tar_object(options, &file_specs)?;
    Ok(StreamingObjectPlan { file_specs, layout })
}

/// Write prepared files as one `rao-v1` object through a parity sink.
///
/// The caller supplies `reserve` after applying its current tape-capacity
/// model. This function verifies that the reserve's projected object block
/// count matches the Layer 3b layout before opening the object. If a lower
/// layer fails after object admission, the parity session may need normal
/// abort/recovery handling by the caller. This initial surface assumes the
/// prepared files are stable between the hash prepass and write pass; mutable
/// source spooling is a separate policy layer.
pub fn write_prepared_object_to_parity(
    parity: &mut ParitySink<'_>,
    tape_uuid: [u8; 16],
    options: &RemTarObjectOptions,
    files: &[PreparedFile],
    reserve: CapacityReserveInput,
) -> Result<StreamingObjectWriteReport, StreamingError> {
    let mut readers = open_prepared_readers(files)?
        .into_iter()
        .map(|reader| Box::new(reader) as Box<dyn Read + Send>)
        .collect::<Vec<_>>();
    write_prepared_object_to_parity_from_readers(
        parity,
        tape_uuid,
        options,
        files,
        &mut readers,
        reserve,
    )
}

/// Write a prepared object from caller-owned readers through the same parity
/// and catalog funnel used by path-backed writes.
pub fn write_prepared_object_to_parity_from_readers(
    parity: &mut ParitySink<'_>,
    tape_uuid: [u8; 16],
    options: &RemTarObjectOptions,
    files: &[PreparedFile],
    readers: &mut [Box<dyn Read + Send>],
    reserve: CapacityReserveInput,
) -> Result<StreamingObjectWriteReport, StreamingError> {
    let plan = plan_prepared_object(options, files)?;
    validate_reserve(options, &plan.layout, reserve)?;
    if readers.len() != files.len() {
        return Err(StreamingError::InvalidInput(format!(
            "prepared file count {} does not match reader count {}",
            files.len(),
            readers.len()
        )));
    }
    let mut streams = Vec::with_capacity(files.len());
    for (file, reader) in files.iter().zip(readers.iter_mut()) {
        streams.push(RemTarFileStream::new(file.spec.clone(), reader.as_mut()));
    }

    let opened = parity.begin_object_with_capacity_reserve_and_bootstrap_object_row(
        reserve,
        BootstrapObjectRowAdmission::PlaintextRao,
    )?;
    let mut object_sink = ObjectDigestBlockSink::new(parity);
    let layout = write_rem_tar_object_from_readers(&mut object_sink, options, &mut streams)?;
    let object_digest = object_sink.finish_digest();
    let manifest_first_chunk_lba = layout.manifest.first_chunk_lba.ok_or_else(|| {
        StreamingError::InvalidInput("generated RAO manifest has no body LBA".to_string())
    })?;
    parity.record_bootstrap_object_row(
        BootstrapObjectRow::plaintext(
            opened.0,
            layout.projected_size_blocks,
            manifest_first_chunk_lba.0,
            layout.manifest.size_bytes,
            layout.manifest.chunk_count,
            layout.manifest_sha256,
        )
        .with_object_id(options.object_id.as_bytes().to_vec()),
    )?;
    let object_close = parity.finish_object()?;
    if opened.0 != object_close.tape_file_number {
        return Err(StreamingError::InvalidInput(
            "parity object tape-file number changed during write".to_string(),
        ));
    }
    if layout.projected_size_blocks != plan.layout.projected_size_blocks {
        return Err(StreamingError::InvalidInput(
            "emitted layout differs from pre-admission plan".to_string(),
        ));
    }

    let catalog = build_catalog_projection(
        tape_uuid,
        options,
        files,
        &layout,
        &object_close,
        object_digest,
    )?;
    let audit_events = vec![StreamingAuditEvent {
        kind: "streaming_object_committed",
        object_id: options.object_id.clone(),
        summary: format!(
            "committed object {} to tape file {} ({} payload files, {} object blocks)",
            options.object_id,
            object_close.tape_file_number,
            layout.files.len(),
            object_close.data_block_count
        ),
    }];
    Ok(StreamingObjectWriteReport {
        layout,
        object_close,
        catalog,
        audit_events,
    })
}

/// Restore one object-local block stream into a destination directory.
pub fn restore_object_to_directory<S: BlockSource + ?Sized>(
    source: &mut S,
    chunk_size: usize,
    block_count: u64,
    destination_root: impl AsRef<Path>,
    options: FilesystemRestoreOptions,
) -> Result<FilesystemRestoreReport, StreamingError> {
    let destination_root = destination_root.as_ref();
    let mut sink = FilesystemRestoreSink::new(destination_root, options)?;
    let stream = stream_rem_tar_object(source, chunk_size, block_count, &mut sink)?;
    let xattrs = apply_restored_xattrs(
        &sink.root,
        &stream.entries,
        &sink.options.xattr_allowed_prefixes,
    )?;
    Ok(FilesystemRestoreReport {
        stream,
        files_written: sink.files_written,
        directories_seen: sink.directories_seen,
        symlinks_written: sink.symlinks_written,
        hardlinks_written: sink.hardlinks_written,
        bytes_written: sink.bytes_written,
        skipped_xattrs: xattrs.skipped,
        applied_privileged_xattrs: xattrs.applied_privileged,
    })
}

/// Scan any normalized archive reader and collect its catalog entries.
pub fn scan_archive_reader(
    reader: &mut dyn ArchiveReader,
) -> Result<ArchiveScanReport, StreamingError> {
    let mut sink = CollectingCatalogSink::default();
    let scan = reader.scan(&mut sink)?;
    Ok(ArchiveScanReport {
        scan,
        entries: sink.entries,
        damages: sink.damages,
        archive_gaps: sink.archive_gaps,
    })
}

/// Restore any normalized archive reader into a destination directory.
pub fn restore_archive_reader_to_directory(
    reader: &mut dyn ArchiveReader,
    destination_root: impl AsRef<Path>,
    options: FilesystemRestoreOptions,
) -> Result<ArchiveFilesystemRestoreReport, StreamingError> {
    let mut sink = NormalizedFilesystemRestoreSink::new(destination_root.as_ref(), options)?;
    let stream = reader.stream_all(&mut sink)?;
    Ok(ArchiveFilesystemRestoreReport {
        stream,
        files_written: sink.files_written,
        directories_seen: sink.directories_seen,
        bytes_written: sink.bytes_written,
        damages: sink.damages,
        archive_gaps: sink.archive_gaps,
        skipped_xattrs: BTreeMap::new(),
        applied_privileged_xattrs: BTreeMap::new(),
    })
}

fn validate_reserve(
    options: &RemTarObjectOptions,
    layout: &RemTarObjectLayout,
    reserve: CapacityReserveInput,
) -> Result<(), StreamingError> {
    if reserve.projected_object_blocks != layout.projected_size_blocks {
        return Err(StreamingError::InvalidInput(format!(
            "capacity reserve projected {} object blocks, but layout projected {}",
            reserve.projected_object_blocks, layout.projected_size_blocks
        )));
    }
    if reserve.block_size_bytes != options.chunk_size as u64 {
        return Err(StreamingError::InvalidInput(format!(
            "capacity reserve block size {} does not match RAO chunk size {}",
            reserve.block_size_bytes, options.chunk_size
        )));
    }
    Ok(())
}

fn open_prepared_readers(files: &[PreparedFile]) -> Result<Vec<File>, StreamingError> {
    files
        .iter()
        .map(|file| {
            File::open(&file.source_path)
                .map_err(|err| io_error("open source file for streaming", &file.source_path, err))
        })
        .collect()
}

struct ObjectDigestBlockSink<'a, S: BlockSink + ?Sized> {
    inner: &'a mut S,
    hasher: Sha256,
}

impl<'a, S: BlockSink + ?Sized> ObjectDigestBlockSink<'a, S> {
    fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    fn finish_digest(self) -> [u8; 32] {
        let digest = self.hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

impl<S: BlockSink + ?Sized> BlockSink for ObjectDigestBlockSink<'_, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.hasher.update(buf);
        Ok(outcome)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

fn build_catalog_projection(
    tape_uuid: [u8; 16],
    options: &RemTarObjectOptions,
    prepared_files: &[PreparedFile],
    layout: &RemTarObjectLayout,
    object_close: &ObjectWriteSummary,
    object_digest: [u8; 32],
) -> Result<StreamingCatalogProjection, StreamingError> {
    if prepared_files.len() != layout.files.len() {
        return Err(StreamingError::InvalidInput(
            "prepared file count does not match emitted layout".to_string(),
        ));
    }
    let logical_size_bytes = layout.files.iter().try_fold(0u64, |acc, file| {
        acc.checked_add(file.size_bytes)
            .ok_or_else(|| StreamingError::InvalidInput("logical size overflow".to_string()))
    })?;
    let object = ObjectCatalogProjection {
        object_id: options.object_id.clone(),
        caller_object_id: options.caller_object_id.clone(),
        body_format: FORMAT_ID.to_string(),
        logical_size_bytes,
        manifest_sha256: layout.manifest_sha256,
    };
    let files = layout
        .files
        .iter()
        .zip(prepared_files.iter())
        .map(|(file, prepared)| file_catalog_projection(&options.object_id, file, &prepared.spec))
        .collect::<Result<Vec<_>, _>>()?;
    let parity_state = ObjectParityState::from_ordinals(
        object_close.first_parity_data_ordinal,
        object_close.data_block_count,
        object_close.highest_protected_ordinal,
    )?;
    let object_copy = ObjectCopyProjection {
        object_id: options.object_id.clone(),
        tape_uuid,
        tape_file_number: object_close.tape_file_number,
        first_parity_data_ordinal: Some(object_close.first_parity_data_ordinal),
        data_block_count: object_close.data_block_count,
        protected_until_ordinal: Some(object_close.highest_protected_ordinal),
        parity_state: Some(parity_state),
        plaintext_digest: object_digest,
        stored_digest: object_digest,
    };
    let mut tape_file_bundle = object_close.committed_bundle()?;
    attach_object_id_to_bundle(
        &mut tape_file_bundle,
        object_close.tape_file_number,
        options,
    );
    Ok(StreamingCatalogProjection {
        object,
        files,
        object_copy,
        tape_file_bundle,
    })
}

fn file_catalog_projection(
    object_id: &str,
    file: &RemTarFileLayout,
    spec: &RemTarFileSpec,
) -> Result<FileCatalogProjection, StreamingError> {
    let file_sha256 = file.file_sha256.ok_or_else(|| {
        StreamingError::InvalidInput(format!(
            "catalog projection supports regular files only, got {:?} for {}",
            file.entry_type, file.path
        ))
    })?;
    Ok(FileCatalogProjection {
        object_id: object_id.to_string(),
        file_id: file.file_id.clone(),
        path: file.path.clone(),
        size_bytes: file.size_bytes,
        file_sha256,
        first_chunk_lba: file.first_chunk_lba,
        chunk_count: file.chunk_count,
        mtime: spec.mtime.clone(),
        executable: file.executable,
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct XattrRestoreAccounting {
    skipped: BTreeMap<String, Vec<String>>,
    applied_privileged: BTreeMap<String, Vec<String>>,
}

#[cfg(unix)]
trait RestoredXattrSetter {
    fn set_path(&mut self, path: &Path, name: &str, value: &[u8]) -> io::Result<()>;
    fn set_file(&mut self, file: &File, name: &str, value: &[u8]) -> io::Result<()>;
}

#[cfg(unix)]
struct PlatformXattrSetter;

#[cfg(unix)]
impl RestoredXattrSetter for PlatformXattrSetter {
    fn set_path(&mut self, path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
        set_symlink_xattr_nofollow(path, name, value)
    }

    fn set_file(&mut self, file: &File, name: &str, value: &[u8]) -> io::Result<()> {
        file.set_xattr(name, value)
    }
}

/// Set an xattr on a restored symlink entry itself, never on its target.
///
/// `xattr` 1.6.1's path-based `set` is the no-follow operation backed by
/// `lsetxattr`; the workspace pins that audited behavior at the dependency.
#[cfg(unix)]
fn set_symlink_xattr_nofollow(path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
    xattr::set(path, name, value)
}

#[cfg(unix)]
fn apply_restored_xattrs(
    root: &Path,
    entries: &[RemTarStreamEntry],
    allowed_prefixes: &[String],
) -> Result<XattrRestoreAccounting, StreamingError> {
    let mut setter = PlatformXattrSetter;
    apply_restored_xattrs_with_setter(root, entries, allowed_prefixes, &mut setter)
}

#[cfg(unix)]
fn apply_restored_xattrs_with_setter<S: RestoredXattrSetter>(
    root: &Path,
    entries: &[RemTarStreamEntry],
    allowed_prefixes: &[String],
    setter: &mut S,
) -> Result<XattrRestoreAccounting, StreamingError> {
    for prefix in allowed_prefixes {
        validate_xattr_namespace_prefix(prefix)?;
    }

    let mut accounting = XattrRestoreAccounting::default();
    for entry in entries {
        if entry.xattrs.is_empty() || entry.path == MANIFEST_PATH {
            continue;
        }
        let mut allowed = Vec::new();
        let mut skipped = Vec::new();
        for (name, value) in &entry.xattrs {
            if restored_xattr_name_is_valid(name)
                && allowed_prefixes
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
            {
                allowed.push((name, value));
            } else {
                skipped.push(name.clone());
            }
        }
        if allowed.is_empty() {
            accounting.skipped.insert(entry.path.clone(), skipped);
            continue;
        }
        let mut applied_privileged = Vec::new();
        let relative = normalize_archive_path(Path::new(&entry.path))?;
        let (destination, final_component_is_symlink) =
            resolve_restored_entry_path_for_xattrs(root, relative.as_str(), entry)?;
        if entry.entry_type == RemTarEntryType::Symlink {
            for (name, value) in allowed {
                if restored_xattr_set_applied(
                    setter.set_path(&destination, name, value),
                    "set restore symlink xattr",
                    &destination,
                )? {
                    if !name.starts_with("user.") {
                        applied_privileged.push(name.clone());
                    }
                } else {
                    skipped.push(name.clone());
                }
            }
        } else if final_component_is_symlink {
            skipped.extend(allowed.into_iter().map(|(name, _)| name.clone()));
        } else {
            let Some(file) = open_restored_xattr_target(&destination)? else {
                skipped.extend(allowed.into_iter().map(|(name, _)| name.clone()));
                skipped.sort();
                accounting.skipped.insert(entry.path.clone(), skipped);
                continue;
            };
            for (name, value) in allowed {
                if restored_xattr_set_applied(
                    setter.set_file(&file, name, value),
                    "set restore xattr",
                    &destination,
                )? {
                    if !name.starts_with("user.") {
                        applied_privileged.push(name.clone());
                    }
                } else {
                    skipped.push(name.clone());
                }
            }
        }
        if !skipped.is_empty() {
            skipped.sort();
            accounting.skipped.insert(entry.path.clone(), skipped);
        }
        if !applied_privileged.is_empty() {
            applied_privileged.sort();
            accounting
                .applied_privileged
                .insert(entry.path.clone(), applied_privileged);
        }
    }
    Ok(accounting)
}

#[cfg(not(unix))]
fn apply_restored_xattrs(
    _root: &Path,
    entries: &[RemTarStreamEntry],
    allowed_prefixes: &[String],
) -> Result<XattrRestoreAccounting, StreamingError> {
    for prefix in allowed_prefixes {
        validate_xattr_namespace_prefix(prefix)?;
    }

    let skipped = entries
        .iter()
        .filter(|entry| !entry.xattrs.is_empty() && entry.path != MANIFEST_PATH)
        .map(|entry| {
            (
                entry.path.clone(),
                entry.xattrs.keys().cloned().collect::<Vec<_>>(),
            )
        })
        .collect();
    Ok(XattrRestoreAccounting {
        skipped,
        applied_privileged: BTreeMap::new(),
    })
}

#[cfg(unix)]
fn restored_xattr_name_is_valid(name: &str) -> bool {
    if name.is_empty() || name.as_bytes().contains(&0) {
        return false;
    }
    #[cfg(any(target_os = "android", target_os = "linux"))]
    if name.len() > 255 {
        return false;
    }
    name.split_once('.')
        .is_some_and(|(namespace, attribute)| !namespace.is_empty() && !attribute.is_empty())
}

#[cfg(unix)]
fn restored_xattr_set_applied(
    result: io::Result<()>,
    context: &str,
    path: &Path,
) -> Result<bool, StreamingError> {
    match result {
        Ok(()) => Ok(true),
        Err(error) if xattrs_unsupported(&error) => Ok(false),
        Err(error) => Err(io_error(context, path, error)),
    }
}

#[cfg(unix)]
fn resolve_restored_entry_path_for_xattrs(
    root: &Path,
    relative: &str,
    entry: &RemTarStreamEntry,
) -> Result<(PathBuf, bool), StreamingError> {
    let parts = normalized_relative_components(relative)?;
    let mut path = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        path.push(part);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|err| io_error("stat restore xattr parent", &path, err))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(StreamingError::InvalidInput(format!(
                "restore xattr parent is not a real directory: {}",
                path.display()
            )));
        }
    }
    if let Some(last) = parts.last() {
        path.push(last);
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|err| io_error("stat restore xattr path", &path, err))?;
    if metadata.file_type().is_symlink() && entry.entry_type != RemTarEntryType::Symlink {
        return Ok((path, true));
    }
    let valid = match entry.entry_type {
        RemTarEntryType::Regular | RemTarEntryType::Hardlink => metadata.file_type().is_file(),
        RemTarEntryType::Directory => metadata.file_type().is_dir(),
        RemTarEntryType::Symlink => metadata.file_type().is_symlink(),
    };
    if !valid {
        return Err(StreamingError::InvalidInput(format!(
            "restore xattr path has wrong type for {}: {}",
            entry.path,
            path.display()
        )));
    }
    Ok((path, metadata.file_type().is_symlink()))
}

#[cfg(unix)]
fn open_restored_xattr_target(path: &Path) -> Result<Option<File>, StreamingError> {
    let mut options = OpenOptions::new();
    options.read(true);
    apply_restore_open_flags(&mut options);
    match options.open(path) {
        Ok(file) => Ok(Some(file)),
        Err(err) if err.raw_os_error() == Some(nix::libc::ELOOP) => Ok(None),
        Err(err) => Err(io_error("open restore xattr target", path, err)),
    }
}

#[cfg(unix)]
fn xattrs_unsupported(error: &io::Error) -> bool {
    error.raw_os_error() == Some(nix::libc::EOPNOTSUPP)
        || error.raw_os_error() == Some(nix::libc::ENOTSUP)
}

fn attach_object_id_to_bundle(
    bundle: &mut CommittedBundle,
    object_tape_file_number: u32,
    options: &RemTarObjectOptions,
) {
    for entry in &mut bundle.entries {
        if entry.kind == TapeFileKind::Object && entry.tape_file_number == object_tape_file_number {
            entry.object_id = Some(options.object_id.clone());
        }
    }
}

fn sha256_file(path: &Path) -> Result<[u8; 32], StreamingError> {
    let file =
        File::open(path).map_err(|err| io_error("open source file for hashing", path, err))?;
    let mut reader = BufReader::with_capacity(HASH_BUFFER_BYTES, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|err| io_error("read source file for hashing", path, err))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Ok(out)
}

fn normalize_archive_path(path: &Path) -> Result<String, StreamingError> {
    if path.as_os_str().is_empty() {
        return Err(StreamingError::InvalidInput(
            "archive path must not be empty".to_string(),
        ));
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    StreamingError::InvalidInput("archive path must be UTF-8".to_string())
                })?;
                if text.is_empty() {
                    return Err(StreamingError::InvalidInput(
                        "archive path component must not be empty".to_string(),
                    ));
                }
                parts.push(text.to_string());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(StreamingError::InvalidInput(format!(
                    "archive path must be relative and stay inside the object: {}",
                    path.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(StreamingError::InvalidInput(
            "archive path must contain a file name".to_string(),
        ));
    }
    Ok(parts.join("/"))
}

fn metadata_mtime(metadata: &fs::Metadata) -> Option<String> {
    metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs().to_string())
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn io_error(context: impl Into<String>, path: &Path, source: io::Error) -> StreamingError {
    StreamingError::Io {
        context: context.into(),
        path: path.to_path_buf(),
        source,
    }
}

fn format_sink_io(context: impl Into<String>, path: &Path, source: io::Error) -> FormatError {
    FormatError::SourceIo {
        context: format!("{} at {}", context.into(), path.display()),
        source,
    }
}

fn restore_path_error(path: &Path, message: impl Into<String>) -> FormatError {
    FormatError::Parse(format!(
        "restore path {} {}",
        path.display(),
        message.into()
    ))
}

/// One destination resolved by the native restore path mapper.
struct NativeRestoreDestination {
    relative: PathBuf,
    destination: PathBuf,
    collision_key: Vec<String>,
}

/// Resolve one RAO entry path using the host platform's native path semantics.
fn resolve_native_restore_destination(
    root: &Path,
    archive_path: &str,
) -> Result<NativeRestoreDestination, FormatError> {
    let native_path = Path::new(archive_path);
    if native_path.as_os_str().is_empty() {
        return Err(FormatError::invalid_path(
            "restore path must contain a file name",
        ));
    }
    if native_path.is_absolute() {
        return Err(FormatError::invalid_path(format!(
            "restore path {archive_path:?} is absolute"
        )));
    }
    if has_empty_native_component(archive_path) {
        return Err(FormatError::invalid_path(format!(
            "restore path {archive_path:?} contains an empty native component"
        )));
    }

    let mut relative = PathBuf::new();
    let mut collision_key = Vec::new();
    for component in native_path.components() {
        match component {
            Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    FormatError::invalid_path(format!("restore path {archive_path:?} is not UTF-8"))
                })?;
                if text.is_empty() {
                    return Err(FormatError::invalid_path(format!(
                        "restore path {archive_path:?} contains an empty native component"
                    )));
                }
                relative.push(part);
                collision_key.push(native_collision_component(text));
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(FormatError::invalid_path(format!(
                    "restore path {archive_path:?} is not a normalized native relative path"
                )));
            }
        }
    }
    if collision_key.is_empty() {
        return Err(FormatError::invalid_path(format!(
            "restore path {archive_path:?} must contain a file name"
        )));
    }

    let destination = root.join(&relative);
    if !destination.starts_with(root) {
        return Err(FormatError::invalid_path(format!(
            "restore path {archive_path:?} escapes destination {}",
            root.display()
        )));
    }
    Ok(NativeRestoreDestination {
        relative,
        destination,
        collision_key,
    })
}

/// Detect adjacent separators that native [`Path`] iteration would normalize away.
fn has_empty_native_component(archive_path: &str) -> bool {
    let mut previous_was_separator = false;
    for character in archive_path.chars() {
        let is_separator =
            character == std::path::MAIN_SEPARATOR || (cfg!(windows) && character == '/');
        if is_separator && previous_was_separator {
            return true;
        }
        previous_was_separator = is_separator;
    }
    false
}

/// Produce a conservative native collision key using full case folding and NFD.
fn native_collision_component(component: &str) -> String {
    let decomposed = component.nfd().collect::<String>();
    UniCase::unicode(decomposed)
        .to_folded_case()
        .nfd()
        .collect()
}

fn ensure_restore_root(root: &Path) -> Result<(), StreamingError> {
    fs::create_dir_all(root).map_err(|err| io_error("create restore root", root, err))?;
    let metadata =
        fs::symlink_metadata(root).map_err(|err| io_error("inspect restore root", root, err))?;
    if metadata.file_type().is_symlink() {
        return Err(StreamingError::InvalidInput(format!(
            "restore root must not be a symlink: {}",
            root.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(StreamingError::InvalidInput(format!(
            "restore root must be a directory: {}",
            root.display()
        )));
    }
    Ok(())
}

fn normalized_relative_components(relative: impl AsRef<Path>) -> Result<Vec<PathBuf>, FormatError> {
    let relative = relative.as_ref();
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(PathBuf::from(part)),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(FormatError::Parse(format!(
                    "restore path {} is not a normalized relative path",
                    relative.display()
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(FormatError::Parse(
            "restore path must contain a file name".to_string(),
        ));
    }
    Ok(parts)
}

fn ensure_restore_directory(path: &Path) -> Result<(), FormatError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(restore_path_error(
            path,
            "escapes destination through a symlink",
        )),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(restore_path_error(path, "exists but is not a directory")),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            fs::create_dir(path)
                .map_err(|err| format_sink_io("create restore directory", path, err))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|err| format_sink_io("inspect restore directory", path, err))?;
            if metadata.file_type().is_symlink() {
                return Err(restore_path_error(
                    path,
                    "escapes destination through a symlink",
                ));
            }
            Ok(())
        }
        Err(err) => Err(format_sink_io("inspect restore directory", path, err)),
    }
}

fn create_restore_dirs_secure(
    root: &Path,
    relative: impl AsRef<Path>,
) -> Result<PathBuf, FormatError> {
    let parts = normalized_relative_components(relative)?;
    let mut current = root.to_path_buf();
    for part in parts {
        current.push(part);
        ensure_restore_directory(&current)?;
    }
    Ok(current)
}

fn create_restore_parent_dirs_secure(
    root: &Path,
    relative: impl AsRef<Path>,
) -> Result<PathBuf, FormatError> {
    let parts = normalized_relative_components(relative)?;
    let mut destination = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        destination.push(part);
        ensure_restore_directory(&destination)?;
    }
    destination.push(parts.last().expect("parts is not empty"));
    reject_existing_restore_symlink(&destination)?;
    Ok(destination)
}

fn resolve_existing_restore_file_secure(
    root: &Path,
    relative: impl AsRef<Path>,
) -> Result<PathBuf, FormatError> {
    let parts = normalized_relative_components(relative)?;
    let mut target = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        target.push(part);
        ensure_restore_directory(&target)?;
    }
    target.push(parts.last().expect("parts is not empty"));
    match fs::symlink_metadata(&target) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(restore_path_error(
            &target,
            "escapes destination through a symlink",
        )),
        Ok(metadata) if metadata.is_file() => Ok(target),
        Ok(_) => Err(restore_path_error(&target, "is not a regular file")),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Err(restore_path_error(&target, "does not exist"))
        }
        Err(err) => Err(format_sink_io(
            "inspect restore hardlink target",
            &target,
            err,
        )),
    }
}

fn reject_existing_restore_symlink(path: &Path) -> Result<(), FormatError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(restore_path_error(
            path,
            "escapes destination through a symlink",
        )),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format_sink_io("inspect restore path", path, err)),
    }
}

fn prepare_restore_symlink_destination(path: &Path, overwrite: bool) -> Result<(), FormatError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(restore_path_error(
            path,
            "escapes destination through a symlink",
        )),
        Ok(metadata) if overwrite && metadata.is_file() => {
            fs::remove_file(path)
                .map_err(|err| format_sink_io("remove existing restore file", path, err))?;
            Ok(())
        }
        Ok(_) => Err(restore_path_error(path, "already exists")),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format_sink_io("inspect restore path", path, err)),
    }
}

#[cfg(unix)]
fn create_restore_symlink(target: &str, destination: &Path) -> Result<(), FormatError> {
    std::os::unix::fs::symlink(target, destination)
        .map_err(|err| format_sink_io("create restore symlink", destination, err))
}

#[cfg(not(unix))]
fn create_restore_symlink(_target: &str, destination: &Path) -> Result<(), FormatError> {
    Err(restore_path_error(
        destination,
        "cannot restore symlink entries on this platform",
    ))
}

fn create_restore_hardlink(target: &Path, destination: &Path) -> Result<(), FormatError> {
    fs::hard_link(target, destination)
        .map_err(|err| format_sink_io("create restore hardlink", destination, err))
}

#[cfg(unix)]
fn apply_restore_open_flags(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    options.custom_flags(nix::libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
fn apply_restore_open_flags(_options: &mut OpenOptions) {}

#[derive(Default)]
struct CollectingCatalogSink {
    entries: Vec<NormalizedEntry>,
    damages: Vec<DamageRange>,
    archive_gaps: Vec<ArchiveGapRange>,
}

impl EntryCatalogSink for CollectingCatalogSink {
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        self.entries.push(entry.clone());
        Ok(())
    }

    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        self.damages.push(range.clone());
        Ok(())
    }

    fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        self.archive_gaps.push(range.clone());
        Ok(())
    }
}

struct FilesystemRestoreSink {
    root: PathBuf,
    options: FilesystemRestoreOptions,
    native_destinations: BTreeMap<Vec<String>, String>,
    current: RestoreTarget,
    files_written: u64,
    directories_seen: u64,
    symlinks_written: u64,
    hardlinks_written: u64,
    bytes_written: u64,
}

impl FilesystemRestoreSink {
    fn new(root: &Path, options: FilesystemRestoreOptions) -> Result<Self, StreamingError> {
        ensure_restore_root(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            options,
            native_destinations: BTreeMap::new(),
            current: RestoreTarget::None,
            files_written: 0,
            directories_seen: 0,
            symlinks_written: 0,
            hardlinks_written: 0,
            bytes_written: 0,
        })
    }

    /// Resolve and reserve one destination before its filesystem entry is created.
    fn register_native_destination(&mut self, archive_path: &str) -> Result<PathBuf, FormatError> {
        let resolved = resolve_native_restore_destination(&self.root, archive_path)?;
        if let Some(previous) = self
            .native_destinations
            .insert(resolved.collision_key, archive_path.to_string())
        {
            return Err(FormatError::invalid_path(format!(
                "restore entries {previous:?} and {archive_path:?} collide after native case folding and Unicode normalization at {}",
                resolved.destination.display()
            )));
        }
        Ok(resolved.relative)
    }
}

impl RemTarEntrySink for FilesystemRestoreSink {
    fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        if entry.path == MANIFEST_PATH && !self.options.include_manifest {
            self.current = RestoreTarget::Skip;
            return Ok(());
        }
        let relative = self.register_native_destination(&entry.path)?;
        match entry.entry_type {
            RemTarEntryType::Regular => {
                let expected_sha256 = expected_file_sha256(entry)?;
                let destination =
                    create_restore_parent_dirs_secure(&self.root, relative.as_path())?;
                let mut options = OpenOptions::new();
                options.write(true);
                if self.options.overwrite {
                    options.create(true).truncate(true);
                } else {
                    options.create_new(true);
                }
                apply_restore_open_flags(&mut options);
                let file = options
                    .open(&destination)
                    .map_err(|err| format_sink_io("open restore file", &destination, err))?;
                self.current = RestoreTarget::File {
                    destination,
                    file,
                    bytes_written: 0,
                    hasher: Sha256::new(),
                    expected_sha256,
                };
            }
            RemTarEntryType::Directory => {
                create_restore_dirs_secure(&self.root, relative.as_path())?;
                self.directories_seen += 1;
                self.current = RestoreTarget::Directory;
            }
            RemTarEntryType::Hardlink => {
                let target = entry.link_target.as_deref().ok_or_else(|| {
                    FormatError::Parse(format!("restore hardlink {} is missing target", entry.path))
                })?;
                let target_relative =
                    resolve_native_restore_destination(&self.root, target)?.relative;
                let source =
                    resolve_existing_restore_file_secure(&self.root, target_relative.as_path())?;
                let destination =
                    create_restore_parent_dirs_secure(&self.root, relative.as_path())?;
                prepare_restore_symlink_destination(&destination, self.options.overwrite)?;
                create_restore_hardlink(&source, &destination)?;
                self.hardlinks_written += 1;
                self.current = RestoreTarget::Hardlink;
            }
            RemTarEntryType::Symlink => {
                let target = entry.link_target.as_deref().ok_or_else(|| {
                    FormatError::Parse(format!("restore symlink {} is missing target", entry.path))
                })?;
                let destination =
                    create_restore_parent_dirs_secure(&self.root, relative.as_path())?;
                prepare_restore_symlink_destination(&destination, self.options.overwrite)?;
                create_restore_symlink(target, &destination)?;
                self.symlinks_written += 1;
                self.current = RestoreTarget::Symlink;
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
        match &mut self.current {
            RestoreTarget::None => Err(FormatError::Parse(
                "restore data arrived without an active file".to_string(),
            )),
            RestoreTarget::Skip => Ok(()),
            RestoreTarget::Directory | RestoreTarget::Hardlink | RestoreTarget::Symlink => Err(
                FormatError::Parse("restore data arrived for a zero-payload entry".to_string()),
            ),
            RestoreTarget::File {
                destination,
                file,
                bytes_written,
                hasher,
                ..
            } => {
                file.write_all(bytes)
                    .map_err(|err| format_sink_io("write restore file", destination, err))?;
                hasher.update(bytes);
                *bytes_written += bytes.len() as u64;
                self.bytes_written += bytes.len() as u64;
                Ok(())
            }
        }
    }

    fn end_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        match std::mem::replace(&mut self.current, RestoreTarget::None) {
            RestoreTarget::None => Err(FormatError::Parse(
                "restore file ended without an active file".to_string(),
            )),
            RestoreTarget::Directory
            | RestoreTarget::Hardlink
            | RestoreTarget::Symlink
            | RestoreTarget::Skip => Ok(()),
            RestoreTarget::File {
                destination,
                mut file,
                bytes_written,
                hasher,
                expected_sha256,
            } => {
                if bytes_written != entry.size_bytes {
                    return Err(FormatError::Parse(format!(
                        "restore file {} wrote {} bytes, expected {}",
                        entry.path, bytes_written, entry.size_bytes
                    )));
                }
                let actual = hasher.finalize();
                if actual.as_slice() != expected_sha256.as_slice() {
                    return Err(FormatError::file_digest_mismatch(
                        entry.path.clone(),
                        &expected_sha256,
                        &actual,
                    ));
                }
                file.flush()
                    .map_err(|err| format_sink_io("flush restore file", &destination, err))?;
                self.files_written += 1;
                Ok(())
            }
        }
    }
}

/// Filesystem sink for normalized/deep-recovery archive readers.
///
/// [`NormalizedEntry`] deliberately exposes no xattr field, so this sink
/// applies no attributes by construction. Native RAO restores use the single
/// xattr-writer funnel in [`apply_restored_xattrs`] instead.
struct NormalizedFilesystemRestoreSink {
    root: PathBuf,
    options: FilesystemRestoreOptions,
    current: NormalizedRestoreTarget,
    files_written: u64,
    directories_seen: u64,
    bytes_written: u64,
    damages: Vec<DamageRange>,
    archive_gaps: Vec<ArchiveGapRange>,
}

impl NormalizedFilesystemRestoreSink {
    fn new(root: &Path, options: FilesystemRestoreOptions) -> Result<Self, StreamingError> {
        for prefix in &options.xattr_allowed_prefixes {
            validate_xattr_namespace_prefix(prefix)?;
        }
        ensure_restore_root(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            options,
            current: NormalizedRestoreTarget::None,
            files_written: 0,
            directories_seen: 0,
            bytes_written: 0,
            damages: Vec::new(),
            archive_gaps: Vec::new(),
        })
    }
}

impl ArchiveEventSink for NormalizedFilesystemRestoreSink {
    fn begin_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        let relative = normalize_archive_path(Path::new(&entry.path)).map_err(|err| {
            FormatError::Parse(format!("invalid restore path {}: {err}", entry.path))
        })?;
        match entry.kind {
            EntryKind::Directory => {
                create_restore_dirs_secure(&self.root, relative.as_str())?;
                self.directories_seen += 1;
                self.current = NormalizedRestoreTarget::Directory;
            }
            EntryKind::RegularFile => {
                let destination = create_restore_parent_dirs_secure(&self.root, relative.as_str())?;
                let mut options = OpenOptions::new();
                options.write(true);
                if self.options.overwrite {
                    options.create(true).truncate(true);
                } else {
                    options.create_new(true);
                }
                apply_restore_open_flags(&mut options);
                let file = options
                    .open(&destination)
                    .map_err(|err| format_sink_io("open restore file", &destination, err))?;
                self.current = NormalizedRestoreTarget::File {
                    file_id: entry.file_id.clone(),
                    destination,
                    file,
                    expected_size: entry.size_bytes,
                    bytes_written: 0,
                    covered_ranges: Vec::new(),
                };
            }
            EntryKind::Symlink | EntryKind::Hardlink | EntryKind::Special => {
                self.current = NormalizedRestoreTarget::Skip;
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError> {
        match &mut self.current {
            NormalizedRestoreTarget::None => Err(FormatError::Parse(
                "restore data arrived without an active entry".to_string(),
            )),
            NormalizedRestoreTarget::Directory => Err(FormatError::Parse(
                "restore data arrived for a directory entry".to_string(),
            )),
            NormalizedRestoreTarget::Skip => Ok(()),
            NormalizedRestoreTarget::File {
                destination,
                file,
                expected_size,
                bytes_written,
                covered_ranges,
                ..
            } => {
                let end_offset = file_offset.checked_add(bytes.len() as u64).ok_or_else(|| {
                    FormatError::Parse("restore file offset overflow".to_string())
                })?;
                if expected_size.is_some_and(|expected| end_offset > expected) {
                    return Err(FormatError::Parse(format!(
                        "restore data for {} extends beyond declared size",
                        destination.display()
                    )));
                }
                file.seek(SeekFrom::Start(file_offset))
                    .map_err(|err| format_sink_io("seek restore file", destination, err))?;
                file.write_all(bytes)
                    .map_err(|err| format_sink_io("write restore file", destination, err))?;
                covered_ranges.push((file_offset, end_offset));
                *bytes_written += bytes.len() as u64;
                self.bytes_written += bytes.len() as u64;
                Ok(())
            }
        }
    }

    fn report_damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        if let NormalizedRestoreTarget::File {
            file_id,
            covered_ranges,
            ..
        } = &mut self.current
        {
            if *file_id == range.file_id
                && matches!(
                    range.status,
                    DamageStatus::Missing | DamageStatus::ReadError | DamageStatus::Unsupported
                )
            {
                covered_ranges.push((range.start, range.end));
            }
        }
        self.damages.push(range.clone());
        Ok(())
    }

    fn report_archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        self.archive_gaps.push(range.clone());
        Ok(())
    }

    fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        match std::mem::replace(&mut self.current, NormalizedRestoreTarget::None) {
            NormalizedRestoreTarget::None => Err(FormatError::Parse(
                "restore entry ended without an active entry".to_string(),
            )),
            NormalizedRestoreTarget::Directory | NormalizedRestoreTarget::Skip => Ok(()),
            NormalizedRestoreTarget::File {
                destination,
                mut file,
                mut covered_ranges,
                ..
            } => {
                if let Some(expected) = entry.size_bytes {
                    normalize_ranges(&mut covered_ranges);
                    if !ranges_cover(&covered_ranges, expected) {
                        return Err(FormatError::Parse(format!(
                            "restore file {} did not cover declared size {}",
                            entry.path, expected
                        )));
                    }
                }
                file.flush()
                    .map_err(|err| format_sink_io("flush restore file", &destination, err))?;
                self.files_written += 1;
                Ok(())
            }
        }
    }
}

enum NormalizedRestoreTarget {
    None,
    Directory,
    Skip,
    File {
        file_id: remanence_format::FileId,
        destination: PathBuf,
        file: File,
        expected_size: Option<u64>,
        bytes_written: u64,
        covered_ranges: Vec<(u64, u64)>,
    },
}

fn normalize_ranges(ranges: &mut Vec<(u64, u64)>) {
    ranges.retain(|(start, end)| start < end);
    ranges.sort_unstable_by_key(|(start, end)| (*start, *end));
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges.drain(..) {
        if let Some((_, last_end)) = merged.last_mut() {
            if start <= *last_end {
                *last_end = (*last_end).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    *ranges = merged;
}

fn ranges_cover(ranges: &[(u64, u64)], expected_end: u64) -> bool {
    if expected_end == 0 {
        return true;
    }
    let mut covered_until = 0u64;
    for &(start, end) in ranges {
        if start > covered_until {
            return false;
        }
        covered_until = covered_until.max(end);
        if covered_until >= expected_end {
            return true;
        }
    }
    false
}

enum RestoreTarget {
    None,
    Skip,
    Directory,
    Hardlink,
    Symlink,
    File {
        destination: PathBuf,
        file: File,
        bytes_written: u64,
        hasher: Sha256,
        expected_sha256: [u8; 32],
    },
}

fn expected_file_sha256(entry: &RemTarStreamEntry) -> Result<[u8; 32], FormatError> {
    let value = entry
        .pax_records
        .get("REMANENCE.file_sha256")
        .ok_or_else(|| {
            FormatError::Parse(format!(
                "restore file {} is missing REMANENCE.file_sha256",
                entry.path
            ))
        })?;
    parse_sha256_hex(value).map_err(|err| {
        FormatError::Parse(format!(
            "restore file {} has invalid REMANENCE.file_sha256: {err}",
            entry.path
        ))
    })
}

fn parse_sha256_hex(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err(format!("expected 64 hex characters, got {}", value.len()));
    }
    let mut out = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(chunk[0]).ok_or_else(|| "contains non-hex characters".to_string())?;
        let low = hex_nibble(chunk[1]).ok_or_else(|| "contains non-hex characters".to_string())?;
        out[index] = (high << 4) | low;
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use remanence_format::{RemTarEntryType, RemTarObjectOptions};
    use remanence_library::{TapeIoError, VecBlockSink, VecBlockSource, VecBlockSourceCall};
    use remanence_parity::{
        BlockSinkRawTapeSink, BootstrapObjectRepresentation, CommittedBundle, CommittedBundleKind,
        CommittedState, FilemarkMap, JournalError, ObjectParitySource, OpenTrust, ParityScheme,
        PhysicalPositionHint, RawReadOutcome, RawTapeSource, SchemeId, ScopedFilemarkMap,
        SpaceFilemarksOutcome, TapeFileJournal, TapeFileMapEntry,
    };

    use super::*;

    const BLOCK_SIZE: u32 = 4096;
    const TAPE_UUID: [u8; 16] = [0x51; 16];

    #[cfg(unix)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestXattrTarget {
        Path,
        File,
    }

    #[cfg(unix)]
    #[derive(Default)]
    struct ScriptedXattrSetter {
        errnos: HashMap<String, i32>,
        calls: Vec<(TestXattrTarget, String)>,
    }

    #[cfg(unix)]
    impl ScriptedXattrSetter {
        fn record(&mut self, target: TestXattrTarget, name: &str) -> io::Result<()> {
            self.calls.push((target, name.to_string()));
            match self.errnos.get(name) {
                Some(errno) => Err(io::Error::from_raw_os_error(*errno)),
                None => Ok(()),
            }
        }
    }

    #[cfg(unix)]
    impl RestoredXattrSetter for ScriptedXattrSetter {
        fn set_path(&mut self, _path: &Path, name: &str, _value: &[u8]) -> io::Result<()> {
            self.record(TestXattrTarget::Path, name)
        }

        fn set_file(&mut self, _file: &File, name: &str, _value: &[u8]) -> io::Result<()> {
            self.record(TestXattrTarget::File, name)
        }
    }

    #[derive(Default)]
    struct TestJournal {
        bundles: Vec<CommittedBundle>,
    }

    impl TapeFileJournal for TestJournal {
        fn tape_uuid(&self) -> [u8; 16] {
            TAPE_UUID
        }

        fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
            self.bundles.push(bundle.clone());
            Ok(())
        }

        fn load_committed(&self) -> Result<CommittedState, JournalError> {
            let retained_end = self
                .bundles
                .iter()
                .rposition(|bundle| bundle.kind == CommittedBundleKind::CheckpointedThrough)
                .map_or(0, |index| index + 1);
            let retained = &self.bundles[..retained_end];
            let last = retained
                .iter()
                .rev()
                .find(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough);
            Ok(CommittedState {
                entries: retained
                    .iter()
                    .filter(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough)
                    .flat_map(|bundle| bundle.entries.iter().cloned())
                    .collect(),
                highest_protected_ordinal: last
                    .map_or(0, |bundle| bundle.highest_protected_ordinal),
                total_committed_ordinals: last.map_or(0, |bundle| bundle.total_committed_ordinals),
                orphaned_bundles: self.bundles[retained_end..].to_vec(),
            })
        }
    }

    #[test]
    fn plan_rejects_nonregular_catalog_projection_entries() {
        let file = PreparedFile {
            source_path: PathBuf::from("/dev/null"),
            spec: RemTarFileSpec::directory("empty/", "00000000-0000-4000-8000-000000000001"),
        };

        let err = plan_prepared_object(&options(), &[file]).unwrap_err();

        assert!(err.to_string().contains("regular files only"), "{err}");
    }

    #[test]
    fn path_and_live_reader_emit_identical_rao_bytes_and_catalog_projection() {
        let source_dir = tempfile::Builder::new()
            .prefix("remanence-stream-equivalence")
            .tempdir()
            .expect("source tempdir");
        let source_path = source_dir.path().join("golden.bin");
        let payload = (0..19_731u32)
            .map(|value| (value.wrapping_mul(31) % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&source_path, &payload).expect("write golden payload");
        let files = vec![
            prepare_regular_file(&source_path, "golden/payload.bin", "golden-file")
                .expect("prepare path source"),
        ];
        let opts = options();
        let plan = plan_prepared_object(&opts, &files).expect("plan object");

        let mut serial_tape = VecBlockSink::new();
        let serial_report = {
            let mut raw = BlockSinkRawTapeSink::new(&mut serial_tape);
            let mut journal = TestJournal::default();
            let mut parity = ParitySink::new_with_journal(
                &mut raw,
                &mut journal,
                scheme(),
                TAPE_UUID,
                BLOCK_SIZE,
            )
            .expect("serial parity");
            parity.write_bootstrap().expect("serial bootstrap");
            write_prepared_object_to_parity(
                &mut parity,
                TAPE_UUID,
                &opts,
                &files,
                capacity_input(plan.layout.projected_size_blocks),
            )
            .expect("serial write")
        };

        let mut overlap_tape = VecBlockSink::new();
        let overlap_report = {
            let mut raw = BlockSinkRawTapeSink::new(&mut overlap_tape);
            let mut journal = TestJournal::default();
            let mut parity = ParitySink::new_with_journal(
                &mut raw,
                &mut journal,
                scheme(),
                TAPE_UUID,
                BLOCK_SIZE,
            )
            .expect("overlap parity");
            parity.write_bootstrap().expect("overlap bootstrap");
            let mut readers: Vec<Box<dyn Read + Send>> =
                vec![Box::new(std::io::Cursor::new(payload))];
            write_prepared_object_to_parity_from_readers(
                &mut parity,
                TAPE_UUID,
                &opts,
                &files,
                &mut readers,
                capacity_input(plan.layout.projected_size_blocks),
            )
            .expect("overlap write")
        };

        assert_eq!(overlap_tape.blocks, serial_tape.blocks);
        assert_eq!(overlap_tape.filemarks, serial_tape.filemarks);
        assert_eq!(overlap_report.catalog, serial_report.catalog);
    }

    #[test]
    fn writes_catalog_projection_and_restores_to_filesystem() {
        let source_dir = tempfile::Builder::new()
            .prefix("remanence-stream-src")
            .tempdir()
            .unwrap();
        let restore_dir = tempfile::Builder::new()
            .prefix("remanence-stream-restore")
            .tempdir()
            .unwrap();
        let first_path = source_dir.path().join("camera.txt");
        let second_path = source_dir.path().join("clip.bin");
        fs::write(&first_path, b"camera payload").unwrap();
        fs::write(&second_path, vec![0xB4; 8193]).unwrap();
        let files = vec![
            prepare_regular_file(&first_path, "camera/a.txt", "file-a").unwrap(),
            prepare_regular_file(&second_path, "video/clip.bin", "file-b").unwrap(),
        ];
        let opts = options();
        let plan = plan_prepared_object(&opts, &files).unwrap();
        let mut tape = VecBlockSink::new();
        let report;
        {
            let mut raw = BlockSinkRawTapeSink::new(&mut tape);
            let mut journal = TestJournal::default();
            let mut parity = ParitySink::new_with_journal(
                &mut raw,
                &mut journal,
                scheme(),
                TAPE_UUID,
                BLOCK_SIZE,
            )
            .unwrap();
            assert_eq!(parity.write_bootstrap().unwrap(), 0);
            report = write_prepared_object_to_parity(
                &mut parity,
                TAPE_UUID,
                &opts,
                &files,
                capacity_input(plan.layout.projected_size_blocks),
            )
            .unwrap();
        }

        assert_eq!(report.catalog.object.object_id, opts.object_id);
        assert_eq!(report.catalog.files.len(), 2);
        assert_eq!(report.catalog.object_copy.tape_file_number, 1);
        let object_entry = report
            .catalog
            .tape_file_bundle
            .entries
            .iter()
            .find(|entry| entry.kind == TapeFileKind::Object)
            .unwrap();
        assert_eq!(
            object_entry.object_id.as_deref(),
            Some(opts.object_id.as_str())
        );
        let bootstrap_object_row = object_entry
            .bootstrap_object_row
            .as_ref()
            .expect("object entry carries bootstrap row");
        assert_eq!(
            bootstrap_object_row.tape_file_number,
            report.object_close.tape_file_number
        );
        assert_eq!(
            bootstrap_object_row.stored_block_count,
            report.layout.projected_size_blocks
        );
        assert_eq!(
            bootstrap_object_row.object_id.as_deref(),
            Some(opts.object_id.as_bytes()),
            "bootstrap binding carries the RAO object_id string bytes verbatim"
        );
        match &bootstrap_object_row.representation {
            BootstrapObjectRepresentation::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            } => {
                assert_eq!(
                    Some(BodyLba(*manifest_first_chunk_lba)),
                    report.layout.manifest.first_chunk_lba
                );
                assert_eq!(*manifest_size_bytes, report.layout.manifest.size_bytes);
                assert_eq!(*manifest_chunk_count, report.layout.manifest.chunk_count);
                assert_eq!(*manifest_sha256, report.layout.manifest_sha256);
            }
            BootstrapObjectRepresentation::Encrypted { .. } => {
                panic!("plaintext streaming write emitted encrypted bootstrap row")
            }
        }
        assert_eq!(report.audit_events[0].kind, "streaming_object_committed");

        let scoped = scoped_map_from_close(&report.object_close);
        let mut physical = PhysicalVecTapeSource::from_sink(&tape);
        let mut object_source = ObjectParitySource::open(
            &mut physical,
            scheme(),
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            report.object_close.tape_file_number,
            OpenTrust::RequireValidated,
        )
        .unwrap();
        let restore = restore_object_to_directory(
            &mut object_source,
            opts.chunk_size,
            report.layout.projected_size_blocks,
            restore_dir.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(restore.files_written, 2);
        assert_eq!(
            fs::read(restore_dir.path().join("camera/a.txt")).unwrap(),
            b"camera payload"
        );
        assert_eq!(
            fs::read(restore_dir.path().join("video/clip.bin")).unwrap(),
            vec![0xB4; 8193]
        );
        assert!(!restore_dir.path().join(MANIFEST_PATH).exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_object_to_directory_preserves_symlink_and_empty_directory() {
        let mut opts = options();
        opts.chunk_size = 4096;
        let specs = vec![
            RemTarFileSpec::new("target.txt", "file-target", 6, sha256_array(b"target")),
            RemTarFileSpec::hardlink("links/copy.txt", "hardlink-copy", "target.txt"),
            RemTarFileSpec::directory("empty/", "dir-empty"),
            RemTarFileSpec::symlink("links/latest", "link-latest", "../target.txt"),
        ];
        let mut readers: Vec<Box<dyn Read>> = vec![
            Box::new(&b"target"[..]),
            Box::new(io::empty()),
            Box::new(io::empty()),
            Box::new(io::empty()),
        ];
        let mut streams: Vec<RemTarFileStream<'_>> = specs
            .into_iter()
            .zip(readers.iter_mut())
            .map(|(spec, reader)| RemTarFileStream::new(spec, reader.as_mut()))
            .collect();
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap();
        let mut source = VecBlockSource::new(sink.blocks);
        let restore_dir = tempfile::Builder::new()
            .prefix("remanence-stream-restore-nonregular")
            .tempdir()
            .unwrap();

        let restore = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            restore_dir.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(restore.files_written, 1);
        assert_eq!(restore.directories_seen, 1);
        assert_eq!(restore.symlinks_written, 1);
        assert_eq!(restore.hardlinks_written, 1);
        assert_eq!(restore.bytes_written, 6);
        let target_path = restore_dir.path().join("target.txt");
        let hardlink_path = restore_dir.path().join("links/copy.txt");
        assert_eq!(fs::read(&target_path).unwrap(), b"target");
        assert_eq!(fs::read(&hardlink_path).unwrap(), b"target");
        let target_metadata = fs::metadata(&target_path).unwrap();
        let hardlink_metadata = fs::metadata(&hardlink_path).unwrap();
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&target_metadata),
            std::os::unix::fs::MetadataExt::ino(&hardlink_metadata)
        );
        assert!(restore_dir.path().join("empty").is_dir());
        let link_path = restore_dir.path().join("links/latest");
        assert!(fs::symlink_metadata(&link_path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(link_path).unwrap(),
            PathBuf::from("../target.txt")
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_object_to_directory_reapplies_xattrs() {
        let name = "user.remanence.test";
        let restore_dir = tempfile::Builder::new()
            .prefix("remanence-stream-restore-xattr")
            .tempdir()
            .unwrap();
        let probe = restore_dir.path().join("probe");
        fs::write(&probe, b"").unwrap();
        xattr::set(&probe, name, b"probe")
            .expect("the restore xattr test requires user.* xattr support");
        fs::remove_file(&probe).unwrap();

        let mut opts = options();
        opts.chunk_size = 4096;
        let payload = b"xattr payload".to_vec();
        let mut spec = RemTarFileSpec::new(
            "tagged.txt",
            "file-tagged",
            payload.len() as u64,
            sha256_array(&payload),
        );
        spec.xattrs.insert(name.to_string(), b"tagged".to_vec());
        spec.extensions.insert(
            "org.example.restore".to_string(),
            remanence_format::RemTarCborValue::Bytes(b"carry-only".to_vec()),
        );
        let mut reader = io::Cursor::new(payload);
        let mut streams = [RemTarFileStream::new(spec, &mut reader)];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object_from_readers(&mut sink, &opts, &mut streams).unwrap();
        let mut source = VecBlockSource::new(sink.blocks);

        let restore = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            restore_dir.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(restore.files_written, 1);
        assert!(restore.skipped_xattrs.is_empty());
        assert!(restore.applied_privileged_xattrs.is_empty());
        assert_eq!(
            restore.stream.entries[0].extensions["org.example.restore"],
            remanence_format::RemTarCborValue::Bytes(b"carry-only".to_vec())
        );
        assert_eq!(
            xattr::get(restore_dir.path().join("tagged.txt"), name).unwrap(),
            Some(b"tagged".to_vec())
        );
    }

    #[test]
    fn apply_restored_xattrs_validates_every_allowed_prefix() {
        for invalid in ["security", "s", ".", "", "user..", "user.\u{1}"] {
            let error = apply_restored_xattrs(Path::new("unused"), &[], &[invalid.to_string()])
                .expect_err("invalid prefix must be rejected before entries are inspected");
            assert!(
                matches!(
                    error,
                    StreamingError::InvalidXattrNamespacePrefix { prefix }
                        if prefix == invalid
                ),
                "unexpected error for {invalid:?}"
            );
        }

        for valid in ["user.", "security."] {
            let accounting =
                apply_restored_xattrs(Path::new("unused"), &[], &[valid.to_string()]).unwrap();
            assert_eq!(accounting, XattrRestoreAccounting::default());
        }
    }

    #[cfg(unix)]
    #[test]
    fn applied_privileged_xattrs_are_accounted_without_values() {
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-applied-privileged-xattrs")
            .tempdir()
            .unwrap();
        fs::write(root.path().join("tagged.txt"), b"payload").unwrap();
        let mut entry = stream_entry("tagged.txt", RemTarEntryType::Regular);
        entry
            .xattrs
            .insert("security.ima".to_string(), b"security-secret".to_vec());
        entry
            .xattrs
            .insert("user.x".to_string(), b"user-secret".to_vec());
        let mut setter = ScriptedXattrSetter::default();

        let accounting = apply_restored_xattrs_with_setter(
            root.path(),
            &[entry],
            &["user.".to_string(), "security.".to_string()],
            &mut setter,
        )
        .unwrap();

        assert!(accounting.skipped.is_empty());
        assert_eq!(
            accounting.applied_privileged.get("tagged.txt").unwrap(),
            &["security.ima"]
        );
        assert_eq!(
            setter.calls,
            [
                (TestXattrTarget::File, "security.ima".to_string()),
                (TestXattrTarget::File, "user.x".to_string()),
            ]
        );
        let report_text = format!("{accounting:?}");
        assert!(!report_text.contains("security-secret"));
        assert!(!report_text.contains("user-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_xattrs_skip_but_permission_denied_fails_for_all_target_types() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("remanence-stream-xattr-errno")
            .tempdir()
            .unwrap();
        fs::write(root.path().join("regular.txt"), b"payload").unwrap();
        symlink("missing-target", root.path().join("link")).unwrap();

        for (path, entry_type, target) in [
            (
                "regular.txt",
                RemTarEntryType::Regular,
                TestXattrTarget::File,
            ),
            ("link", RemTarEntryType::Symlink, TestXattrTarget::Path),
        ] {
            let mut entry = stream_entry(path, entry_type);
            entry
                .xattrs
                .insert("security.ima".to_string(), b"value".to_vec());
            let prefixes = ["security.".to_string()];

            let mut unsupported = ScriptedXattrSetter::default();
            unsupported
                .errnos
                .insert("security.ima".to_string(), nix::libc::EOPNOTSUPP);
            let accounting = apply_restored_xattrs_with_setter(
                root.path(),
                std::slice::from_ref(&entry),
                &prefixes,
                &mut unsupported,
            )
            .unwrap();
            assert_eq!(accounting.skipped[path], ["security.ima"]);
            assert!(accounting.applied_privileged.is_empty());
            assert_eq!(unsupported.calls, [(target, "security.ima".to_string())]);

            let mut denied = ScriptedXattrSetter::default();
            denied
                .errnos
                .insert("security.ima".to_string(), nix::libc::EPERM);
            let error =
                apply_restored_xattrs_with_setter(root.path(), &[entry], &prefixes, &mut denied)
                    .expect_err("EPERM must surface instead of being classified as unsupported");
            assert!(
                matches!(
                    error,
                    StreamingError::Io { source, .. }
                        if source.raw_os_error() == Some(nix::libc::EPERM)
                ),
                "unexpected error for {path}"
            );
            assert_eq!(denied.calls, [(target, "security.ima".to_string())]);
        }

        assert!(xattrs_unsupported(&io::Error::from_raw_os_error(
            nix::libc::EOPNOTSUPP
        )));
        assert!(xattrs_unsupported(&io::Error::from_raw_os_error(
            nix::libc::ENOTSUP
        )));
        assert!(!xattrs_unsupported(&io::Error::from_raw_os_error(
            nix::libc::EPERM
        )));
    }

    #[cfg(unix)]
    #[test]
    fn malformed_xattr_names_never_reach_the_setter() {
        let malformed = "user.bad\0name";
        let mut entry = stream_entry("nul.txt", RemTarEntryType::Regular);
        entry
            .xattrs
            .insert(malformed.to_string(), b"must-not-be-set".to_vec());
        let mut setter = ScriptedXattrSetter::default();

        let accounting = apply_restored_xattrs_with_setter(
            Path::new("unused"),
            &[entry],
            &["user.".to_string()],
            &mut setter,
        )
        .unwrap();

        assert_eq!(accounting.skipped["nul.txt"], [malformed]);
        assert!(accounting.applied_privileged.is_empty());
        assert!(setter.calls.is_empty());
        assert!(!restored_xattr_name_is_valid(""));
        assert!(!restored_xattr_name_is_valid("user."));
        assert!(!restored_xattr_name_is_valid("no-namespace"));
        #[cfg(any(target_os = "android", target_os = "linux"))]
        assert!(!restored_xattr_name_is_valid(&format!(
            "user.{}",
            "x".repeat(251)
        )));
    }

    #[cfg(unix)]
    #[test]
    fn restore_skips_privileged_namespaces_including_security_capability() {
        let restore_dir = tempfile::Builder::new()
            .prefix("remanence-stream-restore-xattr-policy")
            .tempdir()
            .unwrap();
        let mut opts = options();
        opts.chunk_size = 4096;
        let payload = b"xattr policy payload".to_vec();
        let mut spec = RemTarFileSpec::new(
            "tagged.txt",
            "file-tagged",
            payload.len() as u64,
            sha256_array(&payload),
        );
        for (name, value) in [
            ("security.capability", b"capability".as_slice()),
            ("security.ima", b"ima".as_slice()),
            ("system.posix_acl_access", b"acl".as_slice()),
            ("trusted.evil", b"opaque-sensitive-value".as_slice()),
            ("user.test", b"kept".as_slice()),
        ] {
            spec.xattrs.insert(name.to_string(), value.to_vec());
        }
        let (blocks, layout) = rao_blocks_with_payloads(&opts, vec![(spec, payload)]);
        let mut source = VecBlockSource::new(blocks);

        let restore = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            restore_dir.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(
            restore.skipped_xattrs.get("tagged.txt").unwrap(),
            &[
                "security.capability",
                "security.ima",
                "system.posix_acl_access",
                "trusted.evil",
            ]
        );
        let restored = restore_dir.path().join("tagged.txt");
        assert_eq!(
            xattr::get(&restored, "user.test").unwrap(),
            Some(b"kept".to_vec())
        );
        let archived_names = [
            "security.capability",
            "security.ima",
            "system.posix_acl_access",
            "trusted.evil",
            "user.test",
        ];
        let restored_archived_names = xattr::list(&restored)
            .unwrap()
            .filter_map(|name| name.into_string().ok())
            .filter(|name| archived_names.contains(&name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(restored_archived_names, ["user.test"]);
    }

    #[cfg(unix)]
    #[test]
    fn restore_security_namespace_opt_in_reaches_the_real_xattr_setter() {
        let restore_dir = tempfile::Builder::new()
            .prefix("remanence-stream-restore-security-xattr")
            .tempdir()
            .unwrap();
        let security_name = "security.remanence_test";
        let probe = restore_dir.path().join("probe");
        fs::write(&probe, b"").unwrap();
        let platform_can_set_security_xattrs = xattr::set(&probe, security_name, b"probe").is_ok();
        fs::remove_file(&probe).unwrap();

        let mut opts = options();
        opts.chunk_size = 4096;
        let payload = b"security opt-in payload".to_vec();
        let mut spec = RemTarFileSpec::new(
            "security.txt",
            "file-security",
            payload.len() as u64,
            sha256_array(&payload),
        );
        spec.xattrs
            .insert(security_name.to_string(), b"operator-approved".to_vec());
        let (blocks, layout) = rao_blocks_with_payloads(&opts, vec![(spec, payload)]);
        let mut source = VecBlockSource::new(blocks);
        let restore_options = FilesystemRestoreOptions {
            xattr_allowed_prefixes: vec!["user.".to_string(), "security.".to_string()],
            ..FilesystemRestoreOptions::default()
        };
        let result = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            restore_dir.path(),
            restore_options,
        );

        if platform_can_set_security_xattrs {
            let restore = result.expect("security.* probe succeeded, so restore must apply it");
            assert!(restore.skipped_xattrs.is_empty());
            assert_eq!(
                restore.applied_privileged_xattrs["security.txt"],
                [security_name]
            );
            assert_eq!(
                xattr::get(restore_dir.path().join("security.txt"), security_name).unwrap(),
                Some(b"operator-approved".to_vec())
            );
        } else {
            let error = result.expect_err(
                "without security.* privilege, opt-in must attempt the write and surface failure",
            );
            assert!(error.to_string().contains("set restore xattr"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn restore_xattrs_do_not_follow_a_late_destination_symlink_swap() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("remanence-stream-xattr-symlink-root")
            .tempdir()
            .unwrap();
        let outside = tempfile::Builder::new()
            .prefix("remanence-stream-xattr-symlink-outside")
            .tempdir()
            .unwrap();
        let outside_target = outside.path().join("target.txt");
        fs::write(&outside_target, b"outside").unwrap();
        symlink(&outside_target, root.path().join("victim.txt")).unwrap();
        let mut entry = stream_entry("victim.txt", RemTarEntryType::Regular);
        entry
            .xattrs
            .insert("user.no_follow".to_string(), b"must-not-land".to_vec());

        // Public restore applies xattrs immediately after materialization, so the
        // reachable late-swap shape is exercised directly at the single funnel.
        let accounting =
            apply_restored_xattrs(root.path(), &[entry], &["user.".to_string()]).unwrap();

        assert_eq!(accounting.skipped["victim.txt"], ["user.no_follow"]);
        assert!(accounting.applied_privileged.is_empty());
        assert_eq!(xattr::get(&outside_target, "user.no_follow").unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn restore_symlink_entry_xattrs_never_reach_its_outside_target() {
        let restore_dir = tempfile::Builder::new()
            .prefix("remanence-stream-restore-symlink-xattr")
            .tempdir()
            .unwrap();
        let outside = tempfile::Builder::new()
            .prefix("remanence-stream-restore-symlink-target")
            .tempdir()
            .unwrap();
        let outside_target = outside.path().join("target.txt");
        fs::write(&outside_target, b"outside").unwrap();
        let target = outside_target.to_str().unwrap().to_string();
        let mut spec = RemTarFileSpec::symlink("outside-link", "link-outside", target);
        spec.xattrs
            .insert("user.symlink_test".to_string(), b"link-only".to_vec());
        let mut opts = options();
        opts.chunk_size = 4096;
        let (blocks, layout) = rao_blocks_with_payloads(&opts, vec![(spec, Vec::new())]);
        let mut source = VecBlockSource::new(blocks);

        let result = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            restore_dir.path(),
            FilesystemRestoreOptions::default(),
        );

        assert_eq!(
            xattr::get(&outside_target, "user.symlink_test").unwrap(),
            None
        );
        let link = restore_dir.path().join("outside-link");
        match result {
            Ok(restore) => {
                assert!(restore.applied_privileged_xattrs.is_empty());
                match restore.skipped_xattrs.get("outside-link") {
                    Some(names) => assert_eq!(names, &["user.symlink_test"]),
                    None => assert_eq!(
                        xattr::get(&link, "user.symlink_test").unwrap(),
                        Some(b"link-only".to_vec())
                    ),
                }
            }
            Err(StreamingError::Io {
                context, source, ..
            }) if source.raw_os_error() == Some(nix::libc::EPERM) => {
                assert_eq!(context, "set restore symlink xattr");
            }
            Err(error) => panic!("unexpected symlink xattr restore failure: {error}"),
        }
    }

    #[test]
    fn native_restore_inline_rejects_case_fold_collisions_before_second_materialization() {
        let mut opts = options();
        opts.chunk_size = 4096;
        let first = b"first spelling".to_vec();
        let second = b"second spelling".to_vec();
        let (blocks, layout) = rao_blocks_with_payloads(
            &opts,
            vec![
                (
                    RemTarFileSpec::new(
                        "Photos/Frame.txt",
                        "case-first",
                        first.len() as u64,
                        sha256_array(&first),
                    ),
                    first,
                ),
                (
                    RemTarFileSpec::new(
                        "photos/frame.TXT",
                        "case-second",
                        second.len() as u64,
                        sha256_array(&second),
                    ),
                    second,
                ),
            ],
        );
        let mut source = VecBlockSource::new(blocks);
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-native-collision")
            .tempdir()
            .unwrap();

        let error = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            root.path(),
            FilesystemRestoreOptions {
                overwrite: true,
                ..FilesystemRestoreOptions::default()
            },
        )
        .expect_err("case-fold collision must fail inline restore validation");

        assert!(
            matches!(
                &error,
                StreamingError::Format(FormatError::InvalidPath(message))
                    if message.contains("collide after native case folding")
            ),
            "{error}"
        );
        assert_eq!(
            fs::read(root.path().join("Photos/Frame.txt")).unwrap(),
            b"first spelling",
            "the second colliding entry must not silently overwrite the first"
        );
    }

    #[test]
    fn native_restore_inline_rejects_unicode_normalization_collisions() {
        let mut opts = options();
        opts.chunk_size = 4096;
        let first = b"NFC spelling".to_vec();
        let second = b"NFD spelling".to_vec();
        let (blocks, layout) = rao_blocks_with_payloads(
            &opts,
            vec![
                (
                    RemTarFileSpec::new(
                        "Caf\u{e9}.txt",
                        "nfc-first",
                        first.len() as u64,
                        sha256_array(&first),
                    ),
                    first,
                ),
                (
                    RemTarFileSpec::new(
                        "Cafe\u{301}.txt",
                        "nfd-second",
                        second.len() as u64,
                        sha256_array(&second),
                    ),
                    second,
                ),
            ],
        );
        let mut source = VecBlockSource::new(blocks);
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-native-unicode-collision")
            .tempdir()
            .unwrap();

        let error = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            root.path(),
            FilesystemRestoreOptions::default(),
        )
        .expect_err("NFC/NFD collision must fail inline restore validation");

        assert!(
            matches!(
                &error,
                StreamingError::Format(FormatError::InvalidPath(message))
                    if message.contains("collide")
            ),
            "{error}"
        );
        assert_eq!(
            fs::read(root.path().join("Caf\u{e9}.txt")).unwrap(),
            b"NFC spelling"
        );
    }

    #[test]
    fn native_restore_entrypoint_rejects_absolute_escaping_and_empty_component_paths() {
        for (valid_path, invalid_path) in [
            ("xabsolute.txt", "/absolute.txt"),
            ("xx/outside.txt", "../outside.txt"),
            ("safe/xx/outside.txt", "safe/../outside.txt"),
            ("safe/xoutside.txt", "safe//outside.txt"),
        ] {
            let mut opts = options();
            opts.chunk_size = 4096;
            let (blocks, layout) = rao_blocks_with_payloads(
                &opts,
                vec![(
                    RemTarFileSpec::new(valid_path, "invalid-path", 0, sha256_array(b"")),
                    Vec::new(),
                )],
            );
            let blocks =
                replace_first_rao_pax_path(blocks, opts.chunk_size, valid_path, invalid_path);
            let mut source = VecBlockSource::new(blocks);
            let root = tempfile::Builder::new()
                .prefix("remanence-stream-native-invalid-path")
                .tempdir()
                .unwrap();

            let error = restore_object_to_directory(
                &mut source,
                opts.chunk_size,
                layout.projected_size_blocks,
                root.path(),
                FilesystemRestoreOptions::default(),
            )
            .expect_err("invalid native path must fail the restore entrypoint");

            assert!(
                matches!(&error, StreamingError::Format(FormatError::InvalidPath(_))),
                "{invalid_path}: {error}"
            );
            assert!(
                root.path().read_dir().unwrap().next().is_none(),
                "{invalid_path} must fail before its entry is materialized"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn native_restore_entrypoint_rejects_windows_native_path_forms() {
        for invalid_path in [
            r"C:\absolute.txt",
            r"C:drive-relative.txt",
            r"\\host\share\outside.txt",
            r"..\outside.txt",
            r"safe\\outside.txt",
        ] {
            let mut opts = options();
            opts.chunk_size = 4096;
            let (blocks, layout) = rao_blocks_with_payloads(
                &opts,
                vec![(
                    RemTarFileSpec::new(invalid_path, "windows-invalid-path", 0, sha256_array(b"")),
                    Vec::new(),
                )],
            );
            let mut source = VecBlockSource::new(blocks);
            let root = tempfile::Builder::new()
                .prefix("remanence-stream-windows-native-invalid-path")
                .tempdir()
                .unwrap();

            let error = restore_object_to_directory(
                &mut source,
                opts.chunk_size,
                layout.projected_size_blocks,
                root.path(),
                FilesystemRestoreOptions::default(),
            )
            .expect_err("invalid Windows-native path must fail the restore entrypoint");

            assert!(
                matches!(&error, StreamingError::Format(FormatError::InvalidPath(_))),
                "{invalid_path}: {error}"
            );
            assert!(
                root.path().read_dir().unwrap().next().is_none(),
                "{invalid_path} must fail before its entry is materialized"
            );
        }
    }

    #[test]
    fn native_restore_clean_object_is_byte_exact_and_read_once() {
        let mut opts = options();
        opts.chunk_size = 4096;
        let payload = (0..12_345u32)
            .map(|value| (value.wrapping_mul(17) % 251) as u8)
            .collect::<Vec<_>>();
        let (blocks, layout) = rao_blocks_with_payloads(
            &opts,
            vec![(
                RemTarFileSpec::new(
                    "clean/golden.bin",
                    "clean-golden",
                    payload.len() as u64,
                    sha256_array(&payload),
                ),
                payload.clone(),
            )],
        );
        let mut source = VecBlockSource::new(blocks);
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-native-clean")
            .tempdir()
            .unwrap();

        let report = restore_object_to_directory(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            root.path(),
            FilesystemRestoreOptions::default(),
        )
        .expect("clean restore");

        assert_eq!(report.files_written, 1);
        assert_eq!(report.bytes_written, payload.len() as u64);
        assert_eq!(
            fs::read(root.path().join("clean/golden.bin")).unwrap(),
            payload
        );
        let read_lbas = source
            .calls
            .iter()
            .filter_map(|call| match call {
                VecBlockSourceCall::ReadBlock { lba, .. } => Some(*lba),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            read_lbas,
            (0..layout.projected_size_blocks).collect::<Vec<_>>(),
            "each object block must be read exactly once"
        );
        assert!(
            !source
                .calls
                .iter()
                .any(|call| matches!(call, VecBlockSourceCall::Locate { .. })),
            "single-pass restore must not reposition the source"
        );
    }

    #[test]
    fn restore_sink_rejects_paths_that_escape_destination() {
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-escape")
            .tempdir()
            .unwrap();
        let mut sink =
            FilesystemRestoreSink::new(root.path(), FilesystemRestoreOptions::default()).unwrap();
        for path in ["../escape.txt", "/absolute.txt", "safe//empty.txt"] {
            let entry = RemTarStreamEntry {
                entry_type: RemTarEntryType::Regular,
                path: path.to_string(),
                size_bytes: 0,
                link_target: None,
                first_chunk_lba: None,
                chunk_count: 0,
                data_offset: 0,
                pax_records: Default::default(),
                xattrs: Default::default(),
                extensions: Default::default(),
            };

            let err = sink.begin_file(&entry).unwrap_err();

            assert!(matches!(&err, FormatError::InvalidPath(_)), "{path}: {err}");
        }
    }

    #[test]
    fn restore_rejects_missing_file_sha256_metadata() {
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-missing-sha")
            .tempdir()
            .unwrap();
        let mut sink =
            FilesystemRestoreSink::new(root.path(), FilesystemRestoreOptions::default()).unwrap();
        let entry = RemTarStreamEntry {
            entry_type: RemTarEntryType::Regular,
            path: "payload.bin".to_string(),
            size_bytes: 0,
            link_target: None,
            first_chunk_lba: None,
            chunk_count: 0,
            data_offset: 0,
            pax_records: BTreeMap::new(),
            xattrs: Default::default(),
            extensions: Default::default(),
        };

        let err = sink.begin_file(&entry).unwrap_err();

        assert!(err.to_string().contains("REMANENCE.file_sha256"), "{err}");
        assert!(!root.path().join("payload.bin").exists());
    }

    #[test]
    fn restore_rejects_file_sha256_mismatch() {
        let root = tempfile::Builder::new()
            .prefix("remanence-stream-sha-mismatch")
            .tempdir()
            .unwrap();
        let mut sink =
            FilesystemRestoreSink::new(root.path(), FilesystemRestoreOptions::default()).unwrap();
        let mut pax_records = BTreeMap::new();
        pax_records.insert("REMANENCE.file_sha256".to_string(), "00".repeat(32));
        let entry = RemTarStreamEntry {
            entry_type: RemTarEntryType::Regular,
            path: "payload.bin".to_string(),
            size_bytes: 7,
            link_target: None,
            first_chunk_lba: Some(BodyLba(0)),
            chunk_count: 1,
            data_offset: 0,
            pax_records,
            xattrs: Default::default(),
            extensions: Default::default(),
        };

        sink.begin_file(&entry).unwrap();
        sink.write_file_data(b"payload").unwrap();
        let err = sink.end_file(&entry).unwrap_err();

        assert!(
            matches!(err, FormatError::FileDigestMismatch { .. }),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restore_rejects_symlink_parent_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("remanence-stream-symlink-root")
            .tempdir()
            .unwrap();
        let outside = tempfile::Builder::new()
            .prefix("remanence-stream-symlink-outside")
            .tempdir()
            .unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let mut sink = FilesystemRestoreSink::new(
            root.path(),
            FilesystemRestoreOptions {
                overwrite: true,
                include_manifest: true,
                ..FilesystemRestoreOptions::default()
            },
        )
        .unwrap();
        let mut pax_records = BTreeMap::new();
        pax_records.insert(
            "REMANENCE.file_sha256".to_string(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string(),
        );
        let entry = RemTarStreamEntry {
            entry_type: RemTarEntryType::Regular,
            path: "escape/file.txt".to_string(),
            size_bytes: 0,
            link_target: None,
            first_chunk_lba: None,
            chunk_count: 0,
            data_offset: 0,
            pax_records,
            xattrs: Default::default(),
            extensions: Default::default(),
        };

        let err = sink.begin_file(&entry).unwrap_err();

        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!outside.path().join("file.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn restore_archive_reader_rejects_symlink_parent_escape() {
        use std::os::unix::fs::symlink;

        let root = tempfile::Builder::new()
            .prefix("remanence-archive-symlink-root")
            .tempdir()
            .unwrap();
        let outside = tempfile::Builder::new()
            .prefix("remanence-archive-symlink-outside")
            .tempdir()
            .unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let entry = normalized_entry("file-a", "escape/file.txt", EntryKind::RegularFile, Some(0));
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![],
            data: vec![],
        };

        let err = restore_archive_reader_to_directory(
            &mut reader,
            root.path(),
            FilesystemRestoreOptions {
                overwrite: true,
                include_manifest: true,
                ..FilesystemRestoreOptions::default()
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!outside.path().join("file.txt").exists());
    }

    #[test]
    fn scan_archive_reader_collects_normalized_entries_and_damage() {
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(5));
        let damage = damage_range("file-a", 2, 5, DamageStatus::Missing);
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry.clone()],
            damages: vec![damage.clone()],
            data: vec![],
        };

        let report = scan_archive_reader(&mut reader).unwrap();

        assert_eq!(report.scan.entries, 1);
        assert_eq!(report.scan.damage_events, 1);
        assert_eq!(report.entries, vec![entry]);
        assert_eq!(report.damages, vec![damage]);
    }

    #[test]
    fn restore_archive_reader_writes_normalized_entries() {
        let root = tempfile::Builder::new()
            .prefix("remanence-archive-restore")
            .tempdir()
            .unwrap();
        let entries = vec![
            normalized_entry("dir", "camera", EntryKind::Directory, Some(0)),
            normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(7)),
        ];
        let mut reader = ScriptedArchiveReader {
            entries,
            damages: vec![],
            data: vec![("file-a".into(), 0, b"payload".to_vec())],
        };

        let report = restore_archive_reader_to_directory(
            &mut reader,
            root.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(report.stream.entries, 2);
        assert_eq!(report.files_written, 1);
        assert_eq!(report.directories_seen, 1);
        assert_eq!(report.bytes_written, 7);
        assert_eq!(
            fs::read(root.path().join("camera/a.txt")).unwrap(),
            b"payload"
        );
        assert!(report.skipped_xattrs.is_empty());
        assert!(report.applied_privileged_xattrs.is_empty());
    }

    #[test]
    fn normalized_entry_exposes_no_attribute_data() {
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(1));

        let NormalizedEntry {
            file_id: _,
            path: _,
            kind: _,
            link_target: _,
            size_bytes: _,
            adapter_state: _,
        } = entry;
    }

    #[cfg(unix)]
    #[test]
    fn normalized_and_deep_recovery_entries_carry_no_xattrs_to_apply() {
        let mut entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(1));
        entry.adapter_state = b"user.remanence_normalized_probe=must-not-apply".to_vec();

        let normalized_root = tempfile::Builder::new()
            .prefix("remanence-normalized-no-xattrs")
            .tempdir()
            .unwrap();
        let mut normalized_reader = ScriptedArchiveReader {
            entries: vec![entry.clone()],
            damages: vec![],
            data: vec![("file-a".into(), 0, b"x".to_vec())],
        };
        restore_archive_reader_to_directory(
            &mut normalized_reader,
            normalized_root.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        let recovery_root = tempfile::Builder::new()
            .prefix("remanence-deep-recovery-no-xattrs")
            .tempdir()
            .unwrap();
        let mut recovery_reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![],
            data: vec![("file-a".into(), 0, b"x".to_vec())],
        };
        recover_archive_reader_to_directory(
            &mut recovery_reader,
            recovery_root.path(),
            RecoveryOptions::new("test-format", "dump:test"),
        )
        .unwrap();

        let xattr_name = "user.remanence_normalized_probe";
        assert_eq!(
            xattr::get(normalized_root.path().join("camera/a.txt"), xattr_name).unwrap(),
            None
        );
        assert_eq!(
            xattr::get(recovery_root.path().join("camera/a.txt"), xattr_name).unwrap(),
            None
        );
    }

    #[test]
    fn xattr_skip_warning_is_single_and_counted() {
        assert_eq!(xattr_policy_skip_warning(&BTreeMap::new()), None);
        let skipped = BTreeMap::from([
            ("one".to_string(), vec!["system.one".to_string()]),
            (
                "two".to_string(),
                vec!["security.two".to_string(), "trusted.three".to_string()],
            ),
        ]);
        assert_eq!(skipped_xattr_count(&skipped), 3);
        assert_eq!(
            xattr_policy_skip_warning(&skipped).as_deref(),
            Some(
                "3 extended attribute(s) skipped by namespace policy; see skipped_xattrs in the report"
            )
        );
    }

    #[test]
    fn restore_archive_reader_allows_missing_damage_partial_file() {
        let root = tempfile::Builder::new()
            .prefix("remanence-archive-restore-missing")
            .tempdir()
            .unwrap();
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(5));
        let damage = damage_range("file-a", 2, 5, DamageStatus::Missing);
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![damage.clone()],
            data: vec![("file-a".into(), 0, b"ab".to_vec())],
        };

        let report = restore_archive_reader_to_directory(
            &mut reader,
            root.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(report.stream.damage_events, 1);
        assert_eq!(report.damages, vec![damage]);
        assert_eq!(report.bytes_written, 2);
        assert_eq!(fs::read(root.path().join("camera/a.txt")).unwrap(), b"ab");
    }

    #[test]
    fn restore_archive_reader_writes_sparse_offsets() {
        let root = tempfile::Builder::new()
            .prefix("remanence-archive-restore-sparse")
            .tempdir()
            .unwrap();
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(5));
        let damage = damage_range("file-a", 2, 4, DamageStatus::Missing);
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![damage.clone()],
            data: vec![
                ("file-a".into(), 0, b"ab".to_vec()),
                ("file-a".into(), 4, b"e".to_vec()),
            ],
        };

        let report = restore_archive_reader_to_directory(
            &mut reader,
            root.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap();

        assert_eq!(report.damages, vec![damage]);
        assert_eq!(report.bytes_written, 3);
        assert_eq!(
            fs::read(root.path().join("camera/a.txt")).unwrap(),
            b"ab\0\0e"
        );
    }

    #[test]
    fn restore_archive_reader_rejects_short_file_without_tail_damage() {
        let root = tempfile::Builder::new()
            .prefix("remanence-archive-restore-short")
            .tempdir()
            .unwrap();
        let entry = normalized_entry("file-a", "camera/a.txt", EntryKind::RegularFile, Some(5));
        let damage = damage_range("file-a", 0, 1, DamageStatus::Missing);
        let mut reader = ScriptedArchiveReader {
            entries: vec![entry],
            damages: vec![damage],
            data: vec![("file-a".into(), 0, b"ab".to_vec())],
        };

        let err = restore_archive_reader_to_directory(
            &mut reader,
            root.path(),
            FilesystemRestoreOptions::default(),
        )
        .unwrap_err();

        assert!(err.to_string().contains("did not cover declared size 5"));
    }

    fn stream_entry(path: &str, entry_type: RemTarEntryType) -> RemTarStreamEntry {
        RemTarStreamEntry {
            entry_type,
            path: path.to_string(),
            size_bytes: 0,
            link_target: None,
            first_chunk_lba: None,
            chunk_count: 0,
            data_offset: 0,
            pax_records: BTreeMap::new(),
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
        }
    }

    fn rao_blocks_with_payloads(
        options: &RemTarObjectOptions,
        files: Vec<(RemTarFileSpec, Vec<u8>)>,
    ) -> (Vec<Vec<u8>>, RemTarObjectLayout) {
        let (specs, payloads): (Vec<_>, Vec<_>) = files.into_iter().unzip();
        let mut readers = payloads
            .iter()
            .map(|payload| io::Cursor::new(payload.as_slice()))
            .collect::<Vec<_>>();
        let mut streams = specs
            .into_iter()
            .zip(readers.iter_mut())
            .map(|(spec, reader)| RemTarFileStream::new(spec, reader as &mut dyn Read))
            .collect::<Vec<_>>();
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object_from_readers(&mut sink, options, &mut streams).unwrap();
        (sink.blocks, layout)
    }

    /// Replace one same-length pax path so invalid reader inputs use the public restore funnel.
    fn replace_first_rao_pax_path(
        blocks: Vec<Vec<u8>>,
        chunk_size: usize,
        original: &str,
        replacement: &str,
    ) -> Vec<Vec<u8>> {
        assert_eq!(
            original.len(),
            replacement.len(),
            "same-length replacement preserves the pax record footprint"
        );
        assert!(
            blocks.iter().all(|block| block.len() == chunk_size),
            "RAO fixture blocks must all match chunk_size"
        );
        let mut bytes = blocks.into_iter().flatten().collect::<Vec<_>>();
        let needle = format!("path={original}\n");
        let replacement = format!("path={replacement}\n");
        let offset = bytes
            .windows(needle.len())
            .position(|window| window == needle.as_bytes())
            .expect("fixture pax path record must be present");
        bytes[offset..offset + replacement.len()].copy_from_slice(replacement.as_bytes());
        assert_eq!(
            bytes.len() % chunk_size,
            0,
            "same-length replacement must preserve object block alignment"
        );
        bytes.chunks_exact(chunk_size).map(Vec::from).collect()
    }

    fn options() -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            "99999999-9999-9999-9999-999999999999",
            "caller-stream",
            "2026-05-28T11:30:00+05:30",
            "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
        );
        opts.chunk_size = BLOCK_SIZE as usize;
        opts
    }

    fn sha256_array(bytes: &[u8]) -> [u8; 32] {
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }

    fn scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("stream-orchestration-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 2,
        }
    }

    fn capacity_input(projected_object_blocks: u64) -> CapacityReserveInput {
        CapacityReserveInput {
            projected_object_blocks,
            block_size_bytes: BLOCK_SIZE as u64,
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: 4,
            parity_shards_per_epoch: 2,
            sidecar_index_block_count: 1,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 0,
            remaining_bootstrap_count: 1,
            safety_margin_blocks: 4,
            remaining_tape_blocks: 10_000,
            empty_tape_usable_blocks: 10_000,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: 10_000_000,
        }
    }

    fn scoped_map_from_close(close: &ObjectWriteSummary) -> ScopedFilemarkMap {
        let mut entries = vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(
                close.tape_file_number,
                close.data_block_count,
                close.first_parity_data_ordinal,
            ),
        ];
        entries.extend(
            close
                .sidecars_emitted
                .iter()
                .map(|sidecar| sidecar.tape_file_entry().to_map_entry()),
        );
        entries.extend(
            close
                .control_tape_files_emitted
                .iter()
                .map(|entry| entry.to_map_entry()),
        );
        let map = FilemarkMap::new(entries).unwrap();
        ScopedFilemarkMap::from_catalog(map, close.highest_protected_ordinal)
    }

    fn normalized_entry(
        file_id: &str,
        path: &str,
        kind: EntryKind,
        size_bytes: Option<u64>,
    ) -> NormalizedEntry {
        NormalizedEntry {
            file_id: file_id.into(),
            path: path.to_string(),
            kind,
            link_target: None,
            size_bytes,
            adapter_state: Vec::new(),
        }
    }

    fn damage_range(file_id: &str, start: u64, end: u64, status: DamageStatus) -> DamageRange {
        DamageRange {
            file_id: file_id.into(),
            start,
            end,
            status,
            adapter_state: Vec::new(),
        }
    }

    struct ScriptedArchiveReader {
        entries: Vec<NormalizedEntry>,
        damages: Vec<DamageRange>,
        data: Vec<(remanence_format::FileId, u64, Vec<u8>)>,
    }

    impl ArchiveReader for ScriptedArchiveReader {
        fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError> {
            for entry in &self.entries {
                sink.entry(entry)?;
            }
            for damage in &self.damages {
                sink.damage(damage)?;
            }
            Ok(ScanReport {
                entries: self.entries.len() as u64,
                damage_events: self.damages.len() as u64,
                archive_gaps: 0,
            })
        }

        fn stream_all(
            &mut self,
            sink: &mut dyn ArchiveEventSink,
        ) -> Result<StreamReport, FormatError> {
            let mut bytes = 0u64;
            for entry in &self.entries {
                sink.begin_entry(entry)?;
                if entry.kind == EntryKind::RegularFile {
                    for (_, file_offset, data) in self
                        .data
                        .iter()
                        .filter(|(file_id, _, _)| *file_id == entry.file_id)
                    {
                        sink.write_file_data(*file_offset, data)?;
                        bytes += data.len() as u64;
                    }
                }
                for damage in self
                    .damages
                    .iter()
                    .filter(|damage| damage.file_id == entry.file_id)
                {
                    sink.report_damage(damage)?;
                }
                sink.end_entry(entry)?;
            }
            Ok(StreamReport {
                entries: self.entries.len() as u64,
                bytes,
                damage_events: self.damages.len() as u64,
                archive_gaps: 0,
            })
        }

        fn stream_file(
            &mut self,
            _file_id: &remanence_format::FileId,
            _sink: &mut dyn remanence_format::FileDataSink,
        ) -> Result<remanence_format::FileStreamReport, FormatError> {
            Err(FormatError::unsupported(
                "scripted archive reader does not implement stream_file",
            ))
        }
    }

    struct PhysicalVecTapeSource {
        blocks_by_lba: HashMap<u64, Vec<u8>>,
        cursor_lba: u64,
        end_lba: u64,
        configured_block_size: Option<u32>,
    }

    impl PhysicalVecTapeSource {
        fn from_sink(sink: &VecBlockSink) -> Self {
            Self {
                blocks_by_lba: sink
                    .block_lbas
                    .iter()
                    .copied()
                    .zip(sink.blocks.iter().cloned())
                    .collect(),
                cursor_lba: 0,
                end_lba: sink.next_lba(),
                configured_block_size: None,
            }
        }
    }

    impl RawTapeSource for PhysicalVecTapeSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if block_size == 0 {
                return Err(ParityError::Invariant("fixed block size is zero"));
            }
            self.configured_block_size = Some(block_size);
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.cursor_lba = hint.lba;
            Ok(())
        }

        fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            if count < 0 {
                return Err(ParityError::TapeIo(TapeIoError::OperationFailed(
                    "backward filemark spacing is not implemented in this fixture".to_string(),
                )));
            }
            let mut remaining = count;
            while remaining > 0 && self.cursor_lba < self.end_lba {
                if self.blocks_by_lba.contains_key(&self.cursor_lba) {
                    self.cursor_lba += 1;
                    continue;
                }
                self.cursor_lba += 1;
                remaining -= 1;
            }
            Ok(SpaceFilemarksOutcome {
                filemarks_spaced: count - remaining,
                position_after: PhysicalPositionHint::new(self.cursor_lba),
                hit_end_of_data: remaining > 0,
            })
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            if let Some(block) = self.blocks_by_lba.get(&self.cursor_lba) {
                if buf.len() < block.len() {
                    return Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall {
                        actual: block.len() as u32,
                        provided: buf.len() as u32,
                    }));
                }
                buf[..block.len()].copy_from_slice(block);
                self.cursor_lba += 1;
                return Ok(RawReadOutcome::Block {
                    bytes: block.len(),
                    position_after: PhysicalPositionHint::new(self.cursor_lba),
                });
            }
            if self.cursor_lba < self.end_lba {
                self.cursor_lba += 1;
                return Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor_lba),
                });
            }
            Ok(RawReadOutcome::EndOfData {
                position_after: PhysicalPositionHint::new(self.cursor_lba),
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.cursor_lba))
        }
    }
}
