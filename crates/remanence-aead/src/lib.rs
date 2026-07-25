//! REM-ENCRYPT 1.0 AEAD envelope implementation.
//!
//! This crate absorbs Amber's useful cryptographic construction into
//! Remanence as the isolated `remanence-aead` boundary. It implements the
//! REM-ENCRYPT 1.0 hybrid-only encrypted representation: the 128-byte `REMO` header,
//! X-Wing DEK wrapping, deterministic salt derivation, HKDF key split,
//! metadata frame, age-style ChaCha20-Poly1305 STREAM payload, footer/fill
//! validation, and keyless inspection geometry. It accepts only the X-Wing
//! profile and has no dependency on other Remanence
//! crates.

pub mod error;
pub mod header;
pub mod inspect;
pub mod kdf;
pub mod key_frame;
pub mod metadata;
pub mod open;
pub mod range;
pub mod seal;
pub mod stream;
pub mod wrap;
pub mod xwing;

pub use error::{RemObjectAeadError, Result};
pub use header::{
    RemObjectHeader, REM_OBJECT_FOOTER, REM_OBJECT_FORMAT_VERSION, REM_OBJECT_HEADER_LEN,
    REM_OBJECT_MAX_METADATA_FRAME_LEN, REM_OBJECT_METADATA_FRAME_MIN_LEN,
    REM_OBJECT_WRAP_SUITE_XWING,
};
pub use inspect::{inspect_bytes, InspectReport};
pub use kdf::{
    derive_keys, derive_salt, DerivedKeys, LABEL_METADATA, LABEL_OBJECT, LABEL_PAYLOAD, LABEL_SALT,
};
pub use key_frame::{
    KeyFrame, RecipientSlot, REM_OBJECT_KEY_FRAME_MAX_LEN, REM_OBJECT_KEY_FRAME_MAX_SLOTS,
    REM_OBJECT_KEY_FRAME_MIN_LEN,
};
pub use metadata::RemObjectMetadata;
pub use open::{open, open_to_vec, OpenReport};
pub use range::{
    covering_stored_range, open_inner_range_to_vec, open_plaintext_range_from_reader,
    open_plaintext_range_to_vec, CoveringStoredRange, RangeOpenReport,
};
pub use seal::{
    seal, seal_deterministic_for_test_vectors, seal_to_vec, EnvelopeSealOptions, SealOptions,
    SealReport,
};
pub use stream::{
    cipher_offset, decrypt_chunk, encrypt_chunk, payload_frame_len, stored_size_from_parts,
    stream_nonce, PlaintextStats, CHACHA20POLY1305_TAG_LEN,
};
pub use wrap::{
    unwrap_dek, wrap_dek, wrap_info, DataEncryptionKey, RecipientPrivateKey, RecipientPublicKey,
    WRAP_INFO_LEN, WRAP_INFO_PREFIX,
};
pub use xwing::{
    decapsulate as xwing_decapsulate, derive_keypair as derive_xwing_keypair,
    encapsulate as xwing_encapsulate, XWingError, XWingExpandedSecret, XWingPublicKey, XWingSeed,
    XWING_CIPHERTEXT_LEN, XWING_PUBLIC_KEY_LEN, XWING_SEED_LEN, XWING_SHARED_SECRET_LEN,
};
