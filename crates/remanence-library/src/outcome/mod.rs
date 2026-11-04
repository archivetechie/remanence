//! Typed command evidence and device policy shared by library and application owners.
//!
//! The classifier accepts codes, counts and explicit operation authority. Diagnostic
//! messages never enter its input, so a change to Display cannot change fencing.
use crate::TapeIoError;
use remanence_scsi::{decode_usable_sense, sense::DecodedSense, ScsiError};
use std::ops::Range;
mod dispatch;
pub(crate) use dispatch::CommandOutcomes;
pub use dispatch::ContextGuard;

/// Whether the command reached the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// Refused before dispatch.
    NotIssued,
    /// Sent to the device.
    Issued,
}
/// Certainty of the command's completion, independent of success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// A usable completion was captured.
    Known,
    /// Acceptance or completion cannot be proved.
    Unknown,
}
/// Retry budget axis, independent of the durable consequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryCause {
    /// Retry with bounded backoff.
    Transient,
    /// Wait for an operator wake.
    Operator,
    /// Consume the existing three-attempt media budget.
    Media,
}
/// Tape-scoped causes, ordered as library-log v1 codes 0 through 10.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FenceCause {
    /// A fixed batch could not be committed.
    PartialBatch,
    /// A media command's completion is unproved.
    TransferError,
    /// Two closeout checks disagree.
    CloseoutCheckMismatch,
    /// Tail supersession failed.
    SupersedeTailFailed,
    /// Restart lost the standalone write frontier.
    StandaloneWriteLostByRestart,
    /// Append permission could not be established.
    AppendAdmissionRefused,
    /// A reset invalidated write state.
    ResetUnitAttention,
    /// Independent position proof disagrees.
    PositionDrift,
    /// Publication durability is unknown.
    AuthorityAmbiguous,
    /// A suffix contradicts session authority.
    SessionSuffixContradiction,
    /// A closeout read contradicts committed authority.
    CloseoutReadContradiction,
}
impl FenceCause {
    /// Existing persistence reason; changing spelling would alter B5's readers.
    pub const fn reason(self) -> &'static str {
        match self {
            Self::PartialBatch => "partial_batch",
            Self::TransferError => "transfer_error",
            Self::CloseoutCheckMismatch => "CloseoutCheckMismatch",
            Self::SupersedeTailFailed => "supersede_tail_failed",
            Self::StandaloneWriteLostByRestart => "standalone_write_lost_by_restart",
            Self::AppendAdmissionRefused => "append_admission_refused",
            Self::ResetUnitAttention => "reset_unit_attention",
            Self::PositionDrift => "position_drift",
            Self::AuthorityAmbiguous => "AuthorityAmbiguous",
            Self::SessionSuffixContradiction => "SessionSuffixContradiction",
            Self::CloseoutReadContradiction => "CloseoutReadContradiction",
        }
    }
}
/// Command semantics; opcode and service action remain in the evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandClass {
    /// READ, either transfer mode.
    Read,
    /// WRITE, either transfer mode or batching form.
    Write,
    /// Filemark command, including a zero-count synchronous barrier.
    WriteFilemarks {
        /// Requested mark count.
        count: u32,
        /// Whether only command acceptance is synchronous.
        immediate: bool,
    },
    /// LOCATE, SPACE or READ POSITION.
    Position,
    /// Load medium.
    Load,
    /// Unload medium.
    Unload,
    /// Changer MOVE MEDIUM.
    Move,
    /// INQUIRY and discovery.
    Inquiry,
    /// TUR, REQUEST SENSE or status query.
    SenseStatus,
    /// Mode query or selection.
    Mode,
    /// Reservation and removal control.
    Reservation,
    /// Other commands or composite host facts.
    OtherControl,
}
/// Caller-owned phase; the command layer never guesses application authority.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Establish append permission.
    Admission,
    /// Append payload.
    Append,
    /// Make preceding transfers durable.
    Barrier,
    /// Publish authority.
    LogAppend,
    /// Check and close a session.
    Closeout,
    /// Read user data.
    Read,
    /// Verify committed data.
    Verification,
    /// Finalize medium.
    Finalization,
    /// Change inventory.
    Motion,
    /// Initialize medium.
    Initialization,
    /// Read-only diagnostics without tape authority.
    #[default]
    Diagnostic,
}
/// Physical range relevant to the owner's operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Region {
    /// Beginning-of-tape identity.
    Bot,
    /// Durable physical range.
    Committed(Range<u64>),
    /// Uncommitted suffix beginning at C.
    AtOrAfterCursor(u64),
    /// Non-tape control operation.
    NotTape,
    /// No physical proof is available.
    #[default]
    Unknown,
}
/// Identity of the inventory partition affected by uncertain motion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PartitionIdentity {
    /// Changer serial bytes, preserved without parsing diagnostic text.
    Changer(Vec<u8>),
    /// Standalone endpoint identity bytes.
    Standalone(Vec<u8>),
    /// No owner has supplied an identity yet.
    #[default]
    Unknown,
}
/// Result of an authority append, independent of projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuthorityAppend {
    /// No append was attempted.
    #[default]
    NotStarted,
    /// Definitely refused.
    Refused,
    /// Durability is unknown.
    Ambiguous,
    /// The authority record is durable.
    Durable,
}
/// State of a rebuildable projection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Projection {
    /// No pending projection.
    #[default]
    Applied,
    /// Durable authority awaits projection.
    Pending,
}
/// Fresh identity and cursor proof, separate from a failed command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reproof {
    /// No fresh proof.
    #[default]
    NotAttempted,
    /// Fresh proof succeeded.
    Proved,
    /// Physical authority was contradicted.
    Contradicted,
}
/// Existing durability facts supplied by the owning operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DurabilityEvidence {
    /// Synchronized and possibly dispatched pre-barrier tail bounds.
    pub prebarrier_tail: Option<Range<u64>>,
    /// A synchronous barrier completed successfully.
    pub barrier_completed: bool,
    /// Publication certainty.
    pub authority_append: AuthorityAppend,
    /// Projection state after authority publication.
    pub projection: Projection,
    /// Fresh proof of identity and cursor.
    pub reproof: Reproof,
    /// Number of independent checksum reads.
    pub checksum_attempts: u8,
    /// Readiness failure budget already consumed.
    pub drive_failure_budget: u32,
    /// A live session ending durably bounds the dispatched tail.
    pub named_live_tail: bool,
    /// A clean ending prohibits additional complete suffix objects.
    pub clean_ended_suffix: bool,
}
/// Authority and location supplied by the operation owner.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationContext {
    /// Application phase.
    pub phase: Phase,
    /// Physical authority range.
    pub region: Region,
    /// Existing durable proofs.
    pub durability: DurabilityEvidence,
    /// Motion isolation scope.
    pub partition: PartitionIdentity,
    /// The operation belongs to a mounted endpoint.
    pub mounted: bool,
}
/// Position evidence after a command, never a guessed cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PositionEffect {
    /// No position change proved or expected.
    #[default]
    Unchanged,
    /// Proved physical progress.
    Advanced(u64),
    /// Position and identity must be re-established.
    Invalidated,
    /// Position absent, but identity remains usable.
    Forgotten,
}
/// Boundary information present in usable current sense only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoundaryFlags {
    /// A filemark was encountered.
    pub filemark: bool,
    /// End of medium was reported.
    pub eom: bool,
    /// Incorrect length was reported.
    pub ili: bool,
    /// Current BLANK CHECK 00/05 with zero transfer.
    pub physical_eod: bool,
    /// Recovered error without terminal flags.
    pub recovered: bool,
}
/// Typed facts added by completion accounting or a composite operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompletionFact {
    /// No extra fact beyond the captured completion.
    #[default]
    Device,
    /// An operation lost its write result; retain its independently known retry class.
    AbandonedWrite(RetryCause),
    /// Typed DATA PROTECT variant retains the base operator retry.
    DataProtect,
    /// Validated partial write, never a successful credit.
    PartialWrite,
    /// GOOD with an inconsistent fixed READ byte count.
    GoodReadLengthMismatch,
    /// Data transfer or motion completed, but its independent position proof failed.
    PostTransferFailure,
    /// Independent position proof disagrees.
    PositionDrift,
    /// Physical proof contradicts the committed cursor.
    CursorContradiction,
    /// The second closeout checksum disagrees with the first.
    CloseoutMismatch,
    /// A clean-ended suffix contradicts session authority.
    SuffixContradiction,
    /// Admission requires its existing recovery mark and audit.
    AdmissionRecovery,
    /// Typed refusal, including a parsed mode that disallows the operation.
    Refusal(RetryCause),
}
/// Normalized evidence of one command; widths match library-log v1 where shared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionEvidence {
    /// Independently normalized position proof following this transfer or motion. The
    /// primary command's status, sense and byte counts remain in this record.
    pub post_transfer: Option<Box<CompletionEvidence>>,
    /// Dispatch certainty.
    pub dispatch: Dispatch,
    /// Completion certainty.
    pub completion: Completion,
    /// Target status byte, when captured.
    pub scsi_status: Option<u8>,
    /// Host status, when captured.
    pub host_status: Option<u16>,
    /// Driver status, when captured.
    pub driver_status: Option<u16>,
    /// Captured autosense, never a later REQUEST SENSE.
    pub sense: Option<Vec<u8>>,
    /// Strictly decoded fields. Deferred sense remains explicitly deferred.
    pub decoded: Option<DecodedSense>,
    /// Valid current sense residual in its command units.
    pub residual: Option<u64>,
    /// Whether the captured sense declares a valid information field.
    pub residual_valid: bool,
    /// Host-reported bytes, distinct from corrected transfer evidence.
    pub host_transferred_bytes: Option<u64>,
    /// Completion-accounted bytes.
    pub transferred_bytes: Option<u64>,
    /// Requested bytes, if supplied by the dispatch owner.
    pub requested_bytes: Option<u64>,
    /// Requested fixed records.
    pub requested_records: Option<u32>,
    /// Validated complete records, without a commit claim.
    pub records_transferred: Option<u32>,
    /// Fixed record size; None means variable transfer units.
    pub block_size: Option<u32>,
    /// Captured CDB opcode.
    pub opcode: Option<u8>,
    /// Captured CDB service action.
    pub service_action: Option<u16>,
    /// Usable current boundary flags.
    pub boundary: BoundaryFlags,
    /// Cursor certainty.
    pub position: PositionEffect,
    /// Typed completion or composite fact.
    pub fact: CompletionFact,
    /// Whether the command or composite operation failed.
    pub failed: bool,
}
impl CompletionEvidence {
    /// Successful command without additional sense or length information.
    pub fn success() -> Self {
        Self {
            post_transfer: None,
            dispatch: Dispatch::Issued,
            completion: Completion::Known,
            scsi_status: Some(0),
            host_status: None,
            driver_status: None,
            sense: None,
            decoded: None,
            residual: None,
            residual_valid: false,
            host_transferred_bytes: None,
            transferred_bytes: None,
            requested_bytes: None,
            requested_records: None,
            records_transferred: None,
            block_size: None,
            opcode: None,
            service_action: None,
            boundary: BoundaryFlags::default(),
            position: PositionEffect::Unchanged,
            fact: CompletionFact::Device,
            failed: false,
        }
    }
    /// Dispatched command whose completion was lost.
    pub fn unknown() -> Self {
        Self {
            completion: Completion::Unknown,
            scsi_status: None,
            position: PositionEffect::Invalidated,
            failed: true,
            ..Self::success()
        }
    }
    /// Normalize a low-level error without assigning a fence or retry policy.
    pub fn from_scsi(error: &ScsiError) -> Self {
        #[cfg(any(test, feature = "outcome-census"))]
        census::normalize();
        let mut e = Self {
            failed: true,
            scsi_status: None,
            ..Self::success()
        };
        match error {
            ScsiError::CheckCondition {
                sense,
                bytes_transferred,
            } => {
                e.scsi_status = Some(2);
                e.host_transferred_bytes = Some(u64::from(*bytes_transferred));
                e.transferred_bytes = e.host_transferred_bytes;
                e.set_sense(sense);
                if e.decoded.is_none_or(|s| s.is_deferred()) {
                    e.completion = Completion::Unknown;
                    e.position = PositionEffect::Invalidated;
                }
            }
            ScsiError::TransportError {
                status,
                host_status,
                driver_status,
                sense,
                ..
            } => {
                e = Self::unknown();
                e.scsi_status = Some(*status);
                e.host_status = Some(*host_status);
                e.driver_status = Some(*driver_status);
                e.set_sense(sense);
                // Captured sense cannot overrule a failed host or driver completion.
                e.boundary = BoundaryFlags::default();
            }
            ScsiError::Io(_) => e = Self::unknown(),
            ScsiError::UnexpectedStatus { status } => e.scsi_status = Some(*status),
            ScsiError::Cancelled
            | ScsiError::InvalidInput(_)
            | ScsiError::HostCapacityRefused(_) => {
                e.dispatch = Dispatch::NotIssued;
                e.fact = CompletionFact::Refusal(RetryCause::Transient);
            }
            ScsiError::Truncated { .. } | ScsiError::InvalidResponse { .. } => {}
        }
        e
    }
    /// Strict normalization for adapters that retain raw sense but no SCSI error.
    pub fn from_sense(sense: &[u8]) -> Self {
        #[cfg(any(test, feature = "outcome-census"))]
        census::normalize();
        let mut e = Self {
            scsi_status: Some(2),
            failed: true,
            ..Self::success()
        };
        e.set_sense(sense);
        if e.decoded.is_none_or(|s| s.is_deferred()) {
            e.completion = Completion::Unknown;
        }
        e
    }
    fn set_sense(&mut self, sense: &[u8]) {
        self.sense = Some(sense.to_vec());
        self.decoded = decode_usable_sense(sense);
        if let Some(d) = self.decoded.filter(|d| !d.is_deferred()) {
            self.residual_valid = d.valid;
            self.residual = if d.valid {
                sense_information(sense)
            } else {
                None
            };
            self.boundary = BoundaryFlags {
                filemark: d.filemark,
                eom: d.eom,
                ili: d.ili,
                physical_eod: d.key == 8
                    && d.asc == 0
                    && d.ascq == 5
                    && !d.filemark
                    && self.transferred_bytes == Some(0),
                recovered: d.key == 1 && !d.filemark && !d.eom && !d.ili,
            };
        }
    }
    /// Refine captured evidence with the final Layer-3a result, without decoding
    /// sense again. Counts here are the arbitration result, not a host guess.
    pub(crate) fn refine_error(&mut self, error: &TapeIoError) {
        self.failed = true;
        match error {
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::GoodWriteTripwire { outcome, .. },
                ..
            } => {
                self.fact = CompletionFact::PostTransferFailure;
                self.records_transferred = Some(outcome.records_written);
                self.transferred_bytes = Some(u64::from(outcome.bytes_written));
            }
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::WriteFailureWithPositionError { write_error, .. },
                ..
            } => self.refine_error(write_error),
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::PartitionProbeFailed(cause),
                ..
            } => self.refine_error(cause),
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::Transport(_),
                ..
            } => {
                self.completion = Completion::Unknown;
                self.boundary = BoundaryFlags::default();
            }
            crate::TapeIoError {
                kind:
                    crate::TapeIoErrorKind::PartialBatchUncommittable {
                        requested_records,
                        written_records,
                        requested_bytes,
                        written_bytes,
                        end_of_medium,
                        ..
                    },
                ..
            } => {
                self.fact = CompletionFact::PartialWrite;
                self.requested_records = Some(*requested_records);
                self.records_transferred = Some(*written_records);
                self.requested_bytes = Some(*requested_bytes);
                self.transferred_bytes = Some(*written_bytes);
                self.boundary.eom = *end_of_medium;
            }
            crate::TapeIoError {
                kind:
                    crate::TapeIoErrorKind::CheckConditionWithResidue {
                        bytes_transferred, ..
                    },
                ..
            } => {
                self.transferred_bytes = Some(u64::from(*bytes_transferred));
                self.boundary.physical_eod = self.current_sense().is_some_and(|d| {
                    d.key == 8
                        && d.asc == 0
                        && d.ascq == 5
                        && !d.filemark
                        && *bytes_transferred == 0
                });
            }
            crate::TapeIoError {
                kind:
                    crate::TapeIoErrorKind::GoodReadLengthMismatch {
                        requested_bytes,
                        bytes_transferred,
                    },
                ..
            } => {
                self.fact = CompletionFact::GoodReadLengthMismatch;
                self.completion = Completion::Unknown;
                self.requested_bytes = Some(u64::from(*requested_bytes));
                self.host_transferred_bytes = Some(u64::from(*bytes_transferred));
            }
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::PositionDrift { .. },
                ..
            } => self.fact = CompletionFact::PositionDrift,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::DataProtect(_),
                ..
            } => self.fact = CompletionFact::DataProtect,
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::MultiPartitionMedium { .. },
                ..
            } => {
                self.fact = CompletionFact::Refusal(RetryCause::Operator);
            }
            crate::TapeIoError {
                kind: crate::TapeIoErrorKind::FilemarkEncountered,
                ..
            }
            | crate::TapeIoError {
                kind: crate::TapeIoErrorKind::CursorPredecessorNotFilemark,
                ..
            }
            | crate::TapeIoError {
                kind: crate::TapeIoErrorKind::ReadBufferTooSmall { .. },
                ..
            } => {
                self.fact = CompletionFact::CursorContradiction;
            }
            _ => {}
        }
    }

    /// Current usable sense is policy evidence only after a known completion.
    pub fn current_sense(&self) -> Option<DecodedSense> {
        self.decoded
            .filter(|s| self.completion == Completion::Known && !s.is_deferred())
    }
    /// A current MEDIUM ERROR, not malformed autosense or a transport failure.
    pub fn current_medium_damage(&self) -> bool {
        self.current_sense().is_some_and(|s| s.key == 3)
    }
    /// A zero-byte refusal with no inconsistent sense-residual claim.
    pub fn definitely_rejected(&self) -> bool {
        self.current_sense().is_some_and(|s| {
            matches!(s.key, 2 | 5 | 7)
                || (s.key == 6 && s.asc == 0x29 && matches!(s.ascq, 0..=4 | 7))
        }) && self.host_transferred_bytes == Some(0)
            && self
                .residual
                .zip(self.requested_bytes)
                .is_none_or(|(r, bytes)| {
                    r.saturating_mul(u64::from(self.block_size.unwrap_or(1))) == bytes
                })
    }
    /// Isolated physical contradictions used by admission and checked closeout.
    pub fn committed_contradiction(&self) -> bool {
        if self.fact == CompletionFact::GoodReadLengthMismatch {
            return false;
        }
        self.fact == CompletionFact::CursorContradiction
            || self
                .current_sense()
                .is_some_and(|s| s.key == 0 && s.filemark)
            || self.boundary.ili
            || self.current_sense().is_some_and(|s| {
                s.key == 3 || s.key == 8 || (s.key == 4 && matches!(s.asc, 0x11 | 0x14))
            })
    }
}
/// Read a complete information field, preserving absent versus valid zero.
fn sense_information(sense: &[u8]) -> Option<u64> {
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
impl crate::TapeIoErrorKind {
    /// Completion evidence for a typed error, including synthetic adapter errors.
    /// Wrappers preserve their primary command rather than a secondary diagnostic.
    pub(crate) fn normalize(&self) -> CompletionEvidence {
        use crate::TapeIoErrorKind::*;
        match self {
            CheckCondition(e)
            | UnexpectedStatus(e)
            | InvalidRequest(e)
            | Transport(e)
            | DataProtect(e)
            | StateInvalidatingReset(e)
            | MalformedResponse(e) => {
                let mut result = CompletionEvidence::from_scsi(e);
                if matches!(self, DataProtect(_)) {
                    result.fact = CompletionFact::DataProtect;
                }
                if matches!(self, Transport(_)) {
                    result.completion = Completion::Unknown;
                    result.position = PositionEffect::Invalidated;
                    result.boundary = BoundaryFlags::default();
                }
                result
            }
            CheckConditionWithResidue {
                source,
                bytes_transferred,
            } => {
                let mut e = CompletionEvidence::from_scsi(source);
                e.transferred_bytes = Some(u64::from(*bytes_transferred));
                e.boundary.physical_eod = e.current_sense().is_some_and(|d| {
                    d.key == 8
                        && d.asc == 0
                        && d.ascq == 5
                        && !d.filemark
                        && *bytes_transferred == 0
                });
                e
            }
            PartialBatchUncommittable {
                requested_records,
                written_records,
                requested_bytes,
                written_bytes,
                sense,
                end_of_medium,
            } => {
                let mut e = sense
                    .as_ref()
                    .map_or_else(CompletionEvidence::success, |s| {
                        CompletionEvidence::from_sense(s)
                    });
                e.fact = CompletionFact::PartialWrite;
                e.failed = true;
                e.requested_records = Some(*requested_records);
                e.records_transferred = Some(*written_records);
                e.requested_bytes = Some(*requested_bytes);
                e.transferred_bytes = Some(*written_bytes);
                e.boundary.eom = *end_of_medium;
                e.position = PositionEffect::Invalidated;
                e
            }
            PositionDrift { .. } => {
                let mut e = CompletionEvidence::success();
                e.failed = true;
                e.fact = CompletionFact::PositionDrift;
                e.position = PositionEffect::Invalidated;
                e
            }
            GoodWriteTripwire { cause, .. } => cause.completion_evidence(),
            WriteFailureWithPositionError { write_error, .. } => write_error.completion_evidence(),
            PartitionProbeFailed(cause) => cause.completion_evidence(),
            GoodReadLengthMismatch {
                requested_bytes,
                bytes_transferred,
            } => {
                let mut e = CompletionEvidence::unknown();
                e.fact = CompletionFact::GoodReadLengthMismatch;
                e.scsi_status = Some(0);
                e.requested_bytes = Some(u64::from(*requested_bytes));
                e.host_transferred_bytes = Some(u64::from(*bytes_transferred));
                e
            }
            HardEndOfMedium { sense } => {
                let mut e = CompletionEvidence::from_sense(sense);
                e.position = PositionEffect::Invalidated;
                e
            }
            FilemarkEncountered | CursorPredecessorNotFilemark | ReadBufferTooSmall { .. } => {
                let mut e = CompletionEvidence::success();
                e.failed = true;
                e.fact = CompletionFact::CursorContradiction;
                e.boundary.filemark = matches!(self, FilemarkEncountered);
                e
            }
            _ => {
                let mut e = CompletionEvidence::success();
                e.failed = true;
                e.dispatch = Dispatch::NotIssued;
                e.fact = CompletionFact::Refusal(
                    if matches!(self, WriteProtected | MultiPartitionMedium { .. }) {
                        RetryCause::Operator
                    } else {
                        RetryCause::Transient
                    },
                );
                e
            }
        }
    }
}
/// No strings are accepted: messages may describe an outcome but cannot decide it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutcomeInput {
    /// Command semantics.
    pub class: CommandClass,
    /// Caller authority.
    pub context: OperationContext,
    /// Captured command evidence.
    pub evidence: CompletionEvidence,
}
/// Durable or transient consequence; NoFence is not synonymous with success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Persist the existing tape fence.
    WriteFence(FenceCause),
    /// Report damage without fencing the tape.
    ReadFinding(Range<u64>),
    /// Existing drive-fault handling.
    DriveFault,
    /// Refresh the affected inventory partition.
    InventoryUnknown(PartitionIdentity),
    /// Back off without a durable consequence.
    Transient,
    /// No new fence is warranted.
    NoFence,
}
/// Diagnostic reason for the decision. Consumers act on effect and retry instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutcomeReason {
    /// Successful completion.
    Completed,
    /// No usable captured sense.
    UnusableSense,
    /// Host or driver failure.
    TransportFailure,
    /// Proven committed contradiction.
    CommittedContradiction,
    /// Validated partial write.
    UncommittablePartialWrite,
    /// Command definitely rejected.
    DefiniteRejection,
    /// Durable record awaits projection.
    ProjectionUnavailable,
    /// Authority publication is uncertain.
    AuthorityAmbiguous,
    /// Live ending durably names the uncommitted tail.
    NamedLiveTail,
    /// Cursor proof failed.
    PositionDrift,
    /// Reset invalidated write state.
    ResetUnitAttention,
    /// Terminal drive condition.
    DriveFailure,
    /// Current media damage.
    MediaDamage,
    /// Transient device response.
    DeviceFailure,
    /// Host refusal.
    NotIssued,
    /// Existing admission recovery action.
    AdmissionRecovery,
}
/// The single decision passed to persistence, retry and rendering consumers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassifiedOutcome {
    /// The result was finished for a modifying command with tape authority.
    pub write_context: bool,
    /// Completion certainty.
    pub completion: Completion,
    /// Consequence for today's persistence destination.
    pub effect: Effect,
    /// Existing retry budget.
    pub retry: RetryCause,
    /// Diagnostic-only reason.
    pub reason: OutcomeReason,
    /// Fresh cursor/identity proof is mandatory before another write.
    pub requires_reproof: bool,
    /// Original normalized command evidence.
    pub evidence: CompletionEvidence,
    /// Admission must perform its existing recovery flag and audit writes.
    pub recovery_required: bool,
}
/// Decide from typed evidence exactly once at the operation's command boundary.
pub fn classify(input: OutcomeInput) -> ClassifiedOutcome {
    #[cfg(any(test, feature = "outcome-census"))]
    census::increment(input.evidence.dispatch == Dispatch::Issued);
    let OutcomeInput {
        class,
        context,
        evidence,
    } = input;
    let writing = matches!(
        class,
        CommandClass::Write | CommandClass::WriteFilemarks { .. }
    );
    let authority = context.phase != Phase::Diagnostic && context.region != Region::NotTape;
    let mut out = ClassifiedOutcome {
        write_context: writing && authority,
        completion: evidence.completion,
        effect: Effect::NoFence,
        retry: RetryCause::Transient,
        reason: OutcomeReason::Completed,
        requires_reproof: evidence.failed
            && (evidence.fact == CompletionFact::PostTransferFailure
                || context.durability.reproof != Reproof::Proved),
        evidence,
        recovery_required: false,
    };
    if let CompletionFact::AbandonedWrite(retry) = out.evidence.fact {
        out.retry = retry;
    }
    let primary = &out.evidence;
    let e = if primary.fact == CompletionFact::PostTransferFailure {
        primary.post_transfer.as_deref().unwrap_or(primary)
    } else {
        primary
    };
    out.completion = e.completion;
    let d = &context.durability;
    if e.dispatch == Dispatch::NotIssued {
        out.reason = OutcomeReason::NotIssued;
        if let CompletionFact::Refusal(retry) = e.fact {
            out.retry = retry;
        }
        return out;
    }
    if let CompletionFact::Refusal(retry) = e.fact {
        out.retry = retry;
        out.reason = OutcomeReason::DefiniteRejection;
        return out;
    }
    if d.authority_append == AuthorityAppend::Durable && d.projection == Projection::Pending {
        out.reason = OutcomeReason::ProjectionUnavailable;
        return out;
    }
    let composite = match e.fact {
        CompletionFact::SuffixContradiction => Some(FenceCause::SessionSuffixContradiction),
        CompletionFact::CloseoutMismatch if d.checksum_attempts >= 2 => {
            Some(FenceCause::CloseoutCheckMismatch)
        }
        CompletionFact::PositionDrift if writing => Some(FenceCause::PositionDrift),
        _ if d.authority_append == AuthorityAppend::Ambiguous => {
            Some(FenceCause::AuthorityAmbiguous)
        }
        _ => None,
    };
    if let Some(cause) = composite.filter(|_| authority) {
        out.effect = Effect::WriteFence(cause);
        out.retry = if cause == FenceCause::PositionDrift {
            RetryCause::Transient
        } else {
            RetryCause::Operator
        };
        out.reason = if cause == FenceCause::AuthorityAmbiguous {
            OutcomeReason::AuthorityAmbiguous
        } else {
            OutcomeReason::CommittedContradiction
        };
        return out;
    }
    if e.fact == CompletionFact::AdmissionRecovery {
        out.recovery_required = true;
        out.retry = RetryCause::Operator;
        out.reason = OutcomeReason::AdmissionRecovery;
        return out;
    }
    let motion = matches!(
        class,
        CommandClass::Move | CommandClass::Load | CommandClass::Unload
    );
    let current = e.current_sense();
    if e.fact == CompletionFact::DataProtect {
        out.retry = RetryCause::Operator;
    }
    if current.is_some_and(|s| matches!(s.key, 3 | 4)) {
        out.retry = RetryCause::Media;
    }
    if current.is_some_and(|s| matches!(s.key, 5 | 7)) && e.definitely_rejected() {
        // Refusing the independent position query cannot undo the transfer
        // already completed by the primary command.
        if primary.fact != CompletionFact::PostTransferFailure {
            out.reason = OutcomeReason::DefiniteRejection;
            return out;
        }
    }
    if e.fact == CompletionFact::PartialWrite && authority && writing {
        out.effect = Effect::WriteFence(FenceCause::PartialBatch);
        out.reason = OutcomeReason::UncommittablePartialWrite;
        return out;
    }
    if e.completion == Completion::Unknown {
        out.reason = if e.scsi_status == Some(2) && e.decoded.is_none() {
            OutcomeReason::UnusableSense
        } else {
            OutcomeReason::TransportFailure
        };
        out.effect = if motion {
            Effect::InventoryUnknown(context.partition)
        } else if writing && authority {
            if d.named_live_tail {
                out.reason = OutcomeReason::NamedLiveTail;
                Effect::NoFence
            } else {
                Effect::WriteFence(FenceCause::TransferError)
            }
        } else {
            Effect::Transient
        };
        return out;
    }
    if !e.failed {
        return out;
    }
    if motion && current.is_some_and(|s| s.key == 6) {
        out.effect = Effect::InventoryUnknown(context.partition);
        out.reason = OutcomeReason::DeviceFailure;
        return out;
    }
    if writing
        && authority
        && current.is_some_and(|s| s.key == 6 && s.asc == 0x29 && matches!(s.ascq, 0..=4 | 7))
    {
        out.effect = Effect::WriteFence(FenceCause::ResetUnitAttention);
        out.reason = OutcomeReason::ResetUnitAttention;
        return out;
    }
    if class == CommandClass::SenseStatus
        && (current.is_some_and(|s| s.key == 2 && s.asc == 4 && matches!(s.ascq, 3 | 0x20..=0x22))
            || d.drive_failure_budget >= 3)
    {
        out.effect = Effect::DriveFault;
        out.reason = OutcomeReason::DriveFailure;
        return out;
    }
    if primary.fact != CompletionFact::PostTransferFailure && e.committed_contradiction() {
        match (&context.region, context.phase) {
            (Region::Committed(range), Phase::Read | Phase::Verification)
                if class == CommandClass::Read || e.fact == CompletionFact::CursorContradiction =>
            {
                out.effect = Effect::ReadFinding(range.clone());
                out.reason = OutcomeReason::MediaDamage;
                return out;
            }
            (
                Region::Bot | Region::Committed(_),
                Phase::Admission | Phase::Closeout | Phase::Finalization,
            ) if class == CommandClass::Read || e.fact == CompletionFact::CursorContradiction => {
                out.effect = Effect::WriteFence(if context.phase == Phase::Admission {
                    FenceCause::SessionSuffixContradiction
                } else {
                    FenceCause::CloseoutReadContradiction
                });
                out.reason = OutcomeReason::CommittedContradiction;
                return out;
            }
            (Region::AtOrAfterCursor(_), _) if d.reproof == Reproof::Contradicted => {
                out.effect = Effect::WriteFence(FenceCause::SessionSuffixContradiction);
                out.reason = OutcomeReason::CommittedContradiction;
                return out;
            }
            _ => {}
        }
    }
    out.effect = if writing
        && authority
        && (primary.fact == CompletionFact::PostTransferFailure || !e.definitely_rejected())
    {
        Effect::WriteFence(FenceCause::TransferError)
    } else {
        Effect::Transient
    };
    out.reason = OutcomeReason::DeviceFailure;
    out
}

