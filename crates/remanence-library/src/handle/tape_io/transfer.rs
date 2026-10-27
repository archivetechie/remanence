//! SSC READ/WRITE byte accounting at the command's transfer-unit boundary.

use super::{map_scsi, ScsiError, TapeIoError};
use remanence_scsi::decode_sense;

/// Use valid current sense only when the kernel count equals the request
/// (zero kernel residue). A unit is one byte for FIXED=0 or one block for
/// FIXED=1. Saturation bounds oversized residues; negative variable ILI
/// residues describe oversized records and leave the full buffer count intact.
pub(crate) fn transfer_bytes_from_sense(
    sense: &[u8],
    kernel_bytes: u32,
    requested: u32,
    block_size: Option<u32>,
) -> u32 {
    if kernel_bytes != requested {
        return kernel_bytes;
    }
    let Some(decoded) = decode_sense(sense) else {
        return kernel_bytes;
    };
    if decoded.is_deferred() || !decoded.valid {
        return kernel_bytes;
    }
    let Some(residue) = information(sense) else {
        return kernel_bytes;
    };
    if block_size.is_none() && decoded.ili {
        let negative = match decoded.response_code {
            0x70 => residue & (1 << 31) != 0,
            0x72 => residue & (1 << 63) != 0,
            _ => false,
        };
        if negative {
            return requested;
        }
    }
    u64::from(requested).saturating_sub(residue.saturating_mul(u64::from(block_size.unwrap_or(1))))
        as u32
}

/// Read a complete, valid information field within the received sense bytes.
/// Fixed-format information precedes the additional-sense-length boundary.
fn information(sense: &[u8]) -> Option<u64> {
    match sense.first()? & 0x7f {
        0x70 if sense[0] & 0x80 != 0 => Some(u64::from(u32::from_be_bytes(
            sense.get(3..7)?.try_into().ok()?,
        ))),
        0x72 => {
            let end = (8 + usize::from(*sense.get(7)?)).min(sense.len());
            let mut offset = 8;
            while offset + 2 <= end {
                let next = offset + 2 + usize::from(sense[offset + 1]);
                if next > end {
                    return None;
                }
                if sense[offset] == 0 && next - offset >= 12 && sense[offset + 2] & 0x80 != 0 {
                    return Some(u64::from_be_bytes(
                        sense[offset + 4..offset + 12].try_into().ok()?,
                    ));
                }
                offset = next;
            }
            None
        }
        _ => None,
    }
}

/// Preserve transport diagnostics while carrying corrected physical bytes in
/// the library error. Specialized state errors keep their normal classification.
pub(super) fn map_transfer_scsi(
    err: ScsiError,
    requested: u32,
    block_size: Option<u32>,
) -> TapeIoError {
    match map_scsi(err) {
        TapeIoError::CheckCondition(source @ ScsiError::CheckCondition { .. }) => {
            let ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            } = &source
            else {
                unreachable!()
            };
            let corrected =
                transfer_bytes_from_sense(sense, *bytes_transferred, requested, block_size);
            if corrected == *bytes_transferred {
                TapeIoError::CheckCondition(source)
            } else {
                TapeIoError::CheckConditionWithResidue {
                    source,
                    bytes_transferred: corrected,
                }
            }
        }
        mapped => mapped,
    }
}

