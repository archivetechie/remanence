//! Whole-object keyed opening for encrypted RAO envelopes.

use std::io::{Read, Write};

use sha2::{Digest, Sha256};

use crate::error::{RaoAeadError, Result};
use crate::header::{validate_metadata_frame_len, RaoHeader, RAO_FOOTER, RAO_HEADER_LEN};
use crate::metadata::RaoMetadata;
use crate::stream::{
    decrypt_chunk, decrypt_metadata, finalize_sha256, payload_frame_len, round_up, PlaintextStats,
    CHACHA20POLY1305_TAG_LEN,
};
use crate::wrap::{unwrap_dek, RecipientPrivateKey};

/// Report returned after successfully opening a RAO encrypted object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenReport {
    /// Parsed plaintext header.
    pub header: RaoHeader,
    /// Parsed v2 key frame.
    pub key_frame: crate::KeyFrame,
    /// Decrypted metadata.
    pub metadata: RaoMetadata,
    /// Stored object byte length consumed.
    pub stored_size_bytes: u64,
    /// Plaintext stats observed while decrypting.
    pub plaintext: PlaintextStats,
}

/// Open a v2 envelope object with a matching recipient private key.
pub fn open<R: Read, W: Write>(
    mut input: R,
    mut output: W,
    recipient: &RecipientPrivateKey,
) -> Result<OpenReport> {
    let mut header_bytes = [0u8; RAO_HEADER_LEN];
    read_exact(&mut input, &mut header_bytes)?;
    let header = RaoHeader::parse(&header_bytes)?;
    validate_metadata_frame_len(header.metadata_frame_len)?;
    let mut key_frame_bytes = vec![0u8; header.key_frame_len as usize];
    read_exact(&mut input, &mut key_frame_bytes)?;
    let key_frame = crate::KeyFrame::parse(&key_frame_bytes)?;
    let dek = unwrap_dek(&key_frame, &header.object_id, recipient)?;
    let keys = crate::kdf::derive_keys_v2(
        dek.as_bytes(),
        &header.hkdf_salt,
        &header.header_hash_with_key_frame(&key_frame_bytes)?,
    )?;
    let metadata_frame_len = usize::try_from(header.metadata_frame_len)
        .map_err(|_| RaoAeadError::MetadataFrameLengthInvalid)?;
    let mut metadata_frame = vec![0u8; metadata_frame_len];
    read_exact(&mut input, &mut metadata_frame)?;
    let metadata_plaintext = decrypt_metadata(&keys.metadata_key, &metadata_frame)?;
    let metadata = RaoMetadata::from_cbor_bytes(&metadata_plaintext, header.chunk_size)?;
    let expected_salt = crate::kdf::derive_salt_v2(
        dek.as_bytes(),
        &header.object_id_field()?,
        &metadata.plaintext_digest,
        &metadata_plaintext,
    )?;
    if expected_salt != header.hkdf_salt {
        return Err(RaoAeadError::SaltDerivationMismatch);
    }
    let plaintext_stats = decrypt_payload(&mut input, &mut output, &header, &metadata, &keys)?;
    if plaintext_stats.size != metadata.plaintext_size {
        return Err(RaoAeadError::PlaintextSizeMismatch);
    }
    if plaintext_stats.digest != metadata.plaintext_digest {
        return Err(RaoAeadError::PlaintextDigestMismatch);
    }
    read_footer(&mut input)?;
    let payload_len = payload_frame_len(metadata.plaintext_size, header.chunk_size)?;
    let footer_end = (RAO_HEADER_LEN as u64)
        .checked_add(u64::from(header.key_frame_len))
        .and_then(|value| value.checked_add(header.metadata_frame_len))
        .and_then(|value| value.checked_add(payload_len))
        .and_then(|value| value.checked_add(RAO_FOOTER.len() as u64))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let stored_size_bytes = round_up(footer_end, u64::from(header.chunk_size))?;
    read_zero_fill(
        &mut input,
        stored_size_bytes
            .checked_sub(footer_end)
            .ok_or(RaoAeadError::SizeOverflow)?,
    )?;
    ensure_eof(&mut input)?;
    Ok(OpenReport {
        header,
        key_frame,
        metadata,
        stored_size_bytes,
        plaintext: plaintext_stats,
    })
}

/// Open a v2 envelope into a vector.
pub fn open_to_vec(input: &[u8], recipient: &RecipientPrivateKey) -> Result<(Vec<u8>, OpenReport)> {
    let mut out = Vec::new();
    let report = open(input, &mut out, recipient)?;
    Ok((out, report))
}

