//! RAO HPKE Base wrapping for per-object data-encryption keys.

use std::fmt;

use chacha20::{
    cipher::{KeyIvInit, StreamCipher},
    ChaCha20,
};
use hpke::{
    aead::ChaCha20Poly1305, kdf::HkdfSha256, kem::X25519HkdfSha256, setup_receiver, setup_sender,
    Deserializable, Kem as _, OpModeR, OpModeS, Serializable,
};
use rand_core::{CryptoRng, RngCore};
use zeroize::Zeroize;

use crate::error::{RaoAeadError, Result};
use crate::header::{object_id_field, WRAP_SUITE_HPKE_V1};
use crate::key_frame::{KeyFrame, RecipientSlot};

type Kem = X25519HkdfSha256;
type Kdf = HkdfSha256;
type Aead = ChaCha20Poly1305;

/// OS-seeded, zeroize-on-drop CSPRNG for arbitrary-length HPKE entropy draws.
pub(crate) struct EphemeralRng {
    inner: ChaCha20,
}

impl EphemeralRng {
    pub(crate) fn from_os() -> Result<Self> {
        let mut seed = [0u8; 32];
        if getrandom::fill(&mut seed).is_err() {
            seed.zeroize();
            return Err(RaoAeadError::EntropyUnavailable);
        }
        let inner = Self::from_seed(&seed);
        seed.zeroize();
        Ok(inner)
    }

    pub(crate) fn from_seed(seed: &[u8; 32]) -> Self {
        // The zeroize-enabled ChaCha20 core and its buffered wrapper both wipe
        // their state on drop. A random key with this fixed nonce defines an
        // independent stream for each ephemeral generator.
        Self {
            inner: ChaCha20::new(seed.into(), &[0u8; 12].into()),
        }
    }
}

impl RngCore for EphemeralRng {
    fn next_u32(&mut self) -> u32 {
        let mut bytes = [0u8; 4];
        self.fill_bytes(&mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn next_u64(&mut self) -> u64 {
        let mut bytes = [0u8; 8];
        self.fill_bytes(&mut bytes);
        u64::from_le_bytes(bytes)
    }

    fn fill_bytes(&mut self, destination: &mut [u8]) {
        destination.fill(0);
        self.inner.apply_keystream(destination);
    }
}

impl CryptoRng for EphemeralRng {}

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

impl RecipientPublicKey {
    /// Serialize the canonical public-recipient file (`RAOR`, slot, id, label, public key).
    pub fn serialize(&self) -> Result<Vec<u8>> {
        validate_label(&self.epoch_label)?;
        <<Kem as hpke::Kem>::PublicKey as Deserializable>::from_bytes(&self.public_key)
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        let mut out = Vec::with_capacity(54 + self.epoch_label.len());
        out.extend_from_slice(b"RAOR");
        out.push(self.slot_index);
        out.extend_from_slice(&self.recipient_epoch_id);
        out.push(self.epoch_label.len() as u8);
        out.extend_from_slice(self.epoch_label.as_bytes());
        out.extend_from_slice(&self.public_key);
        Ok(out)
    }

    /// Parse a complete canonical public-recipient file.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.get(..4) != Some(b"RAOR") || bytes.len() < 54 {
            return Err(RaoAeadError::InvalidInput(
                "invalid RAO recipient public-key file".to_string(),
            ));
        }
        let slot_index = bytes[4];
        let recipient_epoch_id = bytes[5..21].try_into().expect("fixed slice");
        let label_len = bytes[21] as usize;
        let expected = 54usize
            .checked_add(label_len)
            .ok_or(RaoAeadError::SizeOverflow)?;
        if bytes.len() != expected {
            return Err(RaoAeadError::InvalidInput(
                "invalid RAO recipient public-key file length".to_string(),
            ));
        }
        let epoch_label = std::str::from_utf8(&bytes[22..22 + label_len])
            .map_err(|_| RaoAeadError::InvalidInput("invalid recipient label".to_string()))?
            .to_string();
        validate_label(&epoch_label)?;
        let public_key: [u8; 32] = bytes[22 + label_len..]
            .try_into()
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        <<Kem as hpke::Kem>::PublicKey as Deserializable>::from_bytes(&public_key)
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        Ok(Self {
            slot_index,
            recipient_epoch_id,
            epoch_label,
            public_key,
        })
    }
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
pub fn wrap_dek<R: CryptoRng + RngCore>(
    dek: &DataEncryptionKey,
    object_id: &str,
    recipients: &[RecipientPublicKey],
    rng: &mut R,
) -> Result<KeyFrame> {
    let mut slots = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        slots.push(wrap_recipient(dek, object_id, recipient, rng)?);
    }
    KeyFrame::new(slots)
}

