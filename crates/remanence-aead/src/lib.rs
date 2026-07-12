//! RAO 1.0 AEAD envelope implementation.
//!
//! This crate absorbs Amber's useful cryptographic construction into
//! Remanence as the isolated `remanence-aead` boundary. It implements the RAO
//! encrypted representation from `specs/rao-1.0-specification.md`: the 128-byte
//! `RAO1` header, deterministic salt derivation, HKDF key split, metadata
//! frame, age-style ChaCha20-Poly1305 STREAM payload, footer/fill validation,
//! and keyless inspection geometry. It intentionally contains no legacy AOF1
//! reader and has no dependency on other Remanence crates.

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

pub use error::{RaoAeadError, Result};
pub use header::{
    RaoHeader, RAO_FOOTER, RAO_HEADER_LEN, RAO_MAX_METADATA_FRAME_LEN, RAO_METADATA_FRAME_MIN_LEN,
    WRAP_SUITE_HPKE_V1, WRAP_SUITE_REGISTRY,
};
pub use inspect::{inspect_bytes, InspectReport};
pub use kdf::{
    derive_keys, derive_keys_v2, derive_salt, derive_salt_v2, DerivedKeys, RootKey, LABEL_METADATA,
    LABEL_METADATA_V2, LABEL_OBJECT, LABEL_OBJECT_V2, LABEL_PAYLOAD, LABEL_PAYLOAD_V2, LABEL_SALT,
    LABEL_SALT_V2,
};
pub use key_frame::{
    KeyFrame, RecipientSlot, RAO_KEY_FRAME_MAX_LEN, RAO_KEY_FRAME_MAX_SLOTS, RAO_KEY_FRAME_MIN_LEN,
};
pub use metadata::RaoMetadata;
pub use open::{open, open_envelope, open_envelope_to_vec, open_to_vec, OpenReport};
pub use range::{
    covering_stored_range, covering_stored_range_envelope, open_inner_range_to_vec,
    open_plaintext_range_envelope_from_reader, open_plaintext_range_envelope_to_vec,
    open_plaintext_range_from_reader, open_plaintext_range_to_vec, CoveringStoredRange,
    RangeOpenReport,
};
pub use seal::{
    seal, seal_envelope, seal_envelope_to_vec, seal_to_vec, EnvelopeSealOptions, SealOptions,
    SealReport,
};
pub use stream::{
    cipher_offset, cipher_offset_with_key_frame, decrypt_chunk, encrypt_chunk,
    expected_stored_size, payload_frame_len, stored_size_from_parts,
    stored_size_from_parts_with_key_frame, stream_nonce, PlaintextStats, CHACHA20POLY1305_TAG_LEN,
};
pub use wrap::{
    unwrap_dek, wrap_dek, wrap_info, DataEncryptionKey, RecipientPrivateKey, RecipientPublicKey,
    WRAP_INFO_LEN, WRAP_INFO_PREFIX,
};
