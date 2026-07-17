//! Partial-file restore helpers for `rao-v1` copies.
//!
//! This module bridges catalog file rows (`first_chunk_lba`, `size_bytes`) to
//! the plaintext block planner or RAO encrypted-envelope range opener. It
//! keeps file-range validation in the body-format layer while delegating
//! authentication and decryption to `remanence-aead`.

use remanence_aead::{
    open_inner_range_envelope_to_vec, open_inner_range_to_vec,
    open_plaintext_range_envelope_to_vec, RangeOpenReport, RecipientPrivateKey, RootKey,
};

use crate::error::FormatError;
use crate::model::BodyLba;

/// Bytes and envelope-range metadata returned by encrypted file PFR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedRaoFileRange {
    /// Requested plaintext file bytes.
    pub bytes: Vec<u8>,
    /// Authenticated envelope range report.
    pub envelope: RangeOpenReport,
}

/// Object-local block plan for plaintext RAO file PFR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaintextRaoFileRangePlan {
    /// First object-local body block to read.
    pub first_body_lba: BodyLba,
    /// Number of object blocks that cover the requested range.
    pub block_count: u64,
    /// Byte offset inside the first returned block.
    pub first_block_offset: u64,
    /// Number of caller-requested file bytes to return.
    pub range_len: u64,
}

/// Plan the object-local blocks covering a plaintext RAO member-file byte range.
///
/// `first_chunk_lba` and `file_size_bytes` are the per-file manifest/catalog
/// values. `range_start` and `range_len` address bytes within that member file.
/// Empty-but-valid ranges return `Ok(None)` because no object blocks need to be
/// read.
pub fn plan_plaintext_rao_file_range(
    first_chunk_lba: Option<BodyLba>,
    file_size_bytes: u64,
    chunk_size_bytes: u64,
    range_start: u64,
    range_len: u64,
) -> Result<Option<PlaintextRaoFileRangePlan>, FormatError> {
    if chunk_size_bytes == 0 {
        return Err(FormatError::invalid("chunk_size_bytes must be nonzero"));
    }
    validate_file_range(file_size_bytes, range_start, range_len)?;
    if range_len == 0 {
        return Ok(None);
    }
    let first_chunk_lba = first_chunk_lba.ok_or_else(|| {
        FormatError::invalid("non-empty plaintext file range requires first_chunk_lba")
    })?;
    let first_relative_block = range_start / chunk_size_bytes;
    let range_end = range_start
        .checked_add(range_len)
        .ok_or_else(|| FormatError::invalid("file range arithmetic overflow"))?;
    let last_relative_block = (range_end - 1) / chunk_size_bytes;
    let block_count = last_relative_block
        .checked_sub(first_relative_block)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| FormatError::invalid("file range block arithmetic overflow"))?;
    let first_body_lba = first_chunk_lba
        .0
        .checked_add(first_relative_block)
        .map(BodyLba)
        .ok_or_else(|| FormatError::invalid("file range body LBA overflow"))?;
    Ok(Some(PlaintextRaoFileRangePlan {
        first_body_lba,
        block_count,
        first_block_offset: range_start % chunk_size_bytes,
        range_len,
    }))
}

/// Read and authenticate a member-file byte range from an encrypted RAO object.
///
/// `first_chunk_lba` and `file_size_bytes` are the per-file row values from
/// the manifest or catalog. `range_start` and `range_len` address bytes within
/// that member file, not the whole canonical plaintext object.
pub fn read_encrypted_rao_file_range_to_vec(
    encrypted: &[u8],
    root_key: &RootKey,
    first_chunk_lba: Option<BodyLba>,
    file_size_bytes: u64,
    range_start: u64,
    range_len: u64,
) -> Result<EncryptedRaoFileRange, FormatError> {
    validate_file_range(file_size_bytes, range_start, range_len)?;
    if range_len == 0 {
        let (bytes, envelope) = if let Some(first_chunk_lba) = first_chunk_lba {
            open_inner_range_to_vec(encrypted, root_key, first_chunk_lba.0, range_start, 0)?
        } else {
            remanence_aead::open_plaintext_range_to_vec(encrypted, root_key, 0, 0)?
        };
        return Ok(EncryptedRaoFileRange { bytes, envelope });
    }
    let first_chunk_lba = first_chunk_lba.ok_or_else(|| {
        FormatError::invalid("non-empty encrypted file range requires first_chunk_lba")
    })?;
    let (bytes, envelope) = open_inner_range_to_vec(
        encrypted,
        root_key,
        first_chunk_lba.0,
        range_start,
        range_len,
    )?;
    Ok(EncryptedRaoFileRange { bytes, envelope })
}