fn decrypt_payload<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    header: &RaoHeader,
    metadata: &RaoMetadata,
    keys: &crate::kdf::DerivedKeys,
) -> Result<PlaintextStats> {
    let chunk_count = metadata.plaintext_size / u64::from(header.chunk_size);
    let stored_chunk_len = usize::try_from(
        u64::from(header.chunk_size)
            .checked_add(CHACHA20POLY1305_TAG_LEN)
            .ok_or(RaoAeadError::SizeOverflow)?,
    )
    .map_err(|_| RaoAeadError::SizeOverflow)?;
    let mut encrypted = vec![0u8; stored_chunk_len];
    let mut hasher = Sha256::new();
    let mut count = 0u64;

    for index in 0..chunk_count {
        read_exact_missing_final(input, &mut encrypted)?;
        let final_chunk = index + 1 == chunk_count;
        let plaintext = decrypt_chunk(&keys.payload_key, index, final_chunk, &encrypted)?;
        if plaintext.len() != header.chunk_size as usize {
            return Err(RaoAeadError::AeadAuthenticationFailed);
        }
        output.write_all(&plaintext)?;
        hasher.update(&plaintext);
        count = count
            .checked_add(u64::from(header.chunk_size))
            .ok_or(RaoAeadError::PlaintextSizeMismatch)?;
    }

    Ok(PlaintextStats {
        size: count,
        digest: finalize_sha256(hasher),
    })
}

fn read_footer<R: Read>(input: &mut R) -> Result<()> {
    let mut footer = [0u8; RAO_FOOTER.len()];
    read_exact(input, &mut footer)?;
    if &footer != RAO_FOOTER {
        return Err(RaoAeadError::InvalidFooter);
    }
    Ok(())
}

fn read_zero_fill<R: Read>(input: &mut R, fill_len: u64) -> Result<()> {
    let mut remaining = fill_len;
    let mut buf = [0u8; 8192];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        read_exact(input, &mut buf[..take])?;
        if buf[..take].iter().any(|byte| *byte != 0) {
            return Err(RaoAeadError::FillNotZero);
        }
        remaining -= take as u64;
    }
    Ok(())
}

fn ensure_eof<R: Read>(input: &mut R) -> Result<()> {
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(()),
            Ok(_) => return Err(RaoAeadError::TrailingData),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) => return Err(RaoAeadError::Io(err)),
        }
    }
}

fn read_exact<R: Read>(input: &mut R, buf: &mut [u8]) -> Result<()> {
    input
        .read_exact(buf)
        .map_err(crate::error::map_read_exact_error)
}

fn read_exact_missing_final<R: Read>(input: &mut R, buf: &mut [u8]) -> Result<()> {
    input.read_exact(buf).map_err(|err| {
        if err.kind() == std::io::ErrorKind::UnexpectedEof {
            RaoAeadError::MissingFinalChunk
        } else {
            RaoAeadError::Io(err)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{seal_to_vec, EnvelopeSealOptions, RecipientPrivateKey, SealOptions};

    fn sealed() -> (Vec<u8>, RecipientPrivateKey) {
        let plaintext = vec![0x5a; 1024];
        let primary = RecipientPrivateKey::new([1; 16], "primary", [7; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([2; 16], "recovery", [8; 32]).unwrap();
        let options = EnvelopeSealOptions {
            common: SealOptions {
                chunk_size: 512,
                object_id: "object-1".to_string(),
                plaintext_size: plaintext.len() as u64,
                plaintext_digest: Sha256::digest(&plaintext).into(),
            },
            recipients: vec![
                primary.public_key(0).unwrap(),
                recovery.public_key(1).unwrap(),
            ],
        };
        (seal_to_vec(&plaintext, &options).unwrap().0, primary)
    }

    #[test]
    fn wrong_key_fails_closed() {
        let (sealed, _primary) = sealed();
        let wrong = RecipientPrivateKey::new([3; 16], "wrong", [9; 32]).unwrap();
        assert!(matches!(
            open_to_vec(&sealed, &wrong),
            Err(RaoAeadError::RecipientEpochMismatch)
        ));
    }

    #[test]
    fn missing_footer_is_incomplete() {
        let (mut sealed, primary) = sealed();
        sealed.truncate(sealed.len() - 512);
        assert!(matches!(
            open_to_vec(&sealed, &primary),
            Err(RaoAeadError::MissingFinalChunk | RaoAeadError::InvalidFooter)
        ));
    }
}
