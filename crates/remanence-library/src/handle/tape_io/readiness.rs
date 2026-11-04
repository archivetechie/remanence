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
#[derive(Clone)]
pub struct MediaReadiness {
    /// Original readiness state used by the existing wait machine.
    pub kind: MediaReadinessKind,
    outcome: Option<Box<crate::outcome::ClassifiedOutcome>>,
}
impl std::fmt::Debug for MediaReadiness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.kind, f)
    }
}
impl PartialEq for MediaReadiness {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
    }
}
impl Eq for MediaReadiness {}
impl MediaReadiness {
    /// Construct an initial host observation without inventing a dispatched command.
    pub fn new(kind: MediaReadinessKind) -> Self {
        let mut evidence = crate::outcome::CompletionEvidence::success();
        evidence.dispatch = crate::outcome::Dispatch::NotIssued;
        evidence.failed = !matches!(kind, MediaReadinessKind::Ready);
        let outcome = crate::outcome::classify(crate::outcome::OutcomeInput {
            class: crate::outcome::CommandClass::SenseStatus,
            context: Default::default(),
            evidence,
        });
        Self::pending(kind).with_outcome(outcome)
    }
    pub(crate) fn pending(kind: MediaReadinessKind) -> Self {
        Self {
            kind,
            outcome: None,
        }
    }
    pub(crate) fn with_outcome(mut self, outcome: crate::outcome::ClassifiedOutcome) -> Self {
        self.outcome = Some(Box::new(outcome));
        self
    }
    /// The command that supplied this observation, retained across later polls.
    pub fn outcome(&self) -> &crate::outcome::ClassifiedOutcome {
        self.outcome
            .as_deref()
            .expect("unfinished readiness observation")
    }
}
/// Readiness vocabulary; equality of observations retains the original state semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaReadinessKind {
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
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::Ready,
                ..
            } => f.write_str("drive ready"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::NoMedium { .. },
                ..
            } => f.write_str("no cartridge in the drive"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::BecomingReady { ascq: 0x02, .. },
                ..
            } => f.write_str("drive needs an initializing command (SSC LOAD)"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::BecomingReady { .. },
                ..
            } => f.write_str("drive becoming ready"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnitAttention { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::RepeatedUnitAttention { .. },
                ..
            } => f.write_str("drive reported a change; retry readiness checks"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TerminalNotReady { action, .. },
                ..
            } => write!(f, "drive not ready: {action}"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::CheckCondition { key, asc, ascq },
                ..
            } => write!(
                f,
                "drive refused readiness check (sense {key:02x}/{asc:02x}/{ascq:02x})"
            ),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UndecodedCheckCondition { .. },
                ..
            } => f.write_str("drive returned unreadable sense data"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TargetBusy { .. },
                ..
            } => f.write_str("drive busy"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::ReservationConflict,
                ..
            } => f.write_str("drive reserved by another host"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TaskAborted,
                ..
            } => f.write_str("drive aborted the readiness check"),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnexpectedStatus { status },
                ..
            } => {
                write!(f, "drive returned unexpected status {status:02x}")
            }
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TransportUnknown { detail },
                ..
            } => {
                write!(f, "drive communication uncertain: {detail}")
            }
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::InvalidRequest { detail },
                ..
            } => write!(f, "invalid readiness request: {detail}"),
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
        matches!(
            self,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::Ready,
                ..
            }
        )
    }

    /// True when the drive reports `NOT READY / LOGICAL UNIT NOT READY,
    /// INITIALIZING COMMAND REQUIRED` (`02/04/02`). A cartridge sits in the
    /// drive unthreaded, for example after an SSC UNLOAD that no changer move
    /// followed, and the drive stays not ready until a host sends SSC LOAD.
    pub fn requires_initializing_command(&self) -> bool {
        matches!(
            self,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::BecomingReady { ascq: 0x02, .. },
                ..
            }
        )
    }

    /// Sense key, ASC and ASCQ behind this classification, when the drive
    /// returned decodable sense data.
    pub fn sense(&self) -> Option<(u8, u8, u8)> {
        match self {
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::BecomingReady { ascq, .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TerminalNotReady { ascq, .. },
                ..
            } => Some((0x02, 0x04, *ascq)),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::NoMedium { ascq },
                ..
            } => Some((0x02, 0x3a, *ascq)),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnitAttention { asc, ascq },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::RepeatedUnitAttention { asc, ascq },
                ..
            } => Some((0x06, *asc, *ascq)),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::CheckCondition { key, asc, ascq },
                ..
            } => Some((*key, *asc, *ascq)),
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::Ready,
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UndecodedCheckCondition { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TargetBusy { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::ReservationConflict,
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TaskAborted,
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnexpectedStatus { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TransportUnknown { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::InvalidRequest { .. },
                ..
            } => None,
        }
    }

    /// True for states that can be polled again without operator action.
    pub fn is_retryable_wait(&self) -> bool {
        matches!(
            self,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::BecomingReady { .. },
                ..
            } | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnitAttention { .. },
                ..
            } | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TargetBusy { .. },
                ..
            }
        )
    }

    /// Suggested CLI exit code from the v0.4 readiness design.
    pub fn design_exit_code(&self) -> i32 {
        match self {
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::Ready,
                ..
            } => 0,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::BecomingReady { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnitAttention { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TargetBusy { .. },
                ..
            } => 10,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TransportUnknown { .. },
                ..
            } => 40,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::ReservationConflict,
                ..
            } => 50,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TerminalNotReady { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::NoMedium { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::RepeatedUnitAttention { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::CheckCondition { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UndecodedCheckCondition { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TaskAborted,
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::UnexpectedStatus { .. },
                ..
            }
            | crate::MediaReadiness {
                kind: crate::MediaReadinessKind::InvalidRequest { .. },
                ..
            } => 30,
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
    let outcome = crate::outcome::finish_control(
        crate::outcome::CommandClass::SenseStatus,
        Default::default(),
        0,
        Some(&err),
    );
    let mut error = crate::DriveOpError::pending(crate::DriveOpErrorKind::ScsiError(err));
    error.set_outcome(outcome);
    classify_media_readiness_error_ref(&error, family)
}

/// Classify a borrowed SCSI failure for readiness-aware composed commands.
///
/// This is the same classifier as [`classify_media_readiness_error`], exposed
/// by reference so a higher layer can decide whether a failed LOAD is safe to
/// follow with TEST UNIT READY while retaining the original error for display.
pub fn classify_media_readiness_error_ref(
    error: &crate::DriveOpError,
    family: MediaFamily,
) -> MediaReadiness {
    let crate::DriveOpErrorKind::ScsiError(err) = &error.kind else {
        return MediaReadiness::pending(MediaReadinessKind::InvalidRequest {
            detail: error.to_string(),
        })
        .with_outcome(error.outcome().clone());
    };
    let readiness = match err {
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { sense, .. } => {
            classify_check_condition(sense.clone(), &error.completion_evidence(), family)
        }
        #[cfg(target_os = "linux")]
        ScsiError::UnexpectedStatus { status } => classify_target_status(*status),
        #[cfg(target_os = "linux")]
        ScsiError::TransportError { .. } | ScsiError::Io(_) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::TransportUnknown {
                detail: err.to_string(),
            })
        }
        ScsiError::Cancelled | ScsiError::InvalidInput(_) | ScsiError::HostCapacityRefused(_) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::InvalidRequest {
                detail: err.to_string(),
            })
        }
        ScsiError::Truncated { .. } | ScsiError::InvalidResponse { .. } => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::InvalidRequest {
                detail: err.to_string(),
            })
        }
    };
    readiness.with_outcome(error.outcome().clone())
}

