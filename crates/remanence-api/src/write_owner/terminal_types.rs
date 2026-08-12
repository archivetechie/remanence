//! Terminal-finalization requests, plans, authority, and recovery helpers.

use std::path::Path;
use std::sync::Arc;

use remanence_parity::{
    FileTapeFileJournal, TapeFileJournal, TerminalComponentCommit,
    TerminalComponentReconcileEvidence, TerminalTailAuthority, TerminalTailProgress,
    TerminalTripleWritePlan,
};
use remanence_state::{
    AuditActor, CatalogIndex, TapePoolConfig, TerminalFinalizationOutcome,
    TerminalFinalizationProjection, TerminalFinalizationProjectionInput,
};
use tonic::Status;
use uuid::Uuid;

use super::terminal_finalize::authorize_terminal_intent_capacity;
use super::WriteOwnerConfig;
use crate::{SelectedTape, TapeUuid, FINALIZE_TAPE_OPERATION_KIND};

/// Fully authenticated manual close-out request dispatched under one tape owner.
///
/// The request bypasses only automatic low-watermark eligibility. The drive
/// actor still proves catalog assignment, checkpoint/tape-tail agreement,
/// terminal fit, and the immutable A/gap/B/gap/C layout before publishing the
/// durable `BeforeReplicaA` intent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ManualFinalizeTapeActorRequest {
    pub(crate) candidate_operation_id: Uuid,
    pub(crate) actor: AuditActor,
    pub(crate) actor_fingerprint: String,
    pub(crate) idempotency_key: Uuid,
    pub(crate) request_fingerprint: [u8; 32],
    pub(crate) tape_uuid: TapeUuid,
    pub(crate) expected_pool_id: Option<String>,
    pub(crate) assignment_generation: u64,
    pub(crate) reason: String,
    pub(crate) block_size: u32,
    pub(crate) parity_config: remanence_parity::ParityConfig,
    /// Exact assigned pool policy captured only after the assignment guard.
    /// `None` is the supported unpooled close-only profile.
    pub(crate) pool_config: Option<TapePoolConfig>,
}

/// Durable status returned after the drive actor has accepted or joined a close-out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManualFinalizeTapeActorReply {
    pub(crate) operation_id: Uuid,
    pub(crate) projection: TerminalFinalizationProjection,
}

/// Result of manual close-out admission under the exact-tape owner.
///
/// `Busy` is deliberately not an error: it is the typed, zero-side-effect
/// outcome when an Object/read owner or another tape reservation is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ManualFinalizeTapeResult {
    Busy,
    Accepted(ManualFinalizeTapeActorReply),
}

/// Filesystem and audit authority needed by the no-motion manual-finalize
/// preflight. Keeping this smaller than `WriteOwnerConfig` makes it explicit
/// that admission has no drive or changer capability.
pub(crate) struct ManualFinalizePreflightConfig<'a> {
    pub(crate) checkpoint_journal_dir: &'a Path,
    pub(crate) audit_dir: &'a Path,
    pub(crate) audit_fsync: bool,
    pub(crate) audit_append_lock: &'a Arc<std::sync::Mutex<()>>,
}

#[derive(Clone, Copy)]
pub(super) struct TerminalFinalizeAuditConfig<'a> {
    pub(super) audit_dir: &'a Path,
    pub(super) audit_fsync: bool,
    pub(super) audit_append_lock: &'a Arc<std::sync::Mutex<()>>,
}

impl<'a> From<&'a WriteOwnerConfig> for TerminalFinalizeAuditConfig<'a> {
    fn from(cfg: &'a WriteOwnerConfig) -> Self {
        Self {
            audit_dir: cfg.audit_dir.as_path(),
            audit_fsync: cfg.audit_fsync,
            audit_append_lock: &cfg.audit_append_lock,
        }
    }
}

