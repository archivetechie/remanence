//! REM-OBJECT envelope HKDF-SHA-256 key derivation from a per-object DEK.

use std::fmt;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{RemObjectAeadError, Result};

/// Salt derivation HKDF info label.
pub const LABEL_SALT: &[u8] = b"rem-encrypt-salt-v1";
/// Object-secret HKDF info label.
pub const LABEL_OBJECT: &[u8] = b"rem-encrypt-object-v1";
/// Metadata-key HKDF info label.
pub const LABEL_METADATA: &[u8] = b"rem-encrypt-metadata-v1";
/// Payload-key HKDF info label.
pub const LABEL_PAYLOAD: &[u8] = b"rem-encrypt-payload-v1";

/// Derived REM-OBJECT object, metadata, and payload keys.
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

impl ZeroizeOnDrop for DerivedKeys {}

/// Verify the derived salt identically in whole-object and range opening.
pub(crate) fn verify_salt(expected: &[u8; 16], actual: &[u8; 16]) -> Result<()> {
    if bool::from(expected.ct_eq(actual)) {
        Ok(())
    } else {
        Err(RemObjectAeadError::SaltDerivationMismatch)
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

/// Derive the deterministic nonzero envelope salt from the per-object DEK.
pub fn derive_salt(
    dek: &[u8; 32],
    object_id_field: &[u8; 64],
    plaintext_digest: &[u8; 32],
    metadata_plaintext: &[u8],
) -> Result<[u8; 16]> {
    derive_salt_bytes(
        dek,
        LABEL_SALT,
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
            .map_err(|_| RemObjectAeadError::KdfExpansionFailed)?;
        if !bool::from(salt.ct_eq(&[0; 16])) {
            return Ok(salt);
        }
    }
    Err(RemObjectAeadError::InvalidSalt)
}

/// Derive the three distinct envelope keys from a DEK and header-plus-frame hash.
pub fn derive_keys(dek: &[u8; 32], salt: &[u8; 16], header_hash: &[u8; 32]) -> Result<DerivedKeys> {
    derive_keys_bytes(
        dek,
        salt,
        header_hash,
        LABEL_OBJECT,
        LABEL_METADATA,
        LABEL_PAYLOAD,
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
    let mut keys = DerivedKeys {
        object_secret: [0; 32],
        metadata_key: [0; 32],
        payload_key: [0; 32],
    };
    Hkdf::<Sha256>::new(Some(salt), ikm)
        .expand(&object_info, &mut keys.object_secret)
        .map_err(|_| RemObjectAeadError::KdfExpansionFailed)?;
    Hkdf::<Sha256>::new(Some(&[]), &keys.object_secret)
        .expand(metadata_label, &mut keys.metadata_key)
        .map_err(|_| RemObjectAeadError::KdfExpansionFailed)?;
    Hkdf::<Sha256>::new(Some(&[]), &keys.object_secret)
        .expand(payload_label, &mut keys.payload_key)
        .map_err(|_| RemObjectAeadError::KdfExpansionFailed)?;
    Ok(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salt_verification_rejects_each_single_byte_difference() {
        let expected = [0x42; 16];
        verify_salt(&expected, &expected).unwrap();
        for index in 0..expected.len() {
            let mut changed = expected;
            changed[index] ^= 1;
            assert!(matches!(
                verify_salt(&expected, &changed),
                Err(RemObjectAeadError::SaltDerivationMismatch)
            ));
        }
    }

    #[test]
    fn secret_owners_and_hash_states_zeroize_on_drop() {
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}
        assert_zeroize_on_drop::<DerivedKeys>();
        assert_zeroize_on_drop::<sha2::Sha256>();
        assert_zeroize_on_drop::<sha3::Sha3_256>();
        // HMAC 0.13 has no ZeroizeOnDrop marker; its SHA-256 states wipe on drop.
        assert_zeroize_on_drop::<zeroize::Zeroizing<[u8; 32]>>();
        assert_zeroize_on_drop::<zeroize::Zeroizing<[u8; 1]>>();
        assert_zeroize_on_drop::<zeroize::Zeroizing<Vec<u8>>>();
    }

    #[test]
    fn salt_is_deterministic_and_input_sensitive() {
        let dek = [0x11; 32];
        let object_id = [b'a'; 64];
        let digest = [0x22; 32];
        let metadata = b"metadata";
        let first = derive_salt(&dek, &object_id, &digest, metadata).unwrap();
        let second = derive_salt(&dek, &object_id, &digest, metadata).unwrap();
        let changed = derive_salt(&dek, &object_id, &[0x23; 32], metadata).unwrap();
        assert_eq!(first, second);
        assert_ne!(first, changed);
        assert_ne!(first, [0; 16]);
    }

    #[test]
    fn derived_keys_are_stable() {
        let dek = [0x11; 32];
        let salt = [0x33; 16];
        let header_hash = [0x44; 32];
        let a = derive_keys(&dek, &salt, &header_hash).unwrap();
        let b = derive_keys(&dek, &salt, &header_hash).unwrap();
        assert_eq!(a.metadata_key, b.metadata_key);
        assert_eq!(a.payload_key, b.payload_key);
        assert_ne!(a.metadata_key, a.payload_key);
    }
}
