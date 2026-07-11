//! Keyed partial-range opening for encrypted RAO envelopes.
//!
//! The functions here implement the Section 6 ciphertext mapping: decrypt the
//! authenticated metadata frame, map plaintext body chunks to stored
//! ciphertext ranges, authenticate every fetched chunk, and release only the
//! caller-requested plaintext bytes.

use crate::error::{RaoAeadError, Result};
use crate::header::{RaoHeader, RAO_HEADER_LEN};
use crate::kdf::{derive_keys, derive_salt, RootKey};
use crate::metadata::RaoMetadata;
use crate::stream::{cipher_offset, decrypt_chunk, decrypt_metadata, CHACHA20POLY1305_TAG_LEN};

/// Report returned after successfully opening a plaintext subrange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeOpenReport {
    /// Parsed plaintext header.
    pub header: RaoHeader,
    /// Decrypted metadata.
    pub metadata: RaoMetadata,
    /// Absolute byte offset in the canonical plaintext object.
    pub plaintext_start: u64,
    /// Number of plaintext bytes returned.
    pub plaintext_len: u64,
    /// First authenticated AEAD payload chunk. Absent for empty ranges.
    pub first_chunk: Option<u64>,
    /// Number of authenticated AEAD payload chunks.
    pub chunk_count: u64,
    /// First stored ciphertext byte fetched. Absent for empty ranges.
    pub stored_range_start: Option<u64>,
    /// Number of contiguous stored ciphertext bytes fetched.
    pub stored_range_len: u64,
}

/// Open an absolute plaintext byte range from an encrypted RAO object.
///
/// `plaintext_start` and `plaintext_len` address the canonical plaintext
/// object. The input is the encrypted representation byte string; only the
/// metadata frame and the ciphertext chunks covering the requested range are
/// decrypted.
pub fn open_plaintext_range_to_vec(
    input: &[u8],
    root_key: &RootKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let (header, metadata, keys) = open_authenticated_metadata(input, root_key)?;
    open_plaintext_range_with_context(
        input,
        header,
        metadata,
        keys,
        plaintext_start,
        plaintext_len,
    )
}

/// Open a range relative to an inner RAO body block.
///
/// This is the direct Section 6.2/6.3 bridge used by file-level PFR:
/// `first_inner_chunk` is the member file's `first_chunk_lba`, and
/// `range_start`/`range_len` are byte offsets within that member file.
pub fn open_inner_range_to_vec(
    input: &[u8],
    root_key: &RootKey,
    first_inner_chunk: u64,
    range_start: u64,
    range_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let (header, metadata, keys) = open_authenticated_metadata(input, root_key)?;
    let absolute_start = first_inner_chunk
        .checked_mul(u64::from(header.chunk_size))
        .and_then(|value| value.checked_add(range_start))
        .ok_or(RaoAeadError::SizeOverflow)?;
    open_plaintext_range_with_context(input, header, metadata, keys, absolute_start, range_len)
}

/// Open and authenticate a v2 envelope plaintext range.
///
/// The standalone recovery path deliberately authenticates the complete object
/// before releasing a slice. Tape PFR can later replace this conservative
/// implementation without changing its strict envelope-mode contract.
pub fn open_plaintext_range_envelope_to_vec(
    input: &[u8],
    recipient: &crate::RecipientPrivateKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let (plaintext, opened) = crate::open_envelope_to_vec(input, recipient)?;
    let plaintext_end = validate_range(
        plaintext_start,
        plaintext_len,
        opened.metadata.plaintext_size,
    )?;
    let start = usize::try_from(plaintext_start).map_err(|_| RaoAeadError::SizeOverflow)?;
    let end = usize::try_from(plaintext_end).map_err(|_| RaoAeadError::SizeOverflow)?;
    let bytes = plaintext
        .get(start..end)
        .ok_or(RaoAeadError::SizeOverflow)?
        .to_vec();
    let chunk_size = u64::from(opened.header.chunk_size);
    let (first_chunk, chunk_count, stored_range_start, stored_range_len) = if plaintext_len == 0 {
        (None, 0, None, 0)
    } else {
        let first = plaintext_start / chunk_size;
        let last = (plaintext_end - 1) / chunk_size;
        let count = last - first + 1;
        let start = crate::cipher_offset_with_key_frame(
            opened.header.key_frame_len,
            opened.header.metadata_frame_len,
            opened.header.chunk_size,
            first,
        )?;
        let len = count
            .checked_mul(chunk_size + CHACHA20POLY1305_TAG_LEN)
            .ok_or(RaoAeadError::SizeOverflow)?;
        (Some(first), count, Some(start), len)
    };
    Ok((
        bytes,
        RangeOpenReport {
            header: opened.header,
            metadata: opened.metadata,
            plaintext_start,
            plaintext_len,
            first_chunk,
            chunk_count,
            stored_range_start,
            stored_range_len,
        },
    ))
}