pub(super) struct ManualFinalizeTapeMountRequest {
    pub(super) request: ManualFinalizeTapeActorRequest,
    pub(super) needs_drive_load: bool,
    pub(super) library_serial: String,
    pub(super) barcode: Option<String>,
    pub(super) source_slot: Option<u16>,
    pub(super) drive_uuid: Option<Vec<u8>>,
    pub(super) drive_serial: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct TerminalFinalizeSpec {
    pub(super) tape_uuid: TapeUuid,
    pub(super) block_size: u32,
    pub(super) pool_config: Option<TapePoolConfig>,
    pub(super) trigger: remanence_state::TerminalFinalizationTrigger,
    pub(super) operation_id: Option<Uuid>,
    pub(super) manual: Option<remanence_state::ManualTerminalFinalizationIdentity>,
}

impl TerminalFinalizeSpec {
    pub(super) fn operator(request: &ManualFinalizeTapeActorRequest) -> Self {
        Self {
            tape_uuid: request.tape_uuid,
            block_size: request.block_size,
            pool_config: request.pool_config.clone(),
            trigger: remanence_state::TerminalFinalizationTrigger::OperatorCloseOut,
            operation_id: Some(request.candidate_operation_id),
            manual: Some(remanence_state::ManualTerminalFinalizationIdentity {
                operation_id: *request.candidate_operation_id.as_bytes(),
                operation_kind: FINALIZE_TAPE_OPERATION_KIND.to_string(),
                actor_fingerprint: request.actor_fingerprint.clone(),
                idempotency_key: *request.idempotency_key.as_bytes(),
                request_fingerprint: request.request_fingerprint,
                assigned_pool_id: request.expected_pool_id.clone(),
                expected_pool_id: request.expected_pool_id.clone(),
                assignment_generation: request.assignment_generation,
                reason: request.reason.clone(),
            }),
        }
    }

    pub(super) fn automatic(
        selected: &SelectedTape,
        pool_config: &TapePoolConfig,
        trigger: remanence_state::TerminalFinalizationTrigger,
    ) -> Self {
        debug_assert_ne!(
            trigger,
            remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        );
        Self {
            tape_uuid: selected.tape_uuid,
            block_size: selected.block_size,
            pool_config: Some(pool_config.clone()),
            trigger,
            operation_id: None,
            manual: None,
        }
    }

    pub(super) fn resume(
        intent: &remanence_state::TerminalFinalizationIntent,
        block_size: u32,
        pool_config: &TapePoolConfig,
    ) -> Self {
        Self {
            tape_uuid: intent.tape_uuid,
            block_size,
            pool_config: Some(pool_config.clone()),
            trigger: intent.trigger,
            operation_id: intent
                .manual
                .as_ref()
                .map(|manual| Uuid::from_bytes(manual.operation_id)),
            manual: intent.manual.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct TerminalFinalizeResult {
    pub(super) projection: TerminalFinalizationProjection,
    pub(super) final_record: Option<remanence_state::CheckpointJournalRecord>,
}

pub(super) struct TerminalTailCatalogAuthority<'a> {
    pub(super) checkpoint: &'a mut remanence_state::FileCheckpointJournalLease,
    pub(super) parity_journal: Option<&'a mut FileTapeFileJournal>,
    pub(super) index: &'a mut CatalogIndex,
    pub(super) spec: &'a TerminalFinalizeSpec,
    pub(super) intent: remanence_state::TerminalFinalizationIntent,
    pub(super) tix_fault: Option<&'a crate::terminal_fault::TerminalFaultPlan>,
    pub(super) reconciliation: Option<(
        TerminalTailProgress,
        remanence_parity::TerminalTailComponentPlan,
        TerminalComponentReconcileEvidence,
    )>,
}

impl TerminalTailCatalogAuthority<'_> {
    pub(super) fn set_reconciliation(
        &mut self,
        progress: TerminalTailProgress,
        component: remanence_parity::TerminalTailComponentPlan,
        evidence: TerminalComponentReconcileEvidence,
    ) {
        self.reconciliation = Some((progress, component, evidence));
    }

    pub(super) fn projection_input(
        &self,
        progress: remanence_state::TerminalFinalizationProgress,
        outcome: TerminalFinalizationOutcome,
    ) -> TerminalFinalizationProjectionInput {
        TerminalFinalizationProjectionInput {
            tape_uuid: self.spec.tape_uuid,
            trigger: self.spec.trigger,
            operation_id: self.spec.operation_id,
            progress,
            edition_digest: self.intent.edition_digest,
            layout_digest: self.intent.layout.layout_digest,
            outcome,
            updated_at_utc: None,
        }
    }
}

impl TerminalTailAuthority for TerminalTailCatalogAuthority<'_> {
    fn load_progress(&mut self) -> Result<TerminalTailProgress, String> {
        Ok(parity_progress_from_state(self.intent.progress))
    }

    fn reconcile_next(
        &mut self,
        progress: TerminalTailProgress,
        component: remanence_parity::TerminalTailComponentPlan,
    ) -> Result<TerminalComponentReconcileEvidence, String> {
        let Some((expected_progress, expected_component, evidence)) = self.reconciliation.take()
        else {
            return Err("terminal-tail reconciliation was not supplied by the drive owner".into());
        };
        if expected_progress != progress || expected_component != component {
            return Err("terminal-tail reconciliation does not match the requested step".into());
        }
        Ok(evidence)
    }

