//! RAO v2 HPKE Base wrapping for per-object data-encryption keys.

use std::fmt;

use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, setup_receiver, setup_sender,
    Deserializable, Kem as _, OpModeR, OpModeS, Serializable,
};
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use zeroize::Zeroize;

use crate::error::{RaoAeadError, Result};
use crate::header::{object_id_field, WRAP_SUITE_HPKE_V1};
use crate::key_frame::{KeyFrame, RecipientSlot};

type Kem = X25519HkdfSha256;
type Kdf = HkdfSha256;
type Aead = ChaCha20Poly1305;

/// Frozen prefix in the fixed-width HPKE info transcript.
pub const WRAP_INFO_PREFIX: &[u8; 12] = b"rao-wrap-v1\0";
/// Exact length of the HPKE info transcript.
pub const WRAP_INFO_LEN: usize = 95;

/// Fresh per-object 256-bit data-encryption key.
pub struct DataEncryptionKey([u8; 32]);

impl DataEncryptionKey {
    /// Generate a DEK directly from the fallible operating-system CSPRNG.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| RaoAeadError::EntropyUnavailable)?;
        Ok(Self(bytes))
    }

    /// Construct key material for deterministic conformance tests and opening.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the secret bytes without cloning them.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for DataEncryptionKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for DataEncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DataEncryptionKey(<redacted>)")
    }
}

/// Public recipient epoch configured on an envelope pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipientPublicKey {
    /// Canonical slot index.
    pub slot_index: u8,
    /// Stable recipient epoch id.
    pub recipient_epoch_id: [u8; 16],
    /// Printable recovery label.
    pub epoch_label: String,
    /// Serialized X25519 public key.
    pub public_key: [u8; 32],
}

/// Secret recovery key plus its human-diagnosable epoch identity.
pub struct RecipientPrivateKey {
    /// Stable recipient epoch id.
    pub recipient_epoch_id: [u8; 16],
    /// Printable recovery label.
    pub epoch_label: String,
    private_key: [u8; 32],
}

impl RecipientPrivateKey {
    /// Construct a recipient secret from its canonical 32-byte X25519 encoding.
    pub fn new(
        recipient_epoch_id: [u8; 16],
        epoch_label: impl Into<String>,
        private_key: [u8; 32],
    ) -> Result<Self> {
        let epoch_label = epoch_label.into();
        validate_label(&epoch_label)?;
        <<Kem as hpke::Kem>::PrivateKey as Deserializable>::from_bytes(&private_key)
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        Ok(Self {
            recipient_epoch_id,
            epoch_label,
            private_key,
        })
    }

    /// Derive the corresponding serialized X25519 public key.
    pub fn public_key(&self, slot_index: u8) -> Result<RecipientPublicKey> {
        let secret =
            <<Kem as hpke::Kem>::PrivateKey as Deserializable>::from_bytes(&self.private_key)
                .map_err(|_| RaoAeadError::HpkeFailed)?;
        let public = Kem::sk_to_pk(&secret);
        let public_key = public
            .to_bytes()
            .as_slice()
            .try_into()
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        Ok(RecipientPublicKey {
            slot_index,
            recipient_epoch_id: self.recipient_epoch_id,
            epoch_label: self.epoch_label.clone(),
            public_key,
        })
    }

    /// Serialize the standalone recovery-key file format (`RAOP`, id, label, secret).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(53 + self.epoch_label.len());
        out.extend_from_slice(b"RAOP");
        out.extend_from_slice(&self.recipient_epoch_id);
        out.push(self.epoch_label.len() as u8);
        out.extend_from_slice(self.epoch_label.as_bytes());
        out.extend_from_slice(&self.private_key);
        out
    }

    /// Parse a complete canonical recovery-key file.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.get(..4) != Some(b"RAOP") || bytes.len() < 53 {
            return Err(RaoAeadError::InvalidInput(
                "invalid RAO recipient private-key file".to_string(),
            ));
        }
        let recipient_epoch_id = bytes[4..20].try_into().expect("fixed slice");
        let label_len = bytes[20] as usize;
        let expected = 53usize
            .checked_add(label_len)
            .ok_or(RaoAeadError::SizeOverflow)?;
        if bytes.len() != expected {
            return Err(RaoAeadError::InvalidInput(
                "invalid RAO recipient private-key file length".to_string(),
            ));
        }
        let epoch_label = std::str::from_utf8(&bytes[21..21 + label_len])
            .map_err(|_| RaoAeadError::InvalidInput("invalid recipient label".to_string()))?;
        let private_key = bytes[21 + label_len..]
            .try_into()
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        Self::new(recipient_epoch_id, epoch_label, private_key)
    }
}

