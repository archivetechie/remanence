//! Frozen X-Wing KEM primitive for RAO 2.0.
//!
//! This module implements only the byte-level glue from
//! `draft-connolly-cfrg-xwing-kem-10`: SHAKE256 seed expansion and the
//! SHA3-256 combiner over vetted ML-KEM-768 and X25519 implementations. It
//! does not implement either constituent primitive and is intentionally
//! independent of RAO envelope framing.

use std::fmt;

use libcrux_ml_kem::mlkem768::{self, MlKem768Ciphertext, MlKem768PrivateKey, MlKem768PublicKey};
use rand_core::{CryptoRng, RngCore};
use sha3::{
    digest::{ExtendableOutput, Update, XofReader},
    Digest, Sha3_256, Shake256,
};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519Secret};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Serialized X-Wing decapsulation seed length.
pub const XWING_SEED_LEN: usize = 32;
/// Serialized X-Wing encapsulation key length.
pub const XWING_PUBLIC_KEY_LEN: usize = 1216;
/// Serialized X-Wing ciphertext length.
pub const XWING_CIPHERTEXT_LEN: usize = 1120;
/// X-Wing shared-secret length.
pub const XWING_SHARED_SECRET_LEN: usize = 32;

const MLKEM768_PUBLIC_KEY_LEN: usize = 1184;
const MLKEM768_PRIVATE_KEY_LEN: usize = 2400;
const MLKEM768_CIPHERTEXT_LEN: usize = 1088;
const MLKEM768_KEY_GENERATION_SEED_LEN: usize = 64;
const MLKEM768_ENCAPSULATION_RANDOMNESS_LEN: usize = 32;
const X25519_KEY_LEN: usize = 32;
const XWING_ENCAPSULATION_RANDOMNESS_LEN: usize = 64;
const XWING_LABEL: &[u8; 6] = b"\\.//^\\";

/// Errors raised by the isolated X-Wing primitive.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum XWingError {
    /// The operating system could not provide cryptographic entropy.
    #[error("operating-system entropy unavailable")]
    EntropyUnavailable,
    /// The ML-KEM component of an encapsulation key is not canonical.
    #[error("invalid X-Wing public key")]
    InvalidPublicKey,
    /// X25519 produced the all-zero, non-contributory shared secret.
    #[error("non-contributory X25519 key agreement")]
    NonContributoryKeyAgreement,
}

/// The canonical 32-byte X-Wing decapsulation seed.
pub struct XWingSeed([u8; XWING_SEED_LEN]);

impl XWingSeed {
    /// Generate a fresh seed directly from the fallible operating-system CSPRNG.
    pub fn generate() -> Result<Self, XWingError> {
        let mut seed = Self([0u8; XWING_SEED_LEN]);
        getrandom::fill(&mut seed.0).map_err(|_| XWingError::EntropyUnavailable)?;
        Ok(seed)
    }

    /// Construct a seed from its canonical byte encoding.
    pub fn from_bytes(bytes: [u8; XWING_SEED_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the canonical seed bytes.
    pub fn as_bytes(&self) -> &[u8; XWING_SEED_LEN] {
        &self.0
    }
}

impl Zeroize for XWingSeed {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for XWingSeed {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for XWingSeed {}

impl fmt::Debug for XWingSeed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XWingSeed(<redacted>)")
    }
}

/// Canonical `ML-KEM-768 ek || X25519 pk` X-Wing encapsulation key.
#[derive(Clone, PartialEq, Eq)]
pub struct XWingPublicKey([u8; XWING_PUBLIC_KEY_LEN]);

impl XWingPublicKey {
    /// Parse and validate a serialized X-Wing encapsulation key.
    pub fn from_bytes(bytes: [u8; XWING_PUBLIC_KEY_LEN]) -> Result<Self, XWingError> {
        validate_mlkem_public_key(&bytes)?;
        Ok(Self(bytes))
    }

    /// Borrow the canonical serialized encapsulation key.
    pub fn as_bytes(&self) -> &[u8; XWING_PUBLIC_KEY_LEN] {
        &self.0
    }

    /// Return the canonical serialized encapsulation key.
    pub fn to_bytes(&self) -> [u8; XWING_PUBLIC_KEY_LEN] {
        self.0
    }
}

impl fmt::Debug for XWingPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("XWingPublicKey")
            .field(&"<1216 bytes>")
            .finish()
    }
}

