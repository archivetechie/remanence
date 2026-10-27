//! TEST UNIT READY media-readiness classification.
//!
//! LTO-9 media can spend many minutes conditioning/calibrating after a
//! changer move. During that window, destructive or configuration probes such
//! as READ BLOCK LIMITS, MODE SENSE, REWIND, READ POSITION, LOG SENSE, READ,
//! WRITE, and WRITE FILEMARKS are not safe readiness detectors. This module
//! keeps the detector limited to TEST UNIT READY and turns the raw SCSI result
//! into a stable state machine vocabulary.

use std::time::Duration;

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

/// Operator-facing readiness descriptions shared by mount and initialization.
impl std::fmt::Display for MediaReadiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready => f.write_str("drive ready"),
            Self::NoMedium { .. } => f.write_str("no cartridge in the drive"),
            Self::BecomingReady { ascq: 0x02, .. } => {
                f.write_str("drive needs an initializing command (SSC LOAD)")
            }
            Self::BecomingReady { .. } => f.write_str("drive becoming ready"),
            Self::UnitAttention { .. } | Self::RepeatedUnitAttention { .. } => {
                f.write_str("drive reported a change; retry readiness checks")
            }
            Self::TerminalNotReady { action, .. } => write!(f, "drive not ready: {action}"),
            Self::CheckCondition { key, asc, ascq } => write!(
                f,
                "drive refused readiness check (sense {key:02x}/{asc:02x}/{ascq:02x})"
            ),
            Self::UndecodedCheckCondition { .. } => {
                f.write_str("drive returned unreadable sense data")
            }
            Self::TargetBusy { .. } => f.write_str("drive busy"),
            Self::ReservationConflict => f.write_str("drive reserved by another host"),
            Self::TaskAborted => f.write_str("drive aborted the readiness check"),
            Self::UnexpectedStatus { status } => {
                write!(f, "drive returned unexpected status {status:02x}")
            }
            Self::TransportUnknown { detail } => {
                write!(f, "drive communication uncertain: {detail}")
            }
            Self::InvalidRequest { detail } => write!(f, "invalid readiness request: {detail}"),
        }
    }
}

/// Options for a bounded TEST UNIT READY polling epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaReadinessWaitOptions {
    /// Keep polling retryable readiness states instead of returning after the
    /// first observation.
    pub wait: bool,
    /// Maximum wall time for retryable observations.
    pub timeout: Duration,
    /// Steady-state poll interval after the initial fast-poll ramp.
    pub poll_interval: Duration,
}

/// SSC LOAD commands one media-readiness operation may send when the drive
/// asks for an initializing command (`02/04/02`). The budget re-arms only after
/// the operation records a `ready` observation.
pub const MEDIA_READINESS_MAX_INITIALIZING_LOADS: u32 = 1;

/// One durable observation from a media-readiness polling epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaReadinessPoll {
    /// Classified TEST UNIT READY result.
    pub readiness: MediaReadiness,
    /// Number of TEST UNIT READY commands issued in this polling epoch.
    pub attempts: u64,
    /// Wall time since the polling epoch began.
    pub elapsed: Duration,
    /// True when a retryable state remained after the configured deadline.
    pub timed_out: bool,
}