    fn commit_after_barrier(&mut self, commit: &TerminalComponentCommit) -> Result<(), String> {
        if let Some(journal) = self.parity_journal.as_deref_mut() {
            if let Some(fault) = self.tix_fault {
                fault.abort_component_if_matches(
                    commit.component,
                    crate::terminal_fault::TerminalFaultCut::BeforeParityJournalFsync,
                    Some(commit.observed_position),
                )?;
            }
            journal
                .commit_terminal_component_transition(
                    &commit.journal_bundle,
                    &commit.checkpoint_bundle,
                )
                .map_err(|error| format!("commit terminal component journal: {error}"))?;
            if let Some(fault) = self.tix_fault {
                fault.abort_component_if_matches(
                    commit.component,
                    crate::terminal_fault::TerminalFaultCut::AfterParityJournalFsync,
                    Some(commit.observed_position),
                )?;
            }
        }
        let expected = state_progress_from_parity(commit.previous_progress);
        let next = state_progress_from_parity(commit.next_progress);
        if let Some(fault) = self.tix_fault {
            fault.abort_component_if_matches(
                commit.component,
                crate::terminal_fault::TerminalFaultCut::BeforeCheckpointJournalFsync,
                Some(commit.observed_position),
            )?;
        }
        self.intent = self
            .checkpoint
            .advance_terminal_finalization(expected, next)
            .map_err(|error| format!("advance terminal checkpoint progress: {error}"))?;
        if let Some(fault) = self.tix_fault {
            fault.abort_component_if_matches(
                commit.component,
                crate::terminal_fault::TerminalFaultCut::AfterCheckpointJournalFsync,
                Some(commit.observed_position),
            )?;
        }
        if let Some(fault) = self.tix_fault {
            fault.abort_component_if_matches(
                commit.component,
                crate::terminal_fault::TerminalFaultCut::BeforeSqliteProjection,
                Some(commit.observed_position),
            )?;
        }
        self.index
            .project_terminal_finalization(
                self.projection_input(next, TerminalFinalizationOutcome::InProgress),
            )
            .map_err(|error| format!("project terminal progress: {error}"))?;
        if let Some(fault) = self.tix_fault {
            fault.abort_component_if_matches(
                commit.component,
                crate::terminal_fault::TerminalFaultCut::AfterSqliteProjection,
                Some(commit.observed_position),
            )?;
        }
        Ok(())
    }
}

pub(super) const fn parity_progress_from_state(
    progress: remanence_state::TerminalFinalizationProgress,
) -> TerminalTailProgress {
    use remanence_state::TerminalFinalizationProgress as State;
    match progress {
        State::BeforeReplicaA => TerminalTailProgress::BeforeReplicaA,
        State::AfterReplicaA => TerminalTailProgress::AfterReplicaA,
        State::AfterSeparationAb => TerminalTailProgress::AfterSeparationAb,
        State::AfterReplicaB => TerminalTailProgress::AfterReplicaB,
        State::AfterSeparationBc => TerminalTailProgress::AfterSeparationBc,
        State::AfterReplicaC => TerminalTailProgress::AfterReplicaC,
    }
}

pub(super) const fn state_progress_from_parity(
    progress: TerminalTailProgress,
) -> remanence_state::TerminalFinalizationProgress {
    use remanence_state::TerminalFinalizationProgress as State;
    match progress {
        TerminalTailProgress::BeforeReplicaA => State::BeforeReplicaA,
        TerminalTailProgress::AfterReplicaA => State::AfterReplicaA,
        TerminalTailProgress::AfterSeparationAb => State::AfterSeparationAb,
        TerminalTailProgress::AfterReplicaB => State::AfterReplicaB,
        TerminalTailProgress::AfterSeparationBc => State::AfterSeparationBc,
        TerminalTailProgress::AfterReplicaC => State::AfterReplicaC,
    }
}

pub(super) const fn completed_terminal_component_count(
    progress: remanence_state::TerminalFinalizationProgress,
) -> u8 {
    use remanence_state::TerminalFinalizationProgress as State;
    match progress {
        State::BeforeReplicaA => 0,
        State::AfterReplicaA => 1,
        State::AfterSeparationAb => 2,
        State::AfterReplicaB => 3,
        State::AfterSeparationBc => 4,
        State::AfterReplicaC => 5,
    }
}

pub(super) fn reconcile_terminal_component_host_authority(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    spec: &TerminalFinalizeSpec,
    mut intent: remanence_state::TerminalFinalizationIntent,
    plan: &TerminalTripleWritePlan,
    journal: &mut FileTapeFileJournal,
) -> Result<remanence_state::TerminalFinalizationIntent, Status> {
    let transitions = plan
        .edition
        .descriptor
        .terminal_layout
        .components
        .iter()
        .map(|component| {
            let component =
                remanence_parity::terminal_component_bundle(plan, *component).map_err(|error| {
                    Status::failed_precondition(format!(
                        "build canonical terminal component authority: {error}"
                    ))
                })?;
            let checkpoint = remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: component.highest_protected_ordinal,
                total_committed_ordinals: component.total_committed_ordinals,
            };
            Ok((component, checkpoint))
        })
        .collect::<Result<Vec<_>, Status>>()?;
    let relation = journal
        .terminal_component_authority_relation(
            completed_terminal_component_count(intent.progress),
            &transitions,
        )
        .map_err(|error| {
            Status::failed_precondition(format!(
                "terminal checkpoint/sink-journal authority disagreement before media motion: {error}"
            ))
        })?;
    if relation == remanence_parity::TerminalComponentAuthorityRelation::Aligned {
        // The checkpoint journal is commit authority; SQLite may be one step
        // behind after a crash between its fsync and the projection update.
        // Repair that cache before selecting or moving to another component.
        index
            .project_terminal_finalization(TerminalFinalizationProjectionInput {
                tape_uuid: spec.tape_uuid,
                trigger: spec.trigger,
                operation_id: spec.operation_id,
                progress: intent.progress,
                edition_digest: intent.edition_digest,
                layout_digest: intent.layout.layout_digest,
                outcome: if intent.recovery_required {
                    TerminalFinalizationOutcome::RecoveryRequired
                } else {
                    TerminalFinalizationOutcome::InProgress
                },
                updated_at_utc: None,
            })
            .map_err(crate::status_from_state_error)?;
        return Ok(intent);
    }

