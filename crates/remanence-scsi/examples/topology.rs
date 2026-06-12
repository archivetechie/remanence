//! End-to-end Layer 1 demo: open a changer `/dev/sgN`, fetch its INQUIRY +
//! READ ELEMENT STATUS, print a topology view of the library.
//!
//! Usage:
//!     sudo cargo run --example topology -- /dev/sg4
//!     sudo target/debug/examples/topology /dev/sg4
//!
//! The output mirrors the SMC-3 element model: one line per element, with
//! type / address / full-or-empty / volume tag / source slot (where the
//! cartridge in a drive came from) / drive serial (when DVCID descriptors
//! are present).

use std::env;
use std::fs::OpenOptions;
use std::process::ExitCode;

use remanence_scsi::{
    inquiry, read_element_status as res, sg_io, vpd, DeviceType, ElementType, Inquiry, ScsiError,
    UnitSerial,
};

fn type_letter(t: ElementType) -> &'static str {
    match t {
        ElementType::MediumTransport => "R", // robot
        ElementType::DataTransfer => "D",    // drive
        ElementType::Storage => "S",         // slot
        ElementType::ImportExport => "I",    // ie port
        ElementType::Other(_) => "?",
    }
}

fn run() -> Result<(), ScsiError> {
    let path = env::args().nth(1).unwrap_or_else(|| "/dev/sg4".into());
    // Same read-only-first dance as examples/inquiry.rs — Linux SG_IO has
    // accepted O_RDONLY for FROM_DEV calls since 2.6.18.
    let dev = match OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(_) => OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(ScsiError::Io)?,
    };

    // ---- identify the device first -------------------------------------
    let cdb = inquiry::build_cdb(inquiry::ALLOC_LEN);
    let mut buf = vec![0u8; inquiry::ALLOC_LEN as usize];
    let n = sg_io::execute_in(&dev, &cdb, &mut buf, 5_000)?;
    let std_inq = Inquiry::parse(&buf[..n])?;
    if std_inq.device_type != DeviceType::MediumChanger {
        eprintln!(
            "warn: {path} is {:?}, not a medium changer — RES will likely fail",
            std_inq.device_type
        );
    }

    let cdb = inquiry::build_cdb_vpd(vpd::PAGE_UNIT_SERIAL, vpd::ALLOC_LEN);
    let mut buf = vec![0u8; vpd::ALLOC_LEN as usize];
    let n = sg_io::execute_in(&dev, &cdb, &mut buf, 5_000)?;
    let serial = UnitSerial::parse(&buf[..n])?;

    // ---- READ ELEMENT STATUS ------------------------------------------
    let cdb = res::build_cdb(
        /* element_type             */ 0, // all
        /* starting_element_address */ 0,
        /* num_elements             */ res::SAFE_NUM_ELEMENTS,
        /* voltag                   */ true,
        /* dvcid                    */ true,
        /* curdata                  */ true, // empirically required for DVCID block
        /* alloc_len                */ res::SAFE_ALLOC_LEN,
    );
    let mut buf = vec![0u8; res::SAFE_ALLOC_LEN as usize];
    let n = sg_io::execute_in(&dev, &cdb, &mut buf, 30_000)?;
    let data = res::parse(&buf[..n])?;

    // ---- print ----------------------------------------------------------
    println!(
        "Library:  {}  {} {} {}",
        path,
        std_inq.vendor_str(),
        std_inq.product_str(),
        std_inq.revision_str(),
    );
    println!("Serial:   {}", serial.as_str());
    println!(
        "Elements: {} reported (first address 0x{:04x})",
        data.num_elements, data.first_element_address
    );

    // Per-type summary line
    let robots: usize = data.by_type(ElementType::MediumTransport).count();
    let drives: usize = data.by_type(ElementType::DataTransfer).count();
    let slots: usize = data.by_type(ElementType::Storage).count();
    let ieports: usize = data.by_type(ElementType::ImportExport).count();
    let full: usize = data.elements.iter().filter(|e| e.full).count();
    println!("Layout:   {robots} robot(s), {drives} drive(s), {slots} slot(s), {ieports} IE port(s), {full} full\n");

    println!("  TYPE  ADDR    STATE   VOLTAG          SOURCE   SERIAL");
    for e in &data.elements {
        let voltag = e.primary_voltag.as_deref().unwrap_or("");
        let source = e
            .source_address
            .map(|a| format!("0x{a:04x}"))
            .unwrap_or_default();
        let serial = e.drive_serial.as_deref().unwrap_or("");
        let state = if e.full { "full" } else { "empty" };
        println!(
            "  {:<5} 0x{:04x}  {:<6}  {:<14}  {:<7}  {}",
            type_letter(e.element_type),
            e.address,
            state,
            voltag,
            source,
            serial
        );
    }
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
