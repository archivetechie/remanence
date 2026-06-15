//! Generic format-driver traits for native and foreign archive formats.
//!
//! `rao-v1` has concrete reader/writer functions in `remanence-format`. These
//! traits describe the wider registry boundary: Remanence registers executable
//! format drivers, not just declarative field schemas. Native formats consume
//! object-local block streams, while foreign legacy formats may need physical
//! tape records before they can expose normalized archive events.

use std::io::Read;

use remanence_library::{BlockSink, BlockSource, PhysicalTapeSource, TapeIoError};
use thiserror::Error;

/// RAO gate whose failure can be named across nested plaintext/envelope checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatGate {
    /// The `REMANENCE.format_id` value is missing or unsupported.
    FormatId,
    /// The `REMANENCE.schema_version` major version is unsupported.
    SchemaVersion,
    /// The inner plaintext stream advertises encryption other than `none`.
    Encryption,
    /// The inner plaintext chunk size disagrees with caller or envelope geometry.
    ChunkSize,
    /// The inner plaintext object id disagrees with the authenticated envelope.
    ObjectId,
}

/// Errors returned by Remanence format drivers.
#[derive(Debug, Error)]
pub enum FormatError {
    /// The caller supplied invalid object or file metadata.
    #[error("invalid RAO input: {0}")]
    InvalidInput(String),

    /// Layout math overflowed or could not satisfy a required invariant.
    #[error("RAO layout error: {0}")]
    Layout(String),

    /// The object archive is malformed or unsupported.
    #[error("RAO parse error: {0}")]
    Parse(String),

    /// A tar header checksum does not match the stored checksum.
    #[error("ustar checksum mismatch: stored {stored}, computed {computed}")]
    UstarChecksumMismatch {
        /// Checksum stored in the ustar header.
        stored: u64,
        /// Checksum computed with the checksum field treated as spaces.
        computed: u64,
    },

    /// The archive contains a tar typeflag this reader does not support yet.
    #[error("unsupported tar typeflag 0x{typeflag:02x}")]
    UnsupportedTarTypeflag {
        /// Raw typeflag byte from the ustar header.
        typeflag: u8,
    },

    /// A regular-file payload did not start on a RAO chunk boundary.
    #[error("file data for {path} is not chunk aligned at offset {data_offset}")]
    ChunkAlignmentViolation {
        /// Entry path after applying pax path overrides.
        path: String,
        /// Byte offset of the payload.
        data_offset: u64,
    },

    /// The stream-advertised chunk size does not match the supplied geometry.
    #[error("chunk_size {advertised} does not match supplied geometry {supplied}")]
    ChunkSizeMismatch {
        /// Chunk size advertised by the object stream.
        advertised: usize,
        /// Chunk size supplied out of band by the caller/catalog.
        supplied: usize,
    },

    /// A reader-side effective path violates the canonical relative-path rules.
    #[error("invalid RAO path: {0}")]
    InvalidPath(String),

    /// A hardlink target is absent, not a regular primary, or appears later.
    #[error("invalid hardlink target for {path}: {target}")]
    InvalidHardlinkTarget {
        /// Hardlink entry path after pax processing.
        path: String,
        /// Hardlink target path.
        target: String,
    },

    /// A tar payload or header was truncated.
    #[error("truncated tar payload")]
    TruncatedPayload,

    /// A pax extended-header record is malformed.
    #[error("malformed pax record: {0}")]
    PaxRecordMalformed(String),

    /// CBOR manifest encoding failed.
    #[error("manifest CBOR encode failed: {0}")]
    Cbor(String),

    /// The manifest bytes do not match the trusted anchor digest.
    #[error("manifest digest does not match anchor")]
    ManifestDigestMismatch,

    /// The manifest decoded but violates the RAO manifest schema.
    #[error("manifest schema invalid: {0}")]
    ManifestInvalid(String),

