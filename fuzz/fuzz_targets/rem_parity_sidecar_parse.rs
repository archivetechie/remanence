#![no_main]

//! Fuzz target for the REM-PARITY 1.0 sidecar parsers: header block, footer
//! block, and the whole-tape-file parse the Scanner uses (freeze §18.3).
//! Robustness property only: no panic, no hang, no unbounded allocation.

use libfuzzer_sys::fuzz_target;
use remanence_parity::{
    classify_sidecar_header_block, parse_sidecar_footer_block, parse_sidecar_header_block,
    parse_sidecar_tape_file,
};

const TAPE_UUID: [u8; 16] = *b"rem-fuzz-tape-01";

fuzz_target!(|data: &[u8]| {
    if data.len() > 1 << 21 || data.is_empty() {
        return;
    }
    // Single-block parsers over the raw input.
    let _ = classify_sidecar_header_block(data, &TAPE_UUID);
    let _ = parse_sidecar_header_block(data, &TAPE_UUID);
    let _ = parse_sidecar_footer_block(data, &TAPE_UUID);

    // Whole-tape-file parse: first byte picks a block count (1..=16), the
    // remainder is chunked into that many equal blocks.
    let blocks_wanted = usize::from(data[0] % 16) + 1;
    let body = &data[1..];
    if body.is_empty() {
        return;
    }
    let block_len = (body.len() / blocks_wanted).max(1);
    let blocks: Vec<&[u8]> = body.chunks(block_len).take(blocks_wanted.max(1)).collect();
    if !blocks.is_empty() {
        let _ = parse_sidecar_tape_file(&blocks, &TAPE_UUID);
    }
});
