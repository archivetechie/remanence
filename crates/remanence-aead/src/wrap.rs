//! RAO HPKE Base wrapping for per-object data-encryption keys.

use std::fmt;

use chacha20::{
    cipher::{KeyIvInit, StreamCipher},
    ChaCha20,
};
use hpke::{
    aead::ChaCha20Poly1305,
    generic_array::typenum::{Sum, U1024, U192, U32, U96},
    kdf::HkdfSha256,
    kem::SharedSecret,
    setup_receiver, setup_sender, Deserializable, HpkeError, Kem as _, OpModeR, OpModeS,
    Serializable,
};
use rand_core::{CryptoRng, RngCore};
use sha3::{
    digest::{ExtendableOutput, Update, XofReader},
    Shake256,
};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::error::{RaoAeadError, Result};
use crate::header::object_id_field;
use crate::key_frame::{KeyFrame, RecipientSlot};
use crate::xwing::{
    self, XWingPublicKey, XWingSeed, XWING_CIPHERTEXT_LEN, XWING_PUBLIC_KEY_LEN, XWING_SEED_LEN,
};

type Kem = XWingHpkeKem;
type Kdf = HkdfSha256;
type Aead = ChaCha20Poly1305;

type XWingHpkePublicKeySize = Sum<U1024, U192>;
type XWingHpkeEncappedKeySize = Sum<U1024, U96>;

/// Frozen HPKE KEM identifier assigned to X-Wing by
/// `draft-connolly-cfrg-xwing-kem-10` and the IANA HPKE registry.
///
/// This value feeds the RFC 9180 `suite_id` and must never change for RAO 2.0.
pub const XWING_HPKE_KEM_ID: u16 = 0x647a;
/// Frozen RFC 9180 suite identifier for X-Wing, HKDF-SHA256, and
/// ChaCha20-Poly1305: `"HPKE" || 0x647a || 0x0001 || 0x0003`.
pub const XWING_HPKE_SUITE_ID: &[u8; 10] = b"HPKE\x64\x7a\x00\x01\x00\x03";

// Stage 1c moves this discriminator into the envelope header implementation.
// Stage 1b uses it only in the unchanged-width HPKE info transcript.
const WRAP_SUITE_XWING: u8 = 0x02;
const RECIPIENT_PUBLIC_FILE_FIXED_LEN: usize = 4 + 1 + 16 + 1 + XWING_PUBLIC_KEY_LEN;
const RECIPIENT_PRIVATE_FILE_FIXED_LEN: usize = 4 + 16 + 1 + XWING_SEED_LEN;

#[derive(Clone, Debug, PartialEq, Eq)]
struct XWingHpkePublicKey(XWingPublicKey);

#[derive(Clone, PartialEq, Eq)]
struct XWingHpkePrivateKey([u8; XWING_SEED_LEN]);

impl Zeroize for XWingHpkePrivateKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for XWingHpkePrivateKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for XWingHpkePrivateKey {}

#[derive(Clone)]
struct XWingHpkeEncappedKey([u8; XWING_CIPHERTEXT_LEN]);

struct XWingHpkeKem;

impl Serializable for XWingHpkePublicKey {
    type OutputSize = XWingHpkePublicKeySize;

    fn write_exact(&self, output: &mut [u8]) {
        assert_eq!(output.len(), XWING_PUBLIC_KEY_LEN);
        output.copy_from_slice(self.0.as_bytes());
    }
}

impl Deserializable for XWingHpkePublicKey {
    fn from_bytes(encoded: &[u8]) -> std::result::Result<Self, HpkeError> {
        let bytes = encoded
            .try_into()
            .map_err(|_| HpkeError::IncorrectInputLength(XWING_PUBLIC_KEY_LEN, encoded.len()))?;
        XWingPublicKey::from_bytes(bytes)
            .map(Self)
            .map_err(|_| HpkeError::ValidationError)
    }
}

impl Serializable for XWingHpkePrivateKey {
    type OutputSize = U32;

    fn write_exact(&self, output: &mut [u8]) {
        assert_eq!(output.len(), XWING_SEED_LEN);
        output.copy_from_slice(&self.0);
    }
}

impl Deserializable for XWingHpkePrivateKey {
    fn from_bytes(encoded: &[u8]) -> std::result::Result<Self, HpkeError> {
        encoded
            .try_into()
            .map(Self)
            .map_err(|_| HpkeError::IncorrectInputLength(XWING_SEED_LEN, encoded.len()))
    }
}