/// Read and authenticate a member-file range from a v2 recipient envelope.
pub fn read_envelope_rao_file_range_to_vec(
    encrypted: &[u8],
    recipient: &RecipientPrivateKey,
    first_chunk_lba: Option<BodyLba>,
    file_size_bytes: u64,
    range_start: u64,
    range_len: u64,
) -> Result<EncryptedRaoFileRange, FormatError> {
    validate_file_range(file_size_bytes, range_start, range_len)?;
    if range_len == 0 {
        let (bytes, envelope) = if let Some(first_chunk_lba) = first_chunk_lba {
            open_inner_range_envelope_to_vec(
                encrypted,
                recipient,
                first_chunk_lba.0,
                range_start,
                0,
            )?
        } else {
            open_plaintext_range_envelope_to_vec(encrypted, recipient, 0, 0)?
        };
        return Ok(EncryptedRaoFileRange { bytes, envelope });
    }
    let first_chunk_lba = first_chunk_lba.ok_or_else(|| {
        FormatError::invalid("non-empty encrypted file range requires first_chunk_lba")
    })?;
    let (bytes, envelope) = open_inner_range_envelope_to_vec(
        encrypted,
        recipient,
        first_chunk_lba.0,
        range_start,
        range_len,
    )?;
    Ok(EncryptedRaoFileRange { bytes, envelope })
}

