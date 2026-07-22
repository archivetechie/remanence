//! Public value types for `rao-v1`.

use std::collections::BTreeMap;

/// POSIX tar records are always 512 bytes.
pub const TAR_RECORD_SIZE: usize = 512;

/// Default `rao-v1` chunk / body block size.
pub const DEFAULT_CHUNK_SIZE: usize = 256 * 1024;

/// Format identifier recorded in global pax headers.
pub const FORMAT_ID: &str = "rao-v1";

/// Schema version recorded in global pax headers.
pub const SCHEMA_VERSION: &str = "1.0";

/// Schema version recorded when a v1.1 additive feature is used.
pub const SCHEMA_VERSION_XATTRS: &str = "1.1";

/// The manifest path inside every `rao-v1` object.
pub const MANIFEST_PATH: &str = "_remanence/manifest.cbor";

/// Maximum manifest `file_entries` array length accepted by the RAO profile.
pub(crate) const MAX_FILE_ENTRIES: usize = 10_000_000;

/// Preserved extended attributes keyed by xattr name.
pub type RemTarXattrs = BTreeMap<String, Vec<u8>>;

/// Extension members keyed by their extension name.
pub type RemTarExtensions = BTreeMap<String, RemTarCborValue>;

/// A value in the deterministic RAO manifest CBOR profile.
///
/// The profile intentionally excludes negative integers, floats, tags, and
/// maps with non-text keys. Keeping extension data in this restricted type
/// makes decoded unknown members safe to preserve and canonically re-emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemTarCborValue {
    /// An unsigned integer.
    Unsigned(u64),
    /// An opaque byte string.
    Bytes(Vec<u8>),
    /// A UTF-8 text string.
    Text(String),
    /// A Boolean value.
    Bool(bool),
    /// The CBOR null value.
    Null,
    /// An ordered sequence of profile values.
    Array(Vec<Self>),
    /// A map with text keys. Writers encode keys in deterministic CBOR order.
    Map(BTreeMap<String, Self>),
}

/// Per-object logical block address. Starts at zero for each object archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BodyLba(pub u64);

/// Metadata preservation tier advertised by an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataPreservation {
    /// Preserve path and file bytes only.
    Minimal,
    /// Preserve archival metadata expected to survive across systems.
    Archival,
    /// Preserve full POSIX-style metadata where available.
    Full,
}

impl MetadataPreservation {
    pub(crate) fn as_pax_value(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Archival => "archival",
            Self::Full => "full",
        }
    }
}

/// Object-level options for a `rao-v1` archive.
#[derive(Debug, Clone)]
pub struct RemTarObjectOptions {
    /// Remanence object UUID as text.
    pub object_id: String,
    /// Opaque caller/orchestrator object identifier.
    pub caller_object_id: String,
    /// Body block size. Must be a positive multiple of 512.
    pub chunk_size: usize,
    /// Metadata preservation policy recorded in the global pax header.
    pub metadata_preservation: MetadataPreservation,
    /// Encryption state recorded in the global pax header. The body format
    /// stores this as a flag only; key material never lives here.
    pub encryption: String,
    /// RFC3339 timestamp recorded in the global pax header.
    pub write_timestamp: String,
    /// UUID-like identifier for the manifest file's own pax metadata.
    pub manifest_file_id: String,
    /// Object-level extension members carried under `object_metadata.ext`.
    pub extensions: RemTarExtensions,
}

impl RemTarObjectOptions {
    /// Construct options with the default `rao-v1` chunk size and
    /// archival metadata preservation.
    pub fn new(
        object_id: impl Into<String>,
        caller_object_id: impl Into<String>,
        write_timestamp: impl Into<String>,
        manifest_file_id: impl Into<String>,
    ) -> Self {
        Self {
            object_id: object_id.into(),
            caller_object_id: caller_object_id.into(),
            chunk_size: DEFAULT_CHUNK_SIZE,
            metadata_preservation: MetadataPreservation::Archival,
            encryption: "none".to_string(),
            write_timestamp: write_timestamp.into(),
            manifest_file_id: manifest_file_id.into(),
            extensions: BTreeMap::new(),
        }
    }
}

/// File metadata known before streaming file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarFileSpec {
    /// Entry kind encoded in the ustar typeflag.
    pub entry_type: RemTarEntryType,
    /// UTF-8 path inside the object.
    pub path: String,
    /// Stable file identifier. A UUID string is preferred, but the format
    /// layer treats it as opaque UTF-8 text.
    pub file_id: String,
    /// Exact byte length of the file payload. Non-regular entries have zero.
    pub size_bytes: u64,
    /// SHA-256 of the exact file payload bytes. Absent for non-regular entries.
    pub file_sha256: Option<[u8; 32]>,
    /// Link target. Present only for symbolic-link and hardlink entries.
    pub link_target: Option<String>,
    /// Preserved extended attributes for RAO 1.1 objects.
    pub xattrs: RemTarXattrs,
    /// Entry-level extension members carried without applying them on restore.
    pub extensions: RemTarExtensions,
    /// Optional mtime value recorded in pax when supplied.
    pub mtime: Option<String>,
    /// Optional executable flag recorded in Remanence pax metadata.
    pub executable: Option<bool>,
}