#[cfg(target_os = "linux")]
fn classify_check_condition(
    sense: Vec<u8>,
    evidence: &crate::outcome::CompletionEvidence,
    family: MediaFamily,
) -> MediaReadiness {
    let Some((key, asc, ascq)) = evidence.decoded.map(|s| (s.key, s.asc, s.ascq)) else {
        return crate::MediaReadiness::pending(
            crate::MediaReadinessKind::UndecodedCheckCondition { sense },
        );
    };
    match (key, asc, ascq) {
        (0x02, 0x04, 0x03) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::TerminalNotReady {
                ascq,
                action: "manual_intervention_required",
            })
        }
        (0x02, 0x04, 0x20) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::TerminalNotReady {
                ascq,
                action: "logical_unit_reset_required",
            })
        }
        (0x02, 0x04, 0x21) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::TerminalNotReady {
                ascq,
                action: "hard_reset_required",
            })
        }
        (0x02, 0x04, 0x22) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::TerminalNotReady {
                ascq,
                action: "power_cycle_required",
            })
        }
        (0x02, 0x04, _) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::BecomingReady {
                ascq,
                media_initializing: family == MediaFamily::Lto9OrLater,
            })
        }
        (0x02, 0x3a, _) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::NoMedium { ascq })
        }
        (0x06, _, _) => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::UnitAttention { asc, ascq })
        }
        _ => crate::MediaReadiness::pending(crate::MediaReadinessKind::CheckCondition {
            key,
            asc,
            ascq,
        }),
    }
}

