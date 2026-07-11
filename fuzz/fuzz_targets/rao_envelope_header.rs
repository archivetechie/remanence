#![no_main]

//! Fuzz target for the disjoint RAO v1/v2 header and key-frame parsers.
//!
//! The header parser consumes exactly 128 bytes. Shorter fuzzer inputs are
//! zero-padded so libFuzzer still exercises the frozen-field validation order
//! rather than spending most executions outside the parser.

use libfuzzer_sys::fuzz_target;
use remanence_aead::{KeyFrame, RaoHeader, RAO_HEADER_LEN};

fuzz_target!(|data: &[u8]| {
    let mut header = [0u8; RAO_HEADER_LEN];
    let take = data.len().min(RAO_HEADER_LEN);
    header[..take].copy_from_slice(&data[..take]);
    if let Ok(parsed) = RaoHeader::parse(&header) {
        if parsed.format_version == 2 {
            let key_frame_len = parsed.key_frame_len as usize;
            if let Some(key_frame) = data.get(RAO_HEADER_LEN..RAO_HEADER_LEN + key_frame_len) {
                let _ = KeyFrame::parse(key_frame);
            }
        }
    }
});
