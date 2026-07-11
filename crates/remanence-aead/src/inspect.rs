//! Keyless encrypted RAO inspection and geometry validation.

use sha2::{Digest, Sha256};

use crate::error::{RaoAeadError, Result};
use crate::header::{RaoHeader, RAO_FOOTER, RAO_HEADER_LEN};
use crate::stream::round_up;

/// Keyless report over an encrypted RAO object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectReport {
    /// Parsed plaintext header.
    pub header: RaoHeader,
    /// Parsed canonical v2 key frame, absent for v1.
    pub key_frame: Option<crate::KeyFrame>,
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

/// Inspect and verify non-authenticating encrypted RAO geometry without a key.
pub fn inspect_bytes(bytes: &[u8]) -> Result<InspectReport> {
    let header_bytes: [u8; RAO_HEADER_LEN] = bytes
        .get(..RAO_HEADER_LEN)
        .ok_or(RaoAeadError::UnexpectedEof)?
        .try_into()
        .map_err(|_| RaoAeadError::UnexpectedEof)?;
    let header = RaoHeader::parse(&header_bytes)?;
    let key_frame_len = u64::from(header.key_frame_len);
    let key_frame = if header.format_version == 2 && header.key_frame_len != 0 {
        let end = RAO_HEADER_LEN
            .checked_add(header.key_frame_len as usize)
            .ok_or(RaoAeadError::SizeOverflow)?;
        Some(crate::KeyFrame::parse(
            bytes
                .get(RAO_HEADER_LEN..end)
                .ok_or(RaoAeadError::UnexpectedEof)?,
        )?)
    } else {
        None
    };
    let stored_size_bytes = u64::try_from(bytes.len()).map_err(|_| RaoAeadError::SizeOverflow)?;
    if stored_size_bytes % u64::from(header.chunk_size) != 0 {
        return Err(RaoAeadError::TrailingData);
    }
    if stored_size_bytes
        < RAO_HEADER_LEN as u64
            + key_frame_len
            + RAO_FOOTER.len() as u64
            + header.metadata_frame_len
    {
        return Err(RaoAeadError::UnexpectedEof);
    }

    let digest = Sha256::digest(bytes);
    let mut stored_digest = [0u8; 32];
    stored_digest.copy_from_slice(&digest);

    let stride = u64::from(header.chunk_size)
        .checked_add(16)
        .ok_or(RaoAeadError::SizeOverflow)?;
    let numerator = stored_size_bytes
        .checked_sub(RAO_HEADER_LEN as u64)
        .and_then(|value| value.checked_sub(key_frame_len))
        .and_then(|value| value.checked_sub(RAO_FOOTER.len() as u64))
        .and_then(|value| value.checked_sub(header.metadata_frame_len))
        .ok_or(RaoAeadError::UnexpectedEof)?;
    let chunk_count = numerator / stride;
    if chunk_count == 0 {
        return Err(RaoAeadError::UnexpectedEof);
    }
    let plaintext_size = chunk_count
        .checked_mul(u64::from(header.chunk_size))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let footer_offset = (RAO_HEADER_LEN as u64)
        .checked_add(key_frame_len)
        .and_then(|value| value.checked_add(header.metadata_frame_len))
        .and_then(|value| value.checked_add(chunk_count.checked_mul(stride)?))
        .ok_or(RaoAeadError::SizeOverflow)?;
    let expected_size = round_up(
        footer_offset
            .checked_add(RAO_FOOTER.len() as u64)
            .ok_or(RaoAeadError::SizeOverflow)?,
        u64::from(header.chunk_size),
    )?;
    if expected_size != stored_size_bytes {
        return Err(RaoAeadError::TrailingData);
    }

    let footer_start = usize::try_from(footer_offset).map_err(|_| RaoAeadError::SizeOverflow)?;
    let footer_end = footer_start
        .checked_add(RAO_FOOTER.len())
        .ok_or(RaoAeadError::SizeOverflow)?;
    if bytes.get(footer_start..footer_end) != Some(&RAO_FOOTER[..]) {
        return Err(RaoAeadError::InvalidFooter);
    }
    if bytes[footer_end..].iter().any(|byte| *byte != 0) {
        return Err(RaoAeadError::FillNotZero);
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
    use crate::{seal_to_vec, RootKey, SealOptions};

    #[test]
    fn inspect_reports_header_without_key() {
        let root = RootKey::new([0x11; 32]).unwrap();
        let plaintext = vec![0x5a; 1024];
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
        let (sealed, report) = seal_to_vec(&plaintext, &root, &options).unwrap();
        let inspected = inspect_bytes(&sealed).unwrap();
        assert_eq!(inspected.header.object_id, "object-1");
        assert_eq!(inspected.chunk_count, 2);
        assert_eq!(inspected.plaintext_size, 1024);
        assert_eq!(inspected.stored_digest, report.stored_digest);
    }

    #[test]
    fn inspect_accepts_reserved_v2_registry_geometry_without_key_frame() {
        let header = RaoHeader {
            format_version: 2,
            chunk_size: 512,
            key_id: [0x10; 16],
            hkdf_salt: [0x20; 16],
            metadata_frame_len: 17,
            object_id: "registry-v2".to_string(),
            wrap_suite: crate::WRAP_SUITE_REGISTRY,
            key_frame_len: 0,
        };
        let mut object = header.serialize().unwrap().to_vec();
        object.extend_from_slice(&[0u8; 17]);
        object.extend_from_slice(&[0u8; 512 + 16]);
        object.extend_from_slice(RAO_FOOTER);
        object.resize(object.len().div_ceil(512) * 512, 0);

        let report = inspect_bytes(&object).unwrap();
        assert_eq!(report.header, header);
        assert_eq!(report.key_frame, None);
        assert_eq!(report.chunk_count, 1);
    }
}
