//! Drive/changer actor pool for Layer 5 read and write sessions.
//!
//! Phase 3b reserves individual drive bays for sessions while keeping
//! reconcile and robotics pool-exclusive.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, RwLock};
use std::time::{Duration as StdDuration, Instant};

use ciborium::value::Value as CborValue;
use remanence_format::error::FormatError;
use remanence_format::model::BodyLba;
use remanence_library::{
    classify_media_readiness_error_ref, BlockSize, BlockSource, ChangerHandle, DiscoveryReport,
    DriveHandle, DriveHandleSink, DriveHandleSource, DriveOpError, LoadError, MediaFamily,
    MediaReadiness, MediaReadinessPoll, MediaReadinessWaitEvent, MediaReadinessWaitOptions,
    ReadBatchOutcome, SpaceKind, SpaceResult, StaticAllowlist, TapeConfig, TapeIoError,
    TapePosition,
};
use remanence_parity::{
    checked_bounded_resume_summary, read_terminal_index_inventory_streamed,
    reconcile_terminal_prefix, reconcile_terminal_tail_next, recover_terminal_inventory_from_bot,
    scan_reconstruct_filemark_map, verify_terminal_index_full, BotStructuralRecoveryReason,
    BoundedResumeSummary, BoundedResumeWriterSeed, CloseReason, DriveHandleRawSink,
    DriveHandleRawSource, FileTapeFileJournal, FileTapeFileJournalCommittedSnapshot, FilemarkMap,
    ParityError, ParitySink, ParitySinkSessionState, PhysicalPositionHint, RawTapeSink,
    RawWriteOutcome, TapeFileEntry, TapeFileJournal, TapeFileKind, TerminalComponentCommit,
    TerminalComponentReconcileEvidence, TerminalInventoryOutcome, TerminalInventoryStreamEvent,
    TerminalPrefixPlan, TerminalPrefixReconcileEvidence, TerminalReplicaEvidence,
    TerminalReplicaFailureKind, TerminalTailAuthority, TerminalTailProgress,
    TerminalTailStepOutcome, TerminalTailWriteError, TerminalTripleWritePlan,
};
use remanence_state::{
    AlarmRecord, AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex,
    CleaningConfig, DriveHealthSnapshotInput, DriveHealthSnapshotRecord, FileAuditLog,
    ManualTerminalFinalizationAcceptanceInput, NativeObjectFileRecord, SourceLayer, TapeIoConfig,
    TapeIoFenceRecord, TapePoolConfig, TerminalFinalizationOutcome, TerminalFinalizationProjection,
    TerminalFinalizationProjectionInput,
};
use remanence_stream::StreamingError;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use crate::pool_write::{SelectedTape, WriteObjectToPoolRequest};
use crate::{
    load_tape_by_uuid, pb, status_from_state_error, timestamp_from_rfc3339, verify_tape_identity,
    PoolWriteError, SelectTapeError, TapeUuid, FINALIZE_TAPE_OPERATION_KIND,
};

pub(crate) const SPOOL_MAX_BYTES: u64 = crate::APPEND_SPOOL_MAX_BYTES;
const LOAD_READY_TIMEOUT: StdDuration = StdDuration::from_secs(9_000);
const LOAD_READY_POLL_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Session-independent coordinates used to position a newly minted read
/// session at a catalogued file boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadResumeTarget {
    pub(crate) tape_uuid: [u8; 16],
    pub(crate) object_id: String,
    pub(crate) file_id: String,
    pub(crate) file_boundary_byte_offset: u64,
    pub(crate) expected_position_lba: Option<u64>,
    pub(crate) prior_daemon_epoch: Option<u64>,
}

/// Robotics work to perform after the owner opens and refreshes the library.
pub(crate) enum RoboticsAction {
    Refresh,
    Move {
        src: u16,
        dst: u16,
    },
    Load {
        slot: u16,
        bay: u16,
        wait_ready: bool,
    },
    Unload {
        bay: u16,
        destination: Option<u16>,
    },
    Clean {
        drive_uuid: Vec<u8>,
        trigger: String,
    },
}

pub(crate) enum ChangerCommand {
    Move {
        src: u16,
        dst: u16,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    #[expect(dead_code, reason = "Phase 3a command shape includes explicit refresh")]
    Refresh {
        reply: oneshot::Sender<Result<(), Status>>,
    },
    Reconcile {
        tape_uuid: [u8; 16],
        handle: crate::operations::OperationHandle,
    },
    Robotics {
        library_serial: String,
        action: RoboticsAction,
        handle: crate::operations::OperationHandle,
    },
}

pub(crate) enum DriveCommand {
    WaitReady {
        operation_id: Uuid,
        family: MediaFamily,
        options: MediaReadinessWaitOptions,
        handle: crate::operations::OperationHandle,
        reservation: DriveReservation,
    },
    OpenWrite {
        pool_cfg: TapePoolConfig,
        selected: SelectedTape,
        target_kind: pb::write_session::TargetKind,
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    OpenRead {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        resume_target: Option<ReadResumeTarget>,
        daemon_epoch: u64,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    TapeInventory {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_serial: Option<String>,
        stream_tx: mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    VerifyTapeIndex {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::TapeIndexVerification, Status>>,
    },
    FinalizeTape {
        request: ManualFinalizeTapeActorRequest,
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<ManualFinalizeTapeActorReply, Status>>,
    },
    Unload {
        reply: oneshot::Sender<Result<StdDuration, Status>>,
    },
    PollHealth {
        drive_uuid: Vec<u8>,
        trigger: &'static str,
        session_id: Option<Uuid>,
        tape_uuid: Option<[u8; 16]>,
        reply: oneshot::Sender<Result<DriveHealthSnapshotRecord, Status>>,
    },
    Heartbeat {
        drive_uuid: Vec<u8>,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    AppendFinish {
        session_id: Uuid,
        source: crate::WriteObjectSource,
        archive_path: PathBuf,
        caller_object_id: String,
        expected_content_sha256: Option<[u8; 32]>,
        expected_object_id: Option<[u8; 16]>,
        input_kind: crate::WriteObjectInputKind,
        live_write_counter: Option<Arc<crate::DriveByteCounters>>,
        reply: oneshot::Sender<Result<AppendFinishOutcome, Status>>,
    },
    Checkpoint {
        session_id: Uuid,
        trigger: CheckpointTrigger,
        expected_batch_id: Option<Uuid>,
        reply: Option<oneshot::Sender<Result<CheckpointActorReply, Status>>>,
    },
    TimerIdleClose {
        session_id: Uuid,
        checkpoint_batch_id: Uuid,
    },
    Close {
        session_id: Uuid,
        reply: oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    },
    Abort {
        session_id: Uuid,
        /// The caller's stated reason, when it gave one. Absent is an abort
        /// with no explanation -- common from a client that is itself dying --
        /// and is recorded as the absence of the audit key, not as "".
        reason: Option<String>,
        reply: oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    },
    Get {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    ReadFile {
        session_id: Uuid,
        object_id: String,
        file_id: Vec<u8>,
        stream_chunk_bytes: u32,
        chunk_tx: crate::read_core::ReadStreamSender,
    },
    ReadObjectRange {
        session_id: Uuid,
        object_id: String,
        file_id: String,
        start_byte: u64,
        end_byte: u64,
        stream_chunk_bytes: u32,
        chunk_tx: crate::read_core::ReadStreamSender,
    },
    CloseRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    GetRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
}

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
struct TerminalFinalizeAuditConfig<'a> {
    audit_dir: &'a Path,
    audit_fsync: bool,
    audit_append_lock: &'a Arc<std::sync::Mutex<()>>,
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

struct ManualFinalizeTapeMountRequest {
    request: ManualFinalizeTapeActorRequest,
    needs_drive_load: bool,
    library_serial: String,
    barcode: Option<String>,
    source_slot: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
}

#[derive(Clone, Debug)]
struct TerminalFinalizeSpec {
    tape_uuid: TapeUuid,
    block_size: u32,
    pool_config: Option<TapePoolConfig>,
    trigger: remanence_state::TerminalFinalizationTrigger,
    operation_id: Option<Uuid>,
    manual: Option<remanence_state::ManualTerminalFinalizationIdentity>,
}

impl TerminalFinalizeSpec {
    fn operator(request: &ManualFinalizeTapeActorRequest) -> Self {
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

    fn automatic(
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

    fn resume(
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
struct TerminalFinalizeResult {
    projection: TerminalFinalizationProjection,
    final_record: Option<remanence_state::CheckpointJournalRecord>,
}

struct TerminalTailCatalogAuthority<'a> {
    checkpoint: &'a mut remanence_state::FileCheckpointJournalLease,
    parity_journal: Option<&'a mut FileTapeFileJournal>,
    index: &'a mut CatalogIndex,
    spec: &'a TerminalFinalizeSpec,
    intent: remanence_state::TerminalFinalizationIntent,
    tix_fault: Option<&'a crate::terminal_fault::TerminalFaultPlan>,
    reconciliation: Option<(
        TerminalTailProgress,
        remanence_parity::TerminalTailComponentPlan,
        TerminalComponentReconcileEvidence,
    )>,
}

impl TerminalTailCatalogAuthority<'_> {
    fn set_reconciliation(
        &mut self,
        progress: TerminalTailProgress,
        component: remanence_parity::TerminalTailComponentPlan,
        evidence: TerminalComponentReconcileEvidence,
    ) {
        self.reconciliation = Some((progress, component, evidence));
    }

    fn projection_input(
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

const fn parity_progress_from_state(
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

const fn state_progress_from_parity(
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

const fn completed_terminal_component_count(
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

fn reconcile_terminal_component_host_authority(
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

fn reconcile_and_authorize_parity_resume(
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

fn persist_terminal_recovery_required(
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

const fn terminal_reconciliation_outcome(
    _progress: remanence_state::TerminalFinalizationProgress,
    _evidence: TerminalComponentReconcileEvidence,
) -> TerminalFinalizationOutcome {
    // Reconciliation proves media facts; it does not supply the distinct,
    // audited operator decision required to accept reduced redundancy as a
    // terminal outcome. Until that decision exists, every non-repairable or
    // completion-unknown tail remains recovery-required.
    TerminalFinalizationOutcome::RecoveryRequired
}

#[derive(Debug)]
pub(crate) struct AppendFinishOutcome {
    pub(crate) record: pb::ObjectRecord,
    pub(crate) replay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointTrigger {
    Explicit,
    Timer,
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct CheckpointActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) committed_objects: Vec<pb::ObjectRecord>,
}

fn send_checkpoint_actor_reply(
    reply: oneshot::Sender<Result<CheckpointActorReply, Status>>,
    session: pb::WriteSession,
    committed_receipts: &mut Vec<pb::ObjectRecord>,
) {
    let committed_objects = std::mem::take(committed_receipts);
    if let Err(Ok(unsent)) = reply.send(Ok(CheckpointActorReply {
        session,
        committed_objects,
    })) {
        *committed_receipts = unsent.committed_objects;
    }
}

/// Retain a catalog-replayed object until an explicit checkpoint can claim its durable copy.
///
/// A replay has no pending checkpoint batch because its copy was committed by an earlier
/// session. Append still reports that durable record to the current caller, whose batch contract
/// releases it only from `CheckpointSession`; coalescing by object id also prevents duplicate
/// receipts when the same replay arrives before that checkpoint.
fn retain_replayed_committed_receipt(
    committed_receipts: &mut Vec<pb::ObjectRecord>,
    record: &pb::ObjectRecord,
) {
    if !committed_receipts
        .iter()
        .any(|committed| committed.object_id == record.object_id)
    {
        committed_receipts.push(record.clone());
    }
}

#[derive(Debug)]
pub(crate) struct CloseWriteActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) diagnostics: CloseWriteActorDiagnostics,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CloseWriteActorDiagnostics {
    /// Synchronous object-closing filemark time accumulated by append calls.
    pub(crate) filemark_write_drain: StdDuration,
    /// Catalog/journal commit time accumulated after those filemarks.
    pub(crate) catalog_journal_fsync: StdDuration,
    /// Close-time health snapshot and projection work.
    pub(crate) drive_snapshot: StdDuration,
    /// Always zero for lazy session close; later dismount diagnostics own rewind time.
    pub(crate) rewind: StdDuration,
    /// Always zero for lazy session close; later dismount diagnostics own SSC UNLOAD time.
    pub(crate) ssc_unload: StdDuration,
    /// SessionClosed audit append/fsync and SQLite projection time.
    pub(crate) session_audit_projection: StdDuration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MountedSession {
    pub bay: u16,
    pub library_serial: String,
    pub barcode: Option<String>,
    pub home_slot: Option<u16>,
    pub tape_uuid: TapeUuid,
    pub drive_uuid: Option<Vec<u8>>,
}

/// A library cartridge intentionally left seated after its session closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeatedCartridge {
    pub(crate) bay: u16,
    pub(crate) library_serial: String,
    pub(crate) barcode: Option<String>,
    pub(crate) home_slot: u16,
    pub(crate) tape_uuid: Option<TapeUuid>,
    pub(crate) prior_session_id: Option<Uuid>,
}

/// Generation-tagged idle record used to invalidate stale timeout tasks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParkedCartridge {
    pub(crate) seated: SeatedCartridge,
    generation: u64,
}

#[derive(Default)]
struct ParkedState {
    next_generation: u64,
    by_bay: HashMap<u16, ParkedCartridge>,
}

/// Shared actor/pool lifecycle maps used by timer-driven close-and-park.
#[derive(Clone, Default)]
pub(crate) struct DrivePoolLifecycle {
    sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    parked: Arc<Mutex<ParkedState>>,
    timer_park_tx: Option<mpsc::UnboundedSender<ParkedCartridge>>,
}

impl DrivePoolLifecycle {
    pub(crate) fn with_timer_park_sender(
        timer_park_tx: mpsc::UnboundedSender<ParkedCartridge>,
    ) -> Self {
        Self {
            timer_park_tx: Some(timer_park_tx),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct DrivePool {
    changer_tx: mpsc::Sender<ChangerCommand>,
    drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>,
    reservations: Arc<HashMap<u16, AtomicBool>>,
    sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    tape_reservations: Arc<Mutex<HashSet<TapeUuid>>>,
    parked: Arc<Mutex<ParkedState>>,
    shutting_down: Arc<AtomicBool>,
}

impl DrivePool {
    #[cfg(test)]
    pub(crate) fn new(
        changer_tx: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
    ) -> Self {
        Self::new_with_lifecycle(
            changer_tx,
            drives,
            reservations,
            DrivePoolLifecycle::default(),
        )
    }

    pub(crate) fn new_with_lifecycle(
        changer_tx: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
        lifecycle: DrivePoolLifecycle,
    ) -> Self {
        Self {
            changer_tx,
            drives: Arc::new(drives),
            reservations,
            sessions: lifecycle.sessions,
            tape_reservations: Arc::new(Mutex::new(HashSet::new())),
            parked: lifecycle.parked,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn changer_tx(&self) -> mpsc::Sender<ChangerCommand> {
        self.changer_tx.clone()
    }

    pub(crate) fn drive_tx(&self, bay: u16) -> Result<mpsc::Sender<DriveCommand>, Status> {
        self.drives
            .get(&bay)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("drive bay 0x{bay:04x} not available")))
    }

    #[cfg(test)]
    pub(crate) fn reserve_free_drive(&self) -> Result<u16, Status> {
        let mut bays = self.reservations.keys().copied().collect::<Vec<_>>();
        bays.sort_unstable();
        bays.into_iter()
            .find(|bay| {
                self.reservations.get(bay).is_some_and(|reservation| {
                    reservation
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                })
            })
            .ok_or_else(|| Status::failed_precondition("all drives are busy"))
    }

    pub(crate) fn reserve_drive(&self, bay: u16) -> Result<DriveReservation, Status> {
        if self.is_shutting_down() {
            return Err(Status::unavailable("drive pool is shutting down"));
        }
        self.reserve_drive_inner(bay)
    }

    pub(crate) fn reserve_drive_for_shutdown(&self, bay: u16) -> Result<DriveReservation, Status> {
        self.reserve_drive_inner(bay)
    }

    fn reserve_drive_inner(&self, bay: u16) -> Result<DriveReservation, Status> {
        let reservation = self
            .reservations
            .get(&bay)
            .ok_or_else(|| Status::not_found(format!("drive bay 0x{bay:04x} not available")))?;
        reservation
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| Status::failed_precondition(format!("drive bay 0x{bay:04x} is busy")))?;
        Ok(DriveReservation {
            bay,
            reservations: self.reservations.clone(),
            armed: true,
        })
    }

    pub(crate) fn release(&self, bay: u16) {
        if let Some(reservation) = self.reservations.get(&bay) {
            reservation.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn reserve_all_exclusive(&self) -> Result<(), Status> {
        if self.is_shutting_down() {
            return Err(Status::unavailable("drive pool is shutting down"));
        }
        let mut acquired = Vec::new();
        let mut bays = self.reservations.keys().copied().collect::<Vec<_>>();
        bays.sort_unstable();
        for bay in bays {
            let Some(reservation) = self.reservations.get(&bay) else {
                continue;
            };
            if reservation
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                acquired.push(bay);
            } else {
                for acquired_bay in acquired {
                    self.release(acquired_bay);
                }
                return Err(Status::failed_precondition("drives are busy"));
            }
        }
        Ok(())
    }

    pub(crate) fn release_all(&self) {
        release_all_reservations(&self.reservations);
    }

    pub(crate) fn busy_bays(&self) -> HashSet<u16> {
        self.reservations
            .iter()
            .filter(|(_, reservation)| reservation.load(Ordering::SeqCst))
            .map(|(bay, _)| *bay)
            .collect()
    }

    /// Snapshot sessions by their enforcement key for advisory status only.
    ///
    /// The reservation atomics remain the sole authority for admission. This
    /// projection may race with an open/close and must never gate tape I/O.
    pub(crate) fn sessions_by_bay(&self) -> HashMap<u16, (Uuid, MountedSession)> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .map(|(session_id, mounted)| (mounted.bay, (*session_id, mounted.clone())))
            .collect()
    }

    pub(crate) fn mounted_tape_uuids(&self) -> HashSet<TapeUuid> {
        let mut in_use = self
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .map(|mounted| mounted.tape_uuid)
            .collect::<HashSet<_>>();
        in_use.extend(
            self.tape_reservations
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .iter()
                .copied(),
        );
        in_use
    }

    pub(crate) fn reserve_tape(&self, tape_uuid: TapeUuid) -> Result<TapeReservation, Status> {
        self.reserve_tape_with_after_insert(tape_uuid, |_| {})
    }

    fn reserve_tape_with_after_insert(
        &self,
        tape_uuid: TapeUuid,
        after_insert: impl FnOnce(&HashSet<TapeUuid>),
    ) -> Result<TapeReservation, Status> {
        let sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        if sessions
            .values()
            .any(|mounted| mounted.tape_uuid == tape_uuid)
        {
            return Err(Status::failed_precondition("tape is already mounted"));
        }
        let mut reservations = self
            .tape_reservations
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !reservations.insert(tape_uuid) {
            return Err(Status::failed_precondition("tape is already mounted"));
        }
        after_insert(&reservations);
        // Keep the sessions guard through reservation insertion. Otherwise a
        // concurrent opener can publish a mounted session after our check but
        // before this exact-tape owner becomes visible.
        drop(sessions);
        Ok(TapeReservation {
            tape_uuid,
            reservations: self.tape_reservations.clone(),
        })
    }

    pub(crate) fn record_session(&self, session_id: Uuid, mounted: MountedSession) {
        self.parked
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .by_bay
            .remove(&mounted.bay);
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(session_id, mounted);
    }

    pub(crate) fn session(&self, session_id: Uuid) -> Result<MountedSession, Status> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("session not found"))
    }

    pub(crate) fn forget_session(&self, session_id: Uuid) {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&session_id);
    }

    /// Convert a completed session reservation into an idle seated cartridge.
    /// The bay remains reserved until all bookkeeping is published.
    pub(crate) fn finish_session(
        &self,
        session_id: Uuid,
        mounted: MountedSession,
    ) -> Option<ParkedCartridge> {
        let bay = mounted.bay;
        self.forget_session(session_id);
        let parked = mounted.home_slot.map(|home_slot| {
            self.park_cartridge(SeatedCartridge {
                bay: mounted.bay,
                library_serial: mounted.library_serial,
                barcode: mounted.barcode,
                home_slot,
                tape_uuid: Some(mounted.tape_uuid),
                prior_session_id: Some(session_id),
            })
        });
        self.release(bay);
        parked
    }

    /// Register a cartridge found seated at startup or during reconciliation.
    pub(crate) fn park_cartridge(&self, seated: SeatedCartridge) -> ParkedCartridge {
        let mut state = self.parked.lock().unwrap_or_else(|err| err.into_inner());
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let parked = ParkedCartridge {
            seated,
            generation: state.next_generation,
        };
        state.by_bay.insert(parked.seated.bay, parked.clone());
        parked
    }

    pub(crate) fn parked_at(&self, bay: u16) -> Option<ParkedCartridge> {
        self.parked
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .by_bay
            .get(&bay)
            .cloned()
    }

    pub(crate) fn parked_is_current(&self, parked: &ParkedCartridge) -> bool {
        self.parked_at(parked.seated.bay)
            .is_some_and(|current| current.generation == parked.generation)
    }

    pub(crate) fn forget_parked(&self, parked: &ParkedCartridge) {
        let mut state = self.parked.lock().unwrap_or_else(|err| err.into_inner());
        if state
            .by_bay
            .get(&parked.seated.bay)
            .is_some_and(|current| current.generation == parked.generation)
        {
            state.by_bay.remove(&parked.seated.bay);
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub(crate) async fn poll_drive_health(
        &self,
        bay: u16,
        drive_uuid: Vec<u8>,
    ) -> Result<DriveHealthSnapshotRecord, Status> {
        let tx = self.drive_tx(bay)?;
        let (reply, rx) = oneshot::channel();
        tx.send(DriveCommand::PollHealth {
            drive_uuid,
            trigger: "manual",
            session_id: None,
            tape_uuid: None,
            reply,
        })
        .await
        .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
        rx.await
            .map_err(|_| Status::unavailable("drive actor stopped"))?
    }

    pub(crate) fn heartbeat_drive(&self, bay: u16, drive_uuid: Vec<u8>) -> Result<(), Status> {
        let tx = self.drive_tx(bay)?;
        let (reply, rx) = oneshot::channel();
        tx.blocking_send(DriveCommand::Heartbeat { drive_uuid, reply })
            .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
        rx.blocking_recv()
            .map_err(|_| Status::unavailable("drive actor stopped"))?
    }
}

#[derive(Debug)]
pub(crate) struct DriveReservation {
    bay: u16,
    reservations: Arc<HashMap<u16, AtomicBool>>,
    armed: bool,
}

impl DriveReservation {
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for DriveReservation {
    fn drop(&mut self) {
        if self.armed {
            if let Some(reservation) = self.reservations.get(&self.bay) {
                reservation.store(false, Ordering::SeqCst);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct TapeReservation {
    tape_uuid: TapeUuid,
    reservations: Arc<Mutex<HashSet<TapeUuid>>>,
}

impl Drop for TapeReservation {
    fn drop(&mut self) {
        self.reservations
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&self.tape_uuid);
    }
}

#[derive(Clone)]
pub(crate) struct WriteOwnerConfig {
    pub index_path: PathBuf,
    pub report: DiscoveryReport,
    pub policy: StaticAllowlist,
    pub audit_dir: PathBuf,
    pub audit_fsync: bool,
    pub audit_append_lock: Arc<std::sync::Mutex<()>>,
    pub reservations: Arc<HashMap<u16, AtomicBool>>,
    pub default_library_serial: Option<String>,
    pub library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
    pub snapshot_miss_alarm: u32,
    pub managed_library_serials: Arc<HashSet<String>>,
    pub cleaning: CleaningConfig,
    pub tape_io: TapeIoConfig,
    pub io_memory: Arc<crate::io_memory::IoMemoryReservation>,
    /// Cross-drive claims that make replay-key and canonical-UUID admission
    /// atomic through checkpoint projection.
    pub write_admissions: WriteAdmissionCoordinator,
    pub checkpoint_journal_dir: PathBuf,
    pub checkpoint_max_bytes: u64,
    pub checkpoint_max_objects: u64,
    pub checkpoint_max_age_seconds: u64,
    pub session_idle_seconds: u64,
    pub lifecycle: Option<DrivePoolLifecycle>,
    /// Durable calibration-control store for the wrap-map read
    /// ordering lifecycle (design-read-ordering.md §6.5). The drive
    /// actors run the load harvest against it at session open when
    /// the open freshly mounted the cartridge.
    pub calibration_store: remanence_state::CalibrationControlStore,
}

pub(crate) struct ExclusiveGuard {
    reservations: Arc<HashMap<u16, AtomicBool>>,
}

impl ExclusiveGuard {
    pub(crate) fn from_reserved(reservations: Arc<HashMap<u16, AtomicBool>>) -> Self {
        Self { reservations }
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        release_all_reservations(&self.reservations);
    }
}

pub(crate) struct Spool {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
    cap: u64,
    keep: bool,
}

impl Spool {
    pub(crate) fn create(dir: &Path, cap: u64) -> std::io::Result<Self> {
        let path = dir.join(format!("spool-{}.bin", Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| {
                std::io::Error::new(err.kind(), format!("open {}: {err}", path.display()))
            })?;
        Ok(Self {
            file,
            path,
            written: 0,
            cap,
            keep: false,
        })
    }

    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "spool size overflows u64")
            })?;
        if next > self.cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("spool size cap exceeded at {}", self.path.display()),
            ));
        }
        self.file.write_all(bytes).map_err(|err| {
            std::io::Error::new(err.kind(), format!("write {}: {err}", self.path.display()))
        })?;
        self.written = next;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush().map_err(|err| {
            std::io::Error::new(err.kind(), format!("flush {}: {err}", self.path.display()))
        })?;
        self.keep = true;
        Ok(self.path.clone())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn spawn_changer_actor(
    mut changer: ChangerHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<ChangerCommand> {
    let (tx, rx) = mpsc::channel::<ChangerCommand>(16);
    std::thread::Builder::new()
        .name("rem-changer-actor".to_string())
        .spawn(move || changer_loop(&mut changer, cfg, rx))
        .expect("spawn changer actor thread");
    tx
}

pub(crate) fn spawn_drive_actor(
    bay: u16,
    mut drive: DriveHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<DriveCommand> {
    let (tx, rx) = mpsc::channel::<DriveCommand>(16);
    let actor_tx = tx.clone();
    std::thread::Builder::new()
        .name(format!("rem-drive-actor-{bay:04x}"))
        .spawn(move || drive_loop(bay, &mut drive, cfg, actor_tx, rx))
        .expect("spawn drive actor thread");
    tx
}

fn changer_loop(
    changer: &mut ChangerHandle,
    cfg: WriteOwnerConfig,
    mut rx: mpsc::Receiver<ChangerCommand>,
) {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_changer_commands(
                rx,
                format!("open catalog index: {err}"),
                cfg.reservations.clone(),
            );
            return;
        }
    };
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::Move { src, dst, reply } => {
                let result = changer
                    .move_medium(src, dst, &cfg.policy)
                    .map_err(|err| Status::internal(format!("move medium: {err}")));
                if result.is_ok() {
                    match observe_refreshed_library(&mut index, &cfg, changer.library()) {
                        Ok(()) => clear_library_snapshot_persist_alarm(
                            &mut index,
                            &cfg,
                            changer.library().serial.as_str(),
                        ),
                        Err(err) => record_library_observation_failure(
                            &mut index,
                            &cfg,
                            changer.library(),
                            err.message(),
                        ),
                    }
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Refresh { reply } => {
                let result = changer
                    .refresh()
                    .map_err(|err| Status::internal(format!("refresh inventory: {err}")))
                    .and_then(|()| observe_refreshed_library(&mut index, &cfg, changer.library()));
                if result.is_ok() {
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Reconcile { tape_uuid, handle } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved(cfg.reservations.clone());
                handle_reconcile(&mut index, &cfg, tape_uuid, handle);
                refresh_actor_changer(changer, &cfg);
            }
            ChangerCommand::Robotics {
                library_serial,
                action,
                handle,
            } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved(cfg.reservations.clone());
                handle_robotics(&mut index, &cfg, library_serial, action, handle);
                refresh_actor_changer(changer, &cfg);
            }
        }
    }
}

fn refresh_actor_changer(changer: &mut ChangerHandle, cfg: &WriteOwnerConfig) {
    if changer.refresh().is_ok() {
        match CatalogIndex::open(cfg.index_path.as_path()) {
            Ok(mut index) => {
                if let Err(err) = observe_refreshed_library(&mut index, cfg, changer.library()) {
                    tracing::warn!("failed to observe refreshed drive catalog: {err}");
                }
            }
            Err(err) => tracing::warn!("failed to open catalog for refreshed drive catalog: {err}"),
        }
        publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
    }
}

fn drain_failed_changer_commands(
    mut rx: mpsc::Receiver<ChangerCommand>,
    message: String,
    reservations: Arc<HashMap<u16, AtomicBool>>,
) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::Move { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Refresh { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Reconcile { handle, .. } | ChangerCommand::Robotics { handle, .. } => {
                handle.publish_failed(message.as_str(), &[("phase", "catalog")]);
                release_all_reservations(&reservations);
            }
        }
    }
}

fn release_all_reservations(reservations: &HashMap<u16, AtomicBool>) {
    for reservation in reservations.values() {
        reservation.store(false, Ordering::SeqCst);
    }
}

fn drive_loop(
    bay: u16,
    drive: &mut DriveHandle,
    cfg: WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    mut rx: mpsc::Receiver<DriveCommand>,
) {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_drive_commands(rx, format!("open catalog index: {err}"));
            return;
        }
    };
    let mut snapshot_misses = 0u32;
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::WaitReady {
                operation_id,
                family,
                options,
                handle,
                reservation: _reservation,
            } => handle_drive_wait_ready(
                bay,
                &mut index,
                drive,
                operation_id,
                family,
                options,
                &handle,
            ),
            DriveCommand::OpenWrite {
                pool_cfg,
                selected,
                target_kind,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                reply,
            } => handle_drive_open_write(
                bay,
                &mut index,
                &cfg,
                actor_tx.clone(),
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenWriteActorRequest {
                    pool_cfg,
                    selected,
                    target_kind,
                    needs_drive_load,
                    library_serial,
                    barcode,
                    source_slot,
                    drive_uuid,
                    drive_serial,
                    reply,
                },
            ),
            DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                resume_target,
                daemon_epoch,
                reply,
            } => handle_drive_open_read(
                bay,
                &mut index,
                &cfg,
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenReadActorRequest {
                    tape_uuid,
                    needs_drive_load,
                    library_serial,
                    barcode,
                    source_slot,
                    drive_uuid,
                    drive_serial,
                    resume_target,
                    daemon_epoch,
                    reply,
                },
            ),
            DriveCommand::TapeInventory {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_serial,
                stream_tx,
                reply,
            } => {
                let result = handle_drive_tape_inventory(
                    bay,
                    &mut index,
                    drive,
                    tape_uuid,
                    needs_drive_load,
                    library_serial.as_str(),
                    barcode.as_deref(),
                    source_slot,
                    drive_serial.as_deref(),
                    &stream_tx,
                );
                if let Err(error) = &result {
                    let _ = stream_tx
                        .blocking_send(Err(Status::new(error.code(), error.message().to_string())));
                }
                let _ = reply.send(result);
            }
            DriveCommand::VerifyTapeIndex {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_serial,
                reply,
            } => {
                let result = handle_drive_verify_tape_index(
                    bay,
                    &mut index,
                    drive,
                    tape_uuid,
                    needs_drive_load,
                    library_serial.as_str(),
                    barcode.as_deref(),
                    source_slot,
                    drive_serial.as_deref(),
                );
                let _ = reply.send(result);
            }
            DriveCommand::FinalizeTape {
                request,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                reply,
            } => {
                let result = handle_drive_finalize_tape(
                    bay,
                    &mut index,
                    &cfg,
                    drive,
                    &mut snapshot_misses,
                    ManualFinalizeTapeMountRequest {
                        request,
                        needs_drive_load,
                        library_serial,
                        barcode,
                        source_slot,
                        drive_uuid,
                        drive_serial,
                    },
                );
                let _ = reply.send(result);
            }
            DriveCommand::Unload { reply } => {
                let started = Instant::now();
                let result = drive
                    .unload()
                    .map(|()| started.elapsed())
                    .map_err(|err| Status::internal(format!("unload drive: {err}")));
                let _ = reply.send(result);
            }
            DriveCommand::PollHealth {
                drive_uuid,
                trigger,
                session_id,
                tape_uuid,
                reply,
            } => {
                let result = collect_drive_health_snapshot(
                    &mut index,
                    &cfg,
                    drive,
                    DriveSnapshotRequest {
                        drive_uuid,
                        trigger,
                        session_id,
                        tape_uuid,
                    },
                );
                let _ = reply.send(result);
            }
            DriveCommand::Heartbeat { drive_uuid, reply } => {
                let result = drive
                    .test_unit_ready()
                    .map_err(|err| Status::unavailable(format!("drive heartbeat: {err}")))
                    .and_then(|_| {
                        index
                            .touch_drive_last_seen(&drive_uuid)
                            .map(|_| ())
                            .map_err(status_from_state_error)
                    });
                let _ = reply.send(result);
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::failed_precondition("no active write session")));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::not_found("no active write session")));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active write session")));
            }
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active write session")));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::not_found("no active read session")));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active read session")));
            }
        }
    }
}

fn drain_failed_drive_commands(mut rx: mpsc::Receiver<DriveCommand>, message: String) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed(&message, &[("phase", "drive_actor")]);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::TapeInventory { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::VerifyTapeIndex { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::FinalizeTape { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::internal(message.clone())));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::internal(message.clone())));
            }
        }
    }
}

struct OpenWriteActorRequest {
    pool_cfg: TapePoolConfig,
    selected: SelectedTape,
    target_kind: pb::write_session::TargetKind,
    needs_drive_load: bool,
    library_serial: String,
    barcode: Option<String>,
    source_slot: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
}

struct OpenReadActorRequest {
    tape_uuid: [u8; 16],
    needs_drive_load: bool,
    library_serial: String,
    barcode: Option<String>,
    source_slot: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    resume_target: Option<ReadResumeTarget>,
    daemon_epoch: u64,
    reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
}

struct SessionAuditInput {
    session_id: Uuid,
    session_kind: &'static str,
    event: AuditEvent,
    tape_uuid: Option<[u8; 16]>,
    library_serial: Option<String>,
    drive_bay: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    /// Only ever set for an aborted write session, and only when the caller
    /// supplied one.
    abort_reason: Option<String>,
}

fn record_session_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    input: SessionAuditInput,
) -> Result<(), Status> {
    let _guard = cfg
        .audit_append_lock
        .lock()
        .map_err(|_| Status::internal("session audit append lock poisoned"))?;
    std::fs::create_dir_all(cfg.audit_dir.as_path()).map_err(|err| {
        Status::internal(format!(
            "create session audit directory {}: {err}",
            cfg.audit_dir.display()
        ))
    })?;
    let mut detail = BTreeMap::new();
    detail.insert(
        "session_kind".to_string(),
        CborValue::Text(input.session_kind.to_string()),
    );
    if let Some(tape_uuid) = input.tape_uuid {
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
    }
    if let Some(library_serial) = input.library_serial {
        detail.insert(
            "library_serial".to_string(),
            CborValue::Text(library_serial),
        );
    }
    if let Some(drive_bay) = input.drive_bay {
        detail.insert(
            "drive_bay".to_string(),
            CborValue::Integer(u64::from(drive_bay).into()),
        );
    }
    if let Some(drive_uuid) = input.drive_uuid {
        detail.insert("drive_uuid".to_string(), CborValue::Bytes(drive_uuid));
    }
    if let Some(drive_serial) = input.drive_serial {
        detail.insert("drive_serial".to_string(), CborValue::Text(drive_serial));
    }
    if let Some(abort_reason) = input.abort_reason {
        detail.insert("abort_reason".to_string(), CborValue::Text(abort_reason));
    }
    let mut audit = FileAuditLog::open(cfg.audit_dir.as_path(), cfg.audit_fsync)
        .map_err(crate::status_from_state_error)?;
    let (_, record) = audit
        .append_and_return_record(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(input.session_id),
            idempotency_key: None,
            event: input.event,
            subject: AuditSubject {
                kind: input.session_kind.to_string(),
                id: Some(input.session_id.to_string()),
            },
            detail,
        })
        .map_err(crate::status_from_state_error)?;
    index
        .project_audit_record(&record)
        .map_err(crate::status_from_state_error)
}

struct DriveSnapshotRequest {
    drive_uuid: Vec<u8>,
    trigger: &'static str,
    session_id: Option<Uuid>,
    tape_uuid: Option<[u8; 16]>,
}

fn collect_drive_health_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    request: DriveSnapshotRequest,
) -> Result<DriveHealthSnapshotRecord, Status> {
    let alerts = drive
        .read_tape_alerts()
        .map_err(|err| Status::unavailable(format!("read TapeAlert page: {err}")))?;
    let counters = drive
        .read_error_counters()
        .map_err(|err| Status::unavailable(format!("read error counter pages: {err}")))?;
    let tape_uuid_text = request
        .tape_uuid
        .map(|uuid| Uuid::from_bytes(uuid).to_string())
        .unwrap_or_default();
    let raw_pages = format!(
        "{{\"tape_uuid\":\"{}\",\"tape_alert\":true,\"write_error_counter\":true,\"read_error_counter\":true}}",
        tape_uuid_text
    );
    let snapshot = index
        .record_drive_health_snapshot(DriveHealthSnapshotInput {
            drive_uuid: request.drive_uuid.clone(),
            trigger: request.trigger.to_string(),
            session_id: request.session_id.map(|uuid| uuid.to_string()),
            tape_alert_flags: Some(tape_alert_flags_json(alerts.active())),
            write_errors_corrected: counters.write_errors_corrected.and_then(u64_to_i64),
            write_errors_uncorrected: counters.write_errors_uncorrected.and_then(u64_to_i64),
            read_errors_corrected: counters.read_errors_corrected.and_then(u64_to_i64),
            read_errors_uncorrected: counters.read_errors_uncorrected.and_then(u64_to_i64),
            raw_pages: Some(raw_pages),
            at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    if alerts.is_set(20) || alerts.is_set(21) {
        let due = if alerts.is_set(20) { "now" } else { "periodic" };
        index
            .observe_managed_drive_cleaning_due(&request.drive_uuid, due)
            .map_err(crate::status_from_state_error)?;
    } else {
        index
            .touch_drive_last_seen(&request.drive_uuid)
            .map_err(crate::status_from_state_error)?;
    }
    append_drive_health_evidence(index, cfg, &snapshot)?;
    Ok(snapshot)
}

/// Append the durable evidence twin for a just-committed health snapshot and
/// project that exact record through the same replay funnel used at rebuild.
fn append_drive_health_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    snapshot: &DriveHealthSnapshotRecord,
) -> Result<(), Status> {
    let detail = crate::drive_health_audit_detail(index, snapshot)?;
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: snapshot
                .session_id
                .as_deref()
                .and_then(|value| Uuid::parse_str(value).ok()),
            idempotency_key: None,
            event: AuditEvent::DriveHealthObserved,
            subject_kind: "drive",
            subject_id: Some(crate::bytes_to_hex(snapshot.drive_uuid.as_slice())),
            detail,
        },
    )?;
    Ok(())
}

fn insert_optional_audit_text(
    detail: &mut BTreeMap<String, CborValue>,
    key: &str,
    value: Option<&String>,
) {
    if let Some(value) = value {
        detail.insert(key.to_string(), CborValue::Text(value.clone()));
    }
}

fn raise_alarm_with_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    condition_key: &str,
    kind: &str,
    severity: &str,
    alarm_detail: Option<&str>,
) -> Result<AlarmRecord, Status> {
    let alarm = index
        .raise_alarm(condition_key, kind, severity, alarm_detail)
        .map_err(crate::status_from_state_error)
        .inspect_err(
            |error| tracing::warn!(condition_key, %error, "failed to raise catalog alarm"),
        )?;
    append_alarm_evidence(index, cfg, &alarm, AuditEvent::AlarmRaised).inspect_err(
        |error| tracing::warn!(condition_key, %error, "failed to append raised-alarm evidence"),
    )?;
    Ok(alarm)
}

fn clear_alarm_with_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    condition_key: &str,
) -> Result<Option<AlarmRecord>, Status> {
    let alarm = index
        .clear_alarm(condition_key)
        .map_err(crate::status_from_state_error)
        .inspect_err(
            |error| tracing::warn!(condition_key, %error, "failed to clear catalog alarm"),
        )?;
    if let Some(alarm) = alarm.as_ref() {
        append_alarm_evidence(index, cfg, alarm, AuditEvent::AlarmCleared).inspect_err(
            |error| {
                tracing::warn!(condition_key, %error, "failed to append cleared-alarm evidence")
            },
        )?;
    }
    Ok(alarm)
}

fn append_alarm_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    alarm: &AlarmRecord,
    event: AuditEvent,
) -> Result<(), Status> {
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event,
            subject_kind: "alarm",
            subject_id: Some(alarm.condition_key.clone()),
            detail: crate::alarm_audit_detail(alarm),
        },
    )?;
    Ok(())
}

fn record_session_close_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    drive_uuid: Option<Vec<u8>>,
    session_id: Uuid,
    tape_uuid: [u8; 16],
    consecutive_misses: &mut u32,
) {
    record_session_snapshot(
        index,
        cfg,
        drive,
        drive_uuid,
        session_id,
        tape_uuid,
        "session-close",
        consecutive_misses,
    );
}

#[allow(clippy::too_many_arguments)]
fn record_session_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    drive_uuid: Option<Vec<u8>>,
    session_id: Uuid,
    tape_uuid: [u8; 16],
    trigger: &'static str,
    consecutive_misses: &mut u32,
) {
    let Some(drive_uuid) = drive_uuid else {
        return;
    };
    match collect_drive_health_snapshot(
        index,
        cfg,
        drive,
        DriveSnapshotRequest {
            drive_uuid: drive_uuid.clone(),
            trigger,
            session_id: Some(session_id),
            tape_uuid: Some(tape_uuid),
        },
    ) {
        Ok(_) => {
            clear_snapshot_persist_alarm(index, cfg, drive_uuid.as_slice());
            *consecutive_misses = 0;
        }
        Err(err) => {
            *consecutive_misses = consecutive_misses.saturating_add(1);
            tracing::warn!(
                "drive health snapshot missed session_id={} drive_uuid={} misses={} error={}",
                session_id,
                Uuid::from_slice(&drive_uuid)
                    .map(|uuid| uuid.to_string())
                    .unwrap_or_else(|_| crate::bytes_to_hex(&drive_uuid)),
                *consecutive_misses,
                err
            );
            if cfg.snapshot_miss_alarm > 0 && *consecutive_misses >= cfg.snapshot_miss_alarm {
                let condition_key = snapshot_persist_alarm_key(&drive_uuid);
                let detail = format!(
                    "{{\"session_id\":\"{}\",\"misses\":{},\"error\":\"{}\"}}",
                    session_id,
                    *consecutive_misses,
                    err.to_string().replace('"', "'")
                );
                if let Err(alarm_err) = raise_alarm_with_evidence(
                    index,
                    cfg,
                    condition_key.as_str(),
                    "snapshot-persist-failing",
                    "warning",
                    Some(detail.as_str()),
                ) {
                    tracing::warn!(
                        "failed to raise snapshot miss alarm condition_key={} error={}",
                        condition_key,
                        alarm_err
                    );
                }
            }
        }
    }
}

fn snapshot_persist_alarm_key(drive_uuid: &[u8]) -> String {
    format!(
        "snapshot-persist-failing:{}",
        crate::bytes_to_hex(drive_uuid)
    )
}

fn clear_snapshot_persist_alarm(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive_uuid: &[u8],
) {
    let condition_key = snapshot_persist_alarm_key(drive_uuid);
    if let Err(err) = clear_alarm_with_evidence(index, cfg, condition_key.as_str()) {
        tracing::warn!(
            "failed to clear snapshot miss alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

fn tape_alert_flags_json(flags: &std::collections::BTreeSet<u8>) -> String {
    let body = flags
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

fn u64_to_i64(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}

/// Append gate for one write session: the first failed append poisons
/// the session for all further appends.
///
/// Why: after a failed `AppendFinish` the drive head position and the
/// tape's committed state are unknown territory, and the parity-append
/// guard in `ensure_selected_tape_accepts_write` keys on
/// `total_committed_ordinals > 0` — which is still 0 after a *failed*
/// first append. Without this gate a client retry passes that guard
/// and writes a fresh BOT-relative bootstrap at the current mid-tape
/// position, committing locators that later `space(tape_file_number)`
/// reads mis-resolve. Close/Abort remain allowed (already-committed
/// objects are intact); writing again requires a new session, which
/// re-runs tape selection and the identity/position preparation.
#[derive(Debug, Default)]
struct SessionAppendGate {
    poisoned: bool,
    sealed: bool,
}

#[derive(Debug)]
struct PendingCheckpointBatch {
    batch_id: Uuid,
    opened_at: Instant,
    deadline: Instant,
    logical_bytes: u64,
    used_bytes: u64,
    early_warning: bool,
    objects: Vec<crate::pool_write::PoolWriteResult>,
    /// Daemon-wide idempotency/UUID claims remain live until this batch is
    /// either projected or abandoned. This closes the gap between the
    /// pre-motion catalog read and the later checkpoint transaction.
    _write_admissions: Vec<WriteAdmissionReservation>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct WriteReplayKey {
    pool_id: String,
    caller_object_id: String,
}

#[derive(Debug, Default)]
struct WriteAdmissionState {
    replay_keys: HashSet<WriteReplayKey>,
    object_ids: HashSet<[u8; 16]>,
}

/// Process-wide admission authority shared by every drive actor.
///
/// SQLite remains the durable committed authority. These short-lived claims
/// cover only the interval from the last pre-motion catalog read through the
/// checkpoint projection, so two drive actors cannot both write an identity
/// that the catalog can commit only once.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriteAdmissionCoordinator {
    state: Arc<Mutex<WriteAdmissionState>>,
}

impl WriteAdmissionCoordinator {
    fn reserve(
        &self,
        pool_id: &str,
        caller_object_id: &str,
        object_id: Option<[u8; 16]>,
    ) -> Result<WriteAdmissionReservation, Status> {
        let replay_key = (!caller_object_id.trim().is_empty()).then(|| WriteReplayKey {
            pool_id: pool_id.to_string(),
            caller_object_id: caller_object_id.to_string(),
        });
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if replay_key
            .as_ref()
            .is_some_and(|key| state.replay_keys.contains(key))
            || object_id.is_some_and(|id| state.object_ids.contains(&id))
        {
            return Err(Status::aborted(
                "an append with the same pool/caller replay key or canonical Object UUID is still awaiting checkpoint; retry after that checkpoint completes",
            ));
        }
        if let Some(key) = replay_key.as_ref() {
            state.replay_keys.insert(key.clone());
        }
        if let Some(id) = object_id {
            state.object_ids.insert(id);
        }
        drop(state);
        Ok(WriteAdmissionReservation {
            coordinator: self.clone(),
            replay_key,
            object_id,
            release_on_drop: true,
        })
    }
}

#[derive(Debug)]
struct WriteAdmissionReservation {
    coordinator: WriteAdmissionCoordinator,
    replay_key: Option<WriteReplayKey>,
    object_id: Option<[u8; 16]>,
    release_on_drop: bool,
}

impl WriteAdmissionReservation {
    /// Leave this identity in the coordinator until process restart.
    ///
    /// This is used only after a checkpoint journal fsync succeeded but its
    /// SQLite projection failed. Startup replays every checkpoint journal
    /// before admitting writes, so restarting is the point at which the
    /// durable identity becomes visible and a new coordinator is safe.
    fn quarantine_until_restart(&mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for WriteAdmissionReservation {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(key) = self.replay_key.as_ref() {
            state.replay_keys.remove(key);
        }
        if let Some(id) = self.object_id {
            state.object_ids.remove(&id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_provisional_replay_guards(
    caller_object_id: &str,
    pending_input_kind: crate::WriteObjectInputKind,
    pending_object_id: [u8; 16],
    pending_content_sha256: [u8; 32],
    requested_input_kind: crate::WriteObjectInputKind,
    expected_object_id: Option<[u8; 16]>,
    expected_content_sha256: Option<[u8; 32]>,
    requested_content_sha256: [u8; 32],
) -> Result<(), Status> {
    match (requested_input_kind, expected_object_id) {
        (crate::WriteObjectInputKind::LogicalFile, Some(_)) => {
            return Err(Status::invalid_argument(
                "expected_object_id is valid only for canonical plaintext REM object ingestion",
            ));
        }
        (crate::WriteObjectInputKind::CanonicalPlaintextRemObject, None) => {
            return Err(Status::invalid_argument(
                "canonical plaintext REM object ingestion requires expected_object_id",
            ));
        }
        _ => {}
    }
    if let Some(expected) = expected_content_sha256 {
        if expected != requested_content_sha256 {
            return Err(Status::failed_precondition(format!(
                "content SHA-256 guard mismatch inside checkpoint batch: expected={}, requested={}",
                crate::bytes_to_hex(&expected),
                crate::bytes_to_hex(&requested_content_sha256),
            )));
        }
    }
    if pending_input_kind != requested_input_kind {
        return Err(Status::already_exists(format!(
            "caller_object_id replay changed input kind inside checkpoint batch: caller_object_id={caller_object_id:?}"
        )));
    }
    if let Some(expected) = expected_object_id {
        if expected != pending_object_id {
            return Err(Status::invalid_argument(format!(
                "canonical plaintext REM object replay identity mismatch inside checkpoint batch: committed={}, expected={}",
                Uuid::from_bytes(pending_object_id),
                Uuid::from_bytes(expected),
            )));
        }
    }
    if requested_content_sha256 != pending_content_sha256 {
        return Err(Status::already_exists(format!(
            "caller_object_id replay conflict inside checkpoint batch: caller_object_id={caller_object_id:?}, existing content_sha256={}, requested content_sha256={}",
            crate::bytes_to_hex(&pending_content_sha256),
            crate::bytes_to_hex(&requested_content_sha256),
        )));
    }
    Ok(())
}

struct ParityActorSession {
    scheme: remanence_parity::ParityScheme,
    sink_state: Option<ParitySinkSessionState>,
    journal: Option<FileTapeFileJournal>,
}

struct ParityActorAuthority {
    scheme: remanence_parity::ParityScheme,
    snapshot: FileTapeFileJournalCommittedSnapshot,
    summary: BoundedResumeSummary,
    journal: FileTapeFileJournal,
}

/// Records entry into a direct parity raw-write boundary. Position-only
/// validation does not mark media dirty; the first write command does.
struct ActivityTrackingRawTapeSink<'a> {
    inner: &'a mut dyn RawTapeSink,
    write_attempted: &'a mut bool,
    position_ready: bool,
}

impl<'a> ActivityTrackingRawTapeSink<'a> {
    fn new(inner: &'a mut dyn RawTapeSink, write_attempted: &'a mut bool) -> Self {
        Self {
            inner,
            write_attempted,
            position_ready: false,
        }
    }

    fn ensure_position_ready(&mut self) -> Result<(), ParityError> {
        if !self.position_ready {
            self.inner.position()?;
            self.position_ready = true;
        }
        Ok(())
    }
}

impl RawTapeSink for ActivityTrackingRawTapeSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.ensure_position_ready()?;
        *self.write_attempted = true;
        let result = self.inner.write_fixed_block(buf);
        if result.is_err() {
            self.position_ready = false;
        }
        result
    }

    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError> {
        self.ensure_position_ready()?;
        *self.write_attempted = true;
        let result = self.inner.write_filemarks(count, immed);
        if result.is_err() {
            self.position_ready = false;
        }
        result
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position = self.inner.position()?;
        self.position_ready = true;
        Ok(position)
    }
}

fn parity_journal_path(cfg: &WriteOwnerConfig, tape_uuid: TapeUuid) -> Result<PathBuf, Status> {
    let journal_dir = cfg.checkpoint_journal_dir.parent().ok_or_else(|| {
        Status::internal("checkpoint journal directory has no parent for the parity journal")
    })?;
    Ok(journal_dir.join(format!("{}.remjournal", crate::bytes_to_hex(&tape_uuid))))
}

fn validate_parity_actor_authority(
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    checkpoint: &remanence_state::FileCheckpointJournalLease,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
) -> Result<ParityActorAuthority, Status> {
    let scheme = match &selected.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity actor session requested for a parity-off tape",
            ));
        }
    };
    let path = parity_journal_path(cfg, selected.tape_uuid)?;
    let journal = FileTapeFileJournal::open(
        path,
        selected.tape_uuid,
        selected.block_size,
        scheme.clone(),
    )
    .map_err(|err| Status::internal(format!("open parity tape journal: {err}")))?;
    if journal.orphaned_bundles_preserved_on_open() != 0 {
        tracing::warn!(
            tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
            orphaned_bundle_count = journal.orphaned_bundles_preserved_on_open(),
            "preserved sink-journal bundles beyond the last checkpoint watermark; reconciliation required"
        );
    }
    if checkpoints.is_empty() {
        if journal.orphaned_bundles_preserved_on_open() != 0 {
            return Err(Status::failed_precondition(
                "parity journal has orphan rows but checkpoint authority is empty",
            ));
        }
    } else {
        remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed(
            checkpoint, &journal,
        )
        .map_err(crate::status_from_state_error)?;
    }
    let snapshot = journal
        .committed_snapshot_bounded()
        .map_err(|err| Status::internal(format!("freeze parity tape journal: {err}")))?;
    let summary = checked_bounded_resume_summary(&snapshot)
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
    if checkpoints.is_empty() && summary.committed_tape_file_count != 0 {
        return Err(Status::failed_precondition(
            "parity journal has a committed prefix but checkpoint authority is empty",
        ));
    }
    Ok(ParityActorAuthority {
        scheme,
        snapshot,
        summary,
        journal,
    })
}

fn open_parity_actor_session(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
    authority: ParityActorAuthority,
) -> Result<ParityActorSession, Status> {
    let ParityActorAuthority {
        scheme,
        snapshot,
        summary,
        mut journal,
    } = authority;
    let sink_state = if summary.committed_tape_file_count == 0 {
        let preflight_state = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut write_attempted = false;
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut raw, &mut write_attempted);
            (|| -> Result<ParitySinkSessionState, ParityError> {
                let sink = ParitySink::new_with_journal(
                    &mut tracked,
                    &mut journal,
                    scheme.clone(),
                    selected.tape_uuid,
                    selected.block_size,
                )?;
                sink.into_session_state()
            })()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?
        };
        drive
            .locate(0)
            .map_err(|err| Status::unavailable(format!("locate fresh parity BOT: {err}")))?;
        let mut write_attempted = false;
        let opened = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut raw, &mut write_attempted);
            (|| -> Result<ParitySinkSessionState, ParityError> {
                let mut sink =
                    ParitySink::from_session_state(&mut tracked, &mut journal, preflight_state)?;
                sink.write_bootstrap()?;
                sink.into_session_state()
            })()
        };
        match opened {
            Ok(state) => {
                crate::pool_write::project_fresh_parity_bootstrap_bundle(index, selected, &scheme)
                    .map_err(|err| {
                        Status::internal(format!("project fresh parity bootstrap: {err}"))
                    })?;
                state
            }
            Err(err) => {
                let error = err.to_string();
                let status = status_from_parity_error(&err, error.clone());
                if write_attempted {
                    return Err(fence_failed_parity_raw_write(
                        index,
                        cfg,
                        selected,
                        "fresh_bootstrap",
                        None,
                        None,
                        error.as_str(),
                        status,
                    )
                    .0);
                }
                return Err(status);
            }
        }
    } else {
        let checkpoint = checkpoints.last().ok_or_else(|| {
            Status::failed_precondition(
                "non-fresh parity tape has no shared checkpoint journal watermark",
            )
        })?;
        drive
            .locate(checkpoint.eod_lba)
            .map_err(|err| Status::unavailable(format!("locate parity checkpoint EOD: {err}")))?;
        let plan = summary
            .append_plan(&scheme)
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        if !plan.sidecars_to_emit.is_empty()
            || plan.highest_protected_ordinal_before_rebuild != plan.next_data_ordinal
        {
            return Err(Status::failed_precondition(
                "checkpointed parity tape unexpectedly retains an open epoch",
            ));
        }
        let resume_result = plan
            .complete(Vec::new())
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        let mut raw = DriveHandleRawSink::new(drive);
        let sink = ParitySink::new_sidecar_only_from_bounded_resume(
            &mut raw,
            &mut journal,
            scheme.clone(),
            selected.tape_uuid,
            selected.block_size,
            BoundedResumeWriterSeed {
                committed_prefix_snapshot: snapshot,
                committed_prefix_summary: summary,
                resume_result: &resume_result,
                live_epoch: None,
            },
        )
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        sink.into_session_state()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?
    };
    Ok(ParityActorSession {
        scheme,
        sink_state: Some(sink_state),
        journal: Some(journal),
    })
}

impl PendingCheckpointBatch {
    fn new(max_age: StdDuration) -> Self {
        let opened_at = Instant::now();
        Self {
            batch_id: Uuid::new_v4(),
            opened_at,
            deadline: opened_at + max_age,
            logical_bytes: 0,
            used_bytes: 0,
            early_warning: false,
            objects: Vec::new(),
            _write_admissions: Vec::new(),
        }
    }

    fn push(
        &mut self,
        logical_bytes: u64,
        result: crate::pool_write::PoolWriteResult,
        write_admission: WriteAdmissionReservation,
    ) {
        self.logical_bytes = self.logical_bytes.saturating_add(logical_bytes);
        self.used_bytes = self.used_bytes.max(result.post_write_used_bytes());
        self.early_warning |= result.hardware_early_warning();
        self.objects.push(result);
        self._write_admissions.push(write_admission);
        debug_assert_eq!(self.objects.len(), self._write_admissions.len());
    }

    fn should_checkpoint(&self, cfg: &WriteOwnerConfig) -> bool {
        self.logical_bytes >= cfg.checkpoint_max_bytes
            || self.objects.len() as u64 >= cfg.checkpoint_max_objects
    }

    fn quarantine_write_admissions_until_restart(&mut self) {
        for admission in &mut self._write_admissions {
            admission.quarantine_until_restart();
        }
    }
}

struct BarrierOutcome {
    committed_objects: Vec<pb::ObjectRecord>,
    object_count: u64,
    logical_bytes: u64,
    filemark_drain: StdDuration,
    journal_projection: StdDuration,
    checkpoint_record: remanence_state::CheckpointJournalRecord,
    terminal_checkpoint_record: Option<remanence_state::CheckpointJournalRecord>,
    sealed_after_write: bool,
}

fn extend_durable_checkpoint_records(
    records: &mut Vec<remanence_state::CheckpointJournalRecord>,
    outcome: &BarrierOutcome,
) {
    records.clear();
    records.push(
        outcome
            .terminal_checkpoint_record
            .as_ref()
            .unwrap_or(&outcome.checkpoint_record)
            .clone(),
    );
}

/// Rebuild the catalog projection by streaming checkpoint authority while
/// retaining only the final record needed by the live writer actor.
fn project_checkpoint_authority_bounded(
    index: &mut CatalogIndex,
    lease: &remanence_state::FileCheckpointJournalLease,
) -> Result<Vec<remanence_state::CheckpointJournalRecord>, remanence_state::StateError> {
    let mut last = None;
    lease.for_each_record_bounded(|record| {
        index.project_checkpoint_record(record)?;
        last = Some(record.clone());
        Ok(())
    })?;
    Ok(last.into_iter().collect())
}

#[derive(Debug)]
struct CheckpointBarrierFailure {
    status: Status,
    journal_durable: bool,
    catalog_projected: bool,
    fence_handled: bool,
}

impl CheckpointBarrierFailure {
    fn before_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            catalog_projected: false,
            fence_handled: false,
        }
    }

    fn before_journal_with_fence_handled(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            catalog_projected: false,
            fence_handled: true,
        }
    }

    fn after_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            catalog_projected: false,
            fence_handled: false,
        }
    }

    fn after_projection(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            catalog_projected: true,
            fence_handled: false,
        }
    }

    fn requires_identity_quarantine(&self) -> bool {
        self.journal_durable && !self.catalog_projected
    }
}

/// Rebuild committed-object receipts from the catalog projection made durable by a barrier.
///
/// The caller's pre-barrier WRITTEN acknowledgement is deliberately locator-free, while the
/// pending batch contains pre-projection write results. Reading the durable record's projected
/// object ids back keeps the CHECKPOINTED response aligned with the catalog's canonical
/// object/copy protobuf conversion.
fn checkpointed_objects_from_catalog(
    index: &CatalogIndex,
    checkpoint_record: &remanence_state::CheckpointJournalRecord,
    sealed_after_write: bool,
) -> Result<Vec<pb::ObjectRecord>, CheckpointBarrierFailure> {
    checkpoint_record
        .objects
        .iter()
        .map(|projection| {
            let object_id = projection.object.object_id.as_str();
            let object = index
                .get_native_object(object_id)
                .map_err(|err| {
                    CheckpointBarrierFailure::after_projection(Status::internal(format!(
                        "checkpoint is durable but catalog lookup for committed object {object_id} failed: {err}"
                    )))
                })?
                .ok_or_else(|| {
                    CheckpointBarrierFailure::after_projection(Status::internal(format!(
                        "checkpoint is durable but committed object {object_id} is absent from the catalog projection"
                    )))
                })?;
            let mut record = crate::object_record_to_proto(object).map_err(|err| {
                CheckpointBarrierFailure::after_projection(Status::internal(format!(
                    "checkpoint is durable but committed object {object_id} could not be encoded from the catalog: {}",
                    err.message()
                )))
            })?;
            let append_info = record.append_commit_info.as_mut().ok_or_else(|| {
                CheckpointBarrierFailure::after_projection(Status::internal(format!(
                    "checkpoint is durable but committed object {object_id} has no projected copies"
                )))
            })?;
            append_info.sealed_after_write = Some(sealed_after_write);
            append_info.durability = pb::AppendDurability::Checkpointed as i32;
            Ok(record)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn perform_checkpoint_barrier(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    journal: &mut remanence_state::FileCheckpointJournalLease,
    tape_uuid: TapeUuid,
    checkpoint_ordinal: &mut u64,
    tape_committed_object_count: &mut u64,
    batch: &PendingCheckpointBatch,
    mut parity_session: Option<&mut ParityActorSession>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    cfg: &WriteOwnerConfig,
) -> Result<BarrierOutcome, CheckpointBarrierFailure> {
    let drain_started = Instant::now();
    let next_ordinal = checkpoint_ordinal.checked_add(1).ok_or_else(|| {
        CheckpointBarrierFailure::before_journal(Status::internal(
            "checkpoint ordinal overflows u64",
        ))
    })?;
    let object_count = u64::try_from(batch.objects.len()).map_err(|_| {
        CheckpointBarrierFailure::before_journal(Status::internal(
            "checkpoint object count exceeds u64",
        ))
    })?;
    let next_committed_count = tape_committed_object_count
        .checked_add(object_count)
        .ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "checkpoint committed object count overflows u64",
            ))
        })?;
    let objects = batch
        .objects
        .iter()
        .map(|result| {
            result.checkpoint_projection().cloned().ok_or_else(|| {
                CheckpointBarrierFailure::before_journal(Status::internal(
                    "batched object is missing its checkpoint projection",
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object_tape_file_bundles = if parity_session.is_some() {
        batch
            .objects
            .iter()
            .map(|result| {
                result
                    .write_report()
                    .map(|report| report.catalog.tape_file_bundle.clone())
                    .ok_or_else(|| {
                        CheckpointBarrierFailure::before_journal(Status::internal(
                            "parity checkpoint object is missing its Layer 3c bundle",
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let (
        next_tape_file_number,
        checkpoint_block_size,
        sync,
        barrier_bundle,
        scheme,
        parity_early_warning,
    ) = if let Some(parity_session) = parity_session.as_deref_mut() {
        let state = parity_session.sink_state.take().ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "parity sink session state is unavailable",
            ))
        })?;
        let parity_journal = parity_session.journal.as_mut().ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "active parity session has no append journal",
            ))
        })?;
        let mut raw_write_attempted = false;
        let barrier_result = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut raw, &mut raw_write_attempted);
            (|| -> Result<_, ParityError> {
                let mut sink = ParitySink::from_session_state(&mut tracked, parity_journal, state)?;
                let closed = sink.close_open_epoch(CloseReason::Barrier)?;
                let sink_state = sink.into_session_state()?;
                Ok((closed, sink_state))
            })()
        };
        let (closed, sink_state) = match barrier_result {
            Ok(result) => result,
            Err(err) => {
                let error = err.to_string();
                let status = status_from_parity_error(&err, error.clone());
                if raw_write_attempted {
                    let fenced = fence_failed_parity_raw_write(
                        index,
                        cfg,
                        selected,
                        "checkpoint_barrier",
                        None,
                        Some(batch),
                        error.as_str(),
                        status,
                    );
                    return Err(CheckpointBarrierFailure::before_journal_with_fence_handled(
                        fenced.0,
                    ));
                }
                return Err(CheckpointBarrierFailure::before_journal(status));
            }
        };
        let parity_early_warning = sink_state.hardware_early_warning_seen();
        parity_session.sink_state = Some(sink_state);
        (
            closed.next_tape_file_number,
            objects[0].block_size,
            closed.barrier_outcome,
            closed.committed_bundle,
            Some(parity_session.scheme.clone()),
            parity_early_warning,
        )
    } else {
        let sync = drive.write_filemarks(0).map_err(|err| {
            CheckpointBarrierFailure::before_journal(Status::unavailable(format!(
                "checkpoint batch {} barrier failed; re-send all {} WRITTEN objects: {err}",
                batch.batch_id,
                batch.objects.len()
            )))
        })?;
        let last = objects.last().ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "parity-off checkpoint batch has no Object projection",
            ))
        })?;
        let next_tape_file_number = last.copy.tape_file_number.checked_add(1).ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "checkpoint next tape-file number overflows u64",
            ))
        })?;
        (
            next_tape_file_number,
            last.block_size,
            sync,
            None,
            None,
            false,
        )
    };
    let captured = drive.position().map_err(|err| {
        CheckpointBarrierFailure::before_journal(Status::unavailable(format!(
            "checkpoint batch {} READ POSITION failed; re-send all {} WRITTEN objects: {err}",
            batch.batch_id,
            batch.objects.len()
        )))
    })?;
    if captured.partition != sync.position_after.partition
        || captured.lba != sync.position_after.lba
    {
        return Err(CheckpointBarrierFailure::before_journal(
            Status::unavailable(format!(
            "checkpoint batch {} position proof mismatch: synchronous barrier reported partition {} lba {}, daemon READ POSITION observed partition {} lba {}; re-send all {} WRITTEN objects",
            batch.batch_id,
            sync.position_after.partition,
            sync.position_after.lba,
            captured.partition,
            captured.lba,
            batch.objects.len()
        )),
        ));
    }
    let record = remanence_state::CheckpointJournalRecord {
        ordinal: next_ordinal,
        committed_object_count: next_committed_count,
        eod_partition: captured.partition,
        eod_lba: captured.lba,
        tape_uuid,
        batch_id: *batch.batch_id.as_bytes(),
        next_tape_file_number,
        block_size: checkpoint_block_size,
        objects,
        scheme,
        object_tape_file_bundles,
        barrier_bundle,
        terminal_finalization: None,
        sealed_after_write: false,
    };
    let barrier_used_bytes = captured
        .lba
        .checked_mul(u64::from(checkpoint_block_size))
        .ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "checkpoint post-barrier used-byte count overflows u64",
            ))
        })?;
    let used_bytes = batch.used_bytes.max(barrier_used_bytes);
    let seal_reason = crate::pool_write::selected_tape_seal_reason_at_barrier(
        index,
        selected,
        pool_cfg,
        crate::pool_write::TapePositionAfterWrite {
            used_bytes,
            early_warning: batch.early_warning || sync.early_warning || parity_early_warning,
        },
    )
    .map_err(|err| CheckpointBarrierFailure::before_journal(status_from_pool_write_error(err)))?;
    let filemark_drain = drain_started.elapsed();
    let projection_started = Instant::now();
    journal.append(&record).map_err(|err| {
        CheckpointBarrierFailure::before_journal(Status::internal(format!(
            "checkpoint batch {} journal fsync failed; re-send all {} WRITTEN objects: {err}",
            batch.batch_id,
            batch.objects.len(),
        )))
    })?;
    *checkpoint_ordinal = next_ordinal;
    *tape_committed_object_count = next_committed_count;
    index
        .project_checkpoint_record(&record)
        .map_err(|err| {
            CheckpointBarrierFailure::after_journal(Status::internal(format!(
                "checkpoint is durable in the journal but SQLite projection failed; close the session and retry after journal replay: {err}"
            )))
        })?;
    let terminal_checkpoint_record = if let Some(reason) = seal_reason {
        let trigger = match reason {
            crate::pool_write::TapeSealReason::ReachedLowWatermark => {
                remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark
            }
            crate::pool_write::TapeSealReason::HardwareEarlyWarning => {
                remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning
            }
            crate::pool_write::TapeSealReason::OperatorCloseOut => {
                remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
            }
            crate::pool_write::TapeSealReason::PoolCloseOut => {
                remanence_state::TerminalFinalizationTrigger::PoolCloseOut
            }
            crate::pool_write::TapeSealReason::NoPendingObjectFits => {
                remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits
            }
        };
        if trigger == remanence_state::TerminalFinalizationTrigger::OperatorCloseOut {
            return Err(CheckpointBarrierFailure::after_projection(
                Status::internal(
                    "automatic checkpoint seal unexpectedly carried an operator trigger",
                ),
            ));
        }
        let drive_config = drive.read_config().map_err(|error| {
            CheckpointBarrierFailure::after_projection(Status::unavailable(format!(
                "read terminal-finalization drive config: {error}"
            )))
        })?;
        if drive_config.write_protected {
            return Err(CheckpointBarrierFailure::after_projection(
                Status::failed_precondition(
                    "tape became write-protected before terminal finalization",
                ),
            ));
        }
        let rewritable = matches!(
            drive_config.worm,
            remanence_library::WormMediaState::NotWorm
        );
        let spec = TerminalFinalizeSpec::automatic(selected, pool_cfg, trigger);
        let result = match &selected.parity_config {
            remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
                index, cfg, drive, journal, &record, None, &spec, selected, None, rewritable,
            ),
            remanence_parity::ParityConfig::Scheme(_) => {
                let parity_journal = parity_session
                    .and_then(|session| session.journal.take())
                    .ok_or_else(|| {
                        CheckpointBarrierFailure::after_projection(Status::internal(
                            "automatic parity finalization has no session-owned append journal",
                        ))
                    })?;
                finalize_terminal_with_parity_journal(
                    index,
                    cfg,
                    drive,
                    journal,
                    &record,
                    None,
                    &spec,
                    selected,
                    None,
                    parity_journal,
                    rewritable,
                )
            }
        }
        .map_err(CheckpointBarrierFailure::after_projection)?;
        let final_record = result.final_record.ok_or_else(|| {
            CheckpointBarrierFailure::after_projection(Status::unavailable(
                "automatic terminal finalization requires recovery before sealing can complete",
            ))
        })?;
        *checkpoint_ordinal = final_record.ordinal;
        Some(final_record)
    } else {
        None
    };
    let sealed_after_write = terminal_checkpoint_record.is_some();
    if sealed_after_write {
        if let Err(err) = append_tape_sealed_evidence(index, cfg, selected.tape_uuid) {
            tracing::warn!(error = %err, "failed to append tape sealing evidence");
        }
    }
    let journal_projection = projection_started.elapsed();
    tracing::info!(
        target: "remanence_write_diag",
        phase = "checkpoint_barrier",
        batch_id = %batch.batch_id,
        tape_uuid = %Uuid::from_bytes(tape_uuid),
        batch_objects = object_count,
        batch_logical_bytes = batch.logical_bytes,
        position_partition = captured.partition,
        position_lba = captured.lba,
        position_proof_ok = true,
        filemark_drain_ms = crate::diagnostics::duration_ms(filemark_drain),
        journal_projection_ms = crate::diagnostics::duration_ms(journal_projection),
        "remanence_write_diag",
    );
    let committed_objects = checkpointed_objects_from_catalog(index, &record, sealed_after_write)?;
    Ok(BarrierOutcome {
        committed_objects,
        object_count,
        logical_bytes: batch.logical_bytes,
        filemark_drain,
        journal_projection,
        checkpoint_record: record,
        terminal_checkpoint_record,
        sealed_after_write,
    })
}

fn fence_failed_checkpoint_batch(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    batch: &PendingCheckpointBatch,
    status: Status,
) -> Status {
    let barcode = index
        .get_tape(&selected.tape_uuid)
        .ok()
        .flatten()
        .and_then(|tape| tape.voltag);
    let caller_objects = batch
        .objects
        .iter()
        .map(|result| result.object.caller_object_id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let evidence = serde_json::json!({
        "batch_id": batch.batch_id.to_string(),
        "caller_object_ids": caller_objects,
        "error": status.message(),
    })
    .to_string();
    match index.record_tape_io_fence(remanence_state::TapeIoFenceInput {
        tape_uuid: selected.tape_uuid,
        barcode,
        reason: "checkpoint_barrier_failed".to_string(),
        evidence_json: Some(evidence),
    }) {
        Ok(fence) => {
            match append_tape_io_fence_evidence(index, cfg, &fence, AuditEvent::TapeIoFenceRaised) {
                Ok(()) => status,
                Err(err) => Status::internal(format!(
                    "{}; tape fence {} persisted but its audit evidence failed: {err}",
                    status.message(),
                    fence.quarantine_id
                )),
            }
        }
        Err(err) => {
            tracing::error!(
                tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                batch_id = %batch.batch_id,
                error = %err,
                "failed to persist checkpoint batch tape-I/O fence"
            );
            Status::internal(format!(
                "{}; additionally failed to persist the required tape fence: {err}",
                status.message()
            ))
        }
    }
}

fn parity_raw_write_fence_reason(error: &str) -> &'static str {
    if error.contains("partial fixed batch uncommittable") {
        "partial_batch"
    } else if error.contains("reset UNIT ATTENTION") {
        "reset_unit_attention"
    } else if error.contains("position drift") || error.contains("position mismatch") {
        "position_drift"
    } else {
        "transfer_error"
    }
}

#[allow(clippy::too_many_arguments)]
fn fence_failed_parity_raw_write(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    phase: &'static str,
    current_caller_object_id: Option<&str>,
    batch: Option<&PendingCheckpointBatch>,
    error: &str,
    status: Status,
) -> (Status, bool) {
    let barcode = index
        .get_tape(&selected.tape_uuid)
        .ok()
        .flatten()
        .and_then(|tape| tape.voltag);
    let prior_caller_object_ids = batch.map(|batch| {
        batch
            .objects
            .iter()
            .map(|result| result.object.caller_object_id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    });
    let evidence = serde_json::json!({
        "phase": phase,
        "pool_id": selected.pool_id,
        "tape_uuid": Uuid::from_bytes(selected.tape_uuid).to_string(),
        "batch_id": batch.map(|batch| batch.batch_id.to_string()),
        "prior_caller_object_ids": prior_caller_object_ids,
        "current_caller_object_id": current_caller_object_id,
        "error": error,
    })
    .to_string();
    let fence = match index.record_tape_io_fence(remanence_state::TapeIoFenceInput {
        tape_uuid: selected.tape_uuid,
        barcode,
        reason: parity_raw_write_fence_reason(error).to_string(),
        evidence_json: Some(evidence),
    }) {
        Ok(fence) => fence,
        Err(err) => {
            tracing::error!(
                tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                phase,
                error = %err,
                "failed to persist parity raw-write tape-I/O fence"
            );
            return (
                Status::internal(format!(
                    "{}; additionally failed to persist the required tape fence: {err}",
                    status.message()
                )),
                false,
            );
        }
    };
    match append_tape_io_fence_evidence(index, cfg, &fence, AuditEvent::TapeIoFenceRaised) {
        Ok(()) => (status, true),
        Err(err) => {
            tracing::error!(
                tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                quarantine_id = fence.quarantine_id.as_str(),
                phase,
                error = %err,
                "persisted parity raw-write fence but failed to append audit evidence"
            );
            (
                Status::internal(format!(
                    "{}; tape fence {} persisted but its audit evidence failed: {err}",
                    status.message(),
                    fence.quarantine_id
                )),
                false,
            )
        }
    }
}

fn arm_checkpoint_timer(
    actor_tx: mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    batch_id: Uuid,
    max_age: StdDuration,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(format!("rem-checkpoint-timer-{session_id}"))
        .spawn(move || {
            std::thread::sleep(max_age);
            let _ = actor_tx.blocking_send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Timer,
                expected_batch_id: Some(batch_id),
                reply: None,
            });
        })
        .map(|_| ())
}

fn arm_checkpoint_idle_close(
    actor_tx: mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    checkpoint_batch_id: Uuid,
    idle: StdDuration,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(format!("rem-checkpoint-idle-{session_id}"))
        .spawn(move || {
            std::thread::sleep(idle);
            let _ = actor_tx.blocking_send(DriveCommand::TimerIdleClose {
                session_id,
                checkpoint_batch_id,
            });
        })
        .map(|_| ())
}

fn park_timer_closed_session(cfg: &WriteOwnerConfig, session_id: Uuid) -> Result<(), Status> {
    let Some(lifecycle) = cfg.lifecycle.as_ref() else {
        return Ok(());
    };
    let mounted = lifecycle
        .sessions
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(&session_id);
    let Some(mounted) = mounted else {
        return Ok(());
    };
    let parked = mounted.home_slot.map(|home_slot| {
        let seated = SeatedCartridge {
            bay: mounted.bay,
            library_serial: mounted.library_serial,
            barcode: mounted.barcode,
            home_slot,
            tape_uuid: Some(mounted.tape_uuid),
            prior_session_id: Some(session_id),
        };
        let mut parked = lifecycle
            .parked
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        parked.next_generation = parked.next_generation.wrapping_add(1).max(1);
        let generation = parked.next_generation;
        let parked_cartridge = ParkedCartridge { seated, generation };
        parked.by_bay.insert(mounted.bay, parked_cartridge.clone());
        parked_cartridge
    });
    if let Some(reservation) = cfg.reservations.get(&mounted.bay) {
        reservation.store(false, Ordering::SeqCst);
    }
    if let (Some(parked), Some(timer_park_tx)) = (parked, lifecycle.timer_park_tx.as_ref()) {
        timer_park_tx.send(parked).map_err(|_| {
            Status::internal("timer-closed session could not arm lazy idle dismount")
        })?;
    }
    Ok(())
}

impl SessionAppendGate {
    fn check(&self) -> Result<(), Status> {
        if self.sealed {
            Err(Status::resource_exhausted(
                "selected tape sealed at the checkpoint boundary; reopen against the pool to roll placement",
            ))
        } else if self.poisoned {
            Err(Status::failed_precondition(
                "write session poisoned by a failed append; abort the session and open a new one",
            ))
        } else {
            Ok(())
        }
    }

    fn record_failure(&mut self) {
        self.poisoned = true;
    }

    fn record_sealed(&mut self) {
        self.sealed = true;
    }
}

struct SessionOpenReadinessContext<'a> {
    action: &'static str,
    bay: u16,
    library_serial: &'a str,
    barcode: Option<&'a str>,
    source_slot: Option<u16>,
    drive_serial: Option<&'a str>,
    needs_drive_load: bool,
}

#[cfg(test)]
const SESSION_OPEN_CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_millis(0);
#[cfg(not(test))]
const SESSION_OPEN_CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_secs(1);

fn session_open_short_probe_or_load(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
) -> Result<(), Status> {
    session_open_reject_admission_conflicts(index, &ctx)?;
    let family = session_open_media_family(ctx.barcode);
    let first = drive.probe_media_readiness(family);
    if first.is_ready() {
        return Ok(());
    }
    if session_open_readiness_requires_immediate_load(&ctx, &first) {
        return session_open_immediate_load_then_probe(
            index,
            drive,
            ctx,
            family,
            "drive LOAD IMMED",
        );
    }
    if session_open_readiness_should_retry_once(&first) {
        let second = drive.probe_media_readiness(family);
        if second.is_ready() {
            return Ok(());
        }
        if session_open_readiness_requires_immediate_load(&ctx, &second) {
            return session_open_immediate_load_then_probe(
                index,
                drive,
                ctx,
                family,
                "drive LOAD IMMED after retry",
            );
        }
        return Err(record_session_open_readiness_fence(
            index,
            &ctx,
            "session_open_short_probe",
            &second,
        ));
    }
    Err(record_session_open_readiness_fence(
        index,
        &ctx,
        "session_open_short_probe",
        &first,
    ))
}

fn handle_drive_wait_ready(
    bay: u16,
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    operation_id: Uuid,
    family: MediaFamily,
    options: MediaReadinessWaitOptions,
    handle: &crate::operations::OperationHandle,
) {
    handle.publish_state(
        pb::OperationState::Running,
        &[("phase", "readiness_poll"), ("state", "starting")],
    );
    let result = poll_drive_media_readiness(
        index,
        drive,
        operation_id,
        family,
        options,
        handle,
        "grpc_wait_ready",
    );

    match result {
        Ok(poll) if poll.readiness.is_ready() => handle.publish_state(
            pb::OperationState::Succeeded,
            &[("phase", "ready"), ("state", "ready")],
        ),
        Ok(poll) => {
            let state = if poll.timed_out {
                "timeout_unknown"
            } else {
                session_open_readiness_state(&poll.readiness)
            };
            let summary = if poll.timed_out {
                format!(
                    "timed out waiting for media readiness in drive bay 0x{bay:04x}: {}",
                    session_open_readiness_summary(&poll.readiness)
                )
            } else {
                format!(
                    "media readiness became non-retryable in drive bay 0x{bay:04x}: {}",
                    session_open_readiness_summary(&poll.readiness)
                )
            };
            handle.publish_failed(&summary, &[("phase", "readiness_poll"), ("state", state)]);
        }
        Err(error) if handle.is_cancelled() => handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", error.as_str())],
        ),
        Err(error) => handle.publish_failed(
            error.as_str(),
            &[("phase", "readiness_poll"), ("state", "recording_failed")],
        ),
    }
}

fn poll_drive_media_readiness(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    operation_id: Uuid,
    family: MediaFamily,
    options: MediaReadinessWaitOptions,
    handle: &crate::operations::OperationHandle,
    phase: &str,
) -> Result<MediaReadinessPoll, String> {
    drive.wait_for_media_readiness(
        family,
        None,
        options,
        || {
            handle
                .is_cancelled()
                .then(|| "daemon cancellation".to_string())
        },
        |event| match event {
            MediaReadinessWaitEvent::Poll(poll) => {
                record_session_open_readiness_poll_transition(
                    index,
                    operation_id,
                    phase,
                    &poll.readiness,
                    poll.timed_out,
                )
                .map_err(|error| format!("record media readiness transition: {error}"))?;
                let attempts = poll.attempts.to_string();
                let elapsed_seconds = poll.elapsed.as_secs().to_string();
                let state = if poll.timed_out {
                    "timeout_unknown"
                } else {
                    session_open_readiness_state(&poll.readiness)
                };
                handle.publish_state(
                    pb::OperationState::Running,
                    &[
                        ("phase", "readiness_poll"),
                        ("state", state),
                        ("attempts", attempts.as_str()),
                        ("elapsed_seconds", elapsed_seconds.as_str()),
                    ],
                );
                Ok(())
            }
            MediaReadinessWaitEvent::Cancelled(_) => Ok(()),
        },
    )
}

fn session_open_reject_admission_conflicts(
    index: &mut CatalogIndex,
    ctx: &SessionOpenReadinessContext<'_>,
) -> Result<(), Status> {
    let conflicts = index
        .media_readiness_admission_conflicts(ctx.library_serial, Some(ctx.bay), ctx.barcode, false)
        .map_err(status_from_state_error)?;
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(Status::failed_precondition(
        session_open_admission_error_message(ctx, &conflicts),
    ))
}

fn session_open_admission_error_message(
    ctx: &SessionOpenReadinessContext<'_>,
    conflicts: &[remanence_state::MediaReadinessOperationRecord],
) -> String {
    let conflict_summary = conflicts
        .iter()
        .map(|record| {
            format!(
                "operation={} state={} drive=0x{:04x} barcode={} quarantine={}",
                record.operation_id,
                record.state,
                record.drive_element,
                record.barcode.as_deref().unwrap_or("(unknown)"),
                record.quarantine_id.as_deref().unwrap_or("(none)")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let first_operation = conflicts
        .first()
        .map(|record| record.operation_id.as_str())
        .unwrap_or("(unknown)");
    format!(
        "{} blocked by active media-readiness fence library={} drive=0x{:04x} barcode={}: {}; run `rem tape wait-ready --library {} --resume {} --wait --json` or inspect quarantine before opening a session",
        ctx.action,
        ctx.library_serial,
        ctx.bay,
        ctx.barcode.unwrap_or("(unknown)"),
        conflict_summary,
        ctx.library_serial,
        first_operation,
    )
}

fn session_open_immediate_load_then_probe(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
    family: MediaFamily,
    detail_prefix: &str,
) -> Result<(), Status> {
    let operation_id = Uuid::new_v4();
    if let Err(err) = record_session_open_readiness_operation(index, operation_id, &ctx) {
        return Err(session_open_recording_failure_status(
            &ctx,
            None,
            "record_media_readiness_operation",
            &err,
        ));
    }
    if let Err(err) = record_session_open_mechanical_transition(
        index,
        operation_id,
        "session_open_immediate_load",
        "pre_ready_loading",
        Some(0x1b),
        None,
    ) {
        return Err(session_open_recording_failure_status(
            &ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        ));
    }
    std::thread::sleep(SESSION_OPEN_CONDITIONAL_LOAD_SETTLE);
    if let Err(err) = drive.load_immediate() {
        return Err(record_session_open_command_fence_on_operation(
            index,
            operation_id,
            &ctx,
            Some(0x1b),
            format!("{detail_prefix}: {err}"),
        ));
    }
    session_open_short_probe_after_load(index, drive, ctx, family, operation_id)
}

fn session_open_short_probe_after_load(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
    family: MediaFamily,
    operation_id: Uuid,
) -> Result<(), Status> {
    let first = drive.probe_media_readiness(family);
    if first.is_ready() {
        record_session_open_readiness_transition_on_operation(
            index,
            operation_id,
            &ctx,
            "session_open_after_immediate_load",
            &first,
        )?;
        return Ok(());
    }
    if session_open_readiness_should_retry_once(&first) {
        let second = drive.probe_media_readiness(family);
        if second.is_ready() {
            record_session_open_readiness_transition_on_operation(
                index,
                operation_id,
                &ctx,
                "session_open_after_immediate_load",
                &second,
            )?;
            return Ok(());
        }
        return Err(record_session_open_readiness_fence_on_operation(
            index,
            operation_id,
            &ctx,
            "session_open_after_immediate_load",
            &second,
        ));
    }
    Err(record_session_open_readiness_fence_on_operation(
        index,
        operation_id,
        &ctx,
        "session_open_after_immediate_load",
        &first,
    ))
}

fn session_open_media_family(barcode: Option<&str>) -> MediaFamily {
    if barcode
        .and_then(crate::lto_generation_from_voltag)
        .is_some_and(|generation| generation.generation_number() >= 9)
    {
        MediaFamily::Lto9OrLater
    } else {
        MediaFamily::Unknown
    }
}

fn session_open_readiness_requires_immediate_load(
    ctx: &SessionOpenReadinessContext<'_>,
    readiness: &MediaReadiness,
) -> bool {
    match readiness {
        MediaReadiness::BecomingReady { ascq: 0x02, .. } => true,
        MediaReadiness::NoMedium { .. } => ctx.needs_drive_load,
        _ => false,
    }
}

fn session_open_readiness_should_retry_once(readiness: &MediaReadiness) -> bool {
    matches!(
        readiness,
        MediaReadiness::UnitAttention { .. } | MediaReadiness::TargetBusy { .. }
    )
}

fn record_session_open_command_fence_on_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
    opcode: Option<u8>,
    detail: String,
) -> Status {
    if let Err(err) = record_session_open_mechanical_transition(
        index,
        operation_id,
        "session_open_immediate_load",
        "transport_unknown",
        opcode,
        Some(detail.clone()),
    ) {
        return session_open_recording_failure_status(
            ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        );
    }
    Status::failed_precondition(session_open_readiness_error_message(
        ctx,
        operation_id,
        "transport_unknown",
        detail.as_str(),
    ))
}

fn record_session_open_mechanical_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    state: &str,
    opcode: Option<u8>,
    detail: Option<String>,
) -> Result<(), remanence_state::StateError> {
    index
        .record_media_readiness_transition(remanence_state::MediaReadinessTransitionInput {
            operation_id,
            phase: Some(phase.to_string()),
            state: state.to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            last_cdb_opcode: opcode,
            last_sense_raw: None,
            last_sense_key: None,
            last_asc: None,
            last_ascq: None,
            last_host_status: None,
            last_driver_status: None,
            target_status: None,
            transport_class: (state == "transport_unknown").then(|| "unknown".to_string()),
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json: detail.map(|value| session_open_json_detail("detail", value.as_str())),
            quarantine_id: session_open_state_requires_release(state)
                .then(|| session_open_quarantine_id(operation_id)),
        })
        .map(|_| ())
}

fn record_session_open_readiness_fence(
    index: &mut CatalogIndex,
    ctx: &SessionOpenReadinessContext<'_>,
    phase: &str,
    readiness: &MediaReadiness,
) -> Status {
    let operation_id = Uuid::new_v4();
    if let Err(err) = record_session_open_readiness_operation(index, operation_id, ctx) {
        return session_open_recording_failure_status(
            ctx,
            None,
            "record_media_readiness_operation",
            &err,
        );
    }
    record_session_open_readiness_fence_on_operation(index, operation_id, ctx, phase, readiness)
}

fn record_session_open_readiness_fence_on_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
    phase: &str,
    readiness: &MediaReadiness,
) -> Status {
    if let Err(err) =
        record_session_open_readiness_transition(index, operation_id, phase, readiness)
    {
        return session_open_recording_failure_status(
            ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        );
    }
    let state = session_open_readiness_state(readiness);
    Status::failed_precondition(session_open_readiness_error_message(
        ctx,
        operation_id,
        state,
        session_open_readiness_summary(readiness).as_str(),
    ))
}

fn record_session_open_readiness_transition_on_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
    phase: &str,
    readiness: &MediaReadiness,
) -> Result<(), Status> {
    record_session_open_readiness_transition(index, operation_id, phase, readiness).map_err(|err| {
        session_open_recording_failure_status(
            ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        )
    })
}

fn record_session_open_readiness_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    readiness: &MediaReadiness,
) -> Result<(), remanence_state::StateError> {
    record_session_open_readiness_poll_transition(index, operation_id, phase, readiness, false)
}

fn record_session_open_readiness_poll_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    readiness: &MediaReadiness,
    timed_out: bool,
) -> Result<(), remanence_state::StateError> {
    let state = if timed_out {
        "timeout_unknown"
    } else {
        session_open_readiness_state(readiness)
    };
    let (sense_key, asc, ascq, target_status, transport_class, last_error_json, sense_raw) =
        session_open_readiness_evidence(readiness);
    index
        .record_media_readiness_transition(remanence_state::MediaReadinessTransitionInput {
            operation_id,
            phase: Some(phase.to_string()),
            state: state.to_string(),
            dirty_scope: Some(if readiness.is_ready() {
                "none".to_string()
            } else {
                "drive+tape".to_string()
            }),
            last_cdb_opcode: Some(0x00),
            last_sense_raw: sense_raw,
            last_sense_key: sense_key,
            last_asc: asc,
            last_ascq: ascq,
            last_host_status: None,
            last_driver_status: None,
            target_status,
            transport_class,
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json,
            quarantine_id: session_open_state_requires_release(state)
                .then(|| session_open_quarantine_id(operation_id)),
        })
        .map(|_| ())
}

fn record_session_open_readiness_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
) -> Result<(), remanence_state::StateError> {
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: ctx.library_serial.to_string(),
            changer_sg: None,
            drive_element: ctx.bay,
            drive_sg: None,
            drive_serial: ctx.drive_serial.map(ToOwned::to_owned),
            barcode: ctx.barcode.map(ToOwned::to_owned),
            source_slot: ctx.source_slot,
            media_generation: ctx
                .barcode
                .and_then(crate::lto_generation_from_voltag)
                .map(|generation| generation.generation_number()),
            phase: "session_open_short_probe".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .map(|_| ())
}

fn session_open_readiness_state(readiness: &MediaReadiness) -> &'static str {
    match readiness {
        MediaReadiness::Ready => "ready",
        MediaReadiness::BecomingReady {
            media_initializing: true,
            ..
        } => "media_initializing",
        MediaReadiness::BecomingReady { .. } => "becoming_ready",
        MediaReadiness::UnitAttention { .. } => "unit_attention",
        MediaReadiness::TargetBusy { .. } => "target_busy",
        MediaReadiness::ReservationConflict => "reservation_conflict",
        MediaReadiness::TransportUnknown { .. } => "transport_unknown",
        MediaReadiness::NoMedium { .. }
        | MediaReadiness::RepeatedUnitAttention { .. }
        | MediaReadiness::TerminalNotReady { .. }
        | MediaReadiness::CheckCondition { .. }
        | MediaReadiness::UndecodedCheckCondition { .. }
        | MediaReadiness::TaskAborted
        | MediaReadiness::UnexpectedStatus { .. }
        | MediaReadiness::InvalidRequest { .. } => "terminal_error",
    }
}

fn session_open_state_requires_release(state: &str) -> bool {
    matches!(
        state,
        "aborted_unknown"
            | "timeout_unknown"
            | "transport_unknown"
            | "terminal_error"
            | "reservation_conflict"
    )
}

type SessionOpenReadinessEvidence = (
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn session_open_readiness_evidence(readiness: &MediaReadiness) -> SessionOpenReadinessEvidence {
    match readiness {
        MediaReadiness::Ready => (None, None, None, None, None, None, None),
        MediaReadiness::BecomingReady { ascq, .. } => {
            (Some(0x02), Some(0x04), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::NoMedium { ascq } => {
            (Some(0x02), Some(0x3a), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::UnitAttention { asc, ascq }
        | MediaReadiness::RepeatedUnitAttention { asc, ascq } => {
            (Some(0x06), Some(*asc), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::TerminalNotReady { ascq, action } => (
            Some(0x02),
            Some(0x04),
            Some(*ascq),
            None,
            None,
            Some(session_open_json_detail("action", action)),
            None,
        ),
        MediaReadiness::CheckCondition { key, asc, ascq } => {
            (Some(*key), Some(*asc), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::UndecodedCheckCondition { sense } => (
            None,
            None,
            None,
            None,
            None,
            Some(session_open_json_detail(
                "error",
                "undecoded_check_condition",
            )),
            Some(crate::bytes_to_hex(sense)),
        ),
        MediaReadiness::TargetBusy { status } | MediaReadiness::UnexpectedStatus { status } => {
            (None, None, None, Some(*status), None, None, None)
        }
        MediaReadiness::ReservationConflict => (None, None, None, Some(0x18), None, None, None),
        MediaReadiness::TaskAborted => (None, None, None, Some(0x40), None, None, None),
        MediaReadiness::TransportUnknown { detail } => (
            None,
            None,
            None,
            None,
            Some("unknown".to_string()),
            Some(session_open_json_detail("detail", detail)),
            None,
        ),
        MediaReadiness::InvalidRequest { detail } => (
            None,
            None,
            None,
            None,
            None,
            Some(session_open_json_detail("detail", detail)),
            None,
        ),
    }
}

fn session_open_readiness_summary(readiness: &MediaReadiness) -> String {
    match readiness {
        MediaReadiness::Ready => "ready".to_string(),
        MediaReadiness::BecomingReady {
            ascq,
            media_initializing,
        } => {
            if *media_initializing {
                format!("media initializing/calibrating on TEST UNIT READY sense 02/04/{ascq:02x}")
            } else {
                format!("logical unit becoming ready on TEST UNIT READY sense 02/04/{ascq:02x}")
            }
        }
        MediaReadiness::NoMedium { ascq } => {
            format!("drive reports no medium on TEST UNIT READY sense 02/3a/{ascq:02x}")
        }
        MediaReadiness::UnitAttention { asc, ascq } => {
            format!("unit attention during session-open readiness probe 06/{asc:02x}/{ascq:02x}")
        }
        MediaReadiness::RepeatedUnitAttention { asc, ascq } => {
            format!("repeated unit attention during session-open readiness probe 06/{asc:02x}/{ascq:02x}")
        }
        MediaReadiness::TerminalNotReady { ascq, action } => {
            format!("terminal not-ready state {action} on TEST UNIT READY sense 02/04/{ascq:02x}")
        }
        MediaReadiness::CheckCondition { key, asc, ascq } => {
            format!("readiness probe check condition {key:02x}/{asc:02x}/{ascq:02x}")
        }
        MediaReadiness::UndecodedCheckCondition { .. } => {
            "readiness probe returned undecoded check condition".to_string()
        }
        MediaReadiness::TargetBusy { status } => {
            format!("target busy during readiness probe status=0x{status:02x}")
        }
        MediaReadiness::ReservationConflict => {
            "reservation conflict during readiness probe".to_string()
        }
        MediaReadiness::TaskAborted => "task aborted during readiness probe".to_string(),
        MediaReadiness::UnexpectedStatus { status } => {
            format!("unexpected target status during readiness probe status=0x{status:02x}")
        }
        MediaReadiness::TransportUnknown { detail } => {
            format!("transport completion unknown during readiness probe: {detail}")
        }
        MediaReadiness::InvalidRequest { detail } => {
            format!("invalid readiness probe request: {detail}")
        }
    }
}

fn session_open_quarantine_id(operation_id: Uuid) -> String {
    format!("mrq-{operation_id}")
}

fn session_open_json_detail(field: &str, value: &str) -> String {
    format!(
        "{{\"{}\":\"{}\"}}",
        session_open_json_escape(field),
        session_open_json_escape(value)
    )
}

fn session_open_json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", ch as u32);
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn session_open_readiness_error_message(
    ctx: &SessionOpenReadinessContext<'_>,
    operation_id: Uuid,
    state: &str,
    summary: &str,
) -> String {
    format!(
        "{} blocked by media-readiness fence operation={} library={} drive=0x{:04x} barcode={} media_readiness_state={state}: {summary}; leave the cartridge in place and run `rem tape wait-ready --library {} --resume {} --wait --json`",
        ctx.action,
        operation_id,
        ctx.library_serial,
        ctx.bay,
        ctx.barcode.unwrap_or("(unknown)"),
        ctx.library_serial,
        operation_id,
    )
}

fn session_open_recording_failure_status(
    ctx: &SessionOpenReadinessContext<'_>,
    operation_id: Option<Uuid>,
    phase: &str,
    err: &dyn std::fmt::Display,
) -> Status {
    let operation = operation_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "(unrecorded)".to_string());
    Status::failed_precondition(format!(
        "{} blocked by media-readiness recording failure operation={} library={} drive=0x{:04x} barcode={} media_readiness_state=recording_failed: {phase}: {err}; leave the cartridge in place and inspect the catalog DB before retrying",
        ctx.action,
        operation,
        ctx.library_serial,
        ctx.bay,
        ctx.barcode.unwrap_or("(unknown)"),
    ))
}

struct CloseWriteActorInput<'a> {
    index: &'a mut CatalogIndex,
    cfg: &'a WriteOwnerConfig,
    drive: &'a mut DriveHandle,
    drive_uuid: &'a Option<Vec<u8>>,
    drive_serial: &'a Option<String>,
    snapshot_misses: &'a mut u32,
    session_id: Uuid,
    tape_uuid: TapeUuid,
    target_kind: pb::write_session::TargetKind,
    library_serial: &'a str,
    bay: u16,
    objects_committed: u64,
    bytes_committed: u64,
    opened_at_utc: &'a str,
    last_checkpoint_at_utc: Option<&'a str>,
    state: pb::write_session::State,
    append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics,
    checkpointed_objects: &'a [pb::ObjectRecord],
    /// Set only on the abort path, and only when the caller gave a reason.
    abort_reason: Option<String>,
}

fn close_write_actor(input: CloseWriteActorInput<'_>) -> Result<CloseWriteActorReply, Status> {
    let mut diagnostics = CloseWriteActorDiagnostics {
        filemark_write_drain: input.append_commit_diagnostics.filemark_write_drain,
        catalog_journal_fsync: input.append_commit_diagnostics.catalog_journal_fsync,
        ..CloseWriteActorDiagnostics::default()
    };

    let snapshot_started = Instant::now();
    record_session_close_snapshot(
        input.index,
        input.cfg,
        input.drive,
        input.drive_uuid.clone(),
        input.session_id,
        input.tape_uuid,
        input.snapshot_misses,
    );
    diagnostics.drive_snapshot = snapshot_started.elapsed();

    let mut session = session_proto(WriteSessionProtoInput {
        session_id: input.session_id,
        tape_uuid: &input.tape_uuid,
        target_kind: input.target_kind,
        state: input.state,
        objects_committed: input.objects_committed,
        bytes_committed: input.bytes_committed,
        opened_at_utc: input.opened_at_utc,
        last_checkpoint_at_utc: input.last_checkpoint_at_utc,
        drive_element_address: input.bay,
        pending_batch: None,
    });
    session.checkpointed_objects = input.checkpointed_objects.to_vec();
    session.committed_copies = input
        .checkpointed_objects
        .iter()
        .flat_map(|object| object.copies.iter().cloned())
        .collect();
    let audit_started = Instant::now();
    record_session_event(
        input.index,
        input.cfg,
        SessionAuditInput {
            session_id: input.session_id,
            session_kind: "write",
            event: AuditEvent::SessionClosed,
            tape_uuid: Some(input.tape_uuid),
            library_serial: Some(input.library_serial.to_string()),
            drive_bay: Some(input.bay),
            drive_uuid: input.drive_uuid.clone(),
            drive_serial: input.drive_serial.clone(),
            abort_reason: input.abort_reason,
        },
    )?;
    diagnostics.session_audit_projection = audit_started.elapsed();

    Ok(CloseWriteActorReply {
        session,
        diagnostics,
    })
}

/// Mutable authority owned by one drive actor for the lifetime of an open
/// write session. Command handlers borrow this state so each transition's
/// inputs and mutations remain local and visible to the borrow checker.
type DeferredWriteCloseReply = (
    oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    CloseWriteActorReply,
);

struct WriteSessionState<'a> {
    bay: u16,
    index: &'a mut CatalogIndex,
    cfg: &'a WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    drive: &'a mut DriveHandle,
    snapshot_misses: &'a mut u32,
    pool_cfg: TapePoolConfig,
    selected: SelectedTape,
    target_kind: pb::write_session::TargetKind,
    library_serial: String,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    session_id: Uuid,
    tape_uuid: TapeUuid,
    opened_at_utc: String,
    objects_committed: u64,
    bytes_committed: u64,
    last_checkpoint_at_utc: Option<String>,
    checkpoint_lease: remanence_state::FileCheckpointJournalLease,
    durable_checkpoint_records: Vec<remanence_state::CheckpointJournalRecord>,
    parity_session: Option<ParityActorSession>,
    next_batched_append: Option<crate::pool_write::BatchedNoParityAppendContext>,
    checkpoint_ordinal: u64,
    tape_committed_object_count: u64,
    pending_batch: Option<PendingCheckpointBatch>,
    committed_receipts: Vec<pb::ObjectRecord>,
    timer_checkpoint_waiting: Option<Uuid>,
    append_gate: SessionAppendGate,
    append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics,
    deferred_close_reply: Option<DeferredWriteCloseReply>,
}

impl WriteSessionState<'_> {
    /// Dispatch commands until a close transition releases the drive actor.
    ///
    /// A successful close reply is returned to the caller so the outer owner
    /// can drop this state, including its checkpoint lease, before
    /// acknowledging the transition.
    fn run(&mut self, rx: &mut mpsc::Receiver<DriveCommand>) -> Option<DeferredWriteCloseReply> {
        while let Some(command) = rx.blocking_recv() {
            match command {
                command @ DriveCommand::AppendFinish { .. } => {
                    self.handle_append_finish(command);
                }
                command @ DriveCommand::Checkpoint { .. } => {
                    self.handle_checkpoint(command);
                }
                command @ DriveCommand::TimerIdleClose { .. } => {
                    if self.handle_timer_idle_close(command) {
                        break;
                    }
                }
                command @ DriveCommand::Close { .. } => {
                    if self.handle_close(command) {
                        break;
                    }
                }
                command @ DriveCommand::Abort { .. } => {
                    if self.handle_abort(command) {
                        break;
                    }
                }
                command @ DriveCommand::Get { .. } => {
                    self.handle_get(command);
                }
                DriveCommand::OpenWrite { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::OpenRead { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::TapeInventory { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::VerifyTapeIndex { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::FinalizeTape { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::WaitReady { handle, .. } => {
                    handle
                        .publish_failed("write session already active", &[("phase", "admission")]);
                }
                DriveCommand::Unload { reply } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::PollHealth { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::Heartbeat { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "write session already active",
                    )));
                }
                DriveCommand::ReadFile { chunk_tx, .. }
                | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                    let _ = chunk_tx.blocking_send(Err(Status::failed_precondition(
                        "active session is a write session",
                    )));
                }
                DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "active session is a write session",
                    )));
                }
            }
        }
        self.deferred_close_reply.take()
    }

    /// Complete one accepted append and advance provisional checkpoint state.
    fn handle_append_finish(&mut self, command: DriveCommand) {
        let DriveCommand::AppendFinish {
            session_id: requested,
            source,
            archive_path,
            caller_object_id,
            expected_content_sha256,
            expected_object_id,
            input_kind,
            live_write_counter,
            reply,
        } = command
        else {
            unreachable!("append handler received another drive command");
        };
        let cfg = self.cfg;
        let pool_cfg = self.pool_cfg.clone();
        let selected = self.selected.clone();
        let drive_uuid = self.drive_uuid.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            source.remove_completed_path();
            let _ = reply.send(Err(Status::not_found("write session not found")));
            return;
        }
        if let Err(status) = self.append_gate.check() {
            source.remove_completed_path();
            let _ = reply.send(Err(status));
            return;
        }
        // Any accepted append call is session activity, including a
        // catalog idempotency replay; invalidate a prior timer-close.
        self.timer_checkpoint_waiting = None;
        let source_size = source.size_bytes().unwrap_or(0);
        let stream_control = source.stream_control();
        let cleanup_path = match &source {
            crate::WriteObjectSource::Path(path) => Some(path.clone()),
            crate::WriteObjectSource::Streamed(_) => None,
        };
        let current_caller_object_id = caller_object_id.clone();
        if let Some((provisional_index, pending)) = self.pending_batch.as_ref().and_then(|batch| {
            batch
                .objects
                .iter()
                .enumerate()
                .find(|(_, pending)| pending.object.caller_object_id == caller_object_id)
                .map(|(index, pending)| (index, (batch.batch_id, pending)))
        }) {
            let (batch_id, pending) = pending;
            let requested_hash = source.content_sha256();
            source.remove_completed_path();
            match requested_hash {
                Ok(hash) => match validate_provisional_replay_guards(
                    &caller_object_id,
                    pending.input_kind(),
                    pending.object.object_id,
                    pending.object.content_sha256,
                    input_kind,
                    expected_object_id,
                    expected_content_sha256,
                    hash,
                ) {
                    Ok(()) => {
                        let provisional_ordinal = provisional_index as u64 + 1;
                        let record = pending
                            .object
                            .to_written_proto(batch_id, provisional_ordinal);
                        let _ = reply.send(Ok(AppendFinishOutcome {
                            record,
                            replay: true,
                        }));
                    }
                    Err(status) => {
                        let _ = reply.send(Err(status));
                    }
                },
                Err(err) => {
                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                }
            }
            return;
        }
        let request = WriteObjectToPoolRequest {
            pool_id: pool_cfg.id.clone(),
            source,
            archive_path,
            caller_object_id,
            expected_content_sha256,
            expected_object_id,
            representation: crate::PoolWriteRepresentation::Plaintext,
            input_kind,
        };
        let mut write_admission = match cfg.write_admissions.reserve(
            pool_cfg.id.as_str(),
            request.caller_object_id.as_str(),
            request.expected_object_id,
        ) {
            Ok(reservation) => Some(reservation),
            Err(status) => {
                request.source.remove_completed_path();
                let _ = reply.send(Err(status));
                return;
            }
        };
        let append_started = Instant::now();
        let mut parity_raw_write_attempted = false;
        let result = match crate::pool_write::maybe_replay_pool_write(
            self.index, &pool_cfg, &request,
        ) {
            Ok(Some(result)) => Ok(result),
            Ok(None) => {
                if let Some(parity_session) = self.parity_session.as_mut() {
                    let mut raw = DriveHandleRawSink::new(self.drive);
                    match parity_session.journal.as_mut() {
                            Some(parity_journal) => {
                                crate::pool_write::write_batched_parity_to_selected_tape_after_replay_check(
                                    self.index,
                                    &mut raw,
                                    parity_journal,
                                    &mut parity_session.sink_state,
                                    &pool_cfg,
                                    request,
                                    selected.clone(),
                                    &cfg.io_memory,
                                    &mut parity_raw_write_attempted,
                                )
                            }
                            None => Err(PoolWriteError::InvalidInput(
                                "active parity session has no append journal".to_string(),
                            )),
                        }
                } else {
                    let mut sink = DriveHandleSink(self.drive);
                    if let Some(append) = self.next_batched_append.clone() {
                        self.next_batched_append = Some(append.clone());
                        crate::pool_write::write_batched_to_selected_tape_after_replay_check(
                            self.index,
                            &mut sink,
                            &pool_cfg,
                            request,
                            selected.clone(),
                            live_write_counter,
                            append,
                        )
                    } else {
                        Err(PoolWriteError::InvalidInput(
                            "checkpoint append context is unavailable".to_string(),
                        ))
                    }
                }
            }
            Err(err) => Err(err),
        };
        let append_elapsed = append_started.elapsed();
        if let Some(path) = cleanup_path {
            let _ = std::fs::remove_file(path);
        }
        match result {
            Ok(result) => {
                let logical_size = result.object.logical_size_bytes;
                let replay = result.is_replay();
                self.append_commit_diagnostics
                    .accumulate(result.append_commit_diagnostics());
                let response_record = if !replay {
                    if let Some(previous) = self.next_batched_append.as_ref() {
                        let next_context =
                            match crate::pool_write::next_batched_append_context(previous, &result)
                            {
                                Ok(context) => context,
                                Err(err) => {
                                    self.append_gate.record_failure();
                                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                                    return;
                                }
                            };
                        self.next_batched_append = Some(next_context);
                    }
                    let new_batch = self.pending_batch.is_none();
                    let batch = self.pending_batch.get_or_insert_with(|| {
                        PendingCheckpointBatch::new(StdDuration::from_secs(
                            cfg.checkpoint_max_age_seconds,
                        ))
                    });
                    let provisional_ordinal = batch.objects.len() as u64 + 1;
                    let written = result
                        .object
                        .to_written_proto(batch.batch_id, provisional_ordinal);
                    let object_id = result.object.object_id;
                    let batch_id = batch.batch_id;
                    batch.push(
                        logical_size,
                        result,
                        write_admission
                            .take()
                            .expect("new tape write retains its admission through checkpoint"),
                    );
                    let timer_arm_failed = if new_batch {
                        match arm_checkpoint_timer(
                            self.actor_tx.clone(),
                            session_id,
                            batch_id,
                            StdDuration::from_secs(cfg.checkpoint_max_age_seconds),
                        ) {
                            Ok(()) => false,
                            Err(err) => {
                                tracing::error!(
                                    session_id = %session_id,
                                    batch_id = %batch_id,
                                    error = %err,
                                    "checkpoint timer could not start; forcing an immediate barrier"
                                );
                                true
                            }
                        }
                    } else {
                        false
                    };
                    if timer_arm_failed || batch.should_checkpoint(cfg) {
                        let outcome = perform_checkpoint_barrier(
                            self.index,
                            self.drive,
                            &mut self.checkpoint_lease,
                            tape_uuid,
                            &mut self.checkpoint_ordinal,
                            &mut self.tape_committed_object_count,
                            batch,
                            self.parity_session.as_mut(),
                            &selected,
                            &pool_cfg,
                            cfg,
                        );
                        match outcome {
                            Ok(outcome) => {
                                if let Some(previous) = self.next_batched_append.as_ref() {
                                    let checkpoint_context = match crate::pool_write::batched_append_context_after_checkpoint(
                                        previous,
                                        &outcome.checkpoint_record,
                                    ) {
                                        Ok(context) => context,
                                        Err(err) => {
                                            extend_durable_checkpoint_records(
                                                &mut self.durable_checkpoint_records,
                                                &outcome,
                                            );
                                            self.append_gate.record_failure();
                                            self.pending_batch = None;
                                            let _ = reply.send(Err(status_from_pool_write_error(err)));
                                            return;
                                        }
                                    };
                                    self.next_batched_append = Some(checkpoint_context);
                                }
                                extend_durable_checkpoint_records(
                                    &mut self.durable_checkpoint_records,
                                    &outcome,
                                );
                                self.objects_committed =
                                    self.objects_committed.saturating_add(outcome.object_count);
                                self.bytes_committed =
                                    self.bytes_committed.saturating_add(outcome.logical_bytes);
                                self.last_checkpoint_at_utc =
                                    Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                                self.append_commit_diagnostics.accumulate(
                                    crate::pool_write::AppendCommitDiagnostics {
                                        filemark_write_drain: outcome.filemark_drain,
                                        catalog_journal_fsync: outcome.journal_projection,
                                    },
                                );
                                if outcome.sealed_after_write {
                                    self.append_gate.record_sealed();
                                }
                                let committed = outcome
                                    .committed_objects
                                    .iter()
                                    .find(|record| record.object_id == object_id)
                                    .cloned()
                                    .expect("threshold checkpoint returns current object");
                                self.committed_receipts.extend(outcome.committed_objects);
                                self.pending_batch = None;
                                committed
                            }
                            Err(failure) => {
                                let quarantine_identities = failure.requires_identity_quarantine();
                                let status = if failure.journal_durable || failure.fence_handled {
                                    failure.status
                                } else {
                                    fence_failed_checkpoint_batch(
                                        self.index,
                                        cfg,
                                        &selected,
                                        batch,
                                        failure.status,
                                    )
                                };
                                self.append_gate.record_failure();
                                if quarantine_identities {
                                    self.pending_batch
                                        .as_mut()
                                        .expect("checkpoint failure retains its pending batch")
                                        .quarantine_write_admissions_until_restart();
                                }
                                self.pending_batch = None;
                                let _ = reply.send(Err(status));
                                return;
                            }
                        }
                    } else {
                        written
                    }
                } else {
                    if !replay {
                        self.objects_committed = self.objects_committed.saturating_add(1);
                        self.bytes_committed = self.bytes_committed.saturating_add(logical_size);
                        self.last_checkpoint_at_utc =
                            Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                    }
                    result.object.to_proto()
                };
                tracing::info!(
                    target: "remanence_write_diag",
                    phase = "drive_append_total",
                    session_id = %session_id,
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    payload_bytes = source_size,
                    block_size_bytes = selected.block_size,
                    replay,
                    elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                    throughput_mib_s = if replay {
                        0.0
                    } else {
                        crate::diagnostics::mib_per_s(logical_size, append_elapsed)
                    },
                    "remanence_write_diag",
                );
                if replay {
                    retain_replayed_committed_receipt(
                        &mut self.committed_receipts,
                        &response_record,
                    );
                }
                let _ = reply.send(Ok(AppendFinishOutcome {
                    record: response_record,
                    replay,
                }));
            }
            Err(err) => {
                let terminal_trigger = match &err {
                    PoolWriteError::TerminalCloseRequired { .. } => {
                        Some(remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits)
                    }
                    _ => None,
                };
                if let Some(terminal_trigger) =
                    terminal_trigger.filter(|_| self.pending_batch.is_none())
                {
                    let original_error = err.to_string();
                    let terminal_reason = match terminal_trigger {
                        remanence_state::TerminalFinalizationTrigger::NoPendingObjectFits => {
                            "whole-Object terminal-capacity rollover"
                        }
                        _ => "automatic terminal finalization",
                    };
                    if self.durable_checkpoint_records.is_empty() {
                        let status = Status::resource_exhausted(format!(
                            "{terminal_reason} rejected the Object on a fresh tape; no Object was written and the tape may accept a smaller object: {original_error}"
                        ));
                        let _ = reply.send(Err(status));
                        return;
                    }
                    let drive_config = match self.drive.read_config() {
                        Ok(config) => config,
                        Err(error) => {
                            self.append_gate.record_failure();
                            let _ = reply.send(Err(Status::unavailable(format!(
                                "read {terminal_reason} drive config: {error}"
                            ))));
                            return;
                        }
                    };
                    if drive_config.write_protected {
                        self.append_gate.record_failure();
                        let _ = reply.send(Err(Status::failed_precondition(format!(
                            "tape is write-protected and cannot complete {terminal_reason} finalization"
                        ))));
                        return;
                    }
                    let rewritable = matches!(
                        drive_config.worm,
                        remanence_library::WormMediaState::NotWorm
                    );
                    let spec =
                        TerminalFinalizeSpec::automatic(&selected, &pool_cfg, terminal_trigger);
                    let terminal_result = match &selected.parity_config {
                        remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
                            self.index,
                            cfg,
                            self.drive,
                            &mut self.checkpoint_lease,
                            self.durable_checkpoint_records
                                .last()
                                .expect("automatic finalization requires checkpoint authority"),
                            None,
                            &spec,
                            &selected,
                            None,
                            rewritable,
                        ),
                        remanence_parity::ParityConfig::Scheme(_) => {
                            match self
                                .parity_session
                                .as_mut()
                                .and_then(|session| session.journal.take())
                            {
                                Some(parity_journal) => finalize_terminal_with_parity_journal(
                                    self.index,
                                    cfg,
                                    self.drive,
                                    &mut self.checkpoint_lease,
                                    self.durable_checkpoint_records.last().expect(
                                        "automatic finalization requires checkpoint authority",
                                    ),
                                    None,
                                    &spec,
                                    &selected,
                                    None,
                                    parity_journal,
                                    rewritable,
                                ),
                                None => Err(Status::internal(
                                    "automatic parity rollover has no session-owned append journal",
                                )),
                            }
                        }
                    };
                    match terminal_result {
                        Ok(result) => {
                            self.append_gate.record_sealed();
                            if let Some(record) = result.final_record {
                                self.checkpoint_ordinal = record.ordinal;
                                self.durable_checkpoint_records.clear();
                                self.durable_checkpoint_records.push(record);
                                if let Err(error) =
                                    append_tape_sealed_evidence(self.index, cfg, selected.tape_uuid)
                                {
                                    tracing::warn!(%error, %terminal_reason, "failed to append automatic terminal seal evidence");
                                }
                                let _ = reply.send(Err(Status::resource_exhausted(
                                    format!(
                                        "{terminal_reason} was reached before Object tape motion; selected tape was terminally finalized, reopen against the pool to roll placement: {original_error}"
                                    ),
                                )));
                            } else {
                                let _ = reply.send(Err(Status::unavailable(format!(
                                    "{terminal_reason} permanently closed Object admission, but the terminal tail requires recovery: {original_error}"
                                ))));
                            }
                        }
                        Err(error) => {
                            self.append_gate.record_sealed();
                            let _ = reply.send(Err(error));
                        }
                    }
                    return;
                }
                let tape_started = if self.parity_session.is_some() {
                    parity_raw_write_attempted
                } else {
                    stream_control
                        .as_ref()
                        .map(|control| control.tape_started())
                        .unwrap_or(true)
                };
                if tape_started {
                    self.append_gate.record_failure();
                }
                let original_error = err.to_string();
                let mut status = status_from_pool_write_error(err);
                let mut fence_audited_here = false;
                if tape_started {
                    let failed_batch = self.pending_batch.take();
                    if let Some(batch) = failed_batch.as_ref() {
                        status = Status::unavailable(format!(
                            "checkpoint batch {} failed while appending {}; re-send all {} prior WRITTEN objects and the current object: {original_error}",
                            batch.batch_id,
                            current_caller_object_id,
                            batch.objects.len(),
                        ));
                    }
                    if parity_raw_write_attempted {
                        let fenced = fence_failed_parity_raw_write(
                            self.index,
                            cfg,
                            &selected,
                            "append",
                            Some(current_caller_object_id.as_str()),
                            failed_batch.as_ref(),
                            original_error.as_str(),
                            status,
                        );
                        status = fenced.0;
                        // The helper either audits the exact fence it just
                        // persisted or returns an error describing why it
                        // could not. Never fall back to auditing an unrelated
                        // "latest" active fence for this write failure.
                        fence_audited_here = true;
                    } else if let Some(batch) = failed_batch.as_ref() {
                        status = fence_failed_checkpoint_batch(
                            self.index, cfg, &selected, batch, status,
                        );
                        fence_audited_here = true;
                    }
                }
                tracing::info!(
                    target: "remanence_write_diag",
                    phase = "drive_append_total",
                    session_id = %session_id,
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    payload_bytes = source_size,
                    block_size_bytes = selected.block_size,
                    status = "error",
                    error = %status,
                    elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                    throughput_mib_s = crate::diagnostics::mib_per_s(source_size, append_elapsed),
                    "remanence_write_diag",
                );
                if !fence_audited_here {
                    if let Err(audit_err) =
                        append_latest_tape_io_fence_evidence(self.index, cfg, selected.tape_uuid)
                    {
                        tracing::warn!(
                            "failed to append tape-I/O fence evidence after write error: {audit_err}"
                        );
                    }
                }
                record_session_snapshot(
                    self.index,
                    cfg,
                    self.drive,
                    drive_uuid.clone(),
                    session_id,
                    tape_uuid,
                    "append-failure",
                    self.snapshot_misses,
                );
                let _ = reply.send(Err(status));
            }
        }
    }

    /// Commit the pending checkpoint batch, preserving timer-trigger behavior.
    fn handle_checkpoint(&mut self, command: DriveCommand) {
        let DriveCommand::Checkpoint {
            session_id: requested,
            trigger,
            expected_batch_id,
            reply,
        } = command
        else {
            unreachable!("checkpoint handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let pool_cfg = self.pool_cfg.clone();
        let selected = self.selected.clone();
        let target_kind = self.target_kind;
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            if let Some(reply) = reply {
                let _ = reply.send(Err(Status::not_found("write session not found")));
            }
            return;
        }
        let Some(batch) = self.pending_batch.as_ref() else {
            if let Some(reply) = reply {
                let session = session_proto(WriteSessionProtoInput {
                    session_id,
                    tape_uuid: &tape_uuid,
                    target_kind,
                    state: pb::write_session::State::WriteSessionStateCheckpointed,
                    objects_committed: self.objects_committed,
                    bytes_committed: self.bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
                    drive_element_address: bay,
                    pending_batch: None,
                });
                send_checkpoint_actor_reply(reply, session, &mut self.committed_receipts);
            }
            return;
        };
        if expected_batch_id.is_some_and(|expected| expected != batch.batch_id) {
            return;
        }
        let timer_batch_id = batch.batch_id;
        if trigger == CheckpointTrigger::Timer
            && Instant::now() > batch.deadline + StdDuration::from_secs(1)
        {
            let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
            let detail = serde_json::json!({
                "session_id": session_id.to_string(),
                "batch_id": batch.batch_id.to_string(),
                "deadline_overrun_seconds": Instant::now()
                    .saturating_duration_since(batch.deadline)
                    .as_secs(),
            })
            .to_string();
            let _ = self.index.raise_alarm(
                condition_key.as_str(),
                "checkpoint-barrier-overdue",
                "warning",
                Some(detail.as_str()),
            );
        }
        match perform_checkpoint_barrier(
            self.index,
            self.drive,
            &mut self.checkpoint_lease,
            tape_uuid,
            &mut self.checkpoint_ordinal,
            &mut self.tape_committed_object_count,
            batch,
            self.parity_session.as_mut(),
            &selected,
            &pool_cfg,
            cfg,
        ) {
            Ok(outcome) => {
                if let Some(previous) = self.next_batched_append.as_ref() {
                    let checkpoint_context =
                        match crate::pool_write::batched_append_context_after_checkpoint(
                            previous,
                            &outcome.checkpoint_record,
                        ) {
                            Ok(context) => context,
                            Err(err) => {
                                extend_durable_checkpoint_records(
                                    &mut self.durable_checkpoint_records,
                                    &outcome,
                                );
                                self.append_gate.record_failure();
                                self.pending_batch = None;
                                let status = status_from_pool_write_error(err);
                                if let Some(reply) = reply {
                                    let _ = reply.send(Err(status));
                                } else {
                                    tracing::error!(session_id = %session_id, error = %status, "checkpoint committed but next append context failed");
                                }
                                return;
                            }
                        };
                    self.next_batched_append = Some(checkpoint_context);
                }
                extend_durable_checkpoint_records(&mut self.durable_checkpoint_records, &outcome);
                self.objects_committed =
                    self.objects_committed.saturating_add(outcome.object_count);
                self.bytes_committed = self.bytes_committed.saturating_add(outcome.logical_bytes);
                self.last_checkpoint_at_utc =
                    Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                self.append_commit_diagnostics.accumulate(
                    crate::pool_write::AppendCommitDiagnostics {
                        filemark_write_drain: outcome.filemark_drain,
                        catalog_journal_fsync: outcome.journal_projection,
                    },
                );
                if outcome.sealed_after_write {
                    self.append_gate.record_sealed();
                }
                self.pending_batch = None;
                let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
                let _ = self.index.clear_alarm(condition_key.as_str());
                if trigger == CheckpointTrigger::Timer {
                    self.timer_checkpoint_waiting = Some(timer_batch_id);
                    if let Err(err) = arm_checkpoint_idle_close(
                        self.actor_tx.clone(),
                        session_id,
                        timer_batch_id,
                        StdDuration::from_secs(cfg.session_idle_seconds),
                    ) {
                        let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
                        let detail = serde_json::json!({
                            "session_id": session_id.to_string(),
                            "batch_id": timer_batch_id.to_string(),
                            "error": format!("idle close timer spawn failed: {err}"),
                        })
                        .to_string();
                        let _ = self.index.raise_alarm(
                            condition_key.as_str(),
                            "checkpoint-barrier-overdue",
                            "error",
                            Some(detail.as_str()),
                        );
                        tracing::error!(
                            session_id = %session_id,
                            batch_id = %timer_batch_id,
                            error = %err,
                            "checkpoint idle-close timer could not start"
                        );
                    }
                }
                self.committed_receipts.extend(outcome.committed_objects);
                let session = session_proto(WriteSessionProtoInput {
                    session_id,
                    tape_uuid: &tape_uuid,
                    target_kind,
                    state: pb::write_session::State::WriteSessionStateCheckpointed,
                    objects_committed: self.objects_committed,
                    bytes_committed: self.bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
                    drive_element_address: bay,
                    pending_batch: None,
                });
                if let Some(reply) = reply {
                    send_checkpoint_actor_reply(reply, session, &mut self.committed_receipts);
                }
            }
            Err(failure) => {
                let quarantine_identities = failure.requires_identity_quarantine();
                let status = if failure.journal_durable || failure.fence_handled {
                    failure.status
                } else {
                    fence_failed_checkpoint_batch(self.index, cfg, &selected, batch, failure.status)
                };
                self.append_gate.record_failure();
                if quarantine_identities {
                    self.pending_batch
                        .as_mut()
                        .expect("checkpoint failure retains its pending batch")
                        .quarantine_write_admissions_until_restart();
                }
                self.pending_batch = None;
                if let Some(reply) = reply {
                    let _ = reply.send(Err(status));
                } else {
                    tracing::error!(
                        session_id = %session_id,
                        batch_id = %timer_batch_id,
                        error = %status,
                        "timer-fired checkpoint barrier failed"
                    );
                }
            }
        }
    }

    /// Close an idle session only when the timer still names its latest batch.
    fn handle_timer_idle_close(&mut self, command: DriveCommand) -> bool {
        let DriveCommand::TimerIdleClose {
            session_id: requested,
            checkpoint_batch_id,
        } = command
        else {
            unreachable!("idle-close handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let target_kind = self.target_kind;
        let library_serial = self.library_serial.clone();
        let drive_uuid = self.drive_uuid.clone();
        let drive_serial = self.drive_serial.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id
            || self.timer_checkpoint_waiting != Some(checkpoint_batch_id)
            || self.pending_batch.is_some()
        {
            return false;
        }
        let result = close_write_actor(CloseWriteActorInput {
            index: self.index,
            cfg,
            drive: self.drive,
            drive_uuid: &drive_uuid,
            drive_serial: &drive_serial,
            snapshot_misses: self.snapshot_misses,
            session_id,
            tape_uuid,
            target_kind,
            library_serial: library_serial.as_str(),
            bay,
            objects_committed: self.objects_committed,
            bytes_committed: self.bytes_committed,
            opened_at_utc: opened_at_utc.as_str(),
            last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
            state: pb::write_session::State::WriteSessionStateClosed,
            append_commit_diagnostics: self.append_commit_diagnostics,
            checkpointed_objects: &self.committed_receipts,
            abort_reason: None,
        });
        if let Err(err) = result {
            tracing::error!(session_id = %session_id, error = %err, "idle checkpoint close failed");
            return false;
        }
        if let Err(err) = park_timer_closed_session(cfg, session_id) {
            tracing::error!(session_id = %session_id, error = %err, "timer-closed session could not enter idle eviction");
        }
        true
    }

    /// Flush any pending batch and perform a normal session close.
    fn handle_close(&mut self, command: DriveCommand) -> bool {
        let DriveCommand::Close {
            session_id: requested,
            reply,
        } = command
        else {
            unreachable!("close handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let pool_cfg = self.pool_cfg.clone();
        let selected = self.selected.clone();
        let target_kind = self.target_kind;
        let library_serial = self.library_serial.clone();
        let drive_uuid = self.drive_uuid.clone();
        let drive_serial = self.drive_serial.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            let _ = reply.send(Err(Status::not_found("write session not found")));
            return false;
        }
        if let Some(batch) = self.pending_batch.as_ref() {
            match perform_checkpoint_barrier(
                self.index,
                self.drive,
                &mut self.checkpoint_lease,
                tape_uuid,
                &mut self.checkpoint_ordinal,
                &mut self.tape_committed_object_count,
                batch,
                self.parity_session.as_mut(),
                &selected,
                &pool_cfg,
                cfg,
            ) {
                Ok(outcome) => {
                    if let Some(previous) = self.next_batched_append.as_ref() {
                        let checkpoint_context =
                            match crate::pool_write::batched_append_context_after_checkpoint(
                                previous,
                                &outcome.checkpoint_record,
                            ) {
                                Ok(context) => context,
                                Err(err) => {
                                    extend_durable_checkpoint_records(
                                        &mut self.durable_checkpoint_records,
                                        &outcome,
                                    );
                                    self.append_gate.record_failure();
                                    self.pending_batch = None;
                                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                                    return false;
                                }
                            };
                        self.next_batched_append = Some(checkpoint_context);
                    }
                    extend_durable_checkpoint_records(
                        &mut self.durable_checkpoint_records,
                        &outcome,
                    );
                    self.objects_committed =
                        self.objects_committed.saturating_add(outcome.object_count);
                    self.bytes_committed =
                        self.bytes_committed.saturating_add(outcome.logical_bytes);
                    self.last_checkpoint_at_utc =
                        Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                    self.append_commit_diagnostics.accumulate(
                        crate::pool_write::AppendCommitDiagnostics {
                            filemark_write_drain: outcome.filemark_drain,
                            catalog_journal_fsync: outcome.journal_projection,
                        },
                    );
                    if outcome.sealed_after_write {
                        self.append_gate.record_sealed();
                    }
                    self.committed_receipts.extend(outcome.committed_objects);
                    self.pending_batch = None;
                }
                Err(failure) => {
                    let quarantine_identities = failure.requires_identity_quarantine();
                    let status = if failure.journal_durable || failure.fence_handled {
                        failure.status
                    } else {
                        fence_failed_checkpoint_batch(
                            self.index,
                            cfg,
                            &selected,
                            batch,
                            failure.status,
                        )
                    };
                    self.append_gate.record_failure();
                    if quarantine_identities {
                        self.pending_batch
                            .as_mut()
                            .expect("checkpoint failure retains its pending batch")
                            .quarantine_write_admissions_until_restart();
                    }
                    self.pending_batch = None;
                    let _ = reply.send(Err(status));
                    return false;
                }
            }
        }
        let result = close_write_actor(CloseWriteActorInput {
            index: self.index,
            cfg,
            drive: self.drive,
            drive_uuid: &drive_uuid,
            drive_serial: &drive_serial,
            snapshot_misses: self.snapshot_misses,
            session_id,
            tape_uuid,
            target_kind,
            library_serial: library_serial.as_str(),
            bay,
            objects_committed: self.objects_committed,
            bytes_committed: self.bytes_committed,
            opened_at_utc: opened_at_utc.as_str(),
            last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
            state: pb::write_session::State::WriteSessionStateClosed,
            append_commit_diagnostics: self.append_commit_diagnostics,
            checkpointed_objects: &self.committed_receipts,
            abort_reason: None,
        });
        match result {
            Ok(result) => {
                self.deferred_close_reply = Some((reply, result));
                true
            }
            Err(err) => {
                let _ = reply.send(Err(err));
                false
            }
        }
    }

    /// Close the actor in the aborted state without committing new work.
    fn handle_abort(&mut self, command: DriveCommand) -> bool {
        let DriveCommand::Abort {
            session_id: requested,
            reason,
            reply,
        } = command
        else {
            unreachable!("abort handler received another drive command");
        };
        let bay = self.bay;
        let cfg = self.cfg;
        let target_kind = self.target_kind;
        let library_serial = self.library_serial.clone();
        let drive_uuid = self.drive_uuid.clone();
        let drive_serial = self.drive_serial.clone();
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        if requested != session_id {
            let _ = reply.send(Err(Status::not_found("write session not found")));
            return false;
        }
        let result = close_write_actor(CloseWriteActorInput {
            index: self.index,
            cfg,
            drive: self.drive,
            drive_uuid: &drive_uuid,
            drive_serial: &drive_serial,
            snapshot_misses: self.snapshot_misses,
            session_id,
            tape_uuid,
            target_kind,
            library_serial: library_serial.as_str(),
            bay,
            objects_committed: self.objects_committed,
            bytes_committed: self.bytes_committed,
            opened_at_utc: opened_at_utc.as_str(),
            last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
            state: pb::write_session::State::WriteSessionStateAborted,
            append_commit_diagnostics: self.append_commit_diagnostics,
            checkpointed_objects: &self.committed_receipts,
            abort_reason: reason,
        });
        match result {
            Ok(result) => {
                self.deferred_close_reply = Some((reply, result));
                true
            }
            Err(err) => {
                let _ = reply.send(Err(err));
                false
            }
        }
    }

    /// Report the current write-session projection without mutating it.
    fn handle_get(&mut self, command: DriveCommand) {
        let DriveCommand::Get {
            session_id: requested,
            reply,
        } = command
        else {
            unreachable!("get handler received another drive command");
        };
        let bay = self.bay;
        let target_kind = self.target_kind;
        let session_id = self.session_id;
        let tape_uuid = self.tape_uuid;
        let opened_at_utc = self.opened_at_utc.clone();
        let status = if requested == session_id {
            Ok(session_proto(WriteSessionProtoInput {
                session_id,
                tape_uuid: &tape_uuid,
                target_kind,
                state: if self.pending_batch.is_none() && self.last_checkpoint_at_utc.is_some() {
                    pb::write_session::State::WriteSessionStateCheckpointed
                } else {
                    pb::write_session::State::WriteSessionStateOpen
                },
                objects_committed: self.objects_committed,
                bytes_committed: self.bytes_committed,
                opened_at_utc: opened_at_utc.as_str(),
                last_checkpoint_at_utc: self.last_checkpoint_at_utc.as_deref(),
                drive_element_address: bay,
                pending_batch: self.pending_batch.as_ref(),
            }))
        } else {
            Err(Status::not_found("write session not found"))
        };
        let _ = reply.send(status);
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_drive_open_write(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    request: OpenWriteActorRequest,
) {
    let OpenWriteActorRequest {
        pool_cfg,
        selected,
        target_kind,
        needs_drive_load,
        library_serial,
        barcode,
        source_slot,
        drive_uuid,
        drive_serial,
        reply,
    } = request;
    let actor_open_started = Instant::now();
    let session_id = Uuid::new_v4();
    let tape_uuid = selected.tape_uuid;
    if let Err(status) = session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode.as_deref(),
        "open write session",
    ) {
        let _ = reply.send(Err(status));
        return;
    }

    // Recovery authority must be complete and mutually consistent before any
    // load, rewind, locate, drive-mode change, or write preparation. Keep both
    // journal leases across the later media work so another writer cannot
    // change the validated prefix in between.
    let checkpoint_journal = match remanence_state::FileCheckpointJournal::open(
        cfg.checkpoint_journal_dir.as_path(),
        tape_uuid,
    ) {
        Ok(journal) => journal,
        Err(err) => {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    };
    let pending_terminal_intent = match checkpoint_journal.terminal_finalization_intent() {
        Ok(intent) => intent,
        Err(err) => {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    };
    let (mut checkpoint_lease, pending_terminal_intent) = if pending_terminal_intent.is_some() {
        let lease = match checkpoint_journal.acquire_exclusive_for_terminal_recovery() {
            Ok(lease) => lease,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
        let intent = match lease.terminal_finalization_intent() {
            Ok(intent) => intent,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
        (lease, intent)
    } else {
        let lease = match checkpoint_journal.acquire_exclusive() {
            Ok(lease) => lease,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
        (lease, None)
    };
    let durable_checkpoint_records =
        match project_checkpoint_authority_bounded(index, &checkpoint_lease) {
            Ok(records) => records,
            Err(err) => {
                let _ = reply.send(Err(status_from_state_error(err)));
                return;
            }
        };
    if durable_checkpoint_records
        .last()
        .is_some_and(|record| record.sealed_after_write)
    {
        let _ = reply.send(Err(Status::failed_precondition(format!(
            "checkpoint authority records tape {} as terminal and sealed",
            Uuid::from_bytes(tape_uuid)
        ))));
        return;
    }
    if pending_terminal_intent.is_none() {
        if let Err(err) = crate::pool_write::ensure_empty_checkpoint_matches_catalog_freshness(
            index,
            &selected,
            &durable_checkpoint_records,
        ) {
            let _ = reply.send(Err(status_from_pool_write_error(err)));
            return;
        }
        if let Err(err) = crate::pool_write::ensure_selected_tape_accepts_session_write(
            index, &pool_cfg, &selected,
        ) {
            let _ = reply.send(Err(status_from_pool_write_error(err)));
            return;
        }
    }
    let parity_authority = if pending_terminal_intent.is_none()
        && matches!(
            selected.parity_config,
            remanence_parity::ParityConfig::Scheme(_)
        ) {
        match validate_parity_actor_authority(
            cfg,
            &selected,
            &checkpoint_lease,
            &durable_checkpoint_records,
        ) {
            Ok(authority) => Some(authority),
            Err(status) => {
                let _ = reply.send(Err(status));
                return;
            }
        }
    } else {
        None
    };

    if let Err(status) = session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "open write session",
            bay,
            library_serial: library_serial.as_str(),
            barcode: barcode.as_deref(),
            source_slot,
            drive_serial: drive_serial.as_deref(),
            needs_drive_load,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }

    let media_policy = if pending_terminal_intent.is_some() {
        WriteMediaPolicy::TerminalAppend
    } else {
        WriteMediaPolicy::RewritableObject
    };
    if let Err(status) = prepare_drive_for_write(
        drive,
        &tape_uuid,
        selected.block_size,
        session_id,
        media_policy,
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    // Load-time wrap-map harvest + fence install, one act (design
    // §6.5): only when this open freshly mounted the cartridge — a
    // session on an already-seated tape is the same load, and
    // re-issuing REOWP mid-load would return a snapshot a write may
    // already have invalidated. Runs after identity is proven (the
    // fence binds this tape_uuid) and before any media-modifying CDB
    // of the load can dispatch.
    if needs_drive_load {
        run_load_calibration_harvest(index, drive, cfg, &tape_uuid, barcode.as_deref());
    }
    if let Some(intent) = pending_terminal_intent {
        if intent.manual.is_some() {
            let _ = reply.send(Err(Status::failed_precondition(format!(
                "tape {} has an operator-owned terminal finalization in progress; retry FinalizeTape with its original idempotency key",
                Uuid::from_bytes(tape_uuid)
            ))));
            return;
        }
        let drive_config = match drive.read_config() {
            Ok(config) => config,
            Err(error) => {
                let _ = reply.send(Err(Status::unavailable(format!(
                    "read automatic terminal-recovery drive config: {error}"
                ))));
                return;
            }
        };
        if drive_config.write_protected {
            let _ = reply.send(Err(Status::failed_precondition(
                "tape is write-protected and automatic terminal recovery cannot continue",
            )));
            return;
        }
        let rewritable = matches!(
            drive_config.worm,
            remanence_library::WormMediaState::NotWorm
        );
        let spec = TerminalFinalizeSpec::resume(&intent, selected.block_size, &pool_cfg);
        if let Err(error) =
            index.project_terminal_finalization(TerminalFinalizationProjectionInput {
                tape_uuid,
                trigger: intent.trigger,
                operation_id: None,
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
        {
            let _ = reply.send(Err(status_from_state_error(error)));
            return;
        }
        let result = match &selected.parity_config {
            remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
                index,
                cfg,
                drive,
                &mut checkpoint_lease,
                durable_checkpoint_records
                    .last()
                    .expect("pending terminal intent requires checkpoint authority"),
                Some(intent),
                &spec,
                &selected,
                None,
                rewritable,
            ),
            remanence_parity::ParityConfig::Scheme(_) => finalize_terminal_with_parity(
                index,
                cfg,
                drive,
                &mut checkpoint_lease,
                durable_checkpoint_records
                    .last()
                    .expect("pending terminal intent requires checkpoint authority"),
                Some(intent),
                &spec,
                &selected,
                None,
                rewritable,
            ),
        };
        match result {
            Ok(result) if result.final_record.is_some() => {
                if let Err(error) = append_tape_sealed_evidence(index, cfg, tape_uuid) {
                    tracing::warn!(error = %error, "failed to append recovered tape sealing evidence");
                }
                let _ = reply.send(Err(Status::failed_precondition(format!(
                    "tape {} completed terminal finalization during open recovery and cannot accept Objects",
                    Uuid::from_bytes(tape_uuid)
                ))));
            }
            Ok(_) => {
                let _ = reply.send(Err(Status::unavailable(format!(
                    "tape {} still requires terminal recovery and cannot accept Objects",
                    Uuid::from_bytes(tape_uuid)
                ))));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
        return;
    }
    tracing::info!(
        target: "remanence_write_diag",
        phase = "drive_open_actor",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(tape_uuid),
        needs_drive_load,
        block_size_bytes = selected.block_size,
        elapsed_ms = crate::diagnostics::duration_ms(actor_open_started.elapsed()),
        "remanence_write_diag",
    );

    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let objects_committed = 0u64;
    let bytes_committed = 0u64;
    let last_checkpoint_at_utc = None;
    let last_durable_checkpoint = durable_checkpoint_records.last().cloned();
    let parity_session = if let Some(authority) = parity_authority {
        match open_parity_actor_session(
            index,
            drive,
            cfg,
            &selected,
            &durable_checkpoint_records,
            authority,
        ) {
            Ok(session) => Some(session),
            Err(status) => {
                let _ = reply.send(Err(status));
                return;
            }
        }
    } else {
        None
    };
    let next_batched_append = if parity_session.is_none() {
        match crate::pool_write::first_batched_append_context(
            index,
            &selected,
            &durable_checkpoint_records,
        ) {
            Ok(context) => Some(context),
            Err(err) => {
                let _ = reply.send(Err(status_from_pool_write_error(err)));
                return;
            }
        }
    } else {
        None
    };
    let checkpoint_ordinal = last_durable_checkpoint
        .as_ref()
        .map_or(0, |record| record.ordinal);
    let tape_committed_object_count = last_durable_checkpoint
        .as_ref()
        .map_or(0, |record| record.committed_object_count);
    let pending_batch: Option<PendingCheckpointBatch> = None;
    let committed_receipts = Vec::<pb::ObjectRecord>::new();
    let timer_checkpoint_waiting: Option<Uuid> = None;
    if let Err(status) = record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id,
            session_kind: "write",
            event: AuditEvent::SessionOpened,
            tape_uuid: Some(tape_uuid),
            library_serial: Some(library_serial.clone()),
            drive_bay: Some(bay),
            drive_uuid: drive_uuid.clone(),
            drive_serial: drive_serial.clone(),
            abort_reason: None,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    let open_reply = session_proto(WriteSessionProtoInput {
        session_id,
        tape_uuid: &tape_uuid,
        target_kind,
        state: pb::write_session::State::WriteSessionStateOpen,
        objects_committed,
        bytes_committed,
        opened_at_utc: opened_at_utc.as_str(),
        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
        drive_element_address: bay,
        pending_batch: None,
    });
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    let mut session_state = WriteSessionState {
        bay,
        index,
        cfg,
        actor_tx,
        drive,
        snapshot_misses,
        pool_cfg,
        selected,
        target_kind,
        library_serial,
        drive_uuid,
        drive_serial,
        session_id,
        tape_uuid,
        opened_at_utc,
        objects_committed,
        bytes_committed,
        last_checkpoint_at_utc,
        checkpoint_lease,
        durable_checkpoint_records,
        parity_session,
        next_batched_append,
        checkpoint_ordinal,
        tape_committed_object_count,
        pending_batch,
        committed_receipts,
        timer_checkpoint_waiting,
        append_gate: SessionAppendGate::default(),
        append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics::default(),
        deferred_close_reply: None,
    };
    let deferred_close_reply = session_state.run(rx);
    drop(session_state);
    if let Some((reply, result)) = deferred_close_reply {
        let _ = reply.send(Ok(result));
    }
}

fn append_latest_tape_io_fence_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    let fence = index
        .list_active_tape_io_fences()
        .map_err(crate::status_from_state_error)?
        .into_iter()
        .find(|fence| fence.tape_uuid.as_slice() == tape_uuid);
    let Some(fence) = fence else {
        return Ok(());
    };
    append_tape_io_fence_evidence(index, cfg, &fence, AuditEvent::TapeIoFenceRaised)
}

fn append_tape_sealed_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    crate::ensure_tape_sealed_audit(
        index,
        cfg.audit_dir.as_path(),
        &cfg.audit_append_lock,
        tape_uuid,
    )
}

fn append_tape_io_fence_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    fence: &TapeIoFenceRecord,
    event: AuditEvent,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(fence.tape_uuid.clone()),
        ),
        (
            "quarantine_id".to_string(),
            CborValue::Text(fence.quarantine_id.clone()),
        ),
        ("reason".to_string(), CborValue::Text(fence.reason.clone())),
    ]);
    insert_optional_audit_text(&mut detail, "barcode", fence.barcode.as_ref());
    insert_optional_audit_text(&mut detail, "evidence_json", fence.evidence_json.as_ref());
    insert_optional_audit_text(&mut detail, "release_ack", fence.release_ack.as_ref());
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event,
            subject_kind: "tape_io_fence",
            subject_id: Some(fence.quarantine_id.clone()),
            detail,
        },
    )?;
    Ok(())
}

/// Run the load-time wrap-map harvest for a freshly mounted cartridge
/// (design §6.5): install the volume's real media-write fence and
/// harvest REOWP in one act, via
/// `calibration::harvest_and_install_calibration`. Ordering is an
/// optimisation, so no harvest outcome fails the session open; the
/// result is durably recorded in the calibration store and logged
/// here. The one exception is enforced elsewhere: once the fence is
/// installed, a write whose durable epoch advance cannot be recorded
/// is refused by the media gate itself.
fn run_load_calibration_harvest(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    tape_uuid: &TapeUuid,
    barcode: Option<&str>,
) {
    use crate::calibration::HarvestOutcome;
    let outcome = crate::calibration::harvest_and_install_calibration(
        drive,
        index,
        &cfg.calibration_store,
        *tape_uuid,
        barcode,
    );
    let tape = Uuid::from_bytes(*tape_uuid);
    match outcome {
        HarvestOutcome::Calibrated {
            write_epoch,
            calibration_generation,
            wrap_count,
        } => tracing::info!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            write_epoch,
            calibration_generation,
            wrap_count,
            "wrap-map harvest calibrated the volume"
        ),
        HarvestOutcome::UnsupportedFormat {
            calibration_generation,
            detail,
        } => tracing::info!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            calibration_generation,
            detail = detail.as_str(),
            "wrap-map harvest: unsupported format for this load"
        ),
        HarvestOutcome::Uncalibrated {
            calibration_generation,
            detail,
        } => tracing::warn!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            calibration_generation,
            detail = detail.as_str(),
            "wrap-map harvest left the volume uncalibrated"
        ),
        HarvestOutcome::StoreUnavailable { detail } => tracing::warn!(
            target: "remanence_calibration",
            tape_uuid = %tape,
            detail = detail.as_str(),
            "calibration store unavailable during load harvest; volume state unchanged"
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMediaPolicy {
    RewritableObject,
    TerminalAppend,
}

fn validate_write_media_policy(
    current_cfg: TapeConfig,
    media_policy: WriteMediaPolicy,
) -> Result<(), Status> {
    if current_cfg.write_protected {
        return Err(Status::failed_precondition("tape is write-protected"));
    }
    if media_policy == WriteMediaPolicy::TerminalAppend {
        return Ok(());
    }
    match current_cfg.worm {
        remanence_library::WormMediaState::NotWorm => Ok(()),
        remanence_library::WormMediaState::Worm => Err(Status::failed_precondition(
            "ordinary Object writes require rewritable media; a WORM tape cannot replace an interrupted uncommitted Object tail",
        )),
        remanence_library::WormMediaState::Unknown => Err(Status::failed_precondition(
            "ordinary Object writes require media positively identified as rewritable; the loaded tape's WORM state is unknown",
        )),
    }
}

fn prepare_drive_for_write(
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    block_size: u32,
    session_id: Uuid,
    media_policy: WriteMediaPolicy,
) -> Result<(), Status> {
    let prepare_started = Instant::now();
    drive
        .reverify_invalidated_state()
        .map_err(|err| Status::failed_precondition(format!("reverify drive state: {err}")))?;
    let rewind_verify_started = Instant::now();
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before write verify: {err}")))?;
    let rewind_verify_elapsed = rewind_verify_started.elapsed();
    let verify_started = Instant::now();
    {
        let mut source = DriveHandleSource(drive);
        verify_tape_identity(&mut source, tape_uuid)
            .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    }
    let verify_elapsed = verify_started.elapsed();
    let rewind_write_started = Instant::now();
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before write: {err}")))?;
    let rewind_write_elapsed = rewind_write_started.elapsed();
    let read_config_started = Instant::now();
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config before write: {err}")))?;
    let read_config_elapsed = read_config_started.elapsed();
    validate_write_media_policy(current_cfg, media_policy)?;
    let target_cfg = fixed_no_compression_config(current_cfg, block_size);
    let write_config_started = Instant::now();
    drive
        .write_config(target_cfg)
        .map_err(|err| Status::internal(format!("set fixed-block config: {err}")))?;
    let staging_ring_buffers = drive.staging_ring_buffers();
    let effective_batch_blocks = drive.requested_write_batch_blocks().min(
        drive
            .sg_reserved_size_bytes()
            .checked_div(block_size.max(1))
            .unwrap_or(1)
            .max(1),
    );
    let effective_ring_bytes = u64::from(staging_ring_buffers)
        .saturating_mul(u64::from(effective_batch_blocks))
        .saturating_mul(u64::from(block_size));
    tracing::info!(
        target: "remanence_write_diag",
        phase = "drive_prepare",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(*tape_uuid),
        selected_block_size_bytes = block_size,
        prior_block_size = ?current_cfg.block_size,
        prior_compression = current_cfg.compression,
        target_block_size = ?target_cfg.block_size,
        target_compression = target_cfg.compression,
        staging_ring_buffers,
        effective_batch_blocks,
        effective_ring_bytes,
        rewind_verify_ms = crate::diagnostics::duration_ms(rewind_verify_elapsed),
        verify_bootstrap_ms = crate::diagnostics::duration_ms(verify_elapsed),
        rewind_write_ms = crate::diagnostics::duration_ms(rewind_write_elapsed),
        read_config_ms = crate::diagnostics::duration_ms(read_config_elapsed),
        write_config_ms = crate::diagnostics::duration_ms(write_config_started.elapsed()),
        elapsed_ms = crate::diagnostics::duration_ms(prepare_started.elapsed()),
        "remanence_write_diag",
    );
    Ok(())
}

fn handle_drive_finalize_tape(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    mounted: ManualFinalizeTapeMountRequest,
) -> Result<ManualFinalizeTapeActorReply, Status> {
    let ManualFinalizeTapeMountRequest {
        request,
        needs_drive_load,
        library_serial,
        barcode,
        source_slot,
        drive_uuid,
        drive_serial,
    } = mounted;
    validate_manual_finalize_owned_request(index, &request)?;
    session_open_reject_tape_io_fences(
        index,
        &request.tape_uuid,
        barcode.as_deref(),
        "finalize tape",
    )?;

    let checkpoint_journal = remanence_state::FileCheckpointJournal::open(
        cfg.checkpoint_journal_dir.as_path(),
        request.tape_uuid,
    )
    .map_err(crate::status_from_state_error)?;
    let pending_intent = checkpoint_journal
        .terminal_finalization_intent()
        .map_err(crate::status_from_state_error)?;
    let (mut checkpoint_lease, existing_intent) = if pending_intent.is_some() {
        let lease = checkpoint_journal
            .acquire_exclusive_for_terminal_recovery()
            .map_err(crate::status_from_state_error)?;
        let intent = lease
            .terminal_finalization_intent()
            .map_err(crate::status_from_state_error)?;
        (lease, intent)
    } else {
        let lease = checkpoint_journal
            .acquire_exclusive()
            .map_err(crate::status_from_state_error)?;
        (lease, None)
    };
    let records = project_checkpoint_authority_bounded(index, &checkpoint_lease)
        .map_err(crate::status_from_state_error)?;
    if records
        .last()
        .is_some_and(|record| record.sealed_after_write)
    {
        let sealed = records
            .last()
            .expect("sealed checkpoint predicate requires a final record");
        let completion = sealed.terminal_finalization.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "sealed checkpoint authority has no terminal-finalization completion",
            )
        })?;
        crate::ensure_tape_sealed_audit(
            index,
            cfg.audit_dir.as_path(),
            &cfg.audit_append_lock,
            request.tape_uuid,
        )?;
        crate::ensure_manual_finalize_finished_audit(
            index,
            cfg.audit_dir.as_path(),
            &cfg.audit_append_lock,
            request.tape_uuid,
            completion,
        )?;
        let projection = index
            .terminal_finalization(&request.tape_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| {
                Status::failed_precondition(
                    "sealed checkpoint authority has no terminal-finalization projection",
                )
            })?;
        if projection.operation_id != Some(request.candidate_operation_id) {
            return Err(Status::already_exists(
                "tape was finalized by a different operation",
            ));
        }
        return Ok(ManualFinalizeTapeActorReply {
            operation_id: request.candidate_operation_id,
            projection,
        });
    }
    if records.is_empty() {
        return Err(Status::failed_precondition(
            "manual finalization requires at least one durable checkpoint",
        ));
    }
    let accepted_intent = existing_intent.as_ref().ok_or_else(|| {
        Status::failed_precondition(
            "FinalizeTape drive dispatch is missing its durable BeforeReplicaA intent",
        )
    })?;
    validate_manual_finalize_intent(&request, accepted_intent)?;
    require_manual_finalize_preflight_binding(index, &request)?;
    session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "finalize tape",
            bay,
            library_serial: library_serial.as_str(),
            barcode: barcode.as_deref(),
            source_slot,
            drive_serial: drive_serial.as_deref(),
            needs_drive_load,
        },
    )?;
    prepare_drive_for_write(
        drive,
        &request.tape_uuid,
        request.block_size,
        request.candidate_operation_id,
        WriteMediaPolicy::TerminalAppend,
    )?;
    if needs_drive_load {
        run_load_calibration_harvest(index, drive, cfg, &request.tape_uuid, barcode.as_deref());
    }
    let drive_config = drive
        .read_config()
        .map_err(|error| Status::unavailable(format!("read finalization drive config: {error}")))?;
    if drive_config.write_protected {
        return Err(Status::failed_precondition(
            "tape is write-protected and cannot be finalized",
        ));
    }
    let rewritable = matches!(
        drive_config.worm,
        remanence_library::WormMediaState::NotWorm
    );
    let selected = SelectedTape {
        pool_id: request
            .expected_pool_id
            .clone()
            .unwrap_or_else(|| "(unpooled)".to_string()),
        tape_uuid: request.tape_uuid,
        block_size: request.block_size,
        parity_config: request.parity_config.clone(),
    };
    let spec = TerminalFinalizeSpec::operator(&request);

    let result = match request.parity_config {
        remanence_parity::ParityConfig::None => finalize_terminal_no_parity(
            index,
            cfg,
            drive,
            &mut checkpoint_lease,
            records
                .last()
                .expect("manual finalization requires checkpoint authority"),
            existing_intent,
            &spec,
            &selected,
            Some(&request),
            rewritable,
        ),
        remanence_parity::ParityConfig::Scheme(_) => finalize_terminal_with_parity(
            index,
            cfg,
            drive,
            &mut checkpoint_lease,
            records
                .last()
                .expect("manual finalization requires checkpoint authority"),
            existing_intent,
            &spec,
            &selected,
            Some(&request),
            rewritable,
        ),
    };

    record_session_close_snapshot(
        index,
        cfg,
        drive,
        drive_uuid,
        request.candidate_operation_id,
        request.tape_uuid,
        snapshot_misses,
    );
    result.map(|result| ManualFinalizeTapeActorReply {
        operation_id: request.candidate_operation_id,
        projection: result.projection,
    })
}

fn validate_manual_finalize_intent(
    request: &ManualFinalizeTapeActorRequest,
    intent: &remanence_state::TerminalFinalizationIntent,
) -> Result<(), Status> {
    let manual = intent.manual.as_ref().ok_or_else(|| {
        Status::failed_precondition("pending terminal finalization is not an operator close-out")
    })?;
    if intent.tape_uuid != request.tape_uuid
        || intent.trigger != remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        || manual.operation_id != *request.candidate_operation_id.as_bytes()
        || manual.operation_kind != FINALIZE_TAPE_OPERATION_KIND
        || manual.actor_fingerprint != request.actor_fingerprint
        || manual.idempotency_key != *request.idempotency_key.as_bytes()
        || manual.request_fingerprint != request.request_fingerprint
        || manual.assigned_pool_id != request.expected_pool_id
        || manual.expected_pool_id != request.expected_pool_id
        || manual.assignment_generation != request.assignment_generation
        || manual.reason != request.reason
    {
        return Err(Status::already_exists(
            "pending terminal finalization belongs to a different exact request",
        ));
    }
    Ok(())
}

fn authorize_terminal_intent_capacity(
    index: &CatalogIndex,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    intent: &remanence_state::TerminalFinalizationIntent,
    counts: remanence_parity::TapeIndexReplicaCounts,
) -> Result<(), Status> {
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        // Capacity admission governs future tape motion. Once replica C is
        // durably proved, the reserved tail is already on media; reapplying a
        // changed cap or watermark here could only obstruct host metadata
        // completion and cannot protect tape capacity.
        return Ok(());
    }
    crate::pool_write::authorize_terminal_close_only_plan(
        index,
        spec.pool_config.as_ref(),
        selected,
        intent.layout.components[0].start_lba,
        counts,
        intent.layout.expected_eod_lba,
    )
    .map_err(status_from_pool_write_error)?;
    Ok(())
}

fn plan_terminal_prefix_without_motion(
    index: &CatalogIndex,
    selected: &SelectedTape,
    checkpoint: &remanence_state::FileCheckpointJournalLease,
    checkpoint_journal_dir: &Path,
    previous: &remanence_state::CheckpointJournalRecord,
    request: &ManualFinalizeTapeActorRequest,
    spec: &TerminalFinalizeSpec,
) -> Result<
    (
        remanence_state::TerminalFinalizationIntent,
        TerminalTripleWritePlan,
    ),
    Status,
> {
    let scheme = match &request.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity terminal-prefix preflight called for parity-off tape",
            ));
        }
    };
    let journal_dir = checkpoint_journal_dir.parent().ok_or_else(|| {
        Status::internal("checkpoint journal directory has no parent for the parity journal")
    })?;
    let journal_path = journal_dir.join(format!(
        "{}.remjournal",
        crate::bytes_to_hex(&request.tape_uuid)
    ));
    let journal = FileTapeFileJournal::open(
        journal_path,
        request.tape_uuid,
        request.block_size,
        scheme.clone(),
    )
    .map_err(|error| Status::failed_precondition(format!("open parity journal: {error}")))?;

    // Prove the checkpoint/parity bijection and reject every orphan before a
    // drive or changer handle is even selected. The close seed retains only
    // one length-bounded bundle plus epoch-directory metadata.
    let base_authority = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed(
        checkpoint, &journal,
    )
    .map_err(crate::status_from_state_error)?;
    if base_authority
        .summary()
        .scope
        .covered_prefix_tape_file_count
        == 0
    {
        return Err(Status::failed_precondition(
            "checkpointed parity tape has no replayable committed prefix",
        ));
    }
    let snapshot = journal
        .committed_snapshot_bounded()
        .map_err(|error| Status::failed_precondition(format!("replay parity journal: {error}")))?;
    let prefix = remanence_parity::plan_checkpointed_terminal_index_close(&snapshot)
        .map_err(|error| status_from_parity_error(&error, error.to_string()))?;
    let expected_start_file = previous.next_tape_file_number;
    if prefix.start_tape_file_number != expected_start_file || prefix.start_lba != previous.eod_lba
    {
        return Err(Status::failed_precondition(format!(
            "bounded terminal close seed starts at tape file {}/LBA {}, expected {expected_start_file}/{}",
            prefix.start_tape_file_number, prefix.start_lba, previous.eod_lba
        )));
    }
    let persisted = remanence_state::TerminalFinalizationPrefixPlan::from(&prefix);
    let source = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
        checkpoint,
        &journal,
        &persisted,
    )
    .map_err(crate::status_from_state_error)?;
    let mut source = source;
    build_new_terminal_plan(
        index,
        selected,
        spec,
        previous,
        &mut source,
        TerminalPlanPosition {
            first_tape_file_number: prefix.tail_start_tape_file_number,
            first_start_lba: prefix.tail_start_lba,
            terminal_prefix: Some(&prefix),
        },
    )
}

#[derive(Clone, Copy)]
struct TerminalPlanPosition<'a> {
    first_tape_file_number: u64,
    first_start_lba: u64,
    terminal_prefix: Option<&'a TerminalPrefixPlan>,
}

fn build_new_terminal_plan(
    index: &CatalogIndex,
    selected: &SelectedTape,
    spec: &TerminalFinalizeSpec,
    previous: &remanence_state::CheckpointJournalRecord,
    source: &mut remanence_state::CheckpointTerminalIndexRecordSource<'_>,
    position: TerminalPlanPosition<'_>,
) -> Result<
    (
        remanence_state::TerminalFinalizationIntent,
        TerminalTripleWritePlan,
    ),
    Status,
> {
    let TerminalPlanPosition {
        first_tape_file_number,
        first_start_lba,
        terminal_prefix,
    } = position;
    let summary = source.summary();
    let replica =
        remanence_parity::checked_tape_index_replica_layout(spec.block_size, summary.counts)
            .map_err(|error| {
                Status::failed_precondition(format!("plan terminal replica: {error}"))
            })?;
    let separation_records = remanence_parity::index_separation_records(
        spec.block_size,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .map_err(|error| Status::failed_precondition(format!("plan terminal separation: {error}")))?;
    let layout = remanence_parity::TerminalTailLayout::new(
        0,
        spec.block_size,
        first_tape_file_number,
        first_start_lba,
        replica.replica_record_count,
        separation_records,
    )
    .map_err(|error| Status::failed_precondition(format!("plan terminal layout: {error}")))?;
    crate::pool_write::authorize_terminal_close_only_plan(
        index,
        spec.pool_config.as_ref(),
        selected,
        first_start_lba,
        summary.counts,
        layout.expected_eod_lba,
    )
    .map_err(status_from_pool_write_error)?;
    let edition_sequence = previous
        .ordinal
        .checked_add(1)
        .ok_or_else(|| Status::failed_precondition("terminal edition sequence overflows u64"))?;
    let edition_id = Uuid::new_v4();
    let writer_version = format!("remanence-api/{}", env!("CARGO_PKG_VERSION"));
    let write_timestamp = now_rfc3339()
        .map_err(|error| Status::internal(format!("format terminal timestamp: {error}")))?;
    let edition = remanence_parity::plan_tape_index_edition(
        remanence_parity::TapeIndexEditionDescriptor {
            tape_uuid: spec.tape_uuid,
            edition_id: *edition_id.as_bytes(),
            edition_sequence,
            scope: summary.scope,
            counts: summary.counts,
            block_size: spec.block_size,
            compression_enabled: false,
            writer_version: writer_version.clone(),
            write_timestamp: write_timestamp.clone(),
            terminal_layout: layout,
        },
        source,
    )
    .map_err(|error| Status::failed_precondition(format!("plan terminal edition: {error}")))?;
    let intent = remanence_state::TerminalFinalizationIntent {
        tape_uuid: spec.tape_uuid,
        trigger: spec.trigger,
        manual: spec.manual.clone(),
        progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
        recovery_required: false,
        edition_id: *edition_id.as_bytes(),
        edition_sequence,
        edition_digest: edition.edition_digest,
        writer_version,
        write_timestamp,
        terminal_prefix: terminal_prefix.map(remanence_state::TerminalFinalizationPrefixPlan::from),
        layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
            .map_err(crate::status_from_state_error)?,
    };
    let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
        Status::failed_precondition(format!("plan terminal triple writer: {error}"))
    })?;
    Ok((intent, plan))
}

fn publish_terminal_intent(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    spec: &TerminalFinalizeSpec,
    intent: &remanence_state::TerminalFinalizationIntent,
) -> Result<(), Status> {
    checkpoint
        .begin_terminal_finalization(intent)
        .map_err(crate::status_from_state_error)?;
    index
        .project_terminal_finalization(TerminalFinalizationProjectionInput {
            tape_uuid: spec.tape_uuid,
            trigger: spec.trigger,
            operation_id: spec.operation_id,
            progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
            edition_digest: intent.edition_digest,
            layout_digest: intent.layout.layout_digest,
            outcome: TerminalFinalizationOutcome::InProgress,
            updated_at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    Ok(())
}

/// Distinguish an accepted manual intent from the provisional companion left
/// if the process dies before the guarded SQLite transaction commits.
///
/// Both database halves are committed by one transaction. Seeing neither is
/// therefore the sole removable provisional state; seeing exactly one is
/// corruption, while seeing both requires exact equality before recovery.
pub(crate) fn reconcile_manual_terminal_acceptance(
    index: &CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    intent: &remanence_state::TerminalFinalizationIntent,
) -> Result<bool, Status> {
    let manual = intent.manual.as_ref().ok_or_else(|| {
        Status::failed_precondition("manual acceptance reconciliation has no manual identity")
    })?;
    if intent.trigger != remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        || manual.operation_kind != FINALIZE_TAPE_OPERATION_KIND
    {
        return Err(Status::failed_precondition(
            "manual acceptance reconciliation has invalid operation authority",
        ));
    }
    let operation_id = Uuid::from_bytes(manual.operation_id);
    let idempotency_key = Uuid::from_bytes(manual.idempotency_key);
    let projection = index
        .terminal_finalization(&intent.tape_uuid)
        .map_err(crate::status_from_state_error)?;
    let scope = index
        .idempotency_scope_record(
            manual.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            idempotency_key,
        )
        .map_err(crate::status_from_state_error)?;
    match (projection, scope) {
        (None, None) => {
            checkpoint
                .retire_provisional_manual_finalization(intent, false, false)
                .map_err(crate::status_from_state_error)?;
            Ok(false)
        }
        (Some(_), None) | (None, Some(_)) => Err(Status::failed_precondition(
            "manual terminal acceptance is corrupt: finalization projection and idempotency binding are not both present",
        )),
        (Some(projection), Some(scope)) => {
            if projection.trigger != intent.trigger
                || projection.operation_id != Some(operation_id)
                || projection.edition_digest != intent.edition_digest
                || projection.layout_digest != intent.layout.layout_digest
                || scope.operation_id != operation_id
                || scope.request_fingerprint.as_slice() != manual.request_fingerprint
            {
                return Err(Status::failed_precondition(
                    "manual terminal acceptance database authority differs from its checkpoint companion",
                ));
            }
            Ok(true)
        }
    }
}

/// Complete manual close-out admission without possessing any changer or drive
/// capability. Every request-dependent rejection therefore precedes robotics,
/// LOAD, rewind, locate, mode-page configuration, and tape writes.
pub(crate) fn preflight_manual_finalize_tape(
    index: &mut CatalogIndex,
    cfg: ManualFinalizePreflightConfig<'_>,
    barcode: Option<&str>,
    request: &mut ManualFinalizeTapeActorRequest,
) -> Result<Option<ManualFinalizeTapeActorReply>, Status> {
    let checkpoint_journal =
        remanence_state::FileCheckpointJournal::open(cfg.checkpoint_journal_dir, request.tape_uuid)
            .map_err(crate::status_from_state_error)?;
    let pending_intent = checkpoint_journal
        .terminal_finalization_intent()
        .map_err(crate::status_from_state_error)?;
    let (mut checkpoint, mut existing_intent) = if pending_intent.is_some() {
        let lease = checkpoint_journal
            .acquire_exclusive_for_terminal_recovery()
            .map_err(crate::status_from_state_error)?;
        let intent = lease
            .terminal_finalization_intent()
            .map_err(crate::status_from_state_error)?;
        (lease, intent)
    } else {
        let lease = checkpoint_journal
            .acquire_exclusive()
            .map_err(crate::status_from_state_error)?;
        (lease, None)
    };

    if let Some(intent) = existing_intent
        .as_ref()
        .filter(|intent| intent.manual.is_some())
    {
        if !reconcile_manual_terminal_acceptance(index, &mut checkpoint, intent)? {
            existing_intent = None;
        }
    }

    // The checkpoint intent is the crash-recovery authority. If the process
    // stopped after that fsync but before the audit projection, recover the
    // original operation id directly from the exact actor/kind/key binding.
    if let Some(intent) = existing_intent.as_ref() {
        if let Some(manual) = intent.manual.as_ref() {
            if manual.operation_kind == FINALIZE_TAPE_OPERATION_KIND
                && manual.actor_fingerprint == request.actor_fingerprint
                && manual.idempotency_key == *request.idempotency_key.as_bytes()
            {
                if manual.request_fingerprint != request.request_fingerprint {
                    return Err(Status::already_exists(
                        "FinalizeTape idempotency key is already bound to a different request",
                    ));
                }
                request.candidate_operation_id = Uuid::from_bytes(manual.operation_id);
            }
        }
    }
    if let Some(scope) = index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
    {
        if scope.request_fingerprint.as_slice() != request.request_fingerprint {
            return Err(Status::already_exists(
                "FinalizeTape idempotency key is already bound to a different request",
            ));
        }
        request.candidate_operation_id = scope.operation_id;
    }
    let had_existing_intent = existing_intent.is_some();

    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(request.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    if let Some(fault) = tix_fault.as_ref() {
        fault
            .clear_assignment_before_reread(
                index,
                request.tape_uuid,
                request.assignment_generation,
                request.expected_pool_id.as_deref(),
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX assignment-race hook: {error}"))
            })?;
    }
    validate_manual_finalize_owned_request(index, request)?;
    let previous = checkpoint
        .last_record_bounded()
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| {
            Status::failed_precondition(
                "manual finalization requires at least one durable checkpoint",
            )
        })?;
    if previous.sealed_after_write {
        if had_existing_intent {
            project_sealed_checkpoint_then_retire_terminal_intent(
                index,
                &mut checkpoint,
                &previous,
            )?;
        } else {
            index
                .project_checkpoint_record(&previous)
                .map_err(crate::status_from_state_error)?;
        }
        let completion = previous.terminal_finalization.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "sealed checkpoint authority has no terminal-finalization completion",
            )
        })?;
        crate::ensure_tape_sealed_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            request.tape_uuid,
        )?;
        crate::ensure_manual_finalize_finished_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            request.tape_uuid,
            completion,
        )?;
        let projection = index
            .terminal_finalization(&request.tape_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| {
                Status::failed_precondition(
                    "sealed checkpoint authority has no terminal-finalization projection",
                )
            })?;
        if projection.operation_id != Some(request.candidate_operation_id) {
            return Err(Status::already_exists(
                "tape was finalized by a different operation",
            ));
        }
        return Ok(Some(ManualFinalizeTapeActorReply {
            operation_id: request.candidate_operation_id,
            projection,
        }));
    }
    if !had_existing_intent {
        session_open_reject_tape_io_fences(index, &request.tape_uuid, barcode, "finalize tape")?;
    }

    let spec = TerminalFinalizeSpec::operator(request);
    let selected = SelectedTape {
        pool_id: request
            .expected_pool_id
            .clone()
            .unwrap_or_else(|| "(unpooled)".to_string()),
        tape_uuid: request.tape_uuid,
        block_size: request.block_size,
        parity_config: request.parity_config.clone(),
    };
    let (intent, plan) = match existing_intent {
        Some(intent) => {
            validate_manual_finalize_intent(request, &intent)?;
            match &request.parity_config {
                remanence_parity::ParityConfig::None => {
                    if intent.terminal_prefix.is_some() {
                        return Err(Status::failed_precondition(
                            "parity-off finalization intent unexpectedly has a parity prefix",
                        ));
                    }
                    let mut source = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(&checkpoint)
                        .map_err(crate::status_from_state_error)?;
                    let edition = source
                        .reconstruct_final_edition(&intent)
                        .map_err(crate::status_from_state_error)?;
                    authorize_terminal_intent_capacity(
                        index,
                        &spec,
                        &selected,
                        &intent,
                        source.summary().counts,
                    )?;
                    let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
                        Status::failed_precondition(format!("reconstruct terminal writer: {error}"))
                    })?;
                    (intent, plan)
                }
                remanence_parity::ParityConfig::Scheme(scheme) => {
                    let persisted = intent.terminal_prefix.as_ref().ok_or_else(|| {
                        Status::failed_precondition("parity finalization intent has no prefix plan")
                    })?;
                    let journal_dir = cfg.checkpoint_journal_dir.parent().ok_or_else(|| {
                        Status::internal(
                            "checkpoint journal directory has no parent for the parity journal",
                        )
                    })?;
                    let path = journal_dir.join(format!(
                        "{}.remjournal",
                        crate::bytes_to_hex(&request.tape_uuid)
                    ));
                    let mut journal = FileTapeFileJournal::open(
                        path,
                        request.tape_uuid,
                        request.block_size,
                        scheme.clone(),
                    )
                    .map_err(|error| {
                        Status::failed_precondition(format!("open parity journal: {error}"))
                    })?;
                    let mut source = if journal
                        .terminal_prefix_transition_is_durable(
                            &TerminalPrefixPlan::try_from(persisted)
                                .map_err(crate::status_from_state_error)?
                                .committed_bundle,
                            &remanence_parity::CommittedBundle {
                                kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
                                entries: Vec::new(),
                                highest_protected_ordinal: persisted
                                    .committed_bundle
                                    .highest_protected_ordinal,
                                total_committed_ordinals: persisted
                                    .committed_bundle
                                    .total_committed_ordinals,
                            },
                        )
                        .map_err(|error| {
                            Status::failed_precondition(format!(
                                "inspect terminal-prefix journal transition: {error}"
                            ))
                        })?
                    {
                        remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_after_terminal_prefix(
                            &checkpoint,
                            &journal,
                            persisted,
                        )
                    } else {
                        remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                            &checkpoint,
                            &journal,
                            persisted,
                        )
                    }
                    .map_err(crate::status_from_state_error)?;
                    let edition = source
                        .reconstruct_final_edition(&intent)
                        .map_err(crate::status_from_state_error)?;
                    let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
                        Status::failed_precondition(format!("reconstruct terminal writer: {error}"))
                    })?;
                    drop(source);
                    let intent = reconcile_and_authorize_parity_resume(
                        index,
                        &mut checkpoint,
                        &spec,
                        &selected,
                        intent,
                        &plan,
                        &mut journal,
                    )?;
                    (intent, plan)
                }
            }
        }
        None => match request.parity_config {
            remanence_parity::ParityConfig::None => {
                let mut source = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(&checkpoint)
                        .map_err(crate::status_from_state_error)?;
                let first_file = source.summary().scope.covered_prefix_tape_file_count;
                build_new_terminal_plan(
                    index,
                    &selected,
                    &spec,
                    &previous,
                    &mut source,
                    TerminalPlanPosition {
                        first_tape_file_number: first_file,
                        first_start_lba: previous.eod_lba,
                        terminal_prefix: None,
                    },
                )?
            }
            remanence_parity::ParityConfig::Scheme(_) => plan_terminal_prefix_without_motion(
                index,
                &selected,
                &checkpoint,
                cfg.checkpoint_journal_dir,
                &previous,
                request,
                &spec,
            )?,
        },
    };

    // Planning may stream a large authority. Re-read at the acceptance edge,
    // then let the immediate catalog transaction repeat the same guard while
    // holding assignment writers out through companion publication.
    validate_manual_finalize_owned_request(index, request)?;
    if let Some(fault) = tix_fault.as_ref() {
        fault
            .clear_assignment_after_reread_before_acceptance(
                index,
                request.tape_uuid,
                request.assignment_generation,
                request.expected_pool_id.as_deref(),
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX assignment-race hook: {error}"))
            })?;
    }
    let projected_recovery_required = index
        .terminal_finalization(&request.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .is_some_and(|projection| {
            projection.progress == intent.progress
                && projection.outcome == TerminalFinalizationOutcome::RecoveryRequired
        });
    let projection = TerminalFinalizationProjectionInput {
        tape_uuid: request.tape_uuid,
        trigger: intent.trigger,
        operation_id: Some(request.candidate_operation_id),
        progress: intent.progress,
        edition_digest: intent.edition_digest,
        layout_digest: intent.layout.layout_digest,
        outcome: if intent.recovery_required || projected_recovery_required {
            TerminalFinalizationOutcome::RecoveryRequired
        } else {
            TerminalFinalizationOutcome::InProgress
        },
        updated_at_utc: None,
    };
    if intent.progress == remanence_state::TerminalFinalizationProgress::BeforeReplicaA {
        index
            .accept_manual_terminal_finalization_with(
                ManualTerminalFinalizationAcceptanceInput {
                    tape_uuid: request.tape_uuid,
                    expected_pool_id: request.expected_pool_id.clone(),
                    expected_assignment_generation: request.assignment_generation,
                    actor_fingerprint: request.actor_fingerprint.clone(),
                    operation_kind: FINALIZE_TAPE_OPERATION_KIND.to_string(),
                    idempotency_key: request.idempotency_key,
                    request_fingerprint: request.request_fingerprint,
                    operation_id: request.candidate_operation_id,
                    projection,
                },
                || checkpoint.begin_terminal_finalization(&intent).map(|_| ()),
            )
            .map_err(|error| match error {
                remanence_state::StateError::TapePoolAssignmentConflict(detail) => {
                    Status::failed_precondition(detail)
                }
                other => crate::status_from_state_error(other),
            })?;
    } else {
        index
            .project_terminal_finalization(projection)
            .map_err(crate::status_from_state_error)?;
    }
    // A crash after acceptance but before this append is repaired from the
    // accepted companion before any physical dispatch.
    record_manual_finalize_request_with(
        index,
        cfg.audit_dir,
        cfg.audit_fsync,
        cfg.audit_append_lock,
        request,
    )?;
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        let result = complete_terminal_finalization_host_only(
            index,
            TerminalFinalizeAuditConfig {
                audit_dir: cfg.audit_dir,
                audit_fsync: cfg.audit_fsync,
                audit_append_lock: cfg.audit_append_lock,
            },
            &mut checkpoint,
            &previous,
            &spec,
            intent,
            &plan,
            tix_fault.as_ref(),
        )?;
        return Ok(Some(ManualFinalizeTapeActorReply {
            operation_id: request.candidate_operation_id,
            projection: result.projection,
        }));
    }
    session_open_reject_tape_io_fences(index, &request.tape_uuid, barcode, "finalize tape")?;
    Ok(None)
}

/// Complete an automatic finalization whose terminal media tail is already
/// durable, without acquiring any drive or changer capability.
pub(crate) fn preflight_automatic_terminal_completion(
    index: &mut CatalogIndex,
    cfg: ManualFinalizePreflightConfig<'_>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
) -> Result<bool, Status> {
    let checkpoint_journal = remanence_state::FileCheckpointJournal::open(
        cfg.checkpoint_journal_dir,
        selected.tape_uuid,
    )
    .map_err(crate::status_from_state_error)?;
    let pending = checkpoint_journal
        .terminal_finalization_intent()
        .map_err(crate::status_from_state_error)?;
    if pending
        .as_ref()
        .is_some_and(|intent| intent.manual.is_some())
    {
        return Ok(false);
    }
    let (mut checkpoint, intent) = if pending.is_some() {
        let checkpoint = checkpoint_journal
            .acquire_exclusive_for_terminal_recovery()
            .map_err(crate::status_from_state_error)?;
        let intent = checkpoint
            .terminal_finalization_intent()
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| {
                Status::internal("automatic terminal intent disappeared in preflight")
            })?;
        (checkpoint, Some(intent))
    } else {
        (
            checkpoint_journal
                .acquire_exclusive()
                .map_err(crate::status_from_state_error)?,
            None,
        )
    };
    let previous = checkpoint
        .last_record_bounded()
        .map_err(crate::status_from_state_error)?;
    let Some(previous) = previous else {
        if intent.is_none() {
            return Ok(false);
        }
        return Err(Status::failed_precondition(
            "automatic terminal completion requires checkpoint authority",
        ));
    };
    if previous.sealed_after_write {
        let completion = previous.terminal_finalization.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "sealed checkpoint authority has no terminal-finalization completion",
            )
        })?;
        if completion.manual.is_some() {
            return Ok(false);
        }
        if intent.is_some() {
            project_sealed_checkpoint_then_retire_terminal_intent(
                index,
                &mut checkpoint,
                &previous,
            )?;
        } else {
            // An uninterrupted owner may retire the companion immediately
            // after the sealed checkpoint fsync, then fail before SQLite or
            // audit repair. The sealed record remains sufficient host-only
            // authority; ordinary journal ownership excludes a concurrent
            // writer while it is projected.
            index
                .project_checkpoint_record(&previous)
                .map_err(crate::status_from_state_error)?;
        }
        crate::ensure_tape_sealed_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            selected.tape_uuid,
        )?;
        crate::ensure_manual_finalize_finished_audit(
            index,
            cfg.audit_dir,
            cfg.audit_append_lock,
            selected.tape_uuid,
            completion,
        )?;
        return Ok(true);
    }
    let Some(intent) = intent else {
        return Ok(false);
    };
    let spec = TerminalFinalizeSpec::resume(&intent, selected.block_size, pool_cfg);
    let (intent, plan) = match &selected.parity_config {
        remanence_parity::ParityConfig::None => {
            if intent.terminal_prefix.is_some() {
                return Err(Status::failed_precondition(
                    "parity-off finalization intent unexpectedly has a parity prefix",
                ));
            }
            let mut source =
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(
                    &checkpoint,
                )
                .map_err(crate::status_from_state_error)?;
            let edition = source
                .reconstruct_final_edition(&intent)
                .map_err(crate::status_from_state_error)?;
            authorize_terminal_intent_capacity(
                index,
                &spec,
                selected,
                &intent,
                source.summary().counts,
            )?;
            let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
                Status::failed_precondition(format!("reconstruct terminal writer: {error}"))
            })?;
            (intent, plan)
        }
        remanence_parity::ParityConfig::Scheme(scheme) => {
            let persisted = intent.terminal_prefix.as_ref().ok_or_else(|| {
                Status::failed_precondition("parity finalization intent has no prefix plan")
            })?;
            let journal_dir = cfg.checkpoint_journal_dir.parent().ok_or_else(|| {
                Status::internal("checkpoint journal directory has no parent for parity journal")
            })?;
            let mut journal = FileTapeFileJournal::open(
                journal_dir.join(format!(
                    "{}.remjournal",
                    crate::bytes_to_hex(&selected.tape_uuid)
                )),
                selected.tape_uuid,
                selected.block_size,
                scheme.clone(),
            )
            .map_err(|error| {
                Status::failed_precondition(format!("open parity journal: {error}"))
            })?;
            let prefix =
                TerminalPrefixPlan::try_from(persisted).map_err(crate::status_from_state_error)?;
            let prefix_checkpoint = remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: prefix.committed_bundle.highest_protected_ordinal,
                total_committed_ordinals: prefix.committed_bundle.total_committed_ordinals,
            };
            let prefix_is_durable = journal
                .terminal_prefix_transition_is_durable(&prefix.committed_bundle, &prefix_checkpoint)
                .map_err(|error| {
                    Status::failed_precondition(format!(
                        "inspect terminal-prefix journal transition: {error}"
                    ))
                })?;
            let mut source = if prefix_is_durable {
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_after_terminal_prefix(
                    &checkpoint,
                    &journal,
                    persisted,
                )
            } else {
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                    &checkpoint,
                    &journal,
                    persisted,
                )
            }
            .map_err(crate::status_from_state_error)?;
            let edition = source
                .reconstruct_final_edition(&intent)
                .map_err(crate::status_from_state_error)?;
            let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
                Status::failed_precondition(format!("reconstruct terminal writer: {error}"))
            })?;
            drop(source);
            let intent = reconcile_and_authorize_parity_resume(
                index,
                &mut checkpoint,
                &spec,
                selected,
                intent,
                &plan,
                &mut journal,
            )?;
            (intent, plan)
        }
    };
    if intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        return Ok(false);
    }
    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(selected.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    complete_terminal_finalization_host_only(
        index,
        TerminalFinalizeAuditConfig {
            audit_dir: cfg.audit_dir,
            audit_fsync: cfg.audit_fsync,
            audit_append_lock: cfg.audit_append_lock,
        },
        &mut checkpoint,
        &previous,
        &spec,
        intent,
        &plan,
        tix_fault.as_ref(),
    )?;
    Ok(true)
}

/// Project sealed authority before retiring its matching recovery companion.
///
/// The recovery lease acquisition has already proved that the sealed
/// completion exactly matches the normalized companion. Keeping the companion
/// through projection preserves host-only retry routing if SQLite rejects the
/// update; replay removes it only after projection succeeds.
fn project_sealed_checkpoint_then_retire_terminal_intent(
    index: &mut CatalogIndex,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
) -> Result<(), Status> {
    index
        .project_checkpoint_record(previous)
        .map_err(crate::status_from_state_error)?;
    checkpoint
        .replay_for_terminal_recovery()
        .map_err(crate::status_from_state_error)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finalize_terminal_no_parity(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    existing_intent: Option<remanence_state::TerminalFinalizationIntent>,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    rewritable: bool,
) -> Result<TerminalFinalizeResult, Status> {
    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(spec.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    let mut source =
        remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(
            checkpoint,
        )
        .map_err(crate::status_from_state_error)?;
    let (intent, plan) = match existing_intent {
        Some(intent) => {
            if intent.terminal_prefix.is_some() {
                return Err(Status::failed_precondition(
                    "parity-off finalization intent unexpectedly has a parity prefix",
                ));
            }
            let edition = source
                .reconstruct_final_edition(&intent)
                .map_err(crate::status_from_state_error)?;
            authorize_terminal_intent_capacity(
                index,
                spec,
                selected,
                &intent,
                source.summary().counts,
            )?;
            let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
                Status::failed_precondition(format!("reconstruct terminal writer: {error}"))
            })?;
            (intent, plan)
        }
        None => {
            let first_file = source.summary().scope.covered_prefix_tape_file_count;
            let planned = build_new_terminal_plan(
                index,
                selected,
                spec,
                previous,
                &mut source,
                TerminalPlanPosition {
                    first_tape_file_number: first_file,
                    first_start_lba: previous.eod_lba,
                    terminal_prefix: None,
                },
            )?;
            publish_terminal_intent(index, checkpoint, spec, &planned.0)?;
            planned
        }
    };
    if let Some(request) = manual_request {
        record_manual_finalize_request(index, cfg, request)?;
    }
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        drop(source);
        return complete_terminal_finalization_host_only(
            index,
            TerminalFinalizeAuditConfig::from(cfg),
            checkpoint,
            previous,
            spec,
            intent,
            &plan,
            tix_fault.as_ref(),
        );
    }
    drive
        .locate(previous.eod_lba)
        .map_err(|error| Status::unavailable(format!("locate checkpoint EOD: {error}")))?;
    finish_terminal_tail(
        index,
        cfg,
        drive,
        checkpoint,
        previous,
        spec,
        selected,
        manual_request,
        intent,
        plan,
        source,
        None,
        rewritable,
        tix_fault.as_ref(),
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_terminal_with_parity(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    existing_intent: Option<remanence_state::TerminalFinalizationIntent>,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    rewritable: bool,
) -> Result<TerminalFinalizeResult, Status> {
    let scheme = match &selected.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity finalization called for parity-off tape",
            ));
        }
    };
    let journal_path = parity_journal_path(cfg, spec.tape_uuid)?;
    let journal = FileTapeFileJournal::open(
        journal_path,
        spec.tape_uuid,
        spec.block_size,
        scheme.clone(),
    )
    .map_err(|error| Status::failed_precondition(format!("open parity journal: {error}")))?;
    finalize_terminal_with_parity_journal(
        index,
        cfg,
        drive,
        checkpoint,
        previous,
        existing_intent,
        spec,
        selected,
        manual_request,
        journal,
        rewritable,
    )
}

/// Finalize with the caller's already-exclusive parity journal.
///
/// An open write session owns this handle for its whole append lifetime. Its
/// automatic terminal transition must transfer that handle here instead of
/// opening the same journal a second time and contending with itself.
#[allow(clippy::too_many_arguments)]
fn finalize_terminal_with_parity_journal(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    existing_intent: Option<remanence_state::TerminalFinalizationIntent>,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    journal: FileTapeFileJournal,
    rewritable: bool,
) -> Result<TerminalFinalizeResult, Status> {
    let tix_fault = crate::terminal_fault::TerminalFaultPlan::from_env_for_tape(spec.tape_uuid)
        .map_err(|error| {
            Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
        })?;
    let resuming_existing_intent = existing_intent.is_some();
    let (prefix, intent, plan, source, prefix_snapshot, mut journal) = match existing_intent {
        Some(intent) => {
            let persisted = intent.terminal_prefix.as_ref().ok_or_else(|| {
                Status::failed_precondition("parity finalization intent has no prefix plan")
            })?;
            let prefix =
                TerminalPrefixPlan::try_from(persisted).map_err(crate::status_from_state_error)?;
            let prefix_checkpoint = remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: prefix.committed_bundle.highest_protected_ordinal,
                total_committed_ordinals: prefix.committed_bundle.total_committed_ordinals,
            };
            let prefix_is_durable = journal
                .terminal_prefix_transition_is_durable(&prefix.committed_bundle, &prefix_checkpoint)
                .map_err(|error| {
                    Status::failed_precondition(format!(
                        "inspect terminal-prefix journal transition: {error}"
                    ))
                })?;
            let mut source = if prefix_is_durable {
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_after_terminal_prefix(
                    checkpoint,
                    &journal,
                    persisted,
                )
            } else {
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                    checkpoint,
                    &journal,
                    persisted,
                )
            }
            .map_err(crate::status_from_state_error)?;
            let edition = source
                .reconstruct_final_edition(&intent)
                .map_err(crate::status_from_state_error)?;
            let plan = TerminalTripleWritePlan::new(edition).map_err(|error| {
                Status::failed_precondition(format!("reconstruct terminal writer: {error}"))
            })?;
            let (prefix_snapshot, journal) = if prefix_is_durable {
                (None, journal)
            } else {
                let snapshot = journal
                    .planned_terminal_prefix_base_snapshot_bounded(&prefix.committed_bundle)
                    .map_err(|error| {
                        Status::failed_precondition(format!("freeze terminal-prefix base: {error}"))
                    })?;
                let reconstructed =
                    remanence_parity::plan_checkpointed_terminal_index_close(&snapshot)
                        .map_err(|error| status_from_parity_error(&error, error.to_string()))?;
                if reconstructed != prefix {
                    return Err(Status::failed_precondition(
                        "bounded terminal close seed conflicts with persisted prefix plan",
                    ));
                }
                (Some(snapshot), journal)
            };
            (prefix, intent, plan, source, prefix_snapshot, journal)
        }
        None => {
            // Validate the checkpoint/parity bijection before the resume
            // session performs even positioning motion.
            let base_authority =
                remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed(
                    checkpoint, &journal,
                )
                .map_err(crate::status_from_state_error)?;
            if base_authority
                .summary()
                .scope
                .covered_prefix_tape_file_count
                == 0
            {
                return Err(Status::failed_precondition(
                    "checkpointed parity tape has no replayable committed prefix",
                ));
            }
            let snapshot = journal.committed_snapshot_bounded().map_err(|error| {
                Status::failed_precondition(format!("freeze parity journal: {error}"))
            })?;
            let prefix = remanence_parity::plan_checkpointed_terminal_index_close(&snapshot)
                .map_err(|error| status_from_parity_error(&error, error.to_string()))?;
            let expected_start_file = previous.next_tape_file_number;
            if prefix.start_tape_file_number != expected_start_file
                || prefix.start_lba != previous.eod_lba
            {
                return Err(Status::failed_precondition(format!(
                    "bounded terminal close seed starts at tape file {}/LBA {}, expected {expected_start_file}/{}",
                    prefix.start_tape_file_number, prefix.start_lba, previous.eod_lba
                )));
            }
            let persisted = remanence_state::TerminalFinalizationPrefixPlan::from(&prefix);
            let mut source = remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                checkpoint,
                &journal,
                &persisted,
            )
            .map_err(crate::status_from_state_error)?;
            let planned = build_new_terminal_plan(
                index,
                selected,
                spec,
                previous,
                &mut source,
                TerminalPlanPosition {
                    first_tape_file_number: prefix.tail_start_tape_file_number,
                    first_start_lba: prefix.tail_start_lba,
                    terminal_prefix: Some(&prefix),
                },
            )?;
            publish_terminal_intent(index, checkpoint, spec, &planned.0)?;
            (
                prefix,
                planned.0,
                planned.1,
                source,
                Some(snapshot),
                journal,
            )
        }
    };
    // Before any terminal-prefix/component positioning or write, require the
    // checkpoint intent and sink journal to name the same canonical component
    // history. The only admissible crash skew is one exact barrier-proved sink
    // transition ahead, which is reconciled entirely in host durability first.
    let intent = if resuming_existing_intent {
        reconcile_and_authorize_parity_resume(
            index,
            checkpoint,
            spec,
            selected,
            intent,
            &plan,
            &mut journal,
        )?
    } else {
        reconcile_terminal_component_host_authority(
            index,
            checkpoint,
            spec,
            intent,
            &plan,
            &mut journal,
        )?
    };
    if let Some(request) = manual_request {
        record_manual_finalize_request(index, cfg, request)?;
    }
    if intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        drop(source);
        return complete_terminal_finalization_host_only(
            index,
            TerminalFinalizeAuditConfig::from(cfg),
            checkpoint,
            previous,
            spec,
            intent,
            &plan,
            tix_fault.as_ref(),
        );
    }

    let prefix_checkpoint = remanence_parity::CommittedBundle {
        kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
        entries: Vec::new(),
        highest_protected_ordinal: prefix.committed_bundle.highest_protected_ordinal,
        total_committed_ordinals: prefix.committed_bundle.total_committed_ordinals,
    };
    let prefix_already_journaled = journal
        .terminal_prefix_transition_is_durable(&prefix.committed_bundle, &prefix_checkpoint)
        .map_err(|error| {
            Status::failed_precondition(format!(
                "inspect terminal-prefix journal transition: {error}"
            ))
        })?;

    // The immutable intent is already fsynced at BeforeReplicaA here. The
    // daemon's partial sidecar, when any exists, belongs to the preceding
    // ordinary checkpoint; only the final ParityMap can still require prefix
    // media motion on this path. A restart with a durable prefix skips this
    // motion boundary instead of manufacturing a second cut.
    if let Some(fault) = tix_fault.as_ref().filter(|_| !prefix_already_journaled) {
        let position = drive.position().map_err(|error| {
            Status::unavailable(format!(
                "read position before terminal-prefix fault boundary: {error}"
            ))
        })?;
        fault
            .abort_prefix_if_matches(
                crate::terminal_fault::TerminalFaultCut::BeforeTerminalPrefix,
                Some(PhysicalPositionHint {
                    partition: position.partition,
                    lba: position.lba,
                }),
                &prefix,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }

    let prefix_evidence = {
        let mut raw = DriveHandleRawSource::new(drive);
        reconcile_terminal_prefix(
            &mut raw,
            &prefix,
            &spec.tape_uuid,
            spec.block_size,
            rewritable,
        )
    };
    if prefix_already_journaled {
        if prefix_evidence != TerminalPrefixReconcileEvidence::Complete {
            persist_terminal_recovery_required(index, checkpoint, spec)?;
            return Err(Status::failed_precondition(format!(
                "parity journal records a complete terminal prefix, but media reconciliation found {prefix_evidence:?}"
            )));
        }
    } else {
        let snapshot = prefix_snapshot.as_ref().ok_or_else(|| {
            Status::internal("bounded parity prefix authority snapshot is missing")
        })?;
        let mut raw = DriveHandleRawSink::new(drive);
        let mut faulting = crate::terminal_fault::TerminalPrefixFaultSink::new(
            &mut raw,
            tix_fault.as_ref(),
            &prefix,
        );
        let prefix_result = match remanence_parity::close_checkpointed_terminal_index_prefix(
            &mut faulting,
            &mut journal,
            snapshot,
            &prefix,
            prefix_evidence,
        ) {
            Ok(result) => result,
            Err(error) => {
                persist_terminal_recovery_required(index, checkpoint, spec)?;
                return Err(status_from_parity_error(&error, error.to_string()));
            }
        };
        if let Some(fault) = tix_fault.as_ref() {
            fault
                .abort_prefix_if_matches(
                    crate::terminal_fault::TerminalFaultCut::AfterTerminalPrefix,
                    Some(PhysicalPositionHint::new(prefix_result.used_tape_blocks)),
                    &prefix,
                )
                .map_err(|error| {
                    Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
                })?;
        }
    }

    finish_terminal_tail(
        index,
        cfg,
        drive,
        checkpoint,
        previous,
        spec,
        selected,
        manual_request,
        intent,
        plan,
        source,
        Some(&mut journal),
        rewritable,
        tix_fault.as_ref(),
    )
}

#[allow(clippy::too_many_arguments)]
fn complete_terminal_finalization_host_only(
    index: &mut CatalogIndex,
    audit: TerminalFinalizeAuditConfig<'_>,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    spec: &TerminalFinalizeSpec,
    intent: remanence_state::TerminalFinalizationIntent,
    plan: &TerminalTripleWritePlan,
    tix_fault: Option<&crate::terminal_fault::TerminalFaultPlan>,
) -> Result<TerminalFinalizeResult, Status> {
    if intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        return Err(Status::internal(
            "host-only terminal completion requires durable replica C progress",
        ));
    }
    // Keep a recovery-required companion fail-closed until the sealed
    // checkpoint itself is fsynced. The checkpoint transition validates a
    // normalized completion copy and retires the companion only afterwards.
    let mut completed_intent = intent.clone();
    completed_intent.recovery_required = false;
    let replica_c = plan.edition.descriptor.terminal_layout.components[4];
    let final_bundle = remanence_parity::terminal_component_bundle(plan, replica_c)
        .map_err(|error| Status::internal(format!("build final C authority: {error}")))?;
    let final_record = remanence_state::CheckpointJournalRecord {
        ordinal: previous
            .ordinal
            .checked_add(1)
            .ok_or_else(|| Status::internal("terminal checkpoint ordinal overflows u64"))?,
        committed_object_count: previous.committed_object_count,
        eod_partition: plan.edition.descriptor.terminal_layout.partition,
        eod_lba: plan.edition.descriptor.terminal_layout.expected_eod_lba,
        tape_uuid: spec.tape_uuid,
        batch_id: intent.edition_id,
        next_tape_file_number: replica_c
            .planned_tape_file_number
            .checked_add(1)
            .ok_or_else(|| Status::internal("terminal next tape-file number overflows u64"))?,
        block_size: spec.block_size,
        objects: Vec::new(),
        scheme: previous.scheme.clone(),
        object_tape_file_bundles: Vec::new(),
        barrier_bundle: Some(final_bundle),
        terminal_finalization: Some(completed_intent),
        sealed_after_write: true,
    };
    if let Some(fault) = tix_fault {
        fault
            .abort_if_matches(
                "final_projection",
                crate::terminal_fault::TerminalFaultCut::BeforeFinalCheckpointFsync,
                Some(PhysicalPositionHint {
                    partition: final_record.eod_partition,
                    lba: final_record.eod_lba,
                }),
                None,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }
    checkpoint
        .append_terminal_finalization_with_after_fsync(std::slice::from_ref(&final_record), || {
            if let Some(fault) = tix_fault {
                fault
                    .abort_if_matches(
                        "final_projection",
                        crate::terminal_fault::TerminalFaultCut::AfterFinalCheckpointFsync,
                        Some(PhysicalPositionHint {
                            partition: final_record.eod_partition,
                            lba: final_record.eod_lba,
                        }),
                        None,
                    )
                    .map_err(remanence_state::StateError::JournalReplayFailed)?;
            }
            Ok(())
        })
        .map_err(crate::status_from_state_error)?;
    if let Some(fault) = tix_fault {
        fault
            .abort_if_matches(
                "final_projection",
                crate::terminal_fault::TerminalFaultCut::BeforeFinalSqliteProjection,
                Some(PhysicalPositionHint {
                    partition: final_record.eod_partition,
                    lba: final_record.eod_lba,
                }),
                None,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }
    index
        .project_checkpoint_record(&final_record)
        .map_err(crate::status_from_state_error)?;
    if let Some(fault) = tix_fault {
        fault
            .abort_if_matches(
                "final_projection",
                crate::terminal_fault::TerminalFaultCut::AfterFinalSqliteProjection,
                Some(PhysicalPositionHint {
                    partition: final_record.eod_partition,
                    lba: final_record.eod_lba,
                }),
                None,
            )
            .map_err(|error| {
                Status::failed_precondition(format!("TIX terminal fault plan: {error}"))
            })?;
    }
    crate::ensure_tape_sealed_audit(
        index,
        audit.audit_dir,
        audit.audit_append_lock,
        spec.tape_uuid,
    )?;
    let completion = final_record.terminal_finalization.as_ref().ok_or_else(|| {
        Status::internal("sealed terminal checkpoint omitted its completion authority")
    })?;
    crate::ensure_manual_finalize_finished_audit(
        index,
        audit.audit_dir,
        audit.audit_append_lock,
        spec.tape_uuid,
        completion,
    )?;
    let projection = index
        .terminal_finalization(&spec.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::internal("terminal projection disappeared after completion"))?;
    Ok(TerminalFinalizeResult {
        projection,
        final_record: Some(final_record),
    })
}

#[allow(clippy::too_many_arguments)]
fn finish_terminal_tail(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    checkpoint: &mut remanence_state::FileCheckpointJournalLease,
    previous: &remanence_state::CheckpointJournalRecord,
    spec: &TerminalFinalizeSpec,
    selected: &SelectedTape,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    intent: remanence_state::TerminalFinalizationIntent,
    plan: TerminalTripleWritePlan,
    mut source: remanence_state::CheckpointTerminalIndexRecordSource<'_>,
    parity_journal: Option<&mut FileTapeFileJournal>,
    rewritable: bool,
    tix_fault: Option<&crate::terminal_fault::TerminalFaultPlan>,
) -> Result<TerminalFinalizeResult, Status> {
    let mut authority = TerminalTailCatalogAuthority {
        checkpoint,
        parity_journal,
        index,
        spec,
        intent,
        tix_fault,
        reconciliation: None,
    };
    loop {
        let progress = parity_progress_from_state(authority.intent.progress);
        let Some(component_index) = progress.next_component_index() else {
            break;
        };
        let component = plan.edition.descriptor.terminal_layout.components[component_index];
        let evidence = {
            let mut raw = DriveHandleRawSource::new(drive);
            reconcile_terminal_tail_next(&mut raw, &plan, progress, rewritable)
        };
        authority.set_reconciliation(progress, component, evidence);
        let step_result = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut faulting = crate::terminal_fault::TerminalFaultSink::new(
                &mut raw,
                authority.tix_fault,
                component,
            );
            remanence_parity::write_terminal_tail_step(
                &mut faulting,
                &mut source,
                &mut authority,
                &plan,
            )
        };
        let step = match step_result {
            Ok(step) => step,
            Err(error) => {
                authority.intent = authority
                    .checkpoint
                    .mark_terminal_recovery_required()
                    .map_err(crate::status_from_state_error)?;
                let state_progress = authority.intent.progress;
                let detail = error.to_string();
                let status = status_from_terminal_tail_error(&error);
                authority
                    .index
                    .project_terminal_finalization(authority.projection_input(
                        state_progress,
                        TerminalFinalizationOutcome::RecoveryRequired,
                    ))
                    .map_err(crate::status_from_state_error)?;
                record_terminal_finalize_event(
                    authority.index,
                    cfg,
                    manual_request,
                    AuditEvent::CompletionUnknown,
                    state_progress,
                    Some(("recovery_detail", detail.clone())),
                )?;
                let (status, _) = fence_failed_parity_raw_write(
                    authority.index,
                    cfg,
                    selected,
                    "terminal_tail",
                    None,
                    None,
                    detail.as_str(),
                    status,
                );
                return Err(status);
            }
        };
        match step {
            TerminalTailStepOutcome::Advanced(_) => {}
            TerminalTailStepOutcome::AlreadyComplete => break,
            TerminalTailStepOutcome::RecoveryRequired {
                progress,
                component,
                evidence,
            } => {
                let state_progress = state_progress_from_parity(progress);
                authority.intent = authority
                    .checkpoint
                    .mark_terminal_recovery_required()
                    .map_err(crate::status_from_state_error)?;
                if authority.intent.progress != state_progress {
                    return Err(Status::internal(
                        "terminal recovery evidence disagrees with durable checkpoint progress",
                    ));
                }
                let outcome = terminal_reconciliation_outcome(state_progress, evidence);
                authority
                    .index
                    .project_terminal_finalization(
                        authority.projection_input(state_progress, outcome),
                    )
                    .map_err(crate::status_from_state_error)?;
                record_terminal_finalize_event(
                    authority.index,
                    cfg,
                    manual_request,
                    AuditEvent::CompletionUnknown,
                    state_progress,
                    Some((
                        "recovery_detail",
                        format!(
                            "component {:?}/{} at file {} requires recovery: {evidence:?}",
                            component.kind, component.ordinal, component.planned_tape_file_number
                        ),
                    )),
                )?;
                let projection = authority
                    .index
                    .terminal_finalization(&spec.tape_uuid)
                    .map_err(crate::status_from_state_error)?
                    .ok_or_else(|| Status::internal("recovery projection disappeared"))?;
                return Ok(TerminalFinalizeResult {
                    projection,
                    final_record: None,
                });
            }
        }
    }

    let completed_intent = authority
        .checkpoint
        .terminal_finalization_intent()
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::internal("completed terminal tail lost its durable intent"))?;
    if completed_intent.progress != remanence_state::TerminalFinalizationProgress::AfterReplicaC {
        return Err(Status::internal(
            "terminal writer returned complete before replica C became durable",
        ));
    }
    drop(authority);
    complete_terminal_finalization_host_only(
        index,
        TerminalFinalizeAuditConfig::from(cfg),
        checkpoint,
        previous,
        spec,
        completed_intent,
        &plan,
        tix_fault,
    )
}

fn status_from_terminal_tail_error(error: &TerminalTailWriteError) -> Status {
    let message = format!("terminal tape write failed: {error}");
    match error {
        TerminalTailWriteError::Media(parity) => status_from_parity_error(parity, message),
        TerminalTailWriteError::Layout(_)
        | TerminalTailWriteError::Replica(_)
        | TerminalTailWriteError::Separation(_)
        | TerminalTailWriteError::PlanMismatch(_) => Status::failed_precondition(message),
        TerminalTailWriteError::Authority(_)
        | TerminalTailWriteError::PositionMismatch { .. }
        | TerminalTailWriteError::ShortWrite { .. }
        | TerminalTailWriteError::EndOfMedium => Status::unavailable(message),
    }
}

fn validate_manual_finalize_owned_request(
    index: &CatalogIndex,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let assignment = index
        .get_tape_assignment_snapshot(&request.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape disappeared while finalization owner was held"))?;
    if assignment.assignment_generation != request.assignment_generation
        || assignment.pool_id != request.expected_pool_id
    {
        return Err(Status::failed_precondition(format!(
            "tape assignment changed before terminal finalization: expected generation {} pool {:?}, found generation {} pool {:?}",
            request.assignment_generation,
            request.expected_pool_id,
            assignment.assignment_generation,
            assignment.pool_id
        )));
    }
    match (&request.expected_pool_id, &request.pool_config) {
        (Some(expected), Some(pool_config)) if pool_config.id == *expected => {}
        (None, None) => {}
        _ => {
            return Err(Status::failed_precondition(
                "manual terminal finalization pool policy does not match the guarded assignment",
            ));
        }
    }

    if let Some(scope) = index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
    {
        if scope.request_fingerprint.as_slice() != request.request_fingerprint {
            return Err(Status::already_exists(
                "FinalizeTape idempotency key is already bound to a different request",
            ));
        }
        if scope.operation_id != request.candidate_operation_id {
            return Err(Status::failed_precondition(format!(
                "FinalizeTape owner received operation {}, but durable idempotency authority names {}",
                request.candidate_operation_id, scope.operation_id
            )));
        }
    }
    Ok(())
}

fn require_manual_finalize_preflight_binding(
    index: &CatalogIndex,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let scope = index
        .idempotency_scope_record(
            request.actor_fingerprint.as_str(),
            FINALIZE_TAPE_OPERATION_KIND,
            request.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| {
            Status::failed_precondition(
                "FinalizeTape drive dispatch is missing its durable preflight binding",
            )
        })?;
    if scope.operation_id != request.candidate_operation_id
        || scope.request_fingerprint.as_slice() != request.request_fingerprint
    {
        return Err(Status::already_exists(
            "FinalizeTape drive dispatch does not match its durable preflight binding",
        ));
    }
    Ok(())
}

fn record_manual_finalize_request(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    record_manual_finalize_request_with(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        request,
    )
}

fn record_manual_finalize_request_with(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    _audit_fsync: bool,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    request: &ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(request.tape_uuid.to_vec()),
        ),
        (
            "actor_fingerprint".to_string(),
            CborValue::Text(request.actor_fingerprint.clone()),
        ),
        (
            "request_fingerprint".to_string(),
            CborValue::Bytes(request.request_fingerprint.to_vec()),
        ),
        (
            "assignment_generation".to_string(),
            CborValue::Integer(request.assignment_generation.into()),
        ),
        (
            "reason".to_string(),
            CborValue::Text(request.reason.clone()),
        ),
    ]);
    if let Some(pool_id) = request.expected_pool_id.as_ref() {
        detail.insert(
            "expected_pool_id".to_string(),
            CborValue::Text(pool_id.clone()),
        );
    }
    detail.insert(
        "operation_kind".to_string(),
        CborValue::Text(FINALIZE_TAPE_OPERATION_KIND.to_string()),
    );
    let subject = AuditSubject {
        kind: "tape".to_string(),
        id: Some(Uuid::from_bytes(request.tape_uuid).to_string()),
    };
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("audit append lock poisoned"))?;
    std::fs::create_dir_all(audit_dir).map_err(|error| {
        Status::internal(format!(
            "create audit directory {}: {error}",
            audit_dir.display()
        ))
    })?;
    let mut exact = None;
    let mut exact_count = 0usize;
    let mut conflict = None;
    FileAuditLog::replay_incremental(audit_dir, |record| {
        if record.operation_id != Some(request.candidate_operation_id)
            || record.event != AuditEvent::RequestReceived
        {
            return ControlFlow::Continue(());
        }
        if record.source_layer == SourceLayer::Layer5
            && record.session_id.is_none()
            && record.idempotency_key == Some(request.idempotency_key)
            && record.subject == subject
            && record.detail == detail
        {
            exact_count += 1;
            exact.get_or_insert(record);
        } else {
            conflict.get_or_insert(record);
        }
        ControlFlow::Continue(())
    })
    .map_err(crate::status_from_state_error)?;
    if conflict.is_some() {
        return Err(Status::already_exists(
            "FinalizeTape operation has a conflicting durable RequestReceived audit record",
        ));
    }
    if exact_count > 1 {
        return Err(Status::failed_precondition(format!(
            "FinalizeTape operation has {exact_count} duplicate durable RequestReceived audit records"
        )));
    }
    let record = if let Some(record) = exact {
        record
    } else {
        let mut audit =
            FileAuditLog::open(audit_dir, true).map_err(crate::status_from_state_error)?;
        audit
            .append_and_return_record(AuditEventRecord {
                actor: request.actor.clone(),
                source_layer: SourceLayer::Layer5,
                operation_id: Some(request.candidate_operation_id),
                session_id: None,
                idempotency_key: Some(request.idempotency_key),
                event: AuditEvent::RequestReceived,
                subject,
                detail,
            })
            .map_err(crate::status_from_state_error)?
            .1
    };
    index
        .project_audit_record(&record)
        .map_err(crate::status_from_state_error)
}

fn record_terminal_finalize_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    event: AuditEvent,
    progress: remanence_state::TerminalFinalizationProgress,
    detail_text: Option<(&str, String)>,
) -> Result<(), Status> {
    record_terminal_finalize_event_with(
        index,
        TerminalFinalizeAuditConfig::from(cfg),
        manual_request,
        event,
        progress,
        detail_text,
    )
}

fn record_terminal_finalize_event_with(
    index: &mut CatalogIndex,
    audit: TerminalFinalizeAuditConfig<'_>,
    manual_request: Option<&ManualFinalizeTapeActorRequest>,
    event: AuditEvent,
    progress: remanence_state::TerminalFinalizationProgress,
    detail_text: Option<(&str, String)>,
) -> Result<(), Status> {
    let Some(request) = manual_request else {
        return Ok(());
    };
    record_manual_finalize_event_with(index, audit, request, event, progress, detail_text)
}

fn record_manual_finalize_event_with(
    index: &mut CatalogIndex,
    audit: TerminalFinalizeAuditConfig<'_>,
    request: &ManualFinalizeTapeActorRequest,
    event: AuditEvent,
    progress: remanence_state::TerminalFinalizationProgress,
    detail_text: Option<(&str, String)>,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(request.tape_uuid.to_vec()),
        ),
        (
            "actor_fingerprint".to_string(),
            CborValue::Text(request.actor_fingerprint.clone()),
        ),
        (
            "finalization_progress".to_string(),
            CborValue::Text(manual_finalize_progress_name(progress).to_string()),
        ),
    ]);
    if let Some((key, value)) = detail_text {
        detail.insert(key.to_string(), CborValue::Text(value));
    }
    crate::append_operation_audit(
        index,
        audit.audit_dir,
        audit.audit_fsync,
        audit.audit_append_lock,
        crate::OperationAuditInput {
            actor: AuditActor::System,
            operation_id: request.candidate_operation_id,
            operation_kind: FINALIZE_TAPE_OPERATION_KIND,
            event,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(request.tape_uuid).to_string()),
            idempotency_key: Some(request.idempotency_key),
            detail,
        },
    )
}

const fn manual_finalize_progress_name(
    progress: remanence_state::TerminalFinalizationProgress,
) -> &'static str {
    use remanence_state::TerminalFinalizationProgress as Progress;
    match progress {
        Progress::BeforeReplicaA => "before_replica_a",
        Progress::AfterReplicaA => "after_replica_a",
        Progress::AfterSeparationAb => "after_separation_ab",
        Progress::AfterReplicaB => "after_replica_b",
        Progress::AfterSeparationBc => "after_separation_bc",
        Progress::AfterReplicaC => "after_replica_c",
    }
}

fn fixed_no_compression_config(current_cfg: TapeConfig, block_size: u32) -> TapeConfig {
    TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: block_size,
        },
        compression: false,
        max_block_size_bytes: current_cfg.max_block_size_bytes,
        write_protected: current_cfg.write_protected,
        worm: current_cfg.worm,
    }
}

fn prepare_drive_for_read(
    index: &CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    session_id: Uuid,
) -> Result<(), Status> {
    let block_size = catalog_tape_block_size(index, tape_uuid)?;
    prepare_drive_for_fixed_read(drive, tape_uuid, block_size, session_id)
}

fn catalog_tape_block_size(index: &CatalogIndex, tape_uuid: &TapeUuid) -> Result<u32, Status> {
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::failed_precondition("tape catalog row is missing"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::failed_precondition("tape block_size is missing"))?;
    u32::try_from(block_size).map_err(|_| Status::internal("tape block size does not fit u32"))
}

fn prepare_drive_for_fixed_read(
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    block_size: u32,
    session_id: Uuid,
) -> Result<(), Status> {
    let started = Instant::now();
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config: {err}")))?;
    let target_cfg = fixed_no_compression_config(current_cfg, block_size);
    drive
        .write_config(target_cfg)
        .map_err(|err| Status::internal(format!("set fixed read config: {err}")))?;
    let verified = drive
        .read_config()
        .map_err(|err| Status::internal(format!("verify fixed read config: {err}")))?;
    if verified.block_size != target_cfg.block_size {
        return Err(Status::failed_precondition(format!(
            "fixed read mode verification mismatch: expected {:?}, got {:?}",
            target_cfg.block_size, verified.block_size
        )));
    }
    tracing::info!(
        target: "remanence_read_diag",
        phase = "drive_prepare_read",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(*tape_uuid),
        status = "ok",
        selected_block_size_bytes = block_size,
        prior_block_size = ?current_cfg.block_size,
        target_block_size = ?target_cfg.block_size,
        elapsed_ms = crate::diagnostics::duration_ms(started.elapsed()),
        "remanence_read_diag",
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_drive_tape_inventory(
    bay: u16,
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: TapeUuid,
    needs_drive_load: bool,
    library_serial: &str,
    barcode: Option<&str>,
    source_slot: Option<u16>,
    drive_serial: Option<&str>,
    stream_tx: &mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
) -> Result<(), Status> {
    session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "read terminal tape inventory",
            bay,
            library_serial,
            barcode,
            source_slot,
            drive_serial,
            needs_drive_load,
        },
    )?;
    session_open_reject_tape_io_fences(index, &tape_uuid, barcode, "read terminal tape inventory")?;
    verify_loaded_tape_identity(drive, &tape_uuid)?;
    let block_size = catalog_tape_block_size(index, &tape_uuid)?;
    prepare_drive_for_fixed_read(drive, &tape_uuid, block_size, Uuid::new_v4())?;

    let outcome = {
        let mut source = DriveHandleRawSource::new(drive);
        read_terminal_index_inventory_streamed(&mut source, &tape_uuid, block_size, |event| {
            send_inventory_stream_item(stream_tx, terminal_inventory_event_to_proto(event))
                .map_err(|error| error.message().to_string())
        })
        .map_err(status_from_terminal_inventory_read_error)?
    };
    if matches!(
        outcome,
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(_)
    ) {
        let summary = {
            let mut source = DriveHandleRawSource::new(drive);
            recover_terminal_inventory_from_bot(&mut source, &tape_uuid, block_size, |object| {
                send_inventory_stream_item(stream_tx, bot_recovered_object_to_proto(object))
                    .map_err(|error| error.message().to_string())
            })
            .map_err(status_from_bot_structural_recovery_error)?
        };
        send_inventory_stream_item(
            stream_tx,
            pb::TapeInventoryStreamItem {
                item: Some(pb::tape_inventory_stream_item::Item::Summary(
                    bot_structural_recovery_to_proto(tape_uuid, summary),
                )),
            },
        )?;
        return Ok(());
    }
    send_inventory_stream_item(
        stream_tx,
        pb::TapeInventoryStreamItem {
            item: Some(pb::tape_inventory_stream_item::Item::Summary(
                terminal_inventory_to_proto(tape_uuid, outcome),
            )),
        },
    )
}

fn status_from_terminal_inventory_read_error(
    error: remanence_parity::TerminalInventoryReadError,
) -> Status {
    match error {
        remanence_parity::TerminalInventoryReadError::BlockSize(_) => {
            Status::failed_precondition(format!("read terminal tape inventory: {error}"))
        }
        remanence_parity::TerminalInventoryReadError::Source { .. } => {
            Status::unavailable(format!("read terminal tape inventory: {error}"))
        }
        remanence_parity::TerminalInventoryReadError::SelectedReplica { .. }
        | remanence_parity::TerminalInventoryReadError::TerminalIndexReplicaConflict { .. } => {
            Status::data_loss(format!("read terminal tape inventory: {error}"))
        }
        remanence_parity::TerminalInventoryReadError::StreamVisitor { .. } => {
            Status::cancelled("terminal inventory receiver closed")
        }
    }
}

fn send_inventory_stream_item(
    stream_tx: &mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
    item: pb::TapeInventoryStreamItem,
) -> Result<(), Status> {
    stream_tx
        .blocking_send(Ok(item))
        .map_err(|_| Status::cancelled("terminal inventory receiver closed"))
}

fn terminal_inventory_event_to_proto(
    event: TerminalInventoryStreamEvent,
) -> pb::TapeInventoryStreamItem {
    use pb::tape_inventory_stream_item::Item;
    let item = match event {
        TerminalInventoryStreamEvent::ReplicaAttemptStarted {
            attempt_id,
            replica_ordinal,
        } => Item::ReplicaAttemptStarted(pb::TapeInventoryReplicaAttemptStarted {
            attempt_id,
            replica_ordinal: u32::from(replica_ordinal),
        }),
        TerminalInventoryStreamEvent::StructuralEntry {
            attempt_id,
            replica_ordinal,
            entry,
        } => Item::StructuralEntry(terminal_structural_entry_to_proto(
            attempt_id,
            replica_ordinal,
            entry,
        )),
        TerminalInventoryStreamEvent::ObjectRow {
            attempt_id,
            replica_ordinal,
            row,
        } => Item::ObjectRow(terminal_object_row_to_proto(
            attempt_id,
            replica_ordinal,
            row,
        )),
        TerminalInventoryStreamEvent::ReplicaAttemptRejected {
            attempt_id,
            replica_ordinal,
            failure,
        } => Item::ReplicaAttemptRejected(pb::TapeInventoryReplicaAttemptRejected {
            attempt_id,
            replica_ordinal: u32::from(replica_ordinal),
            failure_kind: terminal_replica_failure_kind_name(failure.kind).to_string(),
            detail: failure.detail,
        }),
    };
    pb::TapeInventoryStreamItem { item: Some(item) }
}

fn terminal_structural_entry_to_proto(
    attempt_id: u64,
    replica_ordinal: u16,
    entry: remanence_parity::TapeIndexReplicaMapEntry,
) -> pb::TapeInventoryStructuralEntry {
    let kind = match entry.kind {
        remanence_parity::TapeIndexReplicaFileKind::Object => {
            pb::TapeInventoryStructuralKind::Object
        }
        remanence_parity::TapeIndexReplicaFileKind::ParitySidecar => {
            pb::TapeInventoryStructuralKind::ParitySidecar
        }
        remanence_parity::TapeIndexReplicaFileKind::Bootstrap => {
            pb::TapeInventoryStructuralKind::Bootstrap
        }
        remanence_parity::TapeIndexReplicaFileKind::ParityMap => {
            pb::TapeInventoryStructuralKind::ParityMap
        }
        remanence_parity::TapeIndexReplicaFileKind::TapeIndexReplica => {
            pb::TapeInventoryStructuralKind::TapeIndexReplica
        }
        remanence_parity::TapeIndexReplicaFileKind::IndexSeparationExtent => {
            pb::TapeInventoryStructuralKind::IndexSeparationExtent
        }
    };
    pb::TapeInventoryStructuralEntry {
        attempt_id,
        replica_ordinal: u32::from(replica_ordinal),
        tape_file_number: entry.tape_file_number,
        kind: kind as i32,
        block_count: entry.block_count,
        first_parity_data_ordinal: entry.first_parity_data_ordinal,
        protected_ordinal_start: entry.protected_ordinal_start,
        protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
        epoch_id: entry.epoch_id,
    }
}

fn terminal_object_row_to_proto(
    attempt_id: u64,
    replica_ordinal: u16,
    row: remanence_parity::TapeIndexReplicaObjectRow,
) -> pb::TapeInventoryObjectRow {
    use pb::tape_inventory_object_row::Representation;
    let representation = match row.representation {
        remanence_parity::ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => Representation::Plaintext(pb::TapeInventoryPlaintextRecovery {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256: manifest_sha256.to_vec(),
        }),
        remanence_parity::ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => Representation::Encrypted(pb::TapeInventoryEncryptedRecovery {
            recipient_epoch_ids: recipient_epoch_ids
                .into_iter()
                .map(|epoch_id| epoch_id.to_vec())
                .collect(),
            metadata_frame_len,
            key_frame_len,
        }),
    };
    pb::TapeInventoryObjectRow {
        attempt_id,
        replica_ordinal: u32::from(replica_ordinal),
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id: row.object_id,
        representation: Some(representation),
    }
}

fn bot_recovered_object_to_proto(
    object: &remanence_parity::BotRecoveredObject,
) -> pb::TapeInventoryStreamItem {
    let state = match object.state {
        remanence_parity::BotRecoveredObjectState::Recovered => {
            pb::TapeInventoryBotObjectState::Recovered
        }
        remanence_parity::BotRecoveredObjectState::Unknown => {
            pb::TapeInventoryBotObjectState::Unknown
        }
        remanence_parity::BotRecoveredObjectState::Incomplete => {
            pb::TapeInventoryBotObjectState::Incomplete
        }
    };
    pb::TapeInventoryStreamItem {
        item: Some(pb::tape_inventory_stream_item::Item::BotObject(
            pb::TapeInventoryBotObject {
                tape_file_number: object.tape_file_number,
                stored_block_count: object.stored_block_count,
                object_id: object.object_id.clone(),
                state: state as i32,
            },
        )),
    }
}

fn terminal_replica_failure_kind_name(kind: TerminalReplicaFailureKind) -> &'static str {
    match kind {
        TerminalReplicaFailureKind::Missing => "missing",
        TerminalReplicaFailureKind::HeaderRead => "header_read",
        TerminalReplicaFailureKind::HeaderInvalid => "header_invalid",
        TerminalReplicaFailureKind::FooterRead => "footer_read",
        TerminalReplicaFailureKind::FooterInvalid => "footer_invalid",
        TerminalReplicaFailureKind::LocalBinding => "local_binding",
        TerminalReplicaFailureKind::TrailingFilemark => "trailing_filemark",
        TerminalReplicaFailureKind::PayloadInvalid => "payload_invalid",
        TerminalReplicaFailureKind::CrossSurvivorConflict => "cross_survivor_conflict",
    }
}

fn status_from_bot_structural_recovery_error(
    error: remanence_parity::BotStructuralRecoveryError,
) -> Status {
    match &error {
        remanence_parity::BotStructuralRecoveryError::Scan { .. } => {
            Status::unavailable(format!("BOT structural tape recovery failed: {error}"))
        }
        remanence_parity::BotStructuralRecoveryError::Visitor { .. } => {
            Status::cancelled("terminal inventory receiver closed")
        }
        remanence_parity::BotStructuralRecoveryError::TapeIdentityMismatch => {
            Status::failed_precondition(format!(
                "BOT structural tape recovery refused the physical identity: {error}"
            ))
        }
        remanence_parity::BotStructuralRecoveryError::ConflictingObjectAuthority { .. }
        | remanence_parity::BotStructuralRecoveryError::ArithmeticOverflow { .. } => {
            Status::data_loss(format!("BOT structural tape recovery failed: {error}"))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_drive_verify_tape_index(
    bay: u16,
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: TapeUuid,
    needs_drive_load: bool,
    library_serial: &str,
    barcode: Option<&str>,
    source_slot: Option<u16>,
    drive_serial: Option<&str>,
) -> Result<pb::TapeIndexVerification, Status> {
    session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "fully verify terminal tape index",
            bay,
            library_serial,
            barcode,
            source_slot,
            drive_serial,
            needs_drive_load,
        },
    )?;
    session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode,
        "fully verify terminal tape index",
    )?;
    verify_loaded_tape_identity(drive, &tape_uuid)?;
    let block_size = catalog_tape_block_size(index, &tape_uuid)?;
    prepare_drive_for_fixed_read(drive, &tape_uuid, block_size, Uuid::new_v4())?;

    let outcome = {
        let mut source = DriveHandleRawSource::new(drive);
        verify_terminal_index_full(&mut source, &tape_uuid, block_size)
            .map_err(status_from_terminal_index_verification_error)?
    };
    Ok(terminal_verification_to_proto(tape_uuid, outcome))
}

fn terminal_verification_to_proto(
    tape_uuid: TapeUuid,
    outcome: remanence_parity::TerminalIndexVerificationOutcome,
) -> pb::TapeIndexVerification {
    use remanence_parity::TerminalIndexVerificationOutcome as Outcome;
    match outcome {
        Outcome::VerifiedComplete(verified) => {
            terminal_verified_to_proto(tape_uuid, *verified, true)
        }
        Outcome::VerifiedDegraded(verified) => {
            terminal_verified_to_proto(tape_uuid, *verified, false)
        }
        Outcome::RecoveryRequired(recovery) => pb::TapeIndexVerification {
            tape_uuid: tape_uuid.to_vec(),
            state: pb::TapeIndexVerificationState::RecoveryRequired as i32,
            fast_inventory: None,
            detail: recovery.detail,
            replica_health: recovery
                .replicas
                .iter()
                .enumerate()
                .map(|(index, evidence)| terminal_replica_health(index, evidence))
                .collect(),
            separation_health: (1u32..=2)
                .map(|separation_ordinal| pb::TapeIndexSeparationHealth {
                    separation_ordinal,
                    state: pb::tape_index_separation_health::State::TapeIndexSeparationStateUnknown
                        as i32,
                    verified_interior_record_count: 0,
                    detail: "canonical prefix authority unavailable".to_string(),
                })
                .collect(),
            measured_eod_lba: recovery.measured_eod.lba,
            verified_prefix_tape_file_count: 0,
            verified_prefix_record_count: 0,
            measured_tape_file_count: recovery.bot_recovery.structural_entry_count,
            edition_digest: Vec::new(),
            layout_digest: Vec::new(),
            payload_digest: Vec::new(),
            canonical_map_digest: Vec::new(),
            verification_basis: "bot_structural_recovery".to_string(),
            recovery_inventory: Some(bot_structural_recovery_to_proto(
                tape_uuid,
                recovery.bot_recovery,
            )),
        },
    }
}

fn terminal_verified_to_proto(
    tape_uuid: TapeUuid,
    verified: remanence_parity::TerminalIndexVerification,
    complete: bool,
) -> pb::TapeIndexVerification {
    pb::TapeIndexVerification {
        tape_uuid: tape_uuid.to_vec(),
        state: if complete {
            pb::TapeIndexVerificationState::VerifiedComplete as i32
        } else {
            pb::TapeIndexVerificationState::VerifiedDegraded as i32
        },
        fast_inventory: None,
        detail: if complete {
            "physical prefix, A/B/C, AB/BC, and terminal EOD validated".to_string()
        } else {
            "canonical physical prefix verified from a surviving replica; degraded terminal component evidence is attached".to_string()
        },
        replica_health: verified
            .replicas
            .iter()
            .enumerate()
            .map(|(index, evidence)| terminal_replica_health(index, evidence))
            .collect(),
        separation_health: terminal_separation_health(&verified.separations),
        measured_eod_lba: verified.measured_eod.lba,
        verified_prefix_tape_file_count: verified.verified_prefix_tape_file_count,
        verified_prefix_record_count: verified.verified_prefix_record_count,
        measured_tape_file_count: verified.measured_tape_file_count,
        edition_digest: verified.edition.edition_digest.to_vec(),
        layout_digest: verified.edition.layout_digest.to_vec(),
        payload_digest: verified.selected_payload.payload_sha256.to_vec(),
        canonical_map_digest: verified.selected_payload.canonical_map_sha256.to_vec(),
        verification_basis: "measured_full_physical".to_string(),
        recovery_inventory: None,
    }
}

fn terminal_separation_health(
    evidence: &[remanence_parity::TerminalSeparationEvidence; 2],
) -> Vec<pb::TapeIndexSeparationHealth> {
    evidence
        .iter()
        .enumerate()
        .map(|(index, evidence)| {
            let (state, verified_interior_record_count, detail) = match evidence {
                remanence_parity::TerminalSeparationEvidence::Valid {
                    interior_record_count,
                } => (
                    pb::tape_index_separation_health::State::TapeIndexSeparationStateValid,
                    *interior_record_count,
                    "header_footer_zero_fill_and_filemark_valid".to_string(),
                ),
                remanence_parity::TerminalSeparationEvidence::Invalid { detail } => (
                    pb::tape_index_separation_health::State::TapeIndexSeparationStateInvalid,
                    0,
                    detail.clone(),
                ),
            };
            pb::TapeIndexSeparationHealth {
                separation_ordinal: u32::try_from(index + 1)
                    .expect("two separation ordinals fit u32"),
                state: state as i32,
                verified_interior_record_count,
                detail,
            }
        })
        .collect()
}

fn status_from_terminal_index_verification_error(
    error: remanence_parity::TerminalIndexVerificationError,
) -> Status {
    match error {
        remanence_parity::TerminalIndexVerificationError::Source { .. }
        | remanence_parity::TerminalIndexVerificationError::PrefixWalk { .. } => {
            Status::unavailable(format!("full terminal index verification failed: {error}"))
        }
        _ => Status::data_loss(format!("full terminal index verification failed: {error}")),
    }
}

fn bot_structural_recovery_to_proto(
    tape_uuid: TapeUuid,
    summary: remanence_parity::BotStructuralRecoverySummary,
) -> pb::TapeInventory {
    pb::TapeInventory {
        tape_uuid: tape_uuid.to_vec(),
        outcome: pb::TapeInventoryOutcome::BotStructuralRecovered as i32,
        selected_replica_ordinal: 0,
        replica_health: (1u32..=3)
            .map(|replica_ordinal| pb::TapeIndexReplicaHealth {
                replica_ordinal,
                state: pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid as i32,
                detail: "terminal replica unavailable; BOT structural recovery used".to_string(),
            })
            .collect(),
        structural_entry_count: summary.structural_entry_count,
        object_row_count: summary.complete_object_count,
        edition_digest: Vec::new(),
        layout_digest: Vec::new(),
        payload_digest: Vec::new(),
        canonical_map_digest: summary.canonical_map_digest.to_vec(),
        inventory_basis: "bot_structural_recovery".to_string(),
        detail: format!(
            "terminal index unavailable; BOT recovery classified {} recovered, {} unknown, and {} incomplete Object candidates",
            summary.recovered_object_count,
            summary.unknown_object_count,
            summary.incomplete_object_count
        ),
        recovered_object_count: summary.recovered_object_count,
        unknown_object_count: summary.unknown_object_count,
        incomplete_object_count: summary.incomplete_object_count,
        damaged_region_count: summary.damaged_region_count,
        selected_attempt_id: 0,
    }
}

fn terminal_inventory_to_proto(
    tape_uuid: TapeUuid,
    outcome: TerminalInventoryOutcome,
) -> pb::TapeInventory {
    match outcome {
        TerminalInventoryOutcome::Inventory(selection) => {
            let outcome = if selection.is_degraded() {
                pb::TapeInventoryOutcome::Degraded
            } else {
                pb::TapeInventoryOutcome::Complete
            };
            let replica_health = selection
                .replicas
                .iter()
                .enumerate()
                .map(|(index, evidence)| terminal_replica_health(index, evidence))
                .collect();
            pb::TapeInventory {
                tape_uuid: tape_uuid.to_vec(),
                outcome: outcome as i32,
                selected_replica_ordinal: u32::from(selection.selected_replica_ordinal),
                replica_health,
                structural_entry_count: selection.payload.structural_entry_count,
                object_row_count: selection.payload.object_row_count,
                edition_digest: selection.edition.edition_digest.to_vec(),
                layout_digest: selection.edition.layout_digest.to_vec(),
                payload_digest: selection.payload.payload_sha256.to_vec(),
                canonical_map_digest: selection.payload.canonical_map_sha256.to_vec(),
                inventory_basis: "terminal_index_fast".to_string(),
                detail: if selection.is_degraded() {
                    format!(
                        "terminal inventory selected replica {}; degraded replica evidence is present",
                        selection.selected_replica_ordinal
                    )
                } else {
                    "terminal inventory selected replica C; all replica envelopes agree".to_string()
                },
                recovered_object_count: 0,
                unknown_object_count: 0,
                incomplete_object_count: 0,
                damaged_region_count: 0,
                selected_attempt_id: selection.selected_attempt_id,
            }
        }
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) => {
            let detail = match recovery.reason {
                BotStructuralRecoveryReason::NoUsableTerminalLayout => {
                    "no usable terminal layout; structural recovery from BOT is required"
                }
                BotStructuralRecoveryReason::AllMembersInvalid => {
                    "terminal replicas A, B, and C are invalid; structural recovery from BOT is required"
                }
            };
            pb::TapeInventory {
                tape_uuid: tape_uuid.to_vec(),
                outcome: pb::TapeInventoryOutcome::BotStructuralRecoveryRequired as i32,
                selected_replica_ordinal: 0,
                replica_health: recovery
                    .replicas
                    .iter()
                    .enumerate()
                    .map(|(index, evidence)| terminal_replica_health(index, evidence))
                    .collect(),
                structural_entry_count: 0,
                object_row_count: 0,
                edition_digest: Vec::new(),
                layout_digest: Vec::new(),
                payload_digest: Vec::new(),
                canonical_map_digest: Vec::new(),
                inventory_basis: "terminal_index_fast".to_string(),
                detail: detail.to_string(),
                recovered_object_count: 0,
                unknown_object_count: 0,
                incomplete_object_count: 0,
                damaged_region_count: 0,
                selected_attempt_id: 0,
            }
        }
    }
}

fn terminal_replica_health(
    index: usize,
    evidence: &TerminalReplicaEvidence,
) -> pb::TapeIndexReplicaHealth {
    let replica_ordinal = u32::try_from(index + 1).expect("three replica indexes fit u32");
    let (state, detail) = match evidence {
        TerminalReplicaEvidence::Valid { .. } => (
            pb::tape_index_replica_health::State::TapeIndexReplicaStateComplete,
            "payload_valid".to_string(),
        ),
        TerminalReplicaEvidence::ConsistentEnvelope => (
            pb::tape_index_replica_health::State::TapeIndexReplicaStateEnvelopeValid,
            "envelope_valid_payload_not_read".to_string(),
        ),
        TerminalReplicaEvidence::Invalid(failure) => (
            pb::tape_index_replica_health::State::TapeIndexReplicaStateInvalid,
            format!(
                "{}: {}",
                terminal_replica_failure_name(failure.kind),
                failure.detail
            ),
        ),
    };
    pb::TapeIndexReplicaHealth {
        replica_ordinal,
        state: state as i32,
        detail,
    }
}

const fn terminal_replica_failure_name(kind: TerminalReplicaFailureKind) -> &'static str {
    match kind {
        TerminalReplicaFailureKind::Missing => "missing",
        TerminalReplicaFailureKind::HeaderRead => "header_read",
        TerminalReplicaFailureKind::HeaderInvalid => "header_invalid",
        TerminalReplicaFailureKind::FooterRead => "footer_read",
        TerminalReplicaFailureKind::FooterInvalid => "footer_invalid",
        TerminalReplicaFailureKind::LocalBinding => "local_binding",
        TerminalReplicaFailureKind::TrailingFilemark => "trailing_filemark",
        TerminalReplicaFailureKind::PayloadInvalid => "payload_invalid",
        TerminalReplicaFailureKind::CrossSurvivorConflict => "cross_survivor_conflict",
    }
}

fn handle_drive_open_read(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    request: OpenReadActorRequest,
) {
    let OpenReadActorRequest {
        tape_uuid,
        needs_drive_load,
        library_serial,
        barcode,
        source_slot,
        drive_uuid,
        drive_serial,
        resume_target,
        daemon_epoch,
        reply,
    } = request;

    if let Err(status) = session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "open read session",
            bay,
            library_serial: library_serial.as_str(),
            barcode: barcode.as_deref(),
            source_slot,
            drive_serial: drive_serial.as_deref(),
            needs_drive_load,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if let Err(status) = session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode.as_deref(),
        "open read session",
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if resume_target
        .as_ref()
        .is_some_and(|target| target.tape_uuid != tape_uuid)
    {
        let _ = reply.send(Err(Status::invalid_argument(
            "resume target tape UUID does not match mounted read target",
        )));
        return;
    }
    let session_id = Uuid::new_v4();
    let position_proof = match resume_target.as_ref() {
        Some(target) => {
            if let Err(status) = prepare_drive_for_read(index, drive, &tape_uuid, session_id) {
                let _ = reply.send(Err(status));
                return;
            }
            match position_read_resume(index, drive, target) {
                Ok(proof) => Some(proof),
                Err(status) => {
                    let _ = reply.send(Err(status));
                    return;
                }
            }
        }
        None => {
            if let Err(status) = verify_loaded_tape_identity(drive, &tape_uuid) {
                let _ = reply.send(Err(status));
                return;
            }
            if let Err(status) = prepare_drive_for_read(index, drive, &tape_uuid, session_id) {
                let _ = reply.send(Err(status));
                return;
            }
            None
        }
    };
    // Load-time wrap-map harvest + fence install (design §6.5); same
    // rule as the write path: fresh mount only, after identity.
    if needs_drive_load {
        run_load_calibration_harvest(index, drive, cfg, &tape_uuid, barcode.as_deref());
    }
    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    if let Err(status) = record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id,
            session_kind: "read",
            event: AuditEvent::SessionOpened,
            tape_uuid: Some(tape_uuid),
            library_serial: Some(library_serial.clone()),
            drive_bay: Some(bay),
            drive_uuid: drive_uuid.clone(),
            drive_serial: drive_serial.clone(),
            abort_reason: None,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    let open_reply = read_session_proto(
        session_id,
        &tape_uuid,
        pb::read_session::State::ReadSessionStateOpen,
        opened_at_utc.as_str(),
        bay,
        position_proof,
        daemon_epoch,
    );
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::ReadFile {
                session_id: requested,
                object_id,
                file_id,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                if requested != session_id {
                    let _ =
                        chunk_tx.blocking_send(Err(Status::not_found("read session not found")));
                    continue;
                }
                let result = if file_id.is_empty() {
                    stream_one_object(
                        index,
                        drive,
                        cfg,
                        session_id,
                        &tape_uuid,
                        object_id.as_str(),
                        stream_chunk_bytes,
                        chunk_tx.clone(),
                    )
                } else {
                    String::from_utf8(file_id)
                        .map_err(|err| {
                            Status::invalid_argument(format!("file_id is not utf-8: {err}"))
                        })
                        .and_then(|file_id| {
                            stream_one_file_range(
                                index,
                                drive,
                                cfg,
                                session_id,
                                &tape_uuid,
                                object_id.as_str(),
                                file_id.as_str(),
                                0,
                                0,
                                stream_chunk_bytes,
                                chunk_tx.clone(),
                            )
                        })
                };
                if let Err(status) = result {
                    record_session_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        "read-failure",
                        snapshot_misses,
                    );
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::ReadObjectRange {
                session_id: requested,
                object_id,
                file_id,
                start_byte,
                end_byte,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                if requested != session_id {
                    let _ =
                        chunk_tx.blocking_send(Err(Status::not_found("read session not found")));
                    continue;
                }
                if let Err(status) = stream_one_file_range(
                    index,
                    drive,
                    cfg,
                    session_id,
                    &tape_uuid,
                    object_id.as_str(),
                    file_id.as_str(),
                    start_byte,
                    end_byte,
                    stream_chunk_bytes,
                    chunk_tx.clone(),
                ) {
                    record_session_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        "read-failure",
                        snapshot_misses,
                    );
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::CloseRead {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    record_session_close_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        snapshot_misses,
                    );
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateClosed,
                        opened_at_utc.as_str(),
                        bay,
                        position_proof,
                        daemon_epoch,
                    ))
                } else {
                    Err(Status::not_found("read session not found"))
                };
                if status.is_ok() {
                    if let Err(err) = record_session_event(
                        index,
                        cfg,
                        SessionAuditInput {
                            session_id,
                            session_kind: "read",
                            event: AuditEvent::SessionClosed,
                            tape_uuid: Some(tape_uuid),
                            library_serial: Some(library_serial.clone()),
                            drive_bay: Some(bay),
                            drive_uuid: drive_uuid.clone(),
                            drive_serial: drive_serial.clone(),
                            abort_reason: None,
                        },
                    ) {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                }
                let _ = reply.send(status);
                if requested == session_id {
                    break;
                }
            }
            DriveCommand::GetRead {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateOpen,
                        opened_at_utc.as_str(),
                        bay,
                        position_proof,
                        daemon_epoch,
                    ))
                } else {
                    Err(Status::not_found("read session not found"))
                };
                let _ = reply.send(status);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::TapeInventory { reply, .. } => {
                let message = "read session already active";
                let _ = reply.send(Err(Status::failed_precondition(message)));
            }
            DriveCommand::VerifyTapeIndex { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::FinalizeTape { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed("read session already active", &[("phase", "admission")]);
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "active session is a read session",
                    )));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
        }
    }
}

fn session_open_reject_tape_io_fences(
    index: &CatalogIndex,
    tape_uuid: &TapeUuid,
    barcode: Option<&str>,
    action: &str,
) -> Result<(), Status> {
    let conflicts = index
        .tape_io_admission_conflicts(tape_uuid, barcode)
        .map_err(status_from_state_error)?;
    let Some(first) = conflicts.first() else {
        return Ok(());
    };
    Err(Status::failed_precondition(format!(
        "{action} blocked by active tape-I/O fence {} tape_uuid={} barcode={} reason={}; release via `rem tape quarantine release {}` before retrying",
        first.quarantine_id,
        Uuid::from_bytes(*tape_uuid),
        barcode.unwrap_or("(unknown)"),
        first.reason,
        first.quarantine_id
    )))
}

fn handle_robotics(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: String,
    action: RoboticsAction,
    handle: crate::operations::OperationHandle,
) {
    if handle.is_cancelled() {
        cancel_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            "cancelled before dispatch",
        );
        return;
    }
    publish_running(&handle, &[("phase", "open")]);
    if let Err(err) = record_library_event(
        index,
        cfg,
        &handle,
        &library_serial,
        AuditEvent::OperationStarted,
        robotics_detail(&action),
    ) {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let lib = match cfg.report.library(&library_serial) {
        Some(lib) => lib,
        None => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "open")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("open library: {err}"),
                &[("phase", "open")],
            );
            return;
        }
    };

    publish_running(&handle, &[("phase", "refresh")]);
    if let Err(err) = library.refresh() {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("refresh inventory: {err}"),
            &[("phase", "refresh")],
        );
        return;
    }

    publish_running(&handle, &[("phase", "execute")]);
    let action_result = match &action {
        RoboticsAction::Refresh => Ok(()),
        RoboticsAction::Move { src, dst } => library
            .move_medium(*src, *dst, &cfg.policy)
            .map_err(|err| err.to_string()),
        RoboticsAction::Load {
            slot,
            bay,
            wait_ready,
        } => run_load_sequence(index, cfg, &handle, &mut library, *slot, *bay, *wait_ready),
        RoboticsAction::Unload { bay, destination } => library
            .unload(*bay, *destination, &cfg.policy)
            .map_err(|err| err.to_string()),
        RoboticsAction::Clean {
            drive_uuid,
            trigger,
        } => run_cleaning_sequence(
            index,
            cfg,
            &handle,
            &mut library,
            drive_uuid.as_slice(),
            trigger.as_str(),
        )
        .map_err(|err| err.to_string()),
    };

    let observe_result = observe_refreshed_library(index, cfg, library.library())
        .map_err(|err| err.message().to_string());
    publish_library_snapshot(&cfg.library_snapshot, library.library().clone());

    match (action_result, observe_result) {
        (Ok(()), Ok(())) => {
            if let Err(err) = record_library_event(
                index,
                cfg,
                &handle,
                &library_serial,
                AuditEvent::OperationFinished,
                BTreeMap::new(),
            ) {
                fail_library_operation(
                    index,
                    cfg,
                    &handle,
                    &library_serial,
                    &format!("record operation finish audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(pb::OperationState::Succeeded, &[("phase", "done")]);
        }
        (Ok(()), Err(message)) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("observe refreshed drive catalog: {message}"),
                &[("phase", "catalog")],
            );
        }
        (Err(message), _) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                message.as_str(),
                &[("phase", "execute")],
            );
        }
    }
}

fn run_load_sequence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    slot: u16,
    bay: u16,
    wait_ready: bool,
) -> Result<(), String> {
    if !wait_ready {
        return library
            .load(slot, bay, &cfg.policy)
            .map_err(|error| error.to_string());
    }

    let barcode = library
        .library()
        .slots
        .iter()
        .find(|candidate| candidate.element_address == slot)
        .and_then(|candidate| candidate.cartridge.clone());
    let drive_bay = library
        .library()
        .drive_bays
        .iter()
        .find(|candidate| candidate.element_address == bay);
    let drive_serial = drive_bay
        .and_then(|candidate| candidate.installed.as_ref())
        .map(|drive| drive.serial.clone());
    let drive_sg = drive_bay
        .and_then(|candidate| candidate.installed.as_ref())
        .and_then(|drive| drive.sg_path.as_ref())
        .map(|path| path.display().to_string());
    let family = session_open_media_family(barcode.as_deref());
    let retryable_load_completion = match library.load(slot, bay, &cfg.policy) {
        Ok(()) => None,
        Err(error) => match retryable_readiness_from_load_error(&error, family) {
            Some(readiness) => Some(readiness),
            None => return Err(error.to_string()),
        },
    };

    let operation_id = handle.op_id_uuid();
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: library.library().serial.clone(),
            changer_sg: Some(library.library().changer_sg.display().to_string()),
            drive_element: bay,
            drive_sg,
            drive_serial,
            barcode,
            source_slot: Some(slot),
            media_generation: library
                .library()
                .drive_bays
                .iter()
                .find(|candidate| candidate.element_address == bay)
                .and_then(|candidate| candidate.loaded_tape.as_deref())
                .and_then(crate::lto_generation_from_voltag)
                .map(|generation| generation.generation_number()),
            phase: "load_drive_readiness".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: OffsetDateTime::now_utc()
                .checked_add(Duration::seconds(9_000))
                .and_then(|deadline| deadline.format(&Rfc3339).ok()),
            evidence_path: None,
        })
        .map_err(|error| format!("record load media-readiness operation: {error}"))?;
    if let Some(readiness) = retryable_load_completion.as_ref() {
        record_session_open_readiness_poll_transition(
            index,
            operation_id,
            "load_drive_completion",
            readiness,
            false,
        )
        .map_err(|error| format!("record LOAD readiness transition: {error}"))?;
    }

    handle.publish_state(
        pb::OperationState::Running,
        &[("phase", "readiness_poll"), ("state", "starting")],
    );
    let mut drive = library
        .open_drive(bay, &cfg.policy)
        .map_err(|error| format!("open drive 0x{bay:04x} for readiness wait: {error}"))?;
    let poll = poll_drive_media_readiness(
        index,
        &mut drive,
        operation_id,
        family,
        MediaReadinessWaitOptions {
            wait: true,
            timeout: LOAD_READY_TIMEOUT,
            poll_interval: LOAD_READY_POLL_INTERVAL,
        },
        handle,
        "load_drive_readiness",
    )?;
    if !poll.readiness.is_ready() {
        let state = if poll.timed_out {
            "timeout_unknown"
        } else {
            session_open_readiness_state(&poll.readiness)
        };
        return Err(format!(
            "load drive 0x{bay:04x} did not reach READY (state={state}): {}",
            session_open_readiness_summary(&poll.readiness)
        ));
    }
    library
        .refresh()
        .map_err(|error| format!("refresh inventory after READY: {error}"))
}

fn retryable_readiness_from_load_error(
    error: &LoadError,
    family: MediaFamily,
) -> Option<MediaReadiness> {
    let LoadError::DriveLoad(DriveOpError::ScsiError(error)) = error else {
        return None;
    };
    let readiness = classify_media_readiness_error_ref(error, family);
    readiness.is_retryable_wait().then_some(readiness)
}

fn observe_refreshed_library(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &remanence_library::Library,
) -> Result<(), Status> {
    crate::observe_drive_catalog_from_libraries(
        index,
        std::iter::once(library),
        &cfg.managed_library_serials,
    )
}

fn library_snapshot_persist_alarm_key(library_serial: &str) -> String {
    format!("snapshot-persist-failing:library:{library_serial}")
}

fn record_library_observation_failure(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &remanence_library::Library,
    error: &str,
) {
    tracing::warn!(
        "failed to observe refreshed drive catalog library_serial={} error={}",
        library.serial,
        error
    );
    let condition_key = library_snapshot_persist_alarm_key(library.serial.as_str());
    let detail = format!(
        "{{\"library_serial\":\"{}\",\"error\":\"{}\"}}",
        library.serial.replace('"', "'"),
        error.replace('"', "'")
    );
    if let Err(err) = raise_alarm_with_evidence(
        index,
        cfg,
        condition_key.as_str(),
        "snapshot-persist-failing",
        "warning",
        Some(detail.as_str()),
    ) {
        tracing::warn!(
            "failed to raise library snapshot alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

fn clear_library_snapshot_persist_alarm(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: &str,
) {
    let condition_key = library_snapshot_persist_alarm_key(library_serial);
    if let Err(err) = clear_alarm_with_evidence(index, cfg, condition_key.as_str()) {
        tracing::warn!(
            "failed to clear library snapshot alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

fn publish_library_snapshot(
    cell: &RwLock<Arc<crate::LibrarySnapshot>>,
    updated: remanence_library::Library,
) {
    let mut report = cell
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .report
        .clone();
    match report
        .libraries
        .iter_mut()
        .find(|library| library.serial == updated.serial)
    {
        Some(slot) => *slot = updated,
        None => report.libraries.push(updated),
    }
    let snapshot = Arc::new(crate::LibrarySnapshot {
        report,
        captured_at: OffsetDateTime::now_utc(),
    });
    *cell.write().unwrap_or_else(|err| err.into_inner()) = snapshot;
}

fn record_library_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    detail.insert(
        "library_serial".to_string(),
        CborValue::Text(library_serial.to_string()),
    );
    crate::append_operation_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::OperationAuditInput {
            actor: AuditActor::System,
            operation_id: handle.op_id_uuid(),
            operation_kind: handle.operation_kind(),
            event,
            subject_kind: "library",
            subject_id: Some(library_serial.to_string()),
            idempotency_key: None,
            detail,
        },
    )
}

fn fail_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    tracing::error!(
        target: "remanence_library_operation",
        operation_id = %handle.op_id_uuid(),
        operation_kind = %handle.operation_kind(),
        library_serial,
        error_summary,
        "library operation failed"
    );
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::OperationFailed,
        detail,
    ) {
        let audit_error = format!("{error_summary}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), progress);
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

fn cancel_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::CancelledBeforeDispatch,
        detail,
    ) {
        let audit_error = format!("{detail_message}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), &[("phase", "audit")]);
    } else {
        handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", detail_message)],
        );
    }
}

fn robotics_detail(action: &RoboticsAction) -> BTreeMap<String, CborValue> {
    let mut detail = BTreeMap::new();
    match action {
        RoboticsAction::Refresh => {}
        RoboticsAction::Move { src, dst } => {
            detail.insert(
                "src".to_string(),
                CborValue::Integer(u64::from(*src).into()),
            );
            detail.insert(
                "dst".to_string(),
                CborValue::Integer(u64::from(*dst).into()),
            );
        }
        RoboticsAction::Load {
            slot,
            bay,
            wait_ready,
        } => {
            detail.insert(
                "slot".to_string(),
                CborValue::Integer(u64::from(*slot).into()),
            );
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
            detail.insert("wait_ready".to_string(), CborValue::Bool(*wait_ready));
        }
        RoboticsAction::Unload { bay, destination } => {
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
            if let Some(dst) = destination {
                detail.insert(
                    "destination".to_string(),
                    CborValue::Integer(u64::from(*dst).into()),
                );
            }
        }
        RoboticsAction::Clean {
            drive_uuid,
            trigger,
        } => {
            detail.insert(
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.clone()),
            );
            detail.insert("trigger".to_string(), CborValue::Text(trigger.clone()));
            detail.insert(
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            );
        }
    }
    detail
}

fn run_cleaning_sequence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    drive_uuid: &[u8],
    trigger: &str,
) -> Result<(), Status> {
    let clean_cfg = &cfg.cleaning;
    if !clean_cfg.auto {
        return Err(Status::failed_precondition(
            "automatic cleaning is disabled",
        ));
    }
    let drive = index
        .get_drive_by_uuid(drive_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("drive not found"))?;
    if drive.managed != "rem" {
        return Err(Status::failed_precondition(
            "cleaning is only available for managed drives",
        ));
    }
    if drive.state != "active" {
        return Err(Status::failed_precondition("cannot clean a retired drive"));
    }
    if !drive.actionable {
        return Err(Status::failed_precondition(
            "drive is non-actionable because its serial identity is blank or collided",
        ));
    }
    let Some(library_serial) = drive.last_library_serial.clone() else {
        return Err(Status::failed_precondition(
            "drive has no current library assignment",
        ));
    };
    let drive_bay = drive
        .last_element_address
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| Status::failed_precondition("drive has no current bay"))?;
    if trigger == "periodic" && !cleaning_drive_is_idle(library, drive_bay)? {
        return Ok(());
    }
    // Join-check FIRST: a trigger while a run is already active is a join
    // (no-op), never a frequency refusal (diff-gate re-check finding).
    if let Some(active_run) = index
        .get_active_clean_run_by_drive(drive_uuid)
        .map_err(status_from_state_error)?
    {
        if active_run.phase != "done"
            && active_run.phase != "failed"
            && active_run.phase != "needs-operator"
        {
            return Ok(());
        }
    }
    let min_interval = parse_duration_or(&clean_cfg.min_interval, Duration::hours(12));
    let weekly_cap = clean_cfg.weekly_cap as usize;
    if cleaning_too_soon(index, drive_uuid, min_interval, weekly_cap)? {
        let detail = format!(
            "{{\"drive_uuid\":\"{}\",\"recovery_step\":\"frequency-cap\"}}",
            json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        );
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!(
                "drive-cleaning-abnormal-frequency:{}",
                crate::bytes_to_hex(drive_uuid)
            )
            .as_str(),
            "drive-cleaning-abnormal-frequency",
            "warning",
            Some(detail.as_str()),
        );
        return Err(Status::failed_precondition(
            "drive-cleaning-abnormal-frequency",
        ));
    }
    if drive.fenced {
        return Err(Status::failed_precondition("drive is already fenced"));
    }
    let run = index
        .begin_clean_run(drive_uuid, library_serial.as_str(), trigger, None)
        .map_err(status_from_state_error)?;
    let fence_detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"recovery_step\":\"fence\"}}",
        json_escape_text(&run.run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
    );
    if let Err(err) = raise_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run.run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(fence_detail.as_str()),
    ) {
        let _ =
            index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(fence_detail.as_str()));
        return Err(err);
    }
    index
        .set_drive_fenced(drive_uuid, true)
        .map_err(status_from_state_error)?;
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial.as_str(),
        AuditEvent::DriveFenced,
        BTreeMap::from([
            (
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.to_vec()),
            ),
            (
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            ),
        ]),
    ) {
        tracing::warn!("failed to append cleaning fence audit: {err}");
    }
    let tape_prefixes = clean_cfg
        .voltag_prefixes
        .iter()
        .map(|prefix| prefix.trim())
        .filter(|prefix| !prefix.is_empty())
        .collect::<Vec<_>>();
    let mut prefix_matches = 0_usize;
    let mut rejected_carts = Vec::new();
    let mut eligible_carts = Vec::new();
    for slot in &library.library().slots {
        let Some(voltag) = slot.cartridge.as_ref() else {
            continue;
        };
        if !tape_prefixes
            .iter()
            .any(|prefix| voltag.starts_with(prefix))
        {
            continue;
        }
        prefix_matches = prefix_matches.saturating_add(1);
        let tape = match index.ensure_cleaning_cartridge(voltag) {
            Ok(tape) => tape,
            Err(err) => {
                rejected_carts.push(format!(
                    "slot=0x{:04x} voltag={} registration={err}",
                    slot.element_address, voltag
                ));
                continue;
            }
        };
        let cleaning_state = match index.get_tape_cleaning_state(tape.tape_uuid.as_slice()) {
            Ok(state) => state.flatten(),
            Err(err) => {
                rejected_carts.push(format!(
                    "slot=0x{:04x} voltag={} state-query={err}",
                    slot.element_address, voltag
                ));
                continue;
            }
        };
        match cleaning_state.as_deref() {
            None | Some("unverified") | Some("ok") => {
                eligible_carts.push((slot.element_address, voltag.clone(), tape));
            }
            Some(state) => rejected_carts.push(format!(
                "slot=0x{:04x} voltag={} cleaning_state={state}",
                slot.element_address, voltag
            )),
        }
    }
    if eligible_carts.is_empty() {
        let rejection_summary = if rejected_carts.is_empty() {
            "none".to_string()
        } else {
            rejected_carts.join("; ")
        };
        let reason = format!(
            "no eligible cleaning cartridge in library {library_serial}: configured prefixes=[{}], inventory prefix matches={prefix_matches}, rejected=[{rejection_summary}]",
            tape_prefixes.join(",")
        );
        tracing::error!(
            target: "remanence_cleaning",
            library_serial,
            drive_uuid = %crate::bytes_to_hex(drive_uuid),
            reason,
            "cleaning cartridge selection failed"
        );
        let detail = format!(
            "{{\"reason\":\"{}\",\"recovery_step\":\"selecting\"}}",
            json_escape_text(&reason)
        );
        let _ = clear_alarm_with_evidence(
            index,
            cfg,
            format!("cleaning-needs-operator:{}", run.run_id).as_str(),
        );
        let _ = index.set_drive_fenced(drive_uuid, false);
        let _ = record_library_event(
            index,
            cfg,
            handle,
            library_serial.as_str(),
            AuditEvent::DriveUnfenced,
            BTreeMap::from([
                (
                    "drive_uuid".to_string(),
                    CborValue::Bytes(drive_uuid.to_vec()),
                ),
                (
                    "component".to_string(),
                    CborValue::Text("cleaning".to_string()),
                ),
            ]),
        );
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("no-cln-cart:{library_serial}").as_str(),
            "no-cln-cart",
            "critical",
            Some(detail.as_str()),
        );
        let _ = index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(detail.as_str()));
        return Err(Status::failed_precondition(reason));
    }
    eligible_carts.sort_by_key(|(slot, _, _)| *slot);
    let (slot_address, voltag, tape_row) = eligible_carts.remove(0);
    let selected = index
        .select_clean_run_cart(
            run.run_id.as_str(),
            tape_row.tape_uuid.as_slice(),
            i64::from(slot_address),
            Some("{\"phase\":\"selecting\"}"),
        )
        .map_err(status_from_state_error)?;
    let selected = selected.ok_or_else(|| Status::internal("selected clean run disappeared"))?;
    let run_id = selected.run_id.clone();
    let complete_timeout = parse_duration_or(&clean_cfg.complete_timeout, Duration::minutes(10));
    let drive_bay = drive
        .last_element_address
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| Status::failed_precondition("drive has no current bay"))?;
    retry_cleaning_move(index, cfg, run_id.as_str(), drive_uuid, "moving-in", || {
        library
            .load(slot_address, drive_bay, &cfg.policy)
            .map_err(|err| format!("load cleaning cartridge: {err}"))?;
        Ok(())
    })?;
    let load_completed = std::time::Instant::now();
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "moving-in",
            Some("{\"phase\":\"moving-in\"}"),
        )
        .map_err(status_from_state_error)?;
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "cleaning",
            Some("{\"phase\":\"cleaning\"}"),
        )
        .map_err(status_from_state_error)?;
    let min_cycle = parse_duration_or(&clean_cfg.min_cycle_duration, Duration::minutes(1));
    if load_completed.elapsed()
        > std::time::Duration::from_millis(complete_timeout.whole_milliseconds().max(0) as u64)
    {
        let detail = format!(
            "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"cart\":\"{}\",\"recovery_step\":\"timeout\"}}",
            json_escape_text(&run_id),
            json_escape_text(&crate::bytes_to_hex(drive_uuid)),
            json_escape_text(&voltag),
        );
        let _ = index.mark_clean_run_needs_operator(run_id.as_str(), Some(detail.as_str()));
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cleaning-needs-operator:{}", run_id).as_str(),
            "cleaning-needs-operator",
            "warning",
            Some(detail.as_str()),
        );
        return Err(Status::deadline_exceeded("cleaning timeout exceeded"));
    }
    if cleaning_drive_is_idle(library, drive_bay)? {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "expired")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(
                run_id.as_str(),
                "failed",
                Some("{\"reason\":\"fast-eject\"}"),
            )
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cln-cart-expired:{}", voltag).as_str(),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"fast-eject\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge fast-ejected during cleaning",
        ));
    }
    let elapsed = load_completed.elapsed();
    let min_cycle_millis = min_cycle.whole_milliseconds().max(0) as u64;
    if elapsed < std::time::Duration::from_millis(min_cycle_millis) {
        std::thread::sleep(std::time::Duration::from_millis(
            min_cycle_millis.saturating_sub(elapsed.as_millis() as u64),
        ));
    }
    let mut drive_handle = library
        .open_drive_with_tape_io(
            drive_bay,
            &cfg.policy,
            crate::tape_io_runtime_config(&cfg.tape_io),
        )
        .map_err(|err| Status::internal(format!("open drive for cleaning verify: {err}")))?;
    let alerts = drive_handle.read_tape_alerts().map_err(|err| {
        let _ = index.terminalize_clean_run(
            run_id.as_str(),
            "failed",
            Some("{\"reason\":\"verify-read-failed\"}"),
        );
        Status::unavailable(format!("read TapeAlert page: {err}"))
    })?;
    let active_alerts = alerts.active();
    if alerts.is_set(22) {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "expired")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(run_id.as_str(), "failed", Some("{\"reason\":\"flag-22\"}"))
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cln-cart-expired:{}", voltag).as_str(),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"flag-22\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge expired during cleaning",
        ));
    }
    if alerts.is_set(20) || alerts.is_set(21) {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "rejected")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(
                run_id.as_str(),
                "failed",
                Some("{\"reason\":\"corroboration\"}"),
            )
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cart-not-cleaning-behavior:{}", voltag).as_str(),
            "cart-not-cleaning-behavior",
            "warning",
            Some("{\"reason\":\"corroboration\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge behaved like data media",
        ));
    }
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "moving-back",
            Some("{\"phase\":\"moving-back\"}"),
        )
        .map_err(status_from_state_error)?;
    retry_cleaning_move(
        index,
        cfg,
        run_id.as_str(),
        drive_uuid,
        "moving-back",
        || {
            library
                .unload(drive_bay, Some(slot_address), &cfg.policy)
                .map_err(|err| format!("unload cleaning cartridge: {err}"))?;
            Ok(())
        },
    )?;
    let eject_observed = std::time::Instant::now();
    if eject_observed.duration_since(load_completed)
        < std::time::Duration::from_millis(min_cycle_millis)
    {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "expired")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(
                run_id.as_str(),
                "failed",
                Some("{\"reason\":\"fast-eject\"}"),
            )
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cln-cart-expired:{}", voltag).as_str(),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"fast-eject\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge fast-ejected during cleaning",
        ));
    }
    let detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"cart\":\"{}\",\"recovery_step\":\"verify\"}}",
        json_escape_text(&run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        json_escape_text(&voltag),
    );
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "verifying",
            Some("{\"phase\":\"verifying\"}"),
        )
        .map_err(status_from_state_error)?;
    index
        .finalize_verified_clean_run(
            run_id.as_str(),
            drive_uuid,
            Some(tape_row.tape_uuid.as_slice()),
            Some(detail.as_str()),
        )
        .map_err(status_from_state_error)?;
    let _ = clear_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run_id).as_str(),
    );
    let _ = clear_alarm_with_evidence(
        index,
        cfg,
        format!(
            "drive-cleaning-abnormal-frequency:{}",
            crate::bytes_to_hex(drive_uuid)
        )
        .as_str(),
    );
    let _ = clear_alarm_with_evidence(index, cfg, format!("cln-cart-expired:{}", voltag).as_str());
    let _ = clear_alarm_with_evidence(
        index,
        cfg,
        format!("cart-not-cleaning-behavior:{}", voltag).as_str(),
    );
    let _ = active_alerts;
    let _ = record_library_event(
        index,
        cfg,
        handle,
        library_serial.as_str(),
        AuditEvent::DriveUnfenced,
        BTreeMap::from([
            (
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.to_vec()),
            ),
            (
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            ),
        ]),
    );
    let _ = record_library_event(
        index,
        cfg,
        handle,
        library_serial.as_str(),
        AuditEvent::DriveCleaned,
        BTreeMap::from([
            (
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.to_vec()),
            ),
            (
                "cart_tape_uuid".to_string(),
                CborValue::Bytes(tape_row.tape_uuid.clone()),
            ),
            (
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            ),
        ]),
    );
    Ok(())
}

fn cleaning_too_soon(
    index: &CatalogIndex,
    drive_uuid: &[u8],
    min_interval: Duration,
    weekly_cap: usize,
) -> Result<bool, Status> {
    let runs = index
        .list_clean_runs(true)
        .map_err(status_from_state_error)?;
    let mut completed = Vec::new();
    for run in runs {
        if run.drive_uuid.as_slice() != drive_uuid {
            continue;
        }
        if run.phase != "done" {
            continue;
        }
        if let Ok(parsed) = OffsetDateTime::parse(run.updated_at_utc.as_str(), &Rfc3339) {
            completed.push(parsed);
        }
    }
    completed.sort_unstable();
    if let Some(last) = completed.last().copied() {
        let since = OffsetDateTime::now_utc() - last;
        if since < min_interval {
            return Ok(true);
        }
    }
    if weekly_cap > 0 {
        let week_ago = OffsetDateTime::now_utc() - Duration::days(7);
        if completed.iter().filter(|value| **value >= week_ago).count() >= weekly_cap {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_duration_or(value: &str, default: Duration) -> Duration {
    parse_simple_duration(value).unwrap_or(default)
}

fn parse_simple_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let split = value.find(|ch: char| !ch.is_ascii_digit())?;
    let (digits, unit) = value.split_at(split);
    let count = digits.parse::<i64>().ok()?;
    match unit {
        "ms" => Some(Duration::milliseconds(count)),
        "s" => Some(Duration::seconds(count)),
        "m" => Some(Duration::minutes(count)),
        "h" => Some(Duration::hours(count)),
        _ => None,
    }
}

fn json_escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn cleaning_drive_is_idle(
    library: &mut remanence_library::LibraryHandle,
    drive_bay: u16,
) -> Result<bool, Status> {
    library
        .refresh()
        .map_err(|err| Status::unavailable(format!("refresh library during cleaning: {err}")))?;
    Ok(library
        .library()
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == drive_bay)
        .map(|bay| !bay.loaded)
        .unwrap_or(true))
}

fn retry_cleaning_move(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run_id: &str,
    drive_uuid: &[u8],
    label: &str,
    mut op: impl FnMut() -> Result<(), String>,
) -> Result<(), Status> {
    let mut last_err = None;
    for attempt in 0..2 {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt == 0 {
                    tracing::warn!("{label} failed once during cleaning; retrying");
                }
            }
        }
    }
    let err = last_err.unwrap_or_else(|| "move failed".to_string());
    let detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"recovery_step\":\"{}\",\"error\":\"{}\"}}",
        json_escape_text(run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        json_escape_text(label),
        json_escape_text(&err),
    );
    let _ = index.terminalize_clean_run(run_id, "failed", Some(detail.as_str()));
    let _ = raise_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(detail.as_str()),
    );
    Err(Status::internal(err))
}

fn handle_reconcile(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
    handle: crate::operations::OperationHandle,
) {
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled before dispatch");
        return;
    }
    publish_running(&handle, &[("phase", "mount")]);
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        &handle,
        &tape_uuid,
        AuditEvent::OperationStarted,
        BTreeMap::new(),
    ) {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let library_serial = match cfg.default_library_serial.as_deref() {
        Some(serial) => serial,
        None => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                "tape reconciliation requires exactly one configured library in this slice",
                &[("phase", "mount")],
            );
            return;
        }
    };
    let lib = match cfg.report.library(library_serial) {
        Some(lib) => lib,
        None => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("open library: {err}"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    let mut drive = match load_tape_by_uuid(index, &mut library, &cfg.policy, &tape_uuid) {
        Ok(drive) => drive,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("mount tape: {err}"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    if let Err(status) = verify_loaded_tape_identity(&mut drive, &tape_uuid) {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            &format!("tape identity: {}", status.message()),
            &[("phase", "mount")],
        );
        return;
    }
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled after mount");
        return;
    }

    let tape = match index.get_tape(&tape_uuid) {
        Ok(Some(tape)) => tape,
        Ok(None) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                "tape not found in catalog",
                &[("phase", "scan")],
            );
            return;
        }
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("catalog lookup: {err}"),
                &[("phase", "scan")],
            );
            return;
        }
    };
    let Some(block_size) = tape
        .block_size
        .and_then(|block_size| u32::try_from(block_size).ok())
    else {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            "tape block size is unknown or outside u32 range",
            &[("phase", "scan")],
        );
        return;
    };

    publish_running(&handle, &[("phase", "scan")]);
    let scan = {
        let mut source = DriveHandleRawSource::new(&mut drive);
        scan_reconstruct_filemark_map(&mut source, &tape_uuid, block_size)
    };
    let scan = match scan {
        Ok(scan) => scan,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("scan filemark map: {err}"),
                &[("phase", "scan")],
            );
            return;
        }
    };
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled after scan");
        return;
    }

    match reconcile_tape_files(index, &tape_uuid, &scan, &handle) {
        Ok(report) => {
            let rebuilt = report.tape_files_rebuilt.to_string();
            let mut detail = BTreeMap::new();
            detail.insert("tape_files".to_string(), CborValue::Text(rebuilt.clone()));
            if let Err(err) = record_reconcile_event(
                index,
                cfg,
                &handle,
                &tape_uuid,
                AuditEvent::OperationFinished,
                detail,
            ) {
                fail_operation(
                    index,
                    cfg,
                    &handle,
                    &tape_uuid,
                    &format!("record operation finish audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(
                pb::OperationState::Succeeded,
                &[("phase", "complete"), ("tape_files", rebuilt.as_str())],
            );
        }
        Err(ReconcileExit::Cancelled(message)) => {
            cancel_operation(index, cfg, &handle, &tape_uuid, message.as_str());
        }
        Err(ReconcileExit::Failed(message)) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                message.as_str(),
                &[("phase", "project")],
            );
        }
    }
}

enum ReconcileExit {
    Cancelled(String),
    Failed(String),
}

fn reconcile_tape_files(
    index: &mut CatalogIndex,
    tape_uuid: &[u8; 16],
    scan: &FilemarkMap,
    handle: &crate::operations::OperationHandle,
) -> Result<remanence_state::TapeJournalIndexReport, ReconcileExit> {
    let existing = index
        .list_tape_files(tape_uuid)
        .map_err(|err| ReconcileExit::Failed(format!("list existing tape files: {err}")))?;
    let existing_object_ids = existing
        .into_iter()
        .filter(|entry| entry.kind == "object")
        .filter_map(|entry| entry.object_id.map(|id| (entry.tape_file_number, id)))
        .collect::<HashMap<_, _>>();

    let mut entries = Vec::with_capacity(scan.entries().len());
    for (idx, map_entry) in scan.entries().iter().enumerate() {
        if handle.is_cancelled() {
            return Err(ReconcileExit::Cancelled(format!(
                "cancelled after {} tape files",
                entries.len()
            )));
        }
        let mut entry = TapeFileEntry::from_map_entry(map_entry.clone());
        if map_entry.kind == TapeFileKind::Object {
            entry.object_id = existing_object_ids
                .get(&map_entry.tape_file_number)
                .cloned();
        }
        entries.push(entry);
        let count = (idx + 1).to_string();
        publish_running(
            handle,
            &[("phase", "project"), ("tape_files", count.as_str())],
        );
    }

    index
        .reconcile_tape_files_projection(
            *tape_uuid,
            &entries,
            scan.max_sidecar_end_exclusive(),
            scan.total_data_ordinals(),
        )
        .map_err(|err| ReconcileExit::Failed(format!("project tape files: {err}")))
}

fn fail_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        handle,
        tape_uuid,
        AuditEvent::OperationFailed,
        detail,
    ) {
        let audit_error = format!("{error_summary}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), progress);
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

fn cancel_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        handle,
        tape_uuid,
        AuditEvent::CancelledBeforeDispatch,
        detail,
    ) {
        let audit_error = format!("{detail_message}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), &[("phase", "audit")]);
    } else {
        handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", detail_message)],
        );
    }
}

fn publish_running(handle: &crate::operations::OperationHandle, progress: &[(&str, &str)]) {
    handle.publish_state(pb::OperationState::Running, progress);
}

fn record_reconcile_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    if tape_uuid.iter().any(|byte| *byte != 0) {
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
    }
    let subject_id = if tape_uuid.iter().any(|byte| *byte != 0) {
        Some(Uuid::from_bytes(*tape_uuid).to_string())
    } else {
        Some(handle.op_id_uuid().to_string())
    };
    let subject_kind = if tape_uuid.iter().any(|byte| *byte != 0) {
        "tape"
    } else {
        "operation"
    };
    crate::append_operation_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::OperationAuditInput {
            actor: AuditActor::System,
            operation_id: handle.op_id_uuid(),
            operation_kind: handle.operation_kind(),
            event,
            subject_kind,
            subject_id,
            idempotency_key: None,
            detail,
        },
    )
}

#[derive(Clone, Copy, Debug, Default)]
struct RestoreReadPhases {
    position: StdDuration,
    transfer: StdDuration,
    bytes: u64,
    records: u64,
    commands: u64,
}

#[derive(Clone, Copy, Debug)]
struct RestoreDiagnosticContext {
    session_id: Uuid,
    tape_uuid: [u8; 16],
    block_size_bytes: u32,
    success: bool,
}

/// Times the existing `BlockSource` safety funnel without reimplementing any
/// tape operation. Every call delegates exactly once to the wrapped source.
struct DiagnosticBlockSource<'a> {
    inner: &'a mut dyn BlockSource,
    phases: RestoreReadPhases,
}

impl<'a> DiagnosticBlockSource<'a> {
    fn new(inner: &'a mut dyn BlockSource) -> Self {
        Self {
            inner,
            phases: RestoreReadPhases::default(),
        }
    }

    fn phases(&self) -> RestoreReadPhases {
        self.phases
    }
}

impl remanence_library::BlockRead for DiagnosticBlockSource<'_> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.read_block(buf);
        self.phases.transfer += started.elapsed();
        if let Ok(bytes) = result {
            self.phases.commands = self.phases.commands.saturating_add(1);
            self.phases.records = self.phases.records.saturating_add(1);
            self.phases.bytes = self.phases.bytes.saturating_add(bytes as u64);
        }
        result
    }
}

impl BlockSource for DiagnosticBlockSource<'_> {
    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<ReadBatchOutcome, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.read_block_batch(
            buf,
            block_size_bytes,
            requested_records,
            remaining_records_in_file,
        );
        self.phases.transfer += started.elapsed();
        if let Ok(outcome) = result {
            self.phases.commands = self.phases.commands.saturating_add(1);
            self.phases.records = self
                .phases
                .records
                .saturating_add(u64::from(outcome.records_read));
            self.phases.bytes = self
                .phases
                .bytes
                .saturating_add(u64::from(outcome.bytes_read));
        }
        result
    }

    fn read_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.read_batch_blocks(block_size_bytes)
    }

    fn read_ring_buffers(&self) -> u32 {
        self.inner.read_ring_buffers()
    }

    fn prove_read_position(
        &mut self,
        expected: TapePosition,
    ) -> Result<remanence_library::DevicePositionProof, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.prove_read_position(expected);
        self.phases.position += started.elapsed();
        result
    }

    fn rewind(&mut self) -> Result<(), TapeIoError> {
        let started = Instant::now();
        let result = self.inner.rewind();
        self.phases.position += started.elapsed();
        result
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.locate(lba);
        self.phases.position += started.elapsed();
        result
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.space(count, kind);
        self.phases.position += started.elapsed();
        result
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.position();
        self.phases.position += started.elapsed();
        result
    }
}

fn log_restore_read_diagnostics(
    drive: &DriveHandle,
    context: RestoreDiagnosticContext,
    phases: RestoreReadPhases,
    relay_diagnostics: StagedReadRelayDiagnostics,
    wall: StdDuration,
) {
    let relay = relay_diagnostics.client_write;
    let phase_sum = wall;
    let bottleneck = if phases.transfer >= relay {
        "drive"
    } else {
        "sender"
    };
    let diagnostics = drive.pipelined_read_diagnostics();
    let effective_batch_blocks = drive.requested_read_batch_blocks().min(
        drive
            .sg_reserved_size_bytes()
            .checked_div(context.block_size_bytes.max(1))
            .unwrap_or(1)
            .max(1),
    );
    let batch_effectiveness = if phases.commands == 0 {
        0.0
    } else {
        phases.records as f64 / phases.commands as f64
    };
    tracing::info!(
        target: "remanence_read_diag",
        phase = "restore_total",
        session_id = %context.session_id,
        tape_uuid = %Uuid::from_bytes(context.tape_uuid),
        status = if context.success { "ok" } else { "error" },
        effective_mode = "fixed_pipelined",
        block_size_bytes = context.block_size_bytes,
        staging_ring_buffers = drive.staging_ring_buffers(),
        effective_batch_blocks,
        batch_effectiveness_records_per_command = batch_effectiveness,
        bytes = phases.bytes,
        records = phases.records,
        commands = phases.commands,
        locate_position_ms = crate::diagnostics::duration_ms(phases.position),
        transfer_ms = crate::diagnostics::duration_ms(phases.transfer),
        relay_ms = crate::diagnostics::duration_ms(relay),
        phase_sum_ms = crate::diagnostics::duration_ms(phase_sum),
        wall_ms = crate::diagnostics::duration_ms(wall),
        bottleneck,
        drive_rate_mib_s = crate::diagnostics::mib_per_s(phases.bytes, phases.transfer),
        relay_rate_mib_s = crate::diagnostics::mib_per_s(phases.bytes, relay),
        client_write_ms = crate::diagnostics::duration_ms(relay_diagnostics.client_write),
        sender_stall_ms = crate::diagnostics::duration_ms(relay_diagnostics.sender_stall),
        client_write_bytes = relay_diagnostics.bytes,
        client_write_rate_mib_s = crate::diagnostics::mib_per_s(
            relay_diagnostics.bytes,
            relay_diagnostics.client_write,
        ),
        gap_samples = diagnostics.gap_samples,
        ioctl_samples = diagnostics.ioctl_samples,
        gap_p50_us = diagnostics.gap_p50_us,
        gap_p95_us = diagnostics.gap_p95_us,
        gap_max_us = diagnostics.gap_max_us,
        ioctl_p50_us = diagnostics.ioctl_p50_us,
        ioctl_p95_us = diagnostics.ioctl_p95_us,
        ioctl_max_us = diagnostics.ioctl_max_us,
        ioctl_mean_us = diagnostics.ioctl_mean_us,
        first_60s_ioctl_samples = diagnostics.first_60s_ioctl_samples,
        first_60s_ioctl_p50_us = diagnostics.first_60s_ioctl_p50_us,
        first_60s_ioctl_p95_us = diagnostics.first_60s_ioctl_p95_us,
        first_60s_ioctl_max_us = diagnostics.first_60s_ioctl_max_us,
        first_60s_ioctl_mean_us = diagnostics.first_60s_ioctl_mean_us,
        accounting_samples = diagnostics.accounting_samples,
        accounting_p50_us = diagnostics.accounting_p50_us,
        accounting_p95_us = diagnostics.accounting_p95_us,
        accounting_max_us = diagnostics.accounting_max_us,
        accounting_mean_us = diagnostics.accounting_mean_us,
        cadence_us = diagnostics.cadence_us,
        effective_feed_bytes_per_second = diagnostics.effective_feed_bytes_per_second,
        time_to_first_ioctl_ms = diagnostics.time_to_first_ioctl_ms,
        steady_reached = diagnostics.steady_reached,
        time_to_steady_ms = diagnostics.time_to_steady_ms,
        steady_window_seconds = diagnostics.steady_window_seconds,
        steady_threshold_percent = diagnostics.steady_threshold_percent,
        ramp_observation_seconds = diagnostics.ramp_observation_seconds,
        "remanence_read_diag",
    );
}

#[cfg(test)]
fn exclusive_restore_relay_phase(
    wall: StdDuration,
    position: StdDuration,
    transfer: StdDuration,
) -> StdDuration {
    wall.saturating_sub(position).saturating_sub(transfer)
}

#[allow(clippy::too_many_arguments)]
fn stream_one_object(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
    tape_uuid: &[u8; 16],
    object_id: &str,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
) -> Result<(), Status> {
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let manifest_sha256 = object
        .metadata_hash
        .as_deref()
        .map(|hash| {
            <[u8; 32]>::try_from(hash)
                .map_err(|_| Status::internal("catalog metadata_hash is not 32 bytes"))
        })
        .transpose()?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let tape_files = index
        .list_tape_files(tape_uuid)
        .map_err(status_from_state_error)?;
    let tape_file = tape_files
        .iter()
        .find(|file| {
            file.tape_file_number == copy.tape_file_number
                && file.kind == "object"
                && file.object_id.as_deref() == Some(object_id)
        })
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::internal("tape block size unknown"))?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit usize"))?;

    let block_size_u32 = u32::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before object read: {err}")))?;

    drive.reset_pipelined_diagnostics();
    let wall_started = Instant::now();
    let (result, phases) = {
        let mut source = DriveHandleSource(drive);
        let mut diagnostic_source = DiagnosticBlockSource::new(&mut source);
        let result = stream_with_staged_read_sender_diagnostics(
            chunk_tx,
            stream_chunk_bytes,
            |writer, terminal| {
                let mut sink = crate::read_core::CapturePayloadSink::new(writer);
                crate::read_core::read_object_payload_with_pipeline(
                    &mut diagnostic_source,
                    block_size_usize,
                    tape_file.block_count,
                    copy.tape_file_number,
                    manifest_sha256,
                    &mut sink,
                    crate::read_core::ReadPipelineConfig {
                        reservoir_bytes: cfg.tape_io.read_reservoir_bytes,
                        high_pct: cfg.tape_io.read_reservoir_high_pct,
                        low_pct: cfg.tape_io.read_reservoir_low_pct,
                        ranged_frontier: false,
                        proof_cadence_bytes: cfg
                            .tape_io
                            .position_check_bytes_ranged
                            .min(cfg.tape_io.read_reservoir_bytes / 2),
                        terminal: Some(terminal),
                    },
                    Arc::clone(&cfg.io_memory),
                )
                .map_err(|err| Status::internal(format!("read object: {err}")))?;
                let (_payload_bytes, _digest) = sink
                    .finish()
                    .map_err(|err| Status::internal(format!("finish payload stream: {err}")))?;
                Ok(())
            },
        );
        (result, diagnostic_source.phases())
    };
    let wall = wall_started.elapsed();
    log_restore_read_diagnostics(
        drive,
        RestoreDiagnosticContext {
            session_id,
            tape_uuid: *tape_uuid,
            block_size_bytes: block_size_u32,
            success: result.is_ok(),
        },
        phases,
        result.as_ref().copied().unwrap_or_default(),
        wall,
    );
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn stream_one_file_range(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
) -> Result<(), Status> {
    let request =
        file_range_read_request(index, tape_uuid, object_id, file_id, start_byte, end_byte)?;
    let block_size_u32 = u32::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;

    drive.reset_pipelined_diagnostics();
    let wall_started = Instant::now();
    let (result, phases) = {
        let mut source = DriveHandleSource(drive);
        let mut diagnostic_source = DiagnosticBlockSource::new(&mut source);
        let result = stream_file_range_from_source(
            &mut diagnostic_source,
            request,
            stream_chunk_bytes,
            chunk_tx,
            &cfg.tape_io,
            Arc::clone(&cfg.io_memory),
        );
        (result, diagnostic_source.phases())
    };
    let wall = wall_started.elapsed();
    log_restore_read_diagnostics(
        drive,
        RestoreDiagnosticContext {
            session_id,
            tape_uuid: *tape_uuid,
            block_size_bytes: block_size_u32,
            success: result.is_ok(),
        },
        phases,
        result.as_ref().copied().unwrap_or_default(),
        wall,
    );
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn file_range_read_request(
    index: &CatalogIndex,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
) -> Result<crate::read_core::PlaintextFileRangeReadRequest, Status> {
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let file = resolve_object_file_for_range(index, object_id, file_id)?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let (range_start, range_len) = requested_file_range(file.size_bytes, start_byte, end_byte)?;

    let tape_files = index
        .list_tape_files(tape_uuid)
        .map_err(status_from_state_error)?;
    let tape_file = tape_files
        .iter()
        .find(|tape_file| {
            tape_file.tape_file_number == copy.tape_file_number
                && tape_file.kind == "object"
                && tape_file.object_id.as_deref() == Some(object_id)
        })
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::internal("tape block size unknown"))?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit usize"))?;
    let physical_file_start_lba =
        derive_physical_file_start_lba(tape_files.as_slice(), tape_file.tape_file_number);
    Ok(crate::read_core::PlaintextFileRangeReadRequest {
        block_size: block_size_usize,
        tape_file_number: tape_file.tape_file_number,
        physical_file_start_lba,
        first_chunk_lba: file.first_chunk_lba.map(BodyLba),
        file_size_bytes: file.size_bytes,
        range_start,
        range_len,
    })
}

/// Derive an absolute tape-file start from the dense committed catalog prefix.
/// Each trailing filemark consumes one physical LBA, matching the filemark-map
/// physical-position calculation. An incomplete or non-dense prefix returns
/// `None` so the range reader uses its logical REWIND/SPACE fallback.
fn derive_physical_file_start_lba(
    tape_files: &[remanence_state::TapeFileRecord],
    target_file_number: u64,
) -> Option<u64> {
    let mut expected_file_number = 0u64;
    let mut next_file_lba = 0u64;
    for tape_file in tape_files {
        if tape_file.tape_file_number != expected_file_number {
            return None;
        }
        if tape_file.tape_file_number == target_file_number {
            return Some(next_file_lba);
        }
        next_file_lba = next_file_lba
            .checked_add(tape_file.block_count)?
            .checked_add(1)?;
        expected_file_number = expected_file_number.checked_add(1)?;
    }
    None
}

fn resolve_object_file_for_range(
    index: &CatalogIndex,
    object_id: &str,
    file_id: &str,
) -> Result<NativeObjectFileRecord, Status> {
    if file_id.is_empty() {
        let files = index
            .list_native_object_files(object_id)
            .map_err(status_from_state_error)?;
        return match files.as_slice() {
            [file] => Ok(file.clone()),
            [] => Err(Status::failed_precondition(
                "empty file_id ranged reads require exactly one object file row; found 0",
            )),
            _ => Err(Status::failed_precondition(format!(
                "empty file_id ranged reads require exactly one object file row; found {}",
                files.len()
            ))),
        };
    }

    let file = index
        .get_native_object_file(object_id, file_id)
        .map_err(status_from_state_error)?;
    file.ok_or_else(|| Status::not_found("object file not found"))
}

fn stream_file_range_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
    tape_io: &TapeIoConfig,
    io_memory: Arc<crate::io_memory::IoMemoryReservation>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    // Ranged reads are opaque stored-payload reads. The daemon does not decrypt
    // or hold key material; clients interpret or decrypt the returned bytes.
    stream_with_staged_read_sender_diagnostics(chunk_tx, stream_chunk_bytes, |writer, terminal| {
        crate::read_core::read_plaintext_file_range_with_pipeline(
            source,
            request,
            writer,
            crate::read_core::ReadPipelineConfig {
                reservoir_bytes: tape_io.read_reservoir_bytes,
                high_pct: tape_io.read_reservoir_high_pct,
                low_pct: tape_io.read_reservoir_low_pct,
                ranged_frontier: true,
                proof_cadence_bytes: tape_io
                    .position_check_bytes_ranged
                    .min(tape_io.read_reservoir_bytes / 2),
                terminal: Some(terminal),
            },
            io_memory,
        )
        .map_err(status_from_file_range_error)
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct StagedReadRelayDiagnostics {
    client_write: StdDuration,
    sender_stall: StdDuration,
    bytes: u64,
}

fn stream_with_staged_read_sender_diagnostics(
    chunk_tx: crate::read_core::ReadStreamSender,
    stream_chunk_bytes: u32,
    produce: impl FnOnce(
        &mut (dyn std::io::Write + Send),
        Arc<crate::read_core::ReadTerminalAccumulator>,
    ) -> Result<(), Status>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    let staged_capacity = crate::read_core::read_stream_channel_capacity(
        usize::try_from(stream_chunk_bytes).unwrap_or(usize::MAX),
    );
    let (tx, rx) = std_mpsc::sync_channel(staged_capacity);
    let poison = Arc::new(Mutex::new(None::<String>));
    let terminal = Arc::new(crate::read_core::ReadTerminalAccumulator::default());
    std::thread::scope(|scope| {
        let sender_poison = Arc::clone(&poison);
        let sender_terminal = Arc::clone(&terminal);
        let sender = scope.spawn(move || {
            let result = drain_staged_read_sender(rx, chunk_tx, stream_chunk_bytes, sender_poison);
            if let Err(status) = &result {
                sender_terminal.record(
                    crate::read_core::ReadTerminalPriority::Sender,
                    status.clone(),
                );
            }
            result
        });
        let mut writer = StagedReadWriter::new(
            tx,
            Arc::clone(&poison),
            usize::try_from(stream_chunk_bytes).unwrap_or(usize::MAX),
        );
        let produce_result = produce(&mut writer, Arc::clone(&terminal)).and_then(|()| {
            writer
                .finish()
                .map_err(|err| Status::internal(format!("finish read stream: {err}")))
        });
        if let Err(status) = &produce_result {
            terminal.record(
                crate::read_core::ReadTerminalPriority::Decode,
                status.clone(),
            );
        }
        drop(writer);
        let sender_result = sender.join().unwrap_or_else(|_| {
            let status = Status::internal("staged read sender thread panicked");
            terminal.record(
                crate::read_core::ReadTerminalPriority::Sender,
                status.clone(),
            );
            Err(status)
        });
        match (produce_result, sender_result) {
            (Ok(()), Ok(diagnostics)) => Ok(diagnostics),
            _ => Err(terminal.finalize_after_join().unwrap_or_else(|| {
                Status::internal("read pipeline failed without terminal cause")
            })),
        }
    })
}

enum StagedReadItem {
    Data(Vec<u8>),
    Finish,
}

struct StagedReadWriter {
    tx: std_mpsc::SyncSender<StagedReadItem>,
    poison: Arc<Mutex<Option<String>>>,
    finished: bool,
    max_chunk_bytes: usize,
}

impl StagedReadWriter {
    fn new(
        tx: std_mpsc::SyncSender<StagedReadItem>,
        poison: Arc<Mutex<Option<String>>>,
        chunk_bytes: usize,
    ) -> Self {
        Self {
            tx,
            poison,
            finished: false,
            max_chunk_bytes: crate::read_core::effective_read_stream_chunk_bytes(chunk_bytes),
        }
    }

    fn check_poison(&self) -> std::io::Result<()> {
        if let Some(message) = self
            .poison
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("staged read sender failed: {message}"),
            ))
        } else {
            Ok(())
        }
    }

    fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.check_poison()?;
        self.tx.send(StagedReadItem::Finish).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "staged read sender stopped")
        })?;
        self.finished = true;
        self.check_poison()
    }
}

impl std::io::Write for StagedReadWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.finished {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "staged read stream already finished",
            ));
        }
        self.check_poison()?;
        for chunk in buf.chunks(self.max_chunk_bytes) {
            self.tx
                .send(StagedReadItem::Data(chunk.to_vec()))
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "staged read sender stopped",
                    )
                })?;
        }
        self.check_poison()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.check_poison()
    }
}

fn drain_staged_read_sender(
    rx: std_mpsc::Receiver<StagedReadItem>,
    chunk_tx: crate::read_core::ReadStreamSender,
    stream_chunk_bytes: u32,
    poison: Arc<Mutex<Option<String>>>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    let mut writer = Some(if stream_chunk_bytes == 0 {
        crate::read_core::ChannelWriter::new(chunk_tx)
    } else {
        crate::read_core::ChannelWriter::with_chunk_size(chunk_tx, stream_chunk_bytes as usize)
    });
    let mut first_error = None;
    let mut diagnostics = StagedReadRelayDiagnostics::default();
    while let Ok(item) = rx.recv() {
        if first_error.is_some() {
            continue;
        }
        let client_started = Instant::now();
        let result = match item {
            StagedReadItem::Data(bytes) => match writer.as_mut() {
                Some(writer) => {
                    let bytes_len = bytes.len() as u64;
                    let result = writer
                        .write_all(&bytes)
                        .map_err(|err| Status::internal(format!("send read stream: {err}")));
                    diagnostics.sender_stall = writer.sender_stall();
                    if result.is_ok() {
                        diagnostics.bytes = diagnostics.bytes.saturating_add(bytes_len);
                    }
                    result
                }
                None => Err(Status::internal("staged read data after finish")),
            },
            StagedReadItem::Finish => match writer.take() {
                Some(mut writer) => {
                    let result = writer
                        .finish()
                        .map_err(|err| Status::internal(format!("finish read stream: {err}")));
                    diagnostics.sender_stall = writer.sender_stall();
                    result
                }
                None => Ok(()),
            },
        };
        diagnostics.client_write += client_started.elapsed();
        if let Err(status) = result {
            set_staged_read_poison(&poison, status.message());
            first_error = Some(status);
        }
    }
    match first_error {
        Some(status) => Err(status),
        None => Ok(diagnostics),
    }
}

fn set_staged_read_poison(poison: &Arc<Mutex<Option<String>>>, message: &str) {
    let mut guard = poison.lock().unwrap_or_else(|err| err.into_inner());
    guard.get_or_insert_with(|| message.to_string());
}

fn requested_file_range(
    file_size_bytes: u64,
    start_byte: u64,
    end_byte: u64,
) -> Result<(u64, u64), Status> {
    if start_byte == 0 && end_byte == 0 {
        return Ok((0, file_size_bytes));
    }
    let range_len = end_byte.checked_sub(start_byte).ok_or_else(|| {
        Status::invalid_argument("end_byte must be greater than or equal to start_byte")
    })?;
    Ok((start_byte, range_len))
}

fn status_from_file_range_error(err: FormatError) -> Status {
    match err {
        FormatError::InvalidInput(message) => Status::invalid_argument(message),
        other => Status::internal(format!("read object range: {other}")),
    }
}

fn verify_loaded_tape_identity(
    drive: &mut DriveHandle,
    tape_uuid: &[u8; 16],
) -> Result<(), Status> {
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before read: {err}")))?;
    let mut source = DriveHandleSource(drive);
    verify_tape_identity(&mut source, tape_uuid)
        .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    Ok(())
}

fn position_read_resume(
    index: &CatalogIndex,
    drive: &mut DriveHandle,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    let request = file_range_read_request(
        index,
        &target.tape_uuid,
        target.object_id.as_str(),
        target.file_id.as_str(),
        0,
        0,
    )?;
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before resume position: {err}")))?;
    let mut source = DriveHandleSource(drive);
    verify_and_position_read_resume_from_source(&mut source, request, target)
}

fn verify_and_position_read_resume_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    verify_tape_identity(source, &target.tape_uuid)
        .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    source
        .locate(0)
        .map_err(|err| Status::internal(format!("return to BOT after identity proof: {err}")))?;
    position_read_resume_from_source(source, request, target)
}

fn position_read_resume_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    let first_chunk_lba = request.first_chunk_lba.ok_or_else(|| {
        Status::failed_precondition("resume target file has no data-chunk boundary")
    })?;
    let block_size = u64::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u64"))?;
    let catalog_boundary = first_chunk_lba
        .0
        .checked_mul(block_size)
        .ok_or_else(|| Status::internal("catalogued file boundary byte offset overflow"))?;
    if target.file_boundary_byte_offset != catalog_boundary {
        return Err(Status::invalid_argument(format!(
            "resume offset is not the catalogued file boundary: expected {catalog_boundary}, got {}",
            target.file_boundary_byte_offset
        )));
    }

    let tape_file_spacing = i64::try_from(request.tape_file_number)
        .map_err(|_| Status::invalid_argument("tape file number exceeds SPACE range"))?;
    let mut positioned = source
        .space(tape_file_spacing, SpaceKind::Filemarks)
        .map_err(|err| Status::internal(format!("space to resume object: {err}")))?
        .position_after;
    let skip_blocks = i64::try_from(first_chunk_lba.0)
        .map_err(|_| Status::invalid_argument("resume file boundary exceeds SPACE range"))?;
    if skip_blocks != 0 {
        positioned = source
            .space(skip_blocks, SpaceKind::Blocks)
            .map_err(|err| Status::internal(format!("space to resume file boundary: {err}")))?
            .position_after;
    }
    let proof = source
        .prove_read_position(positioned)
        .map_err(|err| Status::failed_precondition(format!("resume position proof: {err}")))?;
    if let Some(expected) = target.expected_position_lba {
        if proof.lba() != expected {
            return Err(Status::failed_precondition(format!(
                "resume position proof mismatch: expected LBA {expected}, observed {}",
                proof.lba()
            )));
        }
    }
    Ok(proof.lba())
}

fn read_session_proto(
    session_id: Uuid,
    tape_uuid: &TapeUuid,
    state: pb::read_session::State,
    opened_at_utc: &str,
    drive_element_address: u16,
    position_after_lba: Option<u64>,
    daemon_epoch: u64,
) -> pb::ReadSession {
    pb::ReadSession {
        session_id: session_id.as_bytes().to_vec(),
        // This projection is handed both values by its caller, so Some is the
        // truth here. The presence exists for callers further up that open a
        // session against a drive rather than a named volume.
        tape_uuid: Some(tape_uuid.to_vec()),
        drive_element_address: Some(u32::from(drive_element_address)),
        state: state as i32,
        opened_at: timestamp_from_rfc3339(opened_at_utc),
        position_proof: position_after_lba
            .map(|position_after_lba| pb::DevicePositionProof { position_after_lba }),
        daemon_epoch,
    }
}

struct WriteSessionProtoInput<'a> {
    session_id: Uuid,
    tape_uuid: &'a TapeUuid,
    target_kind: pb::write_session::TargetKind,
    state: pb::write_session::State,
    objects_committed: u64,
    bytes_committed: u64,
    opened_at_utc: &'a str,
    last_checkpoint_at_utc: Option<&'a str>,
    drive_element_address: u16,
    pending_batch: Option<&'a PendingCheckpointBatch>,
}

fn session_proto(input: WriteSessionProtoInput<'_>) -> pb::WriteSession {
    let checkpoint_deadline = input.pending_batch.map(|batch| {
        let remaining = batch.deadline.saturating_duration_since(Instant::now());
        let seconds = OffsetDateTime::now_utc()
            .unix_timestamp()
            .saturating_add(i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX));
        prost_types::Timestamp { seconds, nanos: 0 }
    });
    pb::WriteSession {
        session_id: input.session_id.as_bytes().to_vec(),
        tape_uuid: Some(input.tape_uuid.to_vec()),
        drive_element_address: Some(u32::from(input.drive_element_address)),
        body_format: "rem-object-v1".to_string(),
        state: input.state as i32,
        objects_committed: input.objects_committed,
        bytes_committed: input.bytes_committed,
        opened_at: timestamp_from_rfc3339(input.opened_at_utc),
        last_checkpoint_at: input
            .last_checkpoint_at_utc
            .and_then(timestamp_from_rfc3339),
        target_kind: input.target_kind as i32,
        tape_sequence: vec![input.tape_uuid.to_vec()],
        current_tape_index: 0,
        pending_checkpoint_objects: input
            .pending_batch
            .map_or(0, |batch| batch.objects.len() as u64),
        pending_checkpoint_bytes: input.pending_batch.map_or(0, |batch| batch.logical_bytes),
        // `.map`, not `.map_or(0, ..)`: no pending batch means there is no
        // oldest pending object to have an age. Zero is what an object that
        // arrived this instant reports, so the old default made "nothing
        // waiting" and "something waiting, just now" identical -- and those
        // are opposite answers to "is this session behind on checkpoints?".
        oldest_pending_age_seconds: input
            .pending_batch
            .map(|batch| batch.opened_at.elapsed().as_secs()),
        checkpoint_deadline,
        checkpointed_objects: Vec::new(),
        committed_copies: Vec::new(),
    }
}

pub(crate) fn status_from_pool_write_error(err: PoolWriteError) -> Status {
    let message = err.to_string();
    match err {
        PoolWriteError::Select(select) => status_from_select_tape_error(select),
        PoolWriteError::State(state) => status_from_state_error(state),
        PoolWriteError::InvalidInput(_) => Status::invalid_argument(message),
        PoolWriteError::MissingTapeGeometry(_) => Status::failed_precondition(message),
        PoolWriteError::ParityAppendUnsupported { .. } => Status::failed_precondition(message),
        PoolWriteError::SelectedTapeInsufficientCapacity { .. } => {
            Status::failed_precondition(message)
        }
        PoolWriteError::TerminalCloseRequired { .. } => Status::resource_exhausted(message),
        PoolWriteError::ContentHashMismatch { .. } => Status::failed_precondition(message),
        PoolWriteError::CallerObjectIdConflict { .. }
        | PoolWriteError::CallerObjectIdInputKindConflict { .. } => Status::already_exists(message),
        PoolWriteError::ReplayObjectInvalid { .. } => Status::internal(message),
        PoolWriteError::Streaming(streaming) => status_from_streaming_error(&streaming, message),
        PoolWriteError::Parity(parity) => status_from_parity_error(&parity, message),
        PoolWriteError::PhysicalUsedBytesOverflow { .. }
        | PoolWriteError::Io { .. }
        | PoolWriteError::TapeIo(_)
        | PoolWriteError::TransferWithSecondary { .. }
        | PoolWriteError::TimeFormat(_) => Status::internal(message),
    }
}

fn status_from_streaming_error(err: &StreamingError, message: String) -> Status {
    match err {
        StreamingError::InvalidInput(_) | StreamingError::InvalidXattrNamespacePrefix { .. } => {
            Status::invalid_argument(message)
        }
        StreamingError::Format(format) => status_from_format_error(format, message),
        StreamingError::Parity(parity) => status_from_parity_error(parity, message),
        StreamingError::Io { .. } => Status::internal(message),
    }
}

fn status_from_format_error(err: &FormatError, message: String) -> Status {
    match err {
        FormatError::InvalidInput(_) => Status::invalid_argument(message),
        _ => Status::internal(message),
    }
}

fn status_from_parity_error(err: &ParityError, message: String) -> Status {
    match err {
        ParityError::CapacityReserveExceeded { .. }
        | ParityError::ObjectTooLargeForEmptyTape { .. }
        | ParityError::BootstrapPayloadTooLarge { .. } => Status::resource_exhausted(message),
        _ => Status::internal(message),
    }
}

pub(crate) fn status_from_pinned_tape_error(err: crate::pool_write::PinnedTapeError) -> Status {
    use crate::pool_write::PinnedTapeError;
    let message = err.to_string();
    match err {
        PinnedTapeError::UnknownTape { .. } => Status::not_found(message),
        PinnedTapeError::NotADataTape { .. }
        | PinnedTapeError::PoolGuardMismatch { .. }
        | PinnedTapeError::NotWritable { .. }
        | PinnedTapeError::Fenced { .. }
        | PinnedTapeError::NotBatchEligible { .. } => Status::failed_precondition(message),
        PinnedTapeError::Select(err) => status_from_select_tape_error(err),
        PinnedTapeError::State(state) => status_from_state_error(state),
    }
}

pub(crate) fn status_from_select_tape_error(err: SelectTapeError) -> Status {
    let message = err.to_string();
    match err {
        SelectTapeError::UnknownPool { .. } => Status::invalid_argument(message),
        SelectTapeError::EmptyPool { .. }
        | SelectTapeError::NoWritableTapes { .. }
        | SelectTapeError::NoUnreservedWritableTapes { .. }
        | SelectTapeError::AmbiguousNeedsPolicy { .. } => Status::resource_exhausted(message),
        SelectTapeError::NoBatchedEligibleTapes { .. } => Status::failed_precondition(message),
        SelectTapeError::InvalidTapeGeometry { .. } => Status::failed_precondition(message),
        SelectTapeError::InvalidTapeUuid { .. } => Status::internal(message),
        SelectTapeError::State(state) => status_from_state_error(state),
    }
}

fn now_rfc3339() -> Result<String, time::error::Format> {
    OffsetDateTime::now_utc().format(&Rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use remanence_aead::RecipientPrivateKey;
    use remanence_chaos::model::{ModelTransport, Record, VirtualTape, VirtualWorld};
    use remanence_format::{
        read_encrypted_rem_object_file_range_to_vec, write_encrypted_rem_object,
        write_rem_tar_object, RemTarFile, RemTarObjectLayout, RemTarObjectOptions,
    };
    use remanence_library::{
        DriveBay, ElementLayout, FixtureTransport, IdentitySource, InstalledDrive, Library,
        RecordingLog, RecordingTransport, SgTransport, Slot, VecBlockSink, VecBlockSource,
        VecBlockSourceCall, WormMediaState,
    };
    use remanence_parity::bootstrap::{parse_bootstrap_block, write_bootstrap_block};
    use remanence_parity::{
        BootstrapPayload, CommittedBundle, CommittedBundleKind, ParityConfig, TapeFileEntry,
        TapeFileKind,
    };
    use remanence_state::{
        CatalogIndex, DriveObservationInput, NativeObjectCopyProjectionInput,
        NativeObjectFileProjectionInput, NativeObjectProjectionInput, ProvisionTapeInput,
        TapeFileRecord, TapeJournalIndexInput, TapePoolProjectionInput,
        OBJECT_COPY_REPRESENTATION_PLAINTEXT,
    };
    use std::sync::atomic::AtomicU64;
    use tokio_stream::StreamExt;

    const RANGE_OBJECT_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const RANGE_TAPE_UUID: [u8; 16] = [0xAB; 16];

    #[test]
    fn provisional_replay_enforces_digest_identity_and_input_kind_guards() {
        let object_id = [0x41; 16];
        let digest = [0x42; 32];
        assert!(validate_provisional_replay_guards(
            "canonical-pending",
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            object_id,
            digest,
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            Some(object_id),
            Some(digest),
            digest,
        )
        .is_ok());

        let wrong_digest = validate_provisional_replay_guards(
            "canonical-pending",
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            object_id,
            digest,
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            Some(object_id),
            Some([0x43; 32]),
            digest,
        )
        .expect_err("wrong caller digest guard must fail");
        assert_eq!(wrong_digest.code(), tonic::Code::FailedPrecondition);

        let wrong_id = validate_provisional_replay_guards(
            "canonical-pending",
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            object_id,
            digest,
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            Some([0x44; 16]),
            Some(digest),
            digest,
        )
        .expect_err("wrong object identity guard must fail");
        assert_eq!(wrong_id.code(), tonic::Code::InvalidArgument);

        let wrong_kind = validate_provisional_replay_guards(
            "canonical-pending",
            crate::WriteObjectInputKind::CanonicalPlaintextRemObject,
            object_id,
            digest,
            crate::WriteObjectInputKind::LogicalFile,
            None,
            Some(digest),
            digest,
        )
        .expect_err("logical replay must not conflate a canonical pending object");
        assert_eq!(wrong_kind.code(), tonic::Code::AlreadyExists);
    }

    #[test]
    fn concurrent_drive_admission_allows_only_one_matching_identity() {
        let coordinator = WriteAdmissionCoordinator::default();
        let start = Arc::new(std::sync::Barrier::new(2));
        let finish = Arc::new(std::sync::Barrier::new(2));
        let object_id = [0x73; 16];
        let mut workers = Vec::new();
        for caller in ["drive-a", "drive-b"] {
            let coordinator = coordinator.clone();
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            workers.push(std::thread::spawn(move || {
                start.wait();
                let result = coordinator.reserve("pool", caller, Some(object_id));
                let admitted = result.is_ok();
                finish.wait();
                drop(result);
                admitted
            }));
        }
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().expect("drive admission worker"))
            .collect::<Vec<_>>();
        assert_eq!(outcomes.iter().filter(|admitted| **admitted).count(), 1);

        let replay = coordinator
            .reserve("pool", "drive-c", Some(object_id))
            .expect("identity becomes available after the checkpoint-held claim drops");
        let same_key = coordinator
            .reserve("pool", "drive-c", Some([0x74; 16]))
            .expect_err("pool/caller replay key is independently exclusive");
        assert_eq!(same_key.code(), tonic::Code::Aborted);
        drop(replay);
    }

    #[test]
    fn journal_durable_projection_failure_quarantines_identity_until_restart() {
        let coordinator = WriteAdmissionCoordinator::default();
        let object_id = [0x75; 16];
        let admission = coordinator
            .reserve("pool", "journal-owner", Some(object_id))
            .expect("first drive owns identity");
        let mut failed_batch = PendingCheckpointBatch::new(StdDuration::from_secs(60));
        failed_batch._write_admissions.push(admission);
        let failure = CheckpointBarrierFailure::after_journal(Status::internal(
            "injected catalog projection failure after journal fsync",
        ));
        assert!(failure.requires_identity_quarantine());
        failed_batch.quarantine_write_admissions_until_restart();
        drop(failed_batch);

        let second_drive = coordinator
            .reserve("pool", "other-caller", Some(object_id))
            .expect_err("durable unprojected UUID must remain quarantined");
        assert_eq!(second_drive.code(), tonic::Code::Aborted);

        let projected_coordinator = WriteAdmissionCoordinator::default();
        let projected_admission = projected_coordinator
            .reserve("pool", "projected-owner", Some([0x76; 16]))
            .expect("projected identity starts reserved");
        let projected_failure = CheckpointBarrierFailure::after_projection(Status::internal(
            "injected post-projection receipt failure",
        ));
        assert!(!projected_failure.requires_identity_quarantine());
        drop(projected_admission);
        projected_coordinator
            .reserve("pool", "new-caller", Some([0x76; 16]))
            .expect("catalog-projected failures release the transient claim");
    }

    #[test]
    fn malformed_canonical_caller_bytes_map_to_invalid_argument() {
        let error = crate::pool_write::canonical_admission_format_error(FormatError::Parse(
            "hostile truncated pax record".to_string(),
        ));
        let status = status_from_pool_write_error(error);
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status
            .message()
            .contains("canonical plaintext REM object is malformed"));
    }

    #[test]
    fn automatic_terminal_preflight_accepts_empty_fresh_authority() {
        const TAPE_UUID: TapeUuid = [0xD3; 16];
        const BLOCK_SIZE: u32 = 1024;

        let temp = tempfile::Builder::new()
            .prefix("remanence-empty-automatic-preflight")
            .tempdir()
            .expect("create automatic preflight tempdir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite"))
            .expect("open automatic preflight catalog");
        let selected = SelectedTape {
            pool_id: "fresh-open".to_string(),
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::None,
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let checkpoint_dir = temp.path().join("checkpoints");
        let audit_append_lock = Arc::new(std::sync::Mutex::new(()));

        assert!(
            !preflight_automatic_terminal_completion(
                &mut index,
                ManualFinalizePreflightConfig {
                    checkpoint_journal_dir: &checkpoint_dir,
                    audit_dir: temp.path(),
                    audit_fsync: false,
                    audit_append_lock: &audit_append_lock,
                },
                &selected,
                &pool_cfg,
            )
            .expect("an empty fresh checkpoint has no terminal work"),
            "empty fresh authority must continue to ordinary Object admission"
        );
        let checkpoint = remanence_state::FileCheckpointJournal::open(checkpoint_dir, TAPE_UUID)
            .expect("reopen empty fresh checkpoint");
        assert!(
            checkpoint
                .last()
                .expect("read empty fresh checkpoint")
                .is_none(),
            "preflight must not synthesize checkpoint authority"
        );
        assert!(
            checkpoint
                .terminal_finalization_intent()
                .expect("read empty fresh terminal companion")
                .is_none(),
            "preflight must not synthesize a terminal companion"
        );
    }

    #[test]
    fn tape_reservation_holds_session_guard_through_handoff() {
        const TAPE_UUID: TapeUuid = [0xD4; 16];
        let (changer_tx, _changer_rx) = mpsc::channel(1);
        let pool = DrivePool::new(changer_tx, HashMap::new(), Arc::new(HashMap::new()));
        let sessions = Arc::clone(&pool.sessions);
        let reservation = pool
            .reserve_tape_with_after_insert(TAPE_UUID, |reservations| {
                assert!(reservations.contains(&TAPE_UUID));
                assert!(
                    matches!(
                        sessions.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ),
                    "session publication lock must remain held after exact-tape insertion"
                );
            })
            .expect("reserve exact tape through guarded handoff");
        assert!(pool
            .sessions
            .lock()
            .expect("session map after handoff")
            .is_empty());
        drop(reservation);
    }

    #[test]
    fn parity_and_checkpoint_terminal_progress_are_exhaustive_bijections() {
        use remanence_state::TerminalFinalizationProgress as State;

        for progress in [
            State::BeforeReplicaA,
            State::AfterReplicaA,
            State::AfterSeparationAb,
            State::AfterReplicaB,
            State::AfterSeparationBc,
            State::AfterReplicaC,
        ] {
            let parity = parity_progress_from_state(progress);
            assert_eq!(state_progress_from_parity(parity), progress);
            assert_eq!(
                usize::from(completed_terminal_component_count(progress)),
                parity.next_component_index().unwrap_or(5),
            );
        }
    }

    #[test]
    fn terminal_inventory_status_distinguishes_media_conflict_transport_and_geometry() {
        let conflict = status_from_terminal_inventory_read_error(
            remanence_parity::TerminalInventoryReadError::TerminalIndexReplicaConflict { count: 2 },
        );
        assert_eq!(conflict.code(), tonic::Code::DataLoss);
        assert!(conflict.message().contains("conflicting replica editions"));

        let selected_replica = status_from_terminal_inventory_read_error(
            remanence_parity::TerminalInventoryReadError::SelectedReplica {
                ordinal: 3,
                source: remanence_parity::TapeIndexReplicaError::DigestMismatch {
                    field: "payload",
                },
            },
        );
        assert_eq!(selected_replica.code(), tonic::Code::DataLoss);

        let source = status_from_terminal_inventory_read_error(
            remanence_parity::TerminalInventoryReadError::Source {
                operation: "READ",
                message: "transport unavailable".to_string(),
            },
        );
        assert_eq!(source.code(), tonic::Code::Unavailable);

        let geometry = status_from_terminal_inventory_read_error(
            remanence_parity::TerminalInventoryReadError::BlockSize(
                remanence_parity::TerminalTailLayoutError::UnsupportedBlockSize { block_size: 512 },
            ),
        );
        assert_eq!(geometry.code(), tonic::Code::FailedPrecondition);

        let visitor = status_from_terminal_inventory_read_error(
            remanence_parity::TerminalInventoryReadError::StreamVisitor {
                message: "receiver closed".to_string(),
            },
        );
        assert_eq!(visitor.code(), tonic::Code::Cancelled);
    }

    #[test]
    fn terminal_verification_cross_layout_conflict_is_data_loss() {
        let conflict = status_from_terminal_index_verification_error(
            remanence_parity::TerminalIndexVerificationError::ConflictingLayouts { count: 2 },
        );
        assert_eq!(conflict.code(), tonic::Code::DataLoss);

        let editions = status_from_terminal_index_verification_error(
            remanence_parity::TerminalIndexVerificationError::ConflictingReplicaEditions {
                count: 2,
            },
        );
        assert_eq!(editions.code(), tonic::Code::DataLoss);

        let source = status_from_terminal_index_verification_error(
            remanence_parity::TerminalIndexVerificationError::Source {
                operation: "READ",
                message: "transport unavailable".to_string(),
            },
        );
        assert_eq!(source.code(), tonic::Code::Unavailable);
    }

    #[test]
    fn proved_worm_tail_with_surviving_replicas_requires_explicit_degraded_acceptance() {
        use remanence_state::TerminalFinalizationProgress as Progress;

        for progress in [
            Progress::AfterReplicaA,
            Progress::AfterSeparationAb,
            Progress::AfterReplicaB,
            Progress::AfterSeparationBc,
        ] {
            assert_eq!(
                terminal_reconciliation_outcome(
                    progress,
                    TerminalComponentReconcileEvidence::TornWorm,
                ),
                TerminalFinalizationOutcome::RecoveryRequired,
                "{progress:?}",
            );
        }
    }

    #[test]
    fn unknown_or_zero_replica_tail_remains_recovery_required() {
        use remanence_state::TerminalFinalizationProgress as Progress;

        assert_eq!(
            terminal_reconciliation_outcome(
                Progress::BeforeReplicaA,
                TerminalComponentReconcileEvidence::TornWorm,
            ),
            TerminalFinalizationOutcome::RecoveryRequired,
        );
        assert_eq!(
            terminal_reconciliation_outcome(
                Progress::AfterReplicaB,
                TerminalComponentReconcileEvidence::Unproved,
            ),
            TerminalFinalizationOutcome::RecoveryRequired,
        );
        assert_eq!(
            terminal_reconciliation_outcome(
                Progress::AfterReplicaC,
                TerminalComponentReconcileEvidence::TornWorm,
            ),
            TerminalFinalizationOutcome::RecoveryRequired,
        );
    }

    #[test]
    fn one_transition_ahead_sink_journal_reconciles_before_media_is_available() {
        const BLOCK_SIZE: u32 = 256 * 1024;
        const TAPE_UUID: [u8; 16] = [0x8A; 16];

        struct PrefixRows;
        impl remanence_parity::TapeIndexReplicaRecordSource for PrefixRows {
            fn visit_structural_entries(
                &mut self,
                visitor: &mut dyn FnMut(
                    &remanence_parity::TapeIndexReplicaMapEntry,
                ) -> Result<(), ParityError>,
            ) -> Result<(), ParityError> {
                visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                    tape_file_number: 0,
                    kind: remanence_parity::TapeIndexReplicaFileKind::Bootstrap,
                    block_count: 1,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                })?;
                visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                    tape_file_number: 1,
                    kind: remanence_parity::TapeIndexReplicaFileKind::Object,
                    block_count: 1,
                    first_parity_data_ordinal: Some(0),
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                })
            }

            fn visit_object_rows(
                &mut self,
                visitor: &mut dyn FnMut(
                    &remanence_parity::TapeIndexReplicaObjectRow,
                ) -> Result<(), ParityError>,
            ) -> Result<(), ParityError> {
                visitor(&remanence_parity::TapeIndexReplicaObjectRow {
                    tape_file_number: 1,
                    stored_block_count: 1,
                    object_id: b"8e8e8e8e-8e8e-8e8e-8e8e-8e8e8e8e8e8e".to_vec(),
                    representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: 0,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0x8D; 32],
                    },
                })
            }
        }

        let replica_layout = remanence_parity::checked_tape_index_replica_layout(
            BLOCK_SIZE,
            remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: 2,
                object_row_count: 1,
            },
        )
        .expect("replica layout");
        let layout = remanence_parity::TerminalTailLayout::new(
            0,
            BLOCK_SIZE,
            2,
            4,
            replica_layout.replica_record_count,
            remanence_parity::index_separation_records(
                BLOCK_SIZE,
                remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
            )
            .expect("default separation records"),
        )
        .expect("terminal layout");
        let mut rows = PrefixRows;
        let edition = remanence_parity::plan_tape_index_edition(
            remanence_parity::TapeIndexEditionDescriptor {
                tape_uuid: TAPE_UUID,
                edition_id: [0x8B; 16],
                edition_sequence: 2,
                scope: remanence_parity::TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 2,
                    total_data_ordinals: 1,
                    highest_protected_ordinal: 0,
                },
                counts: remanence_parity::TapeIndexReplicaCounts {
                    structural_entry_count: 2,
                    object_row_count: 1,
                },
                block_size: BLOCK_SIZE,
                compression_enabled: false,
                writer_version: "host-authority-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: layout,
            },
            &mut rows,
        )
        .expect("edition plan");
        let plan = TerminalTripleWritePlan::new(edition.clone()).expect("terminal writer plan");
        let intent = remanence_state::TerminalFinalizationIntent {
            tape_uuid: TAPE_UUID,
            trigger: remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark,
            manual: None,
            progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
            recovery_required: false,
            edition_id: edition.descriptor.edition_id,
            edition_sequence: edition.descriptor.edition_sequence,
            edition_digest: edition.edition_digest,
            writer_version: edition.descriptor.writer_version.clone(),
            write_timestamp: edition.descriptor.write_timestamp.clone(),
            terminal_prefix: None,
            layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
                .expect("persist terminal layout"),
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let checkpoint_journal = remanence_state::FileCheckpointJournal::open(
            temp.path().join("checkpoints"),
            TAPE_UUID,
        )
        .expect("checkpoint journal");
        let object_uuid = Uuid::from_bytes([0x8E; 16]);
        checkpoint_journal
            .append(&remanence_state::CheckpointJournalRecord {
                ordinal: 1,
                committed_object_count: 1,
                eod_partition: 0,
                eod_lba: 4,
                tape_uuid: TAPE_UUID,
                batch_id: [0x8C; 16],
                next_tape_file_number: 2,
                block_size: BLOCK_SIZE,
                objects: vec![remanence_state::CheckpointObjectProjection {
                    object: NativeObjectProjectionInput {
                        object_id: object_uuid.to_string(),
                        caller_object_id: Some("host-authority-object".to_string()),
                        body_format: "rem-object-v1".to_string(),
                        logical_size_bytes: Some(1),
                        content_hash: Some(vec![0x8F; 32]),
                        metadata_hash: Some(vec![0x90; 32]),
                        created_at_utc: Some("2026-08-09T00:00:00Z".to_string()),
                    },
                    files: Vec::new(),
                    copy: NativeObjectCopyProjectionInput {
                        object_id: object_uuid.to_string(),
                        tape_uuid: TAPE_UUID,
                        tape_file_number: 1,
                        first_body_lba: 2,
                        first_parity_data_ordinal: None,
                        protected_until_ordinal: None,
                        status: "committed".to_string(),
                        representation: "plaintext".to_string(),
                        recipient_epoch_ids: None,
                        metadata_frame_len: None,
                        plaintext_digest: Some(vec![0x91; 32]),
                        stored_digest: Some(vec![0x91; 32]),
                    },
                    block_size: BLOCK_SIZE,
                    block_count: 1,
                    fresh_tape: true,
                    total_committed_ordinals: 1,
                    object_recovery_row: remanence_state::CheckpointObjectRecoveryRow {
                        tape_file_number: 1,
                        stored_block_count: 1,
                        object_id: b"8e8e8e8e-8e8e-8e8e-8e8e-8e8e8e8e8e8e".to_vec(),
                        representation:
                            remanence_state::CheckpointObjectRecoveryRepresentation::Plaintext {
                                manifest_first_chunk_lba: 0,
                                manifest_size_bytes: 1,
                                manifest_chunk_count: 1,
                                manifest_sha256: [0x8D; 32],
                            },
                    },
                }],
                scheme: None,
                object_tape_file_bundles: Vec::new(),
                barrier_bundle: None,
                terminal_finalization: None,
                sealed_after_write: false,
            })
            .expect("append base checkpoint");
        let mut checkpoint = checkpoint_journal
            .acquire_exclusive()
            .expect("checkpoint lease");
        checkpoint
            .begin_terminal_finalization(&intent)
            .expect("publish terminal intent");

        let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("open catalog");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: TAPE_UUID,
                voltag: "AUTH01L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        project_checkpoint_authority_bounded(&mut index, &checkpoint)
            .expect("project base checkpoint authority");
        index
            .project_terminal_finalization(TerminalFinalizationProjectionInput {
                tape_uuid: TAPE_UUID,
                trigger: intent.trigger,
                operation_id: None,
                progress: intent.progress,
                edition_digest: intent.edition_digest,
                layout_digest: intent.layout.layout_digest,
                outcome: TerminalFinalizationOutcome::InProgress,
                updated_at_utc: None,
            })
            .expect("project initial finalization");

        let scheme = remanence_parity::default_scheme();
        let mut journal = FileTapeFileJournal::open(
            temp.path().join("tape.remjournal"),
            TAPE_UUID,
            BLOCK_SIZE,
            scheme.clone(),
        )
        .expect("open sink journal");
        let bot_bundle = CommittedBundle {
            kind: CommittedBundleKind::BotBootstrap,
            entries: vec![TapeFileEntry {
                tape_file_number: 0,
                kind: TapeFileKind::Bootstrap,
                block_count: 1,
                physical_start_hint: Some(0),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                object_recovery_row: None,
            }],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        };
        let object_bundle = CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![TapeFileEntry {
                tape_file_number: 1,
                kind: TapeFileKind::Object,
                block_count: 1,
                physical_start_hint: Some(2),
                object_id: Some(object_uuid.to_string()),
                first_parity_data_ordinal: Some(0),
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                object_recovery_row: Some(remanence_parity::ObjectRecoveryRow {
                    tape_file_number: 1,
                    stored_block_count: 1,
                    object_id: Some(b"8e8e8e8e-8e8e-8e8e-8e8e-8e8e8e8e8e8e".to_vec()),
                    representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: 0,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0x8D; 32],
                    },
                }),
            }],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 1,
        };
        let object_checkpoint = CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 1,
        };
        let sink_checkpoint = CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 1,
        };
        journal.commit_bundle(&bot_bundle).expect("journal BOT");
        journal
            .commit_bundle(&object_bundle)
            .expect("journal Object");
        journal
            .commit_bundle(&object_checkpoint)
            .expect("journal Object checkpoint");
        journal
            .commit_terminal_prefix_transition(
                &CommittedBundle {
                    kind: CommittedBundleKind::TerminalPrefix,
                    entries: Vec::new(),
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 1,
                },
                &sink_checkpoint,
            )
            .expect("journal terminal prefix");
        let replica_a = remanence_parity::terminal_component_bundle(&plan, layout.components[0])
            .expect("replica A bundle");
        journal
            .commit_bundle(&replica_a)
            .expect("simulate crash after sink component fsync");

        let spec = TerminalFinalizeSpec {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            pool_config: None,
            trigger: intent.trigger,
            operation_id: None,
            manual: None,
        };
        let reconciled = reconcile_terminal_component_host_authority(
            &mut index,
            &mut checkpoint,
            &spec,
            intent,
            &plan,
            &mut journal,
        )
        .expect("host-only one-transition reconciliation");
        assert_eq!(
            reconciled.progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaA
        );
        let transitions = layout
            .components
            .iter()
            .map(|component| {
                let bundle = remanence_parity::terminal_component_bundle(&plan, *component)
                    .expect("component bundle");
                (bundle, sink_checkpoint.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            journal
                .terminal_component_authority_relation(1, &transitions)
                .expect("host authorities aligned after reconciliation"),
            remanence_parity::TerminalComponentAuthorityRelation::Aligned
        );
        assert_eq!(
            checkpoint
                .terminal_finalization_intent()
                .expect("read durable progress")
                .expect("pending intent")
                .progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaA
        );
        assert_eq!(
            index
                .terminal_finalization(&TAPE_UUID)
                .expect("read projection")
                .expect("finalization projection")
                .progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaA
        );

        // Simulate the separate crash window after the next external
        // checkpoint fsync but before its SQLite projection. Both durable
        // journals agree; restart must repair the cache before media motion.
        journal
            .commit_terminal_component_transition(&transitions[1].0, &transitions[1].1)
            .expect("journal separation AB transition");
        let checkpoint_intent = checkpoint
            .advance_terminal_finalization(
                remanence_state::TerminalFinalizationProgress::AfterReplicaA,
                remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
            )
            .expect("advance external checkpoint without SQLite projection");
        assert_eq!(
            index
                .terminal_finalization(&TAPE_UUID)
                .expect("read deliberately stale projection")
                .expect("stale finalization projection")
                .progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaA
        );
        let reconciled = reconcile_terminal_component_host_authority(
            &mut index,
            &mut checkpoint,
            &spec,
            checkpoint_intent,
            &plan,
            &mut journal,
        )
        .expect("aligned journals repair SQLite before media");
        assert_eq!(
            reconciled.progress,
            remanence_state::TerminalFinalizationProgress::AfterSeparationAb
        );
        assert_eq!(
            index
                .terminal_finalization(&TAPE_UUID)
                .expect("read repaired projection")
                .expect("repaired finalization projection")
                .progress,
            remanence_state::TerminalFinalizationProgress::AfterSeparationAb
        );

        let mut current = reconciled;
        for (component_index, expected_progress) in [
            (
                2,
                remanence_state::TerminalFinalizationProgress::AfterReplicaB,
            ),
            (
                3,
                remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
            ),
        ] {
            journal
                .commit_bundle(&transitions[component_index].0)
                .expect("simulate the next sink component one transition ahead");
            current = reconcile_terminal_component_host_authority(
                &mut index,
                &mut checkpoint,
                &spec,
                current,
                &plan,
                &mut journal,
            )
            .expect("promote one exact sink transition before media");
            assert_eq!(current.progress, expected_progress);
            assert_eq!(
                index
                    .terminal_finalization(&TAPE_UUID)
                    .expect("read promoted projection")
                    .expect("promoted finalization projection")
                    .progress,
                expected_progress
            );
        }

        // Replica C may be barrier-proved in the sink journal one host fsync
        // before checkpoint progress. A newly lowered cap would reject the
        // stale pre-C view, so recovery must reconcile first and then observe
        // that no tape capacity remains to authorize.
        journal
            .commit_terminal_component_transition(&transitions[4].0, &transitions[4].1)
            .expect("journal replica C transition");
        let lowered_pool = TapePoolConfig {
            id: "host-authority-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: Some(u64::from(BLOCK_SIZE)),
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let selected = SelectedTape {
            pool_id: lowered_pool.id.clone(),
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::Scheme(scheme),
        };
        let lowered_spec = TerminalFinalizeSpec {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            pool_config: Some(lowered_pool),
            trigger: current.trigger,
            operation_id: None,
            manual: None,
        };
        assert!(
            authorize_terminal_intent_capacity(
                &index,
                &lowered_spec,
                &selected,
                &current,
                plan.edition.descriptor.counts,
            )
            .is_err(),
            "the deliberately stale pre-C checkpoint view must still see the lowered cap"
        );
        assert_eq!(
            index
                .terminal_finalization(&TAPE_UUID)
                .expect("read stale pre-C projection")
                .expect("stale pre-C finalization projection")
                .progress,
            remanence_state::TerminalFinalizationProgress::AfterSeparationBc
        );
        let completed = reconcile_and_authorize_parity_resume(
            &mut index,
            &mut checkpoint,
            &lowered_spec,
            &selected,
            current,
            &plan,
            &mut journal,
        )
        .expect("replica-C sink proof must reconcile before changed-cap authorization");
        assert_eq!(
            completed.progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaC
        );
        let projected = index
            .terminal_finalization(&TAPE_UUID)
            .expect("read repaired replica C projection")
            .expect("replica C finalization projection");
        assert_eq!(
            projected.progress,
            remanence_state::TerminalFinalizationProgress::AfterReplicaC
        );
        assert_eq!(projected.outcome, TerminalFinalizationOutcome::InProgress);

        let completed = checkpoint
            .mark_terminal_recovery_required()
            .expect("persist post-C recovery classification");
        index
            .project_terminal_finalization(TerminalFinalizationProjectionInput {
                tape_uuid: TAPE_UUID,
                trigger: completed.trigger,
                operation_id: None,
                progress: completed.progress,
                edition_digest: completed.edition_digest,
                layout_digest: completed.layout.layout_digest,
                outcome: TerminalFinalizationOutcome::RecoveryRequired,
                updated_at_utc: None,
            })
            .expect("project post-C recovery classification");
        drop(checkpoint);
        let audit_append_lock = Arc::new(std::sync::Mutex::new(()));
        let selected = SelectedTape {
            pool_id: "host-authority-test".to_string(),
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::None,
        };
        let pool_cfg = TapePoolConfig {
            id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        assert!(preflight_automatic_terminal_completion(
            &mut index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &temp.path().join("checkpoints"),
                audit_dir: temp.path(),
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            &selected,
            &pool_cfg,
        )
        .expect("automatic entry completes final checkpoint without media capability"));
        let finalized = index
            .terminal_finalization(&TAPE_UUID)
            .expect("read finalized projection")
            .expect("finalized projection");
        assert_eq!(finalized.outcome, TerminalFinalizationOutcome::Finalized);
        let checkpoint_journal = remanence_state::FileCheckpointJournal::open(
            temp.path().join("checkpoints"),
            TAPE_UUID,
        )
        .expect("reopen finalized checkpoint journal");
        assert!(
            checkpoint_journal
                .last()
                .expect("read final checkpoint")
                .expect("final record")
                .sealed_after_write
        );
    }

    #[test]
    fn all_invalid_terminal_inventory_projects_explicit_bot_recovery() {
        let replicas = std::array::from_fn(|_| {
            remanence_parity::TerminalReplicaEvidence::Invalid(
                remanence_parity::TerminalReplicaFailure {
                    kind: remanence_parity::TerminalReplicaFailureKind::Missing,
                    detail: "test member missing".to_string(),
                },
            )
        });
        let projected = terminal_inventory_to_proto(
            RANGE_TAPE_UUID,
            remanence_parity::TerminalInventoryOutcome::BotStructuralRecoveryRequired(Box::new(
                remanence_parity::BotStructuralRecoveryRequired {
                    reason: remanence_parity::BotStructuralRecoveryReason::AllMembersInvalid,
                    replicas,
                },
            )),
        );
        assert_eq!(
            projected.outcome,
            pb::TapeInventoryOutcome::BotStructuralRecoveryRequired as i32
        );
        assert_eq!(projected.selected_replica_ordinal, 0);
        assert_eq!(projected.structural_entry_count, 0);
        assert_eq!(projected.object_row_count, 0);
        assert_eq!(projected.replica_health.len(), 3);
        assert!(projected.detail.contains("structural recovery from BOT"));
    }

    #[test]
    fn recovery_required_verification_projects_measured_bot_evidence() {
        let replicas = std::array::from_fn(|_| {
            remanence_parity::TerminalReplicaEvidence::Invalid(
                remanence_parity::TerminalReplicaFailure {
                    kind: remanence_parity::TerminalReplicaFailureKind::PayloadInvalid,
                    detail: "test payload invalid".to_string(),
                },
            )
        });
        let projected = terminal_verification_to_proto(
            RANGE_TAPE_UUID,
            remanence_parity::TerminalIndexVerificationOutcome::RecoveryRequired(Box::new(
                remanence_parity::TerminalIndexRecoveryRequired {
                    measured_eod: remanence_parity::PhysicalPositionHint::new(123),
                    bot_recovery: remanence_parity::BotStructuralRecoverySummary {
                        structural_entry_count: 7,
                        complete_object_count: 4,
                        recovered_object_count: 2,
                        unknown_object_count: 2,
                        incomplete_object_count: 1,
                        canonical_map_digest: [0x44; 32],
                        damaged_region_count: 1,
                    },
                    replicas,
                    detail: "no canonical survivor".to_string(),
                },
            )),
        );

        assert_eq!(
            projected.state,
            pb::TapeIndexVerificationState::RecoveryRequired as i32
        );
        assert_eq!(projected.measured_eod_lba, 123);
        assert_eq!(projected.measured_tape_file_count, 7);
        assert_eq!(
            projected.recovery_inventory.as_ref().map(|row| row.outcome),
            Some(pb::TapeInventoryOutcome::BotStructuralRecovered as i32)
        );
    }

    struct ShortFirstModelWriteTransport {
        inner: ModelTransport,
        write_returned_short: bool,
    }

    impl ShortFirstModelWriteTransport {
        fn new(inner: ModelTransport) -> Self {
            Self {
                inner,
                write_returned_short: false,
            }
        }
    }

    impl SgTransport for ShortFirstModelWriteTransport {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            SgTransport::execute_none(&mut self.inner, cdb)
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            let mut outcome = SgTransport::execute_out(&mut self.inner, cdb, buf)?;
            if cdb.first() == Some(&0x0A) && !self.write_returned_short {
                self.write_returned_short = true;
                outcome.bytes_transferred = outcome.bytes_transferred.saturating_sub(1);
            }
            Ok(outcome)
        }

        fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
            self.inner.set_timeout_for(class);
        }

        fn configure_reserved_buffer(
            &mut self,
            requested_bytes: u32,
        ) -> Result<u32, remanence_library::ScsiError> {
            self.inner.configure_reserved_buffer(requested_bytes)
        }
    }

    struct ArmableShortModelWriteTransport {
        inner: ModelTransport,
        short_next_write: Arc<AtomicBool>,
    }

    impl ArmableShortModelWriteTransport {
        fn new(inner: ModelTransport, short_next_write: Arc<AtomicBool>) -> Self {
            Self {
                inner,
                short_next_write,
            }
        }
    }

    impl SgTransport for ArmableShortModelWriteTransport {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            SgTransport::execute_none(&mut self.inner, cdb)
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            let mut outcome = SgTransport::execute_out(&mut self.inner, cdb, buf)?;
            if cdb.first() == Some(&0x0A) && self.short_next_write.swap(false, Ordering::SeqCst) {
                outcome.bytes_transferred = outcome.bytes_transferred.saturating_sub(1);
            }
            Ok(outcome)
        }

        fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
            self.inner.set_timeout_for(class);
        }

        fn configure_reserved_buffer(
            &mut self,
            requested_bytes: u32,
        ) -> Result<u32, remanence_library::ScsiError> {
            self.inner.configure_reserved_buffer(requested_bytes)
        }
    }

    struct FailNthModelWriteTransport {
        inner: ModelTransport,
        target_write: u64,
        write_count: Arc<AtomicU64>,
    }

    impl FailNthModelWriteTransport {
        fn new(inner: ModelTransport, target_write: u64, write_count: Arc<AtomicU64>) -> Self {
            Self {
                inner,
                target_write,
                write_count,
            }
        }
    }

    impl SgTransport for FailNthModelWriteTransport {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            SgTransport::execute_none(&mut self.inner, cdb)
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            let outcome = SgTransport::execute_out(&mut self.inner, cdb, buf)?;
            let write_ordinal = if cdb.first() == Some(&0x0A) {
                self.write_count.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                0
            };
            if write_ordinal == self.target_write {
                return Err(remanence_library::ScsiError::InvalidInput(
                    "injected terminal write completion failure",
                ));
            }
            Ok(outcome)
        }

        fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
            self.inner.set_timeout_for(class);
        }

        fn configure_reserved_buffer(
            &mut self,
            requested_bytes: u32,
        ) -> Result<u32, remanence_library::ScsiError> {
            self.inner.configure_reserved_buffer(requested_bytes)
        }
    }

    #[test]
    fn restore_phase_decomposition_sums_to_wall_including_saturation() {
        let wall = StdDuration::from_millis(100);
        let position = StdDuration::from_millis(20);
        let transfer = StdDuration::from_millis(65);
        let relay = exclusive_restore_relay_phase(wall, position, transfer);
        assert_eq!(relay, StdDuration::from_millis(15));
        assert_eq!(position + transfer + relay, wall);

        let saturated = exclusive_restore_relay_phase(
            StdDuration::from_millis(5),
            StdDuration::from_millis(4),
            StdDuration::from_millis(4),
        );
        assert_eq!(saturated, StdDuration::ZERO);
    }

    #[test]
    fn session_open_media_family_uses_lto9_barcode_suffix() {
        assert!(matches!(
            session_open_media_family(Some("AOX030L9")),
            MediaFamily::Lto9OrLater
        ));
        assert!(matches!(
            session_open_media_family(Some("AOX030LZ")),
            MediaFamily::Lto9OrLater
        ));
        assert!(matches!(
            session_open_media_family(Some("AOX030L8")),
            MediaFamily::Unknown
        ));
        assert!(matches!(
            session_open_media_family(None),
            MediaFamily::Unknown
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn load_wait_absorbs_only_retryable_drive_completions() {
        fn load_check_condition(key: u8, asc: u8, ascq: u8) -> LoadError {
            let mut sense = vec![0_u8; 32];
            sense[0] = 0x70;
            sense[2] = key;
            sense[7] = 24;
            sense[12] = asc;
            sense[13] = ascq;
            LoadError::DriveLoad(DriveOpError::ScsiError(
                remanence_library::ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                },
            ))
        }

        let first_mount_attention = load_check_condition(0x06, 0x28, 0x00);
        assert!(matches!(
            retryable_readiness_from_load_error(&first_mount_attention, MediaFamily::Lto9OrLater),
            Some(MediaReadiness::UnitAttention {
                asc: 0x28,
                ascq: 0x00
            })
        ));

        let medium_error = load_check_condition(0x03, 0x11, 0x00);
        assert!(
            retryable_readiness_from_load_error(&medium_error, MediaFamily::Lto9OrLater).is_none()
        );
    }

    /// The abort reason is the caller's only account of why a session died,
    /// and until now the server read it off the request and dropped it. It now
    /// reaches the session audit record -- and an abort with no reason leaves
    /// the key out rather than writing an empty string, so a later reader can
    /// tell "the caller said nothing" from "the caller said nothing useful".
    #[test]
    fn abort_reason_reaches_the_session_audit_record_only_when_given() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-abort-reason-audit-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-ABORT-REASON",
            0x0100,
            "DRV-ABORT-REASON",
            0x0400,
            1,
        )));
        let library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(
            temp.path().join("rem-state.sqlite"),
            audit_dir.clone(),
            &library,
            snapshot,
        );

        let explained = Uuid::new_v4();
        let silent = Uuid::new_v4();
        for (session_id, abort_reason) in [
            (explained, Some("rem put: append failed".to_string())),
            (silent, None),
        ] {
            record_session_event(
                &mut index,
                &cfg,
                SessionAuditInput {
                    session_id,
                    session_kind: "write",
                    event: AuditEvent::SessionClosed,
                    tape_uuid: None,
                    library_serial: None,
                    drive_bay: None,
                    drive_uuid: None,
                    drive_serial: None,
                    abort_reason,
                },
            )
            .expect("record session event");
        }

        let records = FileAuditLog::replay(audit_dir.as_path()).expect("replay session audit");
        let detail_for = |session_id: Uuid| {
            records
                .iter()
                .find(|record| record.session_id == Some(session_id))
                .unwrap_or_else(|| panic!("no audit record for session {session_id}"))
                .detail
                .clone()
        };

        assert_eq!(
            detail_for(explained).get("abort_reason"),
            Some(&CborValue::Text("rem put: append failed".to_string())),
            "a stated reason must survive to the audit record"
        );
        assert!(
            !detail_for(silent).contains_key("abort_reason"),
            "an abort with no reason must leave the key out, not record an empty one"
        );
    }

    #[test]
    fn session_open_readiness_fence_records_operation_and_guidance() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-readiness-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let ctx = SessionOpenReadinessContext {
            action: "open write session",
            bay: 0x0001,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: Some(0x03eb),
            drive_serial: Some("8031BDC7D1"),
            needs_drive_load: true,
        };

        let status = record_session_open_readiness_fence(
            &mut index,
            &ctx,
            "session_open_short_probe",
            &MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true,
            },
        );

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status
            .message()
            .contains("media_readiness_state=media_initializing"));
        assert!(status
            .message()
            .contains("rem tape wait-ready --library DEC418146K_LL02"));
        let active = index
            .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
            .expect("active fences");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].phase, "session_open_short_probe");
        assert_eq!(active[0].state, "media_initializing");
        assert_eq!(active[0].dirty_scope.as_deref(), Some("drive+tape"));
        assert_eq!(active[0].drive_element, 1);
        assert_eq!(active[0].drive_serial.as_deref(), Some("8031BDC7D1"));
        assert_eq!(active[0].barcode.as_deref(), Some("AOX030L9"));
        assert_eq!(active[0].source_slot, Some(0x03eb));
        assert_eq!(active[0].media_generation, Some(9));
        assert_eq!(active[0].last_cdb_opcode, Some(0));
        assert_eq!(active[0].last_sense_key, Some(0x02));
        assert_eq!(active[0].last_asc, Some(0x04));
        assert_eq!(active[0].last_ascq, Some(0x01));
        assert!(active[0].quarantine_id.is_none());
    }

    #[test]
    fn session_open_refuses_active_tape_io_fence_until_operator_release() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-tape-io-fence-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let tape_uuid = [0x44; 16];
        let fence = index
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: Some("AOX044L9".to_string()),
                reason: "partial_batch".to_string(),
                evidence_json: Some("{\"records_written\":2}".to_string()),
            })
            .expect("record tape-I/O fence");

        let status = session_open_reject_tape_io_fences(
            &index,
            &tape_uuid,
            Some("AOX044L9"),
            "open write session",
        )
        .expect_err("active tape-I/O fence must block session open");

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains(&fence.quarantine_id));
        assert!(status.message().contains("partial_batch"));

        index
            .release_tape_io_fence(&fence.quarantine_id, "operator released")
            .expect("release tape-I/O fence")
            .expect("released fence");
        session_open_reject_tape_io_fences(
            &index,
            &tape_uuid,
            Some("AOX044L9"),
            "open write session",
        )
        .expect("released tape-I/O fence no longer blocks session open");
    }

    #[test]
    fn parity_raw_activity_tracks_write_entry_but_not_position_queries() {
        struct FailingRawSink;

        impl RawTapeSink for FailingRawSink {
            fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
                Err(ParityError::Invariant("injected raw block failure"))
            }

            fn write_filemarks(
                &mut self,
                _count: u32,
                _immed: bool,
            ) -> Result<RawWriteOutcome, ParityError> {
                Err(ParityError::Invariant("injected raw filemark failure"))
            }

            fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
                Ok(PhysicalPositionHint::new(17))
            }
        }

        let mut inner = FailingRawSink;
        let mut write_attempted = false;
        {
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut inner, &mut write_attempted);
            assert_eq!(
                tracked.position().expect("position succeeds"),
                PhysicalPositionHint::new(17)
            );
        }
        assert!(
            !write_attempted,
            "position queries must not raise a write fence"
        );

        {
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut inner, &mut write_attempted);
            tracked
                .write_fixed_block(&[0xAB; 4])
                .expect_err("injected block failure");
        }
        assert!(
            write_attempted,
            "entering the raw block-write boundary must make later failure fenceable"
        );

        struct PositionFailingRawSink;

        impl RawTapeSink for PositionFailingRawSink {
            fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
                panic!("a failed pre-write position check must prevent the block write")
            }

            fn write_filemarks(
                &mut self,
                _count: u32,
                _immed: bool,
            ) -> Result<RawWriteOutcome, ParityError> {
                panic!("a failed pre-write position check must prevent the filemark write")
            }

            fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
                Err(ParityError::Invariant(
                    "injected pre-write position failure",
                ))
            }
        }

        let mut inner = PositionFailingRawSink;
        let mut write_attempted = false;
        {
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut inner, &mut write_attempted);
            tracked
                .write_fixed_block(&[0xCD; 4])
                .expect_err("position failure must stop before raw write activity");
        }
        assert!(
            !write_attempted,
            "pre-write position failure must not raise a physical-write fence"
        );
    }

    #[tokio::test]
    async fn daemon_rejects_corrupt_checkpoint_authority_before_position_or_config_commands() {
        const BLOCK_SIZE: u32 = 1024;
        const BARCODE: &str = "PARAUTH1";

        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-authority-before-motion-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let index = CatalogIndex::open(&index_path).expect("open test index");
        drop(index);

        let tape_uuid = [0x47; 16];
        let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
        let mut world = VirtualWorld::single_drive(
            "LIB-PARITY-AUTHORITY",
            0x0100,
            "DRV-PARITY-AUTHORITY",
            0x0400,
            1,
        );
        world.put_tape_in_drive(
            0x0100,
            BARCODE,
            Some(0x0400),
            VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
        );
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");

        let checkpoint =
            remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
                .expect("open checkpoint handle");
        std::fs::write(checkpoint.path(), b"legacy-or-torn-checkpoint")
            .expect("write corrupt checkpoint authority");

        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let command_start = world.lock().expect("world lock").command_log.len();
        let pool_cfg = TapePoolConfig {
            id: "parity.authority".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    pool_id: "parity.authority".to_string(),
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    parity_config: ParityConfig::Scheme(scheme),
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial.clone(),
                barcode: Some(BARCODE.to_string()),
                source_slot: None,
                drive_uuid: None,
                drive_serial: Some("DRV-PARITY-AUTHORITY".to_string()),
                reply: open_tx,
            })
            .await
            .expect("send parity write open");
        let status = open_rx
            .await
            .expect("parity open reply")
            .expect_err("corrupt checkpoint authority must reject the session");
        assert!(status.message().contains("torn header"), "{status}");

        let opcodes = world.lock().expect("world lock").command_log[command_start..]
            .iter()
            .map(|record| record.opcode)
            .collect::<Vec<_>>();
        for forbidden in [0x01, 0x92, 0x1a, 0x15] {
            assert!(
                !opcodes.contains(&forbidden),
                "authority rejection issued forbidden drive opcode 0x{forbidden:02x}: {opcodes:02x?}"
            );
        }
    }

    #[tokio::test]
    async fn daemon_rejects_missing_checkpoint_authority_for_catalog_written_media_before_motion() {
        const BLOCK_SIZE: u32 = 1024;

        let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
        for (case, barcode, tape_uuid, parity_config) in [
            (
                "parity",
                "MISPAR01",
                [0x4B; 16],
                ParityConfig::Scheme(scheme.clone()),
            ),
            ("no-parity", "MISNOP01", [0x4C; 16], ParityConfig::None),
        ] {
            let temp = tempfile::Builder::new()
                .prefix(&format!("remanence-missing-{case}-authority-"))
                .tempdir()
                .expect("tempdir");
            let index_path = temp.path().join("rem-state.sqlite");
            let mut index = CatalogIndex::open(&index_path).expect("open test index");
            index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid,
                    voltag: barcode.to_string(),
                    block_size: BLOCK_SIZE,
                    parity: parity_config.clone(),
                    force: false,
                })
                .expect("provision tape");
            let projected_scheme = match &parity_config {
                ParityConfig::Scheme(scheme) => Some(scheme.clone()),
                ParityConfig::None => None,
            };
            index
                .project_committed_tape_file_bundle(
                    TapeJournalIndexInput {
                        tape_uuid,
                        block_size: BLOCK_SIZE,
                        scheme: projected_scheme,
                        journal_offset_bytes: 0,
                    },
                    &CommittedBundle {
                        kind: CommittedBundleKind::BotBootstrap,
                        entries: vec![TapeFileEntry {
                            tape_file_number: 0,
                            kind: TapeFileKind::Bootstrap,
                            block_count: 1,
                            physical_start_hint: Some(0),
                            object_id: None,
                            first_parity_data_ordinal: None,
                            epoch_id: None,
                            protected_ordinal_start: None,
                            protected_ordinal_end_exclusive: None,
                            canonical_metadata_hash: None,
                            object_recovery_row: None,
                        }],
                        highest_protected_ordinal: 0,
                        total_committed_ordinals: 0,
                    },
                )
                .expect("project known written BOT prefix");
            drop(index);

            let mut world = VirtualWorld::single_drive(
                format!("LIB-MISSING-{case}-AUTHORITY"),
                0x0100,
                format!("DRV-MISSING-{case}-AUTHORITY"),
                0x0400,
                1,
            );
            world.put_tape_in_drive(
                0x0100,
                barcode,
                Some(0x0400),
                VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
            );
            let world = Arc::new(Mutex::new(world));
            let mut library = open_model_library(Arc::clone(&world));
            let snapshot = library_snapshot_cell(library.library().clone());
            let audit_dir = temp.path().join("audit");
            std::fs::create_dir_all(&audit_dir).expect("create audit dir");
            let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
            cfg.checkpoint_journal_dir = temp.path().join("missing-checkpoints");

            let serial = library.library().serial.clone();
            let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
            let drive = library
                .open_drive(0x0100, &policy)
                .expect("open model drive");
            let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
            let command_start = world.lock().expect("world lock").command_log.len();
            let (open_tx, open_rx) = oneshot::channel();
            drive_tx
                .send(DriveCommand::OpenWrite {
                    pool_cfg: TapePoolConfig {
                        id: format!("missing.{case}.authority"),
                        display_name: None,
                        copy_class: None,
                        content_class: None,
                        selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
                        watermark_low: 0.9,
                        watermark_high: 0.95,
                        capacity_cap_bytes: None,
                        block_size_bytes: u64::from(BLOCK_SIZE),
                        min_object_size_bytes: 0,
                    },
                    selected: SelectedTape {
                        pool_id: format!("missing.{case}.authority"),
                        tape_uuid,
                        block_size: BLOCK_SIZE,
                        parity_config,
                    },
                    target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                    needs_drive_load: false,
                    library_serial: serial,
                    barcode: Some(barcode.to_string()),
                    source_slot: None,
                    drive_uuid: None,
                    drive_serial: None,
                    reply: open_tx,
                })
                .await
                .expect("send write open");
            let status = open_rx
                .await
                .expect("open reply")
                .expect_err("missing authority for catalog-written tape must reject");
            assert!(
                status.message().contains(
                    "checkpoint journal is empty but catalog records a written tape prefix"
                ),
                "{case}: {status}"
            );
            assert!(
                world.lock().expect("world lock").command_log[command_start..].is_empty(),
                "{case}: missing authority must reject before any drive command"
            );
        }
    }

    #[tokio::test]
    async fn daemon_checkpoint_lease_contention_precedes_conditional_load_for_no_parity() {
        const BLOCK_SIZE: u32 = 1024;
        const BARCODE: &str = "NOAUTH01";

        let temp = tempfile::Builder::new()
            .prefix("remanence-checkpoint-lease-before-load-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        drop(CatalogIndex::open(&index_path).expect("open test index"));
        let tape_uuid = [0x48; 16];
        let mut world = VirtualWorld::single_drive(
            "LIB-CHECKPOINT-LEASE",
            0x0100,
            "DRV-CHECKPOINT-LEASE",
            0x0400,
            1,
        );
        world.put_tape_in_drive(
            0x0100,
            BARCODE,
            Some(0x0400),
            VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
        );
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
        let checkpoint =
            remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
                .expect("open checkpoint handle");
        let _held_lease = checkpoint
            .acquire_exclusive()
            .expect("hold competing checkpoint lease");

        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let command_start = world.lock().expect("world lock").command_log.len();
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: TapePoolConfig {
                    id: "checkpoint.lease".to_string(),
                    display_name: None,
                    copy_class: None,
                    content_class: None,
                    selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
                    watermark_low: 0.9,
                    watermark_high: 0.95,
                    capacity_cap_bytes: None,
                    block_size_bytes: u64::from(BLOCK_SIZE),
                    min_object_size_bytes: 0,
                },
                selected: SelectedTape {
                    pool_id: "checkpoint.lease".to_string(),
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    parity_config: ParityConfig::None,
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: true,
                library_serial: serial,
                barcode: Some(BARCODE.to_string()),
                source_slot: Some(0x0400),
                drive_uuid: None,
                drive_serial: Some("DRV-CHECKPOINT-LEASE".to_string()),
                reply: open_tx,
            })
            .await
            .expect("send no-parity write open");
        open_rx
            .await
            .expect("no-parity open reply")
            .expect_err("competing checkpoint lease must reject session open");
        assert!(
            world.lock().expect("world lock").command_log[command_start..].is_empty(),
            "checkpoint contention must reject before conditional LOAD or any drive command"
        );
    }

    #[test]
    fn fresh_parity_bootstrap_short_completion_fences_without_journal_visibility() {
        const BLOCK_SIZE: u32 = 4096;

        let temp = tempfile::Builder::new()
            .prefix("remanence-fresh-parity-bootstrap-fence-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open test index");
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-FRESH-PARITY-FENCE",
            0x0100,
            "DRV-FRESH-PARITY-FENCE",
            0x0400,
            1,
        )));
        world.lock().expect("world lock").put_tape_in_drive(
            0x0100,
            "PARITY001",
            Some(0x0400),
            VirtualTape::empty(1024 * 1024, BLOCK_SIZE),
        );
        let library_model = world.lock().expect("world lock").library_snapshot();
        let policy = remanence_library::StaticAllowlist::new([library_model.serial.as_str()]);
        let factory_world = Arc::clone(&world);
        let mut library = library_model
            .open_with(&policy, move |path| {
                let role = factory_world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                let model = ModelTransport::new(Arc::clone(&factory_world), role);
                let transport: Box<dyn SgTransport> =
                    if path.to_string_lossy().contains("/sg-chaos-drive-") {
                        Box::new(ShortFirstModelWriteTransport::new(model))
                    } else {
                        Box::new(model)
                    };
                Ok::<_, remanence_library::IoErrorKind>(transport)
            })
            .expect("open model library");
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path, audit_dir.clone(), &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("journals/checkpoints");
        std::fs::create_dir_all(cfg.checkpoint_journal_dir.parent().expect("journal parent"))
            .expect("create journal parent");
        let tape_uuid = [0x46; 16];
        let scheme = remanence_parity::default_scheme_for_block_size(BLOCK_SIZE);
        let selected = SelectedTape {
            pool_id: "fresh-parity-fence-test".to_string(),
            tape_uuid,
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::Scheme(scheme.clone()),
        };

        let mut drive = library.open_drive(0x0100, &policy).expect("open drive");
        let status = {
            let checkpoint_journal = remanence_state::FileCheckpointJournal::open(
                &cfg.checkpoint_journal_dir,
                tape_uuid,
            )
            .expect("open fresh checkpoint journal");
            let checkpoint_lease = checkpoint_journal
                .acquire_exclusive()
                .expect("acquire fresh checkpoint lease");
            let authority =
                validate_parity_actor_authority(&cfg, &selected, &checkpoint_lease, &[])
                    .expect("validate fresh parity authority");
            match open_parity_actor_session(&mut index, &mut drive, &cfg, &selected, &[], authority)
            {
                Ok(_) => panic!("short bootstrap completion must fail closed"),
                Err(status) => status,
            }
        };

        assert_eq!(status.code(), tonic::Code::Internal);
        let active = index
            .tape_io_admission_conflicts(&tape_uuid, Some("PARITY001"))
            .expect("active tape-I/O fence");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].reason, "partial_batch");
        assert!(active[0]
            .evidence_json
            .as_deref()
            .expect("fence evidence")
            .contains("\"phase\":\"fresh_bootstrap\""));
        let world_guard = world.lock().expect("world lock");
        let records = &world_guard
            .tapes
            .get("PARITY001")
            .expect("virtual tape")
            .records;
        assert!(
            matches!(records.as_slice(), [Record::Block(block)] if block.len() == BLOCK_SIZE as usize),
            "the modeled drive physically accepted the bootstrap before reporting it short"
        );
        drop(world_guard);

        let journal_path = parity_journal_path(&cfg, tape_uuid).expect("parity journal path");
        let journal = FileTapeFileJournal::open(journal_path, tape_uuid, BLOCK_SIZE, scheme)
            .expect("reopen parity journal");
        assert!(
            journal
                .load_committed()
                .expect("load committed journal prefix")
                .entries
                .is_empty(),
            "a nonexact bootstrap completion must remain invisible to the committed journal"
        );
        let audit = FileAuditLog::replay(audit_dir.as_path()).expect("replay fence audit");
        assert!(audit.iter().any(|record| {
            record.event == AuditEvent::TapeIoFenceRaised
                && record.subject.id.as_deref() == Some(active[0].quarantine_id.as_str())
        }));
    }

    #[test]
    fn first_parity_raw_write_failure_persists_and_audits_partial_fence() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-first-parity-raw-fence-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open test index");
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-PARITY-FENCE",
            0x0100,
            "DRV-PARITY-FENCE",
            0x0400,
            1,
        )));
        let library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path, audit_dir.clone(), &library, snapshot);
        let tape_uuid = [0x45; 16];
        let selected = SelectedTape {
            pool_id: "parity-fence-test".to_string(),
            tape_uuid,
            block_size: 4096,
            parity_config: ParityConfig::None,
        };
        let error = TapeIoError::PartialBatchUncommittable {
            requested_records: 1,
            written_records: 0,
            requested_bytes: 4096,
            written_bytes: 4095,
            end_of_medium: false,
            sense: None,
        }
        .to_string();

        let (status, audited) = fence_failed_parity_raw_write(
            &mut index,
            &cfg,
            &selected,
            "append",
            Some("caller-first-after-checkpoint"),
            None,
            error.as_str(),
            Status::internal(error.clone()),
        );

        assert_eq!(status.code(), tonic::Code::Internal);
        assert!(audited, "the exact persisted fence must be audited");
        let active = index
            .tape_io_admission_conflicts(&tape_uuid, None)
            .expect("active tape-I/O fence");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].reason, "partial_batch");
        let evidence = active[0].evidence_json.as_deref().expect("fence evidence");
        assert!(evidence.contains("\"phase\":\"append\""), "{evidence}");
        assert!(
            evidence.contains("caller-first-after-checkpoint"),
            "{evidence}"
        );
        session_open_reject_tape_io_fences(
            &index,
            &tape_uuid,
            None,
            "open write session after failed parity append",
        )
        .expect_err("the durable partial fence must block the next session");

        let records = FileAuditLog::replay(audit_dir.as_path()).expect("replay fence audit");
        assert!(records.iter().any(|record| {
            record.event == AuditEvent::TapeIoFenceRaised
                && record.subject.id.as_deref() == Some(active[0].quarantine_id.as_str())
        }));
    }

    #[test]
    fn automatic_terminal_transition_transfers_the_session_parity_journal_lock() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-terminal-journal-transfer-")
            .tempdir()
            .expect("tempdir");
        let tape_uuid = [0x48; 16];
        let scheme = remanence_parity::default_scheme_for_block_size(256 * 1024);
        let path = temp.path().join("terminal.remjournal");
        let journal = FileTapeFileJournal::open(&path, tape_uuid, 256 * 1024, scheme.clone())
            .expect("open session journal");
        let mut session = ParityActorSession {
            scheme: scheme.clone(),
            sink_state: None,
            journal: Some(journal),
        };

        let contended = FileTapeFileJournal::open(&path, tape_uuid, 256 * 1024, scheme.clone())
            .expect_err("a second open must contend with the session-owned journal");
        assert!(contended.is_lock_contended(), "{contended}");

        let terminal_journal = session
            .journal
            .take()
            .expect("transfer the session journal to terminal finalization");
        assert!(session.journal.is_none());
        drop(terminal_journal);
        FileTapeFileJournal::open(&path, tape_uuid, 256 * 1024, scheme)
            .expect("the journal lock is released after terminal ownership ends");
    }

    #[tokio::test]
    async fn partial_epoch_checkpoint_projects_replays_and_first_post_checkpoint_short_write_fences(
    ) {
        const BLOCK_SIZE: u32 = 256 * 1024;
        const POOL_ID: &str = "parity-actor-fence";
        const BARCODE: &str = "PAF001L9";

        let temp = tempfile::Builder::new()
            .prefix("remanence-parity-actor-first-write-fence-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = [0x47; 16];
        let scheme = remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("actor-short-write-test"),
            data_blocks_per_stripe: 128,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        };
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: POOL_ID.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: BARCODE.to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision parity tape");
        index
            .project_tape_pool_membership(tape_uuid, POOL_ID)
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-PARITY-ACTOR-FENCE".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-PARITY-ACTOR-FENCE".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-08-08T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: Some(remanence_parity::ParitySchemeRecord {
                id: scheme.id.as_str().to_string(),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(
                remanence_parity::sole_bot_filemark_map_digest().expect("sole BOT digest"),
            ),
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-08-08T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode parity bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK_SIZE);
        let mut world = VirtualWorld::single_drive(
            "LIB-PARITY-ACTOR-FENCE",
            0x0100,
            "DRV-PARITY-ACTOR-FENCE",
            0x0400,
            1,
        );
        world.put_tape_in_drive(0x0100, BARCODE, Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let short_next_write = Arc::new(AtomicBool::new(false));
        let library_model = world.lock().expect("world lock").library_snapshot();
        let policy = remanence_library::StaticAllowlist::new([library_model.serial.as_str()]);
        let factory_world = Arc::clone(&world);
        let factory_short = Arc::clone(&short_next_write);
        let mut library = library_model
            .open_with(&policy, move |path| {
                let role = factory_world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                let model = ModelTransport::new(Arc::clone(&factory_world), role);
                let transport: Box<dyn SgTransport> =
                    if path.to_string_lossy().contains("/sg-chaos-drive-") {
                        Box::new(ArmableShortModelWriteTransport::new(
                            model,
                            Arc::clone(&factory_short),
                        ))
                    } else {
                        Box::new(model)
                    };
                Ok::<_, remanence_library::IoErrorKind>(transport)
            })
            .expect("open model library");
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        let checkpoint_dir = temp.path().join("checkpoints");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg =
            test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
        cfg.checkpoint_journal_dir = checkpoint_dir.clone();
        cfg.checkpoint_max_objects = 2;
        cfg.checkpoint_max_age_seconds = 3600;
        let serial = library.library().serial.clone();
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
            id: POOL_ID.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    pool_id: POOL_ID.to_string(),
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    parity_config: ParityConfig::Scheme(scheme.clone()),
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial.clone(),
                barcode: Some(BARCODE.to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-PARITY-ACTOR-FENCE".to_string()),
                reply: open_tx,
            })
            .await
            .expect("send parity write open");
        let session = open_rx
            .await
            .expect("parity open reply")
            .expect("open parity actor session");
        let session_id = Uuid::from_slice(&session.session_id).expect("session UUID");

        let bootstrap_index = CatalogIndex::open(&index_path).expect("open bootstrap projection");
        let bootstrap_files = bootstrap_index
            .list_tape_files(&tape_uuid)
            .expect("list freshly projected BOT bootstrap");
        assert_eq!(bootstrap_files.len(), 1);
        assert_eq!(bootstrap_files[0].tape_file_number, 0);
        assert_eq!(bootstrap_files[0].kind, "bootstrap");
        drop(bootstrap_index);

        let first = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("parity-first.bin"),
            "parity-first.bin",
            "parity-actor-first",
            b"first parity checkpoint payload",
        )
        .await;
        assert_eq!(
            first
                .record
                .append_commit_info
                .as_ref()
                .expect("first append info")
                .durability,
            pb::AppendDurability::Written as i32
        );
        let (checkpoint_tx, checkpoint_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Explicit,
                expected_batch_id: None,
                reply: Some(checkpoint_tx),
            })
            .await
            .expect("send parity checkpoint");
        let checkpoint = checkpoint_rx
            .await
            .expect("parity checkpoint reply")
            .expect("parity checkpoint succeeds");
        assert_eq!(checkpoint.committed_objects.len(), 1);

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("close first parity session");
        close_rx
            .await
            .expect("first parity close reply")
            .expect("close checkpointed parity session");

        let checkpoint_journal =
            remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
                .expect("open durable parity checkpoint journal");
        let records = checkpoint_journal
            .replay()
            .expect("replay durable partial-epoch checkpoint");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(
            record.object_tape_file_bundles[0].total_committed_ordinals
                < u64::from(scheme.data_blocks_per_stripe),
            "the regression must close a genuinely partial parity epoch"
        );
        let barrier_bundle = record
            .barrier_bundle
            .as_ref()
            .expect("parity checkpoint sidecar bundle");
        assert_eq!(
            barrier_bundle
                .entries
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![TapeFileKind::ParitySidecar],
            "the barrier must journal exactly the short sidecar"
        );
        assert_eq!(
            barrier_bundle
                .entries
                .last()
                .expect("checkpoint sidecar")
                .tape_file_number
                .checked_add(1)
                .expect("next tape-file number"),
            record.next_tape_file_number,
        );
        assert_eq!(
            barrier_bundle.highest_protected_ordinal,
            barrier_bundle.total_committed_ordinals
        );
        let mut replay_index = CatalogIndex::open(&index_path).expect("open replay projection");
        replay_index
            .project_checkpoint_record(record)
            .expect("idempotently replay the durable partial-epoch checkpoint into SQLite");
        drop(replay_index);

        let (resume_tx, resume_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg,
                selected: SelectedTape {
                    pool_id: POOL_ID.to_string(),
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    parity_config: ParityConfig::Scheme(scheme.clone()),
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some(BARCODE.to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid),
                drive_serial: Some("DRV-PARITY-ACTOR-FENCE".to_string()),
                reply: resume_tx,
            })
            .await
            .expect("send parity resume open");
        let resumed = resume_rx
            .await
            .expect("parity resume reply")
            .expect("resume checkpointed parity actor session");
        let resumed_session_id =
            Uuid::from_slice(&resumed.session_id).expect("resumed parity session UUID");

        short_next_write.store(true, Ordering::SeqCst);
        let second = append_actor_test_file_result(
            &drive_tx,
            resumed_session_id,
            temp.path().join("parity-second.bin"),
            "parity-second.bin",
            "parity-actor-second",
            b"first payload after the durable parity checkpoint",
        )
        .await
        .expect_err("first post-checkpoint raw write must report the injected short completion");
        assert!(second
            .message()
            .contains("partial fixed batch uncommittable"));
        assert!(!short_next_write.load(Ordering::SeqCst));

        let third = append_actor_test_file_result(
            &drive_tx,
            resumed_session_id,
            temp.path().join("parity-third.bin"),
            "parity-third.bin",
            "parity-actor-third",
            b"poisoned session must refuse this payload",
        )
        .await
        .expect_err("the failed raw append must poison the actor session");
        assert_eq!(third.code(), tonic::Code::FailedPrecondition);

        let read_only = CatalogIndex::open_read_only(&index_path).expect("open catalog projection");
        assert!(read_only
            .get_native_object_by_pool_and_caller_object_id(POOL_ID, "parity-actor-first")
            .expect("query checkpointed object")
            .is_some());
        assert!(
            read_only
                .get_native_object_by_caller_object_id("parity-actor-second")
                .expect("query failed object")
                .is_none(),
            "the short post-checkpoint append must not reach catalog visibility"
        );
        let active = read_only
            .tape_io_admission_conflicts(&tape_uuid, Some(BARCODE))
            .expect("active tape-I/O fence");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].reason, "partial_batch");
        assert!(active[0]
            .evidence_json
            .as_deref()
            .expect("fence evidence")
            .contains("parity-actor-second"));
        drop(read_only);

        let world_guard = world.lock().expect("world lock");
        assert!(matches!(
            world_guard
                .tapes
                .get(BARCODE)
                .expect("virtual parity tape")
                .records
                .last(),
            Some(Record::Block(_))
        ));
        drop(world_guard);
        let records = FileAuditLog::replay(audit_dir.as_path()).expect("replay fence audit");
        assert_eq!(
            records
                .iter()
                .filter(|record| {
                    record.event == AuditEvent::TapeIoFenceRaised
                        && record.subject.id.as_deref() == Some(active[0].quarantine_id.as_str())
                })
                .count(),
            1,
            "the actor must audit exactly the fence returned by the failed raw append"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_open_refuses_active_media_readiness_fence_before_drive_probe() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-admission-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let active_operation_id = Uuid::from_u128(0x9100);
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: active_operation_id,
                run_id: None,
                library_serial: "DEC418146K_LL02".to_string(),
                changer_sg: Some("/dev/sg8".to_string()),
                drive_element: 0x0100,
                drive_sg: Some("/dev/sg7".to_string()),
                drive_serial: Some("DRV_MOVE_OBS".to_string()),
                barcode: Some("AOX030L9".to_string()),
                source_slot: Some(0x03eb),
                media_generation: Some(9),
                phase: "readiness_poll".to_string(),
                state: "media_initializing".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record active readiness operation");
        let (mut drive, log) = open_test_drive_with_tur_script("DEC418146K_LL02", vec![None]);
        let before = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .count();
        let ctx = SessionOpenReadinessContext {
            action: "open write session",
            bay: 0x0100,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: Some(0x03eb),
            drive_serial: Some("DRV_MOVE_OBS"),
            needs_drive_load: true,
        };

        let status = session_open_short_probe_or_load(&mut index, &mut drive, ctx)
            .expect_err("active readiness fence must block session-open admission");

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("active media-readiness fence"));
        assert!(status.message().contains(&active_operation_id.to_string()));
        let after = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .count();
        assert_eq!(
            after, before,
            "session-open admission must refuse before TUR or LOAD"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_open_loads_immediate_after_unit_attention_then_load_required() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-load-after-ua-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let (mut drive, log) = open_test_drive_with_tur_script(
            "DEC418146K_LL02",
            vec![
                Some(readiness_fixed_sense(0x06, 0x29, 0x00)),
                Some(readiness_fixed_sense(0x02, 0x04, 0x02)),
                None,
            ],
        );
        let ctx = SessionOpenReadinessContext {
            action: "open write session",
            bay: 0x0100,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: Some(0x03eb),
            drive_serial: Some("DRV_MOVE_OBS"),
            needs_drive_load: true,
        };

        session_open_short_probe_or_load(&mut index, &mut drive, ctx)
            .expect("session-open readiness should issue LOAD IMMED then reach ready");

        let control_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .map(|cdb| (cdb[0], cdb[1], cdb[4]))
            .collect::<Vec<_>>();
        assert_eq!(
            control_cdbs,
            vec![
                (0x00, 0x00, 0x00),
                (0x00, 0x00, 0x00),
                (0x1b, 0x01, 0x01),
                (0x00, 0x00, 0x00)
            ]
        );
        assert!(
            index
                .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
                .expect("active fences")
                .is_empty(),
            "ready session-open probe must not leave an active fence"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_open_loads_immediate_for_already_loaded_initialization_required() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-already-loaded-load-required-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let (mut drive, log) = open_test_drive_with_tur_script(
            "DEC418146K_LL02",
            vec![Some(readiness_fixed_sense(0x02, 0x04, 0x02)), None],
        );
        let ctx = SessionOpenReadinessContext {
            action: "open read session",
            bay: 0x0100,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: None,
            drive_serial: Some("DRV_MOVE_OBS"),
            needs_drive_load: false,
        };

        session_open_short_probe_or_load(&mut index, &mut drive, ctx)
            .expect("already-loaded 04/02 should issue LOAD IMMED then reach ready");

        let control_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .map(|cdb| (cdb[0], cdb[1], cdb[4]))
            .collect::<Vec<_>>();
        assert_eq!(
            control_cdbs,
            vec![(0x00, 0x00, 0x00), (0x1b, 0x01, 0x01), (0x00, 0x00, 0x00)]
        );
        assert!(
            index
                .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
                .expect("active fences")
                .is_empty(),
            "ready session-open probe must not leave an active fence"
        );
    }

    fn changer_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
    }

    fn drive_lto9_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
    }

    fn vpd80_response(serial: &str) -> Vec<u8> {
        let bytes = serial.as_bytes();
        let mut response = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
        response.extend_from_slice(bytes);
        response
    }

    fn test_changer_library(serial: &str) -> Library {
        Library {
            serial: serial.to_string(),
            changer_sg: PathBuf::from("/dev/sg-mock"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
            changer_inquiry: remanence_library::scsi::Inquiry::parse(include_bytes!(
                "../../../fixtures/inquiry/changer-msl-g3.bin"
            ))
            .expect("parse changer inquiry fixture"),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x0100,
                drive_count: 1,
                slot_start: 0x0400,
                slot_count: 1,
                ie_start: 0,
                ie_count: 0,
            },
            drive_bays: vec![DriveBay {
                element_address: 0x0100,
                accessible: true,
                exception: None,
                installed: Some(InstalledDrive {
                    serial: "DRV_MOVE_OBS".to_string(),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    vendor: Some("IBM".to_string()),
                    product: Some("ULT3580".to_string()),
                    revision: Some("A1".to_string()),
                    sg_path: Some(PathBuf::from("/dev/sg-drive-mock")),
                    sysfs_path: None,
                }),
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            }],
            slots: vec![Slot {
                element_address: 0x0400,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("TAPE_MOVE".to_string()),
            }],
            ie_ports: Vec::new(),
        }
    }

    fn open_test_changer(library: &Library) -> ChangerHandle {
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        let serial = library.serial.clone();
        let mut responses = Some(vec![changer_inquiry_response(), vpd80_response(&serial)]);
        library
            .open_with(&policy, move |_| {
                let responses = responses
                    .take()
                    .expect("test changer transport opened once");
                Ok::<_, remanence_library::IoErrorKind>(Box::new(
                    FixtureTransport::new().with_responses(responses),
                )
                    as Box<dyn remanence_library::SgTransport>)
            })
            .expect("open test changer")
            .into_changer()
    }

    #[cfg(target_os = "linux")]
    fn open_test_drive_with_tur_script(
        library_serial: &str,
        tur_senses: Vec<Option<Vec<u8>>>,
    ) -> (DriveHandle, RecordingLog) {
        let library = test_changer_library(library_serial);
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        let log = RecordingLog::new();
        let log_for_factory = log.clone();
        let changer_serial = library.serial.clone();
        let mut changer_responses = Some(vec![
            changer_inquiry_response(),
            vpd80_response(&changer_serial),
        ]);
        let mut drive_responses = Some(vec![
            drive_lto9_inquiry_response(),
            vpd80_response("DRV_MOVE_OBS"),
        ]);
        let mut tur_senses = Some(tur_senses);
        let mut handle = library
            .open_with(&policy, move |path| {
                if path == Path::new("/dev/sg-mock") {
                    let responses = changer_responses
                        .take()
                        .expect("changer opened once in test");
                    Ok::<_, remanence_library::IoErrorKind>(Box::new(RecordingTransport::with_log(
                        FixtureTransport::new().with_responses(responses),
                        log_for_factory.clone(),
                    ))
                        as Box<dyn SgTransport>)
                } else if path == Path::new("/dev/sg-drive-mock") {
                    let responses = drive_responses.take().expect("drive opened once in test");
                    let inner = FixtureTransport::new().with_responses(responses);
                    Ok::<_, remanence_library::IoErrorKind>(Box::new(RecordingTransport::with_log(
                        TurScriptTransport::new(
                            inner,
                            tur_senses.take().expect("TUR script consumed once"),
                        ),
                        log_for_factory.clone(),
                    ))
                        as Box<dyn SgTransport>)
                } else {
                    Err(remanence_library::IoErrorKind {
                        kind: "NotFound",
                        message: format!("unknown test path {path:?}"),
                        raw_os_error: None,
                    })
                }
            })
            .expect("library opens");
        (
            handle.open_drive(0x0100, &policy).expect("drive opens"),
            log,
        )
    }

    #[cfg(target_os = "linux")]
    struct TurScriptTransport<T> {
        inner: T,
        tur_senses: std::collections::VecDeque<Option<Vec<u8>>>,
    }

    #[cfg(target_os = "linux")]
    impl<T> TurScriptTransport<T> {
        fn new(inner: T, tur_senses: Vec<Option<Vec<u8>>>) -> Self {
            Self {
                inner,
                tur_senses: tur_senses.into(),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl<T: SgTransport> SgTransport for TurScriptTransport<T> {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            self.inner.execute_none(cdb)?;
            if cdb == [0, 0, 0, 0, 0, 0] {
                if let Some(Some(sense)) = self.tur_senses.pop_front() {
                    return Err(remanence_library::ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: 0,
                    });
                }
            }
            Ok(())
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_out(cdb, buf)
        }

        fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
            self.inner.set_timeout_for(class)
        }
    }

    #[cfg(target_os = "linux")]
    fn readiness_fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut sense = vec![0u8; 32];
        sense[0] = 0x70;
        sense[2] = key & 0x0f;
        sense[7] = 24;
        sense[12] = asc;
        sense[13] = ascq;
        sense
    }

    fn open_model_library(
        world: std::sync::Arc<std::sync::Mutex<VirtualWorld>>,
    ) -> remanence_library::LibraryHandle {
        let library = world.lock().expect("world lock").library_snapshot();
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        library
            .open_with(&policy, move |path| {
                let role = world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                Ok(Box::new(ModelTransport::new(
                    std::sync::Arc::clone(&world),
                    role,
                )))
            })
            .expect("open model library")
    }

    fn test_write_owner_config(
        index_path: PathBuf,
        audit_dir: PathBuf,
        library: &remanence_library::LibraryHandle,
        library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
    ) -> WriteOwnerConfig {
        let serial = library.library().serial.clone();
        WriteOwnerConfig {
            index_path,
            report: DiscoveryReport {
                libraries: vec![library.library().clone()],
                warnings: Vec::new(),
            },
            policy: remanence_library::StaticAllowlist::new([serial.as_str()]),
            audit_dir,
            audit_fsync: false,
            audit_append_lock: Arc::new(std::sync::Mutex::new(())),
            reservations: Arc::new(HashMap::new()),
            default_library_serial: Some(serial.clone()),
            library_snapshot,
            snapshot_miss_alarm: 1,
            managed_library_serials: Arc::new(HashSet::from([serial])),
            cleaning: remanence_state::CleaningConfig::default(),
            tape_io: remanence_state::TapeIoConfig::default(),
            io_memory: crate::io_memory::IoMemoryReservation::new(
                remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
            )
            .expect("test I/O memory manager"),
            write_admissions: WriteAdmissionCoordinator::default(),
            checkpoint_journal_dir: std::env::temp_dir().join("rem-checkpoint-tests"),
            checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
            checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
            checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
            session_idle_seconds: 1800,
            lifecycle: None,
            calibration_store: remanence_state::CalibrationControlStore::open(
                std::env::temp_dir().join(format!("rem-calibration-tests-{}", Uuid::new_v4())),
            )
            .expect("open test calibration store"),
        }
    }

    fn test_io_memory() -> Arc<crate::io_memory::IoMemoryReservation> {
        crate::io_memory::IoMemoryReservation::new(remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES)
            .expect("test I/O memory manager")
    }

    fn library_snapshot_cell(library: Library) -> Arc<RwLock<Arc<crate::LibrarySnapshot>>> {
        Arc::new(RwLock::new(Arc::new(crate::LibrarySnapshot {
            report: DiscoveryReport {
                libraries: vec![library],
                warnings: Vec::new(),
            },
            captured_at: OffsetDateTime::UNIX_EPOCH,
        })))
    }

    async fn append_actor_test_file(
        drive_tx: &mpsc::Sender<DriveCommand>,
        session_id: Uuid,
        source_path: PathBuf,
        archive_path: &str,
        caller_object_id: &str,
        payload: &[u8],
    ) -> AppendFinishOutcome {
        append_actor_test_file_result(
            drive_tx,
            session_id,
            source_path,
            archive_path,
            caller_object_id,
            payload,
        )
        .await
        .expect("actor test append succeeds")
    }

    async fn append_actor_test_file_result(
        drive_tx: &mpsc::Sender<DriveCommand>,
        session_id: Uuid,
        source_path: PathBuf,
        archive_path: &str,
        caller_object_id: &str,
        payload: &[u8],
    ) -> Result<AppendFinishOutcome, Status> {
        std::fs::write(&source_path, payload).expect("write actor test source");
        let (append_tx, append_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::AppendFinish {
                session_id,
                source: crate::WriteObjectSource::Path(source_path),
                archive_path: PathBuf::from(archive_path),
                caller_object_id: caller_object_id.to_string(),
                expected_content_sha256: None,
                expected_object_id: None,
                input_kind: crate::WriteObjectInputKind::LogicalFile,
                live_write_counter: None,
                reply: append_tx,
            })
            .await
            .expect("send actor test append");
        append_rx.await.expect("actor test append reply")
    }

    /// Open one parity-off actor session against an already seated test tape.
    async fn open_actor_test_write_session(
        drive_tx: &mpsc::Sender<DriveCommand>,
        pool_cfg: &TapePoolConfig,
        tape_uuid: TapeUuid,
        library_serial: &str,
        barcode: &str,
        drive_uuid: &[u8],
        drive_serial: &str,
    ) -> Uuid {
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    pool_id: pool_cfg.id.clone(),
                    tape_uuid,
                    block_size: u32::try_from(pool_cfg.block_size_bytes)
                        .expect("actor test pool block size fits u32"),
                    parity_config: ParityConfig::None,
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: library_serial.to_string(),
                barcode: Some(barcode.to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.to_vec()),
                drive_serial: Some(drive_serial.to_string()),
                reply: open_tx,
            })
            .await
            .expect("send actor test write open");
        let session = open_rx
            .await
            .expect("actor test write open reply")
            .expect("open actor test write session");
        Uuid::from_slice(&session.session_id).expect("actor test write session UUID")
    }

    #[test]
    fn checkpoint_timer_request_queues_behind_existing_drive_actor_work() {
        let (tx, mut rx) = mpsc::channel(4);
        let session_id = Uuid::from_bytes([0x71; 16]);
        let batch_id = Uuid::from_bytes([0x72; 16]);
        let (reply, _reply_rx) = oneshot::channel();
        tx.blocking_send(DriveCommand::Get { session_id, reply })
            .expect("queue in-flight actor command");

        arm_checkpoint_timer(tx, session_id, batch_id, StdDuration::from_millis(0))
            .expect("spawn checkpoint timer");

        assert!(matches!(
            rx.blocking_recv().expect("first queued command"),
            DriveCommand::Get { .. }
        ));
        assert!(matches!(
            rx.blocking_recv().expect("timer checkpoint request"),
            DriveCommand::Checkpoint {
                session_id: queued_session,
                trigger: CheckpointTrigger::Timer,
                expected_batch_id: Some(queued_batch),
                reply: None,
            } if queued_session == session_id && queued_batch == batch_id
        ));
    }

    #[test]
    fn canceled_checkpoint_reply_restores_unclaimed_committed_receipts() {
        let mut receipts = vec![pb::ObjectRecord {
            object_id: vec![0x70; 16],
            ..Default::default()
        }];
        let (reply, receiver) = oneshot::channel();
        drop(receiver);

        send_checkpoint_actor_reply(reply, pb::WriteSession::default(), &mut receipts);

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].object_id, vec![0x70; 16]);
    }

    #[test]
    fn timer_close_parks_session_and_releases_drive_bay() {
        let temp = tempfile::tempdir().expect("temp dir");
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-CHECKPOINT-IDLE",
            0x0100,
            "DRV-CHECKPOINT-IDLE",
            0x0400,
            1,
        )));
        let library = open_model_library(world);
        let snapshot = library_snapshot_cell(library.library().clone());
        let (timer_park_tx, mut timer_park_rx) = mpsc::unbounded_channel();
        let lifecycle = DrivePoolLifecycle::with_timer_park_sender(timer_park_tx);
        let reservations = Arc::new(HashMap::from([(0x0100, AtomicBool::new(true))]));
        let session_id = Uuid::from_bytes([0x73; 16]);
        lifecycle
            .sessions
            .lock()
            .expect("session lifecycle")
            .insert(
                session_id,
                MountedSession {
                    bay: 0x0100,
                    library_serial: "LIB-CHECKPOINT-IDLE".to_string(),
                    barcode: Some("CHK001L9".to_string()),
                    home_slot: Some(0x0400),
                    tape_uuid: [0x74; 16],
                    drive_uuid: Some(vec![0x75; 16]),
                },
            );
        let mut cfg = test_write_owner_config(
            temp.path().join("index.sqlite"),
            temp.path().join("audit"),
            &library,
            snapshot,
        );
        cfg.reservations = Arc::clone(&reservations);
        cfg.lifecycle = Some(lifecycle.clone());

        park_timer_closed_session(&cfg, session_id).expect("close and park session");

        assert!(!lifecycle
            .sessions
            .lock()
            .expect("session lifecycle")
            .contains_key(&session_id));
        let parked = lifecycle
            .parked
            .lock()
            .expect("parked lifecycle")
            .by_bay
            .get(&0x0100)
            .cloned()
            .expect("parked cartridge");
        assert_eq!(parked.seated.prior_session_id, Some(session_id));
        assert!(!reservations[&0x0100].load(Ordering::SeqCst));
        assert_eq!(
            timer_park_rx
                .try_recv()
                .expect("timer close arms idle-dismount scheduling"),
            parked
        );
    }

    #[tokio::test]
    async fn checkpoint_actor_deduplicates_in_batch_and_holds_until_checkpoint() {
        const BLOCK_SIZE: u32 = 256 * 1024;
        let temp = tempfile::Builder::new()
            .prefix("remanence-batched-actor")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = [0x76; 16];
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "checkpoint-test".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "CHK002L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, "checkpoint-test")
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-CHECKPOINT".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-CHECKPOINT".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-21T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK_SIZE);
        let mut world =
            VirtualWorld::single_drive("LIB-CHECKPOINT", 0x0100, "DRV-CHECKPOINT", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "CHK002L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
        cfg.checkpoint_max_objects = 2;
        cfg.checkpoint_max_age_seconds = 3600;
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

        let pool_cfg = TapePoolConfig {
            id: "checkpoint-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg,
                selected: SelectedTape {
                    pool_id: "checkpoint-test".to_string(),
                    tape_uuid,
                    block_size: BLOCK_SIZE,
                    parity_config: ParityConfig::None,
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some("CHK002L9".to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid),
                drive_serial: Some("DRV-CHECKPOINT".to_string()),
                reply: open_tx,
            })
            .await
            .expect("send write open");
        let session = open_rx
            .await
            .expect("open reply")
            .expect("open batched session");
        // The session reports the target kind it was opened with.
        assert_eq!(
            session.target_kind,
            pb::write_session::TargetKind::WriteSessionTargetKindPool as i32
        );
        let session_id = Uuid::from_slice(&session.session_id).expect("session UUID");

        let written = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-1.bin"),
            "payload-1.bin",
            "checkpoint-caller-object-1",
            b"checkpoint payload one",
        )
        .await
        .record;
        let written_info = written
            .append_commit_info
            .as_ref()
            .expect("WRITTEN append info");
        assert_eq!(
            written_info.durability,
            pb::AppendDurability::Written as i32
        );
        assert!(written.copies.is_empty());
        assert_eq!(written_info.tape_file_number, None);
        let replay = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-1-replay.bin"),
            "payload-1.bin",
            "checkpoint-caller-object-1",
            b"checkpoint payload one",
        )
        .await;
        assert!(replay.replay, "same in-batch content must be a replay");
        assert_eq!(replay.record.object_id, written.object_id);
        let conflict = append_actor_test_file_result(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-1-conflict.bin"),
            "payload-1.bin",
            "checkpoint-caller-object-1",
            b"different checkpoint payload",
        )
        .await
        .expect_err("different in-batch content under the same caller id must conflict");
        assert_eq!(conflict.code(), tonic::Code::AlreadyExists);
        let object_id = Uuid::from_slice(&written.object_id)
            .expect("object UUID")
            .to_string();
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
        assert!(read_only
            .get_native_object(&object_id)
            .expect("query WRITTEN object")
            .is_none());
        drop(read_only);

        let (get_tx, get_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Get {
                session_id,
                reply: get_tx,
            })
            .await
            .expect("send session get");
        let pending = get_rx
            .await
            .expect("get reply")
            .expect("get batched session");
        assert_eq!(pending.pending_checkpoint_objects, 1);
        assert!(pending.pending_checkpoint_bytes > 0);
        assert!(pending.checkpoint_deadline.is_some());

        let (checkpoint_tx, checkpoint_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Explicit,
                expected_batch_id: None,
                reply: Some(checkpoint_tx),
            })
            .await
            .expect("send explicit checkpoint");
        let checkpoint = checkpoint_rx
            .await
            .expect("checkpoint reply")
            .expect("checkpoint batch");
        assert_eq!(checkpoint.committed_objects.len(), 1);
        let checkpointed_object = &checkpoint.committed_objects[0];
        assert_eq!(
            checkpointed_object
                .append_commit_info
                .as_ref()
                .expect("checkpointed append info")
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(
            checkpointed_object
                .append_commit_info
                .as_ref()
                .expect("checkpointed append info")
                .sealed_after_write,
            Some(false)
        );
        assert_eq!(checkpointed_object.copies.len(), 1);
        assert_eq!(checkpointed_object.copies[0].tape_uuid, tape_uuid);
        assert_eq!(checkpointed_object.copies[0].tape_file_number, 1);
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
        assert!(read_only
            .get_native_object(&object_id)
            .expect("query checkpointed object")
            .is_some());
        drop(read_only);

        let second = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-2.bin"),
            "payload-2.bin",
            "checkpoint-caller-object-2",
            b"checkpoint payload two",
        )
        .await
        .record;
        assert_eq!(
            second
                .append_commit_info
                .as_ref()
                .expect("second WRITTEN info")
                .durability,
            pb::AppendDurability::Written as i32
        );
        let third = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-3.bin"),
            "payload-3.bin",
            "checkpoint-caller-object-3",
            b"checkpoint payload three",
        )
        .await
        .record;
        assert_eq!(
            third
                .append_commit_info
                .as_ref()
                .expect("threshold CHECKPOINTED info")
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(
            third
                .append_commit_info
                .as_ref()
                .expect("threshold CHECKPOINTED info")
                .sealed_after_write,
            Some(false)
        );
        assert_eq!(third.copies.len(), 1);
        assert_eq!(third.copies[0].tape_uuid, tape_uuid);
        assert_eq!(third.copies[0].tape_file_number, 3);

        let (receipt_tx, receipt_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Explicit,
                expected_batch_id: None,
                reply: Some(receipt_tx),
            })
            .await
            .expect("request automatic-checkpoint receipts");
        let receipts = receipt_rx
            .await
            .expect("receipt reply")
            .expect("retrieve automatic checkpoint receipts");
        assert_eq!(
            receipts.committed_objects.len(),
            2,
            "automatic threshold checkpoints retain their full copy set"
        );
        let threshold_copy_placements = receipts
            .committed_objects
            .iter()
            .map(|object| {
                assert_eq!(object.copies.len(), 1);
                (
                    object.copies[0].tape_uuid.clone(),
                    object.copies[0].tape_file_number,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            threshold_copy_placements,
            vec![(tape_uuid.to_vec(), 2), (tape_uuid.to_vec(), 3)]
        );
        let fourth = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-4.bin"),
            "payload-4.bin",
            "checkpoint-caller-object-4",
            b"checkpoint payload four",
        )
        .await
        .record;
        assert_eq!(
            fourth
                .append_commit_info
                .as_ref()
                .expect("close-trigger WRITTEN info")
                .durability,
            pb::AppendDurability::Written as i32
        );
        let fourth_object_id = Uuid::from_slice(&fourth.object_id)
            .expect("fourth object UUID")
            .to_string();

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("send close");
        let closed = close_rx
            .await
            .expect("close reply")
            .expect("close checkpointed session");
        assert_eq!(closed.session.checkpointed_objects.len(), 1);
        assert_eq!(
            closed.session.checkpointed_objects[0].object_id,
            fourth.object_id
        );
        assert_eq!(closed.session.committed_copies.len(), 1);
        assert_eq!(closed.session.committed_copies[0].tape_uuid, tape_uuid);
        assert_eq!(closed.session.committed_copies[0].tape_file_number, 4);
        let journal = remanence_state::FileCheckpointJournal::open(
            temp.path().join("checkpoints"),
            tape_uuid,
        )
        .expect("open checkpoint journal after session lease release");
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
        assert!(read_only
            .get_native_object(&fourth_object_id)
            .expect("query close-checkpointed object")
            .is_some());
        assert_eq!(
            journal
                .last()
                .expect("replay close checkpoint")
                .expect("close checkpoint record")
                .committed_object_count,
            4
        );
        let records = journal.replay().expect("replay all checkpoints");
        assert_eq!(
            records
                .iter()
                .map(|record| record.next_tape_file_number)
                .collect::<Vec<_>>(),
            vec![2, 4, 5],
            "each checkpoint names the first free dense tape file"
        );
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open final projection");
        let tape_files = read_only
            .list_tape_files(&tape_uuid)
            .expect("list tape files");
        assert_eq!(tape_files.len(), 5);
        assert_eq!(tape_files.last().expect("last tape file").kind, "object");
        drop(read_only);

        let world = world.lock().expect("world lock");
        let tape = world.tapes.get("CHK002L9").expect("checkpoint tape");
        let bootstraps = tape
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Block(block) => parse_bootstrap_block(block).ok(),
                Record::ZeroBlock(_) | Record::Filemark => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(bootstraps.len(), 1, "only the BOT Bootstrap is physical");
        assert_eq!(bootstraps[0].sequence, 0);
        for record in &records {
            assert!(record
                .objects
                .iter()
                .all(|object| !object.object_recovery_row.object_id.is_empty()));
        }
        assert_eq!(
            records.last().expect("last record").eod_lba as usize,
            tape.records.len(),
            "journal EOD names the physical Object boundary"
        );
    }

    #[tokio::test]
    async fn daemon_watermark_terminalization_is_atomic_and_terminal_failure_stays_fenced() {
        const BLOCK_SIZE: u32 = 256 * 1024;

        for reject_terminal in [false, true] {
            let case = if reject_terminal {
                "failure"
            } else {
                "success"
            };
            let temp = tempfile::Builder::new()
                .prefix(&format!("remanence-daemon-terminal-{case}-"))
                .tempdir()
                .expect("tempdir");
            let index_path = temp.path().join("rem-state.sqlite");
            let tape_uuid = if reject_terminal {
                [0x7C; 16]
            } else {
                [0x7B; 16]
            };
            let barcode = if reject_terminal {
                "TRMFL1L9"
            } else {
                "TRMOK1L9"
            };
            let pool_id = format!("terminal.{case}");
            let library_serial = format!("LIB-TERMINAL-{case}");
            let drive_serial = format!("DRV-TERMINAL-{case}");
            let mut index = CatalogIndex::open(&index_path).expect("open catalog");
            index
                .upsert_tape_pool_projection(TapePoolProjectionInput {
                    pool_id: pool_id.clone(),
                    display_name: None,
                    copy_class: None,
                    content_class: None,
                    created_at_utc: None,
                })
                .expect("project pool");
            index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid,
                    voltag: barcode.to_string(),
                    block_size: BLOCK_SIZE,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision tape");
            index
                .project_tape_pool_membership(tape_uuid, &pool_id)
                .expect("assign pool");
            let drive_uuid = index
                .observe_drive(DriveObservationInput {
                    serial: drive_serial.clone(),
                    identity_source: "DvcidAndInquiry".to_string(),
                    vendor: Some("IBM".to_string()),
                    product: Some("ULT3580".to_string()),
                    firmware_rev: Some("A1".to_string()),
                    managed: "rem".to_string(),
                    library_serial: Some(library_serial.clone()),
                    element_address: Some(0x0100),
                    observed_at_utc: Some("2026-08-08T00:00:00Z".to_string()),
                })
                .expect("observe drive")
                .drive_uuid;
            drop(index);

            let bootstrap = BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid,
                written_by_version: "test".to_string(),
                written_at: "2026-08-08T00:00:00Z".to_string(),
                sequence: 0,
                block_size_bytes: BLOCK_SIZE,
                drive_compression: false,
            };
            let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
            write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
            let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK_SIZE);
            tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
            tape.written_bytes = u64::from(BLOCK_SIZE);
            let mut world = VirtualWorld::single_drive(
                library_serial.clone(),
                0x0100,
                drive_serial.clone(),
                0x0400,
                1,
            );
            world.put_tape_in_drive(0x0100, barcode, Some(0x0400), tape);
            let world = Arc::new(Mutex::new(world));
            let library_model = world.lock().expect("world lock").library_snapshot();
            let policy = remanence_library::StaticAllowlist::new([library_serial.as_str()]);
            let transport_world = Arc::clone(&world);
            let write_count = Arc::new(AtomicU64::new(0));
            let transport_write_count = Arc::clone(&write_count);
            let mut library = library_model
                .open_with(&policy, move |path| {
                    let role = transport_world
                        .lock()
                        .expect("world lock")
                        .role_for_path(path)
                        .expect("known model path");
                    let model = ModelTransport::new(Arc::clone(&transport_world), role);
                    let transport: Box<dyn SgTransport> = if reject_terminal {
                        Box::new(FailNthModelWriteTransport::new(
                            model,
                            4,
                            Arc::clone(&transport_write_count),
                        ))
                    } else {
                        Box::new(model)
                    };
                    Ok::<_, remanence_library::IoErrorKind>(transport)
                })
                .expect("open model library");
            let snapshot = library_snapshot_cell(library.library().clone());
            let audit_dir = temp.path().join("audit");
            std::fs::create_dir_all(&audit_dir).expect("create audit dir");
            let mut cfg =
                test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
            cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
            cfg.checkpoint_max_objects = 1;
            cfg.checkpoint_max_age_seconds = 3600;
            let drive = library
                .open_drive(0x0100, &policy)
                .expect("open model drive");
            let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
            let pool_cfg = TapePoolConfig {
                id: pool_id,
                display_name: None,
                copy_class: None,
                content_class: None,
                selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
                watermark_low: 0.000_000_000_001,
                watermark_high: 0.95,
                capacity_cap_bytes: None,
                block_size_bytes: u64::from(BLOCK_SIZE),
                min_object_size_bytes: 0,
            };
            let session_id = open_actor_test_write_session(
                &drive_tx,
                &pool_cfg,
                tape_uuid,
                &library_serial,
                barcode,
                &drive_uuid,
                &drive_serial,
            )
            .await;
            let first = append_actor_test_file_result(
                &drive_tx,
                session_id,
                temp.path().join("terminal-source.bin"),
                "terminal.bin",
                "terminal-caller",
                b"terminal watermark payload",
            )
            .await;

            if reject_terminal {
                let status = match first {
                    Err(status) => status,
                    Ok(outcome) => panic!(
                        "short terminal component must fail append; observed {} WRITE CDBs: {outcome:?}",
                        write_count.load(Ordering::SeqCst)
                    ),
                };
                assert!(
                    status.message().contains("terminal tape write failed"),
                    "{status}"
                );
            } else {
                let outcome = first.expect("watermark terminalization succeeds");
                let append_info = outcome
                    .record
                    .append_commit_info
                    .expect("checkpointed append info");
                assert_eq!(
                    append_info.durability,
                    pb::AppendDurability::Checkpointed as i32
                );
                assert_eq!(append_info.sealed_after_write, Some(true));
            }

            let second = append_actor_test_file_result(
                &drive_tx,
                session_id,
                temp.path().join("terminal-second-source.bin"),
                "terminal-second.bin",
                "terminal-second-caller",
                b"must not reach tape",
            )
            .await
            .expect_err("terminal outcome must close the append gate");
            if reject_terminal {
                assert_eq!(second.code(), tonic::Code::FailedPrecondition);
                assert!(second.message().contains("poisoned"), "{second}");
            } else {
                assert_eq!(second.code(), tonic::Code::ResourceExhausted);
                assert!(second.message().contains("sealed"), "{second}");
            }

            let (close_tx, close_rx) = oneshot::channel();
            drive_tx
                .send(DriveCommand::Close {
                    session_id,
                    reply: close_tx,
                })
                .await
                .expect("send close");
            close_rx
                .await
                .expect("close reply")
                .expect("close terminal session");

            let checkpoint = remanence_state::FileCheckpointJournal::open(
                temp.path().join("checkpoints"),
                tape_uuid,
            )
            .expect("open checkpoint journal after lease release");
            let read_only = CatalogIndex::open_read_only(&index_path).expect("open catalog");
            let tape = read_only
                .get_tape(&tape_uuid)
                .expect("query tape")
                .expect("tape exists");
            if reject_terminal {
                let error = checkpoint
                    .replay()
                    .expect_err("terminal failure retains finalization intent");
                assert!(
                    error
                        .to_string()
                        .contains("pending terminal finalization intent"),
                    "{error}"
                );
                assert_eq!(tape.state, "recovery_required");
                assert!(read_only
                    .get_native_object_by_caller_object_id("terminal-caller")
                    .expect("query committed object")
                    .is_some());
                let fences = read_only
                    .tape_io_admission_conflicts(&tape_uuid, Some(barcode))
                    .expect("query terminal failure fence");
                assert_eq!(fences.len(), 1);
                assert_eq!(fences[0].reason, "transfer_error");
            } else {
                let records = checkpoint.replay().expect("replay terminal authority");
                assert_eq!(records.len(), 2);
                assert!(!records[0].sealed_after_write);
                assert!(records[1].sealed_after_write);
                assert!(records[1].objects.is_empty());
                assert_eq!(tape.state, "sealed");
                assert_eq!(tape.written_extent_lba, Some(records[1].eod_lba));
                assert!(read_only
                    .get_native_object_by_caller_object_id("terminal-caller")
                    .expect("query committed object")
                    .is_some());
                assert!(read_only
                    .tape_io_admission_conflicts(&tape_uuid, Some(barcode))
                    .expect("query success fences")
                    .is_empty());
            }
        }
    }

    #[tokio::test]
    async fn manual_finalize_closes_below_low_and_same_key_replay_moves_no_tape() {
        const BLOCK_SIZE: u32 = 256 * 1024;
        let temp = tempfile::Builder::new()
            .prefix("remanence-manual-terminal-below-low-")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = [0x7D; 16];
        let barcode = "TRMOP1L9";
        let pool_id = "terminal.operator";
        let library_serial = "LIB-TERMINAL-OPERATOR";
        let drive_serial = "DRV-TERMINAL-OPERATOR";
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: barcode.to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign pool");
        let assignment_generation = index
            .get_tape_assignment_snapshot(&tape_uuid)
            .expect("assignment query")
            .expect("assignment exists")
            .assignment_generation;
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: drive_serial.to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some(library_serial.to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-08-09T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-08-09T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK_SIZE);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK_SIZE);
        let mut world = VirtualWorld::single_drive(library_serial, 0x0100, drive_serial, 0x0400, 1);
        world.put_tape_in_drive(0x0100, barcode, Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let library_model = world.lock().expect("world lock").library_snapshot();
        let policy = remanence_library::StaticAllowlist::new([library_serial]);
        let transport_world = Arc::clone(&world);
        let mut library = library_model
            .open_with(&policy, move |path| {
                let role = transport_world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                Ok::<_, remanence_library::IoErrorKind>(Box::new(ModelTransport::new(
                    Arc::clone(&transport_world),
                    role,
                )) as Box<dyn SgTransport>)
            })
            .expect("open model library");
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg =
            test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
        let checkpoint_dir = temp.path().join("checkpoints");
        cfg.checkpoint_journal_dir = checkpoint_dir.clone();
        cfg.checkpoint_max_objects = 1;
        cfg.checkpoint_max_age_seconds = 3600;
        let audit_append_lock = Arc::clone(&cfg.audit_append_lock);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.97,
            watermark_high: 0.98,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let session_id = open_actor_test_write_session(
            &drive_tx,
            &pool_cfg,
            tape_uuid,
            library_serial,
            barcode,
            &drive_uuid,
            drive_serial,
        )
        .await;
        let object_payload = vec![0x7d; 64 * BLOCK_SIZE as usize];
        append_actor_test_file_result(
            &drive_tx,
            session_id,
            temp.path().join("operator-terminal-source.bin"),
            "operator-terminal.bin",
            "operator-terminal-caller",
            &object_payload,
        )
        .await
        .expect("checkpoint one Object below low watermark");
        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("send close");
        close_rx
            .await
            .expect("close reply")
            .expect("close below-low write session without finalizing");

        let checkpoint = remanence_state::FileCheckpointJournal::open(
            temp.path().join("checkpoints"),
            tape_uuid,
        )
        .expect("open checkpoint journal");
        let records = checkpoint.replay().expect("replay pre-final checkpoint");
        assert_eq!(records.len(), 1);
        assert!(!records[0].sealed_after_write);
        let capacity_blocks =
            crate::pool_write::raw_capacity_bytes(crate::pool_write::LtoGen::Lto9)
                / u64::from(BLOCK_SIZE);
        let low_watermark_blocks =
            remanence_state::watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)
                .expect("low watermark");
        assert!(records[0].eod_lba < low_watermark_blocks);

        let operation_id = Uuid::from_u128(0x7d01);
        let request = ManualFinalizeTapeActorRequest {
            candidate_operation_id: operation_id,
            actor: AuditActor::User("operator@example.invalid".to_string()),
            actor_fingerprint: "sha256:operator-test".to_string(),
            idempotency_key: Uuid::from_u128(0x7d02),
            request_fingerprint: [0x7D; 32],
            tape_uuid,
            expected_pool_id: Some(pool_id.to_string()),
            assignment_generation,
            reason: "ship partially filled copy offsite".to_string(),
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::None,
            pool_config: Some(pool_cfg.clone()),
        };
        let mut impossible = request.clone();
        impossible.candidate_operation_id = Uuid::from_u128(0x7d03);
        impossible.idempotency_key = Uuid::from_u128(0x7d04);
        impossible.request_fingerprint = [0x7E; 32];
        let impossible_policy = impossible.pool_config.as_mut().expect("pooled request");
        impossible_policy.watermark_low = 0.0001;
        impossible_policy.watermark_high = 0.0002;
        impossible_policy.capacity_cap_bytes = Some(8_241 * u64::from(BLOCK_SIZE));
        let commands_before_rejection = world.lock().expect("world lock").command_log.len();
        let mut rejected_index = CatalogIndex::open(&index_path).expect("open writable catalog");
        let rejected = preflight_manual_finalize_tape(
            &mut rejected_index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: &audit_dir,
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            Some(barcode),
            &mut impossible,
        )
        .expect_err("exact close that cannot fit must fail");
        assert_eq!(
            rejected.code(),
            tonic::Code::ResourceExhausted,
            "{rejected:?}"
        );
        assert_eq!(
            world.lock().expect("world lock").command_log.len(),
            commands_before_rejection,
            "fit rejection must precede every drive command"
        );
        assert!(rejected_index
            .idempotency_scope_record(
                impossible.actor_fingerprint.as_str(),
                FINALIZE_TAPE_OPERATION_KIND,
                impossible.idempotency_key,
            )
            .expect("query rejected idempotency scope")
            .is_none());
        assert!(checkpoint
            .terminal_finalization_intent()
            .expect("query rejected terminal intent")
            .is_none());

        // Failed companion publication must roll back both halves of manual
        // acceptance. No drive command is possible because preflight owns
        // neither a drive nor a changer, and the scoped key remains reusable.
        let mut blocked_intent_path = checkpoint.path().as_os_str().to_os_string();
        blocked_intent_path.push(".finalizing.new");
        let blocked_intent_path = std::path::PathBuf::from(blocked_intent_path);
        std::fs::create_dir(&blocked_intent_path).expect("block intent temporary file creation");
        let commands_before_crash_cut = world.lock().expect("world lock").command_log.len();
        let mut crash_index = CatalogIndex::open(&index_path).expect("open crash-cut catalog");
        let crash_error = preflight_manual_finalize_tape(
            &mut crash_index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: &audit_dir,
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            Some(barcode),
            &mut request.clone(),
        )
        .expect_err("intent publication failure rolls back acceptance");
        assert_eq!(crash_error.code(), tonic::Code::Internal);
        assert_eq!(
            world.lock().expect("world lock").command_log.len(),
            commands_before_crash_cut,
            "crash cut before intent must issue zero drive commands"
        );
        assert!(checkpoint
            .terminal_finalization_intent()
            .expect("read crash-cut intent")
            .is_none());
        assert!(crash_index
            .idempotency_scope_record(
                request.actor_fingerprint.as_str(),
                FINALIZE_TAPE_OPERATION_KIND,
                request.idempotency_key,
            )
            .expect("read crash-cut idempotency binding")
            .is_none());
        assert!(crash_index
            .terminal_finalization(&request.tape_uuid)
            .expect("read crash-cut finalization projection")
            .is_none());
        std::fs::remove_dir(&blocked_intent_path).expect("unblock intent publication");

        let mut recovered_request = request.clone();
        let recovered_operation_id = Uuid::from_u128(0x7d05);
        recovered_request.candidate_operation_id = recovered_operation_id;
        let mut retry_index = CatalogIndex::open(&index_path).expect("open retry catalog");
        assert!(preflight_manual_finalize_tape(
            &mut retry_index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: &audit_dir,
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            Some(barcode),
            &mut recovered_request,
        )
        .expect("identical crash retry rejoins")
        .is_none());
        assert_eq!(
            recovered_request.candidate_operation_id,
            recovered_operation_id
        );
        let durable = checkpoint
            .terminal_finalization_intent()
            .expect("read retry intent")
            .expect("retry published BeforeReplicaA intent");
        assert_eq!(
            durable.progress,
            remanence_state::TerminalFinalizationProgress::BeforeReplicaA
        );
        assert_eq!(
            durable
                .manual
                .as_ref()
                .expect("manual identity")
                .operation_id,
            *recovered_operation_id.as_bytes()
        );
        assert_eq!(
            durable
                .manual
                .as_ref()
                .expect("manual identity")
                .operation_kind,
            FINALIZE_TAPE_OPERATION_KIND
        );

        // Model process death after companion fsync but before the guarded
        // SQLite commit by rolling back both database halves while retaining
        // the exact BeforeReplicaA companion. Retry must retire only that
        // provisional companion, rebuild acceptance atomically, and move no
        // media.
        let raw =
            rusqlite::Connection::open(&index_path).expect("open acceptance rollback fixture");
        let rollback = raw
            .unchecked_transaction()
            .expect("begin acceptance rollback fixture");
        rollback
            .execute(
                "update tapes
                 set finalization_progress = null,
                     finalization_trigger = null,
                     finalization_operation_id = null,
                     finalization_edition_digest = null,
                     finalization_layout_digest = null,
                     completed_replicas = null,
                     finalization_outcome = null,
                     state = 'ready'
                 where tape_uuid = ?1",
                rusqlite::params![tape_uuid.to_vec()],
            )
            .expect("roll back finalization projection fixture");
        rollback
            .execute(
                "delete from idempotency_keys
                 where actor_fingerprint = ?1
                   and operation_kind = ?2
                   and idempotency_key = ?3",
                rusqlite::params![
                    recovered_request.actor_fingerprint.as_str(),
                    FINALIZE_TAPE_OPERATION_KIND,
                    recovered_request.idempotency_key.to_string()
                ],
            )
            .expect("roll back idempotency projection fixture");
        rollback
            .commit()
            .expect("commit acceptance rollback fixture");
        drop(raw);
        assert!(checkpoint
            .terminal_finalization_intent()
            .expect("read provisional companion")
            .is_some());
        let commands_before_provisional_retry = world.lock().expect("world lock").command_log.len();
        let mut provisional_retry = recovered_request.clone();
        assert!(preflight_manual_finalize_tape(
            &mut retry_index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: &audit_dir,
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            Some(barcode),
            &mut provisional_retry,
        )
        .expect("retry repairs provisional companion")
        .is_none());
        assert_eq!(
            world.lock().expect("world lock").command_log.len(),
            commands_before_provisional_retry,
            "provisional acceptance retry must issue zero drive commands"
        );
        assert!(retry_index
            .idempotency_scope_record(
                recovered_request.actor_fingerprint.as_str(),
                FINALIZE_TAPE_OPERATION_KIND,
                recovered_request.idempotency_key,
            )
            .expect("read repaired idempotency binding")
            .is_some());
        assert!(retry_index
            .terminal_finalization(&tape_uuid)
            .expect("read repaired finalization projection")
            .is_some());

        let mut changed_request = recovered_request.clone();
        changed_request.reason = "different exact reason bytes".to_string();
        changed_request.request_fingerprint = [0x7F; 32];
        let changed = preflight_manual_finalize_tape(
            &mut retry_index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: &audit_dir,
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            Some(barcode),
            &mut changed_request,
        )
        .expect_err("changed request conflicts with durable binding");
        assert_eq!(changed.code(), tonic::Code::AlreadyExists);
        assert_eq!(
            world.lock().expect("world lock").command_log.len(),
            commands_before_crash_cut,
            "changed retry conflict must issue zero drive commands"
        );

        let (finalize_tx, finalize_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::FinalizeTape {
                request: recovered_request.clone(),
                needs_drive_load: false,
                library_serial: library_serial.to_string(),
                barcode: Some(barcode.to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some(drive_serial.to_string()),
                reply: finalize_tx,
            })
            .await
            .expect("send manual finalize");
        let first = finalize_rx
            .await
            .expect("manual finalize reply")
            .expect("manual finalization below low succeeds");
        assert_eq!(first.operation_id, recovered_operation_id);
        assert_eq!(
            first.projection.outcome,
            TerminalFinalizationOutcome::Finalized
        );
        assert_eq!(
            first.projection.trigger,
            remanence_state::TerminalFinalizationTrigger::OperatorCloseOut
        );
        let records_after_first = world
            .lock()
            .expect("world lock")
            .tapes
            .get(barcode)
            .expect("virtual tape")
            .records
            .len();

        let (replay_tx, replay_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::FinalizeTape {
                request: recovered_request,
                needs_drive_load: false,
                library_serial: library_serial.to_string(),
                barcode: Some(barcode.to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid),
                drive_serial: Some(drive_serial.to_string()),
                reply: replay_tx,
            })
            .await
            .expect("send idempotent replay");
        let replay = replay_rx
            .await
            .expect("idempotent replay reply")
            .expect("same-key replay returns completed operation");
        assert_eq!(replay, first);
        assert_eq!(
            world
                .lock()
                .expect("world lock")
                .tapes
                .get(barcode)
                .expect("virtual tape")
                .records
                .len(),
            records_after_first,
            "same-key replay must not append another terminal component"
        );

        let final_records = checkpoint.replay().expect("replay sealed checkpoint");
        assert_eq!(final_records.len(), 2);
        assert!(final_records[1].sealed_after_write);
        assert_eq!(
            final_records[1]
                .terminal_finalization
                .as_ref()
                .expect("durable terminal intent")
                .manual
                .as_ref()
                .expect("manual operation identity")
                .reason,
            "ship partially filled copy offsite"
        );

        // Recreate the live post-sealed-checkpoint/pre-SQLite window without
        // restarting the daemon. The durable sealed record must repair the
        // stale projection before any fence or media-capable path is consulted.
        let commands_before_host_repair = world.lock().expect("world lock").command_log.len();
        let mut fenced_index = CatalogIndex::open(&index_path).expect("open fenced catalog");
        fenced_index
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: Some(barcode.to_string()),
                reason: "post_replica_c_host_projection".to_string(),
                evidence_json: None,
            })
            .expect("record post-C fence");
        drop(fenced_index);
        rusqlite::Connection::open(&index_path)
            .expect("open raw projection fixture")
            .execute(
                "update tapes set state = 'finalizing', finalization_outcome = 'in_progress' where tape_uuid = ?1",
                rusqlite::params![tape_uuid.to_vec()],
            )
            .expect("downgrade only the disposable SQLite projection");
        let mut host_index = CatalogIndex::open(&index_path).expect("reopen stale projection");
        let mut host_retry = request.clone();
        let repaired = preflight_manual_finalize_tape(
            &mut host_index,
            ManualFinalizePreflightConfig {
                checkpoint_journal_dir: &checkpoint_dir,
                audit_dir: &audit_dir,
                audit_fsync: false,
                audit_append_lock: &audit_append_lock,
            },
            Some(barcode),
            &mut host_retry,
        )
        .expect("sealed checkpoint repairs stale SQLite despite active fence")
        .expect("sealed retry completes in host preflight");
        assert_eq!(
            repaired.projection.outcome,
            TerminalFinalizationOutcome::Finalized
        );
        assert_eq!(
            world.lock().expect("world lock").command_log.len(),
            commands_before_host_repair,
            "sealed host repair must issue zero drive commands"
        );
        let audit = FileAuditLog::replay(&audit_dir).expect("replay manual finalization audit");
        assert_eq!(
            audit
                .iter()
                .filter(|record| {
                    record.event == AuditEvent::OperationFinished
                        && record.operation_id == Some(recovered_operation_id)
                })
                .count(),
            1,
            "manual finalization and every sealed retry share one completion event"
        );
        assert_eq!(
            audit
                .iter()
                .filter(|record| {
                    record.event == AuditEvent::TapeSealed
                        && record.subject.kind == "tape"
                        && record.subject.id.as_deref()
                            == Some(crate::bytes_to_hex(tape_uuid.as_slice()).as_str())
                })
                .count(),
            1,
            "manual finalization and every sealed retry share one TapeSealed event"
        );
    }

    /// Mount-dispatched explicit checkpoints must return catalog-projected copies after reopening
    /// a session, including a catalog replay whose append acknowledgement is already durable.
    #[tokio::test]
    async fn sequential_sessions_and_replay_return_catalog_copies_through_mount() {
        const BLOCK_SIZE: u32 = 256 * 1024;
        let temp = tempfile::Builder::new()
            .prefix("remanence-sequential-batch-one")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = [0x77; 16];
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "batch-one-test".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "CHK003L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, "batch-one-test")
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-BATCH-ONE".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-BATCH-ONE".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-21T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK_SIZE);
        let mut world =
            VirtualWorld::single_drive("LIB-BATCH-ONE", 0x0100, "DRV-BATCH-ONE", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "CHK003L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
        cfg.checkpoint_max_objects = 2;
        cfg.checkpoint_max_age_seconds = 3600;
        let library_serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([library_serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
            id: "batch-one-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let (changer_tx, _changer_rx) = mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0100, AtomicBool::new(false))]));
        let pool = DrivePool::new(
            changer_tx,
            HashMap::from([(0x0100, drive_tx.clone())]),
            reservations,
        );
        let state_index = CatalogIndex::open(&index_path).expect("open mount test catalog");
        let mut state = crate::ApiState::new_with_pool_configs(state_index, [pool_cfg.clone()]);
        state.drive_pool = Some(pool.clone());

        let mut previous_session_id = None;
        for (session_ordinal, expected_tape_file_number) in [(1u8, 1u64), (2, 2)] {
            let session_id = open_actor_test_write_session(
                &drive_tx,
                &pool_cfg,
                tape_uuid,
                library_serial.as_str(),
                "CHK003L9",
                &drive_uuid,
                "DRV-BATCH-ONE",
            )
            .await;
            assert_ne!(Some(session_id), previous_session_id);
            previous_session_id = Some(session_id);
            pool.record_session(
                session_id,
                MountedSession {
                    bay: 0x0100,
                    library_serial: library_serial.clone(),
                    barcode: Some("CHK003L9".to_string()),
                    home_slot: Some(0x0400),
                    tape_uuid,
                    drive_uuid: Some(drive_uuid.clone()),
                },
            );
            let source_path = temp
                .path()
                .join(format!("batch-one-source-{session_ordinal}.bin"));
            std::fs::write(&source_path, format!("batch-one payload {session_ordinal}"))
                .expect("write mount append source");
            let append = crate::mount::append_finish(
                &state,
                session_id,
                crate::mount::AppendFinishRequest {
                    spool_path: source_path,
                    archive_path: PathBuf::from(format!("payload-{session_ordinal}.bin")),
                    caller_object_id: format!("batch-one-caller-{session_ordinal}"),
                    expected_content_sha256: None,
                    expected_object_id: None,
                    input_kind: crate::WriteObjectInputKind::LogicalFile,
                },
            )
            .await
            .expect("append through mount dispatcher");
            let written_info = append
                .append_commit_info
                .as_ref()
                .expect("batch-of-one WRITTEN append info");
            assert_eq!(
                written_info.durability,
                pb::AppendDurability::Written as i32
            );
            assert!(append.copies.is_empty());

            let checkpoint = crate::mount::checkpoint_write_session(
                &state,
                session_id,
                CheckpointTrigger::Explicit,
            )
            .await
            .expect("explicit checkpoint through mount dispatcher");
            assert_eq!(checkpoint.committed_objects.len(), 1);
            let committed = &checkpoint.committed_objects[0];
            assert_eq!(committed.object_id, append.object_id);
            let committed_info = committed
                .append_commit_info
                .as_ref()
                .expect("batch-of-one CHECKPOINTED append info");
            assert_eq!(
                committed_info.durability,
                pb::AppendDurability::Checkpointed as i32
            );
            assert_eq!(committed.copies.len(), 1);
            assert_eq!(committed.copies[0].tape_uuid, tape_uuid);
            assert_eq!(
                committed.copies[0].tape_file_number,
                expected_tape_file_number
            );

            let (close_tx, close_rx) = oneshot::channel();
            drive_tx
                .send(DriveCommand::Close {
                    session_id,
                    reply: close_tx,
                })
                .await
                .expect("send actor test write close");
            let closed = close_rx
                .await
                .expect("actor test write close reply")
                .expect("close actor test write session");
            assert!(closed.session.checkpointed_objects.is_empty());
            assert!(closed.session.committed_copies.is_empty());
            pool.forget_session(session_id);
        }

        let replay_session_id = open_actor_test_write_session(
            &drive_tx,
            &pool_cfg,
            tape_uuid,
            library_serial.as_str(),
            "CHK003L9",
            &drive_uuid,
            "DRV-BATCH-ONE",
        )
        .await;
        pool.record_session(
            replay_session_id,
            MountedSession {
                bay: 0x0100,
                library_serial,
                barcode: Some("CHK003L9".to_string()),
                home_slot: Some(0x0400),
                tape_uuid,
                drive_uuid: Some(drive_uuid),
            },
        );
        let replay_source = temp.path().join("batch-one-replay-source.bin");
        std::fs::write(&replay_source, "batch-one payload 1").expect("write replay source");
        let replay = crate::mount::append_finish(
            &state,
            replay_session_id,
            crate::mount::AppendFinishRequest {
                spool_path: replay_source,
                archive_path: PathBuf::from("payload-1.bin"),
                caller_object_id: "batch-one-caller-1".to_string(),
                expected_content_sha256: None,
                expected_object_id: None,
                input_kind: crate::WriteObjectInputKind::LogicalFile,
            },
        )
        .await
        .expect("replay append through mount dispatcher");
        assert_eq!(
            replay
                .append_commit_info
                .as_ref()
                .expect("catalog replay append info")
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(replay.copies.len(), 1);
        assert_eq!(replay.copies[0].tape_file_number, 1);

        let replay_checkpoint = crate::mount::checkpoint_write_session(
            &state,
            replay_session_id,
            CheckpointTrigger::Explicit,
        )
        .await
        .expect("explicit replay checkpoint through mount dispatcher");
        assert_eq!(
            replay_checkpoint.committed_objects.len(),
            1,
            "catalog replay must remain claimable by the explicit checkpoint"
        );
        assert_eq!(
            replay_checkpoint.committed_objects[0].object_id,
            replay.object_id
        );
        assert_eq!(replay_checkpoint.committed_objects[0].copies.len(), 1);
        assert_eq!(
            replay_checkpoint.committed_objects[0].copies[0].tape_file_number,
            1
        );

        let claimed_again = crate::mount::checkpoint_write_session(
            &state,
            replay_session_id,
            CheckpointTrigger::Explicit,
        )
        .await
        .expect("repeat explicit replay checkpoint through mount dispatcher");
        assert!(
            claimed_again.committed_objects.is_empty(),
            "a replay receipt must be returned by exactly one explicit checkpoint"
        );

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id: replay_session_id,
                reply: close_tx,
            })
            .await
            .expect("send replay session close");
        close_rx
            .await
            .expect("replay session close reply")
            .expect("close replay session");
        pool.forget_session(replay_session_id);
    }

    struct RangeCatalogFixture {
        index: CatalogIndex,
        _temp: tempfile::TempDir,
        blocks: Vec<Vec<u8>>,
        layout: RemTarObjectLayout,
    }

    fn range_options(block_size: usize) -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            RANGE_OBJECT_ID,
            "caller-range",
            "2026-06-16T12:00:00Z",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        );
        opts.chunk_size = block_size;
        opts
    }

    fn cataloged_payload_fixture(payload: &[u8]) -> RangeCatalogFixture {
        let opts = range_options(512);
        let files = [RemTarFile {
            path: "payload.rem-object",
            file_id: "payload-file",
            data: payload,
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).expect("write wrapped payload");
        let payload_layout = &layout.files[0];
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-range-test-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: RANGE_TAPE_UUID,
                voltag: "RANGE01".to_string(),
                block_size: opts.chunk_size as u32,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_native_object_and_committed_tape_file_bundle(
                NativeObjectProjectionInput {
                    object_id: RANGE_OBJECT_ID.to_string(),
                    caller_object_id: Some("caller-range".to_string()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(payload.len() as u64),
                    content_hash: payload_layout.file_sha256.map(|hash| hash.to_vec()),
                    metadata_hash: None,
                    created_at_utc: Some("2026-06-16T12:00:00Z".to_string()),
                },
                &[NativeObjectFileProjectionInput {
                    object_id: RANGE_OBJECT_ID.to_string(),
                    file_id: "payload-file".to_string(),
                    path: "payload.rem-object".to_string(),
                    size_bytes: payload.len() as u64,
                    file_sha256: payload_layout
                        .file_sha256
                        .expect("regular payload hash")
                        .to_vec(),
                    first_chunk_lba: payload_layout.first_chunk_lba.map(|lba| lba.0),
                    chunk_count: payload_layout.chunk_count,
                    mtime: Some("0".to_string()),
                    executable: Some(false),
                }],
                &[NativeObjectCopyProjectionInput {
                    object_id: RANGE_OBJECT_ID.to_string(),
                    tape_uuid: RANGE_TAPE_UUID,
                    tape_file_number: 0,
                    first_body_lba: 0,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: None,
                    stored_digest: None,
                }],
                TapeJournalIndexInput {
                    tape_uuid: RANGE_TAPE_UUID,
                    block_size: opts.chunk_size as u32,
                    scheme: None,
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 0,
                        kind: TapeFileKind::Object,
                        block_count: layout.projected_size_blocks,
                        physical_start_hint: Some(0),
                        object_id: Some(RANGE_OBJECT_ID.to_string()),
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        object_recovery_row: None,
                    }],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                },
            )
            .expect("project range fixture");
        RangeCatalogFixture {
            index,
            _temp: temp,
            blocks: sink.blocks,
            layout,
        }
    }

    #[test]
    fn ranged_absolute_lba_derives_from_dense_filemark_prefix() {
        let tape_uuid = RANGE_TAPE_UUID.to_vec();
        let files = [
            TapeFileRecord {
                tape_uuid: tape_uuid.clone(),
                tape_file_number: 0,
                kind: "bootstrap".to_string(),
                block_count: 1,
                object_id: None,
                canonical_metadata_hash: None,
                canonical_metadata_hash_algorithm: None,
            },
            TapeFileRecord {
                tape_uuid: tape_uuid.clone(),
                tape_file_number: 1,
                kind: "object".to_string(),
                block_count: 10,
                object_id: Some("first".to_string()),
                canonical_metadata_hash: None,
                canonical_metadata_hash_algorithm: None,
            },
            TapeFileRecord {
                tape_uuid,
                tape_file_number: 2,
                kind: "object".to_string(),
                block_count: 3,
                object_id: Some("target".to_string()),
                canonical_metadata_hash: None,
                canonical_metadata_hash_algorithm: None,
            },
        ];
        assert_eq!(derive_physical_file_start_lba(&files, 2), Some(13));

        let mut incomplete = files.to_vec();
        incomplete.remove(1);
        assert_eq!(
            derive_physical_file_start_lba(&incomplete, 2),
            None,
            "a non-dense prefix must use the logical fallback"
        );
    }

    async fn collect_stream_chunks(
        mut rx: crate::read_core::ReadStreamReceiver,
    ) -> Result<Vec<u8>, Status> {
        let mut bytes = Vec::new();
        let mut saw_last = false;
        while let Some(item) = rx.next().await {
            let chunk = item?;
            bytes.extend_from_slice(&chunk.data);
            saw_last |= chunk.is_last;
            if chunk.is_last {
                break;
            }
        }
        assert!(saw_last, "range stream must send terminal frame");
        Ok(bytes)
    }

    #[tokio::test]
    async fn l3_read_actor_batches_are_consumed_by_staged_sender() {
        let (tx, rx) = crate::read_core::read_stream_channel(4);

        let diagnostics = stream_with_staged_read_sender_diagnostics(tx, 4, |writer, _| {
            std::io::Write::write_all(writer, b"abcdef")
                .map_err(|err| Status::internal(format!("write staged bytes: {err}")))?;
            std::io::Write::write_all(writer, b"gh")
                .map_err(|err| Status::internal(format!("write staged bytes: {err}")))?;
            Ok(())
        })
        .expect("staged read sender succeeds");
        assert_eq!(diagnostics.bytes, 8);

        let bytes = collect_stream_chunks(rx)
            .await
            .expect("collect staged read stream");
        assert_eq!(bytes, b"abcdefgh");
    }

    #[tokio::test]
    async fn staged_sender_surfaces_full_channel_stall_time() {
        let requested_chunk =
            u32::try_from(crate::read_core::READ_STREAM_CHANNEL_BYTE_BUDGET + 1).unwrap();
        let (tx, rx) = crate::read_core::read_stream_channel(requested_chunk as usize);
        let sender = tokio::task::spawn_blocking(move || {
            stream_with_staged_read_sender_diagnostics(tx, requested_chunk, |writer, _| {
                std::io::Write::write_all(writer, b"a")
                    .map_err(|err| Status::internal(format!("write first byte: {err}")))?;
                std::io::Write::write_all(writer, b"b")
                    .map_err(|err| Status::internal(format!("write second byte: {err}")))?;
                Ok(())
            })
        });
        tokio::time::sleep(StdDuration::from_millis(10)).await;
        let bytes = collect_stream_chunks(rx)
            .await
            .expect("drain staged stream");
        let diagnostics = sender
            .await
            .expect("sender task joins")
            .expect("staged sender succeeds");

        assert_eq!(bytes, b"ab");
        assert!(
            diagnostics.sender_stall >= StdDuration::from_millis(5),
            "full-channel wait must surface in restore diagnostics: {:?}",
            diagnostics.sender_stall
        );
    }

    #[tokio::test]
    async fn l3_read_sink_error_drains_without_hanging_actor_writer() {
        let (tx, rx) = crate::read_core::read_stream_channel(1);
        drop(rx);

        let err = stream_with_staged_read_sender_diagnostics(tx, 1, |writer, _| {
            for _ in 0..8 {
                std::io::Write::write_all(writer, b"x").map_err(|err| {
                    Status::internal(format!("actor observed staged sender failure: {err}"))
                })?;
            }
            Ok(())
        })
        .expect_err("closed gRPC receiver must fail staged sender");

        assert!(
            err.message().contains("read stream closed")
                || err.message().contains("staged read sender failed"),
            "sink error should be surfaced, got {err}"
        );
    }

    async fn stream_fixture_range(
        fixture: &RangeCatalogFixture,
        file_id: &str,
        start_byte: u64,
        end_byte: u64,
    ) -> Result<Vec<u8>, Status> {
        let request = file_range_read_request(
            &fixture.index,
            &RANGE_TAPE_UUID,
            RANGE_OBJECT_ID,
            file_id,
            start_byte,
            end_byte,
        )?;
        let mut source = VecBlockSource::new(fixture.blocks.clone());
        let (tx, rx) = crate::read_core::read_stream_channel(256);
        stream_file_range_from_source(
            &mut source,
            request,
            0,
            tx,
            &TapeIoConfig::default(),
            test_io_memory(),
        )?;
        collect_stream_chunks(rx).await
    }

    #[test]
    fn append_gate_poisons_session_after_failed_append() {
        let mut gate = SessionAppendGate::default();
        assert!(gate.check().is_ok(), "fresh session must accept appends");

        gate.record_failure();

        let status = gate.check().expect_err("poisoned gate must refuse");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("poisoned"));
        // Poisoning is permanent for the session's lifetime.
        assert!(gate.check().is_err());
    }

    #[test]
    fn channel_and_command_bounds_hold() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<mpsc::Sender<ChangerCommand>>();
        assert_send::<ChangerCommand>();
        assert_send_sync::<mpsc::Sender<DriveCommand>>();
        assert_send::<DriveCommand>();
        assert_send_sync::<mpsc::Sender<Result<pb::BytesChunk, Status>>>();
    }

    #[tokio::test]
    async fn changer_move_succeeds_and_publishes_snapshot_when_catalog_observation_fails() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-move-observe-failure")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        CatalogIndex::open(&index_path).expect("create catalog");
        let sqlite = rusqlite::Connection::open(&index_path).expect("open raw sqlite");
        sqlite
            .execute_batch(
                "create trigger fail_drive_observation
                 before insert on drives
                 begin
                   select raise(fail, 'injected drive catalog observation failure');
                 end;",
            )
            .expect("install observation failure trigger");
        drop(sqlite);

        let library = test_changer_library("LIB_MOVE_OBS_FAIL");
        let snapshot_cell = library_snapshot_cell(library.clone());
        let changer = open_test_changer(&library);
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        let cfg = WriteOwnerConfig {
            index_path: index_path.clone(),
            report: DiscoveryReport {
                libraries: vec![library.clone()],
                warnings: Vec::new(),
            },
            policy,
            audit_dir: temp.path().join("audit"),
            audit_fsync: false,
            audit_append_lock: Arc::new(std::sync::Mutex::new(())),
            reservations: Arc::new(HashMap::new()),
            default_library_serial: Some(library.serial.clone()),
            library_snapshot: Arc::clone(&snapshot_cell),
            snapshot_miss_alarm: 1,
            managed_library_serials: Arc::new(HashSet::from([library.serial.clone()])),
            cleaning: remanence_state::CleaningConfig::default(),
            tape_io: remanence_state::TapeIoConfig::default(),
            io_memory: test_io_memory(),
            write_admissions: WriteAdmissionCoordinator::default(),
            checkpoint_journal_dir: temp.path().join("checkpoints"),
            checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
            checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
            checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
            session_idle_seconds: 1800,
            lifecycle: None,
            calibration_store: remanence_state::CalibrationControlStore::open(
                temp.path().join("calibration"),
            )
            .expect("open test calibration store"),
        };
        let actor = spawn_changer_actor(changer, cfg);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        actor
            .send(ChangerCommand::Move {
                src: 0x0400,
                dst: 0x0100,
                reply: reply_tx,
            })
            .await
            .expect("send move command");
        let result = reply_rx.await.expect("move reply");

        assert!(
            result.is_ok(),
            "physical move success must not be converted to failure by catalog observation: {result:?}"
        );
        let published = snapshot_cell
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let published_library = published
            .report
            .libraries
            .iter()
            .find(|candidate| candidate.serial == library.serial)
            .expect("published library");
        let bay = &published_library.drive_bays[0];
        assert!(bay.loaded, "published snapshot must include the moved tape");
        assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_MOVE"));
        assert_eq!(bay.source_slot, Some(0x0400));
        assert!(!published_library.slots[0].full);

        let alarm_key = library_snapshot_persist_alarm_key(library.serial.as_str());
        let alarm = CatalogIndex::open(&index_path)
            .expect("reopen catalog")
            .get_alarm(alarm_key.as_str())
            .expect("lookup alarm")
            .expect("observation failure alarm");
        assert_eq!(alarm.kind, "snapshot-persist-failing");
        assert_eq!(alarm.state, "open");
        assert!(
            alarm
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("injected drive catalog observation failure")),
            "alarm detail must surface the observation failure: {alarm:?}"
        );
    }

    #[test]
    fn spool_enforces_size_cap() {
        let dir = std::env::temp_dir().join(format!("remanence-spool-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create spool test dir");
        let mut spool = Spool::create(&dir, 4).expect("create spool");
        assert!(spool.path().exists());
        assert!(spool.write_chunk(b"ab").is_ok());
        assert!(spool.write_chunk(b"cde").is_err());
        let path = spool.path().to_path_buf();
        drop(spool);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn spool_removes_unfinished_file_on_drop() {
        let dir =
            std::env::temp_dir().join(format!("remanence-spool-drop-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create spool test dir");
        let path = {
            let mut spool = Spool::create(&dir, 4).expect("create spool");
            spool.write_chunk(b"ab").expect("write chunk");
            spool.path().to_path_buf()
        };
        assert!(!path.exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn process_loss_without_drop_leaves_owned_spool_for_startup_reconciliation() {
        let dir = std::env::temp_dir().join(format!(
            "remanence-spool-process-loss-test-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create spool test dir");
        let mut spool = Spool::create(&dir, 16).expect("create spool");
        spool.write_chunk(b"orphan").expect("write orphan bytes");
        let path = spool.path().to_path_buf();

        std::mem::forget(spool);

        assert!(path.exists(), "process loss bypasses Spool::drop");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 spool name");
        assert!(name.starts_with("spool-") && name.ends_with(".bin"));
        std::fs::remove_file(path).expect("remove simulated orphan");
        std::fs::remove_dir(dir).expect("remove spool test dir");
    }

    #[test]
    fn session_protos_include_drive_element_address() {
        let session_id = Uuid::from_u128(0x5E5510);
        let tape_uuid = [0xAB; 16];
        let opened_at = "2026-06-10T12:00:00Z";

        let write = session_proto(WriteSessionProtoInput {
            session_id,
            tape_uuid: &tape_uuid,
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            state: pb::write_session::State::WriteSessionStateOpen,
            objects_committed: 0,
            bytes_committed: 0,
            opened_at_utc: opened_at,
            last_checkpoint_at_utc: None,
            drive_element_address: 0x0100,
            pending_batch: None,
        });
        let read = read_session_proto(
            session_id,
            &tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            opened_at,
            0x0101,
            None,
            7,
        );

        assert_eq!(write.drive_element_address, Some(0x0100));
        assert_eq!(read.drive_element_address, Some(0x0101));
        assert_eq!(read.daemon_epoch, 7);
    }

    fn resume_target_for_fixture(fixture: &RangeCatalogFixture) -> ReadResumeTarget {
        let first_chunk_lba = fixture.layout.files[0]
            .first_chunk_lba
            .expect("fixture file has a body-chunk boundary")
            .0;
        ReadResumeTarget {
            tape_uuid: RANGE_TAPE_UUID,
            object_id: RANGE_OBJECT_ID.to_string(),
            file_id: "payload-file".to_string(),
            file_boundary_byte_offset: first_chunk_lba * 512,
            expected_position_lba: Some(first_chunk_lba),
            prior_daemon_epoch: Some(11),
        }
    }

    #[test]
    fn cold_resume_relocates_returns_proof_and_mints_fresh_session() {
        let fixture = cataloged_payload_fixture(b"cold resume payload");
        let target = resume_target_for_fixture(&fixture);
        let request = file_range_read_request(
            &fixture.index,
            &target.tape_uuid,
            target.object_id.as_str(),
            target.file_id.as_str(),
            0,
            0,
        )
        .expect("resolve durable resume coordinates");

        let first_session_id = Uuid::new_v4();
        let first = read_session_proto(
            first_session_id,
            &target.tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            "2026-07-12T00:00:00Z",
            0x0101,
            None,
            target.prior_daemon_epoch.expect("prior epoch"),
        );
        drop(first);

        let mut cold_source = VecBlockSource::new(fixture.blocks.clone());
        let proof_lba = position_read_resume_from_source(&mut cold_source, request, &target)
            .expect("cold resume position proof");
        let resumed_session_id = Uuid::new_v4();
        let resumed = read_session_proto(
            resumed_session_id,
            &target.tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            "2026-07-12T00:01:00Z",
            0x0101,
            Some(proof_lba),
            12,
        );

        assert_ne!(resumed_session_id, first_session_id);
        assert_eq!(resumed.session_id, resumed_session_id.as_bytes());
        assert_eq!(resumed.daemon_epoch, 12);
        assert_eq!(
            resumed
                .position_proof
                .expect("resume open returns proof")
                .position_after_lba,
            target.expected_position_lba.expect("expected LBA")
        );
        assert!(cold_source.calls.iter().any(|call| matches!(
            call,
            VecBlockSourceCall::Space {
                kind: SpaceKind::Blocks,
                ..
            }
        )));
    }

    #[test]
    fn wrong_tape_is_rejected_before_position_even_at_matching_lba() {
        let fixture = cataloged_payload_fixture(b"wrong tape position collision");
        let actual_tape_uuid = RANGE_TAPE_UUID;
        let requested_tape_uuid = [0xCD; 16];
        let payload = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid: actual_tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-12T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
        };
        let mut block = vec![0u8; 4096];
        write_bootstrap_block(&payload, &mut block).expect("write wrong-tape bootstrap");
        let mut target = resume_target_for_fixture(&fixture);
        target.tape_uuid = requested_tape_uuid;
        let request = file_range_read_request(
            &fixture.index,
            &actual_tape_uuid,
            target.object_id.as_str(),
            target.file_id.as_str(),
            0,
            0,
        )
        .expect("resolve colliding physical position");
        let expected_lba = target.expected_position_lba.expect("expected LBA");
        let mut blocks = vec![block];
        blocks.resize_with(expected_lba as usize + 1, || vec![0u8; 512]);
        let mut source = VecBlockSource::new(blocks);

        let error = verify_and_position_read_resume_from_source(&mut source, request, &target)
            .expect_err("wrong tape must fail before trusting its matching LBA");

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("tape identity mismatch"));
        assert_eq!(
            source.cursor(),
            1,
            "identity read stops at an LBA from which the expected proof was reachable"
        );
        assert!(source.calls.iter().all(|call| !matches!(
            call,
            VecBlockSourceCall::Space { .. } | VecBlockSourceCall::Position
        )));
    }

    #[test]
    fn resume_rejects_mid_file_offset_without_positioning() {
        let fixture = cataloged_payload_fixture(b"file-boundary payload");
        let mut target = resume_target_for_fixture(&fixture);
        target.file_boundary_byte_offset += 1;
        let request = file_range_read_request(
            &fixture.index,
            &target.tape_uuid,
            target.object_id.as_str(),
            target.file_id.as_str(),
            0,
            0,
        )
        .expect("resolve durable resume coordinates");
        let mut source = VecBlockSource::new(fixture.blocks.clone());

        let error = position_read_resume_from_source(&mut source, request, &target)
            .expect_err("mid-file resume must fail");

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().contains("file boundary"));
        assert!(
            source.calls.is_empty(),
            "invalid offset must not move the tape"
        );
    }

    #[test]
    fn serialized_resume_token_contains_no_session_id() {
        let persisted_session_id = [0xEE; 16];
        let token = pb::ReadResumeTarget {
            tape_uuid: RANGE_TAPE_UUID.to_vec(),
            object_id: Uuid::parse_str(RANGE_OBJECT_ID)
                .expect("object UUID")
                .as_bytes()
                .to_vec(),
            file_id: b"payload-file".to_vec(),
            file_boundary_byte_offset: 1024,
            expected_position_lba: Some(17),
            daemon_epoch: Some(41),
        };

        let encoded = token.encode_to_vec();

        assert!(
            !encoded
                .windows(persisted_session_id.len())
                .any(|window| window == persisted_session_id),
            "the durable resume token must not serialize a session id"
        );
    }

    #[test]
    fn pool_write_status_maps_nested_input_and_capacity_errors() {
        let invalid = status_from_pool_write_error(PoolWriteError::Streaming(
            StreamingError::InvalidInput("bad archive path".to_string()),
        ));
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        let invalid_prefix = status_from_pool_write_error(PoolWriteError::Streaming(
            StreamingError::InvalidXattrNamespacePrefix {
                prefix: "s".to_string(),
            },
        ));
        assert_eq!(invalid_prefix.code(), tonic::Code::InvalidArgument);

        let exhausted = status_from_pool_write_error(PoolWriteError::Parity(
            ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks: 10,
                empty_tape_usable_blocks: 9,
                required_reserve_blocks: 1,
            },
        ));
        assert_eq!(exhausted.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn session_close_snapshot_success_clears_snapshot_persist_alarm() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-snapshot-alarm")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let drive_uuid = Uuid::new_v4().as_bytes().to_vec();
        let condition_key = snapshot_persist_alarm_key(&drive_uuid);
        index
            .raise_alarm(
                condition_key.as_str(),
                "snapshot-persist-failing",
                "warning",
                Some("{\"misses\":3}"),
            )
            .expect("raise snapshot alarm");

        index
            .clear_alarm(condition_key.as_str())
            .expect("clear snapshot alarm");

        assert_eq!(
            index
                .get_alarm(condition_key.as_str())
                .expect("alarm lookup")
                .expect("alarm row")
                .state,
            "cleared"
        );
    }

    #[test]
    fn failure_snapshots_are_keyed_by_failing_session() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-failure-snapshots")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-FAIL-SNAPSHOT".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-FAIL-SNAPSHOT".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-18T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let mut world =
            VirtualWorld::single_drive("LIB-FAIL-SNAPSHOT", 0x0100, "DRV-FAIL-SNAPSHOT", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "FAIL001L9", Some(0x0400), VirtualTape::default());
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let mut drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let append_session = Uuid::new_v4();
        let read_session = Uuid::new_v4();
        let tape_uuid = [0x77; 16];
        let mut misses = 0;

        record_session_snapshot(
            &mut index,
            &cfg,
            &mut drive,
            Some(drive_uuid.clone()),
            append_session,
            tape_uuid,
            "append-failure",
            &mut misses,
        );
        record_session_snapshot(
            &mut index,
            &cfg,
            &mut drive,
            Some(drive_uuid.clone()),
            read_session,
            tape_uuid,
            "read-failure",
            &mut misses,
        );

        let rows = index
            .list_drive_health_snapshots(&drive_uuid)
            .expect("list failure snapshots");
        let append_session = append_session.to_string();
        let read_session = read_session.to_string();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].trigger, "append-failure");
        assert_eq!(rows[0].session_id.as_deref(), Some(append_session.as_str()));
        assert_eq!(rows[1].trigger, "read-failure");
        assert_eq!(rows[1].session_id.as_deref(), Some(read_session.as_str()));
    }

    /// Build a virtual world with one drive holding a seated no-parity tape
    /// whose BOT bootstrap matches `tape_uuid`, provisioned in the catalog
    /// under `voltag`. Returns everything a read-open harvest test needs.
    #[allow(clippy::type_complexity)]
    fn read_harvest_world(
        temp: &tempfile::TempDir,
        tape_uuid: [u8; 16],
        voltag: &str,
    ) -> (
        std::path::PathBuf,
        remanence_state::CalibrationControlStore,
        mpsc::Sender<DriveCommand>,
        String,
        String,
    ) {
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: voltag.to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-08-04T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; 4096];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
        tape.records = vec![
            Record::Block(bootstrap_block),
            Record::Filemark,
            Record::Filemark,
        ];
        tape.written_bytes = 4096;
        let mut world =
            VirtualWorld::single_drive("LIB-READ-HARVEST", 0x0100, "DRV-READ-HARVEST", 0x0400, 1);
        world.put_tape_in_drive(0x0100, voltag, Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        let calibration_store = cfg.calibration_store.clone();
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        (
            index_path,
            calibration_store,
            drive_tx,
            serial,
            voltag.to_string(),
        )
    }

    async fn open_read_on_fresh_mount(
        drive_tx: &mpsc::Sender<DriveCommand>,
        tape_uuid: [u8; 16],
        serial: &str,
        barcode: &str,
    ) -> pb::ReadSession {
        let (open_read_tx, open_read_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load: true,
                library_serial: serial.to_string(),
                barcode: Some(barcode.to_string()),
                source_slot: None,
                drive_uuid: None,
                drive_serial: Some("DRV-READ-HARVEST".to_string()),
                resume_target: None,
                daemon_epoch: 1,
                reply: open_read_tx,
            })
            .await
            .expect("send read open");
        open_read_rx
            .await
            .expect("read open reply")
            .expect("open read session")
    }

    /// The prompt's read-mount harvest fixture: a tape mounted purely to
    /// read — no write session anywhere — calibrates the volume, asserted
    /// through `servable_wrap_map`. Without the read-side harvest a
    /// restore-only workload would leave every volume permanently
    /// uncalibrated.
    #[tokio::test]
    async fn read_only_mount_harvest_calibrates_the_volume() {
        use crate::calibration::{servable_wrap_map, WrapMapServeOutcome};

        let temp = tempfile::Builder::new()
            .prefix("remanence-read-mount-harvest")
            .tempdir()
            .expect("tempdir");
        let tape_uuid = [0x79; 16];
        let (index_path, store, drive_tx, serial, voltag) =
            read_harvest_world(&temp, tape_uuid, "RDH001L9");
        assert_eq!(
            store.row(tape_uuid).state,
            remanence_state::VolumeCalibrationState::Uncalibrated,
            "no history before the read mount"
        );
        assert_eq!(store.row(tape_uuid).calibration_generation, 0);

        let session = open_read_on_fresh_mount(&drive_tx, tape_uuid, &serial, &voltag).await;
        let session_id = Uuid::from_slice(&session.session_id).expect("read session UUID");

        // The read-only mount harvested: the volume is durably calibrated
        // and its map is servable for planning.
        let row = store.row(tape_uuid);
        assert_eq!(
            row.state,
            remanence_state::VolumeCalibrationState::Calibrated,
            "the read mount's load harvest calibrated the volume"
        );
        assert!(row.calibration_generation > 0);
        let index = CatalogIndex::open(&index_path).expect("reopen catalog");
        match servable_wrap_map(&index, &store, tape_uuid).expect("serve") {
            WrapMapServeOutcome::Servable { map, .. } => {
                assert_eq!(map.wrap_count(), 1);
                assert!(map.mapped_extent_lba() > 0);
            }
            other => panic!("read-only mount must leave a servable map, got {other:?}"),
        }
        drop(index);

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::CloseRead {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("send read close");
        close_rx
            .await
            .expect("read close reply")
            .expect("close read session");
    }

    /// The failure half of the same placement rule: when the harvest cannot
    /// calibrate (a recognised-but-unsupported format — REOWP is never even
    /// issued), no harvest outcome fails the open. The session opens, reads
    /// can proceed, and the volume is honestly not servable.
    #[tokio::test]
    async fn read_open_succeeds_when_the_harvest_cannot_calibrate() {
        use crate::calibration::{servable_wrap_map, WrapMapServeOutcome, WrapMapServeRefusal};

        let temp = tempfile::Builder::new()
            .prefix("remanence-read-mount-unsupported")
            .tempdir()
            .expect("tempdir");
        let tape_uuid = [0x7A; 16];
        // M8 is recognised but unsupported (conflicting published geometry).
        let (index_path, store, drive_tx, serial, voltag) =
            read_harvest_world(&temp, tape_uuid, "RDH002M8");

        let session = open_read_on_fresh_mount(&drive_tx, tape_uuid, &serial, &voltag).await;
        assert!(
            !session.session_id.is_empty(),
            "no harvest outcome fails the open"
        );

        let row = store.row(tape_uuid);
        assert_eq!(
            row.state,
            remanence_state::VolumeCalibrationState::UnsupportedFormat,
            "the refusal is recorded durably, not silently dropped"
        );
        let index = CatalogIndex::open(&index_path).expect("reopen catalog");
        match servable_wrap_map(&index, &store, tape_uuid).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::UnsupportedFormat);
            }
            other => panic!("unsupported format must not serve a map, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn induced_append_and_read_failures_persist_session_snapshots() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-induced-failure-snapshots")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        let tape_uuid = [0x78; 16];
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "failure-test".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "FAIL002L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, "failure-test")
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-INDUCED-FAIL".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-INDUCED-FAIL".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-18T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-18T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; 4096];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
        tape.records = vec![
            Record::Block(bootstrap_block),
            Record::Filemark,
            Record::Filemark,
        ];
        tape.written_bytes = 4096;
        let mut world =
            VirtualWorld::single_drive("LIB-INDUCED-FAIL", 0x0100, "DRV-INDUCED-FAIL", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "FAIL002L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

        let (open_read_tx, open_read_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load: false,
                library_serial: serial.clone(),
                barcode: Some("FAIL002L9".to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-INDUCED-FAIL".to_string()),
                resume_target: None,
                daemon_epoch: 1,
                reply: open_read_tx,
            })
            .await
            .expect("send read open");
        let read_session = open_read_rx
            .await
            .expect("read open reply")
            .expect("open read session");
        let read_session_id =
            Uuid::from_slice(&read_session.session_id).expect("read session UUID");
        let (chunk_tx, mut chunk_rx) = crate::read_core::read_stream_channel(4096);
        drive_tx
            .send(DriveCommand::ReadFile {
                session_id: read_session_id,
                object_id: Uuid::new_v4().to_string(),
                file_id: Vec::new(),
                stream_chunk_bytes: 4096,
                chunk_tx,
            })
            .await
            .expect("send failing read");
        chunk_rx
            .next()
            .await
            .expect("read failure item")
            .expect_err("missing object induces read failure");
        let (close_read_tx, close_read_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::CloseRead {
                session_id: read_session_id,
                reply: close_read_tx,
            })
            .await
            .expect("send read close");
        close_read_rx
            .await
            .expect("read close reply")
            .expect("close read session");

        let pool_cfg = TapePoolConfig {
            id: "failure-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let (open_write_tx, open_write_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    pool_id: "failure-test".to_string(),
                    tape_uuid,
                    block_size: 4096,
                    parity_config: ParityConfig::None,
                },
                target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some("FAIL002L9".to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-INDUCED-FAIL".to_string()),
                reply: open_write_tx,
            })
            .await
            .expect("send write open");
        let write_session = open_write_rx
            .await
            .expect("write open reply")
            .expect("open write session");
        let write_session_id =
            Uuid::from_slice(&write_session.session_id).expect("write session UUID");
        let spool = temp.path().join("invalid-archive-path.spool");
        std::fs::write(&spool, b"induced append failure").expect("write spool");
        let (append_tx, append_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::AppendFinish {
                session_id: write_session_id,
                source: crate::WriteObjectSource::Path(spool),
                archive_path: PathBuf::from("../invalid"),
                caller_object_id: "failure-test-object".to_string(),
                expected_content_sha256: None,
                expected_object_id: None,
                input_kind: crate::WriteObjectInputKind::LogicalFile,
                live_write_counter: None,
                reply: append_tx,
            })
            .await
            .expect("send failing append");
        append_rx
            .await
            .expect("append reply")
            .expect_err("invalid archive path induces append failure");

        let close_command_start = world.lock().expect("world lock").command_log.len();
        let (close_write_tx, close_write_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id: write_session_id,
                reply: close_write_tx,
            })
            .await
            .expect("send write close");
        let close_reply = close_write_rx
            .await
            .expect("write close reply")
            .expect("close write session");
        assert_eq!(
            close_reply.session.state,
            pb::write_session::State::WriteSessionStateClosed as i32
        );
        assert_eq!(
            close_reply.diagnostics.filemark_write_drain,
            StdDuration::ZERO
        );
        assert_eq!(
            close_reply.diagnostics.catalog_journal_fsync,
            StdDuration::ZERO
        );
        assert_eq!(close_reply.diagnostics.rewind, StdDuration::ZERO);
        assert_eq!(close_reply.diagnostics.ssc_unload, StdDuration::ZERO);
        let close_opcodes = world
            .lock()
            .expect("world lock")
            .command_log
            .iter()
            .skip(close_command_start)
            .map(|command| command.opcode)
            .collect::<Vec<_>>();
        assert!(
            !close_opcodes.contains(&0x1b),
            "session close must leave the cartridge seated: {close_opcodes:?}"
        );
        assert!(
            !close_opcodes.contains(&0x01),
            "diagnostics must not add a separate REWIND command: {close_opcodes:?}"
        );

        let check = CatalogIndex::open(&index_path).expect("reopen catalog");
        let rows = check
            .list_drive_health_snapshots(&drive_uuid)
            .expect("list snapshots");
        let read_session_text = read_session_id.to_string();
        let write_session_text = write_session_id.to_string();
        assert!(
            rows.iter().any(|row| {
                row.trigger == "read-failure"
                    && row.session_id.as_deref() == Some(read_session_text.as_str())
            }),
            "missing read-failure snapshot: {rows:#?}"
        );
        assert!(
            rows.iter().any(|row| {
                row.trigger == "append-failure"
                    && row.session_id.as_deref() == Some(write_session_text.as_str())
            }),
            "missing append-failure snapshot: {rows:#?}"
        );
    }

    #[test]
    fn fixed_no_compression_config_preserves_drive_reported_fields() {
        let current = TapeConfig {
            block_size: BlockSize::Variable,
            compression: true,
            max_block_size_bytes: 8 * 1024 * 1024,
            write_protected: true,
            worm: WormMediaState::Unknown,
        };

        let prepared = fixed_no_compression_config(current, 4096);

        assert_eq!(prepared.block_size, BlockSize::Fixed { size_bytes: 4096 });
        assert!(!prepared.compression);
        assert_eq!(prepared.max_block_size_bytes, current.max_block_size_bytes);
        assert_eq!(prepared.write_protected, current.write_protected);
        assert_eq!(prepared.worm, current.worm);
    }

    #[test]
    fn object_write_media_policy_requires_positive_rewritable_evidence() {
        let config = |write_protected, worm| TapeConfig {
            block_size: BlockSize::Variable,
            compression: false,
            max_block_size_bytes: 8 * 1024 * 1024,
            write_protected,
            worm,
        };

        validate_write_media_policy(
            config(false, WormMediaState::NotWorm),
            WriteMediaPolicy::RewritableObject,
        )
        .expect("positively identified rewritable media is admitted");

        let worm = validate_write_media_policy(
            config(false, WormMediaState::Worm),
            WriteMediaPolicy::RewritableObject,
        )
        .expect_err("WORM media cannot support whole-Object tail replacement");
        assert_eq!(worm.code(), tonic::Code::FailedPrecondition);
        assert!(worm.message().contains("WORM tape"), "{worm}");

        let unknown = validate_write_media_policy(
            config(false, WormMediaState::Unknown),
            WriteMediaPolicy::RewritableObject,
        )
        .expect_err("unknown WORM state must fail closed");
        assert_eq!(unknown.code(), tonic::Code::FailedPrecondition);
        assert!(unknown.message().contains("state is unknown"), "{unknown}");

        let protected = validate_write_media_policy(
            config(true, WormMediaState::NotWorm),
            WriteMediaPolicy::RewritableObject,
        )
        .expect_err("write-protected media must be refused");
        assert_eq!(protected.code(), tonic::Code::FailedPrecondition);
        assert!(
            protected.message().contains("write-protected"),
            "{protected}"
        );

        for worm in [WormMediaState::Worm, WormMediaState::Unknown] {
            validate_write_media_policy(config(false, worm), WriteMediaPolicy::TerminalAppend)
                .expect("terminal recovery retains its append-only WORM policy");
        }
    }

    #[test]
    fn prepare_object_write_rejects_worm_before_mode_select_or_media_write() {
        const BLOCK_SIZE: u32 = 4096;
        let tape_uuid = [0x4D; 16];
        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-08-11T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(1024 * 1024, BLOCK_SIZE);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK_SIZE);
        tape.worm = true;

        let mut world =
            VirtualWorld::single_drive("LIB-WORM-GATE", 0x0100, "DRV-WORM-GATE", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "WORM001L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let mut drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let command_start = world.lock().expect("world lock").command_log.len();

        let error = prepare_drive_for_write(
            &mut drive,
            &tape_uuid,
            BLOCK_SIZE,
            Uuid::new_v4(),
            WriteMediaPolicy::RewritableObject,
        )
        .expect_err("ordinary Object session must reject WORM media");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("WORM tape"), "{error}");

        let opcodes = world.lock().expect("world lock").command_log[command_start..]
            .iter()
            .map(|command| command.opcode)
            .collect::<Vec<_>>();
        assert!(
            opcodes.contains(&0x1a),
            "the refusal must use current drive-reported media state: {opcodes:02x?}"
        );
        for forbidden in [0x15, 0x0a, 0x10] {
            assert!(
                !opcodes.contains(&forbidden),
                "WORM refusal issued forbidden opcode 0x{forbidden:02x}: {opcodes:02x?}"
            );
        }
    }

    #[test]
    fn prepare_drive_for_read_sets_catalog_fixed_block_size_and_rejects_missing_geometry() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-read-mode-prepare-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let tape_uuid = [0x44; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "DATA044L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");

        let mut world = VirtualWorld::single_drive("LIB-READ-PREP", 0x0100, "DRV-READ", 0x0400, 1);
        world.put_tape_in_drive(
            0x0100,
            "DATA044L9",
            Some(0x0400),
            VirtualTape::empty(1024 * 1024, 1024),
        );
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let mut drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");

        prepare_drive_for_read(&index, &mut drive, &tape_uuid, Uuid::new_v4())
            .expect("prepare fixed read mode");

        assert_eq!(
            world
                .lock()
                .expect("world lock")
                .tapes
                .get("DATA044L9")
                .expect("model tape")
                .block_size,
            4096
        );

        let missing_tape_uuid = [0x45; 16];
        let error = prepare_drive_for_read(&index, &mut drive, &missing_tape_uuid, Uuid::new_v4())
            .expect_err("missing catalog geometry must fail closed");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("catalog row is missing"));
    }

    #[tokio::test]
    async fn empty_file_id_ranges_are_payload_relative_real_bytes() {
        let payload: Vec<u8> = (0..1600)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect();
        let fixture = cataloged_payload_fixture(&payload);
        assert!(fixture.layout.files[0].first_chunk_lba.is_some());

        let mid = stream_fixture_range(&fixture, "", 400, 900)
            .await
            .expect("mid range");
        assert_eq!(mid, payload[400..900]);

        let to_eof = stream_fixture_range(&fixture, "", 1200, payload.len() as u64)
            .await
            .expect("range to eof");
        assert_eq!(to_eof, payload[1200..]);

        let empty = stream_fixture_range(&fixture, "", 777, 777)
            .await
            .expect("empty range");
        assert!(empty.is_empty());

        let whole = stream_fixture_range(&fixture, "", 0, 0)
            .await
            .expect("whole payload range");
        assert_eq!(whole, payload);
    }

    #[tokio::test]
    async fn member_scoped_ranges_still_resolve_file_id() {
        let payload = b"member scoped range bytes".to_vec();
        let fixture = cataloged_payload_fixture(&payload);

        let got = stream_fixture_range(&fixture, "payload-file", 7, 13)
            .await
            .expect("member range");

        assert_eq!(got, b"scoped");
    }

    #[test]
    fn frequency_cap_alarm_triggers_on_recent_run() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-cap")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-CAP".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("mainlib".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T04:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;

        let run = index
            .begin_clean_run(&drive_uuid, "mainlib", "periodic", None)
            .expect("begin run");
        index
            .terminalize_clean_run(run.run_id.as_str(), "done", Some("{\"stage\":\"done\"}"))
            .expect("finish run");

        assert!(
            cleaning_too_soon(&index, &drive_uuid, Duration::seconds(0), 1)
                .expect("frequency check"),
            "one completed run in the current week must hit the weekly cap"
        );
    }

    #[test]
    fn cleaning_alarm_failure_rolls_back_fence_before_error() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-alarm-fail")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let db = rusqlite::Connection::open(&index_path).expect("open sqlite");
        db.execute_batch(
            "create trigger fail_alarm_insert
             before insert on alarms
             begin
               select raise(fail, 'injected alarm failure');
             end;",
        )
        .expect("install alarm failure trigger");
        drop(db);

        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-ALARM".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-ALARM".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;

        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-ALARM",
            0x0100,
            "DRV-ALARM",
            0x0400,
            1,
        )));
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot_cell = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot_cell);
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("alarm failure must fail cleaning");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            !index
                .get_drive_by_uuid(&drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "fence must be rolled back when alarm insertion fails"
        );
    }

    #[test]
    fn periodic_cleaning_defers_on_busy_drive_and_now_fences_after_session_end() {
        let busy_world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-POLICY",
            0x0100,
            "DRV-POLICY",
            0x0400,
            1,
        )));
        {
            let mut world = busy_world.lock().expect("world lock");
            world.put_tape_in_drive(0x0100, "DATA-BUSY", None, VirtualTape::default());
            world.put_tape_in_slot(
                0x0400,
                "CLN-POLICY",
                VirtualTape {
                    cleaning_cart: true,
                    ..VirtualTape::default()
                },
            );
        }
        let busy_library = open_model_library(std::sync::Arc::clone(&busy_world));
        let busy_snapshot = library_snapshot_cell(busy_library.library().clone());
        let busy_temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-periodic")
            .tempdir()
            .expect("temp dir");
        let busy_index_path = busy_temp.path().join("rem-state.sqlite");
        let mut busy_index = CatalogIndex::open(&busy_index_path).expect("open");
        let busy_drive_uuid = busy_index
            .observe_drive(DriveObservationInput {
                serial: "DRV-POLICY".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-POLICY".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:10:00Z".to_string()),
            })
            .expect("observe busy drive")
            .drive_uuid;
        let cln_uuid = [0x91; 16];
        busy_index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: cln_uuid,
                voltag: "CLN-POLICY".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision cleaning tape");
        busy_index
            .set_tape_kind(&cln_uuid, "cleaning")
            .expect("mark cleaning cart")
            .expect("cleaning tape row");
        busy_index
            .set_tape_cleaning_state(&cln_uuid, "ok")
            .expect("mark cleaning cart state")
            .expect("cleaning tape row");
        let busy_cfg = test_write_owner_config(
            busy_index_path.clone(),
            busy_temp.path().join("audit"),
            &busy_library,
            busy_snapshot,
        );
        std::fs::create_dir_all(&busy_cfg.audit_dir).expect("create audit dir");

        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = busy_library;
        assert!(
            run_cleaning_sequence(
                &mut busy_index,
                &busy_cfg,
                &handle,
                &mut library,
                &busy_drive_uuid,
                "periodic",
            )
            .is_ok(),
            "periodic cleaning must defer while the drive is busy"
        );
        assert!(
            !busy_index
                .get_drive_by_uuid(&busy_drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "periodic defer must not fence the drive"
        );
        assert!(
            busy_index
                .get_active_clean_run_by_drive(&busy_drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "periodic defer must not create a clean run"
        );

        let now_world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-NOW", 0x0100, "DRV-NOW", 0x0400, 1,
        )));
        {
            let mut world = now_world.lock().expect("world lock");
            world.put_tape_in_drive(0x0100, "DATA-NOW", None, VirtualTape::default());
            world.put_tape_in_slot(
                0x0400,
                "CLN-NOW",
                VirtualTape {
                    cleaning_cart: true,
                    ..VirtualTape::default()
                },
            );
        }
        let now_library = open_model_library(std::sync::Arc::clone(&now_world));
        let now_snapshot = library_snapshot_cell(now_library.library().clone());
        let now_temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-now")
            .tempdir()
            .expect("temp dir");
        let now_index_path = now_temp.path().join("rem-state.sqlite");
        let mut now_index = CatalogIndex::open(&now_index_path).expect("open");
        let now_drive_uuid = now_index
            .observe_drive(DriveObservationInput {
                serial: "DRV-NOW".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-NOW".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:11:00Z".to_string()),
            })
            .expect("observe now drive")
            .drive_uuid;
        let now_uuid = [0x92; 16];
        now_index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: now_uuid,
                voltag: "CLN-NOW".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision cleaning tape");
        now_index
            .set_tape_kind(&now_uuid, "cleaning")
            .expect("mark cleaning cart")
            .expect("cleaning tape row");
        now_index
            .set_tape_cleaning_state(&now_uuid, "ok")
            .expect("mark cleaning cart state")
            .expect("cleaning tape row");
        let now_cfg = test_write_owner_config(
            now_index_path.clone(),
            now_temp.path().join("audit"),
            &now_library,
            now_snapshot,
        );
        std::fs::create_dir_all(&now_cfg.audit_dir).expect("create audit dir");

        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = now_library;
        let err = run_cleaning_sequence(
            &mut now_index,
            &now_cfg,
            &handle,
            &mut library,
            &now_drive_uuid,
            "now",
        )
        .expect_err("now cleaning should fence and then hit the busy-drive path");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            now_index
                .get_drive_by_uuid(&now_drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "now cleaning must fence the drive"
        );
    }

    #[test]
    fn no_cln_cart_branch_unfences_drive_and_raises_alarm() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-NOCART",
            0x0100,
            "DRV-NOCART",
            0x0400,
            1,
        )));
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-no-cart")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-NOCART".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-NOCART".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:20:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let cfg = test_write_owner_config(
            index_path.clone(),
            temp.path().join("audit"),
            &library,
            snapshot,
        );
        std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("no-cart branch must stop cleaning");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            !index
                .get_drive_by_uuid(&drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "no-cart branch must leave the drive unfenced"
        );
        assert!(
            index
                .get_alarm(format!("no-cln-cart:{}", library.library().serial).as_str())
                .expect("alarm lookup")
                .is_some_and(|alarm| alarm.state == "open"),
            "no-cart branch must raise the standing alarm"
        );
        assert!(
            index
                .get_active_clean_run_by_drive(&drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "no-cart branch must not leave an active clean run"
        );
    }

    #[test]
    fn cleaning_frequency_cap_refuses_before_fence_or_run() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-CAP", 0x0100, "DRV-CAP", 0x0400, 1,
        )));
        {
            let mut world = world.lock().expect("world lock");
            world.put_tape_in_slot(
                0x0400,
                "CLN-CAP",
                VirtualTape {
                    cleaning_cart: true,
                    ..VirtualTape::default()
                },
            );
        }
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-frequency-cap")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-CAP".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-CAP".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:30:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let completed = index
            .begin_clean_run(&drive_uuid, "LIB-CAP", "now", None)
            .expect("begin prior run");
        index
            .terminalize_clean_run(
                completed.run_id.as_str(),
                "done",
                Some("{\"stage\":\"done\"}"),
            )
            .expect("finish prior run");
        let cfg = WriteOwnerConfig {
            cleaning: remanence_state::CleaningConfig {
                weekly_cap: 1,
                min_interval: "0s".to_string(),
                ..remanence_state::CleaningConfig::default()
            },
            ..test_write_owner_config(
                index_path.clone(),
                temp.path().join("audit"),
                &library,
                snapshot,
            )
        };
        std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("frequency cap must reject");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            !index
                .get_drive_by_uuid(&drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "frequency cap must not fence the drive"
        );
        assert!(
            index
                .get_active_clean_run_by_drive(&drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "frequency cap must not leave an active clean run"
        );
        assert!(
            index
                .get_alarm(
                    format!(
                        "drive-cleaning-abnormal-frequency:{}",
                        crate::bytes_to_hex(&drive_uuid)
                    )
                    .as_str()
                )
                .expect("alarm lookup")
                .is_some_and(|alarm| alarm.state == "open"),
            "frequency cap must raise the abnormal-frequency alarm"
        );
    }

    #[test]
    fn inventory_only_cleaning_cart_is_recognized_before_fast_eject() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-FAST", 0x0100, "DRV-FAST", 0x0400, 1,
        )));
        {
            let mut world = world.lock().expect("world lock");
            world.put_tape_in_slot(
                0x0400,
                "CLNU01L9",
                VirtualTape {
                    cleaning_cart: true,
                    cleaning_cart_expired: true,
                    ..VirtualTape::default()
                },
            );
        }
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-fast-eject")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-FAST".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-FAST".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:40:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let cfg = test_write_owner_config(
            index_path.clone(),
            temp.path().join("audit"),
            &library,
            snapshot,
        );
        std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("fast-eject cart must be rejected");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            !err.message().contains("no eligible cleaning cartridge"),
            "inventory-only cart must reach the physical cleaning path: {err}"
        );
        let cart = index
            .get_tape_by_voltag("CLNU01L9")
            .expect("cleaning cart lookup")
            .expect("inventory cart registered");
        assert_eq!(cart.kind, "cleaning");
        assert_eq!(
            index
                .get_tape_cleaning_state(cart.tape_uuid.as_slice())
                .expect("cleaning state lookup")
                .flatten()
                .as_deref(),
            Some("expired")
        );
        assert!(
            index
                .get_active_clean_run_by_drive(&drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "fast-eject path should not leave the selected clean run active"
        );
    }

    #[tokio::test]
    async fn encrypted_payload_is_served_opaque_and_decrypted_client_side() {
        let mut encrypted_opts = RemTarObjectOptions::new(
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "caller-encrypted",
            "2026-06-16T12:00:00Z",
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
        );
        encrypted_opts.chunk_size = 512;
        let secret: Vec<u8> = (0..1800)
            .map(|value| u8::try_from((value * 7) % 251).unwrap())
            .collect();
        let encrypted_files = [RemTarFile {
            path: "secret.bin",
            file_id: "secret-file",
            data: secret.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        }];
        let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
        let recipients = vec![
            primary.public_key(0).unwrap(),
            recovery.public_key(1).unwrap(),
        ];
        let mut encrypted_sink = VecBlockSink::new();
        let encrypted_report = write_encrypted_rem_object(
            &mut encrypted_sink,
            &encrypted_opts,
            &encrypted_files,
            &recipients,
        )
        .expect("write encrypted payload");
        let encrypted_payload: Vec<u8> = encrypted_sink.blocks.iter().flatten().copied().collect();
        assert_eq!(&encrypted_payload[0..4], b"REMO");

        let fixture = cataloged_payload_fixture(&encrypted_payload);
        let header = stream_fixture_range(&fixture, "", 0, 64)
            .await
            .expect("opaque header range");
        assert_eq!(header, encrypted_payload[0..64]);

        let opaque = stream_fixture_range(&fixture, "", 0, encrypted_payload.len() as u64)
            .await
            .expect("opaque encrypted payload");
        assert_eq!(opaque, encrypted_payload);

        let opened = read_encrypted_rem_object_file_range_to_vec(
            &opaque,
            &primary,
            encrypted_report.plaintext_layout.files[0].first_chunk_lba,
            secret.len() as u64,
            300,
            333,
        )
        .expect("client-side decrypt range");
        assert_eq!(opened.bytes, secret[300..633]);
    }

    #[tokio::test]
    async fn invalid_payload_ranges_return_typed_status() {
        let payload = b"short payload".to_vec();
        let fixture = cataloged_payload_fixture(&payload);

        let past_eof = stream_fixture_range(&fixture, "", 99, 100)
            .await
            .expect_err("past EOF must fail");
        assert_eq!(past_eof.code(), tonic::Code::InvalidArgument);

        let overflow_request = file_range_read_request(
            &fixture.index,
            &RANGE_TAPE_UUID,
            RANGE_OBJECT_ID,
            "",
            u64::MAX - 1,
            u64::MAX,
        )
        .expect("request builder allows planner to catch arithmetic overflow");
        let mut source = VecBlockSource::new(fixture.blocks.clone());
        let (tx, _rx) = crate::read_core::read_stream_channel(8);
        let overflow = stream_file_range_from_source(
            &mut source,
            overflow_request,
            0,
            tx,
            &TapeIoConfig::default(),
            test_io_memory(),
        )
        .expect_err("overflow must fail");
        assert_eq!(overflow.code(), tonic::Code::InvalidArgument);

        let reversed =
            file_range_read_request(&fixture.index, &RANGE_TAPE_UUID, RANGE_OBJECT_ID, "", 5, 4)
                .expect_err("end before start must fail");
        assert_eq!(reversed.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn terminal_inventory_stream_proto_preserves_complete_book_rows() {
        let structural = terminal_inventory_event_to_proto(
            remanence_parity::TerminalInventoryStreamEvent::StructuralEntry {
                attempt_id: 7,
                replica_ordinal: 3,
                entry: remanence_parity::TapeIndexReplicaMapEntry {
                    tape_file_number: u64::MAX,
                    kind: remanence_parity::TapeIndexReplicaFileKind::ParitySidecar,
                    block_count: u64::MAX - 1,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: Some(u64::MAX - 3),
                    protected_ordinal_end_exclusive: Some(u64::MAX - 2),
                    epoch_id: Some(u64::MAX - 4),
                },
            },
        );
        let Some(pb::tape_inventory_stream_item::Item::StructuralEntry(structural)) =
            structural.item
        else {
            panic!("structural event must remain structural")
        };
        assert_eq!(structural.attempt_id, 7);
        assert_eq!(structural.replica_ordinal, 3);
        assert_eq!(structural.tape_file_number, u64::MAX);
        assert_eq!(structural.block_count, u64::MAX - 1);
        assert_eq!(structural.protected_ordinal_start, Some(u64::MAX - 3));
        assert_eq!(structural.epoch_id, Some(u64::MAX - 4));

        let object = terminal_inventory_event_to_proto(
            remanence_parity::TerminalInventoryStreamEvent::ObjectRow {
                attempt_id: 7,
                replica_ordinal: 3,
                row: remanence_parity::TapeIndexReplicaObjectRow {
                    tape_file_number: u64::MAX - 8,
                    stored_block_count: u64::MAX - 9,
                    object_id: b"complete-object-id".to_vec(),
                    representation: remanence_parity::ObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: u64::MAX - 10,
                        manifest_size_bytes: u64::MAX - 11,
                        manifest_chunk_count: u64::MAX - 12,
                        manifest_sha256: [0x5a; 32],
                    },
                },
            },
        );
        let Some(pb::tape_inventory_stream_item::Item::ObjectRow(object)) = object.item else {
            panic!("Object event must remain an Object row")
        };
        assert_eq!(object.object_id, b"complete-object-id");
        let Some(pb::tape_inventory_object_row::Representation::Plaintext(plaintext)) =
            object.representation
        else {
            panic!("plaintext recovery anchors must be present")
        };
        assert_eq!(plaintext.manifest_first_chunk_lba, u64::MAX - 10);
        assert_eq!(plaintext.manifest_sha256, vec![0x5a; 32]);
    }
}
