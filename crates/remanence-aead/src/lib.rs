//! RAO 1.0 AEAD envelope implementation.
//!
//! This crate absorbs Amber's useful cryptographic construction into
//! Remanence as the isolated `remanence-aead` boundary. It implements the RAO
//! encrypted representation from `docs/rao-1.0-specification.md`: the 128-byte
//! `RAO1` header, deterministic salt derivation, HKDF key split, metadata
//! frame, age-style ChaCha20-Poly1305 STREAM payload, footer/fill validation,
//! and keyless inspection geometry. It intentionally contains no legacy AOF1
//! reader and has no dependency on other Remanence crates.

pub mod error;
pub mod header;
pub mod inspect;
pub mod kdf;
pub mod metadata;
pub mod open;
pub mod range;
pub mod seal;
pub mod stream;

pub use error::{RaoAeadError, Result};
pub use header::{
    RaoHeader, RAO_FOOTER, RAO_HEADER_LEN, RAO_MAX_METADATA_FRAME_LEN, RAO_METADATA_FRAME_MIN_LEN,
};
pub use inspect::{inspect_bytes, InspectReport};
pub use kdf::{
    derive_keys, derive_salt, DerivedKeys, RootKey, LABEL_METADATA, LABEL_OBJECT, LABEL_PAYLOAD,
    LABEL_SALT,
};
pub use metadata::RaoMetadata;
pub use open::{open, open_to_vec, OpenReport};
pub use range::{open_inner_range_to_vec, open_plaintext_range_to_vec, RangeOpenReport};
pub use seal::{seal, seal_to_vec, SealOptions, SealReport};
pub use stream::{
    cipher_offset, decrypt_chunk, encrypt_chunk, expected_stored_size, payload_frame_len,
    stored_size_from_parts, stream_nonce, PlaintextStats, CHACHA20POLY1305_TAG_LEN,
};