fn open_plaintext_range_with_context(
    input: &[u8],
    header: RaoHeader,
    metadata: RaoMetadata,
    keys: crate::kdf::DerivedKeys,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let plaintext_end = validate_range(plaintext_start, plaintext_len, metadata.plaintext_size)?;
    if plaintext_len == 0 {
        return Ok((
            Vec::new(),
            RangeOpenReport {
                header,
                metadata,
                plaintext_start,
                plaintext_len,
                first_chunk: None,
                chunk_count: 0,
                stored_range_start: None,
                stored_range_len: 0,
            },
        ));
    }

    let chunk_size = u64::from(header.chunk_size);
    let object_chunk_count = metadata.plaintext_size / chunk_size;
    let first_chunk = plaintext_start / chunk_size;
    let last_chunk = (plaintext_end - 1) / chunk_size;
    let fetched_chunk_count = last_chunk
        .checked_sub(first_chunk)
        .and_then(|value| value.checked_add(1))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_chunk_len_u64 = chunk_size
        .checked_add(CHACHA20POLY1305_TAG_LEN)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_chunk_len =
        usize::try_from(stored_chunk_len_u64).map_err(|_| RaoAeadError::SizeOverflow)?;
    let stored_range_start =
        cipher_offset(header.metadata_frame_len, header.chunk_size, first_chunk)?;
    let last_chunk_start = cipher_offset(header.metadata_frame_len, header.chunk_size, last_chunk)?;
    let stored_range_end = last_chunk_start
        .checked_add(stored_chunk_len_u64)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_range_len = stored_range_end
        .checked_sub(stored_range_start)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_start =
        usize::try_from(stored_range_start).map_err(|_| RaoAeadError::SizeOverflow)?;
    let stored_end = usize::try_from(stored_range_end).map_err(|_| RaoAeadError::SizeOverflow)?;
    let encrypted_range = input
        .get(stored_start..stored_end)
        .ok_or(RaoAeadError::UnexpectedEof)?;

    let chunk_size_usize = usize::try_from(chunk_size).map_err(|_| RaoAeadError::SizeOverflow)?;
    let plaintext_len_usize =
        usize::try_from(plaintext_len).map_err(|_| RaoAeadError::SizeOverflow)?;
    let mut decrypted = Vec::new();
    decrypted
        .try_reserve_exact(
            usize::try_from(fetched_chunk_count)
                .ok()
                .and_then(|count| count.checked_mul(chunk_size_usize))
                .ok_or(RaoAeadError::SizeOverflow)?,
        )
        .map_err(|_| RaoAeadError::SizeOverflow)?;

    for offset in 0..fetched_chunk_count {
        let chunk_index = first_chunk
            .checked_add(offset)
            .ok_or(RaoAeadError::SizeOverflow)?;
        let encrypted_start = usize::try_from(offset)
            .ok()
            .and_then(|value| value.checked_mul(stored_chunk_len))
            .ok_or(RaoAeadError::SizeOverflow)?;
        let encrypted_end = encrypted_start
            .checked_add(stored_chunk_len)
            .ok_or(RaoAeadError::SizeOverflow)?;
        let final_chunk = chunk_index + 1 == object_chunk_count;
        let plaintext = decrypt_chunk(
            &keys.payload_key,
            chunk_index,
            final_chunk,
            &encrypted_range[encrypted_start..encrypted_end],
        )?;
        if plaintext.len() != chunk_size_usize {
            return Err(RaoAeadError::AeadAuthenticationFailed);
        }
        decrypted.extend_from_slice(&plaintext);
    }

    let trim_start =
        usize::try_from(plaintext_start % chunk_size).map_err(|_| RaoAeadError::SizeOverflow)?;
    let trim_end = trim_start
        .checked_add(plaintext_len_usize)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let bytes = decrypted
        .get(trim_start..trim_end)
        .ok_or(RaoAeadError::SizeOverflow)?
        .to_vec();

    Ok((
        bytes,
        RangeOpenReport {
            header,
            metadata,
            plaintext_start,
            plaintext_len,
            first_chunk: Some(first_chunk),
            chunk_count: fetched_chunk_count,
            stored_range_start: Some(stored_range_start),
            stored_range_len,
        },
    ))
}