impl Drop for RecipientPrivateKey {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

impl fmt::Debug for RecipientPrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecipientPrivateKey")
            .field("recipient_epoch_id", &self.recipient_epoch_id)
            .field("epoch_label", &self.epoch_label)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

/// Build the frozen 95-byte `rao-wrap-v1` HPKE info transcript.
pub fn wrap_info(
    object_id: &str,
    recipient_epoch_id: &[u8; 16],
    slot_index: u8,
) -> Result<[u8; WRAP_INFO_LEN]> {
    let mut info = [0u8; WRAP_INFO_LEN];
    info[..12].copy_from_slice(WRAP_INFO_PREFIX);
    info[12..76].copy_from_slice(&object_id_field(object_id)?);
    info[76..92].copy_from_slice(recipient_epoch_id);
    info[92] = slot_index;
    info[93] = 2;
    info[94] = WRAP_SUITE_HPKE_V1;
    Ok(info)
}

/// Wrap one DEK to all recipients, failing closed if any slot fails.
pub fn wrap_dek(
    dek: &DataEncryptionKey,
    object_id: &str,
    recipients: &[RecipientPublicKey],
) -> Result<KeyFrame> {
    let mut slots = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        validate_label(&recipient.epoch_label)?;
        let public =
            <<Kem as hpke::Kem>::PublicKey as Deserializable>::from_bytes(&recipient.public_key)
                .map_err(|_| RaoAeadError::HpkeFailed)?;
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| RaoAeadError::EntropyUnavailable)?;
        let mut rng = ChaCha20Rng::from_seed(seed);
        seed.zeroize();
        let info = wrap_info(
            object_id,
            &recipient.recipient_epoch_id,
            recipient.slot_index,
        )?;
        let (enc, mut context) =
            setup_sender::<Aead, Kdf, Kem, _>(&OpModeS::Base, &public, &info, &mut rng)
                .map_err(|_| RaoAeadError::HpkeFailed)?;
        let ciphertext = context
            .seal(dek.as_bytes(), &[])
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        slots.push(RecipientSlot {
            slot_index: recipient.slot_index,
            recipient_epoch_id: recipient.recipient_epoch_id,
            epoch_label: recipient.epoch_label.clone(),
            enc: enc
                .to_bytes()
                .as_slice()
                .try_into()
                .map_err(|_| RaoAeadError::HpkeFailed)?,
            ciphertext: ciphertext
                .try_into()
                .map_err(|_| RaoAeadError::HpkeFailed)?,
        });
    }
    KeyFrame::new(slots)
}

/// Unwrap the slot matching the supplied private-key epoch; never tries another mode.
pub fn unwrap_dek(
    frame: &KeyFrame,
    object_id: &str,
    recipient: &RecipientPrivateKey,
) -> Result<DataEncryptionKey> {
    let slot = frame
        .slots
        .iter()
        .find(|slot| slot.recipient_epoch_id == recipient.recipient_epoch_id)
        .ok_or(RaoAeadError::RecipientEpochMismatch)?;
    let secret =
        <<Kem as hpke::Kem>::PrivateKey as Deserializable>::from_bytes(&recipient.private_key)
            .map_err(|_| RaoAeadError::HpkeFailed)?;
    let enc = <<Kem as hpke::Kem>::EncappedKey as Deserializable>::from_bytes(&slot.enc)
        .map_err(|_| RaoAeadError::HpkeFailed)?;
    let info = wrap_info(object_id, &slot.recipient_epoch_id, slot.slot_index)?;
    let mut context = setup_receiver::<Aead, Kdf, Kem>(&OpModeR::Base, &secret, &enc, &info)
        .map_err(|_| RaoAeadError::HpkeFailed)?;
    let mut plaintext = context
        .open(&slot.ciphertext, &[])
        .map_err(|_| RaoAeadError::HpkeFailed)?;
    let bytes = plaintext
        .as_slice()
        .try_into()
        .map_err(|_| RaoAeadError::HpkeFailed)?;
    plaintext.zeroize();
    Ok(DataEncryptionKey::from_bytes(bytes))
}

fn validate_label(label: &str) -> Result<()> {
    if label.len() > 32 || !label.as_bytes().iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err(RaoAeadError::InvalidKeyFrame);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_byte_exact_and_fixed_width() {
        let info = wrap_info("obj", &[0x44; 16], 7).unwrap();
        assert_eq!(&info[..12], b"rao-wrap-v1\0");
        assert_eq!(&info[12..15], b"obj");
        assert!(info[15..76].iter().all(|byte| *byte == 0));
        assert_eq!(&info[76..92], &[0x44; 16]);
        assert_eq!(&info[92..], &[7, 2, 1]);
    }

    #[test]
    fn private_key_file_is_canonical() {
        let key = RecipientPrivateKey::new([3; 16], "safe-2026", [7; 32]).unwrap();
        let bytes = key.serialize();
        let parsed = RecipientPrivateKey::parse(&bytes).unwrap();
        assert_eq!(parsed.recipient_epoch_id, [3; 16]);
        assert_eq!(parsed.epoch_label, "safe-2026");
        assert_eq!(parsed.serialize(), bytes);
    }

    #[test]
    fn wrap_round_trip_and_transplant_rejection() {
        let secret = RecipientPrivateKey::new([3; 16], "safe-2026", [7; 32]).unwrap();
        let public = secret.public_key(0).unwrap();
        let dek = DataEncryptionKey::from_bytes([9; 32]);
        let frame = wrap_dek(&dek, "object-a", &[public]).unwrap();
        assert_eq!(
            unwrap_dek(&frame, "object-a", &secret).unwrap().as_bytes(),
            &[9; 32]
        );
        assert!(unwrap_dek(&frame, "object-b", &secret).is_err());
    }
}