impl Serializable for XWingHpkeEncappedKey {
    type OutputSize = XWingHpkeEncappedKeySize;

    fn write_exact(&self, output: &mut [u8]) {
        assert_eq!(output.len(), XWING_CIPHERTEXT_LEN);
        output.copy_from_slice(&self.0);
    }
}

impl Deserializable for XWingHpkeEncappedKey {
    fn from_bytes(encoded: &[u8]) -> std::result::Result<Self, HpkeError> {
        encoded
            .try_into()
            .map(Self)
            .map_err(|_| HpkeError::IncorrectInputLength(XWING_CIPHERTEXT_LEN, encoded.len()))
    }
}

impl hpke::Kem for XWingHpkeKem {
    type PublicKey = XWingHpkePublicKey;
    type PrivateKey = XWingHpkePrivateKey;
    type EncappedKey = XWingHpkeEncappedKey;
    type NSecret = U32;

    const KEM_ID: u16 = XWING_HPKE_KEM_ID;

    fn derive_keypair(ikm: &[u8]) -> (Self::PrivateKey, Self::PublicKey) {
        // X-Wing draft-10 §5.6 derives its canonical 32-byte seed from
        // variable-length HPKE IKM with SHAKE256 before running key generation.
        let mut seed_bytes = Zeroizing::new([0u8; XWING_SEED_LEN]);
        let mut shake = Shake256::default();
        shake.update(ikm);
        shake.finalize_xof().read(&mut seed_bytes[..]);

        let seed = XWingSeed::from_bytes(*seed_bytes);
        let (public_key, expanded_secret) = xwing::derive_keypair(&seed);
        drop(expanded_secret);
        (
            XWingHpkePrivateKey(*seed_bytes),
            XWingHpkePublicKey(public_key),
        )
    }

    fn sk_to_pk(private_key: &Self::PrivateKey) -> Self::PublicKey {
        let seed = XWingSeed::from_bytes(private_key.0);
        let (public_key, expanded_secret) = xwing::derive_keypair(&seed);
        drop(expanded_secret);
        XWingHpkePublicKey(public_key)
    }

    fn encap<R: CryptoRng + RngCore>(
        recipient_public_key: &Self::PublicKey,
        sender_identity: Option<(&Self::PrivateKey, &Self::PublicKey)>,
        rng: &mut R,
    ) -> std::result::Result<(SharedSecret<Self>, Self::EncappedKey), HpkeError> {
        if sender_identity.is_some() {
            return Err(HpkeError::EncapError);
        }
        let (encapped_key, mut secret) =
            xwing::encapsulate(&recipient_public_key.0, rng).map_err(|_| HpkeError::EncapError)?;
        let mut shared_secret = SharedSecret::<Self>::default();
        shared_secret.0.copy_from_slice(&secret);
        secret.zeroize();
        Ok((shared_secret, XWingHpkeEncappedKey(encapped_key)))
    }

    fn decap(
        recipient_private_key: &Self::PrivateKey,
        sender_identity: Option<&Self::PublicKey>,
        encapped_key: &Self::EncappedKey,
    ) -> std::result::Result<SharedSecret<Self>, HpkeError> {
        if sender_identity.is_some() {
            return Err(HpkeError::DecapError);
        }
        let seed = XWingSeed::from_bytes(recipient_private_key.0);
        let mut secret =
            xwing::decapsulate(&seed, &encapped_key.0).map_err(|_| HpkeError::DecapError)?;
        let mut shared_secret = SharedSecret::<Self>::default();
        shared_secret.0.copy_from_slice(&secret);
        secret.zeroize();
        Ok(shared_secret)
    }
}

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
    /// Serialized X-Wing encapsulation key (`ML-KEM-768 ek || X25519 pk`).
    pub public_key: [u8; XWING_PUBLIC_KEY_LEN],
}