    /// A full-file restore digest did not match `REMANENCE.file_sha256`.
    #[error("file digest mismatch for {path}: expected {expected}, actual {actual}")]
    FileDigestMismatch {
        /// Effective path after pax processing.
        path: String,
        /// Expected lowercase hex SHA-256.
        expected: String,
        /// Actual lowercase hex SHA-256.
        actual: String,
    },

    /// A decrypted inner RAO stream disagrees with the authenticated envelope.
    #[error("inner RAO object mismatch at {gate:?}: {message}")]
    InnerObjectMismatch {
        /// Structured gate that failed.
        gate: FormatGate,
        /// Human-readable diagnostic.
        message: String,
    },

    /// The RAO encrypted envelope failed to seal, open, or inspect.
    #[error("RAO AEAD envelope failed: {0}")]
    Aead(#[from] remanence_aead::RaoAeadError),

    /// The requested operation is not supported by this format driver.
    #[error("unsupported format operation: {0}")]
    UnsupportedOperation(String),

    /// The archive advertises a format feature this reader does not implement.
    #[error("unsupported format feature: {0}")]
    UnsupportedFeature(String),

    /// The archive failed a named RAO format gate.
    #[error("unsupported format feature: {message}")]
    UnsupportedFormatGate {
        /// Structured gate that failed.
        gate: FormatGate,
        /// Human-readable diagnostic.
        message: String,
    },

    /// The block sink accepted a body-block write but reported that the
    /// block was not fully committed.
    #[error(
        "block sink wrote {bytes_written} of {expected_bytes} bytes \
         (early_warning={early_warning}, end_of_medium={end_of_medium})"
    )]
    IncompleteBlockWrite {
        /// Bytes the writer attempted to commit as one body block.
        expected_bytes: u64,
        /// Bytes the sink reported as committed.
        bytes_written: u64,
        /// Whether the sink reported early warning.
        early_warning: bool,
        /// Whether the sink reported hard end-of-medium.
        end_of_medium: bool,
    },

    /// Reading caller-provided source bytes failed.
    #[error("source I/O failed while {context}: {source}")]
    SourceIo {
        /// Operation being performed when the read failed.
        context: String,
        /// Underlying source read error.
        source: std::io::Error,
    },

    /// The underlying block sink/source failed.
    #[error(transparent)]
    TapeIo(#[from] TapeIoError),
}

impl FormatError {
    /// Construct an invalid-input error.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    /// Construct a layout error.
    pub fn layout(message: impl Into<String>) -> Self {
        Self::Layout(message.into())
    }

    /// Construct a parse error.
    pub fn parse(message: impl Into<String>) -> Self {
        Self::Parse(message.into())
    }

    /// Construct a CBOR error.
    pub fn cbor(message: impl Into<String>) -> Self {
        Self::Cbor(message.into())
    }

    /// Construct a manifest-schema error.
    pub fn manifest_invalid(message: impl Into<String>) -> Self {
        Self::ManifestInvalid(message.into())
    }

    /// Construct an invalid-path error.
    pub fn invalid_path(message: impl Into<String>) -> Self {
        Self::InvalidPath(message.into())
    }

    /// Construct a full-file digest mismatch error.
    pub fn file_digest_mismatch(path: impl Into<String>, expected: &[u8], actual: &[u8]) -> Self {
        Self::FileDigestMismatch {
            path: path.into(),
            expected: hex_lower(expected),
            actual: hex_lower(actual),
        }
    }

    /// Construct an inner-object mismatch error.
    pub fn inner_object_mismatch(gate: FormatGate, message: impl Into<String>) -> Self {
        Self::InnerObjectMismatch {
            gate,
            message: message.into(),
        }
    }

    /// Construct an unsupported-operation error.
    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::UnsupportedOperation(message.into())
    }

    /// Construct an unsupported-feature error.
    pub fn unsupported_feature(message: impl Into<String>) -> Self {
        Self::UnsupportedFeature(message.into())
    }

    /// Construct an unsupported-format-gate error.
    pub fn unsupported_format_gate(gate: FormatGate, message: impl Into<String>) -> Self {
        Self::UnsupportedFormatGate {
            gate,
            message: message.into(),
        }
    }

    /// Construct a source-I/O error.
    pub fn source_io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::SourceIo {
            context: context.into(),
            source,
        }
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

