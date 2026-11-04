//! Per-handle result carrier. Context is scoped by the owner; consumers borrow
//! immutable outcomes and never call the pure classifier for the same command.
use super::*;
use std::sync::{Arc, Mutex};

/// Restores the previous command context on every exit, including unwinding.
#[must_use]
pub struct ContextGuard {
    context: Arc<Mutex<OperationContext>>,
    previous: Option<OperationContext>,
}
impl Drop for ContextGuard {
    fn drop(&mut self) {
        *self.context.lock().unwrap_or_else(|e| e.into_inner()) =
            self.previous.take().expect("context guard");
    }
}
/// Handle-local carrier for the current operation's command outcomes.
#[derive(Default)]
pub(crate) struct CommandOutcomes {
    context: Arc<Mutex<OperationContext>>,
    last: Option<ClassifiedOutcome>,
    media_failure: Option<(u64, ClassifiedOutcome)>,
    applied_media_failure: Option<u64>,
    sequence: u64,
    pending: Vec<(CommandClass, Option<PendingCommand>)>,
}

/// Captured completion awaiting the owning command's final public result.
struct PendingCommand {
    class: CommandClass,
    context: OperationContext,
    evidence: CompletionEvidence,
    media_dispatches: u64,
}
impl CommandOutcomes {
    /// Finish a control result, including refusal before the transport was called.
    pub(crate) fn finish_control(
        &mut self,
        boundary: usize,
        error: Option<&mut crate::DriveOpError>,
    ) {
        assert_eq!(self.pending.len(), boundary + 1, "control boundary order");
        let (_, pending) = self.pending.pop().expect("control boundary");
        if let Some(pending) = pending {
            let outcome = self.publish(pending);
            if let Some(error) = error {
                error.set_outcome(outcome);
            }
        } else if let Some(error) = error {
            error.finish_synthetic();
        }
    }
    pub(crate) fn begin(&mut self, class: CommandClass) -> usize {
        let boundary = self.pending.len();
        if boundary == 0 {
            self.last = None;
        }
        self.pending.push((class, None));
        boundary
    }

    /// Finish after arbitration and position proof. A rejected preflight has no
    /// pending command and must neither classify nor reuse an older outcome.
    pub(crate) fn finish(
        &mut self,
        boundary: usize,
        mut error: Option<&mut TapeIoError>,
        transferred: Option<(u32, u64)>,
        position: PositionEffect,
    ) {
        assert_eq!(self.pending.len(), boundary + 1, "command boundary order");
        let (_, pending) = self.pending.pop().expect("command boundary");
        let Some(mut pending) = pending else {
            if let Some(error) = error {
                error.finish_synthetic();
            }
            return;
        };
        let evidence = &mut pending.evidence;
        match error.as_deref() {
            Some(error) => {
                // A failed independent proof is not the primary transfer's
                // sense. Preserve partial-write precedence; otherwise retain
                // the completed transfer and point policy at that proof.
                if evidence
                    .post_transfer
                    .as_ref()
                    .is_some_and(|proof| proof.failed)
                    && !matches!(
                        error,
                        crate::TapeIoError {
                            kind: crate::TapeIoErrorKind::WriteFailureWithPositionError { .. },
                            ..
                        } | crate::TapeIoError {
                            kind: crate::TapeIoErrorKind::PartialBatchUncommittable { .. },
                            ..
                        }
                    )
                {
                    evidence.failed = true;
                    evidence.fact = CompletionFact::PostTransferFailure;
                    if matches!(
                        error,
                        crate::TapeIoError {
                            kind: crate::TapeIoErrorKind::GoodWriteTripwire { .. },
                            ..
                        }
                    ) {
                        evidence.refine_error(error);
                    }
                } else {
                    evidence.refine_error(error);
                }
            }
            None => {
                evidence.failed = false;
                evidence.completion = Completion::Known;
                if let Some((records, bytes)) = transferred {
                    evidence.records_transferred = Some(records);
                    evidence.transferred_bytes = Some(bytes);
                }
            }
        }
        evidence.position =
            if evidence.dispatch == Dispatch::NotIssued || evidence.definitely_rejected() {
                PositionEffect::Unchanged
            } else {
                position
            };
        // Public adapters can add validity checks to the same command (for
        // example position() on position_report()). Let the outermost adapter
        // finalize that command. A second CDB already held by the parent, such
        // as LOCATE followed by READ POSITION, remains an independent result.
        if let Some((class, slot)) = self.pending.last_mut() {
            if *class == pending.class && slot.is_none() {
                *slot = Some(pending);
                return;
            }
        }
        let outcome = self.publish(pending);
        if let Some(error) = error.as_mut() {
            error.set_outcome(outcome);
        }
    }

    pub(crate) fn current_evidence(&self) -> Option<&CompletionEvidence> {
        match self.pending.last() {
            Some((_, pending)) => pending.as_ref().map(|p| &p.evidence),
            None => self.last.as_ref().map(|o| &o.evidence),
        }
    }