/// Test-only per-thread command classification accounting.
#[cfg(any(test, feature = "outcome-census"))]
pub mod census {
    use std::cell::Cell;
    std::thread_local! { static COUNT: Cell<u64> = const { Cell::new(0) }; static TOTAL: Cell<u64> = const { Cell::new(0) }; static NORMALIZATIONS: Cell<u64> = const { Cell::new(0) }; }
    pub(super) fn normalize() {
        NORMALIZATIONS.with(|c| c.set(c.get() + 1));
    }
    /// Number of raw-error normalization operations on this thread.
    pub fn normalizations() -> u64 {
        NORMALIZATIONS.with(Cell::get)
    }
    pub(super) fn increment(issued: bool) {
        TOTAL.with(|c| c.set(c.get() + 1));
        if issued {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    }
    /// All classifier calls, including local refusals and synthetic facts.
    pub fn total() -> u64 {
        TOTAL.with(Cell::get)
    }
    /// Classifications of issued commands/facts; local preflight refusals are separate.
    pub fn count() -> u64 {
        COUNT.with(Cell::get)
    }
}
#[cfg(test)]
#[path = "b3b_tests.rs"]
mod tests;

impl RetryCause {
    /// Read the retry axis already classified at the error's finishing boundary.
    pub fn tape_io(error: &TapeIoError) -> Self {
        error.outcome().retry
    }
}

/// Finish a synthetic tape error at construction; dispatched commands use their captured evidence.
pub(crate) fn finish_tapeioerror(
    kind: &crate::TapeIoErrorKind,
    class: CommandClass,
    context: OperationContext,
) -> ClassifiedOutcome {
    use crate::TapeIoErrorKind;
    match kind {
        TapeIoErrorKind::PartitionProbeFailed(error)
        | TapeIoErrorKind::GoodWriteTripwire { cause: error, .. }
        | TapeIoErrorKind::WriteFailureWithPositionError {
            write_error: error, ..
        } => return error.outcome().clone(),
        _ => {}
    }
    classify(OutcomeInput {
        class,
        context,
        evidence: kind.normalize(),
    })
}
/// Finish a changer refusal or a directly constructed changer error.
pub(crate) fn finish_moveerror(kind: &crate::MoveErrorKind) -> ClassifiedOutcome {
    let evidence = match kind {
        crate::MoveErrorKind::ScsiError(error) => CompletionEvidence::from_scsi(error),
        _ => {
            let mut e = CompletionEvidence::success();
            e.failed = true;
            e.dispatch = Dispatch::NotIssued;
            e.fact = CompletionFact::Refusal(RetryCause::Operator);
            e
        }
    };
    classify(OutcomeInput {
        class: CommandClass::Move,
        context: OperationContext::default(),
        evidence,
    })
}
/// Finish a drive-control refusal, preserving an already-finished inventory error.
pub(crate) fn finish_driveoperror(kind: &crate::DriveOpErrorKind) -> ClassifiedOutcome {
    let evidence = match kind {
        crate::DriveOpErrorKind::Inventory(error) => return error.outcome().clone(),
        crate::DriveOpErrorKind::ScsiError(error) => CompletionEvidence::from_scsi(error),
        _ => {
            let mut e = CompletionEvidence::success();
            e.failed = true;
            e.dispatch = Dispatch::NotIssued;
            e.fact = CompletionFact::Refusal(RetryCause::Operator);
            e
        }
    };
    classify(OutcomeInput {
        class: CommandClass::OtherControl,
        context: OperationContext::default(),
        evidence,
    })
}
impl From<remanence_scsi::ScsiError> for crate::MoveError {
    fn from(error: remanence_scsi::ScsiError) -> Self {
        Self::new(crate::MoveErrorKind::ScsiError(error))
    }
}
impl From<remanence_scsi::ScsiError> for crate::DriveOpError {
    fn from(error: remanence_scsi::ScsiError) -> Self {
        Self::new(crate::DriveOpErrorKind::ScsiError(error))
    }
}
impl From<crate::MoveError> for crate::DriveOpError {
    fn from(error: crate::MoveError) -> Self {
        Self::new(crate::DriveOpErrorKind::Inventory(error))
    }
}

/// Finish one raw control command at its owner, before any renderer or retry consumer.
pub fn finish_control(
    class: CommandClass,
    context: OperationContext,
    opcode: u8,
    error: Option<&ScsiError>,
) -> ClassifiedOutcome {
    let mut evidence =
        error.map_or_else(CompletionEvidence::success, CompletionEvidence::from_scsi);
    evidence.opcode = Some(opcode);
    classify(OutcomeInput {
        class,
        context,
        evidence,
    })
}

/// Finish the distinct host fact that an operation ended after a write without a returned command result.
pub fn finish_abandoned_write(context: OperationContext) -> ClassifiedOutcome {
    classify(OutcomeInput {
        class: CommandClass::Write,
        context,
        evidence: CompletionEvidence::unknown(),
    })
}

/// Finish the S15 host fact while preserving the failed operation's typed retry.
pub fn finish_abandoned_write_with_retry(
    context: OperationContext,
    retry: RetryCause,
) -> ClassifiedOutcome {
    let mut evidence = CompletionEvidence::unknown();
    evidence.fact = CompletionFact::AbandonedWrite(retry);
    classify(OutcomeInput {
        class: CommandClass::Write,
        context,
        evidence,
    })
}
