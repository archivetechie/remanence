//! RAO v2 HKDF-SHA-256 key derivation from a per-object DEK.

use std::fmt;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::error::{RaoAeadError, Result};

/// v2 salt derivation HKDF info label.
pub const LABEL_SALT_V2: &[u8] = b"rao2-salt-v1";
/// v2 object-secret HKDF info label.
pub const LABEL_OBJECT_V2: &[u8] = b"rao2-object-v1";
/// v2 metadata-key HKDF info label.
pub const LABEL_METADATA_V2: &[u8] = b"rao2-metadata-v1";
/// v2 payload-key HKDF info label.
pub const LABEL_PAYLOAD_V2: &[u8] = b"rao2-payload-v1";

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

/// Derive the deterministic nonzero v2 salt from the per-object DEK.
pub fn derive_salt_v2(
    dek: &[u8; 32],
    object_id_field: &[u8; 64],
    plaintext_digest: &[u8; 32],
    metadata_plaintext: &[u8],
) -> Result<[u8; 16]> {
    derive_salt_bytes(
        dek,
        LABEL_SALT_V2,
        object_id_field,
        plaintext_digest,
        metadata_plaintext,
    )
}

fn derive_salt_bytes(
    ikm: &[u8],
    label: &[u8],
    object_id_field: &[u8; 64],
    plaintext_digest: &[u8; 32],
    metadata_plaintext: &[u8],
) -> Result<[u8; 16]> {
    let metadata_hash = Sha256::digest(metadata_plaintext);
    for ctr in 0u8..=u8::MAX {
        let mut info = Vec::with_capacity(label.len() + 1 + object_id_field.len() + 32 + 32);
        info.extend_from_slice(label);
        info.push(ctr);
        info.extend_from_slice(object_id_field);
        info.extend_from_slice(plaintext_digest);
        info.extend_from_slice(&metadata_hash);
        let mut salt = [0u8; 16];
        Hkdf::<Sha256>::new(Some(&[]), ikm)
            .expand(&info, &mut salt)
            .map_err(|_| RaoAeadError::KdfExpansionFailed)?;
        if salt != [0; 16] {
            return Ok(salt);
        }
    }
    Err(RaoAeadError::InvalidSalt)
}

/// Derive the three distinct v2 keys from a DEK and header-plus-frame hash.
pub fn derive_keys_v2(
    dek: &[u8; 32],
    salt: &[u8; 16],
    header_hash: &[u8; 32],
) -> Result<DerivedKeys> {
    derive_keys_bytes(
        dek,
        salt,
        header_hash,
        LABEL_OBJECT_V2,
        LABEL_METADATA_V2,
        LABEL_PAYLOAD_V2,
    )
}

fn derive_keys_bytes(
    ikm: &[u8],
    salt: &[u8; 16],
    header_hash: &[u8; 32],
    object_label: &[u8],
    metadata_label: &[u8],
    payload_label: &[u8],
) -> Result<DerivedKeys> {
    let mut object_info = Vec::with_capacity(object_label.len() + header_hash.len());
    object_info.extend_from_slice(object_label);
    object_info.extend_from_slice(header_hash);
    let mut object_secret = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(&object_info, &mut object_secret)
        .map_err(|_| RaoAeadError::KdfExpansionFailed)?;
    let mut metadata_key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&[]), &object_secret)
        .expand(metadata_label, &mut metadata_key)
        .map_err(|_| RaoAeadError::KdfExpansionFailed)?;
    let mut payload_key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&[]), &object_secret)
        .expand(payload_label, &mut payload_key)
        .map_err(|_| RaoAeadError::KdfExpansionFailed)?;
    Ok(DerivedKeys {
        object_secret,
        metadata_key,
        payload_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salt_is_deterministic_and_input_sensitive() {
        let dek = [0x11; 32];
        let object_id = [b'a'; 64];
        let digest = [0x22; 32];
        let metadata = b"metadata";
        let first = derive_salt_v2(&dek, &object_id, &digest, metadata).unwrap();
        let second = derive_salt_v2(&dek, &object_id, &digest, metadata).unwrap();
        let changed = derive_salt_v2(&dek, &object_id, &[0x23; 32], metadata).unwrap();
        assert_eq!(first, second);
        assert_ne!(first, changed);
        assert_ne!(first, [0; 16]);
    }

    #[test]
    fn derived_keys_are_stable() {
        let dek = [0x11; 32];
        let salt = [0x33; 16];
        let header_hash = [0x44; 32];
        let a = derive_keys_v2(&dek, &salt, &header_hash).unwrap();
        let b = derive_keys_v2(&dek, &salt, &header_hash).unwrap();
        assert_eq!(a.metadata_key, b.metadata_key);
        assert_eq!(a.payload_key, b.payload_key);
        assert_ne!(a.metadata_key, a.payload_key);
    }
}
