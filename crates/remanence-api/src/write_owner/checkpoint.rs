//! Parity-session authority and durable checkpoint barriers.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration as StdDuration, Instant};

use remanence_library::DriveHandle;
use remanence_parity::{
    checked_bounded_resume_summary, BoundedResumeSummary, BoundedResumeWriterSeed, CloseReason,
    DriveHandleRawSink, FileTapeFileJournal, FileTapeFileJournalCommittedSnapshot, ParityError,
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
}

impl<'a> ActivityTrackingRawTapeSink<'a> {
    pub(super) fn new(inner: &'a mut dyn RawTapeSink, write_attempted: &'a mut bool) -> Self {
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

pub(super) fn parity_journal_path(
    cfg: &WriteOwnerConfig,
    tape_uuid: TapeUuid,
) -> Result<PathBuf, Status> {
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
    pub(super) catalog_projected: bool,
    pub(super) fence_handled: bool,
}

impl CheckpointBarrierFailure {
    pub(super) fn before_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            catalog_projected: false,
            fence_handled: false,
        }
    }

    pub(super) fn before_journal_with_fence_handled(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            catalog_projected: false,
            fence_handled: true,
        }
    }

    pub(super) fn after_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            catalog_projected: false,
            fence_handled: false,
        }
    }

    pub(super) fn after_projection(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
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
    if let Err(err) = journal.append(&record) {
        return Err(if err.is_checkpoint_append_authority_uncertain() {
            CheckpointBarrierFailure::after_journal(Status::internal(format!(
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

pub(super) fn fence_failed_checkpoint_batch(
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

pub(super) fn parity_raw_write_fence_reason(error: &str) -> &'static str {
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
pub(super) fn fence_failed_parity_raw_write(
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
