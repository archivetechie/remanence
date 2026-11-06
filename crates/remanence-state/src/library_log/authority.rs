//! Typed library writer. The state lease and append mutex cover finalization
//! checks, fold preflight, durable publication and the resulting in-memory state.
//! This API is deliberately disconnected from production until B4b-2.
use super::store::{Clock, Origin, StoreError, UuidSource};
use super::*;
use crate::StateLockGuard;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

/// Tape-summary adapter. Implementations must remain stable while the caller
/// holds the same tape admission ownership through this library append.
pub trait FinalizationView {
    /// Committed instance count from the native summary or projection. The caller
    /// keeps tape admission ownership stable through this append, as above.
    fn committed_instance_count(&self, tape: TapeRef) -> u64;
    /// An outstanding manual Prepared, or an automatic seal, forbids changing identity or assignment.
    fn finalization_blocks_change(&self, tape: TapeRef) -> bool;
}

/// Writer refusal; a pending failure names the durable entry requiring repair.
#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    /// Runtime admission found an open or Unknown physical motion in this scope.
    #[error("motion scope requires physical reconciliation before another command")]
    MotionReconciliationRequired,
    /// Resolve applied this prefix before refusing the candidate or a later pending record.
    /// Callers must project these records in order before reporting the refusal.
    #[error("{error}")]
    AppliedBeforeRefusal {
        /// The original typed refusal, including pending-record identity when applicable.
        error: Box<AuthorityError>,
        /// Fold-issued applications that remain valid despite the refusal.
        applied: Vec<AppliedLibraryRecord>,
    },
    /// Storage or fold preflight error.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// A tape summary protects an accepted or prepared finalization.
    #[error("finalization protects {0:?}")]
    FinalizationInProgress(TapeRef),
    /// Pool changes are forbidden while a tape holds committed data.
    #[error("tape {tape_uuid} holds {instance_count} committed instances; a tape holding data cannot change pool; it can only be retired")]
    TapeHoldsCommittedInstances {
        /// Occupied tape identity.
        tape_uuid: uuid::Uuid,
        /// Committed instances observed under append/admission ownership.
        instance_count: u64,
    },
    /// Do not invent caller-chosen operation identities.
    #[error("operation acceptance must mint a fresh identity")]
    OperationIdentityRequired,
    /// Deferred authority is retained and must not be skipped.
    #[error("pending record {sequence} ({record_uuid:?}) rejected: {error}")]
    PendingRecordRejected {
        /// Durable source sequence.
        sequence: u64,
        /// Durable record identity.
        record_uuid: Id,
        /// Frozen fold refusal.
        error: FoldError,
    },
}

/// A durable record for which the fold emitted an application notification.
/// Its private fields prevent constructing a projector input without validation.
#[derive(Clone, Debug)]
pub struct AppliedLibraryRecord {
    pub(crate) log_uuid: Id,
    pub(crate) record_hash: Hash,
    pub(crate) sequence: u64,
    pub(crate) record: Record,
    pub(crate) progress: FoldProgress,
}
impl AppliedLibraryRecord {
    /// Source sequence, not an SQL row identifier.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Read-only envelope and decoded record.
    pub fn record(&self) -> &Record {
        &self.record
    }
}

/// Fold-owned ingestion diagnostics captured after a receive/resolve batch.
/// This is not a readiness rule; SQL must never infer pending from absent rows.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct FoldProgress {
    pub received: u64,
    pub non_applications: u64,
    pub pending: u64,
}
impl FoldProgress {
    pub(crate) fn capture(fold: &LibraryFold) -> Self {
        Self {
            received: fold.received_through,
            non_applications: fold.non_applications,
            pending: fold.pending.len() as u64,
        }
    }
}

/// Publication receipt plus exactly the applications made by this call, in order.
#[derive(Clone, Debug)]
pub struct AuthorityReceipt {
    /// Durable acknowledgment (diagnostics may be explicitly unsynced).
    pub receipt: Receipt,
    /// Includes older pending entries retried before this append.
    pub applied: Vec<AppliedLibraryRecord>,
}

