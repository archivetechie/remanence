#![no_main]

//! Fuzz target for the RAO 1.0 manifest-profile CBOR decoder.
//!
//! The target intentionally validates only the deterministic-CBOR profile,
//! matching the Section 14.8 requirement to fuzz both CBOR decoders separately
//! from full manifest schema semantics.

use libfuzzer_sys::fuzz_target;
use remanence_format::validate_manifest_cbor_for_fuzz;

fuzz_target!(|data: &[u8]| {
    let _ = validate_manifest_cbor_for_fuzz(data);
});
