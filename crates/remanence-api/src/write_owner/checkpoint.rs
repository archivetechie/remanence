//! Parity-session authority and durable checkpoint barriers.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration as StdDuration, Instant};

use remanence_library::{DriveHandle, TapeIoError};
use remanence_parity::{
    checked_bounded_resume_summary, rebuild_open_epoch_from_bounded_summary, resume_record_result,
    BoundedResumeSummary, BoundedResumeWriterSeed, CloseReason, DriveHandleRawSink,
    DriveHandleRawSource, FileTapeFileJournal, FileTapeFileJournalCommittedSnapshot, ParityError,
    ParitySink, ParitySinkSessionState, PhysicalPositionHint, RawTapeSink, RawWriteOutcome,
};
use remanence_state::{AuditEvent, CatalogIndex, TapePoolConfig};
use tokio::sync::mpsc;
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::DriveCommand;
use super::actor_runtime::WriteOwnerConfig;
use super::restore::{status_from_parity_error, status_from_pool_write_error};
use super::terminal_finalize::{
    finalize_terminal_no_parity, finalize_terminal_with_parity_journal,
};
use super::terminal_types::TerminalFinalizeSpec;
use super::write_session::{append_tape_io_fence_evidence, append_tape_sealed_evidence};
use super::{CheckpointTrigger, ParkedCartridge, SeatedCartridge, WriteAdmissionReservation};
use crate::catalog_conversion::object_record_to_proto;
use crate::{pb, SelectedTape, TapeUuid};

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
pub(super) struct SessionAppendGate {
    pub(super) poisoned: bool,
    pub(super) sealed: bool,
}

#[derive(Debug)]
pub(crate) struct PendingCheckpointBatch {
    pub(super) batch_id: Uuid,
    pub(super) opened_at: Instant,
    pub(super) deadline: Instant,
    pub(super) logical_bytes: u64,
    pub(super) used_bytes: u64,
    pub(super) early_warning: bool,
    pub(super) objects: Vec<crate::pool_write::PoolWriteResult>,
    /// Daemon-wide idempotency/UUID claims remain live until this batch is
    /// either projected or abandoned. This closes the gap between the
    /// pre-motion catalog read and the later checkpoint transaction.
    pub(super) _write_admissions: Vec<WriteAdmissionReservation>,
}

pub(super) struct ParityActorSession {
    pub(super) scheme: remanence_parity::ParityScheme,
    pub(super) sink_state: Option<ParitySinkSessionState>,
    pub(super) journal: Option<FileTapeFileJournal>,
    pub(super) retention: Option<crate::io_memory::IoMemoryPermit>,
    pub(super) close_at_barrier: bool,
}

/// One seated cartridge's accumulator, owned by its drive actor between sessions.
pub(super) struct RetainedParityEpoch {
    pub(super) state: ParitySinkSessionState,
    pub(super) permit: crate::io_memory::IoMemoryPermit,
    pub(super) selected: SelectedTape,
    /// Zero denotes a seated cartridge without a known home slot.
    pub(super) parked_generation: u64,
    pub(super) identity_generation: u64,
    pub(super) head_at_eod: bool,
}

/// Reserve parity shards, their vector headers, the codec matrix and a full epoch's CRCs.
pub(super) fn retained_epoch_bytes(
    scheme: &remanence_parity::ParityScheme,
    block_size: u32,
) -> u64 {
    let stripes = u64::from(scheme.stripes_per_neighborhood);
    let parity = u64::from(scheme.parity_blocks_per_stripe);
    let data = u64::from(scheme.data_blocks_per_stripe);
    let header = std::mem::size_of::<Vec<u8>>() as u64;
    std::mem::size_of::<ParitySinkSessionState>() as u64
        + stripes * parity * u64::from(block_size)
        + (stripes * (parity + 1) + parity) * header
        + (stripes * data).next_power_of_two().max(4) * 8
        + parity * data
}

pub(super) struct ParityActorAuthority {
    pub(super) scheme: remanence_parity::ParityScheme,
    pub(super) snapshot: FileTapeFileJournalCommittedSnapshot,
    pub(super) summary: BoundedResumeSummary,
    pub(super) journal: FileTapeFileJournal,
}

/// Records entry into a direct parity raw-write boundary. Position-only
/// validation does not mark media dirty; the first write command does.
pub(super) struct ActivityTrackingRawTapeSink<'a> {
    inner: &'a mut dyn RawTapeSink,
    write_attempted: &'a mut bool,
    position_ready: bool,
    #[cfg(test)]
    fail_next_write: bool,
}

