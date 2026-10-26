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
