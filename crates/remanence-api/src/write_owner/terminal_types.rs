//! Terminal-finalization requests, plans, authority, and recovery helpers.

use remanence_state::authority::TapeAuthorityView;
use remanence_state::authority::TapeAuthorityWriter as _;
use std::path::Path;
use std::sync::Arc;

use remanence_parity::{
    TerminalComponentCommit, TerminalComponentReconcileEvidence, TerminalTailAuthority,
    TerminalTailProgress, TerminalTripleWritePlan,
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
    Busy { operation_id: Option<Uuid> },
    Accepted(ManualFinalizeTapeActorReply),
}

/// Terminal producers carry the process's typed authority capability. Historical
/// audit fixtures exist only in unit-test builds and cannot be selected by a
/// production constructor.
#[derive(Clone)]
pub(crate) enum TerminalAuthority {
    Typed(remanence_state::library_runtime::LibraryRuntime),
    #[cfg(test)]
    HistoricalFixture(FixtureTerminalAudit),
}
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct FixtureTerminalAudit {
    pub audit_dir: std::path::PathBuf,
    pub audit_fsync: bool,
    pub audit_append_lock: Arc<std::sync::Mutex<()>>,
}
impl TerminalAuthority {
    pub(crate) fn runtime(&self) -> Option<&remanence_state::library_runtime::LibraryRuntime> {
        match self {
            Self::Typed(runtime) => Some(runtime),
            #[cfg(test)]
            Self::HistoricalFixture(_) => None,
        }
    }
    #[cfg(test)]
    pub(crate) fn fixture(
        library: Option<remanence_state::library_runtime::LibraryRuntime>,
        dir: impl AsRef<Path>,
        fsync: bool,
        lock: &Arc<std::sync::Mutex<()>>,
    ) -> Self {
        library.map(Self::Typed).unwrap_or_else(|| {
            Self::HistoricalFixture(FixtureTerminalAudit {
                audit_dir: dir.as_ref().to_path_buf(),
                audit_fsync: fsync,
                audit_append_lock: lock.clone(),
            })
        })
    }
    #[cfg(test)]
    pub(crate) fn historical_fixture(&self) -> Option<&FixtureTerminalAudit> {
        match self {
            Self::HistoricalFixture(f) => Some(f),
            _ => None,
        }
    }
    pub(crate) fn from_api(state: &crate::ApiState) -> Result<Self, Status> {
        if let Some(runtime) = state.catalog.library_runtime() {
            return Ok(Self::Typed(runtime));
        }
        #[cfg(test)]
        {
            Ok(Self::fixture(
                None,
                state.audit_dir.as_path(),
                state.audit_fsync,
                &state.audit_append_lock,
            ))
        }
        #[cfg(not(test))]
        {
            Err(Status::failed_precondition(
                "terminal work requires shared typed authority",
            ))
        }
    }
}
impl From<&WriteOwnerConfig> for TerminalAuthority {
    fn from(cfg: &WriteOwnerConfig) -> Self {
        cfg.authority.clone()
    }
}
impl From<&TerminalWriteConfig> for TerminalAuthority {
    fn from(cfg: &TerminalWriteConfig) -> Self {
        cfg.authority.clone()
    }
}