impl RemTarFileSpec {
    /// Construct a file spec.
    pub fn new(
        path: impl Into<String>,
        file_id: impl Into<String>,
        size_bytes: u64,
        file_sha256: [u8; 32],
    ) -> Self {
        Self {
            entry_type: RemTarEntryType::Regular,
            path: path.into(),
            file_id: file_id.into(),
            size_bytes,
            file_sha256: Some(file_sha256),
            link_target: None,
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
            mtime: None,
            executable: None,
        }
    }

    /// Construct a symbolic-link entry spec.
    pub fn symlink(
        path: impl Into<String>,
        file_id: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            entry_type: RemTarEntryType::Symlink,
            path: path.into(),
            file_id: file_id.into(),
            size_bytes: 0,
            file_sha256: None,
            link_target: Some(target.into()),
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
            mtime: None,
            executable: None,
        }
    }

    /// Construct a hardlink entry spec.
    pub fn hardlink(
        path: impl Into<String>,
        file_id: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            entry_type: RemTarEntryType::Hardlink,
            path: path.into(),
            file_id: file_id.into(),
            size_bytes: 0,
            file_sha256: None,
            link_target: Some(target.into()),
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
            mtime: None,
            executable: None,
        }
    }

    /// Construct an empty-directory entry spec.
    pub fn directory(path: impl Into<String>, file_id: impl Into<String>) -> Self {
        Self {
            entry_type: RemTarEntryType::Directory,
            path: path.into(),
            file_id: file_id.into(),
            size_bytes: 0,
            file_sha256: None,
            link_target: None,
            xattrs: BTreeMap::new(),
            extensions: BTreeMap::new(),
            mtime: None,
            executable: None,
        }
    }
}

/// RAO entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemTarEntryType {
    /// Regular file with byte payload.
    Regular,
    /// Hardlink to a preceding regular-file primary, with no payload.
    Hardlink,
    /// Symbolic link with a target string and no payload.
    Symlink,
    /// Directory entry with no payload. RAO writers emit these for empty dirs.
    Directory,
}

impl RemTarEntryType {
    /// Return the manifest text value for non-regular entries.
    pub fn manifest_value(self) -> Option<&'static str> {
        match self {
            Self::Regular => None,
            Self::Hardlink => Some("hardlink"),
            Self::Symlink => Some("symlink"),
            Self::Directory => Some("directory"),
        }
    }
}

/// File content and metadata supplied to the writer.
#[derive(Debug, Clone, Copy)]
pub struct RemTarFile<'a> {
    /// UTF-8 path inside the object.
    pub path: &'a str,
    /// Stable file identifier.
    pub file_id: &'a str,
    /// Exact payload bytes to write.
    pub data: &'a [u8],
    /// Optional mtime value recorded in pax when supplied.
    pub mtime: Option<&'a str>,
    /// Optional executable flag recorded in Remanence pax metadata.
    pub executable: Option<bool>,
}

/// File metadata plus a streaming byte source supplied to the writer.
///
/// The caller must precompute [`RemTarFileSpec::size_bytes`] and
/// [`RemTarFileSpec::file_sha256`] before constructing this value. The writer
/// consumes exactly `size_bytes` bytes from `reader`, hashes them during the
/// write, and rejects the object if the observed hash differs from the spec.
pub struct RemTarFileStream<'a> {
    /// Preplanned metadata for this file.
    pub spec: RemTarFileSpec,
    /// Source bytes for the file payload.
    pub reader: &'a mut dyn std::io::Read,
}

impl<'a> RemTarFileStream<'a> {
    /// Construct one streaming file input.
    pub fn new(spec: RemTarFileSpec, reader: &'a mut dyn std::io::Read) -> Self {
        Self { spec, reader }
    }
}

/// Planned layout for one file entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemTarFileLayout {
    /// Entry kind encoded in the ustar typeflag.
    pub entry_type: RemTarEntryType,
    /// UTF-8 path inside the object.
    pub path: String,
    /// Stable file identifier.
    pub file_id: String,
    /// Exact payload byte length. Non-regular entries have zero.
    pub size_bytes: u64,
    /// SHA-256 of the exact file payload bytes. Absent for non-regular entries.
    pub file_sha256: Option<[u8; 32]>,
    /// Link target. Present only for symbolic-link and hardlink entries.
    pub link_target: Option<String>,
    /// Preserved extended attributes for RAO 1.1 objects.
    pub xattrs: RemTarXattrs,
    /// Entry-level extension members carried without applying them on restore.
    pub extensions: RemTarExtensions,
    /// Optional executable flag recorded in Remanence pax metadata.
    pub executable: Option<bool>,
    /// First data chunk LBA. Absent for zero-length files.
    pub first_chunk_lba: Option<BodyLba>,
    /// Number of body chunks that contain file data.
    pub chunk_count: u64,
    /// Byte offset at which this file's pax extended header begins.
    pub pax_header_offset: u64,
    /// Byte offset at which this file's payload begins.
    pub data_offset: u64,
    /// Number of spaces carried by `REMANENCE.pad`.
    pub pad_spaces: usize,
    /// Encoded pax body byte length before tar record padding.
    pub pax_body_len: usize,
    /// True for the generated manifest entry.
    pub is_manifest: bool,
}
