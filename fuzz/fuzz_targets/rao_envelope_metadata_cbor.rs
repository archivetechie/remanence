#![no_main]

//! Fuzz target for the RAO 1.0 encrypted-envelope metadata CBOR decoder.
//!
//! This exercises the metadata-profile deterministic-CBOR validator, the v1
//! schema checks, and the size arithmetic validation against the default
//! 512-byte RAO chunk size.

use libfuzzer_sys::fuzz_target;
use remanence_aead::RaoMetadata;

fuzz_target!(|data: &[u8]| {
    let _ = RaoMetadata::from_cbor_bytes(data, 512);
});