/// Callback event emitted by the shared media-readiness poll loop.
#[derive(Debug)]
pub enum MediaReadinessWaitEvent<'a> {
    /// A TEST UNIT READY result that callers should persist or publish.
    Poll(&'a MediaReadinessPoll),
    /// Cancellation detected before another TEST UNIT READY was issued.
    Cancelled(&'a str),
    /// The drive asked for an initializing command (`02/04/02`) and the wait
    /// is about to send its SSC LOAD (IMMED). The callback records the
    /// attempt before the command reaches the drive; an error stops the wait
    /// without sending it.
    InitializingLoad(&'a MediaReadinessPoll),
    /// The SSC LOAD failed. `readiness` classifies the failure and `detail`
    /// carries the drive error. A retryable classification keeps polling; any
    /// other ends the wait with that classification.
    InitializingLoadFailed {
        /// Readiness classification of the LOAD failure.
        readiness: &'a MediaReadiness,
        /// Display text of the drive error.
        detail: &'a str,
    },
}

/// Apply the common fast-to-steady polling schedule used by readiness waits.
pub fn media_readiness_poll_interval(elapsed: Duration, steady_poll: Duration) -> Duration {
    if elapsed < Duration::from_secs(5) {
        Duration::from_secs(1)
    } else if elapsed < Duration::from_secs(15) {
        Duration::from_secs(2)
    } else if elapsed < Duration::from_secs(60) {
        Duration::from_secs(5)
    } else {
        steady_poll
    }
}

impl MediaReadiness {
    /// True when the medium is ready for subsequent tape commands.
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// True when the drive reports `NOT READY / LOGICAL UNIT NOT READY,
    /// INITIALIZING COMMAND REQUIRED` (`02/04/02`). A cartridge sits in the
    /// drive unthreaded, for example after an SSC UNLOAD that no changer move
    /// followed, and the drive stays not ready until a host sends SSC LOAD.
    pub fn requires_initializing_command(&self) -> bool {
        matches!(self, Self::BecomingReady { ascq: 0x02, .. })
    }

    /// Sense key, ASC and ASCQ behind this classification, when the drive
    /// returned decodable sense data.
    pub fn sense(&self) -> Option<(u8, u8, u8)> {
        match self {
            Self::BecomingReady { ascq, .. } | Self::TerminalNotReady { ascq, .. } => {
                Some((0x02, 0x04, *ascq))
            }
            Self::NoMedium { ascq } => Some((0x02, 0x3a, *ascq)),
            Self::UnitAttention { asc, ascq } | Self::RepeatedUnitAttention { asc, ascq } => {
                Some((0x06, *asc, *ascq))
            }
            Self::CheckCondition { key, asc, ascq } => Some((*key, *asc, *ascq)),
            Self::Ready
            | Self::UndecodedCheckCondition { .. }
            | Self::TargetBusy { .. }
            | Self::ReservationConflict
            | Self::TaskAborted
            | Self::UnexpectedStatus { .. }
            | Self::TransportUnknown { .. }
            | Self::InvalidRequest { .. } => None,
        }
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

/// Name of an additional sense code and qualifier that readiness probes
/// report, as the SPC additional sense table words it, in lower case.
///
/// Returns `None` for codes outside the readiness vocabulary; callers then
/// show the raw `KK/AA/QQ` triple alone.
pub fn media_readiness_sense_name(asc: u8, ascq: u8) -> Option<&'static str> {
    Some(match (asc, ascq) {
        (0x04, 0x00) => "not ready, cause not reportable",
        (0x04, 0x01) => "becoming ready",
        (0x04, 0x02) => "initializing command required",
        (0x04, 0x03) => "manual intervention required",
        (0x04, 0x04) => "format in progress",
        (0x04, 0x07) => "operation in progress",
        (0x04, 0x12) => "offline",
        (0x04, 0x20) => "logical unit reset required",
        (0x04, 0x21) => "hard reset required",
        (0x04, 0x22) => "power cycle required",
        (0x28, 0x00) => "not ready to ready change, medium may have changed",
        (0x29, 0x00) => "power on, reset, or bus device reset occurred",
        (0x29, 0x01) => "power on occurred",
        (0x29, 0x02) => "SCSI bus reset occurred",
        (0x29, 0x03) => "bus device reset function occurred",
        (0x29, 0x04) => "device internal reset",
        (0x2a, 0x01) => "mode parameters changed",
        (0x30, 0x00) => "incompatible medium installed",
        (0x30, 0x03) => "cleaning cartridge installed",
        (0x3a, 0x00) => "medium not present",
        (0x3a, 0x01) => "medium not present, tray closed",
        (0x3a, 0x02) => "medium not present, tray open",
        (0x3a, 0x04) => "medium not present, medium auxiliary memory accessible",
        (0x53, 0x00) => "media load or eject failed",
        _ => return None,
    })
}

/// `KK/AA/QQ` label of a sense triple, followed by its name in parentheses
/// when [`media_readiness_sense_name`] knows it. Quarantine listings, daemon
/// logs and operation progress print this form.
pub fn media_readiness_sense_label(sense_key: u8, asc: u8, ascq: u8) -> String {
    match media_readiness_sense_name(asc, ascq) {
        Some(name) => format!("{sense_key:02x}/{asc:02x}/{ascq:02x} ({name})"),
        None => format!("{sense_key:02x}/{asc:02x}/{ascq:02x}"),
    }
}

/// Classify a failed TEST UNIT READY call.
pub fn classify_media_readiness_error(err: ScsiError, family: MediaFamily) -> MediaReadiness {
    classify_media_readiness_error_ref(&err, family)
}

/// Classify a borrowed SCSI failure for readiness-aware composed commands.
///
/// This is the same classifier as [`classify_media_readiness_error`], exposed
/// by reference so a higher layer can decide whether a failed LOAD is safe to
/// follow with TEST UNIT READY while retaining the original error for display.
pub fn classify_media_readiness_error_ref(err: &ScsiError, family: MediaFamily) -> MediaReadiness {
    match err {
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { sense, .. } => classify_check_condition(sense.clone(), family),
        #[cfg(target_os = "linux")]
        ScsiError::UnexpectedStatus { status } => classify_target_status(*status),
        #[cfg(target_os = "linux")]
        ScsiError::TransportError { .. } | ScsiError::Io(_) => MediaReadiness::TransportUnknown {
            detail: err.to_string(),
        },
        ScsiError::Cancelled | ScsiError::InvalidInput(_) => MediaReadiness::InvalidRequest {
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

    #[cfg(target_os = "linux")]
    #[test]
    fn initializing_command_required_is_named_and_flagged() {
        let readiness = classify_media_readiness_error(
            check_condition(0x02, 0x04, 0x02),
            MediaFamily::Lto9OrLater,
        );

        assert!(readiness.requires_initializing_command());
        assert!(readiness.is_retryable_wait());
        assert_eq!(readiness.sense(), Some((0x02, 0x04, 0x02)));
        assert_eq!(
            media_readiness_sense_name(0x04, 0x02),
            Some("initializing command required")
        );
        assert_eq!(
            media_readiness_sense_label(0x02, 0x04, 0x02),
            "02/04/02 (initializing command required)"
        );
        assert_eq!(media_readiness_sense_label(0x03, 0x11, 0x00), "03/11/00");
        assert!(!MediaReadiness::BecomingReady {
            ascq: 0x01,
            media_initializing: true
        }
        .requires_initializing_command());
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