/// Ephemeral expanded X-Wing decapsulation material.
///
/// The ML-KEM decapsulation key and X25519 private key are wiped on drop.
/// RAO persists only [`XWingSeed`], never this expanded form.
#[must_use = "expanded X-Wing secret material should be used or dropped promptly"]
pub struct XWingExpandedSecret {
    mlkem_private_key: [u8; MLKEM768_PRIVATE_KEY_LEN],
    x25519_private_key: [u8; X25519_KEY_LEN],
    x25519_public_key: [u8; X25519_KEY_LEN],
}

impl Zeroize for XWingExpandedSecret {
    fn zeroize(&mut self) {
        self.mlkem_private_key.zeroize();
        self.x25519_private_key.zeroize();
        self.x25519_public_key.zeroize();
    }
}

impl Drop for XWingExpandedSecret {
    fn drop(&mut self) {
        self.zeroize();
    }
}

impl ZeroizeOnDrop for XWingExpandedSecret {}

impl fmt::Debug for XWingExpandedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XWingExpandedSecret(<redacted>)")
    }
}

/// Derive the X-Wing public key and ephemeral expanded secret from a seed.
pub fn derive_keypair(seed: &XWingSeed) -> (XWingPublicKey, XWingExpandedSecret) {
    let mut expanded = Zeroizing::new([0u8; 96]);
    let mut shake = Shake256::default();
    Update::update(&mut shake, seed.as_bytes());
    shake.finalize_xof().read(&mut expanded[..]);

    let mut mlkem_seed = Zeroizing::new([0u8; MLKEM768_KEY_GENERATION_SEED_LEN]);
    mlkem_seed.copy_from_slice(&expanded[..MLKEM768_KEY_GENERATION_SEED_LEN]);
    let (mlkem_private, mlkem_public) = mlkem768::generate_key_pair(*mlkem_seed).into_parts();
    let mlkem_private_key = mlkem_private.into();
    let mlkem_public_key: [u8; MLKEM768_PUBLIC_KEY_LEN] = mlkem_public.into();

    let x25519_private_key: [u8; X25519_KEY_LEN] =
        expanded[64..96].try_into().expect("fixed expansion");
    let x25519_secret = X25519Secret::from(x25519_private_key);
    let x25519_public_key = X25519PublicKey::from(&x25519_secret).to_bytes();

    let mut public_key = [0u8; XWING_PUBLIC_KEY_LEN];
    public_key[..MLKEM768_PUBLIC_KEY_LEN].copy_from_slice(&mlkem_public_key);
    public_key[MLKEM768_PUBLIC_KEY_LEN..].copy_from_slice(&x25519_public_key);

    (
        XWingPublicKey(public_key),
        XWingExpandedSecret {
            mlkem_private_key,
            x25519_private_key: x25519_secret.to_bytes(),
            x25519_public_key,
        },
    )
}

/// Encapsulate to a validated X-Wing public key using caller-supplied CSPRNG.
pub fn encapsulate<R>(
    public_key: &XWingPublicKey,
    rng: &mut R,
) -> Result<([u8; XWING_CIPHERTEXT_LEN], [u8; XWING_SHARED_SECRET_LEN]), XWingError>
where
    R: CryptoRng + RngCore,
{
    let mut randomness = Zeroizing::new([0u8; XWING_ENCAPSULATION_RANDOMNESS_LEN]);
    rng.fill_bytes(&mut randomness[..]);
    encapsulate_deterministic(public_key, &randomness)
}

/// Decapsulate an X-Wing ciphertext using only the canonical 32-byte seed.
pub fn decapsulate(
    seed: &XWingSeed,
    ciphertext: &[u8; XWING_CIPHERTEXT_LEN],
) -> Result<[u8; XWING_SHARED_SECRET_LEN], XWingError> {
    let (_, expanded_secret) = derive_keypair(seed);

    let mlkem_ciphertext = MlKem768Ciphertext::from(
        <[u8; MLKEM768_CIPHERTEXT_LEN]>::try_from(&ciphertext[..MLKEM768_CIPHERTEXT_LEN])
            .expect("fixed ciphertext"),
    );
    let mlkem_private = MlKem768PrivateKey::from(&expanded_secret.mlkem_private_key);
    let ss_m = Zeroizing::new(mlkem768::decapsulate(&mlkem_private, &mlkem_ciphertext));
    wipe_mlkem_private_key(mlkem_private);

    let x25519_secret = X25519Secret::from(expanded_secret.x25519_private_key);
    let ct_x_bytes: [u8; X25519_KEY_LEN] = ciphertext[MLKEM768_CIPHERTEXT_LEN..]
        .try_into()
        .expect("fixed ciphertext");
    let ct_x = X25519PublicKey::from(ct_x_bytes);
    let ss_x = x25519_secret.diffie_hellman(&ct_x);
    // This constant-time all-zero check rejects a public low-order input; it
    // does not branch on the secret scalar's bytes.
    if !ss_x.was_contributory() {
        return Err(XWingError::NonContributoryKeyAgreement);
    }

    Ok(combine(
        &ss_m,
        ss_x.as_bytes(),
        &ct_x_bytes,
        &expanded_secret.x25519_public_key,
    ))
}

