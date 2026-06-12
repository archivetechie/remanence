//! RAO encrypted metadata schema and deterministic-CBOR validation.

use crate::error::{RaoAeadError, Result};
use crate::header::validate_chunk_size;
use crate::stream::CHACHA20POLY1305_TAG_LEN;

const KEY_METADATA_VERSION: u64 = 0;
const KEY_PLAINTEXT_SIZE: u64 = 1;
const KEY_PLAINTEXT_DIGEST_ALG: u64 = 2;
const KEY_PLAINTEXT_DIGEST: u64 = 3;

const MAX_DEPTH: usize = 32;
const MAX_ITEMS: usize = 65_536;

/// Decrypted RAO metadata fields required by version 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaoMetadata {
    /// Length of the canonical plaintext object in bytes.
    pub plaintext_size: u64,
    /// SHA-256 of the canonical plaintext object.
    pub plaintext_digest: [u8; 32],
}

impl RaoMetadata {
    /// Construct metadata and validate the size against `chunk_size`.
    pub fn new(plaintext_size: u64, plaintext_digest: [u8; 32], chunk_size: u32) -> Result<Self> {
        let metadata = Self {
            plaintext_size,
            plaintext_digest,
        };
        metadata.validate(chunk_size)?;
        Ok(metadata)
    }

    /// Encode the exact v1 writer schema: four required integer keys only.
    pub fn to_cbor_bytes(&self, chunk_size: u32) -> Result<Vec<u8>> {
        self.validate(chunk_size)?;
        let mut out = Vec::new();
        encode_type_len(5, 4, &mut out);
        encode_unsigned(KEY_METADATA_VERSION, &mut out);
        encode_unsigned(1, &mut out);
        encode_unsigned(KEY_PLAINTEXT_SIZE, &mut out);
        encode_unsigned(self.plaintext_size, &mut out);
        encode_unsigned(KEY_PLAINTEXT_DIGEST_ALG, &mut out);
        encode_text("sha256", &mut out);
        encode_unsigned(KEY_PLAINTEXT_DIGEST, &mut out);
        encode_bytes(&self.plaintext_digest, &mut out);
        Ok(out)
    }

    /// Decode metadata bytes, validating deterministic-CBOR canonical form.
    pub fn from_cbor_bytes(bytes: &[u8], chunk_size: u32) -> Result<Self> {
        let mut decoder = Decoder::new(bytes);
        let metadata = decoder.decode_metadata_map(chunk_size)?;
        if decoder.pos != bytes.len() {
            return Err(RaoAeadError::InvalidCborEncoding);
        }
        Ok(metadata)
    }

    /// Validate metadata values against the encrypted envelope chunk size.
    pub fn validate(&self, chunk_size: u32) -> Result<()> {
        validate_chunk_size(chunk_size)?;
        let chunk = u64::from(chunk_size);
        if self.plaintext_size == 0 || self.plaintext_size % chunk != 0 {
            return Err(RaoAeadError::InvalidMetadataField);
        }
        let chunk_count = self.plaintext_size / chunk;
        let tag_bytes = CHACHA20POLY1305_TAG_LEN
            .checked_mul(chunk_count)
            .ok_or(RaoAeadError::InvalidMetadataField)?;
        self.plaintext_size
            .checked_add(tag_bytes)
            .ok_or(RaoAeadError::InvalidMetadataField)?;
        Ok(())
    }
}

fn encode_unsigned(value: u64, out: &mut Vec<u8>) {
    encode_type_len(0, value, out);
}

