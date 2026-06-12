//! End-to-end example: open `/dev/sgN`, send a standard INQUIRY and the
//! Unit Serial Number VPD (page 0x80), print both parsed responses.
//!
//! Usage:
//!     cargo run --example inquiry -- /dev/sg0
//!     sudo target/debug/examples/inquiry /dev/sg4   # for the changer
//!
//! `/dev/sgN` is typically root-only, so run via sudo (or add yourself to
//! the `disk` group on Linux).

use std::env;
use std::fs::OpenOptions;
use std::process::ExitCode;

use remanence_scsi::{inquiry, sg_io, vpd, Inquiry, ScsiError, UnitSerial};

fn run() -> Result<(), ScsiError> {
    let path = env::args().nth(1).unwrap_or_else(|| "/dev/sg0".into());
    // SG_IO classically required an O_RDWR fd, but Linux >= 2.6.18 accepts
    // O_RDONLY for FROM_DEV transfers. Try read-only first; fall back to
    // r/w if the kernel rejects it.
    let dev = match OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(_) => OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(ScsiError::Io)?,
    };

    // 1. Standard INQUIRY -------------------------------------------------
    let cdb = inquiry::build_cdb(inquiry::ALLOC_LEN);
    let mut buf = vec![0u8; inquiry::ALLOC_LEN as usize];
    let n = sg_io::execute_in(&dev, &cdb, &mut buf, 5_000)?;
    let std_inq = Inquiry::parse(&buf[..n])?;

    // 2. VPD page 0x80 (unit serial) -------------------------------------
    let cdb = inquiry::build_cdb_vpd(vpd::PAGE_UNIT_SERIAL, vpd::ALLOC_LEN);
    let mut buf = vec![0u8; vpd::ALLOC_LEN as usize];
    let n = sg_io::execute_in(&dev, &cdb, &mut buf, 5_000)?;
    let serial = UnitSerial::parse(&buf[..n])?;

    println!("Device:   {path}");
    println!("  type:     {:?}", std_inq.device_type);
    println!("  vendor:   {:?}", std_inq.vendor_str());
    println!("  product:  {:?}", std_inq.product_str());
    println!("  revision: {:?}", std_inq.revision_str());
    println!("  serial:   {:?}", serial.as_str());
    println!("  rmb:      {}", std_inq.removable);
    println!("  version:  0x{:02x}", std_inq.version);
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