impl<'a> ActivityTrackingRawTapeSink<'a> {
    pub(super) fn new(inner: &'a mut dyn RawTapeSink, write_attempted: &'a mut bool) -> Self {
        Self {
            inner,
            write_attempted,
            position_ready: false,
            #[cfg(test)]
            fail_next_write: false,
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
    fn locate_for_overwrite(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.position_ready = false;
        self.inner.locate_for_overwrite(hint)
    }

    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.ensure_position_ready()?;
        *self.write_attempted = true;
        let result = self.inner.write_fixed_block(buf);
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_write) {
            return Err(ParityError::Invariant(
                "injected epoch close write completion failure",
            ));
        }
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

pub(super) fn parity_journal_path(
    cfg: &impl super::terminal_types::TerminalConfig,
    tape_uuid: TapeUuid,
) -> Result<PathBuf, Status> {
    let cfg = &cfg.terminal_config();
    let journal_dir = cfg.checkpoint_journal_dir.parent().ok_or_else(|| {
        Status::internal("checkpoint journal directory has no parent for the parity journal")
    })?;
    Ok(journal_dir.join(format!("{}.remjournal", crate::bytes_to_hex(&tape_uuid))))
}

pub(super) fn validate_parity_actor_authority(
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
    let journal = resume_record_result(FileTapeFileJournal::open(
        path,
        selected.tape_uuid,
        selected.block_size,
        scheme.clone(),
    ))
    .map_err(|err| status_from_parity_error(&err, format!("open parity tape journal: {err}")))?;
    if journal.orphaned_bundles_preserved_on_open() != 0 {
        tracing::warn!(
            tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
            orphaned_bundle_count = journal.orphaned_bundles_preserved_on_open(),
            "preserved sink-journal bundles beyond the last checkpoint watermark; reconciliation required"
        );
    }
    let snapshot = resume_record_result(journal.committed_snapshot_bounded()).map_err(|err| {
        status_from_parity_error(&err, format!("freeze parity tape journal: {err}"))
    })?;
    let summary = checked_bounded_resume_summary(&snapshot)
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
    crate::pool_write::validate_parity_resume_checkpoint_authority(
        checkpoint,
        &journal,
        checkpoints.is_empty(),
        summary.committed_tape_file_count,
    )
    .map_err(|refusal| match refusal {
        crate::pool_write::ResumeAuthorityRefusal::Records(err) => {
            status_from_parity_error(&err, err.to_string())
        }
        crate::pool_write::ResumeAuthorityRefusal::State(err) => {
            crate::status_from_state_error(err)
        }
    })?;
    Ok(ParityActorAuthority {
        scheme,
        snapshot,
        summary,
        journal,
    })
}

pub(super) fn open_parity_actor_session(
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
                    remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
                )?;
                sink.into_session_state()
            })()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?
        };
        drive
            .locate_admitted_append(0)
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
            .locate_admitted_append(checkpoint.eod_lba)
            .map_err(|err| Status::unavailable(format!("locate parity checkpoint EOD: {err}")))?;
        let plan = summary
            .append_plan(&scheme)
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        let live_epoch = if plan.highest_protected_ordinal_before_rebuild < plan.next_data_ordinal {
            let mut source = EpochRebuildSource {
                inner: DriveHandleRawSource::new(drive),
                contradiction: false,
            };
            let rebuilt = rebuild_open_epoch_from_bounded_summary(
                &mut source,
                &summary,
                &scheme,
                selected.tape_uuid,
                selected.block_size,
            );
            let contradiction = source.contradiction;
            rebuilt
                .map_err(|err| {
                    let status = status_from_parity_error(&err, err.to_string());
                    if contradiction {
                        fence_open_epoch(
                            index,
                            cfg,
                            selected.tape_uuid,
                            (
                                plan.highest_protected_ordinal_before_rebuild,
                                plan.next_data_ordinal,
                            ),
                            &status,
                        );
                    }
                    status
                })?
                .live_epoch
        } else {
            None
        };
        drive
            .locate_admitted_append(checkpoint.eod_lba)
            .map_err(|err| Status::unavailable(format!("restore parity append position: {err}")))?;
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
                live_epoch,
            },
            remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
        )
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        sink.into_session_state()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?
    };
    let needs_retention = sink_state
        .terminal_triple_capacity_runtime_state()
        .map_err(|error| status_from_parity_error(&error, error.to_string()))?
        .current_epoch_fill_blocks
        > 0;
    let retention = if needs_retention {
        cfg.io_memory.try_reserve_epoch(
            retained_epoch_bytes(&scheme, selected.block_size),
            format!(
                "tape {} drive {:?}",
                Uuid::from_bytes(selected.tape_uuid),
                drive.endpoint().durable_key()
            ),
        )
    } else {
        None
    };
    if needs_retention && retention.is_none() {
        tracing::info!(tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
            reservation = retained_epoch_bytes(&scheme, selected.block_size),
            ceiling = cfg.io_memory.ceiling(), retained_epochs = %cfg.io_memory.retained_diagnostics(),
            "session open refuses parity epoch retention at the memory ceiling");
    }
    Ok(ParityActorSession {
        scheme,
        sink_state: Some(sink_state),
        journal: Some(journal),
        retention,
        close_at_barrier: false,
    })
}

impl PendingCheckpointBatch {
    pub(super) fn new(max_age: StdDuration) -> Self {
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

    pub(super) fn push(
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

    pub(super) fn should_checkpoint(&self, cfg: &WriteOwnerConfig) -> bool {
        self.logical_bytes >= cfg.checkpoint_max_bytes
            || self.objects.len() as u64 >= cfg.checkpoint_max_objects
    }

    pub(super) fn quarantine_write_admissions_until_restart(&mut self) {
        for admission in &mut self._write_admissions {
            admission.quarantine_until_restart();
        }
    }
}

pub(super) struct BarrierOutcome {
    pub(super) committed_objects: Vec<pb::ObjectRecord>,
    pub(super) object_count: u64,
    pub(super) logical_bytes: u64,
    pub(super) filemark_drain: StdDuration,
    pub(super) journal_projection: StdDuration,
    pub(super) checkpoint_record: remanence_state::CheckpointJournalRecord,
    pub(super) terminal_checkpoint_record: Option<remanence_state::CheckpointJournalRecord>,
    pub(super) sealed_after_write: bool,
}

pub(super) fn extend_durable_checkpoint_records(
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
pub(super) fn project_checkpoint_authority_bounded(
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
pub(super) struct CheckpointBarrierFailure {
    pub(super) status: Status,
    pub(super) journal_durable: bool,
    /// The append lacks a proven durable journal boundary.
    pub(super) durability_uncertain: bool,
    pub(super) catalog_projected: bool,
    pub(super) fence_handled: bool,
}

impl CheckpointBarrierFailure {
    pub(super) fn before_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            durability_uncertain: true,
            catalog_projected: false,
            fence_handled: false,
        }
    }

    pub(super) fn before_journal_with_fence_handled(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            durability_uncertain: true,
            catalog_projected: false,
            fence_handled: true,
        }
    }

    pub(super) fn after_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            durability_uncertain: false,
            catalog_projected: false,
            fence_handled: false,
        }
    }

    /// Preserve uncertain journal authority separately from a durable projection failure.
    pub(super) fn uncertain_journal(status: Status) -> Self {
        Self {
            durability_uncertain: true,
            ..Self::after_journal(status)
        }
    }

    pub(super) fn after_projection(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            durability_uncertain: false,
            catalog_projected: true,
            fence_handled: false,
        }
    }

    pub(super) fn requires_identity_quarantine(&self) -> bool {
        self.journal_durable && !self.catalog_projected
    }
}

