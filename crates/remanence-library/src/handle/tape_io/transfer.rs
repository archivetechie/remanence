//! SSC READ/WRITE byte accounting at the command's transfer-unit boundary.

use super::{map_scsi, ScsiError, TapeIoError};
/// Delegate raw completions to the outcome classifier's sense-residual rule.
pub(crate) fn transfer_bytes_from_sense(
    sense: &[u8],
    kernel_bytes: u32,
    requested: u32,
    block_size: Option<u32>,
) -> u32 {
    crate::outcome::data_transfer_bytes_from_sense(sense, kernel_bytes, requested, block_size)
}

/// Use the same rule with already-normalized command evidence.
pub(crate) fn transfer_bytes_from_evidence(
    evidence: &crate::outcome::CompletionEvidence,
    kernel_bytes: u32,
    requested: u32,
    block_size: Option<u32>,
) -> u32 {
    evidence.data_transfer_bytes(u64::from(kernel_bytes), u64::from(requested), block_size) as u32
}

/// Preserve transport diagnostics while carrying corrected physical bytes in
/// the library error. Specialized state errors keep their normal classification.
pub(super) fn map_transfer_scsi(
    err: ScsiError,
    requested: u32,
    block_size: Option<u32>,
) -> TapeIoError {
    match map_scsi(err) {
        crate::TapeIoError {
            kind: crate::TapeIoErrorKind::CheckCondition(source @ ScsiError::CheckCondition { .. }),
            ..
        } => {
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
                crate::TapeIoError::pending(crate::TapeIoErrorKind::CheckCondition(source))
            } else {
                crate::TapeIoError::pending(crate::TapeIoErrorKind::CheckConditionWithResidue {
                    source,
                    bytes_transferred: corrected,
                })
            }
        }
        mapped => mapped,
    }
}

/// Typed crediting result for a fixed verification READ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationReadCompletion {
    /// The completion is io error.
    IoError,
    /// The completion is wrong length.
    WrongLength,
    /// The completion is unclean read.
    UncleanRead,
    /// The completion is unconfirmed boundary.
    UnconfirmedBoundary,
    /// The completion is medium error.
    MediumError,
    /// The completion is recovered error.
    RecoveredError,
    /// The completion is filemark.
    Filemark,
    /// The completion is end of data.
    EndOfData,
}

impl VerificationReadCompletion {
    /// Stable diagnostic label.
    pub fn name(self) -> &'static str {
        match self {
            Self::IoError => "io_error",
            Self::WrongLength => "wrong_length",
            Self::UncleanRead => "unclean_read",
            Self::UnconfirmedBoundary => "unconfirmed_boundary",
            Self::MediumError => "medium_error",
            Self::RecoveredError => "recovered_error",
            Self::Filemark => "filemark",
            Self::EndOfData => "end_of_data",
        }
    }
}

