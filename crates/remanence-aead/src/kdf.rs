//! RAO HKDF-SHA-256 root-key handling and key derivation.

use std::fmt;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::error::{RaoAeadError, Result};

/// Salt derivation HKDF info label.
pub const LABEL_SALT: &[u8] = b"rao1-salt-v1";
/// Object-secret HKDF info label.
pub const LABEL_OBJECT: &[u8] = b"rao1-object-v1";
/// Metadata-key HKDF info label.
pub const LABEL_METADATA: &[u8] = b"rao1-metadata-v1";
/// Payload-key HKDF info label.
pub const LABEL_PAYLOAD: &[u8] = b"rao1-payload-v1";

/// Caller-supplied root key material.
#[derive(Clone)]
pub struct RootKey {
    bytes: Vec<u8>,
}

impl RootKey {
    /// Construct root key material. RAO requires at least 32 bytes.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let mut bytes = bytes.into();
        if bytes.len() < 32 {
            bytes.zeroize();
            return Err(RaoAeadError::InvalidRootKey);
        }
        Ok(Self { bytes })
    }

    /// Return the root-key bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for RootKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl fmt::Debug for RootKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootKey")
            .field("len", &self.bytes.len())
            .field("bytes", &"<redacted>")
            .finish()
    }
}

/// Derived RAO object, metadata, and payload keys.
pub struct DerivedKeys {
    /// Header-bound object secret.
    pub object_secret: [u8; 32],
    /// Metadata-frame AEAD key.
    pub metadata_key: [u8; 32],
    /// STREAM payload AEAD key.
    pub payload_key: [u8; 32],
}

impl Drop for DerivedKeys {
    fn drop(&mut self) {
        self.object_secret.zeroize();
        self.metadata_key.zeroize();
        self.payload_key.zeroize();
    }
}

impl fmt::Debug for DerivedKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DerivedKeys")
            .field("object_secret", &"<redacted>")
            .field("metadata_key", &"<redacted>")
            .field("payload_key", &"<redacted>")
            .finish()
    }
}

/// Derive the deterministic nonzero 16-byte header salt.
pub fn derive_salt(
    root_key: &RootKey,
    object_id_field: &[u8; 64],
    plaintext_digest: &[u8; 32],
    metadata_plaintext: &[u8],
) -> Result<[u8; 16]> {
    let metadata_hash = Sha256::digest(metadata_plaintext);
    for ctr in 0u8..=u8::MAX {
        let mut info = Vec::with_capacity(LABEL_SALT.len() + 1 + object_id_field.len() + 32 + 32);
        info.extend_from_slice(LABEL_SALT);
        info.push(ctr);
        info.extend_from_slice(object_id_field);
        info.extend_from_slice(plaintext_digest);
        info.extend_from_slice(&metadata_hash);

        let mut salt = [0u8; 16];
        Hkdf::<Sha256>::new(Some(&[]), root_key.as_bytes())
            .expand(&info, &mut salt)
            .map_err(|_| RaoAeadError::InvalidRootKey)?;
        if salt != [0; 16] {
            return Ok(salt);
        }
    }
    Err(RaoAeadError::InvalidSalt)
}

/// Derive object, metadata, and payload keys from the final header.
pub fn derive_keys(
    root_key: &RootKey,
    salt: &[u8; 16],
    header_hash: &[u8; 32],
) -> Result<DerivedKeys> {
    let mut object_info = Vec::with_capacity(LABEL_OBJECT.len() + header_hash.len());
    object_info.extend_from_slice(LABEL_OBJECT);
    object_info.extend_from_slice(header_hash);

    let mut object_secret = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), root_key.as_bytes())
        .expand(&object_info, &mut object_secret)
        .map_err(|_| RaoAeadError::InvalidRootKey)?;

    let mut metadata_key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&[]), &object_secret)
        .expand(LABEL_METADATA, &mut metadata_key)
        .map_err(|_| RaoAeadError::InvalidRootKey)?;

    let mut payload_key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&[]), &object_secret)
        .expand(LABEL_PAYLOAD, &mut payload_key)
        .map_err(|_| RaoAeadError::InvalidRootKey)?;

    let derived = DerivedKeys {
        object_secret,
        metadata_key,
        payload_key,
    };
    object_secret.zeroize();
    metadata_key.zeroize();
    payload_key.zeroize();
    Ok(derived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::RaoHeader;

    #[test]
    fn short_root_key_fails() {
        assert!(matches!(
            RootKey::new([1u8; 31]),
            Err(RaoAeadError::InvalidRootKey)
        ));
    }

    #[test]
    fn salt_is_deterministic_and_input_sensitive() {
        let root_key = RootKey::new([0x11; 32]).unwrap();
        let object_id = [b'a'; 64];
        let digest = [0x22; 32];
        let metadata = b"metadata";
        let first = derive_salt(&root_key, &object_id, &digest, metadata).unwrap();
        let second = derive_salt(&root_key, &object_id, &digest, metadata).unwrap();
        let changed = derive_salt(&root_key, &object_id, &[0x23; 32], metadata).unwrap();
        assert_eq!(first, second);
        assert_ne!(first, changed);
        assert_ne!(first, [0; 16]);
    }

    #[test]
    fn derived_keys_are_stable() {
        let root_key = RootKey::new([0x11; 32]).unwrap();
        let header = RaoHeader::new(262_144, [0x22; 16], [0x33; 16], 64, "object").unwrap();
        let a = derive_keys(&root_key, &header.hkdf_salt, &header.header_hash().unwrap()).unwrap();
        let b = derive_keys(&root_key, &header.hkdf_salt, &header.header_hash().unwrap()).unwrap();
        assert_eq!(a.metadata_key, b.metadata_key);
        assert_eq!(a.payload_key, b.payload_key);
        assert_ne!(a.metadata_key, a.payload_key);
    }
}
