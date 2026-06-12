//! Error types for the SCSI layer.

use thiserror::Error;

/// Top-level error for all SCSI operations.
#[derive(Debug, Error)]
pub enum ScsiError {
    /// A response was shorter than the SCSI spec requires.
    #[error("response too short: got {got} bytes, need at least {need}")]
    Truncated {
        /// How many bytes we actually received.
        got: usize,
        /// How many bytes the spec requires.
        need: usize,
    },

    /// A field in the response had a value the spec does not allow here.
    #[error("invalid response value at byte {offset}: {detail}")]
    InvalidResponse {
        /// Byte offset of the offending field in the response buffer.
        offset: usize,
        /// Human-readable description of what was wrong.
        detail: &'static str,
    },

    /// The OS rejected the ioctl, or the device returned a check-condition
    /// with sense data. Currently only emitted on Linux from `sg_io`.
    #[cfg(target_os = "linux")]
    #[error("SG_IO ioctl failed: {0}")]
    Io(#[from] std::io::Error),

    /// The target responded with CHECK CONDITION; the sense buffer is
    /// included for the caller to decode if they need to.
    ///
    /// `bytes_transferred` is the kernel-reported count of bytes that
    /// actually moved across the SG_IO data phase before the target
    /// raised CHECK CONDITION — computed from `dxfer_len - resid`.
    /// LTO drives frequently return CHECK CONDITION mid-transfer
    /// (ILI on a short read, EOM on write, etc.), and Layer 3a needs
    /// the partial-transfer count to surface a faithful
    /// `WriteOutcome::bytes_written` / `read_block` length to the
    /// caller. Always 0 for `execute_none` (no data phase).
    /// Codex bb10e63f flagged the earlier sense-only variant as
    /// lossy on the WRITE-EOM path.
    #[cfg(target_os = "linux")]
    #[error("SCSI check condition (bytes_transferred={bytes_transferred}): sense={sense:02x?}")]
    CheckCondition {
        /// Up to 32 bytes of REQUEST SENSE data the HBA captured for us.
        sense: Vec<u8>,
        /// Bytes that crossed the data phase before CHECK CONDITION.
        bytes_transferred: u32,
    },

    /// The target returned a SCSI status byte other than GOOD or CHECK
    /// CONDITION while the host/driver transport itself reported success.
    /// This keeps target-level states such as BUSY, RESERVATION CONFLICT,
    /// and TASK SET FULL distinct from cable/HBA/driver transport faults.
    #[cfg(target_os = "linux")]
    #[error("unexpected SCSI status: status=0x{status:02x}")]
    UnexpectedStatus {
        /// Raw SCSI status byte from the device.
        status: u8,
    },

    /// The SG_IO ioctl returned 0 but a status field in the response header
    /// indicates the transport failed (host adapter error, driver error, or
    /// the SG `info` "ok" bit was clear). The kernel hands back a response
    /// like this for cable disconnects, bus resets, and timeouts that the
    /// driver caught after we passed the ioctl boundary.
    #[cfg(target_os = "linux")]
    #[error(
        "SG_IO transport error: status=0x{status:02x} host_status=0x{host_status:04x} \
         driver_status=0x{driver_status:04x} info=0x{info:08x}"
    )]
    TransportError {
        /// Raw SCSI status byte from the device.
        status: u8,
        /// Host-adapter status (SG_ERR_DID_*).
        host_status: u16,
        /// Linux SCSI driver status (SG_ERR_DRIVER_*).
        driver_status: u16,
        /// SG `info` field; bit 0 (SG_INFO_OK_MASK) is clear when something
        /// went wrong above the SCSI status byte.
        info: u32,
        /// Sense bytes, if the driver captured any.
        sense: Vec<u8>,
    },

    /// A caller-supplied argument violated a documented constraint of the
    /// transport. Distinct from [`ScsiError::InvalidResponse`], which is
    /// about bytes coming back from the device.
    #[error("invalid argument: {0}")]
    InvalidInput(&'static str),
}
