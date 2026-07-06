//! TEST UNIT READY media-readiness classification.
//!
//! LTO-9 media can spend many minutes conditioning/calibrating after a
//! changer move. During that window, destructive or configuration probes such
//! as READ BLOCK LIMITS, MODE SENSE, REWIND, READ POSITION, LOG SENSE, READ,
//! WRITE, and WRITE FILEMARKS are not safe readiness detectors. This module
//! keeps the detector limited to TEST UNIT READY and turns the raw SCSI result
//! into a stable state machine vocabulary.

use remanence_scsi::ScsiError;

use super::decode_sense_key_asc;

/// Media family hint used when interpreting `NOT READY / becoming ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaFamily {
    /// Unknown or unclassified media generation.
    #[default]
    Unknown,
    /// LTO-9 or a later LTO generation where `02/04/xx` commonly means
    /// long-running media initialization/calibration.
    Lto9OrLater,
}

/// Classified outcome of a TEST UNIT READY readiness probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaReadiness {
    /// TEST UNIT READY returned GOOD.
    Ready,
    /// The drive reports `NOT READY / LOGICAL UNIT IS IN PROCESS OF BECOMING
    /// READY` (`02/04/xx`). The exact ASCQ is retained.
    BecomingReady {
        /// ASCQ from the sense bytes.
        ascq: u8,
        /// True when the caller supplied an LTO-9-or-later media hint.
        media_initializing: bool,
    },
    /// The drive is loaded with no medium (`02/3A/xx`).
    NoMedium {
        /// ASCQ from the sense bytes.
        ascq: u8,
    },
    /// Unit attention. Callers may perform a bounded retry, but should not
    /// switch to media-access commands until a later TUR is ready.
    UnitAttention {
        /// Additional Sense Code.
        asc: u8,
        /// Additional Sense Code Qualifier.
        ascq: u8,
    },
    /// Unit Attention repeated inside one readiness polling epoch. The first UA
    /// can be a normal post-load/reset notification; repetition means the
    /// caller no longer has a settled media-readiness signal.
    RepeatedUnitAttention {
        /// Additional Sense Code.
        asc: u8,
        /// Additional Sense Code Qualifier.
        ascq: u8,
    },
    /// A terminal `02/04/xx` variant requiring operator or reset action.
    TerminalNotReady {
        /// Additional Sense Code Qualifier.
        ascq: u8,
        /// Human-readable action class.
        action: &'static str,
    },
    /// CHECK CONDITION with decoded sense that is not part of the readiness
    /// state machine.
    CheckCondition {
        /// Sense key.
        key: u8,
        /// Additional Sense Code.
        asc: u8,
        /// Additional Sense Code Qualifier.
        ascq: u8,
    },
    /// CHECK CONDITION without decodable sense bytes.
    UndecodedCheckCondition {
        /// Raw sense bytes captured by the transport.
        sense: Vec<u8>,
    },
    /// Target returned BUSY or TASK SET FULL. The command did not execute;
    /// callers may retry with bounded backoff.
    TargetBusy {
        /// Raw SCSI status byte.
        status: u8,
    },
    /// Target returned RESERVATION CONFLICT. This is ownership/refusal, not a
    /// media-initialization signal.
    ReservationConflict,
    /// Target returned TASK ABORTED. Treat as dirty/unknown evidence until a
    /// transport-specific recovery rule exists.
    TaskAborted,
    /// Other non-GOOD target status.
    UnexpectedStatus {
        /// Raw SCSI status byte.
        status: u8,
    },
    /// Transport/HBA/kernel-level uncertainty. The readiness CDB completion is
    /// unknown, so callers should fence the operation for RCA/reconciliation.
    TransportUnknown {
        /// Display string from the low-level error.
        detail: String,
    },
    /// A caller or fixture violated the command contract before the CDB could
    /// be sent.
    InvalidRequest {
        /// Display string from the low-level error.
        detail: String,
    },
}

impl MediaReadiness {
    /// True when the medium is ready for subsequent tape commands.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// True for states that can be polled again without operator action.
    pub fn is_retryable_wait(&self) -> bool {
        matches!(
            self,
            Self::BecomingReady { .. } | Self::UnitAttention { .. } | Self::TargetBusy { .. }
        )
    }

    /// Suggested CLI exit code from the v0.4 readiness design.
    pub fn design_exit_code(&self) -> i32 {
        match self {
            Self::Ready => 0,
            Self::BecomingReady { .. } | Self::UnitAttention { .. } | Self::TargetBusy { .. } => 10,
            Self::TransportUnknown { .. } => 40,
            Self::ReservationConflict => 50,
            Self::TerminalNotReady { .. }
            | Self::NoMedium { .. }
            | Self::RepeatedUnitAttention { .. }
            | Self::CheckCondition { .. }
            | Self::UndecodedCheckCondition { .. }
            | Self::TaskAborted
            | Self::UnexpectedStatus { .. }
            | Self::InvalidRequest { .. } => 30,
        }
    }
}

