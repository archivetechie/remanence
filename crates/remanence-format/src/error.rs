//! Error surface for Layer 3b body-format operations.

use remanence_library::TapeIoError;
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

/// Errors returned by `remanence-format`.
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
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }

    pub(crate) fn layout(message: impl Into<String>) -> Self {
        Self::Layout(message.into())
    }

    pub(crate) fn parse(message: impl Into<String>) -> Self {
        Self::Parse(message.into())
    }

    pub(crate) fn cbor(message: impl Into<String>) -> Self {
        Self::Cbor(message.into())
    }

    pub(crate) fn manifest_invalid(message: impl Into<String>) -> Self {
        Self::ManifestInvalid(message.into())
    }

    pub(crate) fn invalid_path(message: impl Into<String>) -> Self {
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

    pub(crate) fn inner_object_mismatch(gate: FormatGate, message: impl Into<String>) -> Self {
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

    pub(crate) fn unsupported_format_gate(gate: FormatGate, message: impl Into<String>) -> Self {
        Self::UnsupportedFormatGate {
            gate,
            message: message.into(),
        }
    }

    pub(crate) fn source_io(context: impl Into<String>, source: std::io::Error) -> Self {
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