/// Stable identifier for one file entry inside an archive object.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub String);

impl FileId {
    /// Borrow the identifier as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for FileId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for FileId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

/// What kind of input a format driver needs before it can parse bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceRequirement {
    /// Object-local fixed blocks, normally from Layer 3c's object source.
    ObjectBlocks,
    /// Physical tape records and filemarks, before Remanence object decoding.
    PhysicalTapeRecords,
    /// A dump file or equivalent byte stream.
    ByteStreamDump,
    /// Already-materialized object bytes.
    ObjectBytes,
}

/// Capabilities advertised by a format driver for the current source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FormatCapabilities {
    /// The driver can enumerate entries without restoring all payload bytes.
    pub catalog_scan: bool,
    /// The driver can stream entries in archive order.
    pub sequential_restore: bool,
    /// The driver can seek to an individual file after an index exists.
    pub indexed_file_restore: bool,
    /// The driver can stream a byte range within one file.
    pub range_read: bool,
    /// The driver can create new objects in this format.
    pub write: bool,
    /// The driver can validate format-level checksums.
    pub verify: bool,
    /// The driver can report damaged byte ranges without aborting the stream.
    pub damage_events: bool,
    /// The driver preserves archival metadata beyond path and bytes.
    pub metadata_preserving: bool,
}

impl FormatCapabilities {
    /// Capabilities currently implemented by the native `rao-v1`
    /// code paths. Keep this honest: indexed/range restore and
    /// driver-level verify need a concrete `NativeBodyFormat` adapter
    /// before they can be advertised here.
    pub const REM_TAR_V1: Self = Self {
        catalog_scan: true,
        sequential_restore: true,
        indexed_file_restore: false,
        range_read: false,
        write: true,
        verify: false,
        damage_events: false,
        metadata_preserving: true,
    };
}

/// Static descriptor every registered format driver exposes.
pub trait FormatDescriptor {
    /// Stable format identifier recorded in catalogs and manifests.
    fn id(&self) -> &'static str;

    /// Version string for this driver's on-tape/stream contract.
    fn version(&self) -> &'static str;

    /// Input source required by this driver.
    fn source_requirement(&self) -> SourceRequirement;

    /// Capabilities available for this driver/source combination.
    fn capabilities(&self) -> FormatCapabilities;
}

/// Metadata needed to open one native Remanence body object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectFormatMetadata {
    /// Format identifier recorded for the object.
    pub format_id: String,
    /// Object-local block size in bytes.
    pub block_size_bytes: usize,
    /// Number of object-local blocks in the object tape file.
    pub block_count: u64,
    /// Adapter-owned serialized state.
    pub adapter_state: Vec<u8>,
}

/// Normalized archive entry visible to Layer 4/5 callers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEntry {
    /// Stable file identifier inside the object.
    pub file_id: FileId,
    /// UTF-8 presentation path.
    pub path: String,
    /// Entry kind.
    pub kind: EntryKind,
    /// Link target for symlink or hardlink entries, when the driver can expose
    /// it without format-specific adapter decoding.
    pub link_target: Option<String>,
    /// Payload size, when the format knows it before streaming.
    pub size_bytes: Option<u64>,
    /// Adapter-owned entry metadata.
    pub adapter_state: Vec<u8>,
}

/// Normalized archive entry type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular byte stream.
    RegularFile,
    /// Directory entry.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Hard link.
    Hardlink,
    /// Device or other unsupported special file.
    Special,
}

/// Non-fatal integrity status for a file byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageStatus {
    /// Format-level checksum failed for this range.
    ChecksumFailed,
    /// Tape read failed but the driver could keep scanning.
    ReadError,
    /// Source ended before the format-declared range was complete.
    Missing,
    /// Entry type or payload encoding is not supported by this driver.
    Unsupported,
}