/// Sole typed library writer; no SQL connection and no channel waits under its locks.
///
/// ```compile_fail
/// use remanence_state::library_log::{Body, Diagnostic};
/// fn construct(value: Diagnostic) { let _ = Body::Diagnostic(value); }
/// ```
pub struct LibraryAuthority {
    // Drop the log before releasing the encompassing state-directory lease.
    log: Mutex<LibraryLog>,
    _state_lock: Arc<StateLockGuard>,
}
impl LibraryAuthority {
    /// Open the log under an owned state-directory lease. The log must reside
    /// beneath this lease's directory; callers cannot bind an unrelated lease.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        directory: &Path,
        log_uuid: Id,
        origin: Origin,
        clock: Box<dyn Clock>,
        uuids: Box<dyn UuidSource>,
        diagnostic_fsync: bool,
        view: &dyn TapeView,
        state_lock: Arc<StateLockGuard>,
    ) -> Result<Self, AuthorityError> {
        std::fs::create_dir_all(directory).map_err(StoreError::Io)?;
        let directory = std::fs::canonicalize(directory).map_err(StoreError::Io)?;
        let root =
            std::fs::canonicalize(state_lock.path().parent().unwrap()).map_err(StoreError::Io)?;
        if !directory.starts_with(&root) || directory == root {
            return Err(StoreError::Io(std::io::Error::other(
                "library log is outside state lease",
            ))
            .into());
        }
        Self::open_bound(
            directory.as_path(),
            log_uuid,
            origin,
            clock,
            uuids,
            diagnostic_fsync,
            view,
            state_lock,
        )
    }

    /// Open canonical configured roots already validated by the state manifest.
    /// Kept crate-private so arbitrary callers cannot bypass the lease binding.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_bound(
        directory: &Path,
        log_uuid: Id,
        origin: Origin,
        clock: Box<dyn Clock>,
        uuids: Box<dyn UuidSource>,
        diagnostic_fsync: bool,
        view: &dyn TapeView,
        state_lock: Arc<StateLockGuard>,
    ) -> Result<Self, AuthorityError> {
        Ok(Self {
            log: Mutex::new(LibraryLog::open(
                directory,
                log_uuid,
                origin,
                clock,
                uuids,
                diagnostic_fsync,
                view,
            )?),
            _state_lock: state_lock,
        })
    }

    /// Current typed identity from the fold; this never consults SQL.
    pub fn tape_state(&self, uuid: Id) -> Option<fold::TapeState> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .tapes
            .get(&uuid)
            .cloned()
    }

    /// Return exact typed raises for operator recovery; no SQL reason parsing.
    pub fn active_fence(&self, sequence: u64) -> Option<FenceRaised> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .active_fence(sequence)
            .cloned()
    }

    fn write(
        &self,
        body: Body,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
        finalization: Option<(&dyn FinalizationView, Vec<TapeRef>)>,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write_with_readiness_owner(body, actor, layer, view, finalization, false)
    }

    #[allow(clippy::too_many_arguments)]
    fn write_with_readiness_owner(
        &self,
        body: Body,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
        finalization: Option<(&dyn FinalizationView, Vec<TapeRef>)>,
        require_readiness_owner: bool,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((summary, tapes)) = &finalization {
            for &tape in tapes {
                if !matches!(body, Body::AssignPool(_)) && summary.finalization_blocks_change(tape)
                {
                    return Err(AuthorityError::FinalizationInProgress(tape));
                }
            }
        }
        // Confirmation is a writer duty, not a new replay-fold rule. Inspect
        // durable history under append ownership; report any refusal only after
        // pending resolution so its already-applied prefix is still forwarded.
        let confirmation = if let Body::MotionOutcome(outcome) = &body {
            outcome.confirmation.as_ref().map(|confirmation| {
                let (sequence, expected) = match confirmation {
                    Confirmation::Inventory(sequence) => (*sequence, Tag::InventoryReconciled),
                    Confirmation::Readiness(sequence) => (*sequence, Tag::ReadinessObserved),
                };
                log.lookup_application(sequence)
                    .map(|record| record.is_some_and(|(record, _)| record.body.tag() == expected))
            })
        } else {
            None
        };
        let mut sequences = Vec::new();
        let result = log
            .append_checked(
                body,
                actor,
                layer,
                view,
                &mut |n| sequences.push(n),
                &|fold, body| {
                    if require_readiness_owner {
                        let Body::ReadinessObserved(readiness) = body else {
                            return Err(FoldError::WrongCounterpart.into());
                        };
                        if !matches!(
                            fold.open_operation_kind(readiness.operation),
                            Some(OperationKind::LoadDrive | OperationKind::MediaReadinessWait)
                        ) {
                            return Err(FoldError::WrongCounterpart.into());
                        }
                    }
                    if let Some(confirmation) = &confirmation {
                        if !confirmation.clone()? {
                            return Err(FoldError::Confirmation.into());
                        }
                    }
                    // Resolve may change the assignment. Check occupancy against
                    // the resolved state while the append/admission lock still holds.
                    if let (Body::AssignPool(assignment), Some((summary, _))) =
                        (body, &finalization)
                    {
                        if fold
                            .tapes
                            .get(&assignment.tape.uuid)
                            .is_some_and(|state| state.pool != assignment.pool)
                        {
                            let instance_count = summary.committed_instance_count(assignment.tape);
                            if instance_count != 0 {
                                return Err(StoreError::TapeHoldsCommittedInstances {
                                    tape_uuid: uuid::Uuid::from_bytes(assignment.tape.uuid),
                                    instance_count,
                                });
                            }
                        }
                        if summary.finalization_blocks_change(assignment.tape) {
                            return Err(StoreError::FinalizationInProgress(assignment.tape));
                        }
                    }
                    if let Body::FenceRaised(fence) = body {
                        let live = |t: TapeRef| {
                            let state = fold.tapes.get(&t.uuid).ok_or(FoldError::UnknownTape)?;
                            if state.tape != t {
                                return Err(FoldError::IncarnationMismatch);
                            }
                            if state.retired {
                                return Err(FoldError::RetiredIdentity);
                            }
                            Ok(())
                        };
                        if let FenceScope::Tape(t) = fence.scope {
                            live(t)?;
                        }
                        if let Some(at) = &fence.at {
                            live(at.tape)?;
                        }
                    }
                    Ok(())
                },
            )
            .map_err(|error| {
                if let StoreError::FinalizationInProgress(tape) = error {
                    return AuthorityError::FinalizationInProgress(tape);
                }
                if let StoreError::TapeHoldsCommittedInstances {
                    tape_uuid,
                    instance_count,
                } = error
                {
                    return AuthorityError::TapeHoldsCommittedInstances {
                        tape_uuid,
                        instance_count,
                    };
                }
                if let StoreError::Fold(ref fold_error) = error {
                    if let Some((sequence, record_uuid)) = log.state().fold.pending_rejection {
                        return AuthorityError::PendingRecordRejected {
                            sequence,
                            record_uuid,
                            error: fold_error.clone(),
                        };
                    }
                }
                AuthorityError::Store(error)
            });
        let progress = FoldProgress::capture(&log.state().fold);
        let applied = sequences
            .into_iter()
            .map(|sequence| {
                let (record, record_hash) = log
                    .lookup_application(sequence)
                    .map_err(StoreError::Fold)?
                    .ok_or(StoreError::Fold(FoldError::HistoryUnavailable))?;
                Ok(AppliedLibraryRecord {
                    log_uuid: log.log_uuid(),
                    record_hash,
                    sequence,
                    record,
                    progress,
                })
            })
            .collect::<Result<_, StoreError>>()?;
        match result {
            Ok(receipt) => Ok(AuthorityReceipt { receipt, applied }),
            Err(error) if applied.is_empty() => Err(error),
            Err(error) => Err(AuthorityError::AppliedBeforeRefusal {
                error: Box::new(error),
                applied,
            }),
        }
    }

    /// Accept a new operation, minting its identity here instead of trusting a caller-supplied UUID.
    pub fn begin_operation(
        &self,
        kind: OperationKind,
        accepted: Accepted,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<(Id, AuthorityReceipt), AuthorityError> {
        let operation = *uuid::Uuid::new_v4().as_bytes();
        let receipt = self.write(
            Body::OperationRecorded(OperationRecorded {
                operation,
                kind,
                previous: None,
                event: OperationEvent::Accepted(accepted),
            }),
            actor,
            layer,
            view,
            None,
        )?;
        Ok((operation, receipt))
    }
    /// Start an unchained operation with a fresh identity (no request acceptance).
    pub fn start_operation(
        &self,
        kind: OperationKind,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<(Id, AuthorityReceipt), AuthorityError> {
        let operation = *uuid::Uuid::new_v4().as_bytes();
        let receipt = self.write(
            Body::OperationRecorded(OperationRecorded {
                operation,
                kind,
                previous: None,
                event: OperationEvent::Started(Unit),
            }),
            actor,
            layer,
            view,
            None,
        )?;
        Ok((operation, receipt))
    }
    /// Advance an existing operation. Initial events use `begin_operation` or `start_operation`.
    pub fn operation_recorded(
        &self,
        value: OperationRecorded,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        if matches!(value.event, OperationEvent::Accepted(_)) || value.previous.is_none() {
            return Err(AuthorityError::OperationIdentityRequired);
        }
        self.write(Body::OperationRecorded(value), actor, layer, view, None)
    }
    /// Publish AssignPool with an explicit expected generation.
    #[allow(clippy::too_many_arguments)]
    pub fn assign_pool(
        &self,
        mut value: AssignPool,
        expected: u64,
        finalization: &dyn FinalizationView,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        value.change = GenerationChange {
            expected,
            next: expected
                .checked_add(1)
                .ok_or(StoreError::Fold(FoldError::GenerationMismatch))?,
        };
        let finalization = Some((finalization, vec![value.tape]));
        self.write(Body::AssignPool(value), actor, layer, view, finalization)
    }
    /// Publish Label with an explicit expected generation.
    #[allow(clippy::too_many_arguments)]
    pub fn label(
        &self,
        mut value: Label,
        expected: u64,
        finalization: &dyn FinalizationView,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        value.change = GenerationChange {
            expected,
            next: expected
                .checked_add(1)
                .ok_or(StoreError::Fold(FoldError::GenerationMismatch))?,
        };
        let finalization = Some((
            finalization,
            std::iter::once(value.tape)
                .chain(value.prior_holder.as_ref().map(|p| p.tape))
                .collect(),
        ));
        self.write(Body::Label(value), actor, layer, view, finalization)
    }
    /// Publish Retire with an explicit expected generation.
    #[allow(clippy::too_many_arguments)]
    pub fn retire(
        &self,
        mut value: Retire,
        expected: u64,
        finalization: &dyn FinalizationView,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        value.assignment_generation = expected;
        let finalization = Some((finalization, vec![value.tape]));
        self.write(Body::Retire(value), actor, layer, view, finalization)
    }
    /// The caller must state the provenance of what it actually did; copying a measured spec does not attest a new measurement.
    #[allow(clippy::too_many_arguments)]
    pub fn medium_spec_set(
        &self,
        mut value: MediumSpecSet,
        expected: u64,
        source: GenerationSource,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        value.change = GenerationChange {
            expected,
            next: expected
                .checked_add(1)
                .ok_or(StoreError::Fold(FoldError::GenerationMismatch))?,
        };
        value.medium.source = source;
        let finalization = None;
        self.write(Body::MediumSpecSet(value), actor, layer, view, finalization)
    }
    /// Publish DriveDecision with an explicit expected generation.
    #[allow(clippy::too_many_arguments)]
    pub fn drive_decision(
        &self,
        mut value: DriveDecision,
        expected: u64,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        value.change = GenerationChange {
            expected,
            next: expected
                .checked_add(1)
                .ok_or(StoreError::Fold(FoldError::GenerationMismatch))?,
        };
        let finalization = None;
        self.write(Body::DriveDecision(value), actor, layer, view, finalization)
    }
    /// Publish AlarmChanged with an explicit expected generation.
    #[allow(clippy::too_many_arguments)]
    pub fn alarm_changed(
        &self,
        mut value: AlarmChanged,
        expected: u64,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        value.change = GenerationChange {
            expected,
            next: expected
                .checked_add(1)
                .ok_or(StoreError::Fold(FoldError::GenerationMismatch))?,
        };
        let finalization = None;
        self.write(Body::AlarmChanged(value), actor, layer, view, finalization)
    }
    /// Publish a typed Reprovision; fold validation precedes any publication.
    pub fn reprovision(
        &self,
        value: Reprovision,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::Reprovision(value), actor, layer, view, None)
    }
    /// Publish a typed SupersedeTail; fold validation precedes any publication.
    pub fn supersede_tail(
        &self,
        value: SupersedeTail,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::SupersedeTail(value), actor, layer, view, None)
    }
    /// Publish a typed AcceptUnprotectedFinalization; fold validation precedes any publication.
    pub fn accept_unprotected_finalization(
        &self,
        value: AcceptUnprotectedFinalization,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(
            Body::AcceptUnprotectedFinalization(value),
            actor,
            layer,
            view,
            None,
        )
    }
    /// Publish a typed FenceRaised; fold validation precedes any publication.
    pub fn fence_raised(
        &self,
        value: FenceRaised,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::FenceRaised(value), actor, layer, view, None)
    }
    /// Publish a typed FenceReleased; fold validation precedes any publication.
    pub fn fence_released(
        &self,
        value: FenceReleased,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::FenceReleased(value), actor, layer, view, None)
    }
    /// Current accepted configuration from authoritative replay.
    pub fn configuration(&self) -> Option<ConfigurationAccepted> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .configuration()
            .cloned()
    }
    /// Bounded active readiness lookup for the polling producer. Terminal
    /// chains require a separate authenticated historical retry lookup.
    pub(crate) fn active_readiness(
        &self,
        operation: Id,
    ) -> Result<Option<(DecisionRef, ReadinessObserved)>, AuthorityError> {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        if log.state().refusal.is_some() || !log.state().fold.pending.is_empty() {
            return Err(StoreError::Fold(FoldError::HistoryUnavailable).into());
        }
        Ok(log
            .state()
            .fold
            .readiness_head(operation)
            .map(|(r, v)| (r, v.clone())))
    }
    /// Existing chain and generation for a real readiness observation.
    pub fn readiness_context(
        &self,
        operation: Id,
        endpoint: &DriveKey,
    ) -> Result<(Option<(DecisionRef, ReadinessObserved)>, u64), AuthorityError> {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let fold = &log.state().fold;
        Ok((
            fold.readiness_head(operation)
                .map(|(source, value)| (source, value.clone())),
            fold.next_readiness_generation(endpoint)
                .map_err(StoreError::Fold)?,
        ))
    }
    /// Actual per-operation LOAD budget from admitted intent records.
    pub fn readiness_load_attempts(&self, operation: Id) -> Result<u32, AuthorityError> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .readiness_load_attempts(operation)
            .map_err(|error| StoreError::Fold(error).into())
    }
    /// Runtime admission checks the exact CDB target's open/Unknown motion.
    pub fn motion_requires_reconciliation(&self, motion: &Motion) -> bool {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .motion_requires_reconciliation(motion)
    }
    /// Read the next observation generation while runtime dispatch is serialized.
    pub fn next_inventory_generation(
        &self,
        subject: &InventorySubject,
    ) -> Result<u64, AuthorityError> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .next_inventory_generation(subject)
            .map_err(|error| StoreError::Fold(error).into())
    }
    /// Read the next LOAD attempt while the runtime serializes producer dispatch.
    pub fn next_load_attempt(
        &self,
        operation: Option<Id>,
        endpoint: &DriveKey,
    ) -> Result<u32, AuthorityError> {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .fold
            .next_load_attempt(operation, endpoint)
            .map_err(|error| StoreError::Fold(error).into())
    }
    /// Publish a typed MotionIntent; fold validation precedes any publication.
    pub fn motion_intent(
        &self,
        value: MotionIntent,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::MotionIntent(value), actor, layer, view, None)
    }
    /// Publish a typed MotionOutcome; fold validation precedes any publication.
    pub fn motion_outcome(
        &self,
        value: MotionOutcome,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::MotionOutcome(value), actor, layer, view, None)
    }
    /// Publish a typed InventoryReconciled; fold validation precedes any publication.
    pub fn inventory_reconciled(
        &self,
        value: InventoryReconciled,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::InventoryReconciled(value), actor, layer, view, None)
    }
    /// Publish a typed VerificationProgress; fold validation precedes any publication.
    pub fn verification_progress(
        &self,
        value: VerificationProgress,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::VerificationProgress(value), actor, layer, view, None)
    }
    /// Publish a typed VerificationVerdict; fold validation precedes any publication.
    pub fn verification_verdict(
        &self,
        value: VerificationVerdict,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::VerificationVerdict(value), actor, layer, view, None)
    }
    /// Publish a typed ManualFinalizationAccepted; fold validation precedes any publication.
    pub fn manual_finalization_accepted(
        &self,
        value: ManualFinalizationAccepted,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(
            Body::ManualFinalizationAccepted(value),
            actor,
            layer,
            view,
            None,
        )
    }
    /// Publish a typed ConfigurationAccepted; fold validation precedes any publication.
    pub fn configuration_accepted(
        &self,
        value: ConfigurationAccepted,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::ConfigurationAccepted(value), actor, layer, view, None)
    }
    /// Start a readiness operation with a freshly minted operation identity.
    /// The initial record must not name a previous readiness observation.
    pub fn begin_readiness(
        &self,
        mut value: ReadinessObserved,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<(Id, AuthorityReceipt), AuthorityError> {
        if value.previous.is_some() {
            return Err(AuthorityError::OperationIdentityRequired);
        }
        value.operation = *uuid::Uuid::new_v4().as_bytes();
        let operation = value.operation;
        let receipt = self.write(Body::ReadinessObserved(value), actor, layer, view, None)?;
        Ok((operation, receipt))
    }
    /// Begin readiness under an already writer-minted live operation. Owner
    /// validation and append share the source lock, including pending resolution.
    pub fn begin_readiness_for_operation(
        &self,
        value: ReadinessObserved,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        if value.previous.is_some() {
            return Err(AuthorityError::OperationIdentityRequired);
        }
        self.write_with_readiness_owner(
            Body::ReadinessObserved(value),
            actor,
            layer,
            view,
            None,
            true,
        )
    }
    /// Advance an existing readiness chain; initial observations use begin_readiness.
    pub fn readiness_observed(
        &self,
        value: ReadinessObserved,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        if value.previous.is_none() {
            return Err(AuthorityError::OperationIdentityRequired);
        }
        self.write(Body::ReadinessObserved(value), actor, layer, view, None)
    }
    /// Publish a typed DriveObserved; fold validation precedes any publication.
    pub fn drive_observed(
        &self,
        value: DriveObserved,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::DriveObserved(value), actor, layer, view, None)
    }
    /// Publish a typed DriveHealthObserved; fold validation precedes any publication.
    pub fn drive_health_observed(
        &self,
        value: DriveHealthObserved,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::DriveHealthObserved(value), actor, layer, view, None)
    }
    /// Publish a typed CleaningRecorded; fold validation precedes any publication.
    pub fn cleaning_recorded(
        &self,
        value: CleaningRecorded,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::CleaningRecorded(value), actor, layer, view, None)
    }
    /// Publish a typed SessionObserved; fold validation precedes any publication.
    pub fn session_observed(
        &self,
        value: SessionObserved,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::SessionObserved(value), actor, layer, view, None)
    }
    /// Publish a typed WrapMapCached; fold validation precedes any publication.
    pub fn wrap_map_cached(
        &self,
        value: WrapMapCached,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::WrapMapCached(value), actor, layer, view, None)
    }
    /// Publish a typed RecoveryRecorded; fold validation precedes any publication.
    pub fn recovery_recorded(
        &self,
        value: RecoveryRecorded,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::RecoveryRecorded(value), actor, layer, view, None)
    }
    /// Publish a typed Diagnostic; fold validation precedes any publication.
    pub fn diagnostic(
        &self,
        value: Diagnostic,
        actor: AuditActor,
        layer: SourceLayer,
        view: &dyn TapeView,
    ) -> Result<AuthorityReceipt, AuthorityError> {
        self.write(Body::Diagnostic(value), actor, layer, view, None)
    }
}

