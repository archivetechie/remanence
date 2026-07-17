//! Keyed partial-range opening for encrypted RAO envelopes.
//!
//! The functions here implement the Section 6 ciphertext mapping: decrypt the
//! authenticated metadata frame, map plaintext body chunks to stored
//! ciphertext ranges, authenticate every fetched chunk, and release only the
//! caller-requested plaintext bytes. This is per-frame fail-closed behavior,
//! not whole-object authentication: corruption in an unrelated payload frame
//! does not invalidate an otherwise authenticated returned range.

use crate::error::{RaoAeadError, Result};
use crate::header::{RaoHeader, RAO_HEADER_LEN};
use crate::metadata::RaoMetadata;
use crate::stream::{cipher_offset, decrypt_chunk, decrypt_metadata, CHACHA20POLY1305_TAG_LEN};
use std::io::{Read, Write};

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

/// Authenticated geometry for a requested plaintext range.
///
/// This is the query surface used by callers that must fetch only the stored
/// payload frames covering a plaintext member. The mapping is deliberately
/// produced here, beside [`cipher_offset`], so consumers never
/// duplicate the envelope geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoveringStoredRange {
    /// Parsed and authenticated envelope header.
    pub header: RaoHeader,
    /// Decrypted and authenticated envelope metadata.
    pub metadata: RaoMetadata,
    /// Requested absolute plaintext start.
    pub plaintext_start: u64,
    /// Requested plaintext length.
    pub plaintext_len: u64,
    /// First covering payload chunk, absent for an empty range.
    pub first_chunk: Option<u64>,
    /// Number of covering payload chunks.
    pub chunk_count: u64,
    /// Absolute stored offset of the first covering frame, absent when empty.
    pub stored_range_start: Option<u64>,
    /// Length of the contiguous covering stored range.
    pub stored_range_len: u64,
}

/// Open and authenticate a v2 envelope plaintext range with per-frame semantics.
pub fn open_plaintext_range_to_vec(
    input: &[u8],
    recipient: &crate::RecipientPrivateKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let (header, metadata, keys) = open_authenticated_metadata(input, recipient)?;
    open_plaintext_range_from_slice_with_context(
        input,
        header,
        metadata,
        keys,
        plaintext_start,
        plaintext_len,
    )
}

/// Open a v2 envelope range relative to an inner RAO body block.
pub fn open_inner_range_to_vec(
    input: &[u8],
    recipient: &crate::RecipientPrivateKey,
    first_inner_chunk: u64,
    range_start: u64,
    range_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let (header, metadata, keys) = open_authenticated_metadata(input, recipient)?;
    let absolute_start = first_inner_chunk
        .checked_mul(u64::from(header.chunk_size))
        .and_then(|value| value.checked_add(range_start))
        .ok_or(RaoAeadError::SizeOverflow)?;
    open_plaintext_range_from_slice_with_context(
        input,
        header,
        metadata,
        keys,
        absolute_start,
        range_len,
    )
}