fn open_authenticated_metadata(
    input: &[u8],
    root_key: &RootKey,
) -> Result<(RaoHeader, RaoMetadata, crate::kdf::DerivedKeys)> {
    let header_bytes: [u8; RAO_HEADER_LEN] = input
        .get(..RAO_HEADER_LEN)
        .ok_or(RaoAeadError::UnexpectedEof)?
        .try_into()
        .map_err(|_| RaoAeadError::UnexpectedEof)?;
    let header = RaoHeader::parse(&header_bytes)?;
    if header.format_version != 1 {
        return Err(RaoAeadError::KeyModeMismatch);
    }
    let keys = derive_keys(root_key, &header.hkdf_salt, &header.header_hash()?)?;
    let metadata_frame_len =
        usize::try_from(header.metadata_frame_len).map_err(|_| RaoAeadError::SizeOverflow)?;
    let metadata_start = RAO_HEADER_LEN;
    let metadata_end = metadata_start
        .checked_add(metadata_frame_len)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let metadata_frame = input
        .get(metadata_start..metadata_end)
        .ok_or(RaoAeadError::UnexpectedEof)?;
    let metadata_plaintext = decrypt_metadata(&keys.metadata_key, metadata_frame)?;
    let metadata = RaoMetadata::from_cbor_bytes(&metadata_plaintext, header.chunk_size)?;
    let expected_salt = derive_salt(
        root_key,
        &header.object_id_field()?,
        &metadata.plaintext_digest,
        &metadata_plaintext,
    )?;
    if expected_salt != header.hkdf_salt {
        return Err(RaoAeadError::SaltDerivationMismatch);
    }
    Ok((header, metadata, keys))
}

fn validate_range(start: u64, len: u64, plaintext_size: u64) -> Result<u64> {
    let end = start.checked_add(len).ok_or_else(|| {
        RaoAeadError::InvalidInput("plaintext range arithmetic overflow".to_string())
    })?;
    if len == 0 {
        if start > plaintext_size {
            return Err(RaoAeadError::InvalidInput(
                "empty plaintext range starts past object end".to_string(),
            ));
        }
        return Ok(end);
    }
    if end > plaintext_size {
        return Err(RaoAeadError::InvalidInput(
            "plaintext range extends past object end".to_string(),
        ));
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cipher_offset, open_to_vec, seal_to_vec, SealOptions};
    use sha2::{Digest, Sha256};

    fn sealed() -> (Vec<u8>, Vec<u8>, RootKey, SealOptions) {
        let root = RootKey::new([0x11; 32]).unwrap();
        let plaintext: Vec<u8> = (0..1536).map(|i| (i % 251) as u8).collect();
        let digest = Sha256::digest(&plaintext);
        let mut plaintext_digest = [0u8; 32];
        plaintext_digest.copy_from_slice(&digest);
        let options = SealOptions {
            chunk_size: 512,
            key_id: [0x10; 16],
            object_id: "object-1".to_string(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest,
        };
        let sealed = seal_to_vec(&plaintext, &root, &options).unwrap().0;
        (sealed, plaintext, root, options)
    }

    #[test]
    fn plaintext_range_fetches_authenticates_and_trims_ciphertext_chunks() {
        let (sealed, plaintext, root, options) = sealed();
        let (range, report) = open_plaintext_range_to_vec(&sealed, &root, 400, 700).unwrap();

        assert_eq!(range, plaintext[400..1100]);
        assert_eq!(report.plaintext_start, 400);
        assert_eq!(report.plaintext_len, 700);
        assert_eq!(report.first_chunk, Some(0));
        assert_eq!(report.chunk_count, 3);
        assert_eq!(
            report.stored_range_start,
            Some(cipher_offset(report.header.metadata_frame_len, options.chunk_size, 0).unwrap())
        );
        assert_eq!(
            report.stored_range_len,
            3 * (u64::from(options.chunk_size) + CHACHA20POLY1305_TAG_LEN)
        );
    }

    #[test]
    fn range_open_does_not_authenticate_unrequested_payload_chunks() {
        let (mut sealed, plaintext, root, options) = sealed();
        let report = crate::inspect_bytes(&sealed).unwrap();
        let chunk_two = cipher_offset(report.header.metadata_frame_len, options.chunk_size, 2)
            .unwrap() as usize;
        sealed[chunk_two] ^= 0x80;

        assert!(matches!(
            open_to_vec(&sealed, &root),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
        let (range, _report) = open_plaintext_range_to_vec(&sealed, &root, 0, 512).unwrap();
        assert_eq!(range, plaintext[..512]);
    }

    #[test]
    fn range_open_fails_closed_for_requested_chunk_authentication_failure() {
        let (mut sealed, _plaintext, root, options) = sealed();
        let report = crate::inspect_bytes(&sealed).unwrap();
        let chunk_one = cipher_offset(report.header.metadata_frame_len, options.chunk_size, 1)
            .unwrap() as usize;
        sealed[chunk_one] ^= 0x80;

        assert!(matches!(
            open_plaintext_range_to_vec(&sealed, &root, 512, 128),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
    }

    #[test]
    fn inner_range_maps_from_body_lba_space() {
        let (sealed, plaintext, root, _options) = sealed();
        let (range, report) = open_inner_range_to_vec(&sealed, &root, 1, 25, 100).unwrap();

        assert_eq!(range, plaintext[537..637]);
        assert_eq!(report.plaintext_start, 537);
        assert_eq!(report.first_chunk, Some(1));
        assert_eq!(report.chunk_count, 1);
    }
}
