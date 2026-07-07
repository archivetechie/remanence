//! Linux `SG_IO` transport — a safe wrapper around the SCSI-generic ioctl.
//!
//! The `/dev/sgN` character devices expose a single ioctl number, `SG_IO`
//! (0x2285), that takes an `sg_io_hdr_t` struct describing the CDB,
//! optional in/out data buffers, a sense buffer, and a timeout. The kernel
//! issues the CDB to the target and fills in status/result fields.
//!
//! Three entry points cover the data directions Remanence needs:
//!
//! - [`execute_in`] — `SG_DXFER_FROM_DEV`. Issues the CDB and reads up
//!   to `data_in.len()` bytes back from the device. Used by discovery
//!   (INQUIRY, VPD, READ ELEMENT STATUS) and Layer 3a reads
//!   (READ POSITION, READ, READ BLOCK LIMITS).
//! - [`execute_none`] — `SG_DXFER_NONE`. Issues the CDB with no data
//!   phase in either direction. Used by Layer 2b state-changing
//!   primitives (MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT /
//!   ALLOW MEDIUM REMOVAL, SSC LOAD/UNLOAD) and Layer 3a positioning
//!   (REWIND, LOCATE, SPACE, WRITE FILEMARKS).
//! - [`execute_out`] — `SG_DXFER_TO_DEV`. Issues the CDB and sends up
//!   to `data_out.len()` bytes to the device. Used by Layer 3a writes
//!   (WRITE block, MODE SELECT).
//!
//! All three share the same error vocabulary on [`ScsiError`]:
//! `InvalidInput` for caller-side limit violations, `Io` for ioctl
//! failures, `CheckCondition` for target-reported CHECK CONDITION
//! (with sense bytes and bytes_transferred derived from `resid`), and
//! `TransportError` for host/driver/transport-level faults that the
//! ioctl reports without a CHECK CONDITION.

use core::ffi::{c_int, c_uint, c_ushort};
use std::fs::File;
use std::os::unix::io::AsRawFd;

use crate::error::ScsiError;

// `sg_io_hdr_t` from `/usr/include/scsi/sg.h`. We mirror it field-for-field
// rather than depending on a libc that may or may not expose it. All
// pointer fields are 8 bytes on x86_64.
#[repr(C)]
#[allow(non_camel_case_types, non_snake_case)]
struct sg_io_hdr_t {
    interface_id: c_int,    // 'S'
    dxfer_direction: c_int, // SG_DXFER_*
    cmd_len: u8,            // CDB length
    mx_sb_len: u8,          // sense buffer capacity
    iovec_count: c_ushort,  // scatter/gather entries; 0 = none
    dxfer_len: c_uint,      // data-transfer byte count
    dxferp: *mut u8,        // data buffer
    cmdp: *const u8,        // CDB
    sbp: *mut u8,           // sense buffer
    timeout: c_uint,        // ms; 0 = default, u32::MAX = infinite
    flags: c_uint,          // SG_FLAG_*
    pack_id: c_int,
    usr_ptr: *mut u8,
    status: u8, // raw SCSI status byte
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8, // sense bytes actually written
    host_status: c_ushort,
    driver_status: c_ushort,
    resid: c_int, // dxfer_len - actual_transferred
    duration: c_uint,
    info: c_uint,
}