/// Non-fatal damage or provenance event for one file range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DamageRange {
    /// File identifier associated with the damaged range.
    pub file_id: FileId,
    /// Start byte offset within the file.
    pub start: u64,
    /// Exclusive end byte offset within the file.
    pub end: u64,
    /// Damage status.
    pub status: DamageStatus,
    /// Adapter-owned source location or diagnostic bytes.
    pub adapter_state: Vec<u8>,
}

/// Why source bytes could not be attributed to a normalized file range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveGapCause {
    /// The driver encountered bytes that are not valid for the current archive state.
    UnrecognizedData,
    /// The source could not be read at this position.
    ReadError,
    /// The source ended before the driver reached a valid archive boundary.
    Missing,
    /// The driver intentionally skipped source bytes while looking for a new boundary.
    Resync,
    /// The source bytes describe an unsupported format feature.
    Unsupported,
}

/// Non-fatal damage or provenance event for source bytes not tied to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveGapRange {
    /// Adapter-defined inclusive start source offset.
    ///
    /// For byte-stream drivers this is usually a byte offset. For physical tape
    /// drivers this may be a logical archive offset, with physical position
    /// details serialized into `adapter_state`.
    pub source_start: u64,
    /// Adapter-defined exclusive end source offset.
    pub source_end: u64,
    /// Gap cause.
    pub cause: ArchiveGapCause,
    /// Adapter-owned source location, raw bytes, or diagnostic details.
    pub adapter_state: Vec<u8>,
}

/// Normalized manifest emitted by a format driver after scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatManifest {
    /// Format identifier.
    pub format_id: String,
    /// Entries in archive order.
    pub entries: Vec<NormalizedEntry>,
    /// Adapter-owned serialized index or continuation state.
    pub adapter_state: Vec<u8>,
}

/// Confidence level returned by a foreign-format probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeConfidence {
    /// The source is not this format.
    NoMatch,
    /// The source has weak hints but needs further validation.
    Possible,
    /// The source has a valid format header or equivalent signature.
    Probable,
    /// The source has been validated enough to open safely.
    Certain,
}

/// Result of probing a physical tape or dump for a foreign format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeResult {
    /// Format identifier.
    pub format_id: String,
    /// Probe confidence.
    pub confidence: ProbeConfidence,
    /// Input source required to continue.
    pub source_requirement: SourceRequirement,
    /// Adapter-owned probe state.
    pub adapter_state: Vec<u8>,
}

/// Summary returned after a scan-only pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanReport {
    /// Number of entries reported to the catalog sink.
    pub entries: u64,
    /// Number of non-fatal damage events reported.
    pub damage_events: u64,
    /// Number of non-fatal source gaps not attributable to one file.
    pub archive_gaps: u64,
}

/// Summary returned after streaming an archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamReport {
    /// Number of entries streamed.
    pub entries: u64,
    /// Number of file payload bytes delivered.
    pub bytes: u64,
    /// Number of non-fatal damage events reported.
    pub damage_events: u64,
    /// Number of non-fatal source gaps not attributable to one file.
    pub archive_gaps: u64,
}

/// Summary returned after streaming one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileStreamReport {
    /// Number of file payload bytes delivered.
    pub bytes: u64,
    /// Number of non-fatal damage events reported.
    pub damage_events: u64,
}

/// Sink for normalized catalog entries discovered during a scan.
pub trait EntryCatalogSink {
    /// Record one normalized entry.
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;

    /// Record one non-fatal damage event discovered during the scan.
    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError>;

    /// Record one non-fatal source gap not attributable to one file.
    fn archive_gap(&mut self, _range: &ArchiveGapRange) -> Result<(), FormatError> {
        Ok(())
    }
}

/// Sink for sequential archive restore events.
pub trait ArchiveEventSink {
    /// Called before an entry's payload bytes are delivered.
    fn begin_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;

