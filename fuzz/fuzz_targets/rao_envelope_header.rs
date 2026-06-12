#![no_main]

//! Fuzz target for the RAO 1.0 encrypted-envelope header parser.
//!
//! The header parser consumes exactly 128 bytes. Shorter fuzzer inputs are
//! zero-padded so libFuzzer still exercises the frozen-field validation order
//! rather than spending most executions outside the parser.

use libfuzzer_sys::fuzz_target;
use remanence_aead::{RaoHeader, RAO_HEADER_LEN};

fuzz_target!(|data: &[u8]| {
    let mut header = [0u8; RAO_HEADER_LEN];
    let take = data.len().min(RAO_HEADER_LEN);
    header[..take].copy_from_slice(&data[..take]);
    let _ = RaoHeader::parse(&header);
});