    /// Credited transfer arithmetic is known before the independent cursor proof.
    pub(crate) fn transfer_counts(&mut self, records: u32, bytes: u64) {
        if let Some((CommandClass::Read | CommandClass::Write, Some(command))) =
            self.pending.last_mut()
        {
            command.evidence.records_transferred = Some(records);
            command.evidence.transferred_bytes = Some(bytes);
        }
    }

    pub(crate) fn context(&self) -> OperationContext {
        self.context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn scope(&self, context: OperationContext) -> ContextGuard {
        let previous = std::mem::replace(
            &mut *self.context.lock().unwrap_or_else(|e| e.into_inner()),
            context,
        );
        ContextGuard {
            context: self.context.clone(),
            previous: Some(previous),
        }
    }
    pub(crate) fn record(
        &mut self,
        class: CommandClass,
        evidence: CompletionEvidence,
        media_dispatches: u64,
    ) {
        let context = self
            .context
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let pending = PendingCommand {
            class,
            context,
            evidence,
            media_dispatches,
        };
        if let Some((expected, slot)) = self.pending.last_mut() {
            if *expected == class {
                assert!(slot.is_none(), "multiple commands in one result boundary");
                *slot = Some(pending);
                return;
            }
        }
        self.publish(pending);
    }

    fn publish(&mut self, pending: PendingCommand) -> ClassifiedOutcome {
        let PendingCommand {
            class,
            context,
            evidence,
            media_dispatches,
        } = pending;
        let outcome = classify(OutcomeInput {
            class,
            context,
            evidence,
        });
        // Retain an independent position proof alongside its data command or
        // motion. It has already been normalized and classified; the outer
        // finisher must not decode it again or replace the primary completion.
        if class == CommandClass::Position {
            if let Some((_, Some(parent))) = self.pending.last_mut() {
                if matches!(
                    parent.class,
                    CommandClass::Read
                        | CommandClass::Write
                        | CommandClass::WriteFilemarks { .. }
                        | CommandClass::Position
                ) {
                    parent.evidence.post_transfer = Some(Box::new(outcome.evidence.clone()));
                }
            }
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("command sequence exhausted");
        if matches!(
            class,
            CommandClass::Write | CommandClass::WriteFilemarks { .. }
        ) && outcome.evidence.failed
        {
            self.media_failure = Some((media_dispatches, outcome.clone()));
            self.applied_media_failure = None;
        }
        self.last = Some(outcome.clone());
        outcome
    }
    pub(crate) fn last(&self) -> Option<&ClassifiedOutcome> {
        self.last.as_ref()
    }
    pub(crate) fn media_failure_after(&self, before: u64) -> Option<&ClassifiedOutcome> {
        self.media_failure
            .as_ref()
            .filter(|(dispatches, _)| *dispatches > before)
            .map(|(_, outcome)| outcome)
    }
    /// Associate an already-finished adapter/host fact with this invocation's last media dispatch.
    pub(crate) fn retain_failure(&mut self, dispatches: u64, outcome: ClassifiedOutcome) {
        if self
            .media_failure
            .as_ref()
            .is_some_and(|(previous, value)| *previous == dispatches && value == &outcome)
        {
            return;
        }
        self.media_failure = Some((dispatches, outcome));
        self.applied_media_failure = None;
    }
    pub(crate) fn applied(&self) -> bool {
        self.media_failure
            .as_ref()
            .is_some_and(|(dispatches, _)| self.applied_media_failure == Some(*dispatches))
    }
    pub(crate) fn mark_applied(&mut self) {
        self.applied_media_failure = self
            .media_failure
            .as_ref()
            .map(|(dispatches, _)| *dispatches);
    }
}

impl CommandClass {
    /// Decode command semantics without inspecting messages or device sense.
    pub fn from_cdb(cdb: &[u8]) -> Self {
        match cdb.first().copied() {
            Some(0x08 | 0x88) => Self::Read,
            Some(0x0a | 0x8a) => Self::Write,
            Some(0x10) if cdb.len() >= 5 => Self::WriteFilemarks {
                count: u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]),
                immediate: cdb[1] & 1 != 0,
            },
            Some(0x01 | 0x11 | 0x2b | 0x34 | 0x91 | 0x92) => Self::Position,
            Some(0x1b) if cdb.get(4).is_some_and(|v| v & 1 != 0) => Self::Load,
            Some(0x1b) => Self::Unload,
            Some(0xa5) => Self::Move,
            Some(0x12) => Self::Inquiry,
            Some(0x00 | 0x03 | 0x4d) => Self::SenseStatus,
            Some(0x15 | 0x1a | 0x55 | 0x5a) => Self::Mode,
            Some(0x16 | 0x17 | 0x1e | 0x56 | 0x57 | 0x5e | 0x5f) => Self::Reservation,
            _ => Self::OtherControl,
        }
    }
}