impl RecipientPublicKey {
    /// Serialize the canonical public-recipient file (`RAOR`, slot, id, label, public key).
    pub fn serialize(&self) -> Result<Vec<u8>> {
        validate_label(&self.epoch_label)?;
        <<Kem as hpke::Kem>::PublicKey as Deserializable>::from_bytes(&self.public_key)
            .map_err(|_| RaoAeadError::HpkeFailed)?;
        let mut out = Vec::with_capacity(RECIPIENT_PUBLIC_FILE_FIXED_LEN + self.epoch_label.len());
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
        if bytes.get(..4) != Some(b"RAOR") || bytes.len() < RECIPIENT_PUBLIC_FILE_FIXED_LEN {
            return Err(RaoAeadError::InvalidInput(
                "invalid RAO recipient public-key file".to_string(),
            ));
        }
        let slot_index = bytes[4];
        let recipient_epoch_id = bytes[5..21].try_into().expect("fixed slice");
        let label_len = bytes[21] as usize;
        let expected = RECIPIENT_PUBLIC_FILE_FIXED_LEN
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
        let public_key: [u8; XWING_PUBLIC_KEY_LEN] = bytes[22 + label_len..]
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
    private_key: [u8; XWING_SEED_LEN],
}

impl RecipientPrivateKey {
    /// Construct a recipient secret from its canonical 32-byte X-Wing seed.
    pub fn new(
        recipient_epoch_id: [u8; 16],
        epoch_label: impl Into<String>,
        private_key: [u8; XWING_SEED_LEN],
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

    /// Derive the corresponding serialized X-Wing public key.
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
        let mut out = Vec::with_capacity(RECIPIENT_PRIVATE_FILE_FIXED_LEN + self.epoch_label.len());
        out.extend_from_slice(b"RAOP");
        out.extend_from_slice(&self.recipient_epoch_id);
        out.push(self.epoch_label.len() as u8);
        out.extend_from_slice(self.epoch_label.as_bytes());
        out.extend_from_slice(&self.private_key);
        out
    }

    /// Parse a complete canonical recovery-key file.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.get(..4) != Some(b"RAOP") || bytes.len() < RECIPIENT_PRIVATE_FILE_FIXED_LEN {
            return Err(RaoAeadError::InvalidInput(
                "invalid RAO recipient private-key file".to_string(),
            ));
        }
        let recipient_epoch_id = bytes[4..20].try_into().expect("fixed slice");
        let label_len = bytes[20] as usize;
        let expected = RECIPIENT_PRIVATE_FILE_FIXED_LEN
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
    info[94] = WRAP_SUITE_XWING;
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

    const RAO_XWING_WRAP_KAT: &str = include_str!("../testdata/xwing-wrap-kat.txt");

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