/// Validate a member-file byte range.
pub fn validate_file_range(
    file_size_bytes: u64,
    range_start: u64,
    range_len: u64,
) -> Result<(), FormatError> {
    let range_end = range_start
        .checked_add(range_len)
        .ok_or_else(|| FormatError::invalid("file range arithmetic overflow"))?;
    if range_len == 0 {
        if range_start > file_size_bytes {
            return Err(FormatError::invalid(
                "empty file range starts past file end",
            ));
        }
        return Ok(());
    }
    if range_end > file_size_bytes {
        return Err(FormatError::invalid("file range extends past file end"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use remanence_aead::{cipher_offset, open_to_vec, RaoAeadError, CHACHA20POLY1305_TAG_LEN};
    use remanence_library::VecBlockSink;

    use super::*;
    use crate::{write_encrypted_rao_object, RemTarFile, RemTarObjectOptions};

    fn encrypted_object() -> (Vec<u8>, Vec<u8>, RootKey, Option<BodyLba>, u64, u64) {
        let mut opts = RemTarObjectOptions::new(
            "55555555-5555-5555-5555-555555555555",
            "caller-pfr",
            "2026-05-27T22:10:00+05:30",
            "66666666-6666-6666-6666-666666666666",
        );
        opts.chunk_size = 512;
        let payload: Vec<u8> = (0..1400).map(|i| (i % 251) as u8).collect();
        let files = [RemTarFile {
            path: "secret.bin",
            file_id: "file-secret",
            data: &payload,
            mtime: None,
            executable: Some(false),
        }];
        let root_key = RootKey::new([0x42; 32]).unwrap();
        let mut sink = VecBlockSink::new();
        let report =
            write_encrypted_rao_object(&mut sink, &opts, &files, &root_key, [0x24; 16]).unwrap();
        let first_chunk_lba = report.plaintext_layout.files[0].first_chunk_lba;
        let metadata_frame_len = report.envelope.header.metadata_frame_len;
        let encrypted = sink.blocks.iter().flatten().copied().collect();
        (
            encrypted,
            payload,
            root_key,
            first_chunk_lba,
            opts.chunk_size as u64,
            metadata_frame_len,
        )
    }

    #[test]
    fn encrypted_file_range_maps_body_lba_and_trims_payload_bytes() {
        let (encrypted, payload, root_key, first_chunk_lba, chunk_size, _metadata_len) =
            encrypted_object();

        let range = read_encrypted_rao_file_range_to_vec(
            &encrypted,
            &root_key,
            first_chunk_lba,
            payload.len() as u64,
            400,
            500,
        )
        .unwrap();

        let first_chunk_lba = first_chunk_lba.unwrap();
        assert_eq!(range.bytes, payload[400..900]);
        assert_eq!(
            range.envelope.plaintext_start,
            first_chunk_lba.0 * chunk_size + 400
        );
        assert_eq!(range.envelope.plaintext_len, 500);
        assert_eq!(range.envelope.first_chunk, Some(first_chunk_lba.0));
        assert_eq!(range.envelope.chunk_count, 2);
    }

    #[test]
    fn encrypted_file_pfr_fetches_boundary_and_final_chunk_ciphertext_ranges() {
        let (encrypted, payload, root_key, first_chunk_lba, chunk_size, metadata_len) =
            encrypted_object();
        let first_chunk_lba = first_chunk_lba.unwrap();
        let chunk_size_u32 = u32::try_from(chunk_size).unwrap();
        let stored_chunk_len = chunk_size + CHACHA20POLY1305_TAG_LEN;

        let boundary = read_encrypted_rao_file_range_to_vec(
            &encrypted,
            &root_key,
            Some(first_chunk_lba),
            payload.len() as u64,
            400,
            500,
        )
        .unwrap();

        assert_eq!(boundary.bytes, payload[400..900]);
        assert_eq!(boundary.envelope.first_chunk, Some(first_chunk_lba.0));
        assert_eq!(boundary.envelope.chunk_count, 2);
        assert_eq!(
            boundary.envelope.stored_range_start,
            Some(cipher_offset(metadata_len, chunk_size_u32, first_chunk_lba.0).unwrap())
        );
        assert_eq!(boundary.envelope.stored_range_len, 2 * stored_chunk_len);

        let final_range_len = 100u64;
        let final_range_start = payload.len() as u64 - final_range_len;
        let final_chunk_range = read_encrypted_rao_file_range_to_vec(
            &encrypted,
            &root_key,
            Some(first_chunk_lba),
            payload.len() as u64,
            final_range_start,
            final_range_len,
        )
        .unwrap();
        let final_file_chunk = first_chunk_lba.0 + final_range_start / chunk_size;

        assert_eq!(
            final_file_chunk,
            first_chunk_lba.0 + (payload.len() as u64 - 1) / chunk_size
        );
        assert_eq!(
            final_chunk_range.bytes,
            payload[usize::try_from(final_range_start).unwrap()..]
        );
        assert_eq!(
            final_chunk_range.envelope.first_chunk,
            Some(final_file_chunk)
        );
        assert_eq!(final_chunk_range.envelope.chunk_count, 1);
        assert_eq!(
            final_chunk_range.envelope.stored_range_start,
            Some(cipher_offset(metadata_len, chunk_size_u32, final_file_chunk).unwrap())
        );
        assert_eq!(
            final_chunk_range.envelope.stored_range_len,
            stored_chunk_len
        );
    }

    #[test]
    fn encrypted_file_range_does_not_authenticate_unrequested_file_chunks() {
        let (mut encrypted, payload, root_key, first_chunk_lba, _chunk_size, metadata_len) =
            encrypted_object();
        let first_chunk_lba = first_chunk_lba.unwrap();
        let unrequested_chunk_offset =
            cipher_offset(metadata_len, 512, first_chunk_lba.0 + 2).unwrap() as usize;
        encrypted[unrequested_chunk_offset] ^= 0x40;

        assert!(matches!(
            open_to_vec(&encrypted, &root_key),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
        let range = read_encrypted_rao_file_range_to_vec(
            &encrypted,
            &root_key,
            Some(first_chunk_lba),
            payload.len() as u64,
            400,
            500,
        )
        .unwrap();
        assert_eq!(range.bytes, payload[400..900]);
    }

    #[test]
    fn encrypted_file_range_fails_closed_for_requested_chunk_damage() {
        let (mut encrypted, payload, root_key, first_chunk_lba, _chunk_size, metadata_len) =
            encrypted_object();
        let first_chunk_lba = first_chunk_lba.unwrap();
        let requested_chunk_offset =
            cipher_offset(metadata_len, 512, first_chunk_lba.0 + 1).unwrap() as usize;
        encrypted[requested_chunk_offset] ^= 0x40;

        let err = read_encrypted_rao_file_range_to_vec(
            &encrypted,
            &root_key,
            Some(first_chunk_lba),
            payload.len() as u64,
            400,
            500,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            FormatError::Aead(RaoAeadError::AeadAuthenticationFailed)
        ));
    }

    #[test]
    fn empty_encrypted_file_range_authenticates_metadata_without_first_lba() {
        let (encrypted, _payload, root_key, _first_chunk_lba, _chunk_size, _metadata_len) =
            encrypted_object();

        let range =
            read_encrypted_rao_file_range_to_vec(&encrypted, &root_key, None, 0, 0, 0).unwrap();

        assert!(range.bytes.is_empty());
        assert_eq!(range.envelope.plaintext_len, 0);
        assert_eq!(range.envelope.chunk_count, 0);
        assert_eq!(range.envelope.first_chunk, None);
    }

    #[test]
    fn zero_length_range_inside_nonempty_file_reports_file_offset() {
        let (encrypted, payload, root_key, first_chunk_lba, chunk_size, _metadata_len) =
            encrypted_object();
        let first_chunk_lba = first_chunk_lba.unwrap();

        let range = read_encrypted_rao_file_range_to_vec(
            &encrypted,
            &root_key,
            Some(first_chunk_lba),
            payload.len() as u64,
            700,
            0,
        )
        .unwrap();

        assert!(range.bytes.is_empty());
        assert_eq!(
            range.envelope.plaintext_start,
            first_chunk_lba.0 * chunk_size + 700
        );
        assert_eq!(range.envelope.plaintext_len, 0);
        assert_eq!(range.envelope.first_chunk, None);
    }

    #[test]
    fn plaintext_file_range_plan_maps_span_to_body_blocks() {
        let plan = plan_plaintext_rao_file_range(Some(BodyLba(8)), 1400, 512, 400, 500).unwrap();

        assert_eq!(
            plan,
            Some(PlaintextRaoFileRangePlan {
                first_body_lba: BodyLba(8),
                block_count: 2,
                first_block_offset: 400,
                range_len: 500,
            })
        );
    }

    #[test]
    fn plaintext_file_range_plan_skips_blocks_before_range() {
        let plan = plan_plaintext_rao_file_range(Some(BodyLba(8)), 1400, 512, 800, 300).unwrap();

        assert_eq!(
            plan,
            Some(PlaintextRaoFileRangePlan {
                first_body_lba: BodyLba(9),
                block_count: 2,
                first_block_offset: 288,
                range_len: 300,
            })
        );
    }

    #[test]
    fn plaintext_file_range_plan_accepts_empty_range_without_lba() {
        assert_eq!(
            plan_plaintext_rao_file_range(None, 0, 512, 0, 0).unwrap(),
            None
        );
    }

    #[test]
    fn plaintext_file_range_plan_rejects_past_eof_range() {
        let err =
            plan_plaintext_rao_file_range(Some(BodyLba(8)), 1400, 512, 1300, 101).unwrap_err();

        assert!(matches!(err, FormatError::InvalidInput(_)));
    }
}
