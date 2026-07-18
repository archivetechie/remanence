//! RAO encrypted-stream funnel for format and CLI callers.
//!
//! These functions deliberately contain no cryptographic construction or
//! framing logic. They keep callers at the body-format boundary while
//! delegating every operation to `remanence-aead`.

use std::io::{Read, Write};

use remanence_aead::{
    CoveringStoredRange, EnvelopeSealOptions, OpenReport, RangeOpenReport, RecipientPrivateKey,
    SealReport,
};

use crate::FormatError;

/// Seal canonical RAO plaintext as a recipient envelope.
pub fn seal_envelope_rao_stream<R: Read, W: Write>(
    plaintext: R,
    output: W,
    options: &EnvelopeSealOptions,
) -> Result<SealReport, FormatError> {
    remanence_aead::seal(plaintext, output, options).map_err(Into::into)
}

/// Open a recipient envelope to canonical RAO plaintext.
pub fn open_envelope_rao_stream<R: Read, W: Write>(
    input: R,
    output: W,
    recipient: &RecipientPrivateKey,
) -> Result<OpenReport, FormatError> {
    remanence_aead::open(input, output, recipient).map_err(Into::into)
}

/// Authenticate an envelope prefix and map a plaintext range to stored bytes.
pub fn covering_envelope_rao_stored_range(
    authenticated_prefix: &[u8],
    recipient: &RecipientPrivateKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<CoveringStoredRange, FormatError> {
    remanence_aead::covering_stored_range(
        authenticated_prefix,
        recipient,
        plaintext_start,
        plaintext_len,
    )
    .map_err(Into::into)
}

/// Open a plaintext range from bounded covering ciphertext frames.
pub fn open_envelope_rao_range_from_reader<R: Read + ?Sized, W: Write + ?Sized>(
    authenticated_prefix: &[u8],
    ranged_input: &mut R,
    stored_range_start: u64,
    output: &mut W,
    recipient: &RecipientPrivateKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<RangeOpenReport, FormatError> {
    remanence_aead::open_plaintext_range_from_reader(
        authenticated_prefix,
        ranged_input,
        stored_range_start,
        output,
        recipient,
        plaintext_start,
        plaintext_len,
    )
    .map_err(Into::into)
}