    fn wrap_kat_field(name: &str) -> Vec<u8> {
        let prefix = format!("{name}=");
        let value = RAO_XWING_WRAP_KAT
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("missing X-Wing wrap KAT field {name}"));
        decode_hex(value)
    }

    fn wrap_kat_array<const N: usize>(name: &str) -> [u8; N] {
        let bytes = wrap_kat_field(name);
        bytes
            .try_into()
            .unwrap_or_else(|value: Vec<u8>| panic!("KAT field {name} is {} bytes", value.len()))
    }

    #[test]
    fn info_is_byte_exact_and_fixed_width() {
        let info = wrap_info("obj", &[0x44; 16], 7).unwrap();
        assert_eq!(&info[..12], b"rao-wrap-v1\0");
        assert_eq!(&info[12..15], b"obj");
        assert!(info[15..76].iter().all(|byte| *byte == 0));
        assert_eq!(&info[76..92], &[0x44; 16]);
        assert_eq!(&info[92..], &[7, 2, WRAP_SUITE_XWING]);
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
        assert_eq!(
            bytes.len(),
            RECIPIENT_PUBLIC_FILE_FIXED_LEN + "safe-2026".len()
        );
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
        let other_recipient = RecipientPrivateKey::new([3; 16], "safe-2026", [8; 32]).unwrap();
        assert!(
            unwrap_dek(&frame, "object-a", &other_recipient).is_err(),
            "a slot wrapped to one X-Wing seed must reject a transplant to another seed"
        );
    }

    #[test]
    fn many_deterministically_random_recipients_round_trip() {
        let mut seed_rng = EphemeralRng::from_seed(&[0x91; 32]);
        let mut encapsulation_rng = EphemeralRng::from_seed(&[0xa2; 32]);
        let dek = DataEncryptionKey::from_bytes([0x5a; 32]);
        for case in 0u8..24 {
            let mut seed = [0u8; XWING_SEED_LEN];
            seed_rng.fill_bytes(&mut seed);
            let recipient =
                RecipientPrivateKey::new([case; 16], format!("recipient-{case}"), seed).unwrap();
            seed.zeroize();
            let public = recipient.public_key(case).unwrap();
            let frame = wrap_dek(
                &dek,
                "many-recipient-round-trip",
                &[public],
                &mut encapsulation_rng,
            )
            .unwrap();
            assert_eq!(
                unwrap_dek(&frame, "many-recipient-round-trip", &recipient)
                    .unwrap()
                    .as_bytes(),
                dek.as_bytes(),
                "X-Wing wrap round trip {case}"
            );
        }
    }

    // RFC 9180 Appendix A.2 exercises DHKEM(X25519) and no longer applies to
    // RAO's X-Wing-only KEM. This adapter check replaces that legacy fixture.
    #[test]
    fn xwing_hpke_adapter_has_frozen_sizes_id_and_derivation() {
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>(_value: &T) {}

        assert_eq!(Kem::KEM_ID, XWING_HPKE_KEM_ID);
        assert_eq!(
            <Kdf as hpke::kdf::Kdf>::KDF_ID,
            u16::from_be_bytes(XWING_HPKE_SUITE_ID[6..8].try_into().unwrap())
        );
        assert_eq!(
            <Aead as hpke::aead::Aead>::AEAD_ID,
            u16::from_be_bytes(XWING_HPKE_SUITE_ID[8..10].try_into().unwrap())
        );
        assert_eq!(
            XWING_HPKE_KEM_ID,
            u16::from_be_bytes(XWING_HPKE_SUITE_ID[4..6].try_into().unwrap())
        );
        assert_eq!(
            <<Kem as hpke::Kem>::PublicKey as Serializable>::size(),
            XWING_PUBLIC_KEY_LEN
        );
        assert_eq!(
            <<Kem as hpke::Kem>::PrivateKey as Serializable>::size(),
            XWING_SEED_LEN
        );
        assert_eq!(
            <<Kem as hpke::Kem>::EncappedKey as Serializable>::size(),
            XWING_CIPHERTEXT_LEN
        );

        let (private, public) = Kem::derive_keypair(b"variable-length X-Wing HPKE ikm");
        assert_zeroize_on_drop(&private);
        assert_eq!(Kem::sk_to_pk(&private), public);
    }

    #[test]
    fn rao_xwing_wrap_vector_is_byte_exact() {
        let seed = wrap_kat_array("seed");
        let recipient_epoch_id = wrap_kat_array("recipient_epoch_id");
        let slot_index = wrap_kat_array::<1>("slot_index")[0];
        let object_id = String::from_utf8(wrap_kat_field("object_id")).unwrap();
        let secret = RecipientPrivateKey::new(recipient_epoch_id, "safe-2026", seed).unwrap();
        let public = secret.public_key(slot_index).unwrap();
        let expected_public = wrap_kat_field("pk");
        assert_eq!(expected_public.len(), XWING_PUBLIC_KEY_LEN);
        assert_eq!(public.public_key.as_slice(), expected_public);

        let dek = DataEncryptionKey::from_bytes(wrap_kat_array("dek"));
        let encapsulation_randomness = wrap_kat_field("encapsulation_randomness");
        assert_eq!(encapsulation_randomness.len(), 64);
        assert!(encapsulation_randomness
            .iter()
            .all(|byte| *byte == encapsulation_randomness[0]));
        let mut rng = CountingByteRng {
            byte: encapsulation_randomness[0],
            bytes_generated: 0,
        };
        let slot = wrap_recipient(&dek, &object_id, &public, &mut rng).unwrap();
        assert_eq!(
            rng.bytes_generated,
            encapsulation_randomness.len(),
            "X-Wing encapsulation entropy draw"
        );
        let expected_enc = wrap_kat_field("enc");
        assert_eq!(expected_enc.len(), XWING_CIPHERTEXT_LEN);
        assert_eq!(slot.enc.as_slice(), expected_enc);
        let expected_ciphertext = wrap_kat_field("ciphertext");
        assert_eq!(expected_ciphertext.len(), 48);
        assert_eq!(slot.ciphertext.as_slice(), expected_ciphertext);

        assert_eq!(
            unwrap_dek(&KeyFrame::new(vec![slot]).unwrap(), &object_id, &secret)
                .unwrap()
                .as_bytes(),
            dek.as_bytes()
        );
    }
}