/// Decode boundary/error completions without the verification reader's LOCATE redo.
/// Only complete records in a current, aligned filemark/EOD response are credited.
pub fn verification_read_completion(
    error: &TapeIoError,
    size: u32,
    requested: u32,
) -> (u32, &'static str) {
    if size == 0 || requested == 0 {
        return (0, "io_error");
    }
    let source = match error {
        TapeIoError::CheckCondition(source) => source,
        TapeIoError::CheckConditionWithResidue { source, .. } => source,
        TapeIoError::GoodReadLengthMismatch { .. } | TapeIoError::ReadBufferTooSmall { .. } => {
            return (0, "wrong_length")
        }
        TapeIoError::OperationFailed(message)
            if message.starts_with("unclean verification batch:") =>
        {
            return (0, "unclean_read")
        }
        _ => return (0, "io_error"),
    };
    let ScsiError::CheckCondition {
        sense,
        bytes_transferred: kernel_bytes,
    } = source
    else {
        return (0, "io_error");
    };
    let Some(sense) = decode_sense(sense) else {
        return (0, "io_error");
    };
    if sense.is_deferred() {
        return (
            0,
            if (sense.key == 0 && sense.filemark && !sense.ili && !sense.eom) || sense.key == 8 {
                "unconfirmed_boundary"
            } else {
                "io_error"
            },
        );
    }
    if sense.ili {
        return (0, "wrong_length");
    }
    if sense.key == 3 {
        return (0, "medium_error");
    }
    if sense.key == 1 {
        return (0, "recovered_error");
    }
    let boundary = if sense.key == 0 && sense.filemark && !sense.eom {
        "filemark"
    } else if sense.key == 8 {
        "end_of_data"
    } else {
        return (0, "io_error");
    };
    // Some hosts omit the kernel residual. Otherwise both independent counts
    // must confirm the same whole-block prefix; buffer capacity is not evidence.
    let residue = match source {
        ScsiError::CheckCondition { sense: bytes, .. } if sense.valid => information(bytes),
        _ => None,
    };
    let records = residue.and_then(|r| u64::from(requested).checked_sub(r));
    if let Some(records) = records {
        let full_bytes = u64::from(size) * u64::from(requested);
        let kernel_bytes = u64::from(*kernel_bytes);
        if kernel_bytes == full_bytes
            || (kernel_bytes % u64::from(size) == 0 && kernel_bytes / u64::from(size) == records)
        {
            return (records as u32, boundary);
        }
    }
    (0, "unconfirmed_boundary")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirm only aligned, agreeing counts or the host's omitted residual.
    #[test]
    fn boundary_credit_requires_confirmed_whole_blocks() {
        for flags in [0x80, 8] {
            for (kernel, residue, valid, expected) in [
                (8, 2, true, Some(2)),
                (16, 2, true, Some(2)), // field host omits kernel residual
                (8, 1, true, None),     // kernel 2, sense 3
                (9, 2, true, None),     // non-whole kernel count
                (16, 5, true, None),    // residue exceeds request
                (16, 2, false, None),
                (8, 2, false, None),
                (0, 4, true, Some(0)),
                (4, 2, true, None),
                (12, 2, true, None),
                (20, 2, true, None),
                (16, 0, true, Some(4)),
            ] {
                let mut sense = vec![0; 18];
                sense[0] = if valid { 0xf0 } else { 0x70 };
                sense[2] = flags;
                sense[3..7].copy_from_slice(&u32::to_be_bytes(residue));
                sense[7] = 10;
                let error = TapeIoError::CheckCondition(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: kernel,
                });
                assert_eq!(
                    verification_read_completion(&error, 4, 4),
                    (
                        expected.unwrap_or(0),
                        if expected.is_none() {
                            "unconfirmed_boundary"
                        } else if flags == 8 {
                            "end_of_data"
                        } else {
                            "filemark"
                        }
                    ),
                    "kernel={kernel}, residue={residue}, valid={valid}"
                );
            }
        }
    }
    #[test]
    fn boundary_credit_rejects_deferred_sense_and_uses_original_kernel_count() {
        for response in [0xf0, 0xf1] {
            for kernel in [8, 12, 16] {
                let mut sense = vec![0; 18];
                sense[0] = response;
                sense[2] = 0x80;
                sense[3..7].copy_from_slice(&1u32.to_be_bytes());
                sense[7] = 10;
                let error = TapeIoError::CheckConditionWithResidue {
                    source: ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: kernel,
                    },
                    bytes_transferred: 12,
                };
                let expected = if response == 0xf0 && kernel != 8 {
                    (3, "filemark")
                } else {
                    (0, "unconfirmed_boundary")
                };
                assert_eq!(verification_read_completion(&error, 4, 4), expected);
            }
        }
    }
}