/// Frozen, validated library prefixes. Clones retain the state and store leases;
/// appending can continue, while a projector sees only the captured byte limits.
#[derive(Clone)]
pub struct LibrarySnapshot {
    source: store::ReadSnapshot,
    _state_lock: Arc<StateLockGuard>,
}
impl LibrarySnapshot {
    pub(crate) fn inspect(
        &self,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(AppliedLibraryRecord) -> Result<(), StoreError>,
        received: &mut dyn FnMut(&Record, &Receipt) -> Result<(), StoreError>,
    ) -> Result<store::Replay, StoreError> {
        self.source.inspect(view, applied, received)
    }
    pub(crate) fn replay(
        &self,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(AppliedLibraryRecord) -> Result<(), StoreError>,
    ) -> Result<store::Replay, StoreError> {
        self.source.replay(view, applied)
    }
}
impl LibraryAuthority {
    /// Capture durable references under the append lock, releasing it before
    /// any projector channel wait. No decoded record history is retained.
    pub fn snapshot(&self) -> Result<LibrarySnapshot, AuthorityError> {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        Ok(LibrarySnapshot {
            source: log.snapshot()?,
            _state_lock: self._state_lock.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_log::tests as fixture;
    /// A failed pending retry retains the exact failing record and every later
    /// pending entry, and the writer names that record instead of skipping it.
    #[test]
    fn authority_pending_retry_names_rejected_durable_record() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("library");
        let (mut log, _) = fixture::open(&dir, &fixture::Present);
        fixture::put(&mut log, fixture::provision(1, None));
        let receipt = fixture::put(
            &mut log,
            Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
                at: fixture::at(1, 1),
                operation: [8; 16],
                actor_fingerprint: "operator".into(),
                idempotency_key: [8; 16],
                request_fingerprint: [1; 32],
                assignment_generation: 1,
            }),
        );
        drop(log);
        let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
        let (log, _) = fixture::open(&dir, &fixture::View::default());
        let authority = LibraryAuthority {
            log: Mutex::new(log),
            _state_lock: lease,
        };
        let wrong = fixture::View(std::collections::BTreeMap::from([(
            ([1; 16], 1),
            TapeLookup::Present {
                kind: crate::tape_log::Tag::SessionOpened,
                digest: [1; 32],
            },
        )]));
        let Body::Diagnostic(diagnostic) = fixture::diagnostic() else {
            unreachable!()
        };
        let result =
            authority.diagnostic(diagnostic, AuditActor::System, SourceLayer::Layer4, &wrong);
        assert!(
            matches!(result,Err(AuthorityError::PendingRecordRejected {sequence,record_uuid,error:FoldError::CounterpartKind}) if sequence==receipt.sequence && record_uuid==receipt.record_uuid)
        );
        let log = authority.log.lock().unwrap();
        assert_eq!(log.state().last_sequence, 2);
        assert_eq!(log.state().fold.pending[0].sequence, receipt.sequence);
    }
}