fn wrap_recipient<R: CryptoRng + RngCore>(
    dek: &DataEncryptionKey,
    object_id: &str,
    recipient: &RecipientPublicKey,
    rng: &mut R,
) -> Result<RecipientSlot> {
    validate_label(&recipient.epoch_label)?;
    let public =
        <<Kem as hpke::Kem>::PublicKey as Deserializable>::from_bytes(&recipient.public_key)
            .map_err(|_| RaoAeadError::HpkeFailed)?;
    let info = wrap_info(
        object_id,
        &recipient.recipient_epoch_id,
        recipient.slot_index,
    )?;
    let (enc, mut context) = setup_sender::<Aead, Kdf, Kem, _>(&OpModeS::Base, &public, &info, rng)
        .map_err(|_| RaoAeadError::HpkeFailed)?;
    let ciphertext = context
        .seal(dek.as_bytes(), &[])
        .map_err(|_| RaoAeadError::HpkeFailed)?;
    Ok(RecipientSlot {
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
    })
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

    struct CountingByteRng {
        byte: u8,
        bytes_generated: usize,
    }

    impl RngCore for CountingByteRng {
        fn next_u32(&mut self) -> u32 {
            let mut bytes = [0u8; 4];
            self.fill_bytes(&mut bytes);
            u32::from_le_bytes(bytes)
        }

        fn next_u64(&mut self) -> u64 {
            let mut bytes = [0u8; 8];
            self.fill_bytes(&mut bytes);
            u64::from_le_bytes(bytes)
        }

        fn fill_bytes(&mut self, destination: &mut [u8]) {
            destination.fill(self.byte);
            self.bytes_generated += destination.len();
        }
    }

    impl CryptoRng for CountingByteRng {}

    fn decode_hex(hex: &str) -> Vec<u8> {
        assert_eq!(hex.len() % 2, 0);
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap();
                u8::from_str_radix(pair, 16).unwrap()
            })
            .collect()
    }

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
    fn ephemeral_rng_serves_draws_larger_than_its_os_seed() {
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>(_value: &T) {}

        let mut rng = EphemeralRng::from_os().expect("OS entropy should seed ephemeral RNG");
        assert_zeroize_on_drop(&rng.inner);
        let mut output = [0u8; 96];
        rng.fill_bytes(&mut output);
        assert!(output.iter().any(|byte| *byte != 0));
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
    fn public_key_file_is_canonical() {
        let private = RecipientPrivateKey::new([3; 16], "safe-2026", [7; 32]).unwrap();
        let public = private.public_key(4).unwrap();
        let bytes = public.serialize().unwrap();
        assert_eq!(RecipientPublicKey::parse(&bytes).unwrap(), public);
    }

    #[test]
    fn wrap_round_trip_and_transplant_rejection() {
        let secret = RecipientPrivateKey::new([3; 16], "safe-2026", [7; 32]).unwrap();
        let public = secret.public_key(0).unwrap();
        let dek = DataEncryptionKey::from_bytes([9; 32]);
        let mut rng = CountingByteRng {
            byte: 0x42,
            bytes_generated: 0,
        };
        let frame = wrap_dek(&dek, "object-a", &[public], &mut rng).unwrap();
        assert_eq!(
            unwrap_dek(&frame, "object-a", &secret).unwrap().as_bytes(),
            &[9; 32]
        );
        assert!(unwrap_dek(&frame, "object-b", &secret).is_err());
    }

    #[test]
    fn rfc_9180_appendix_a2_base_vector_opens() {
        let private = <<Kem as hpke::Kem>::PrivateKey as Deserializable>::from_bytes(&decode_hex(
            "8057991eef8f1f1af18f4a9491d16a1ce333f695d4db8e38da75975c4478e0fb",
        ))
        .unwrap();
        let enc = <<Kem as hpke::Kem>::EncappedKey as Deserializable>::from_bytes(&decode_hex(
            "1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a",
        ))
        .unwrap();
        let info = decode_hex("4f6465206f6e2061204772656369616e2055726e");
        let aad = decode_hex("436f756e742d30");
        let ciphertext = decode_hex(concat!(
            "1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db",
            "21993c62ce81883d2dd1b51a28"
        ));
        let mut context =
            setup_receiver::<Aead, Kdf, Kem>(&OpModeR::Base, &private, &enc, &info).unwrap();
        assert_eq!(
            context.open(&ciphertext, &aad).unwrap(),
            b"Beauty is truth, truth beauty"
        );
    }

    #[test]
    fn rao_wrap_vector_is_byte_exact() {
        let secret = RecipientPrivateKey::new([3; 16], "safe-2026", [7; 32]).unwrap();
        let public = secret.public_key(0).unwrap();
        let dek = DataEncryptionKey::from_bytes([9; 32]);
        let mut rng = CountingByteRng {
            byte: 0x42,
            bytes_generated: 0,
        };
        let slot = wrap_recipient(&dek, "object-a", &public, &mut rng).unwrap();
        assert_eq!(rng.bytes_generated, 32, "rust-hpke X25519 entropy draw");
        assert_eq!(
            slot.enc,
            [
                0xae, 0x3b, 0xf1, 0xcd, 0x87, 0xc2, 0xd2, 0xed, 0x25, 0xaf, 0x4a, 0x1a, 0x23, 0x9e,
                0xed, 0x04, 0xa9, 0x90, 0xf0, 0x0e, 0x74, 0x03, 0xe4, 0xc8, 0x06, 0x59, 0x27, 0xde,
                0x01, 0x0f, 0xd1, 0x7a,
            ]
        );
        assert_eq!(
            slot.ciphertext,
            [
                0xfd, 0x48, 0x22, 0x7f, 0x58, 0xc8, 0xa2, 0xb4, 0xac, 0x3e, 0xb0, 0xb2, 0x24, 0xb1,
                0x18, 0x5e, 0x85, 0x8c, 0x7a, 0x46, 0x44, 0xf9, 0x6a, 0x70, 0x67, 0xd2, 0xc2, 0xd3,
                0x2d, 0x1c, 0x67, 0xda, 0xd5, 0x73, 0xcb, 0xa8, 0xd9, 0x4b, 0x66, 0x8c, 0xa2, 0xab,
                0x98, 0xb6, 0xca, 0x12, 0xa1, 0x8c,
            ]
        );
    }
}