fn encode_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    encode_type_len(2, bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn encode_text(value: &str, out: &mut Vec<u8>) {
    encode_type_len(3, value.len() as u64, out);
    out.extend_from_slice(value.as_bytes());
}

fn encode_type_len(major: u8, value: u64, out: &mut Vec<u8>) {
    let prefix = major << 5;
    match value {
        0..=23 => out.push(prefix | value as u8),
        24..=0xff => out.extend_from_slice(&[prefix | 24, value as u8]),
        0x100..=0xffff => {
            out.push(prefix | 25);
            out.extend_from_slice(&(value as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(prefix | 26);
            out.extend_from_slice(&(value as u32).to_be_bytes());
        }
        _ => {
            out.push(prefix | 27);
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    pos: usize,
    items: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            pos: 0,
            items: 0,
        }
    }

    fn decode_metadata_map(&mut self, chunk_size: u32) -> Result<RaoMetadata> {
        self.bump_item()?;
        let (major, len, _key_encoding) = self.read_type_len()?;
        if major != 5 {
            return Err(RaoAeadError::InvalidCborEncoding);
        }
        let len_usize = usize::try_from(len).map_err(|_| RaoAeadError::InvalidCborEncoding)?;
        if len_usize > MAX_ITEMS {
            return Err(RaoAeadError::InvalidCborEncoding);
        }

        let mut prev_key = None::<Vec<u8>>;
        let mut metadata_version = None;
        let mut plaintext_size = None;
        let mut plaintext_digest_alg = None;
        let mut plaintext_digest = None;

        for _ in 0..len_usize {
            let key_start = self.pos;
            let key = self.decode_top_level_key()?;
            let key_bytes = self.bytes[key_start..self.pos].to_vec();
            if prev_key.as_ref().is_some_and(|prev| prev >= &key_bytes) {
                return Err(RaoAeadError::InvalidCborEncoding);
            }
            prev_key = Some(key_bytes);

            match key {
                KEY_METADATA_VERSION => metadata_version = Some(self.decode_unsigned()?),
                KEY_PLAINTEXT_SIZE => plaintext_size = Some(self.decode_unsigned()?),
                KEY_PLAINTEXT_DIGEST_ALG => {
                    plaintext_digest_alg = Some(self.decode_text()?.to_string());
                }
                KEY_PLAINTEXT_DIGEST => {
                    let bytes = self.decode_bytes()?;
                    if bytes.len() != 32 {
                        return Err(RaoAeadError::InvalidMetadataField);
                    }
                    let mut digest = [0u8; 32];
                    digest.copy_from_slice(bytes);
                    plaintext_digest = Some(digest);
                }
                _ => self.skip_item(1)?,
            }
        }

        if metadata_version.ok_or(RaoAeadError::MissingRequiredMetadataField)? != 1 {
            return Err(RaoAeadError::InvalidMetadataField);
        }
        if plaintext_digest_alg.ok_or(RaoAeadError::MissingRequiredMetadataField)? != "sha256" {
            return Err(RaoAeadError::InvalidMetadataField);
        }
        let metadata = RaoMetadata {
            plaintext_size: plaintext_size.ok_or(RaoAeadError::MissingRequiredMetadataField)?,
            plaintext_digest: plaintext_digest.ok_or(RaoAeadError::MissingRequiredMetadataField)?,
        };
        metadata.validate(chunk_size)?;
        Ok(metadata)
    }

    fn decode_top_level_key(&mut self) -> Result<u64> {
        self.bump_item()?;
        let (major, value, _encoding) = self.read_type_len()?;
        if major != 0 {
            return Err(RaoAeadError::InvalidCborEncoding);
        }
        Ok(value)
    }

    fn decode_unsigned(&mut self) -> Result<u64> {
        self.bump_item()?;
        let (major, value, _encoding) = self.read_type_len()?;
        if major != 0 {
            return Err(RaoAeadError::InvalidMetadataField);
        }
        Ok(value)
    }

    fn decode_bytes(&mut self) -> Result<&'a [u8]> {
        self.bump_item()?;
        let (major, len, _encoding) = self.read_type_len()?;
        if major != 2 {
            return Err(RaoAeadError::InvalidMetadataField);
        }
        self.take_len(len)
    }

    fn decode_text(&mut self) -> Result<&'a str> {
        self.bump_item()?;
        let (major, len, _encoding) = self.read_type_len()?;
        if major != 3 {
            return Err(RaoAeadError::InvalidMetadataField);
        }
        let bytes = self.take_len(len)?;
        std::str::from_utf8(bytes).map_err(|_| RaoAeadError::InvalidCborEncoding)
    }

    fn skip_item(&mut self, depth: usize) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(RaoAeadError::InvalidCborEncoding);
        }
        self.bump_item()?;
        let (major, len, _encoding) = self.read_type_len()?;
        match major {
            0 | 1 => Ok(()),
            2 => {
                self.take_len(len)?;
                Ok(())
            }
            3 => {
                let bytes = self.take_len(len)?;
                std::str::from_utf8(bytes)
                    .map(|_| ())
                    .map_err(|_| RaoAeadError::InvalidCborEncoding)
            }
            4 => {
                for _ in 0..len {
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            5 => {
                let mut prev_key = None::<Vec<u8>>;
                for _ in 0..len {
                    let key_start = self.pos;
                    self.skip_item(depth + 1)?;
                    let key_bytes = self.bytes[key_start..self.pos].to_vec();
                    if prev_key.as_ref().is_some_and(|prev| prev >= &key_bytes) {
                        return Err(RaoAeadError::InvalidCborEncoding);
                    }
                    prev_key = Some(key_bytes);
                    self.skip_item(depth + 1)?;
                }
                Ok(())
            }
            7 => match len {
                20..=22 => Ok(()),
                _ => Err(RaoAeadError::InvalidCborEncoding),
            },
            _ => Err(RaoAeadError::InvalidCborEncoding),
        }
    }

    fn read_type_len(&mut self) -> Result<(u8, u64, Vec<u8>)> {
        let start = self.pos;
        let first = self.take_one()?;
        let major = first >> 5;
        let ai = first & 0x1f;
        let value = match ai {
            0..=23 => u64::from(ai),
            24 => {
                let value = u64::from(self.take_one()?);
                if value < 24 {
                    return Err(RaoAeadError::InvalidCborEncoding);
                }
                value
            }
            25 => {
                let bytes = self.take_array::<2>()?;
                let value = u64::from(u16::from_be_bytes(bytes));
                if value <= 0xff {
                    return Err(RaoAeadError::InvalidCborEncoding);
                }
                value
            }
            26 => {
                let bytes = self.take_array::<4>()?;
                let value = u64::from(u32::from_be_bytes(bytes));
                if value <= 0xffff {
                    return Err(RaoAeadError::InvalidCborEncoding);
                }
                value
            }
            27 => {
                let value = u64::from_be_bytes(self.take_array::<8>()?);
                if value <= 0xffff_ffff {
                    return Err(RaoAeadError::InvalidCborEncoding);
                }
                value
            }
            _ => return Err(RaoAeadError::InvalidCborEncoding),
        };
        Ok((major, value, self.bytes[start..self.pos].to_vec()))
    }

    fn bump_item(&mut self) -> Result<()> {
        self.items = self
            .items
            .checked_add(1)
            .ok_or(RaoAeadError::InvalidCborEncoding)?;
        if self.items > MAX_ITEMS {
            return Err(RaoAeadError::InvalidCborEncoding);
        }
        Ok(())
    }

    fn take_len(&mut self, len: u64) -> Result<&'a [u8]> {
        let len = usize::try_from(len).map_err(|_| RaoAeadError::InvalidCborEncoding)?;
        let end = self
            .pos
            .checked_add(len)
            .ok_or(RaoAeadError::InvalidCborEncoding)?;
        let bytes = self
            .bytes
            .get(self.pos..end)
            .ok_or(RaoAeadError::UnexpectedEof)?;
        self.pos = end;
        Ok(bytes)
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let bytes = self.take_len(N as u64)?;
        let mut out = [0u8; N];
        out.copy_from_slice(bytes);
        Ok(out)
    }

    fn take_one(&mut self) -> Result<u8> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or(RaoAeadError::UnexpectedEof)?;
        self.pos += 1;
        Ok(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_encodes_expected_v1_shape() {
        let metadata = RaoMetadata::new(262_144, [0x44; 32], 262_144).unwrap();
        let bytes = metadata.to_cbor_bytes(262_144).unwrap();
        assert_eq!(bytes[0], 0xa4);
        assert_eq!(&bytes[1..4], &[0x00, 0x01, 0x01]);
        let parsed = RaoMetadata::from_cbor_bytes(&bytes, 262_144).unwrap();
        assert_eq!(parsed, metadata);
    }

    #[test]
    fn metadata_rejects_noncanonical_integer() {
        let bytes = [0xa1, 0x18, 0x00, 0x01];
        assert!(matches!(
            RaoMetadata::from_cbor_bytes(&bytes, 512),
            Err(RaoAeadError::InvalidCborEncoding)
        ));
    }

    #[test]
    fn metadata_rejects_duplicate_or_unsorted_keys() {
        let bytes = [0xa2, 0x00, 0x01, 0x00, 0x01];
        assert!(matches!(
            RaoMetadata::from_cbor_bytes(&bytes, 512),
            Err(RaoAeadError::InvalidCborEncoding)
        ));
    }

    #[test]
    fn metadata_accepts_unknown_unsigned_extension_key() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[0xa5, 0x00, 0x01, 0x01, 0x19, 0x02, 0x00]);
        bytes.extend_from_slice(&[0x02, 0x66]);
        bytes.extend_from_slice(b"sha256");
        bytes.extend_from_slice(&[0x03, 0x58, 0x20]);
        bytes.extend_from_slice(&[0x44; 32]);
        bytes.extend_from_slice(&[0x04, 0x83, 0x20, 0xf5, 0xf6]);
        let parsed = RaoMetadata::from_cbor_bytes(&bytes, 512).unwrap();
        assert_eq!(parsed.plaintext_size, 512);
        assert_eq!(parsed.plaintext_digest, [0x44; 32]);
    }

    #[test]
    fn metadata_rejects_bad_plaintext_size() {
        assert!(matches!(
            RaoMetadata::new(1, [0; 32], 512),
            Err(RaoAeadError::InvalidMetadataField)
        ));
        assert!(matches!(
            RaoMetadata::new(u64::MAX - 511, [0; 32], 512),
            Err(RaoAeadError::InvalidMetadataField)
        ));
    }
}