fn encapsulate_deterministic(
    public_key: &XWingPublicKey,
    randomness: &[u8; XWING_ENCAPSULATION_RANDOMNESS_LEN],
) -> Result<([u8; XWING_CIPHERTEXT_LEN], [u8; XWING_SHARED_SECRET_LEN]), XWingError> {
    let pk_m_bytes: [u8; MLKEM768_PUBLIC_KEY_LEN] = public_key.0[..MLKEM768_PUBLIC_KEY_LEN]
        .try_into()
        .expect("fixed public key");
    let pk_m = MlKem768PublicKey::from(pk_m_bytes);
    if !mlkem768::validate_public_key(&pk_m) {
        return Err(XWingError::InvalidPublicKey);
    }

    let mut mlkem_randomness = Zeroizing::new([0u8; MLKEM768_ENCAPSULATION_RANDOMNESS_LEN]);
    mlkem_randomness.copy_from_slice(&randomness[..MLKEM768_ENCAPSULATION_RANDOMNESS_LEN]);
    let (ct_m, ss_m) = mlkem768::encapsulate(&pk_m, *mlkem_randomness);
    let ss_m = Zeroizing::new(ss_m);

    let ephemeral_secret = X25519Secret::from(
        <[u8; X25519_KEY_LEN]>::try_from(&randomness[32..])
            .expect("fixed encapsulation randomness"),
    );
    let ct_x = X25519PublicKey::from(&ephemeral_secret).to_bytes();
    let pk_x_bytes: [u8; X25519_KEY_LEN] = public_key.0[MLKEM768_PUBLIC_KEY_LEN..]
        .try_into()
        .expect("fixed public key");
    let pk_x = X25519PublicKey::from(pk_x_bytes);
    let ss_x = ephemeral_secret.diffie_hellman(&pk_x);
    // This constant-time all-zero check rejects a public low-order input; it
    // does not branch on the secret scalar's bytes.
    if !ss_x.was_contributory() {
        return Err(XWingError::NonContributoryKeyAgreement);
    }

    let mut ciphertext = [0u8; XWING_CIPHERTEXT_LEN];
    ciphertext[..MLKEM768_CIPHERTEXT_LEN].copy_from_slice(ct_m.as_ref());
    ciphertext[MLKEM768_CIPHERTEXT_LEN..].copy_from_slice(&ct_x);
    let shared_secret = combine(&ss_m, ss_x.as_bytes(), &ct_x, &pk_x_bytes);
    Ok((ciphertext, shared_secret))
}

fn validate_mlkem_public_key(public_key: &[u8; XWING_PUBLIC_KEY_LEN]) -> Result<(), XWingError> {
    let bytes: [u8; MLKEM768_PUBLIC_KEY_LEN] = public_key[..MLKEM768_PUBLIC_KEY_LEN]
        .try_into()
        .expect("fixed public key");
    if mlkem768::validate_public_key(&MlKem768PublicKey::from(bytes)) {
        Ok(())
    } else {
        Err(XWingError::InvalidPublicKey)
    }
}

fn wipe_mlkem_private_key(private_key: MlKem768PrivateKey) {
    let mut bytes: [u8; MLKEM768_PRIVATE_KEY_LEN] = private_key.into();
    bytes.zeroize();
}

