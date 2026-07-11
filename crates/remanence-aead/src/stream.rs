//! ChaCha20-Poly1305 STREAM helpers for RAO payload and metadata frames.

use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use sha2::{Digest, Sha256};

use crate::error::{RaoAeadError, Result};
use crate::header::{RAO_FOOTER, RAO_HEADER_LEN};

/// ChaCha20-Poly1305 authentication tag length.
pub const CHACHA20POLY1305_TAG_LEN: u64 = 16;

/// Size and digest of plaintext processed by the STREAM layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaintextStats {
    /// Number of plaintext bytes processed.
    pub size: u64,
    /// SHA-256 over exactly the processed plaintext bytes.
    pub digest: [u8; 32],
}

/// Encrypt one metadata frame with zero nonce and empty AAD.
pub fn encrypt_metadata(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>> {
    cipher_from_key(key)
        .encrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: plaintext,
                aad: &[],
            },
        )
        .map_err(|_| RaoAeadError::AeadAuthenticationFailed)
}

/// Decrypt one metadata frame with zero nonce and empty AAD.
pub fn decrypt_metadata(key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>> {
    cipher_from_key(key)
        .decrypt(
            Nonce::from_slice(&[0u8; 12]),
            Payload {
                msg: ciphertext,
                aad: &[],
            },
        )
        .map_err(|_| RaoAeadError::AeadAuthenticationFailed)
}

/// Encrypt one full RAO STREAM payload chunk.
pub fn encrypt_chunk(
    key: &[u8; 32],
    counter: u64,
    final_chunk: bool,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    cipher_from_key(key)
        .encrypt(
            Nonce::from_slice(&stream_nonce(counter, final_chunk)),
            Payload {
                msg: plaintext,
                aad: &[],
            },
        )
        .map_err(|_| RaoAeadError::AeadAuthenticationFailed)
}

/// Decrypt one full RAO STREAM payload chunk.
pub fn decrypt_chunk(
    key: &[u8; 32],
    counter: u64,
    final_chunk: bool,
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    cipher_from_key(key)
        .decrypt(
            Nonce::from_slice(&stream_nonce(counter, final_chunk)),
            Payload {
                msg: ciphertext,
                aad: &[],
            },
        )
        .map_err(|_| RaoAeadError::AeadAuthenticationFailed)
}

/// RAO STREAM nonce: 11-byte big-endian counter plus final flag.
pub fn stream_nonce(counter: u64, final_chunk: bool) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[3..11].copy_from_slice(&counter.to_be_bytes());
    nonce[11] = u8::from(final_chunk);
    nonce
}

/// Return `P / C`, rejecting zero or non-multiple plaintext sizes.
pub fn chunk_count(plaintext_size: u64, chunk_size: u32) -> Result<u64> {
    let chunk = u64::from(chunk_size);
    if chunk == 0 || plaintext_size == 0 || plaintext_size % chunk != 0 {
        return Err(RaoAeadError::InvalidMetadataField);
    }
    Ok(plaintext_size / chunk)
}

/// Payload ciphertext frame length for `plaintext_size` and `chunk_size`.
pub fn payload_frame_len(plaintext_size: u64, chunk_size: u32) -> Result<u64> {
    let chunks = chunk_count(plaintext_size, chunk_size)?;
    plaintext_size
        .checked_add(
            CHACHA20POLY1305_TAG_LEN
                .checked_mul(chunks)
                .ok_or(RaoAeadError::SizeOverflow)?,
        )
        .ok_or(RaoAeadError::SizeOverflow)
}

/// Stored object size after footer and zero fill.
pub fn stored_size_from_parts(
    chunk_size: u32,
    metadata_frame_len: u64,
    plaintext_size: u64,
) -> Result<u64> {
    stored_size_from_parts_with_key_frame(chunk_size, 0, metadata_frame_len, plaintext_size)
}

/// Compute padded stored size with a variable plaintext key frame.
pub fn stored_size_from_parts_with_key_frame(
    chunk_size: u32,
    key_frame_len: u32,
    metadata_frame_len: u64,
    plaintext_size: u64,
) -> Result<u64> {
    let payload_len = payload_frame_len(plaintext_size, chunk_size)?;
    let footer_end = (RAO_HEADER_LEN as u64)
        .checked_add(u64::from(key_frame_len))
        .and_then(|value| value.checked_add(metadata_frame_len))
        .and_then(|value| value.checked_add(payload_len))
        .and_then(|value| value.checked_add(RAO_FOOTER.len() as u64))
        .ok_or(RaoAeadError::SizeOverflow)?;
    round_up(footer_end, u64::from(chunk_size))
}

/// Stored object size for a metadata frame plus plaintext size.
pub fn expected_stored_size(
    chunk_size: u32,
    metadata_frame_len: u64,
    plaintext_size: u64,
) -> Result<u64> {
    stored_size_from_parts(chunk_size, metadata_frame_len, plaintext_size)
}

/// Ciphertext offset of inner body block `b`.
pub fn cipher_offset(metadata_frame_len: u64, chunk_size: u32, b: u64) -> Result<u64> {
    cipher_offset_with_key_frame(0, metadata_frame_len, chunk_size, b)
}

/// Stored offset of payload block `b` with an explicit key-frame length.
pub fn cipher_offset_with_key_frame(
    key_frame_len: u32,
    metadata_frame_len: u64,
    chunk_size: u32,
    b: u64,
) -> Result<u64> {
    let stride = u64::from(chunk_size)
        .checked_add(CHACHA20POLY1305_TAG_LEN)
        .ok_or(RaoAeadError::SizeOverflow)?;
    (RAO_HEADER_LEN as u64)
        .checked_add(u64::from(key_frame_len))
        .and_then(|value| value.checked_add(metadata_frame_len))
        .and_then(|base| base.checked_add(b.checked_mul(stride)?))
        .ok_or(RaoAeadError::SizeOverflow)
}

pub(crate) fn finalize_sha256(hasher: Sha256) -> [u8; 32] {
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

pub(crate) fn round_up(value: u64, multiple: u64) -> Result<u64> {
    if multiple == 0 {
        return Err(RaoAeadError::SizeOverflow);
    }
    let remainder = value % multiple;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(multiple - remainder)
            .ok_or(RaoAeadError::SizeOverflow)
    }
}

fn cipher_from_key(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(Key::from_slice(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_matches_spec_shape() {
        let nonce = stream_nonce(0x0102_0304_0506_0708, true);
        assert_eq!(&nonce[0..3], &[0, 0, 0]);
        assert_eq!(&nonce[3..11], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(nonce[11], 1);
    }

    #[test]
    fn chunk_count_requires_full_final_chunks() {
        assert_eq!(chunk_count(512, 512).unwrap(), 1);
        assert_eq!(chunk_count(1024, 512).unwrap(), 2);
        assert!(matches!(
            chunk_count(513, 512),
            Err(RaoAeadError::InvalidMetadataField)
        ));
        assert!(matches!(
            chunk_count(0, 512),
            Err(RaoAeadError::InvalidMetadataField)
        ));
    }

    #[test]
    fn chunk_encrypt_decrypt_requires_computed_finality() {
        let key = [9u8; 32];
        let chunk = vec![7u8; 512];
        let encrypted = encrypt_chunk(&key, 0, true, &chunk).unwrap();
        assert_eq!(decrypt_chunk(&key, 0, true, &encrypted).unwrap(), chunk);
        assert!(matches!(
            decrypt_chunk(&key, 0, false, &encrypted),
            Err(RaoAeadError::AeadAuthenticationFailed)
        ));
    }
}
