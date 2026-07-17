//! Typed errors for RAO encrypted-envelope processing.

use thiserror::Error;

/// Convenience result alias for RAO AEAD operations.
pub type Result<T> = std::result::Result<T, RaoAeadError>;

/// Errors named to match the RAO 1.0 envelope error taxonomy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RaoAeadError {
    /// The input does not begin with the `RAO1` envelope magic.
    #[error("invalid RAO magic bytes")]
    InvalidMagicBytes,
    /// The header length field is not 128.
    #[error("invalid RAO header length")]
    InvalidHeaderLength,
    /// The format version field is not supported for `RAO1`.
    #[error("unsupported RAO format version")]
    UnsupportedFormatVersion,
    /// The suite id is not HKDF-SHA-256 + ChaCha20-Poly1305.
    #[error("invalid RAO AEAD suite")]
    InvalidSuite,
    /// The v2 wrapping suite is unknown or inconsistent with its key frame.
    #[error("invalid RAO wrapping suite")]
    InvalidWrapSuite,
    /// The v2 key-frame length is outside its frozen bounds.
    #[error("invalid RAO key-frame length")]
    InvalidKeyFrameLength,
    /// The v2 wrapped-key frame is malformed or non-canonical.
    #[error("invalid RAO wrapped-key frame")]
    InvalidKeyFrame,
    /// The operating system could not provide cryptographic randomness.
    #[error("operating-system CSPRNG failed")]
    EntropyUnavailable,
    /// HPKE key parsing, encapsulation, or authenticated opening failed.
    #[error("RAO HPKE operation failed")]
    HpkeFailed,
    /// No wrapped-key slot matches the supplied recipient epoch.
    #[error("no RAO recipient slot matches the supplied private key")]
    RecipientEpochMismatch,
    /// The chunk size is not a positive multiple of 512.
    #[error("invalid RAO chunk size")]
    InvalidChunkSize,
    /// Reserved header bytes or flags are nonzero.
    #[error("reserved RAO header bytes or flags are not zero")]
    ReservedBytesNotZero,
    /// HKDF could not expand one of the fixed-size v2 output keys.
    #[error("RAO HKDF expansion failed")]
    KdfExpansionFailed,
    /// The header salt is invalid.
    #[error("invalid RAO HKDF salt")]
    InvalidSalt,
    /// The metadata frame length is outside RAO bounds.
    #[error("invalid RAO metadata frame length")]
    MetadataFrameLengthInvalid,
    /// The encrypted object id header field is malformed.
    #[error("invalid RAO object_id header field")]
    InvalidObjectIdField,
    /// The caller supplied invalid sealing input.
    #[error("invalid RAO sealing input: {0}")]
    InvalidInput(String),
    /// Input ended before a required envelope byte was available.
    #[error("unexpected end of file")]
    UnexpectedEof,
    /// A STREAM payload ended before the authenticated final chunk.
    #[error("missing authenticated final AEAD chunk")]
    MissingFinalChunk,
    /// The completion footer is absent or misplaced.
    #[error("invalid RAO completion footer")]
    InvalidFooter,
    /// Bytes after the stored object were present.
    #[error("trailing data after RAO object")]
    TrailingData,
    /// Final fill bytes were not all zero.
    #[error("RAO final fill is not zero")]
    FillNotZero,
    /// ChaCha20-Poly1305 authentication failed.
    #[error("AEAD authentication failed")]
    AeadAuthenticationFailed,
    /// Metadata CBOR violates the RAO deterministic-CBOR profile.
    #[error("invalid deterministic CBOR encoding")]
    InvalidCborEncoding,
    /// A required metadata key is absent.
    #[error("missing required metadata field")]
    MissingRequiredMetadataField,
    /// A metadata key has the wrong type or value.
    #[error("invalid metadata field")]
    InvalidMetadataField,
    /// Header salt does not match the spec derivation.
    #[error("RAO salt derivation mismatch")]
    SaltDerivationMismatch,
    /// Plaintext digest did not match metadata.
    #[error("plaintext digest mismatch")]
    PlaintextDigestMismatch,
    /// Plaintext size did not match metadata.
    #[error("plaintext size mismatch")]
    PlaintextSizeMismatch,
    /// Derived arithmetic overflowed.
    #[error("RAO size arithmetic overflow")]
    SizeOverflow,
    /// Underlying I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub(crate) fn map_read_exact_error(err: std::io::Error) -> RaoAeadError {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        RaoAeadError::UnexpectedEof
    } else {
        RaoAeadError::Io(err)
    }
}