/// Rebuild committed-object receipts from the catalog projection made durable by a barrier.
///
/// The caller's pre-barrier WRITTEN acknowledgement is deliberately locator-free, while the
/// pending batch contains pre-projection write results. Reading the durable record's projected
/// object ids back keeps the CHECKPOINTED response aligned with the catalog's canonical
/// object/copy protobuf conversion.
pub(super) fn checkpointed_objects_from_catalog(
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
            let mut record = object_record_to_proto(object).map_err(|err| {
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
pub(super) fn perform_checkpoint_barrier(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    journal: &mut remanence_state::FileCheckpointJournalLease,
    tape_uuid: TapeUuid,
    checkpoint_ordinal: &mut u64,
    tape_committed_object_count: &mut u64,
    batch: &PendingCheckpointBatch,
    parity_session: Option<&mut ParityActorSession>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    cfg: &WriteOwnerConfig,
) -> Result<BarrierOutcome, CheckpointBarrierFailure> {
    perform_checkpoint_barrier_mode(
        index,
        drive,
        journal,
        tape_uuid,
        checkpoint_ordinal,
        tape_committed_object_count,
        batch,
        parity_session,
        selected,
        pool_cfg,
        cfg,
        true,
        &mut false,
    )
}

/// Run the checkpoint funnel with an explicit terminal-transition policy.
#[allow(clippy::too_many_arguments)]
fn perform_checkpoint_barrier_mode(
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
    allow_finalization: bool,
    media_written: &mut bool,
) -> Result<BarrierOutcome, CheckpointBarrierFailure> {
    let allow_finalization = allow_finalization
        && !cfg
            .lifecycle
            .as_ref()
            .is_some_and(|l| l.shutting_down.load(Ordering::SeqCst));
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
        if parity_session.retention.is_none() && !parity_session.close_at_barrier {
            let bytes = retained_epoch_bytes(&parity_session.scheme, selected.block_size);
            parity_session.retention = cfg.io_memory.try_reserve_epoch(
                bytes,
                format!(
                    "tape {} drive {:?}",
                    Uuid::from_bytes(tape_uuid),
                    drive.endpoint().durable_key()
                ),
            );
            tracing::info!(target: "remanence_write_diag", phase = "parity_retention",
                tape_uuid = %Uuid::from_bytes(tape_uuid), reserved_bytes = bytes,
                retained = parity_session.retention.is_some(),
                close_reason = if parity_session.retention.is_some() { "none" } else { "io_memory_ceiling" },
                "remanence_write_diag");
        }
        let parity_journal = parity_session.journal.as_mut().ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "active parity session has no append journal",
            ))
        })?;
        let mut raw_write_attempted = false;
        let barrier_result = {
            let mut raw = DriveHandleRawSink::new(drive);
            let mut tracked = ActivityTrackingRawTapeSink::new(&mut raw, &mut raw_write_attempted);
            #[cfg(test)]
            {
                tracked.fail_next_write =
                    cfg.io_memory.epoch_test_action.load(Ordering::Relaxed) == 1;
            }
            (|| -> Result<_, ParityError> {
                let mut sink = ParitySink::from_session_state(&mut tracked, parity_journal, state)?;
                let closed =
                    if parity_session.retention.is_some() && !parity_session.close_at_barrier {
                        sink.checkpoint()?
                    } else {
                        sink.close_open_epoch(CloseReason::Barrier)?
                    };
                let sink_state = sink.into_session_state()?;
                Ok((closed, sink_state))
            })()
        };
        *media_written |= raw_write_attempted;
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
        if sink_state
            .terminal_triple_capacity_runtime_state()
            .is_ok_and(|runtime| runtime.current_epoch_fill_blocks == 0)
        {
            parity_session.retention = None;
        }
        parity_session.sink_state = Some(sink_state);
        (
            closed.next_tape_file_number,
            selected.block_size,
            closed.barrier_outcome,
            closed.committed_bundle,
            Some(parity_session.scheme.clone()),
            parity_early_warning,
        )
    } else {
        *media_written = true;
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
    #[cfg(test)]
    let captured = if cfg.io_memory.epoch_test_action.load(Ordering::Relaxed) == 2 {
        remanence_library::TapePosition {
            lba: captured.lba + 1,
            ..captured
        }
    } else {
        captured
    };
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
    let mut record = remanence_state::CheckpointJournalRecord {
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
        seal_due: None,
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
    #[cfg(test)]
    let seal_reason = if matches!(
        cfg.io_memory.epoch_test_action.load(Ordering::Relaxed),
        7 | 11
    ) {
        Some(crate::pool_write::TapeSealReason::HardwareEarlyWarning)
    } else {
        seal_reason
    };
    record.seal_due = seal_reason
        .filter(|_| !allow_finalization)
        .map(|reason| match reason {
            crate::pool_write::TapeSealReason::HardwareEarlyWarning => {
                remanence_state::SealDueReason::HardwareEarlyWarning
            }
            _ => remanence_state::SealDueReason::ReachedLowWatermark,
        });
    let filemark_drain = drain_started.elapsed();
    let projection_started = Instant::now();
    #[cfg(test)]
    if cfg.io_memory.epoch_test_action.load(Ordering::Relaxed) == 3 {
        return Err(CheckpointBarrierFailure::before_journal(Status::internal(
            "injected checkpoint journal append failure",
        )));
    }
    if let Err(err) = journal.append(&record) {
        return Err(if err.is_checkpoint_append_authority_uncertain() {
            CheckpointBarrierFailure::uncertain_journal(Status::internal(format!(
                "checkpoint batch {} journal append failed with durable authority uncertain; close the session and retry only after journal reconciliation: {err}",
                batch.batch_id,
            )))
        } else {
            CheckpointBarrierFailure::before_journal(Status::internal(format!(
                "checkpoint batch {} journal fsync failed with rollback proved; re-send all {} WRITTEN objects: {err}",
                batch.batch_id,
                batch.objects.len(),
            )))
        });
    }
    *checkpoint_ordinal = next_ordinal;
    *tape_committed_object_count = next_committed_count;
    // Hold a competing SQLite writer only across projection, after journal sync.
    #[cfg(test)]
    let projection_lock = if matches!(
        cfg.io_memory.epoch_test_action.load(Ordering::Relaxed),
        10 | 11
    ) {
        let connection = rusqlite::Connection::open(&cfg.index_path).unwrap();
        connection.execute_batch("BEGIN IMMEDIATE").unwrap();
        Some(connection)
    } else {
        None
    };
    let projection = index.project_checkpoint_record(&record);
    #[cfg(test)]
    drop(projection_lock);
    projection.map_err(|err| {
            tracing::error!(%err, "durable checkpoint awaits SQLite projection through journal replay");
            CheckpointBarrierFailure::after_journal(Status::internal(format!(
                "checkpoint is durable in the journal but SQLite projection failed; close the session and retry after journal replay: {err}"
            )))
        })?;
    #[cfg(test)]
    if cfg.io_memory.epoch_test_action.load(Ordering::Relaxed) == 9 {
        return Err(CheckpointBarrierFailure::after_projection(
            Status::internal("injected post-projection failure"),
        ));
    }
    if record.seal_due.is_some() {
        tracing::info!(tape_uuid = %Uuid::from_bytes(tape_uuid), ?seal_reason,
            "terminal seal is due; removal leaves finalization to the next write session or explicit finalize");
    }
    let terminal_checkpoint_record =
        if let Some(reason) = seal_reason.filter(|_| allow_finalization) {
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
                    let carried = parity_session
                        .as_deref_mut()
                        .and_then(|s| s.sink_state.take());
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
                        carried,
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

#[allow(clippy::too_many_arguments)]
pub(super) fn fence_failed_checkpoint_batch(
    drive: &mut remanence_library::DriveHandle,
    lease: &remanence_state::FileCheckpointJournalLease,
    journal: Option<&mut FileTapeFileJournal>,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    batch: &PendingCheckpointBatch,
    status: Status,
) -> Status {
    let status = match crate::append_admission::abandon_pending_append(drive, lease, journal) {
        Ok(()) => status,
        Err(error) => Status::unavailable(format!(
            "{}; abandonment marker failed: {error}",
            status.message()
        )),
    };
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
    match index.record_tape_io_fence_at_endpoint(
        remanence_state::TapeIoFenceInput {
            tape_uuid: selected.tape_uuid,
            barcode,
            reason: "checkpoint_barrier_failed".to_string(),
            evidence_json: Some(evidence),
        },
        selected.standalone_endpoint.as_ref(),
    ) {
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

pub(super) fn parity_raw_write_fence_reason(error: &str) -> &'static str {
    if error.contains("TailBeyondKnownState") || error.contains("append admission") {
        "append_admission_refused"
    } else if error.contains("partial fixed batch uncommittable") {
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
pub(super) fn fence_failed_parity_raw_write(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    selected: &SelectedTape,
    phase: &'static str,
    current_caller_object_id: Option<&str>,
    batch: Option<&PendingCheckpointBatch>,
    error: &str,
    status: Status,
) -> (Status, bool) {
    let cfg = &cfg.terminal_config();
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
    let fence = match index.record_tape_io_fence_at_endpoint(
        remanence_state::TapeIoFenceInput {
            tape_uuid: selected.tape_uuid,
            barcode,
            reason: parity_raw_write_fence_reason(error).to_string(),
            evidence_json: Some(evidence),
        },
        selected.standalone_endpoint.as_ref(),
    ) {
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

pub(super) fn arm_checkpoint_timer(
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

pub(super) fn arm_checkpoint_idle_close(
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

pub(super) fn park_timer_closed_session(
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
) -> Result<(), Status> {
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
    let drive_key = mounted.drive_key();
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
        parked
            .by_drive
            .insert(drive_key.clone(), parked_cartridge.clone());
        parked_cartridge
    });
    if let Some(reservation) = cfg.reservations.get(&drive_key) {
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
    pub(super) fn check(&self) -> Result<(), Status> {
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

    pub(super) fn record_failure(&mut self) {
        self.poisoned = true;
    }

    pub(super) fn record_sealed(&mut self) {
        self.sealed = true;
    }
}

/// Close committed data with the same abandonment and fence discipline as session close.
#[allow(clippy::too_many_arguments)]
pub(super) fn close_committed_epoch(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    lease: &mut remanence_state::FileCheckpointJournalLease,
    ordinal: &mut u64,
    count: &mut u64,
    session: &mut ParityActorSession,
    selected: &SelectedTape,
    pool: &TapePoolConfig,
    allow_finalization: bool,
) -> Result<BarrierOutcome, CheckpointBarrierFailure> {
    session.close_at_barrier = true;
    let batch = PendingCheckpointBatch::new(StdDuration::ZERO);
    let mut media_written = false;
    match perform_checkpoint_barrier_mode(
        index,
        drive,
        lease,
        selected.tape_uuid,
        ordinal,
        count,
        &batch,
        Some(session),
        selected,
        pool,
        cfg,
        allow_finalization,
        &mut media_written,
    ) {
        Ok(outcome) => Ok(outcome),
        Err(mut failure) => {
            if failure.fence_handled && failure.durability_uncertain {
                if let Err(error) = crate::append_admission::abandon_pending_append(
                    drive,
                    lease,
                    session.journal.as_mut(),
                ) {
                    tracing::error!(%error, "fenced epoch close abandonment marker fails");
                }
            } else if media_written && failure.durability_uncertain {
                failure.status = fence_failed_checkpoint_batch(
                    drive,
                    lease,
                    session.journal.as_mut(),
                    index,
                    cfg,
                    selected,
                    &batch,
                    failure.status,
                );
            }
            Err(failure)
        }
    }
}

/// Close a seated open epoch, rebuilding when no matching accumulator remains.
/// A fenced cartridge keeps its epoch until append recovery clears the fence.
pub(super) fn close_retained_before_removal(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    bay: u16,
    retained: &mut Option<RetainedParityEpoch>,
    progress: Option<&crate::operations::OperationHandle>,
) -> Result<(), Status> {
    let endpoint = remanence_state::DriveKey::new(&cfg.actor_library_serial, bay);
    let barcode = cfg
        .library_snapshot
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .report
        .library(&cfg.actor_library_serial)
        .and_then(|library| library.drive_bays.iter().find(|d| d.element_address == bay))
        .and_then(|drive| drive.loaded_tape.clone());
    if index
        .tape_io_is_fenced(
            retained.as_ref().map(|e| &e.selected.tape_uuid),
            barcode.as_deref(),
            Some(&endpoint),
        )
        .map_err(crate::status_from_state_error)?
    {
        tracing::info!(
            bay,
            "fenced removal leaves the epoch open until append recovery"
        );
        return Err(Status::failed_precondition(
            "retained epoch blocked by active tape-I/O fence",
        ));
    }
    let mut epoch = retained.take();
    let mut closing_tape = epoch.as_ref().map(|e| e.selected.tape_uuid);
    let mut open_range = None;
    let mut epoch_closed = false;
    // No failure can use the catalog's epoch range until authority projection succeeds.
    let mut projection_failed = true;
    let shutdown = cfg
        .lifecycle
        .as_ref()
        .is_some_and(|l| l.shutting_down.load(Ordering::SeqCst));
    let result = (|| -> Result<(), Status> {
        let snapshot = cfg
            .library_snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let barcode = snapshot
            .report
            .libraries
            .iter()
            .find(|l| l.serial == cfg.actor_library_serial)
            .and_then(|l| {
                l.drive_bays
                    .iter()
                    .find(|d| d.element_address == bay && d.loaded)
            })
            .and_then(|d| d.loaded_tape.as_deref());
        let tape = if let Some(barcode) = barcode {
            index
                .get_tape_by_voltag(barcode)
                .map_err(crate::status_from_state_error)?
        } else if let Some(epoch) = epoch.as_ref() {
            index
                .get_tape(&epoch.selected.tape_uuid)
                .map_err(crate::status_from_state_error)?
        } else {
            None
        };
        let Some(tape) = tape else {
            tracing::info!(
                bay,
                "epoch close skipped: cartridge has no catalog write identity"
            );
            return Ok(());
        };
        let tape_uuid = crate::catalog_conversion::decode_uuid_bytes(&tape.tape_uuid, "tape_uuid")?;
        closing_tape = Some(tape_uuid);
        if !remanence_state::FileCheckpointJournal::exists(&cfg.checkpoint_journal_dir, tape_uuid)
            .map_err(crate::status_from_state_error)?
        {
            tracing::info!(
                bay,
                "epoch close skipped: cartridge has no checkpoint journal"
            );
            return Ok(());
        }
        // Catalog evidence can only skip a close until journal projection succeeds.
        if tape.scheme_id.is_none() {
            tracing::info!(bay, "epoch close skipped: cartridge has no parity scheme");
            return Ok(());
        }
        let Some(pool) = cfg
            .tape_pools
            .iter()
            .find(|p| Some(p.id.as_str()) == tape.pool_id.as_deref())
            .cloned()
        else {
            tracing::info!(tape_uuid = %Uuid::from_bytes(tape_uuid), "epoch close skipped: no pool configuration");
            return Ok(());
        };
        if tape.state != "ready"
            || tape.kind != "data"
            || tape.terminal_finalization.is_some()
            || tape
                .body_format
                .as_deref()
                .is_some_and(|format| format != "rem-object-v1")
        {
            tracing::info!(tape_uuid = %Uuid::from_bytes(tape_uuid), state = %tape.state,
                "epoch close skipped: tape is not writable");
            return Ok(());
        }
        let fences = index
            .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
            .map_err(crate::status_from_state_error)?;
        if !fences.is_empty() {
            tracing::info!(tape_uuid = %Uuid::from_bytes(tape_uuid),
                "fenced removal leaves the epoch open until append recovery");
            return Ok(());
        }
        let journal =
            remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
                .map_err(crate::status_from_state_error)?;
        let mut lease = if journal
            .terminal_finalization_intent()
            .map_err(crate::status_from_state_error)?
            .is_some()
        {
            journal.acquire_exclusive_for_terminal_recovery()
        } else {
            journal.acquire_exclusive()
        }
        .map_err(crate::status_from_state_error)?;
        let records = project_checkpoint_authority_bounded(index, &lease)
            .map_err(crate::status_from_state_error)?;
        let tape = index
            .get_tape(&tape_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::failed_precondition("close tape disappears from catalog"))?;
        if lease
            .terminal_finalization_intent()
            .map_err(crate::status_from_state_error)?
            .is_some()
        {
            tracing::info!(tape_uuid = %Uuid::from_bytes(tape_uuid), "epoch close skipped: terminal intent closes the epoch");
            return Ok(());
        }
        projection_failed = false;
        if tape.highest_protected_ordinal == tape.total_committed_ordinals {
            tracing::info!(bay, "epoch close skipped: journal has no open parity epoch");
            return Ok(());
        }
        open_range = Some((
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals,
        ));
        let pool_id = tape.pool_id.as_deref().unwrap_or("(unpooled)");
        let (block_size, parity_config) = crate::pool_write::selected_tape_geometry(&tape, pool_id)
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let selected = SelectedTape {
            standalone_endpoint: None,
            tape_uuid,
            block_size,
            parity_config,
            pool_id: pool_id.to_owned(),
        };
        if let Err(error) =
            crate::pool_write::ensure_selected_tape_accepts_session_write(index, &pool, &selected)
        {
            if let crate::pool_write::PoolWriteError::State(error) = error {
                return Err(crate::status_from_state_error(error));
            }
            tracing::info!(%error, tape_uuid = %Uuid::from_bytes(tape_uuid), "epoch close skipped: write admission refused");
            return Ok(());
        }
        let parked_matches = epoch.as_ref().is_some_and(|e| {
            cfg.lifecycle.as_ref().is_some_and(|l| {
                l.parked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .by_drive
                    .get(&crate::drive_pool::DriveKey::new(
                        &cfg.actor_library_serial,
                        bay,
                    ))
                    .map_or(e.parked_generation == 0, |p| {
                        p.generation == e.parked_generation && p.seated.tape_uuid == Some(tape_uuid)
                    })
            })
        });
        if shutdown
            && !epoch.as_ref().is_some_and(|e| {
                parked_matches
                    && e.head_at_eod
                    && e.identity_generation == drive.identity_generation()
                    && drive
                        .known_position()
                        .is_some_and(|p| p.partition == 0 && p.lba == e.state.used_tape_blocks())
            })
        {
            let (w, t) = open_range.expect("open tape range");
            tracing::warn!(tape_uuid = %Uuid::from_bytes(tape_uuid), open_epoch_start = w, open_epoch_end = t,
                "epoch_left_open: shutdown dismounts after positioning; the next append rebuilds the epoch");
            audit_removal_epoch(
                index,
                cfg,
                bay,
                Some(tape_uuid),
                "epoch_left_open",
                format!("shutdown cannot prove the head is at EOD; [{w},{t})"),
                Some((w, t)),
            );
            return Ok(());
        }
        let config = drive
            .read_config()
            .map_err(|e| Status::unavailable(e.to_string()))?;
        if let Err(error) = crate::pool_write::require_rewritable_object_media(config) {
            tracing::info!(%error, "epoch close skipped: medium is not writable");
            return Ok(());
        }
        let last = records.last().ok_or_else(|| {
            let error = Status::failed_precondition("open epoch has no checkpoint");
            fence_open_epoch(
                index,
                cfg,
                tape_uuid,
                open_range.expect("open tape range"),
                &error,
            );
            error
        })?;
        if !shutdown {
            super::write_session::prepare_drive_for_write_observing_identity(
                drive,
                &tape_uuid,
                block_size,
                Uuid::new_v4(),
                super::write_session::WriteMediaPolicy::RewritableObject,
                |error| {
                    let contradiction = match error {
                        crate::pool_write::TapeIdentityError::Mismatch { .. }
                        | crate::pool_write::TapeIdentityError::AbsentBootstrap(_) => true,
                        crate::pool_write::TapeIdentityError::Io(error) => {
                            committed_io_contradiction(error, block_size, 1)
                        }
                    };
                    if contradiction {
                        fence_open_epoch(
                            index,
                            cfg,
                            tape_uuid,
                            open_range.expect("open tape range"),
                            &Status::failed_precondition(error.to_string()),
                        );
                    }
                },
            )?;
        }
        let context = crate::append_admission::context(
            tape_uuid,
            block_size,
            last.eod_partition,
            last.eod_lba,
        );
        let _admission = crate::append_admission::admit_append_boundary(
            drive,
            crate::append_admission::AppendAuthority::Checkpoint {
                eod: context.boundary,
            },
            context,
        )
        .map_err(|error| {
            let status = Status::failed_precondition(error.to_string());
            fence_admission_contradiction(index, cfg, tape_uuid, block_size, open_range, &error);
            status
        })?;
        let authority = validate_parity_actor_authority(cfg, &selected, &lease, &records)?;
        #[cfg(test)]
        let authority = if cfg.io_memory.epoch_test_action.load(Ordering::Relaxed) == 8 {
            let mut authority = authority;
            authority.summary.total_committed_ordinals += 1;
            authority
        } else {
            authority
        };
        let carried = epoch.take().filter(|e| {
            parked_matches
                && e.selected.tape_uuid == tape_uuid
                && e.identity_generation == drive.identity_generation()
                && e.state.matches_committed_summary(&authority.summary)
                && e.state.used_tape_blocks() == last.eod_lba
        });
        if shutdown && carried.is_none() {
            return Err(Status::failed_precondition(
                "shutdown retained epoch does not match checkpoint summary",
            ));
        }
        if let Some(progress) = progress {
            progress.publish_state(pb::OperationState::Running, &[("phase", "epoch_close")]);
        }
        let mut session = if let Some(e) = carried {
            ParityActorSession {
                scheme: authority.scheme,
                sink_state: Some(e.state),
                journal: Some(authority.journal),
                retention: Some(e.permit),
                close_at_barrier: true,
            }
        } else {
            tracing::info!(tape_uuid = %Uuid::from_bytes(tape_uuid), "removal rebuilds the committed open epoch");
            open_parity_actor_session(index, drive, cfg, &selected, &records, authority)?
        };
        let mut ordinal = last.ordinal;
        let mut count = last.committed_object_count;
        close_committed_epoch(
            index,
            drive,
            cfg,
            &mut lease,
            &mut ordinal,
            &mut count,
            &mut session,
            &selected,
            &pool,
            false,
        )
        .map_err(|failure| {
            epoch_closed = failure.journal_durable && !failure.durability_uncertain;
            failure.status
        })?;
        Ok(())
    })();
    drive.clear_append_admission();
    if let Err(error) = result {
        if projection_failed {
            tracing::warn!(%error, bay, "epoch_projection_failed: epoch state is unknown until replay");
            audit_removal_epoch(
                index,
                cfg,
                bay,
                closing_tape,
                "epoch_projection_failed",
                format!(
                    "checkpoint projection fails; epoch state is unknown until replay: {error}"
                ),
                None,
            );
            return Ok(());
        }
        // Projection may close the epoch before a later audit or seal-due write fails.
        // Read its current range; unavailable catalog evidence never supplies a range.
        let (range, closed) =
            closing_tape
                .filter(|_| !epoch_closed)
                .map_or((None, epoch_closed), |uuid| match index.get_tape(&uuid) {
                    Ok(Some(tape))
                        if tape.highest_protected_ordinal < tape.total_committed_ordinals =>
                    {
                        (
                            Some((
                                tape.highest_protected_ordinal,
                                tape.total_committed_ordinals,
                            )),
                            false,
                        )
                    }
                    Ok(Some(tape)) => (
                        None,
                        tape.highest_protected_ordinal == tape.total_committed_ordinals,
                    ),
                    Ok(None) => (None, false),
                    Err(cause) => {
                        tracing::warn!(%cause, "cannot read removal epoch evidence");
                        (None, false)
                    }
                });
        if closed {
            tracing::error!(%error, bay, "durable epoch close follow-up fails");
        } else {
            tracing::warn!(%error, bay, "epoch_left_open: epoch close fails before removal");
            audit_removal_epoch(
                index,
                cfg,
                bay,
                closing_tape,
                "epoch_left_open",
                error.to_string(),
                range,
            );
        }
    }
    Ok(())
}

/// Audit an epoch left open by removal or a failed session open.
pub(super) fn audit_removal_epoch(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    bay: u16,
    tape_uuid: Option<TapeUuid>,
    kind: &'static str,
    reason: String,
    range: Option<(u64, u64)>,
) {
    use ciborium::value::Value as CborValue;
    let mut detail = std::collections::BTreeMap::from([
        ("session_kind".into(), CborValue::Text(kind.into())),
        ("abort_reason".into(), CborValue::Text(reason)),
        (
            "drive_bay".into(),
            CborValue::Integer(u64::from(bay).into()),
        ),
        (
            "library_serial".into(),
            CborValue::Text(cfg.actor_library_serial.clone()),
        ),
    ]);
    if let Some((w, t)) = range {
        detail.insert("open_epoch_start".into(), CborValue::Integer(w.into()));
        detail.insert("open_epoch_end".into(), CborValue::Integer(t.into()));
    }
    if let Some(uuid) = tape_uuid {
        detail.insert("tape_uuid".into(), CborValue::Bytes(uuid.to_vec()));
    }
    if let Err(error) = crate::audit_projection::append_operation_audit(
        index,
        &cfg.audit_dir,
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::audit_projection::OperationAuditInput {
            actor: remanence_state::AuditActor::System,
            operation_id: Uuid::new_v4(),
            operation_kind: kind,
            event: AuditEvent::OperationFailed,
            subject_kind: "tape",
            subject_id: tape_uuid.map(|u| Uuid::from_bytes(u).to_string()),
            idempotency_key: None,
            detail,
        },
    ) {
        tracing::error!(%error,"cannot audit removal epoch");
    }
}

/// Persist observed contradictions in a known committed open epoch.
fn fence_open_epoch(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: TapeUuid,
    range: (u64, u64),
    error: &Status,
) {
    record_epoch_fence(
        index,
        cfg,
        tape_uuid,
        "epoch_close_failed",
        serde_json::json!({
            "open_epoch_start": range.0, "open_epoch_end": range.1,
            "error": error.message(), "phase": "open_epoch"
        })
        .to_string(),
    );
}

/// Fence admission contradictions with their phase and any known open range.
pub(super) fn fence_admission_contradiction(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: TapeUuid,
    block_size: u32,
    range: Option<(u64, u64)>,
    error: &crate::append_admission::AdmissionError,
) -> bool {
    use crate::append_admission::AdmissionError;
    let mut evidence = match error {
        AdmissionError::TailBeyondKnownState {
            expected_eod_lba, ..
        } => serde_json::json!({
            "phase": "append_suffix", "suffix_start_lba": expected_eod_lba,
            "error": error.to_string()
        }),
        AdmissionError::Io(cause) if committed_io_contradiction(cause, block_size, 1) => {
            serde_json::json!({ "phase": "append_admission", "error": error.to_string() })
        }
        _ => return false,
    };
    if matches!(error, AdmissionError::Io(_)) {
        if let Some((w, t)) = range.filter(|(w, t)| w < t) {
            evidence["open_epoch_start"] = w.into();
            evidence["open_epoch_end"] = t.into();
        }
    }
    record_epoch_fence(
        index,
        cfg,
        tape_uuid,
        "admission_contradiction",
        evidence.to_string(),
    );
    true
}

/// Persist and audit a tape fence with the evidence supplied by its caller.
fn record_epoch_fence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: TapeUuid,
    reason: &str,
    evidence: String,
) {
    let tape = match index.get_tape(&tape_uuid) {
        Ok(Some(tape)) => tape,
        other => {
            tracing::error!(?other, "cannot read catalog for open epoch fence");
            return;
        }
    };
    if index
        .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
        .is_ok_and(|fences| !fences.is_empty())
    {
        return;
    }
    match index.record_tape_io_fence(remanence_state::TapeIoFenceInput {
        tape_uuid,
        barcode: tape.voltag,
        reason: reason.into(),
        evidence_json: Some(evidence),
    }) {
        Ok(fence) => {
            if let Err(error) =
                append_tape_io_fence_evidence(index, cfg, &fence, AuditEvent::TapeIoFenceRaised)
            {
                tracing::error!(%error, "open epoch fence audit fails");
            }
        }
        Err(error) => tracing::error!(%error, "open epoch fence persistence fails"),
    }
}

/// Classify committed-range failures with the verification reader's completion rules.
fn committed_read_contradiction(error: &ParityError, size: u32, count: u32) -> bool {
    let ParityError::TapeIo(error) = error else {
        return false;
    };
    committed_io_contradiction(error, size, count)
}

/// Classify tape contradictions shared by identity, append admission and rebuild.
fn committed_io_contradiction(error: &TapeIoError, size: u32, count: u32) -> bool {
    if matches!(error, TapeIoError::GoodReadLengthMismatch { .. }) {
        return false;
    }
    if matches!(error, TapeIoError::FilemarkEncountered) {
        return true;
    }
    use remanence_library::VerificationReadCompletion as Completion;
    matches!(
        remanence_library::verification_read_completion(error, size, count).1,
        Completion::MediumError
            | Completion::WrongLength
            | Completion::Filemark
            | Completion::EndOfData
    ) || error.check_condition_transfer().is_some_and(|(sense, _)| {
        remanence_library::decode_scsi_sense(sense).is_some_and(|s| {
            !s.is_deferred()
                && (s.key == 8
                    || (s.key == 0 && s.filemark)
                    // Hardware errors qualify only for unrecovered reads or missing records.
                    || (s.key == 4 && matches!(s.asc, 0x11 | 0x14)))
        })
    })
}

/// Track media contradictions without turning transient drive errors into fences.
struct EpochRebuildSource<'a> {
    inner: DriveHandleRawSource<'a>,
    contradiction: bool,
}
impl remanence_parity::RawTapeSource for EpochRebuildSource<'_> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.inner.verification_context()
    }
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.inner.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let result = self.inner.locate_physical(hint);
        self.contradiction |= result
            .as_ref()
            .is_err_and(|e| committed_read_contradiction(e, 1, 1));
        result
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.locate_end_of_data()
    }
    fn space_filemarks(
        &mut self,
        count: i64,
    ) -> Result<remanence_parity::SpaceFilemarksOutcome, ParityError> {
        self.inner.space_filemarks(count)
    }
    fn read_record(
        &mut self,
        buf: &mut [u8],
    ) -> Result<remanence_parity::RawReadOutcome, ParityError> {
        let result = self.inner.read_record(buf);
        self.contradiction |= match &result {
            Ok(remanence_parity::RawReadOutcome::Block { bytes, .. }) => *bytes != buf.len(),
            Ok(_) => true,
            Err(error) => committed_read_contradiction(error, buf.len() as u32, 1),
        };
        result
    }
    fn read_forward_records(
        &mut self,
        buf: &mut [u8],
        size: u32,
        count: u32,
    ) -> remanence_parity::raw::RawForwardReadBatch {
        let result = self.inner.read_forward_records(buf, size, count);
        self.contradiction |= match &result.batch.end {
            remanence_parity::raw::RawReadEnd::Complete => false,
            remanence_parity::raw::RawReadEnd::Error(error) => {
                committed_read_contradiction(error, size, count)
            }
            _ => true,
        };
        result
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.position()
    }
}

/// A GOOD completion with an inconsistent host count never proves a media contradiction.
#[test]
fn good_read_length_mismatch_stays_transient() {
    let error = remanence_library::TapeIoError::GoodReadLengthMismatch {
        requested_bytes: 4096,
        bytes_transferred: 2048,
    };
    assert!(error.is_completion_unknown());
    assert!(!committed_read_contradiction(
        &ParityError::TapeIo(error),
        4096,
        1
    ));
}