/// Decode boundary/error completions without the verification reader's LOCATE redo.
/// Only complete records in a current, aligned filemark/EOD response are credited.
pub fn verification_read_completion(
    error: &TapeIoError,
    size: u32,
    requested: u32,
) -> (u32, VerificationReadCompletion) {
    if size == 0 || requested == 0 {
        return (0, VerificationReadCompletion::IoError);
    }
    let source = match error {
        crate::TapeIoError {
            kind: crate::TapeIoErrorKind::CheckCondition(source),
            ..
        } => source,
        crate::TapeIoError {
            kind: crate::TapeIoErrorKind::CheckConditionWithResidue { source, .. },
            ..
        } => source,
        crate::TapeIoError {
            kind: crate::TapeIoErrorKind::GoodReadLengthMismatch { .. },
            ..
        }
        | crate::TapeIoError {
            kind: crate::TapeIoErrorKind::ReadBufferTooSmall { .. },
            ..
        } => return (0, VerificationReadCompletion::WrongLength),
        crate::TapeIoError {
            kind: crate::TapeIoErrorKind::OperationFailed(message),
            ..
        } if message.starts_with("unclean verification batch:") => {
            return (0, VerificationReadCompletion::UncleanRead)
        }
        _ => return (0, VerificationReadCompletion::IoError),
    };
    let ScsiError::CheckCondition {
        bytes_transferred: kernel_bytes,
        ..
    } = source
    else {
        return (0, VerificationReadCompletion::IoError);
    };
    let evidence = error.completion_evidence();
    let Some(sense) = evidence.decoded else {
        return (0, VerificationReadCompletion::IoError);
    };
    if sense.is_deferred() {
        return (
            0,
            if (sense.key == 0 && sense.filemark && !sense.ili && !sense.eom) || sense.key == 8 {
                VerificationReadCompletion::UnconfirmedBoundary
            } else {
                VerificationReadCompletion::IoError
            },
        );
    }
    if sense.ili {
        return (0, VerificationReadCompletion::WrongLength);
    }
    if sense.key == 3 {
        return (0, VerificationReadCompletion::MediumError);
    }
    if sense.key == 1 {
        return (0, VerificationReadCompletion::RecoveredError);
    }
    let boundary = if sense.key == 0 && sense.filemark && !sense.eom {
        VerificationReadCompletion::Filemark
    } else if sense.key == 8 {
        VerificationReadCompletion::EndOfData
    } else {
        return (0, VerificationReadCompletion::IoError);
    };
    // A valid residual proves the whole-record prefix. Resolve host disagreement
    // with the same classifier rule used by ordinary and pipelined transfers.
    let records = evidence
        .residual
        .filter(|&r| r >= 1)
        .and_then(|r| u64::from(requested).checked_sub(r));
    if let Some(records) = records {
        let bytes = evidence.data_transfer_bytes(
            u64::from(*kernel_bytes),
            u64::from(size) * u64::from(requested),
            Some(size),
        );
        if bytes == records * u64::from(size) {
            return (records as u32, boundary);
        }
    }
    (0, VerificationReadCompletion::UnconfirmedBoundary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Confirm valid whole-block sense counts even when SG disagrees.
    #[test]
    fn boundary_credit_requires_confirmed_whole_blocks() {
        for flags in [0x80, 8] {
            for (kernel, residue, valid, expected) in [
                (8, 2, true, Some(2)),
                (16, 2, true, Some(2)), // field host omits kernel residual
                (8, 1, true, Some(3)),  // sense wins over kernel 2
                (9, 2, true, Some(2)),  // sense wins over non-whole kernel count
                (16, 5, true, None),    // residue exceeds request
                (16, 2, false, None),
                (8, 2, false, None),
                (0, 4, true, Some(0)),
                (4, 2, true, Some(2)),
                (12, 2, true, Some(2)),
                (20, 2, true, Some(2)),
                (16, 0, true, None),
            ] {
                let mut sense = vec![0; 18];
                sense[0] = if valid { 0xf0 } else { 0x70 };
                sense[2] = flags;
                sense[3..7].copy_from_slice(&u32::to_be_bytes(residue));
                sense[7] = 10;
                let error = crate::TapeIoError::new(crate::TapeIoErrorKind::CheckCondition(
                    ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: kernel,
                    },
                ));
                assert_eq!(
                    verification_read_completion(&error, 4, 4),
                    (
                        expected.unwrap_or(0),
                        if expected.is_none() {
                            VerificationReadCompletion::UnconfirmedBoundary
                        } else if flags == 8 {
                            VerificationReadCompletion::EndOfData
                        } else {
                            VerificationReadCompletion::Filemark
                        }
                    ),
                    "kernel={kernel}, residue={residue}, valid={valid}"
                );
            }
        }
    }
    #[test]
    fn boundary_credit_rejects_deferred_sense_and_uses_valid_residual() {
        for response in [0xf0, 0xf1] {
            for kernel in [8, 12, 16] {
                let mut sense = vec![0; 18];
                sense[0] = response;
                sense[2] = 0x80;
                sense[3..7].copy_from_slice(&1u32.to_be_bytes());
                sense[7] = 10;
                let error =
                    crate::TapeIoError::new(crate::TapeIoErrorKind::CheckConditionWithResidue {
                        source: ScsiError::CheckCondition {
                            sense,
                            bytes_transferred: kernel,
                        },
                        bytes_transferred: 12,
                    });
                let expected = if response == 0xf0 {
                    (3, VerificationReadCompletion::Filemark)
                } else {
                    (0, VerificationReadCompletion::UnconfirmedBoundary)
                };
                assert_eq!(verification_read_completion(&error, 4, 4), expected);
            }
        }
    }
}

#[cfg(test)]
mod b3b_strict_tests {
    use super::*;
    /// A truncated MEDIUM ERROR cannot become a verification damage fact.
    #[test]
    fn b3b_verification_completion_rejects_truncated_medium() {
        let error = crate::TapeIoError::new(crate::TapeIoErrorKind::CheckCondition(
            ScsiError::CheckCondition {
                sense: vec![0x70, 0, 3],
                bytes_transferred: 0,
            },
        ));
        assert_eq!(
            verification_read_completion(&error, 512, 4).1,
            VerificationReadCompletion::IoError
        );
    }
    /// Seven captured bytes can contain a residual but cannot prove usable sense.
    #[test]
    fn b3b_transfer_residual_requires_usable_sense() {
        assert_eq!(
            transfer_bytes_from_sense(&[0xf0, 0, 3, 0, 0, 0, 2], 2048, 2048, Some(512)),
            2048
        );
    }
}