    /// Called with contiguous payload bytes for the active entry.
    ///
    /// `file_offset` is the byte offset within the active file where `bytes`
    /// belong. Drivers should emit data and file-scoped damage events in
    /// increasing file-offset order when the format allows that, but sinks must
    /// use this explicit offset instead of assuming pure append semantics.
    fn write_file_data(&mut self, file_offset: u64, bytes: &[u8]) -> Result<(), FormatError>;

    /// Called for non-fatal damage associated with the active entry.
    fn report_damage(&mut self, range: &DamageRange) -> Result<(), FormatError>;

    /// Called for non-fatal source gaps not attributable to one file.
    fn report_archive_gap(&mut self, _range: &ArchiveGapRange) -> Result<(), FormatError> {
        Ok(())
    }

    /// Called after all available payload bytes for the active entry.
    fn end_entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError>;
}

/// Sink for single-file restore events.
pub trait FileDataSink {
    /// Called with contiguous file payload bytes.
    fn write_data(&mut self, bytes: &[u8]) -> Result<(), FormatError>;

    /// Called for non-fatal damage associated with the file.
    fn report_damage(&mut self, range: &DamageRange) -> Result<(), FormatError>;
}

/// Normalized archive reader produced by any readable format driver.
pub trait ArchiveReader {
    /// Scan entries and adapter-owned index state without restoring payloads.
    fn scan(&mut self, sink: &mut dyn EntryCatalogSink) -> Result<ScanReport, FormatError>;

    /// Stream entries in archive order.
    fn stream_all(&mut self, sink: &mut dyn ArchiveEventSink) -> Result<StreamReport, FormatError>;

    /// Stream one file by id. Drivers without this capability return
    /// [`FormatError::UnsupportedOperation`].
    fn stream_file(
        &mut self,
        file_id: &FileId,
        sink: &mut dyn FileDataSink,
    ) -> Result<FileStreamReport, FormatError>;
}

/// Normalized archive writer produced by writable native body formats.
pub trait ArchiveWriter {
    /// Write one regular file entry from a streaming source.
    fn write_file(
        &mut self,
        entry: &NormalizedEntry,
        reader: &mut dyn Read,
    ) -> Result<(), FormatError>;

    /// Finish the object and return adapter-owned manifest state.
    fn finish(&mut self) -> Result<FormatManifest, FormatError>;
}

/// Driver for native Remanence body formats.
pub trait NativeBodyFormat: FormatDescriptor {
    /// Open a reader over an object-local block source.
    fn open_object_reader<'a>(
        &self,
        source: &'a mut dyn BlockSource,
        metadata: &ObjectFormatMetadata,
    ) -> Result<Box<dyn ArchiveReader + 'a>, FormatError>;

    /// Open a writer over an object-local block sink.
    fn open_object_writer<'a>(
        &self,
        sink: &'a mut dyn BlockSink,
        manifest: &FormatManifest,
    ) -> Result<Box<dyn ArchiveWriter + 'a>, FormatError>;
}

/// Driver for read-only legacy or foreign tape formats.
pub trait ForeignTapeFormat: FormatDescriptor {
    /// Probe a physical tape stream for this format.
    fn probe(&self, source: &mut dyn PhysicalTapeSource) -> Result<ProbeResult, FormatError>;

    /// Open a normalized reader over a physical tape stream.
    fn open_tape_reader<'a>(
        &self,
        source: &'a mut dyn PhysicalTapeSource,
        probe: &ProbeResult,
    ) -> Result<Box<dyn ArchiveReader + 'a>, FormatError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rem_tar_v1_capabilities_do_not_advertise_unwired_driver_features() {
        let capabilities = FormatCapabilities::REM_TAR_V1;

        assert!(capabilities.catalog_scan);
        assert!(capabilities.sequential_restore);
        assert!(capabilities.write);
        assert!(capabilities.metadata_preserving);
        assert!(!capabilities.indexed_file_restore);
        assert!(!capabilities.range_read);
        assert!(!capabilities.verify);
    }
}