#[cfg(test)]
mod refusal_delivery_tests {
    use super::*;
    use crate::library_log::tests as fixture;
    use crate::projector::apply::{ApplyPath, AuthorityRecord};

    struct Free;
    impl FinalizationView for Free {
        fn committed_instance_count(&self, _: TapeRef) -> u64 {
            0
        }
        fn finalization_blocks_change(&self, _: TapeRef) -> bool {
            false
        }
    }

    /// Resolution is durable fold output even when candidate preflight refuses.
    /// Forward the error's prefix immediately; the next success cannot re-emit it.
    #[test]
    fn authority_refused_append_delivers_resolved_prefix_without_restart() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("library");
        let (mut log, _) = fixture::open(&dir, &fixture::Present);
        fixture::put(&mut log, fixture::provision(1, None));
        fixture::put(
            &mut log,
            Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
                at: fixture::at(1, 1),
                operation: [8; 16],
                actor_fingerprint: "operator".into(),
                idempotency_key: [8; 16],
                request_fingerprint: [1; 32],
                assignment_generation: 1,
            }),
        );
        drop(log);
        let mut writer = crate::projector::memory_projection_fixture();
        let report = crate::projector::thread::rebuild_library(
            &mut writer,
            &dir,
            fixture::LOG,
            &fixture::View::default(),
            ApplyPath::Startup,
        )
        .unwrap();
        assert_eq!(report.pending.len(), 1);
        let (log, _) = fixture::open(&dir, &fixture::View::default());
        let authority = LibraryAuthority {
            log: Mutex::new(log),
            _state_lock: Arc::new(StateLockGuard::acquire(root.path()).unwrap()),
        };
        let refusal = authority
            .assign_pool(
                AssignPool {
                    tape: fixture::tape(1),
                    pool: Some("archive".into()),
                    change: GenerationChange {
                        expected: 999,
                        next: 1000,
                    },
                    source: AssignmentSource::Operator,
                    configuration: None,
                },
                999,
                &Free,
                AuditActor::System,
                SourceLayer::Layer4,
                &fixture::Present,
            )
            .unwrap_err();
        let AuthorityError::AppliedBeforeRefusal { error, applied } = refusal else {
            panic!("resolved prefix lost");
        };
        assert!(matches!(
            *error,
            AuthorityError::Store(StoreError::Fold(FoldError::GenerationMismatch))
        ));
        assert_eq!(
            applied
                .iter()
                .map(AppliedLibraryRecord::sequence)
                .collect::<Vec<_>>(),
            [2]
        );
        for record in applied {
            writer
                .apply(&AuthorityRecord::Library(Box::new(record)), ApplyPath::Live)
                .unwrap();
        }
        // The SQL effect of #2 must exist before another append or restart.
        let before_diagnostic = writer.normalized_authority_dump();
        assert_eq!(before_diagnostic["idempotency_keys"].len(), 1);
        assert_eq!(before_diagnostic["library_projection_records"].len(), 2);
        let Body::Diagnostic(diagnostic) = fixture::diagnostic() else {
            unreachable!()
        };
        let next = authority
            .diagnostic(
                diagnostic,
                AuditActor::System,
                SourceLayer::Layer4,
                &fixture::Present,
            )
            .unwrap();
        assert_eq!(
            next.applied
                .iter()
                .map(AppliedLibraryRecord::sequence)
                .collect::<Vec<_>>(),
            [3]
        );
        for record in next.applied {
            let marks = writer
                .apply(&AuthorityRecord::Library(Box::new(record)), ApplyPath::Live)
                .unwrap();
            assert_eq!((marks.applied_records, marks.pending), (3, 0));
        }
        assert_eq!(
            writer.normalized_authority_dump()["idempotency_keys"],
            before_diagnostic["idempotency_keys"]
        );
    }
    /// A later pending refusal preserves both the earlier application and the
    /// exact identity of the still-pending record that failed.
    #[test]
    fn authority_later_pending_refusal_delivers_applied_prefix() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("library");
        let (mut log, _) = fixture::open(&dir, &fixture::Present);
        fixture::put(&mut log, fixture::provision(1, None));
        fixture::put(&mut log, fixture::provision(2, None));
        let mut last = None;
        for n in [1u8, 2] {
            last = Some(fixture::put(
                &mut log,
                Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
                    at: fixture::at(n, 1),
                    operation: [n; 16],
                    actor_fingerprint: "operator".into(),
                    idempotency_key: [n; 16],
                    request_fingerprint: [n; 32],
                    assignment_generation: 1,
                }),
            ));
        }
        drop(log);
        let mut writer = crate::projector::memory_projection_fixture();
        crate::projector::thread::rebuild_library(
            &mut writer,
            &dir,
            fixture::LOG,
            &fixture::View::default(),
            ApplyPath::Startup,
        )
        .unwrap();
        let (log, _) = fixture::open(&dir, &fixture::View::default());
        let authority = LibraryAuthority {
            log: Mutex::new(log),
            _state_lock: Arc::new(StateLockGuard::acquire(root.path()).unwrap()),
        };
        let view = fixture::View(std::collections::BTreeMap::from([
            (
                ([1; 16], 1),
                TapeLookup::Present {
                    kind: crate::tape_log::Tag::FinalizationPrepared,
                    digest: [1; 32],
                },
            ),
            (
                ([2; 16], 1),
                TapeLookup::Present {
                    kind: crate::tape_log::Tag::SessionOpened,
                    digest: [1; 32],
                },
            ),
        ]));
        let Body::Diagnostic(d) = fixture::diagnostic() else {
            unreachable!()
        };
        let error = authority
            .diagnostic(d, AuditActor::System, SourceLayer::Layer4, &view)
            .unwrap_err();
        let AuthorityError::AppliedBeforeRefusal { error, applied } = error else {
            panic!("lost applied prefix")
        };
        let last = last.unwrap();
        assert!(
            matches!(*error,AuthorityError::PendingRecordRejected{sequence:4,record_uuid,error:FoldError::CounterpartKind} if record_uuid==last.record_uuid)
        );
        assert_eq!(
            applied
                .iter()
                .map(AppliedLibraryRecord::sequence)
                .collect::<Vec<_>>(),
            [3]
        );
        for record in applied {
            let marks = writer
                .apply(&AuthorityRecord::Library(Box::new(record)), ApplyPath::Live)
                .unwrap();
            assert_eq!((marks.applied_records, marks.pending), (3, 1));
        }
        assert_eq!(
            writer.normalized_authority_dump()["idempotency_keys"].len(),
            1
        );
    }
}