    let previous = parity_progress_from_state(intent.progress);
    let component_index = previous.next_component_index().ok_or_else(|| {
        Status::failed_precondition(
            "sink journal is ahead of already-complete terminal checkpoint progress",
        )
    })?;
    let (component, sink_checkpoint) = transitions.get(component_index).ok_or_else(|| {
        Status::internal("terminal authority relation selected an impossible component")
    })?;
    journal
        .commit_terminal_component_transition(component, sink_checkpoint)
        .map_err(|error| {
            Status::failed_precondition(format!(
                "reconcile one-transition-ahead terminal sink journal: {error}"
            ))
        })?;
    let next = previous.successor().ok_or_else(|| {
        Status::internal("one-transition-ahead terminal authority has no successor")
    })?;
    let next_state = state_progress_from_parity(next);
    intent = checkpoint
        .advance_terminal_finalization(intent.progress, next_state)
        .map_err(|error| {
            Status::failed_precondition(format!(
                "reconcile terminal checkpoint progress from sink journal: {error}"
            ))
        })?;
    index
        .project_terminal_finalization(TerminalFinalizationProjectionInput {
            tape_uuid: spec.tape_uuid,
            trigger: spec.trigger,
            operation_id: spec.operation_id,
            progress: next_state,
            edition_digest: intent.edition_digest,
            layout_digest: intent.layout.layout_digest,
            outcome: TerminalFinalizationOutcome::InProgress,
            updated_at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    Ok(intent)
}

pub(super) fn reconcile_and_authorize_parity_resume(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    intent: remanence_state::TerminalFinalizationIntent,
    plan: &TerminalTripleWritePlan,
    journal: &mut FileTapeFileJournal,
) -> Result<remanence_state::TerminalFinalizationIntent, Status> {
    let intent = reconcile_terminal_component_host_authority(
        index, checkpoint, spec, intent, plan, journal,
    )?;
    authorize_terminal_intent_capacity(
        index,
        spec,
        selected,
        &intent,
        plan.edition.descriptor.counts,
    )?;
    Ok(intent)
}

pub(super) fn persist_terminal_recovery_required(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    spec: &TerminalFinalizeSpec,
) -> Result<remanence_state::TerminalFinalizationIntent, Status> {
    let intent = checkpoint
        .mark_terminal_recovery_required()
        .map_err(crate::status_from_state_error)?;
    index
        .project_terminal_finalization(TerminalFinalizationProjectionInput {
            tape_uuid: spec.tape_uuid,
            trigger: spec.trigger,
            operation_id: spec.operation_id,
            progress: intent.progress,
            edition_digest: intent.edition_digest,
            layout_digest: intent.layout.layout_digest,
            outcome: TerminalFinalizationOutcome::RecoveryRequired,
            updated_at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    Ok(intent)
}

pub(super) const fn terminal_reconciliation_outcome(
    _progress: remanence_state::TerminalFinalizationProgress,
    _evidence: TerminalComponentReconcileEvidence,
) -> TerminalFinalizationOutcome {
    // Reconciliation proves media facts; it does not supply the distinct,
    // audited operator decision required to accept reduced redundancy as a
    // terminal outcome. Until that decision exists, every non-repairable or
    // completion-unknown tail remains recovery-required.
    TerminalFinalizationOutcome::RecoveryRequired
}