fn combine(
    ss_m: &[u8; XWING_SHARED_SECRET_LEN],
    ss_x: &[u8; XWING_SHARED_SECRET_LEN],
    ct_x: &[u8; X25519_KEY_LEN],
    pk_x: &[u8; X25519_KEY_LEN],
) -> [u8; XWING_SHARED_SECRET_LEN] {
    let mut hasher = Sha3_256::new();
    Digest::update(&mut hasher, ss_m);
    Digest::update(&mut hasher, ss_x);
    Digest::update(&mut hasher, ct_x);
    Digest::update(&mut hasher, pk_x);
    Digest::update(&mut hasher, XWING_LABEL);
    hasher.finalize_reset().into()
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;

    use libcrux_kem::{key_gen_derand, Algorithm};

    use super::*;
    use crate::wrap::EphemeralRng;

    const DRAFT10_KAT: &str = include_str!("../testdata/xwing-draft10-kat.txt");
    const ROUND_TRIP_CASES: usize = 24;

    struct FixedRng<const N: usize> {
        bytes: [u8; N],
        offset: usize,
    }

    impl<const N: usize> FixedRng<N> {
        fn new(bytes: [u8; N]) -> Self {
            Self { bytes, offset: 0 }
        }
    }

    impl<const N: usize> RngCore for FixedRng<N> {
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
            let end = self
                .offset
                .checked_add(destination.len())
                .expect("fixed RNG offset overflow");
            assert!(
                end <= N,
                "fixed RNG exhausted: requested {} bytes after {} of {N}",
                destination.len(),
                self.offset
            );
            destination.copy_from_slice(&self.bytes[self.offset..end]);
            self.offset = end;
        }
    }

    impl<const N: usize> CryptoRng for FixedRng<N> {}

    impl<const N: usize> Drop for FixedRng<N> {
        fn drop(&mut self) {
            self.bytes.zeroize();
            self.offset.zeroize();
        }
    }

    #[test]
    fn exact_sizes_are_frozen() {
        assert_eq!(size_of::<XWingSeed>(), 32);
        assert_eq!(size_of::<XWingPublicKey>(), 1216);
        assert_eq!(size_of::<[u8; XWING_CIPHERTEXT_LEN]>(), 1120);
        assert_eq!(size_of::<[u8; XWING_SHARED_SECRET_LEN]>(), 32);
        assert_eq!(XWING_SEED_LEN, 32);
        assert_eq!(XWING_PUBLIC_KEY_LEN, 1216);
        assert_eq!(XWING_CIPHERTEXT_LEN, 1120);
        assert_eq!(XWING_SHARED_SECRET_LEN, 32);
    }

    #[test]
    fn secret_types_are_zeroize_on_drop() {
        fn assert_zeroize_on_drop<T: ZeroizeOnDrop>() {}

        assert_zeroize_on_drop::<XWingSeed>();
        assert_zeroize_on_drop::<XWingExpandedSecret>();
    }

    #[test]
    fn round_trips_many_os_random_seeds() {
        let mut rng = EphemeralRng::from_os().expect("OS CSPRNG must be available");
        for case in 0..ROUND_TRIP_CASES {
            let seed = XWingSeed::generate().expect("OS CSPRNG must be available");
            let (public_key, expanded_secret) = derive_keypair(&seed);
            drop(expanded_secret);
            let (ciphertext, sender_secret) =
                encapsulate(&public_key, &mut rng).expect("valid key must encapsulate");
            let recipient_secret =
                decapsulate(&seed, &ciphertext).expect("valid ciphertext must decapsulate");
            assert_eq!(
                sender_secret, recipient_secret,
                "X-Wing round trip failed for random case {case}"
            );
        }
    }

    #[test]
    fn derivation_and_fixed_ikm_encapsulation_are_deterministic() {
        let seed_bytes = decode_kat::<XWING_SEED_LEN>("seed");
        let seed_a = XWingSeed::from_bytes(seed_bytes);
        let seed_b = XWingSeed::from_bytes(seed_bytes);
        let (public_a, expanded_a) = derive_keypair(&seed_a);
        let (public_b, expanded_b) = derive_keypair(&seed_b);
        assert_eq!(public_a, public_b);
        drop((expanded_a, expanded_b));

        let encapsulation_seed = decode_kat::<XWING_ENCAPSULATION_RANDOMNESS_LEN>("eseed");
        let mut rng_a = FixedRng::new(encapsulation_seed);
        let mut rng_b = FixedRng::new(encapsulation_seed);
        let result_a = encapsulate(&public_a, &mut rng_a).expect("KAT key must encapsulate");
        let result_b = encapsulate(&public_b, &mut rng_b).expect("KAT key must encapsulate");
        assert_eq!(result_a, result_b);
    }

    #[test]
    fn draft10_official_vector_one_matches_byte_exactly() {
        let seed = XWingSeed::from_bytes(decode_kat("seed"));
        let expected_public_key = decode_kat::<XWING_PUBLIC_KEY_LEN>("pk");
        let expected_ciphertext = decode_kat::<XWING_CIPHERTEXT_LEN>("enc");
        let expected_shared_secret = decode_kat::<XWING_SHARED_SECRET_LEN>("ss");
        let encapsulation_seed = decode_kat::<XWING_ENCAPSULATION_RANDOMNESS_LEN>("eseed");

        let (public_key, expanded_secret) = derive_keypair(&seed);
        drop(expanded_secret);
        assert_eq!(public_key.as_bytes(), &expected_public_key);

        let mut rng = FixedRng::new(encapsulation_seed);
        let (ciphertext, shared_secret) =
            encapsulate(&public_key, &mut rng).expect("official KAT key must encapsulate");
        assert_eq!(ciphertext, expected_ciphertext);
        assert_eq!(shared_secret, expected_shared_secret);
        assert_eq!(
            decapsulate(&seed, &ciphertext).expect("official KAT must decapsulate"),
            expected_shared_secret
        );
    }

    #[test]
    fn libcrux_kem_reference_agrees_on_draft10_bytes() {
        // Draft revisions 07 through 10 did not alter the construction, so
        // libcrux-kem's Draft06 variant is a byte-compatible reference.
        let seed_bytes = decode_kat::<XWING_SEED_LEN>("seed");
        let encapsulation_seed = decode_kat::<XWING_ENCAPSULATION_RANDOMNESS_LEN>("eseed");
        let seed = XWingSeed::from_bytes(seed_bytes);

        let (public_key, expanded_secret) = derive_keypair(&seed);
        drop(expanded_secret);
        let mut rng = FixedRng::new(encapsulation_seed);
        let (ciphertext, shared_secret) =
            encapsulate(&public_key, &mut rng).expect("KAT key must encapsulate");

        let (reference_private, reference_public) =
            key_gen_derand(Algorithm::XWingKemDraft06, &seed_bytes)
                .expect("reference key derivation must succeed");
        assert_eq!(reference_public.encode(), public_key.as_bytes());
        let (reference_secret, reference_ciphertext) = reference_public
            .encapsulate_derand(&encapsulation_seed)
            .expect("reference encapsulation must succeed");
        assert_eq!(reference_ciphertext.encode(), ciphertext);
        assert_eq!(reference_secret.encode(), shared_secret);
        assert_eq!(
            reference_ciphertext
                .decapsulate(&reference_private)
                .expect("reference decapsulation must succeed")
                .encode(),
            shared_secret
        );
    }

    #[test]
    fn rejects_non_contributory_x25519_inputs() {
        let seed = XWingSeed::from_bytes(decode_kat("seed"));
        let (public_key, expanded_secret) = derive_keypair(&seed);
        drop(expanded_secret);

        let mut invalid_public_bytes = public_key.to_bytes();
        invalid_public_bytes[MLKEM768_PUBLIC_KEY_LEN..].fill(0);
        let invalid_public =
            XWingPublicKey::from_bytes(invalid_public_bytes).expect("ML-KEM key remains valid");
        let mut rng = FixedRng::new(decode_kat::<XWING_ENCAPSULATION_RANDOMNESS_LEN>("eseed"));
        assert_eq!(
            encapsulate(&invalid_public, &mut rng),
            Err(XWingError::NonContributoryKeyAgreement)
        );

        let mut invalid_ciphertext = decode_kat::<XWING_CIPHERTEXT_LEN>("enc");
        invalid_ciphertext[MLKEM768_CIPHERTEXT_LEN..].fill(0);
        assert_eq!(
            decapsulate(&seed, &invalid_ciphertext),
            Err(XWingError::NonContributoryKeyAgreement)
        );
    }

    fn kat_value(name: &str) -> &str {
        DRAFT10_KAT
            .lines()
            .filter(|line| !line.starts_with('#'))
            .find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix('='))
            })
            .unwrap_or_else(|| panic!("missing {name} in draft-10 KAT"))
    }

    fn decode_kat<const N: usize>(name: &str) -> [u8; N] {
        let hex = kat_value(name);
        assert_eq!(
            hex.len(),
            N * 2,
            "{name} has wrong encoded length in draft-10 KAT"
        );
        let mut bytes = [0u8; N];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let offset = index * 2;
            *byte = u8::from_str_radix(&hex[offset..offset + 2], 16)
                .unwrap_or_else(|_| panic!("invalid hex in draft-10 KAT field {name}"));
        }
        bytes
    }
}
