#![no_main]

//! Fuzz target for the REM-OBJECT envelope scalar-header parser.
//!
//! The header parser consumes exactly 128 bytes. Shorter fuzzer inputs are
//! zero-padded so libFuzzer still exercises the frozen-field validation order
//! rather than spending most executions outside the parser.

use libfuzzer_sys::fuzz_target;
use remanence_aead::{RemObjectHeader, REM_OBJECT_HEADER_LEN};

fuzz_target!(|data: &[u8]| {
    let mut header = [0u8; REM_OBJECT_HEADER_LEN];
    let take = data.len().min(REM_OBJECT_HEADER_LEN);
    header[..take].copy_from_slice(&data[..take]);
    let _ = RemObjectHeader::parse(&header);
});
