#![no_main]

//! Fuzz target for the REM-PARITY 1.0 parity_map parsers: header and footer
//! blocks plus the whole-tape-file parse with its payload digest and §10.5
//! directory-invariant checks (freeze §18.3). Robustness property only.

use libfuzzer_sys::fuzz_target;
use remanence_parity::{
    classify_parity_map_header_block, parse_parity_map_footer_block,
    parse_parity_map_header_block, parse_parity_map_tape_file,
};

const TAPE_UUID: [u8; 16] = *b"rem-fuzz-tape-01";

fuzz_target!(|data: &[u8]| {
    if data.len() > 1 << 21 || data.is_empty() {
        return;
    }
    let _ = classify_parity_map_header_block(data, &TAPE_UUID);
    let _ = parse_parity_map_header_block(data, &TAPE_UUID);
    let _ = parse_parity_map_footer_block(data, &TAPE_UUID);

    let blocks_wanted = usize::from(data[0] % 16) + 1;
    let body = &data[1..];
    if body.is_empty() {
        return;
    }
    let block_len = (body.len() / blocks_wanted).max(1);
    let blocks: Vec<Vec<u8>> = body
        .chunks(block_len)
        .take(blocks_wanted.max(1))
        .map(<[u8]>::to_vec)
        .collect();
    if !blocks.is_empty() {
        let _ = parse_parity_map_tape_file(&blocks, &TAPE_UUID);
    }
});
