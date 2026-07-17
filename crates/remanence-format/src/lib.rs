//! Layer 3b — pluggable tape body formats for Remanence.
//!
//! This crate starts with `rao-v1`, the default body format in
//! `docs/spec-v0.4.md` §8.  A `rao-v1` object is a complete POSIX
//! pax tar archive in one tape file, streamed through
//! [`remanence_library::BlockSink`].  The implementation keeps the
//! body-format layer independent of Layer 3c: callers compose it with
//! `remanence-parity` by passing a parity-wrapped `BlockSink`.

#![warn(missing_docs)]
#![warn(unsafe_op_in_unsafe_fn)]

/// Backward-compatible error module for callers that import
/// `remanence_format::error::FormatError`.
pub mod error {
    pub use remanence_format_driver::{FormatError, FormatGate};
}
mod envelope;
pub mod layout;
mod manifest;
pub mod model;
pub mod pax;
pub mod pfr;
pub mod reader;
pub mod tar;
pub mod writer;

pub use envelope::{
    covering_envelope_rao_stored_range, open_envelope_rao_range_from_reader,
    open_envelope_rao_stream, seal_envelope_rao_stream,
};
pub use error::{FormatError, FormatGate};
pub use layout::{plan_rem_tar_object, RemTarObjectLayout};
#[cfg(feature = "fuzzing")]
pub use manifest::validate_manifest_cbor_for_fuzz;
pub use model::{
    BodyLba, MetadataPreservation, RemTarEntryType, RemTarFile, RemTarFileLayout, RemTarFileSpec,
    RemTarFileStream, RemTarObjectOptions, RemTarXattrs, DEFAULT_CHUNK_SIZE, FORMAT_ID,
    MANIFEST_PATH, SCHEMA_VERSION, SCHEMA_VERSION_XATTRS, TAR_RECORD_SIZE,
};
pub use pfr::{
    plan_plaintext_rao_file_range, read_encrypted_rao_file_range_to_vec,
    read_envelope_rao_file_range_to_vec, validate_file_range, EncryptedRaoFileRange,
    PlaintextRaoFileRangePlan,
};
pub use reader::{
    read_encrypted_rao_object, read_encrypted_rao_object_with_manifest_anchor,
    read_encrypted_rao_object_with_mode, read_encrypted_rao_object_with_mode_and_manifest_anchor,
    read_envelope_rao_object, read_envelope_rao_object_with_manifest_anchor,
    read_envelope_rao_object_with_mode, read_envelope_rao_object_with_mode_and_manifest_anchor,
    read_rem_tar_object, read_rem_tar_object_with_manifest_anchor, read_rem_tar_object_with_mode,
    read_rem_tar_object_with_mode_and_manifest_anchor, stream_rem_tar_object,
    stream_rem_tar_object_with_manifest_anchor, stream_rem_tar_object_with_mode,
    stream_rem_tar_object_with_mode_and_manifest_anchor, EncryptedRaoReadObject, ReadMode,
    RemTarDigestMismatch, RemTarEntrySink, RemTarReadEntry, RemTarReadObject, RemTarReadWarning,
    RemTarStreamEntry, RemTarStreamReport,
};
pub use remanence_format_driver::{
    ArchiveEventSink, ArchiveGapCause, ArchiveGapRange, ArchiveReader, ArchiveWriter, DamageRange,
    DamageStatus, EntryCatalogSink, EntryKind, FileDataSink, FileId, FileStreamReport,
    ForeignTapeFormat, FormatCapabilities, FormatDescriptor, FormatManifest, NativeBodyFormat,
    NormalizedEntry, ObjectFormatMetadata, ProbeConfidence, ProbeResult, ScanReport,
    SourceRequirement, StreamReport,
};
pub use writer::{
    write_encrypted_rao_object, write_encrypted_rao_object_from_readers, write_envelope_rao_object,
    write_envelope_rao_object_from_readers, write_rem_tar_object,
    write_rem_tar_object_from_readers, BodyBlockWriter, EncryptedRaoWriteReport,
};