// Compile-time guard: if anyone touches the struct above (reorders fields,
// drops one, picks the wrong type) this catches the regression at build
// time on x86_64. Numbers below match /usr/include/scsi/sg.h on Linux/glibc.
#[cfg(target_arch = "x86_64")]
const _SG_IO_HDR_T_LAYOUT_OK: () = {
    use core::mem::offset_of;

    assert!(core::mem::size_of::<sg_io_hdr_t>() == 88);
    assert!(core::mem::align_of::<sg_io_hdr_t>() == 8);
    assert!(offset_of!(sg_io_hdr_t, interface_id) == 0);
    assert!(offset_of!(sg_io_hdr_t, dxfer_direction) == 4);
    assert!(offset_of!(sg_io_hdr_t, cmd_len) == 8);
    assert!(offset_of!(sg_io_hdr_t, mx_sb_len) == 9);
    assert!(offset_of!(sg_io_hdr_t, iovec_count) == 10);
    assert!(offset_of!(sg_io_hdr_t, dxfer_len) == 12);
    assert!(offset_of!(sg_io_hdr_t, dxferp) == 16);
    assert!(offset_of!(sg_io_hdr_t, cmdp) == 24);
    assert!(offset_of!(sg_io_hdr_t, sbp) == 32);
    assert!(offset_of!(sg_io_hdr_t, timeout) == 40);
    assert!(offset_of!(sg_io_hdr_t, flags) == 44);
    assert!(offset_of!(sg_io_hdr_t, pack_id) == 48);
    assert!(offset_of!(sg_io_hdr_t, usr_ptr) == 56);
    assert!(offset_of!(sg_io_hdr_t, status) == 64);
    assert!(offset_of!(sg_io_hdr_t, masked_status) == 65);
    assert!(offset_of!(sg_io_hdr_t, msg_status) == 66);
    assert!(offset_of!(sg_io_hdr_t, sb_len_wr) == 67);
    assert!(offset_of!(sg_io_hdr_t, host_status) == 68);
    assert!(offset_of!(sg_io_hdr_t, driver_status) == 70);
    assert!(offset_of!(sg_io_hdr_t, resid) == 72);
    assert!(offset_of!(sg_io_hdr_t, duration) == 76);
    assert!(offset_of!(sg_io_hdr_t, info) == 80);
};

const SG_DXFER_NONE: c_int = -1;
const SG_DXFER_TO_DEV: c_int = -2;
const SG_DXFER_FROM_DEV: c_int = -3;
#[allow(dead_code)]
const SG_DXFER_TO_FROM_DEV: c_int = -4;
#[cfg(target_os = "linux")]
const SG_SET_RESERVED_SIZE: nix::libc::c_ulong = 0x2275;
#[cfg(target_os = "linux")]
const SG_GET_RESERVED_SIZE: nix::libc::c_ulong = 0x2272;

/// Sense buffer size we hand the kernel. 32 bytes covers fixed-format sense
/// (SPC-5 §4.5.3); modern descriptor-format sense can be longer but the
/// useful prefix still fits here.
const SENSE_BUF_LEN: u8 = 32;

/// SG `info` mask — when bit 0 is clear, the transport reported success.
/// See `<scsi/sg.h>` `SG_INFO_OK_MASK`.
const SG_INFO_OK_MASK: u32 = 0x01;

/// SCSI status byte for CHECK CONDITION (SAM-3 §5.3.1).
const STATUS_CHECK_CONDITION: u8 = 0x02;

/// Largest CDB the sg v3 interface accepts.
const MAX_CDB_LEN: usize = 16;

fn captured_sense(sense: &[u8; SENSE_BUF_LEN as usize], len: u8) -> Vec<u8> {
    sense.get(..len as usize).unwrap_or(&[]).to_vec()
}

fn classify_non_check_condition_failure(
    hdr: &sg_io_hdr_t,
    sense: &[u8; SENSE_BUF_LEN as usize],
) -> Option<ScsiError> {
    let info = hdr.info;
    let transport_ok =
        (info & SG_INFO_OK_MASK) == 0 && hdr.host_status == 0 && hdr.driver_status == 0;
    if hdr.status != 0 && transport_ok {
        return Some(ScsiError::UnexpectedStatus { status: hdr.status });
    }

    let transport_bad = (info & SG_INFO_OK_MASK) != 0
        || hdr.host_status != 0
        || hdr.driver_status != 0
        || hdr.status != 0;
    if transport_bad {
        return Some(ScsiError::TransportError {
            status: hdr.status,
            host_status: hdr.host_status,
            driver_status: hdr.driver_status,
            info,
            sense: captured_sense(sense, hdr.sb_len_wr),
        });
    }

    None
}