/// Authenticate a v2 recipient prefix and return its covering stored frames.
pub fn covering_stored_range(
    authenticated_prefix: &[u8],
    recipient: &crate::RecipientPrivateKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<CoveringStoredRange> {
    let (header, metadata, _keys) = open_authenticated_metadata(authenticated_prefix, recipient)?;
    range_geometry(header, metadata, plaintext_start, plaintext_len)
}

/// Stream a v2 recipient-envelope plaintext range from a bounded ranged input.
pub fn open_plaintext_range_from_reader<R: Read + ?Sized, W: Write + ?Sized>(
    authenticated_prefix: &[u8],
    ranged_input: &mut R,
    stored_range_start: u64,
    output: &mut W,
    recipient: &crate::RecipientPrivateKey,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<RangeOpenReport> {
    let (header, metadata, keys) = open_authenticated_metadata(authenticated_prefix, recipient)?;
    open_plaintext_range_from_reader_with_context(
        ranged_input,
        stored_range_start,
        output,
        header,
        metadata,
        keys,
        plaintext_start,
        plaintext_len,
    )
}

fn open_plaintext_range_from_slice_with_context(
    input: &[u8],
    header: RaoHeader,
    metadata: RaoMetadata,
    keys: crate::kdf::DerivedKeys,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<(Vec<u8>, RangeOpenReport)> {
    let geometry = range_geometry(header, metadata, plaintext_start, plaintext_len)?;
    let Some(stored_range_start) = geometry.stored_range_start else {
        return Ok((Vec::new(), geometry.into()));
    };
    let stored_range_end = stored_range_start
        .checked_add(geometry.stored_range_len)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_start =
        usize::try_from(stored_range_start).map_err(|_| RaoAeadError::SizeOverflow)?;
    let stored_end = usize::try_from(stored_range_end).map_err(|_| RaoAeadError::SizeOverflow)?;
    let encrypted_range = input
        .get(stored_start..stored_end)
        .ok_or(RaoAeadError::UnexpectedEof)?;
    let mut ranged_input = std::io::Cursor::new(encrypted_range);
    let mut bytes = Vec::new();
    let report = open_plaintext_range_from_reader_with_context(
        &mut ranged_input,
        stored_range_start,
        &mut bytes,
        geometry.header,
        geometry.metadata,
        keys,
        plaintext_start,
        plaintext_len,
    )?;
    Ok((bytes, report))
}

fn range_geometry(
    header: RaoHeader,
    metadata: RaoMetadata,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<CoveringStoredRange> {
    let plaintext_end = validate_range(plaintext_start, plaintext_len, metadata.plaintext_size)?;
    if plaintext_len == 0 {
        return Ok(CoveringStoredRange {
            header,
            metadata,
            plaintext_start,
            plaintext_len,
            first_chunk: None,
            chunk_count: 0,
            stored_range_start: None,
            stored_range_len: 0,
        });
    }
    let chunk_size = u64::from(header.chunk_size);
    let first_chunk = plaintext_start / chunk_size;
    let last_chunk = (plaintext_end - 1) / chunk_size;
    let chunk_count = last_chunk
        .checked_sub(first_chunk)
        .and_then(|value| value.checked_add(1))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_chunk_len = chunk_size
        .checked_add(CHACHA20POLY1305_TAG_LEN)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_range_start = cipher_offset(
        header.key_frame_len,
        header.metadata_frame_len,
        header.chunk_size,
        first_chunk,
    )?;
    let stored_range_end = cipher_offset(
        header.key_frame_len,
        header.metadata_frame_len,
        header.chunk_size,
        last_chunk,
    )?
    .checked_add(stored_chunk_len)
    .ok_or(RaoAeadError::SizeOverflow)?;
    Ok(CoveringStoredRange {
        header,
        metadata,
        plaintext_start,
        plaintext_len,
        first_chunk: Some(first_chunk),
        chunk_count,
        stored_range_start: Some(stored_range_start),
        stored_range_len: stored_range_end
            .checked_sub(stored_range_start)
            .ok_or(RaoAeadError::SizeOverflow)?,
    })
}

#[allow(clippy::too_many_arguments)]
fn open_plaintext_range_from_reader_with_context<R: Read + ?Sized, W: Write + ?Sized>(
    ranged_input: &mut R,
    stored_range_start: u64,
    output: &mut W,
    header: RaoHeader,
    metadata: RaoMetadata,
    keys: crate::kdf::DerivedKeys,
    plaintext_start: u64,
    plaintext_len: u64,
) -> Result<RangeOpenReport> {
    let geometry = range_geometry(header, metadata, plaintext_start, plaintext_len)?;
    let Some(expected_stored_start) = geometry.stored_range_start else {
        return Ok(geometry.into());
    };
    if stored_range_start != expected_stored_start {
        return Err(RaoAeadError::InvalidInput(format!(
            "ranged input starts at stored byte {stored_range_start}, expected {expected_stored_start}"
        )));
    }

    let chunk_size = u64::from(geometry.header.chunk_size);
    let chunk_size_usize = usize::try_from(chunk_size).map_err(|_| RaoAeadError::SizeOverflow)?;
    let stored_chunk_len = usize::try_from(
        chunk_size
            .checked_add(CHACHA20POLY1305_TAG_LEN)
            .ok_or(RaoAeadError::SizeOverflow)?,
    )
    .map_err(|_| RaoAeadError::SizeOverflow)?;
    let object_chunk_count = geometry.metadata.plaintext_size / chunk_size;
    let first_chunk = geometry.first_chunk.ok_or(RaoAeadError::SizeOverflow)?;
    let requested_end = plaintext_start
        .checked_add(plaintext_len)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let mut encrypted = vec![0u8; stored_chunk_len];

    for offset in 0..geometry.chunk_count {
        ranged_input
            .read_exact(&mut encrypted)
            .map_err(crate::error::map_read_exact_error)?;
        let chunk_index = first_chunk
            .checked_add(offset)
            .ok_or(RaoAeadError::SizeOverflow)?;
        let plaintext = decrypt_chunk(
            &keys.payload_key,
            chunk_index,
            chunk_index + 1 == object_chunk_count,
            &encrypted,
        )?;
        if plaintext.len() != chunk_size_usize {
            return Err(RaoAeadError::AeadAuthenticationFailed);
        }
        let chunk_start = chunk_index
            .checked_mul(chunk_size)
            .ok_or(RaoAeadError::SizeOverflow)?;
        let chunk_end = chunk_start
            .checked_add(chunk_size)
            .ok_or(RaoAeadError::SizeOverflow)?;
        let selected_start = chunk_start.max(plaintext_start);
        let selected_end = chunk_end.min(requested_end);
        let local_start = usize::try_from(selected_start - chunk_start)
            .map_err(|_| RaoAeadError::SizeOverflow)?;
        let local_end =
            usize::try_from(selected_end - chunk_start).map_err(|_| RaoAeadError::SizeOverflow)?;
        output.write_all(&plaintext[local_start..local_end])?;
    }
    Ok(geometry.into())
}

impl From<CoveringStoredRange> for RangeOpenReport {
    fn from(value: CoveringStoredRange) -> Self {
        Self {
            header: value.header,
            metadata: value.metadata,
            plaintext_start: value.plaintext_start,
            plaintext_len: value.plaintext_len,
            first_chunk: value.first_chunk,
            chunk_count: value.chunk_count,
            stored_range_start: value.stored_range_start,
            stored_range_len: value.stored_range_len,
        }
    }
}

fn open_authenticated_metadata(
    input: &[u8],
    recipient: &crate::RecipientPrivateKey,
) -> Result<(RaoHeader, RaoMetadata, crate::kdf::DerivedKeys)> {
    let header_bytes: [u8; RAO_HEADER_LEN] = input
        .get(..RAO_HEADER_LEN)
        .ok_or(RaoAeadError::UnexpectedEof)?
        .try_into()
        .map_err(|_| RaoAeadError::UnexpectedEof)?;
    let header = RaoHeader::parse(&header_bytes)?;
    let key_frame_len =
        usize::try_from(header.key_frame_len).map_err(|_| RaoAeadError::SizeOverflow)?;
    let key_frame_end = RAO_HEADER_LEN
        .checked_add(key_frame_len)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let key_frame_bytes = input
        .get(RAO_HEADER_LEN..key_frame_end)
        .ok_or(RaoAeadError::UnexpectedEof)?;
    let key_frame = crate::KeyFrame::parse(key_frame_bytes)?;
    let dek = crate::unwrap_dek(&key_frame, &header.object_id, recipient)?;
    let keys = crate::derive_keys_v2(
        dek.as_bytes(),
        &header.hkdf_salt,
        &header.header_hash_with_key_frame(key_frame_bytes)?,
    )?;

    let metadata_frame_len =
        usize::try_from(header.metadata_frame_len).map_err(|_| RaoAeadError::SizeOverflow)?;
    let metadata_end = key_frame_end
        .checked_add(metadata_frame_len)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let metadata_frame = input
        .get(key_frame_end..metadata_end)
        .ok_or(RaoAeadError::UnexpectedEof)?;
    let metadata_plaintext = decrypt_metadata(&keys.metadata_key, metadata_frame)?;
    let metadata = RaoMetadata::from_cbor_bytes(&metadata_plaintext, header.chunk_size)?;
    let expected_salt = crate::derive_salt_v2(
        dek.as_bytes(),
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
    use crate::{
        cipher_offset, open_to_vec, seal_to_vec, EnvelopeSealOptions, RecipientPrivateKey,
        SealOptions,
    };
    use sha2::{Digest, Sha256};
    use std::io::Read;

    fn prefix_len(header: &RaoHeader) -> usize {
        RAO_HEADER_LEN + header.key_frame_len as usize + header.metadata_frame_len as usize
    }

    struct CountingReader<'a> {
        inner: &'a [u8],
        bytes_read: usize,
        largest_read: usize,
    }

    impl<'a> CountingReader<'a> {
        fn new(inner: &'a [u8]) -> Self {
            Self {
                inner,
                bytes_read: 0,
                largest_read: 0,
            }
        }
    }

    impl Read for CountingReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.largest_read = self.largest_read.max(buf.len());
            let count = buf.len().min(self.inner.len());
            buf[..count].copy_from_slice(&self.inner[..count]);
            self.inner = &self.inner[count..];
            self.bytes_read += count;
            Ok(count)
        }
    }

    fn sealed() -> (Vec<u8>, Vec<u8>, RecipientPrivateKey, SealOptions) {
        let plaintext: Vec<u8> = (0..1536).map(|i| (i % 251) as u8).collect();
        let common = SealOptions {
            chunk_size: 512,
            object_id: "object-v2-range".to_string(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest: Sha256::digest(&plaintext).into(),
        };
        let safe = RecipientPrivateKey::new([1; 16], "safe", [7; 32]).unwrap();
        let escrow = RecipientPrivateKey::new([2; 16], "escrow", [8; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common: common.clone(),
            recipients: vec![safe.public_key(0).unwrap(), escrow.public_key(1).unwrap()],
        };
        let sealed = seal_to_vec(&plaintext, &options).unwrap().0;
        (sealed, plaintext, safe, common)
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
            Some(
                cipher_offset(
                    report.header.key_frame_len,
                    report.header.metadata_frame_len,
                    options.chunk_size,
                    0,
                )
                .unwrap()
            )
        );
        assert_eq!(
            report.stored_range_len,
            3 * (u64::from(options.chunk_size) + CHACHA20POLY1305_TAG_LEN)
        );
    }

    #[test]
    fn reader_range_matches_whole_object_trim_and_reads_only_covering_frames() {
        let (sealed, plaintext, root, _) = sealed();
        let inspected = crate::inspect_bytes(&sealed).unwrap();
        let prefix = &sealed[..prefix_len(&inspected.header)];
        let plan = covering_stored_range(prefix, &root, 400, 700).unwrap();
        let start = plan.stored_range_start.unwrap() as usize;
        let end = start + plan.stored_range_len as usize;
        let mut input = CountingReader::new(&sealed[start..end]);
        let mut output = Vec::new();

        let report = open_plaintext_range_from_reader(
            prefix,
            &mut input,
            start as u64,
            &mut output,
            &root,
            400,
            700,
        )
        .unwrap();

        assert_eq!(output, plaintext[400..1100]);
        assert_eq!(input.bytes_read as u64, plan.stored_range_len);
        assert_eq!(input.largest_read, 512 + 16);
        assert_eq!(report.chunk_count, 3);
    }

    #[test]
    fn recipient_reader_range_matches_whole_object_trim() {
        let (sealed, plaintext, recipient, _) = sealed();
        let inspected = crate::inspect_bytes(&sealed).unwrap();
        let prefix = &sealed[..prefix_len(&inspected.header)];
        let plan = covering_stored_range(prefix, &recipient, 500, 600).unwrap();
        let start = plan.stored_range_start.unwrap() as usize;
        let end = start + plan.stored_range_len as usize;
        let mut input = CountingReader::new(&sealed[start..end]);
        let mut output = Vec::new();

        open_plaintext_range_from_reader(
            prefix,
            &mut input,
            start as u64,
            &mut output,
            &recipient,
            500,
            600,
        )
        .unwrap();

        assert_eq!(output, plaintext[500..1100]);
        assert_eq!(input.bytes_read as u64, plan.stored_range_len);
    }

    #[test]
    fn reader_range_rejects_tampered_prefix_and_covering_chunk() {
        let (sealed, _plaintext, root, _) = sealed();
        let inspected = crate::inspect_bytes(&sealed).unwrap();
        let prefix_end = prefix_len(&inspected.header);
        let mut bad_prefix = sealed[..prefix_end].to_vec();
        bad_prefix[0x20] ^= 0x80;
        assert!(matches!(
            covering_stored_range(&bad_prefix, &root, 512, 64),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));

        let prefix = &sealed[..prefix_end];
        let plan = covering_stored_range(prefix, &root, 512, 64).unwrap();
        let start = plan.stored_range_start.unwrap() as usize;
        let end = start + plan.stored_range_len as usize;
        let mut bad_chunk = sealed[start..end].to_vec();
        bad_chunk[7] ^= 0x40;
        let mut output = Vec::new();
        assert!(matches!(
            open_plaintext_range_from_reader(
                prefix,
                &mut bad_chunk.as_slice(),
                start as u64,
                &mut output,
                &root,
                512,
                64,
            ),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn independent_member_ranges_read_sum_of_covering_ranges_not_n_times_object() {
        let (sealed, plaintext, root, _) = sealed();
        let inspected = crate::inspect_bytes(&sealed).unwrap();
        let prefix = &sealed[..prefix_len(&inspected.header)];
        let members = [(17, 31), (600, 40), (1400, 100)];
        let mut total_read = 0usize;
        let mut total_planned = 0u64;

        for (start, len) in members {
            let plan = covering_stored_range(prefix, &root, start, len).unwrap();
            let stored_start = plan.stored_range_start.unwrap() as usize;
            let stored_end = stored_start + plan.stored_range_len as usize;
            let mut input = CountingReader::new(&sealed[stored_start..stored_end]);
            let mut output = Vec::new();
            open_plaintext_range_from_reader(
                prefix,
                &mut input,
                stored_start as u64,
                &mut output,
                &root,
                start,
                len,
            )
            .unwrap();
            assert_eq!(output, plaintext[start as usize..(start + len) as usize]);
            total_read += input.bytes_read;
            total_planned += plan.stored_range_len;
        }

        assert_eq!(total_read as u64, total_planned);
        assert!(total_read < sealed.len());
        assert!(total_read < members.len() * sealed.len());
    }

    #[test]
    fn range_open_does_not_authenticate_unrequested_payload_chunks() {
        let (mut sealed, plaintext, root, options) = sealed();
        let report = crate::inspect_bytes(&sealed).unwrap();
        let chunk_two = cipher_offset(
            report.header.key_frame_len,
            report.header.metadata_frame_len,
            options.chunk_size,
            2,
        )
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
        let chunk_one = cipher_offset(
            report.header.key_frame_len,
            report.header.metadata_frame_len,
            options.chunk_size,
            1,
        )
        .unwrap() as usize;
        sealed[chunk_one] ^= 0x80;

        assert!(matches!(
            open_plaintext_range_to_vec(&sealed, &root, 512, 128),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
    }

    #[test]
    fn v2_range_authenticates_returned_frames_but_not_unrequested_payload() {
        let (sealed, plaintext, safe, _) = sealed();
        let inspected = crate::inspect_bytes(&sealed).unwrap();
        let chunk_zero = cipher_offset(
            inspected.header.key_frame_len,
            inspected.header.metadata_frame_len,
            inspected.header.chunk_size,
            0,
        )
        .unwrap() as usize;
        let chunk_two = cipher_offset(
            inspected.header.key_frame_len,
            inspected.header.metadata_frame_len,
            inspected.header.chunk_size,
            2,
        )
        .unwrap() as usize;

        let mut requested_tamper = sealed.clone();
        requested_tamper[chunk_zero] ^= 0x80;
        assert!(matches!(
            open_plaintext_range_to_vec(&requested_tamper, &safe, 0, 128),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));

        let mut unrelated_tamper = sealed;
        unrelated_tamper[chunk_two] ^= 0x80;
        let (range, _) = open_plaintext_range_to_vec(&unrelated_tamper, &safe, 0, 128).unwrap();
        assert_eq!(range, plaintext[..128]);
    }

    #[test]
    fn v2_range_rejects_key_frame_tamper_outside_requested_payload() {
        let (mut sealed, _plaintext, safe, _) = sealed();
        sealed[RAO_HEADER_LEN + 6] ^= 0x80;
        assert!(open_plaintext_range_to_vec(&sealed, &safe, 0, 128).is_err());
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
