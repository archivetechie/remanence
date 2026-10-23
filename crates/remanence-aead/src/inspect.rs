//! Keyless encrypted REM-OBJECT inspection and geometry validation.

use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

use crate::error::{RemObjectAeadError, Result};
use crate::header::{RemObjectHeader, REM_OBJECT_FOOTER, REM_OBJECT_HEADER_LEN};
use crate::stream::round_up;

/// Keyless report over an encrypted REM-OBJECT object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReport {
    /// Parsed plaintext header.
    pub header: RemObjectHeader,
    /// Parsed canonical key frame.
    pub key_frame: crate::KeyFrame,
    /// Total stored input size.
    pub stored_size_bytes: u64,
    /// SHA-256 over all stored bytes.
    pub stored_digest: [u8; 32],
    /// Keylessly derived payload chunk count.
    pub chunk_count: u64,
    /// Keylessly derived plaintext size.
    pub plaintext_size: u64,
    /// Derived footer byte offset.
    pub footer_offset: u64,
}

/// Inspect and verify non-authenticating encrypted REM-OBJECT geometry without a key.
pub fn inspect_bytes(bytes: &[u8]) -> Result<InspectReport> {
    inspect_reader(&mut std::io::Cursor::new(bytes))
}

/// Inspect a seekable stored object with bounded memory, without decrypting it.
pub fn inspect_reader(input: &mut (impl Read + Seek)) -> Result<InspectReport> {
    input.seek(SeekFrom::Start(0))?;
    let mut header_bytes = [0; REM_OBJECT_HEADER_LEN];
    input
        .read_exact(&mut header_bytes)
        .map_err(crate::error::map_read_exact_error)?;
    let header = RemObjectHeader::parse(&header_bytes)?;
    let key_frame_len = u64::from(header.key_frame_len);
    let mut key_frame_bytes = vec![0; header.key_frame_len as usize];
    input
        .read_exact(&mut key_frame_bytes)
        .map_err(crate::error::map_read_exact_error)?;
    let key_frame = crate::KeyFrame::parse(&key_frame_bytes)?;
    let stored_size_bytes = input.seek(SeekFrom::End(0))?;
    if !stored_size_bytes.is_multiple_of(u64::from(header.chunk_size)) {
        return Err(RemObjectAeadError::TrailingData);
    }
    if stored_size_bytes
        < REM_OBJECT_HEADER_LEN as u64
            + key_frame_len
            + REM_OBJECT_FOOTER.len() as u64
            + header.metadata_frame_len
    {
        return Err(RemObjectAeadError::UnexpectedEof);
    }

    input.seek(SeekFrom::Start(0))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    let stored_digest = digest.finalize().into();

    let stride = u64::from(header.chunk_size)
        .checked_add(16)
        .ok_or(RemObjectAeadError::SizeOverflow)?;
    let numerator = stored_size_bytes
        .checked_sub(REM_OBJECT_HEADER_LEN as u64)
        .and_then(|value| value.checked_sub(key_frame_len))
        .and_then(|value| value.checked_sub(REM_OBJECT_FOOTER.len() as u64))
        .and_then(|value| value.checked_sub(header.metadata_frame_len))
        .ok_or(RemObjectAeadError::UnexpectedEof)?;
    let chunk_count = numerator / stride;
    if chunk_count == 0 {
        return Err(RemObjectAeadError::UnexpectedEof);
    }
    let plaintext_size = chunk_count
        .checked_mul(u64::from(header.chunk_size))
        .ok_or(RemObjectAeadError::SizeOverflow)?;
    let footer_offset = (REM_OBJECT_HEADER_LEN as u64)
        .checked_add(key_frame_len)
        .and_then(|value| value.checked_add(header.metadata_frame_len))
        .and_then(|value| value.checked_add(chunk_count.checked_mul(stride)?))
        .ok_or(RemObjectAeadError::SizeOverflow)?;
    let expected_size = round_up(
        footer_offset
            .checked_add(REM_OBJECT_FOOTER.len() as u64)
            .ok_or(RemObjectAeadError::SizeOverflow)?,
        u64::from(header.chunk_size),
    )?;
    if expected_size != stored_size_bytes {
        return Err(RemObjectAeadError::TrailingData);
    }

    input.seek(SeekFrom::Start(footer_offset))?;
    let mut footer = [0; 16];
    input
        .read_exact(&mut footer)
        .map_err(crate::error::map_read_exact_error)?;
    if &footer != REM_OBJECT_FOOTER {
        return Err(RemObjectAeadError::InvalidFooter);
    }
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        if buffer[..n].iter().any(|byte| *byte != 0) {
            return Err(RemObjectAeadError::FillNotZero);
        }
    }

    Ok(InspectReport {
        header,
        key_frame,
        stored_size_bytes,
        stored_digest,
        chunk_count,
        plaintext_size,
        footer_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{seal_to_vec, EnvelopeSealOptions, RecipientPrivateKey, SealOptions};

    #[test]
    fn inspect_reports_header_without_key() {
        let plaintext = vec![0x5a; 1024];
        let primary = RecipientPrivateKey::new([1; 16], "primary", [7; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([2; 16], "recovery", [8; 32]).unwrap();
        let options = EnvelopeSealOptions {
            allow_single_recipient: false,
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
        let (sealed, report) = seal_to_vec(&plaintext, &options).unwrap();
        let inspected = inspect_bytes(&sealed).unwrap();
        assert_eq!(inspected.header.object_id, "object-1");
        assert_eq!(inspected.chunk_count, 2);
        assert_eq!(inspected.plaintext_size, 1024);
        assert_eq!(inspected.stored_digest, report.stored_digest);
    }
}
