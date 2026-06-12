//! Generic format-driver traits for native and foreign archive formats.
//!
//! `rao-v1` has concrete reader/writer functions in this crate. These
//! traits describe the wider registry boundary: Remanence registers executable
//! format drivers, not just declarative field schemas. Native formats consume
//! object-local block streams, while foreign legacy formats may need physical
//! tape records before they can expose normalized archive events.

use std::io::Read;

use remanence_library::{BlockSink, BlockSource, PhysicalTapeSource};

use crate::error::FormatError;

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