/// Classify a failed TEST UNIT READY call.
pub fn classify_media_readiness_error(err: ScsiError, family: MediaFamily) -> MediaReadiness {
    match err {
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { sense, .. } => classify_check_condition(sense, family),
        #[cfg(target_os = "linux")]
        ScsiError::UnexpectedStatus { status } => classify_target_status(status),
        #[cfg(target_os = "linux")]
        ScsiError::TransportError { .. } | ScsiError::Io(_) => MediaReadiness::TransportUnknown {
            detail: err.to_string(),
        },
        ScsiError::InvalidInput(_) => MediaReadiness::InvalidRequest {
            detail: err.to_string(),
        },
        ScsiError::Truncated { .. } | ScsiError::InvalidResponse { .. } => {
            MediaReadiness::InvalidRequest {
                detail: err.to_string(),
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn classify_check_condition(sense: Vec<u8>, family: MediaFamily) -> MediaReadiness {
    let Some((key, asc, ascq)) = decode_sense_key_asc(&sense) else {
        return MediaReadiness::UndecodedCheckCondition { sense };
    };
    match (key, asc, ascq) {
        (0x02, 0x04, 0x03) => MediaReadiness::TerminalNotReady {
            ascq,
            action: "manual_intervention_required",
        },
        (0x02, 0x04, 0x20) => MediaReadiness::TerminalNotReady {
            ascq,
            action: "logical_unit_reset_required",
        },
        (0x02, 0x04, 0x21) => MediaReadiness::TerminalNotReady {
            ascq,
            action: "hard_reset_required",
        },
        (0x02, 0x04, 0x22) => MediaReadiness::TerminalNotReady {
            ascq,
            action: "power_cycle_required",
        },
        (0x02, 0x04, _) => MediaReadiness::BecomingReady {
            ascq,
            media_initializing: family == MediaFamily::Lto9OrLater,
        },
        (0x02, 0x3a, _) => MediaReadiness::NoMedium { ascq },
        (0x06, _, _) => MediaReadiness::UnitAttention { asc, ascq },
        _ => MediaReadiness::CheckCondition { key, asc, ascq },
    }
}

#[cfg(target_os = "linux")]
fn classify_target_status(status: u8) -> MediaReadiness {
    match status {
        0x08 | 0x28 => MediaReadiness::TargetBusy { status },
        0x18 => MediaReadiness::ReservationConflict,
        0x40 => MediaReadiness::TaskAborted,
        _ => MediaReadiness::UnexpectedStatus { status },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut v = vec![0u8; 32];
        v[0] = 0x70;
        v[2] = key & 0x0f;
        v[7] = 24;
        v[12] = asc;
        v[13] = ascq;
        v
    }

    #[cfg(target_os = "linux")]
    fn check_condition(key: u8, asc: u8, ascq: u8) -> ScsiError {
        ScsiError::CheckCondition {
            sense: fixed_sense(key, asc, ascq),
            bytes_transferred: 0,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lto9_becoming_ready_is_media_initializing_with_exact_ascq() {
        let readiness = classify_media_readiness_error(
            check_condition(0x02, 0x04, 0x01),
            MediaFamily::Lto9OrLater,
        );

        assert_eq!(
            readiness,
            MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true
            }
        );
        assert!(readiness.is_retryable_wait());
        assert_eq!(readiness.design_exit_code(), 10);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn terminal_0403_keeps_operator_action() {
        let readiness = classify_media_readiness_error(
            check_condition(0x02, 0x04, 0x03),
            MediaFamily::Lto9OrLater,
        );

        assert_eq!(
            readiness,
            MediaReadiness::TerminalNotReady {
                ascq: 0x03,
                action: "manual_intervention_required"
            }
        );
        assert!(!readiness.is_retryable_wait());
        assert_eq!(readiness.design_exit_code(), 30);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn busy_and_task_set_full_are_retryable_target_busy() {
        assert_eq!(
            classify_media_readiness_error(
                ScsiError::UnexpectedStatus { status: 0x08 },
                MediaFamily::Unknown
            ),
            MediaReadiness::TargetBusy { status: 0x08 }
        );
        assert_eq!(
            classify_media_readiness_error(
                ScsiError::UnexpectedStatus { status: 0x28 },
                MediaFamily::Unknown
            ),
            MediaReadiness::TargetBusy { status: 0x28 }
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reservation_conflict_is_ownership_refusal() {
        let readiness = classify_media_readiness_error(
            ScsiError::UnexpectedStatus { status: 0x18 },
            MediaFamily::Unknown,
        );

        assert_eq!(readiness, MediaReadiness::ReservationConflict);
        assert_eq!(readiness.design_exit_code(), 50);
    }

    #[test]
    fn repeated_unit_attention_is_terminal_for_readiness_epoch() {
        let readiness = MediaReadiness::RepeatedUnitAttention {
            asc: 0x29,
            ascq: 0x00,
        };

        assert!(!readiness.is_retryable_wait());
        assert_eq!(readiness.design_exit_code(), 30);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn transport_error_is_unknown_completion() {
        let readiness = classify_media_readiness_error(
            ScsiError::TransportError {
                status: 0,
                host_status: 0x0003,
                driver_status: 0,
                info: 1,
                sense: Vec::new(),
            },
            MediaFamily::Lto9OrLater,
        );

        assert!(matches!(readiness, MediaReadiness::TransportUnknown { .. }));
        assert_eq!(readiness.design_exit_code(), 40);
    }
}