#[cfg(target_os = "linux")]
fn classify_target_status(status: u8) -> MediaReadiness {
    match status {
        0x08 | 0x28 => {
            crate::MediaReadiness::pending(crate::MediaReadinessKind::TargetBusy { status })
        }
        0x18 => crate::MediaReadiness::pending(crate::MediaReadinessKind::ReservationConflict),
        0x40 => crate::MediaReadiness::pending(crate::MediaReadinessKind::TaskAborted),
        _ => crate::MediaReadiness::pending(crate::MediaReadinessKind::UnexpectedStatus { status }),
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
            crate::MediaReadiness::new(crate::MediaReadinessKind::BecomingReady {
                ascq: 0x01,
                media_initializing: true
            })
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
            crate::MediaReadiness::new(crate::MediaReadinessKind::TerminalNotReady {
                ascq: 0x03,
                action: "manual_intervention_required"
            })
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
            crate::MediaReadiness::new(crate::MediaReadinessKind::TargetBusy { status: 0x08 })
        );
        assert_eq!(
            classify_media_readiness_error(
                ScsiError::UnexpectedStatus { status: 0x28 },
                MediaFamily::Unknown
            ),
            crate::MediaReadiness::new(crate::MediaReadinessKind::TargetBusy { status: 0x28 })
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reservation_conflict_is_ownership_refusal() {
        let readiness = classify_media_readiness_error(
            ScsiError::UnexpectedStatus { status: 0x18 },
            MediaFamily::Unknown,
        );

        assert_eq!(
            readiness,
            crate::MediaReadiness::new(crate::MediaReadinessKind::ReservationConflict)
        );
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
        assert!(
            !crate::MediaReadiness::new(crate::MediaReadinessKind::BecomingReady {
                ascq: 0x01,
                media_initializing: true
            })
            .requires_initializing_command()
        );
    }

    #[test]
    fn repeated_unit_attention_is_terminal_for_readiness_epoch() {
        let readiness =
            crate::MediaReadiness::new(crate::MediaReadinessKind::RepeatedUnitAttention {
                asc: 0x29,
                ascq: 0x00,
            });

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

        assert!(matches!(
            readiness,
            crate::MediaReadiness {
                kind: crate::MediaReadinessKind::TransportUnknown { .. },
                ..
            }
        ));
        assert_eq!(readiness.design_exit_code(), 40);
    }
}
