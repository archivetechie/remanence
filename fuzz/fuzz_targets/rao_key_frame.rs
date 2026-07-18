#![no_main]

//! Fuzz target for direct wrapped-DEK key-frame parsing.

use libfuzzer_sys::fuzz_target;
use remanence_aead::KeyFrame;

fuzz_target!(|data: &[u8]| {
    let _ = KeyFrame::parse(data);
});