// SG_IO is defined in sg.h as a literal 0x2285 (no _IOC encoding of size).
// The macro expands to an unsafe `pub fn sg_io_ioctl(...)`; the wrapper
// `execute_in` is the only thing we expose. Allow missing_docs for the
// macro-generated function to keep the public API focused on `execute_in`.
#[allow(missing_docs)]
mod ioctl {
    use super::sg_io_hdr_t;
    use nix::ioctl_readwrite_bad;
    ioctl_readwrite_bad!(sg_io, 0x2285, sg_io_hdr_t);
}

/// Send a CDB to `dev` and read the response into `data_in`. Returns the
/// number of bytes actually transferred (`data_in.len() - residual`).
///
/// Error variants:
/// - [`ScsiError::InvalidInput`] — the caller's `cdb` or `data_in` violated
///   a documented limit of the sg v3 interface (1..=16-byte CDB, transfer
///   length must fit in a `u32`).
/// - [`ScsiError::Io`] — the kernel rejected the ioctl outright.
/// - [`ScsiError::CheckCondition`] — the target replied CHECK CONDITION;
///   the truncated REQUEST SENSE buffer is included.
/// - [`ScsiError::TransportError`] — the ioctl returned success but a
///   transport-layer field (host_status / driver_status / info) indicates
///   the request didn't complete cleanly (cable disconnect, bus reset,
///   timeout caught after the ioctl boundary, etc.).
///
/// `timeout_ms` of 0 lets the kernel pick its default.
pub fn execute_in(
    dev: &File,
    cdb: &[u8],
    data_in: &mut [u8],
    timeout_ms: u32,
) -> Result<usize, ScsiError> {
    // -- input bounds: catch silent truncation before the ioctl boundary.
    if cdb.is_empty() || cdb.len() > MAX_CDB_LEN {
        return Err(ScsiError::InvalidInput(
            "cdb length must be 1..=16 bytes (sg v3 limit)",
        ));
    }
    if data_in.len() > u32::MAX as usize {
        return Err(ScsiError::InvalidInput(
            "data_in must be <= u32::MAX bytes (SG_IO dxfer_len is c_uint)",
        ));
    }

    let mut sense = [0u8; SENSE_BUF_LEN as usize];
    let dxfer_len = data_in.len() as c_uint;

    let mut hdr = sg_io_hdr_t {
        interface_id: 'S' as c_int,
        dxfer_direction: SG_DXFER_FROM_DEV,
        cmd_len: cdb.len() as u8, // safe: bounds-checked above
        mx_sb_len: SENSE_BUF_LEN,
        iovec_count: 0,
        dxfer_len,
        dxferp: data_in.as_mut_ptr(),
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: timeout_ms,
        flags: 0,
        pack_id: 0,
        usr_ptr: core::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };

    // SAFETY: `hdr` is fully initialized above. The buffers pointed to by
    // `dxferp`, `cmdp`, and `sbp` outlive this call because they are stack
    // locals (`sense`) or borrowed for the duration (`data_in`, `cdb`). The
    // ioctl number 0x2285 matches the kernel's SG_IO definition.
    unsafe {
        ioctl::sg_io(dev.as_raw_fd(), &mut hdr).map_err(|e| ScsiError::Io(e.into()))?;
    }

    // CHECK CONDITION — the common, expected "soft" error. Status is the
    // raw SCSI status byte; high bits are reserved/vendor and the spec
    // defines status values masked at 0xFE.
    //
    // Compute bytes_transferred FIRST: CHECK CONDITION can be raised
    // after a partial data transfer (ILI on short read, EOM on write
    // — see IBM LTO SCSI Reference §4.12.1 / Table 17), and Layer 3a
    // needs the partial count. If resid is out of range we clamp to 0
    // rather than synthesise a fake count. Codex bb10e63f.
    if (hdr.status & 0xFE) == STATUS_CHECK_CONDITION {
        let bytes_transferred = if hdr.resid < 0 || (hdr.resid as i64) > (dxfer_len as i64) {
            0
        } else {
            dxfer_len - (hdr.resid as u32)
        };
        let sense = captured_sense(&sense, hdr.sb_len_wr);
        return Err(ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        });
    }

    // Transport-layer failure: ioctl said 0 but the kernel marked the
    // request as not OK, or the host/driver complained, or the SCSI status
    // byte was something other than GOOD (0) / CHECK_CONDITION.
    let info = hdr.info as u32;
    if let Some(error) = classify_non_check_condition_failure(&hdr, &sense) {
        return Err(error);
    }

    // Compute bytes transferred. resid can in principle be negative
    // (target sent more than we asked for) or larger than dxfer_len
    // (driver bug). Both cases indicate the kernel/driver/HBA are
    // misbehaving — surface it as a transport error rather than silently
    // returning a wrong count.
    if hdr.resid < 0 || (hdr.resid as i64) > (dxfer_len as i64) {
        return Err(ScsiError::TransportError {
            status: hdr.status,
            host_status: hdr.host_status as u16,
            driver_status: hdr.driver_status as u16,
            info,
            sense: Vec::new(),
        });
    }
    Ok((dxfer_len as usize) - (hdr.resid as usize))
}