/// No-motion preflight has source ownership and native storage, but no drive.
pub(crate) struct ManualFinalizePreflightConfig<'a> {
    pub(crate) authority: TerminalAuthority,
    pub(crate) mounted_endpoint: Option<&'a remanence_library::DriveKey>,
    pub(crate) checkpoint_journal_dir: &'a Path,
}
/// Persistence context shared by actor and standalone terminal writers.
#[derive(Clone)]
pub(crate) struct TerminalWriteConfig {
    pub authority: TerminalAuthority,
    pub io_memory: Arc<crate::io_memory::IoMemoryReservation>,
    pub mounted_endpoint: Option<remanence_library::DriveKey>,
    pub checkpoint_journal_dir: std::path::PathBuf,
}
pub(crate) trait TerminalConfig {
    fn terminal_config(&self) -> TerminalWriteConfig;
}
impl TerminalConfig for TerminalWriteConfig {
    fn terminal_config(&self) -> TerminalWriteConfig {
        self.clone()
    }
}
impl TerminalConfig for WriteOwnerConfig {
    fn terminal_config(&self) -> TerminalWriteConfig {
        TerminalWriteConfig {
            authority: TerminalAuthority::from(self),
            io_memory: self.io_memory.clone(),
            mounted_endpoint: self.mounted().map(|m| m.endpoint.clone()),
            checkpoint_journal_dir: self.checkpoint_journal_dir.clone(),
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
    pub(super) actor: AuditActor,
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
            actor: request.actor.clone(),
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
        Self {
            actor: AuditActor::System,
            tape_uuid: selected.tape_uuid,
            block_size: selected.block_size,
            pool_config: Some(pool_config.clone()),
            trigger,
            operation_id: None,
            manual: None,
        }
    }

    pub(super) fn resume(
        intent: &remanence_state::authority::AuthorityFinalization,
        block_size: u32,
        pool_config: &TapePoolConfig,
    ) -> Self {
        Self {
            actor: AuditActor::System,
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
    pub(super) final_record: Option<remanence_state::authority::AuthorityCommit>,
}

pub(super) struct TerminalTailCatalogAuthority<'a> {
    pub(super) checkpoint: &'a mut remanence_state::TapeAuthorityLease,
    pub(super) index: &'a mut CatalogIndex,
    pub(super) spec: &'a TerminalFinalizeSpec,
    pub(super) intent: remanence_state::authority::AuthorityFinalization,
    pub(super) tix_fault: Option<&'a crate::terminal_fault::TerminalFaultPlan>,
    pub(super) durable_projection_pending: bool,
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

    fn commit_after_barrier(&mut self, commit: &mut TerminalComponentCommit) -> Result<(), String> {
        self.durable_projection_pending = false;
        let expected = state_progress_from_parity(commit.previous_progress);
        let next = state_progress_from_parity(commit.next_progress);
        self.intent = self
            .checkpoint
            .advance_finalization(expected, next)
            .map_err(|error| {
                self.durable_projection_pending = error.is_checkpoint_append_durable();
                format!("advance terminal checkpoint progress: {error}")
            })?;
        self.durable_projection_pending = true;
        if let Some(fault) = self.tix_fault {
            fault.abort_component_if_matches(
                commit.component,
                crate::terminal_fault::TerminalFaultCut::BeforeSqliteProjection,
                Some(commit.observed_position),
            )?;
        }
        self.index
            .project_tape_log(&self.checkpoint.freeze().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        if let Some(fault) = self.tix_fault {
            fault.abort_component_if_matches(
                commit.component,
                crate::terminal_fault::TerminalFaultCut::AfterSqliteProjection,
                Some(commit.observed_position),
            )?;
        }
        self.durable_projection_pending = false;
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
    spec: &TerminalFinalizeSpec,
    intent: remanence_state::authority::AuthorityFinalization,
    plan: &TerminalTripleWritePlan,
    journal: &mut dyn remanence_parity::authority::StructuralAuthority,
) -> Result<remanence_state::authority::AuthorityFinalization, Status> {
    let transitions = plan
        .edition
        .descriptor
        .terminal_layout
        .components
        .iter()
        .map(|component| {
            let component = remanence_parity::terminal_component_publication(plan, *component)
                .map_err(|error| {
                    Status::failed_precondition(format!(
                        "build canonical terminal component authority: {error}"
                    ))
                })?;
            Ok(component)
        })
        .collect::<Result<Vec<_>, Status>>()?;
    journal
        .component_relation(completed_terminal_component_count(intent.progress), &transitions)
        .map_err(|error| {
            Status::failed_precondition(format!(
                "terminal checkpoint/sink-journal authority disagreement before media motion: {error}"
            ))
        })?;
    // Repair SQLite from the validated native component prefix before media motion.
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
    Ok(intent)
}

pub(super) fn reconcile_and_authorize_parity_resume(
    index: &mut CatalogIndex,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    intent: remanence_state::authority::AuthorityFinalization,
    plan: &TerminalTripleWritePlan,
    journal: &mut dyn remanence_parity::authority::StructuralAuthority,
) -> Result<remanence_state::authority::AuthorityFinalization, Status> {
    let intent = reconcile_terminal_component_host_authority(index, spec, intent, plan, journal)?;
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
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    spec: &TerminalFinalizeSpec,
) -> Result<remanence_state::authority::AuthorityFinalization, Status> {
    let intent = checkpoint
        .require_finalization_recovery()
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
