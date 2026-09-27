#![no_main]

//! Fuzz target for the REM-OBJECT 1.0 manifest-profile CBOR decoder.
//!
//! The target intentionally validates only the deterministic-CBOR profile,
//! as the REM Implementation and Operations Guide, §2.5, recommends: fuzz the
//! manifest CBOR decoder as a separate target (docs/rem-implementation-guide.md).

use libfuzzer_sys::fuzz_target;
use remanence_format::validate_manifest_cbor_for_fuzz;

fuzz_target!(|data: &[u8]| {
    let _ = validate_manifest_cbor_for_fuzz(data);
});