/// Send a CDB with no data phase (`SG_DXFER_NONE`). For state-changing
/// SCSI commands whose operation lives entirely in the CDB bytes:
/// MOVE MEDIUM, INITIALIZE ELEMENT STATUS, PREVENT/ALLOW MEDIUM
/// REMOVAL, SSC LOAD/UNLOAD, and friends.
///
/// Returns `Ok(())` on success. Error variants mirror [`execute_in`]'s
/// vocabulary (InvalidInput, Io, CheckCondition, TransportError); the
/// "bytes transferred" surface doesn't apply since there is no data
/// phase to count.
pub fn execute_none(dev: &File, cdb: &[u8], timeout_ms: u32) -> Result<(), ScsiError> {
    if cdb.is_empty() || cdb.len() > MAX_CDB_LEN {
        return Err(ScsiError::InvalidInput(
            "cdb length must be 1..=16 bytes (sg v3 limit)",
        ));
    }

    let mut sense = [0u8; SENSE_BUF_LEN as usize];

    let mut hdr = sg_io_hdr_t {
        interface_id: 'S' as c_int,
        dxfer_direction: SG_DXFER_NONE,
        cmd_len: cdb.len() as u8, // safe: bounds-checked above
        mx_sb_len: SENSE_BUF_LEN,
        iovec_count: 0,
        dxfer_len: 0,
        dxferp: core::ptr::null_mut(),
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: timeout_ms,
        flags: 0,
        pack_id: 0,
        usr_ptr: core::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };

    // SAFETY: `hdr` is fully initialized above. `cmdp` and `sbp` outlive
    // the call (stack-local `sense` and borrowed `cdb`). `dxferp` is null
    // and the kernel won't dereference it because `dxfer_direction =
    // SG_DXFER_NONE` and `dxfer_len = 0`.
    unsafe {
        ioctl::sg_io(dev.as_raw_fd(), &mut hdr).map_err(|e| ScsiError::Io(e.into()))?;
    }

    if (hdr.status & 0xFE) == STATUS_CHECK_CONDITION {
        let sense = captured_sense(&sense, hdr.sb_len_wr);
        // execute_none has no data phase, so bytes_transferred is
        // structurally 0 — the field exists for API uniformity
        // with execute_in.
        return Err(ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        });
    }

    if let Some(error) = classify_non_check_condition_failure(&hdr, &sense) {
        return Err(error);
    }
    Ok(())
}

