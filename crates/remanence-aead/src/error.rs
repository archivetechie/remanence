//! Typed errors for REM-OBJECT encrypted-envelope processing.

use thiserror::Error;

/// Convenience result alias for REM-OBJECT AEAD operations.
pub type Result<T> = std::result::Result<T, RemObjectAeadError>;

/// Errors named to match the REM-OBJECT 1.0 envelope error taxonomy.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RemObjectAeadError {
    /// The input does not begin with the `REMO` envelope magic.
    #[error("invalid REM-OBJECT magic bytes")]
    InvalidMagicBytes,
    /// The header length field is not 128.
    #[error("invalid REM-OBJECT header length")]
    InvalidHeaderLength,
    /// The format version field is not supported for `REMO`.
    #[error("unsupported REM-OBJECT format version")]
    UnsupportedFormatVersion,
    /// The suite id is not HKDF-SHA-256 + ChaCha20-Poly1305.
    #[error("invalid REM-OBJECT AEAD suite")]
    InvalidSuite,
    /// The wrapping suite is unknown or inconsistent with its key frame.
    #[error("invalid REM-OBJECT wrapping suite")]
    InvalidWrapSuite,
    /// The key-frame length is outside its frozen bounds.
    #[error("invalid REM-OBJECT key-frame length")]
    InvalidKeyFrameLength,
    /// The wrapped-key frame is malformed or non-canonical.
    #[error("invalid REM-OBJECT wrapped-key frame")]
    InvalidKeyFrame,
    /// The operating system could not provide cryptographic randomness.
    #[error("operating-system CSPRNG failed")]
    EntropyUnavailable,
    /// HPKE key parsing, encapsulation, or authenticated opening failed.
    #[error("REM-OBJECT HPKE operation failed")]
    HpkeFailed,
    /// No wrapped-key slot matches the supplied recipient epoch.
    #[error("no REM-OBJECT recipient slot matches the supplied private key")]
    RecipientEpochMismatch,
    /// The chunk size is not a positive multiple of 512.
    #[error("invalid REM-OBJECT chunk size")]
    InvalidChunkSize,
    /// Reserved header bytes or flags are nonzero.
    #[error("reserved REM-OBJECT header bytes or flags are not zero")]
    ReservedBytesNotZero,
    /// HKDF could not expand one of the fixed-size output keys.
    #[error("REM-OBJECT HKDF expansion failed")]
    KdfExpansionFailed,
    /// The header salt is invalid.
    #[error("invalid REM-OBJECT HKDF salt")]
    InvalidSalt,
    /// The metadata frame length is outside REM-OBJECT bounds.
    #[error("invalid REM-OBJECT metadata frame length")]
    MetadataFrameLengthInvalid,
    /// The encrypted object id header field is malformed.
    #[error("invalid REM-OBJECT object_id header field")]
    InvalidObjectIdField,
    /// The caller supplied invalid sealing input.
    #[error("invalid REM-OBJECT sealing input: {0}")]
    InvalidInput(String),
    /// Input ended before a required envelope byte was available.
    #[error("unexpected end of file")]
    UnexpectedEof,
    /// A STREAM payload ended before the authenticated final chunk.
    #[error("missing authenticated final AEAD chunk")]
    MissingFinalChunk,
    /// The completion footer is absent or misplaced.
    #[error("invalid REM-OBJECT completion footer")]
    InvalidFooter,
    /// Bytes after the stored object were present.
    #[error("trailing data after REM-OBJECT object")]
    TrailingData,
    /// Final fill bytes were not all zero.
    #[error("REM-OBJECT final fill is not zero")]
    FillNotZero,
    /// ChaCha20-Poly1305 authentication failed.
    #[error("AEAD authentication failed")]
    AeadAuthenticationFailed,
    /// Metadata CBOR violates the REM-OBJECT deterministic-CBOR profile.
    #[error("invalid deterministic CBOR encoding")]
    InvalidCborEncoding,
    /// A required metadata key is absent.
    #[error("missing required metadata field")]
    MissingRequiredMetadataField,
    /// A metadata key has the wrong type or value.
    #[error("invalid metadata field")]
    InvalidMetadataField,
    /// Header salt does not match the spec derivation.
    #[error("REM-OBJECT salt derivation mismatch")]
    SaltDerivationMismatch,
    /// Plaintext digest did not match metadata.
    #[error("plaintext digest mismatch")]
    PlaintextDigestMismatch,
    /// Plaintext size did not match metadata.
    #[error("plaintext size mismatch")]
    PlaintextSizeMismatch,
    /// Derived arithmetic overflowed.
    #[error("REM-OBJECT size arithmetic overflow")]
    SizeOverflow,
    /// Underlying I/O failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub(crate) fn map_read_exact_error(err: std::io::Error) -> RemObjectAeadError {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        RemObjectAeadError::UnexpectedEof
    } else {
        RemObjectAeadError::Io(err)
    }
}
