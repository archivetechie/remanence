//! Operator-path smoke test for the supported load -> drive-pinned verify
//! composition. The ignored test is intentionally strict when selected: all
//! required QuadStor/daemon coordinates must be supplied, and failures panic.

use std::process::Command;

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set for this ignored test"))
}

#[test]
#[ignore = "requires a configured rem-daemon and safe QuadStor VTL cartridge"]
fn rem_load_then_drive_pinned_verify_roundtrips() {
    let endpoint = required("REM_QUADSTOR_CROSS_DRIVE_ENDPOINT");
    let library = required("REM_QUADSTOR_CROSS_DRIVE_LIBRARY");
    let bay = required("REM_QUADSTOR_CROSS_DRIVE_BAY");
    let locator = required("REM_QUADSTOR_CROSS_DRIVE_LOCATOR");
    let expected = required("REM_QUADSTOR_CROSS_DRIVE_EXPECTED_SHA256");
    let barcode = std::env::var("REM_QUADSTOR_CROSS_DRIVE_BARCODE").ok();
    let slot = std::env::var("REM_QUADSTOR_CROSS_DRIVE_SLOT").ok();
    assert_ne!(
        barcode.is_some(),
        slot.is_some(),
        "set exactly one of REM_QUADSTOR_CROSS_DRIVE_BARCODE or REM_QUADSTOR_CROSS_DRIVE_SLOT"
    );

    let mut load = Command::new(env!("CARGO_BIN_EXE_rem"));
    load.args([
        "drive",
        "load",
        "--endpoint",
        endpoint.as_str(),
        "--library",
        library.as_str(),
        "--bay",
        bay.as_str(),
        "--json",
    ]);
    if let Some(barcode) = barcode.as_deref() {
        load.args(["--barcode", barcode]);
    }
    if let Some(slot) = slot.as_deref() {
        load.args(["--slot", slot]);
    }
    let load = load.output().expect("run supported drive load verb");
    assert!(
        load.status.success(),
        "drive load failed: stdout={} stderr={}",
        String::from_utf8_lossy(&load.stdout),
        String::from_utf8_lossy(&load.stderr)
    );

    let verify = Command::new(env!("CARGO_BIN_EXE_rem"))
        .args([
            "archive",
            "verify",
            "--endpoint",
            endpoint.as_str(),
            "--library",
            library.as_str(),
            "--drive",
            bay.as_str(),
            "--locator",
            locator.as_str(),
            "--expected-sha256",
            expected.as_str(),
        ])
        .output()
        .expect("run supported drive-pinned verify verb");
    assert!(
        verify.status.success(),
        "drive-pinned verify failed: stdout={} stderr={}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
}
