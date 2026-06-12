//! Remanence SCSI core — Layer 1.
//!
//! This crate provides:
//! - CDB construction and response parsing for the SCSI commands Remanence
//!   needs (starting with `INQUIRY`),
//! - a thin, safe wrapper around Linux's `SG_IO` ioctl for sending those
//!   CDBs to `/dev/sgN` devices.
//!
//! The parser half (`inquiry`, future `element_status`, etc.) is pure Rust
//! and portable — testable against captured fixtures from any host. The
//! transport half (`sg_io`) is Linux-only and gated behind `cfg(target_os
//! = "linux")` so that unit tests still build on macOS/CI.

#![warn(missing_docs)]

pub mod error;
pub mod initialize_element_status;
pub mod inquiry;
pub mod load_unload;
pub mod locate;
pub mod mode;
pub mod move_medium;
pub mod prevent_allow;
pub mod read_block_limits;
pub mod read_element_status;
pub mod read_position;
pub mod read_write;
pub mod rewind;
pub mod sense;
pub mod space;
pub mod vpd;
pub mod write_filemarks;

#[cfg(target_os = "linux")]
pub mod sg_io;

pub use error::ScsiError;
pub use inquiry::{DeviceType, Inquiry};
pub use read_element_status::{Element, ElementStatusData, ElementType};
pub use sense::{decode_sense, DecodedSense};
pub use vpd::{
    Association, CodeSet, DesignatorType, DeviceDesignator, DeviceIdentification, UnitSerial,
    VpdHeader,
};