/// Send a CDB to `dev` and write `data_out` to the device. Returns
/// the number of bytes actually transferred (`data_out.len() -
/// residual`).
///
/// Error variants mirror [`execute_in`]'s vocabulary
/// (`InvalidInput`, `Io`, `CheckCondition`, `TransportError`). On
/// CHECK CONDITION the `bytes_transferred` field is computed from
/// `dxfer_len - resid` before bailing — Layer 3a's `write_block`
/// path needs the partial-transfer count to surface a faithful
/// `WriteOutcome::bytes_written` to the caller when the drive hits
/// EOM or early-warning mid-write.
pub fn execute_out(
    dev: &File,
    cdb: &[u8],
    data_out: &[u8],
    timeout_ms: u32,
) -> Result<usize, ScsiError> {
    if cdb.is_empty() || cdb.len() > MAX_CDB_LEN {
        return Err(ScsiError::InvalidInput(
            "cdb length must be 1..=16 bytes (sg v3 limit)",
        ));
    }
    if data_out.len() > u32::MAX as usize {
        return Err(ScsiError::InvalidInput(
            "data_out must be <= u32::MAX bytes (SG_IO dxfer_len is c_uint)",
        ));
    }

    let mut sense = [0u8; SENSE_BUF_LEN as usize];
    let dxfer_len = data_out.len() as c_uint;

    // SG_IO's `dxferp` is `*mut u8` regardless of direction. For
    // SG_DXFER_TO_DEV the kernel only reads from the pointer; we
    // can safely cast `&[u8]` to `*mut u8` because the kernel
    // honours the direction flag and does not write through it.
    let mut hdr = sg_io_hdr_t {
        interface_id: 'S' as c_int,
        dxfer_direction: SG_DXFER_TO_DEV,
        cmd_len: cdb.len() as u8,
        mx_sb_len: SENSE_BUF_LEN,
        iovec_count: 0,
        dxfer_len,
        dxferp: data_out.as_ptr() as *mut u8,
        cmdp: cdb.as_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: timeout_ms,
        flags: 0,
        pack_id: 0,
        usr_ptr: core::ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };

    // SAFETY: `hdr` is fully initialized above. The buffers pointed
    // to by `dxferp`, `cmdp`, and `sbp` outlive this call (stack
    // local `sense`, borrowed `data_out` and `cdb`). The direction
    // flag is SG_DXFER_TO_DEV so the kernel will only read from
    // `dxferp`, not write through it. Ioctl number 0x2285 matches
    // the kernel's SG_IO definition.
    unsafe {
        ioctl::sg_io(dev.as_raw_fd(), &mut hdr).map_err(|e| ScsiError::Io(e.into()))?;
    }

    if (hdr.status & 0xFE) == STATUS_CHECK_CONDITION {
        let bytes_transferred = if hdr.resid < 0 || (hdr.resid as i64) > (dxfer_len as i64) {
            0
        } else {
            dxfer_len - (hdr.resid as u32)
        };
        let sense = captured_sense(&sense, hdr.sb_len_wr);
        return Err(ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        });
    }

    let info = hdr.info as u32;
    if let Some(error) = classify_non_check_condition_failure(&hdr, &sense) {
        return Err(error);
    }

    if hdr.resid < 0 || (hdr.resid as i64) > (dxfer_len as i64) {
        return Err(ScsiError::TransportError {
            status: hdr.status,
            host_status: hdr.host_status as u16,
            driver_status: hdr.driver_status as u16,
            info,
            sense: Vec::new(),
        });
    }
    Ok((dxfer_len as usize) - (hdr.resid as usize))
}

/// Request a Linux sg reserved buffer size and return the actual size
/// granted by the kernel.
///
/// The sg driver may grant less than requested depending on HBA and
/// `max_sectors_kb` limits. Callers must treat the returned value as
/// authoritative for per-command DMA sizing.
#[cfg(target_os = "linux")]
pub fn set_reserved_size(dev: &File, requested_bytes: u32) -> Result<u32, ScsiError> {
    let mut requested: nix::libc::c_int = requested_bytes.try_into().map_err(|_| {
        ScsiError::InvalidInput("SG_SET_RESERVED_SIZE requested size must fit in c_int")
    })?;
    // SAFETY: SG_SET_RESERVED_SIZE expects the third ioctl argument to
    // point to an int. `requested` lives for the duration of the call.
    let rc = unsafe {
        nix::libc::ioctl(
            dev.as_raw_fd(),
            SG_SET_RESERVED_SIZE,
            &mut requested as *mut nix::libc::c_int,
        )
    };
    if rc < 0 {
        return Err(ScsiError::Io(std::io::Error::last_os_error()));
    }
    get_reserved_size(dev)
}

