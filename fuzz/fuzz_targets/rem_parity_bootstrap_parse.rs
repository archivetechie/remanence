#![no_main]

//! Fuzz target for the REM-PARITY 1.0 bootstrap block parser — the entry the
//! catalog-less Scanner feeds raw tape blocks into (freeze criterion §18.3).
//! Robustness property only: no panic, no hang, no unbounded allocation.

use libfuzzer_sys::fuzz_target;
use remanence_parity::bootstrap::{has_bootstrap_magic, parse_bootstrap_block};

fuzz_target!(|data: &[u8]| {
    if data.len() > 1 << 21 {
        return;
    }
    let _ = has_bootstrap_magic(data);
    let _ = parse_bootstrap_block(data);
});