/// Query the actual Linux sg reserved buffer size.
#[cfg(target_os = "linux")]
pub fn get_reserved_size(dev: &File) -> Result<u32, ScsiError> {
    let mut size: nix::libc::c_int = 0;
    // SAFETY: SG_GET_RESERVED_SIZE writes an int through the third
    // ioctl argument. `size` is a valid out-parameter for this call.
    let rc = unsafe {
        nix::libc::ioctl(
            dev.as_raw_fd(),
            SG_GET_RESERVED_SIZE,
            &mut size as *mut nix::libc::c_int,
        )
    };
    if rc < 0 {
        return Err(ScsiError::Io(std::io::Error::last_os_error()));
    }
    if size < 0 {
        return Err(ScsiError::TransportError {
            status: 0,
            host_status: 0,
            driver_status: 0,
            info: 0,
            sense: Vec::new(),
        });
    }
    Ok(size as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn completion_hdr(status: u8, host_status: u16, driver_status: u16, info: u32) -> sg_io_hdr_t {
        sg_io_hdr_t {
            interface_id: 'S' as c_int,
            dxfer_direction: SG_DXFER_NONE,
            cmd_len: 0,
            mx_sb_len: SENSE_BUF_LEN,
            iovec_count: 0,
            dxfer_len: 0,
            dxferp: core::ptr::null_mut(),
            cmdp: core::ptr::null(),
            sbp: core::ptr::null_mut(),
            timeout: 0,
            flags: 0,
            pack_id: 0,
            usr_ptr: core::ptr::null_mut(),
            status,
            masked_status: 0,
            msg_status: 0,
            sb_len_wr: 0,
            host_status: host_status as c_ushort,
            driver_status: driver_status as c_ushort,
            resid: 0,
            duration: 0,
            info: info as c_uint,
        }
    }

    #[test]
    fn classifier_keeps_target_busy_separate_from_transport_errors() {
        let sense = [0u8; SENSE_BUF_LEN as usize];
        let hdr = completion_hdr(0x08, 0, 0, 0);

        let error =
            classify_non_check_condition_failure(&hdr, &sense).expect("BUSY status should fail");

        assert!(matches!(
            error,
            ScsiError::UnexpectedStatus { status: 0x08 }
        ));
    }

    #[test]
    fn classifier_keeps_host_failures_as_transport_errors() {
        let sense = [0u8; SENSE_BUF_LEN as usize];
        let hdr = completion_hdr(0x08, 0x0001, 0, 0);

        let error =
            classify_non_check_condition_failure(&hdr, &sense).expect("host failure should fail");

        assert!(matches!(
            error,
            ScsiError::TransportError {
                status: 0x08,
                host_status: 0x0001,
                ..
            }
        ));
    }

    #[test]
    fn rejects_empty_cdb() {
        // /dev/null is fine as a file handle — execute_in returns
        // InvalidInput before any ioctl is attempted.
        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let mut buf = [0u8; 4];
        let r = execute_in(&dev, &[], &mut buf, 0);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))), "got {r:?}");
    }

    #[test]
    fn rejects_oversize_cdb() {
        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let cdb = [0u8; 17];
        let mut buf = [0u8; 4];
        let r = execute_in(&dev, &cdb, &mut buf, 0);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))), "got {r:?}");
    }

    #[test]
    fn execute_none_rejects_empty_cdb() {
        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let r = execute_none(&dev, &[], 0);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))), "got {r:?}");
    }

    #[test]
    fn execute_none_rejects_oversize_cdb() {
        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let cdb = [0u8; 17];
        let r = execute_none(&dev, &cdb, 0);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))), "got {r:?}");
    }

    #[test]
    fn execute_out_rejects_empty_cdb() {
        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let payload = [0u8; 4];
        let r = execute_out(&dev, &[], &payload, 0);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))), "got {r:?}");
    }

    #[test]
    fn execute_out_rejects_oversize_cdb() {
        let dev = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let cdb = [0u8; 17];
        let payload = [0u8; 4];
        let r = execute_out(&dev, &cdb, &payload, 0);
        assert!(matches!(r, Err(ScsiError::InvalidInput(_))), "got {r:?}");
    }
}
