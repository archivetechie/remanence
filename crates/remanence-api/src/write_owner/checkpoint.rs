//! Parity-session authority and durable checkpoint barriers.

use remanence_state::authority::TapeAuthorityView;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration as StdDuration, Instant};

use remanence_library::{DriveHandle, TapeIoError};
use remanence_parity::{
    checked_bounded_resume_summary, rebuild_open_epoch_from_bounded_summary_checked,
    resume_record_result, BoundedResumeSummary, BoundedResumeWriterSeed, DriveHandleRawSink,
    DriveHandleRawSource, FrozenFileRows, ParityError, ParitySink, ParitySinkSessionState,
    PhysicalPositionHint, RawTapeSink, RawWriteOutcome,
};
use remanence_state::{CatalogIndex, TapePoolConfig};
use tokio::sync::mpsc;
use tonic::Status;
use uuid::Uuid;

use super::actor_protocol::DriveCommand;
use super::actor_runtime::WriteOwnerConfig;
use super::restore::{status_from_parity_error, status_from_selected_write_error};
use super::terminal_types::TerminalFinalizeSpec;
use super::write_session::raise_tape_io_fence;
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
    pub(super) read_attempts: u32,
    pub(super) scheme: remanence_parity::ParityScheme,
    pub(super) sink_state: Option<ParitySinkSessionState>,
    pub(super) permit: Option<crate::io_memory::IoMemoryPermit>,
}

pub(super) fn reserve_closeout_memory(
    cfg: &impl super::terminal_types::TerminalConfig,
    scheme: &remanence_parity::ParityScheme,
    block_size: u32,
    extent_count: usize,
    tape_uuid: TapeUuid,
) -> Result<crate::io_memory::IoMemoryPermit, Status> {
    let bytes = remanence_parity::closeout::EpochAccumulator::working_reservation_bytes(
        scheme,
        block_size,
        extent_count as u64,
    )
    .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    cfg.terminal_config()
        .io_memory
        .try_reserve_epoch(
            bytes,
            format!("tape {} closeout", Uuid::from_bytes(tape_uuid)),
        )
        .map_err(|available| {
            Status::resource_exhausted(format!(
                "CloseoutMemory: need {bytes} bytes, {available} available; closeout remains due."
            ))
        })
}

/// Check rebuilt parity against the frozen last durable original-write evidence.
/// The caller owns its reservation across this function and the resulting state.
pub(super) fn checked_epoch_rebuild(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &impl super::terminal_types::TerminalConfig,
    selected: &SelectedTape,
    summary: &BoundedResumeSummary,
    previous: &remanence_state::authority::AuthorityCommit,
) -> Result<remanence_parity::ResumeOpenEpochRebuild, Status> {
    if previous.tape_uuid != selected.tape_uuid
        || previous.next_file != summary.committed_tape_file_count
        || previous.block_size != selected.block_size
        || previous.scheme.as_ref() != Some(summary.scheme())
        || previous.protected != summary.highest_protected_ordinal
        || previous.total != summary.total_committed_ordinals
        || previous.cursor.lba != summary.append_position.lba
        || previous.cursor.partition != summary.append_position.partition
    {
        return Err(Status::failed_precondition(
            "closeout check authority disagrees with frozen committed range",
        ));
    }
    let check = match previous.check {
        Some((check, remanence_parity::CheckProvenance::OriginalWrite)) => Some(check),
        _ => {
            return Err(Status::failed_precondition(
                "open epoch lacks durable check provenance",
            ))
        }
    };
    let mut source = EpochRebuildSource {
        inner: DriveHandleRawSource::new(drive),
        contradiction: false,
    };
    let result = rebuild_open_epoch_from_bounded_summary_checked(
        &mut source,
        summary,
        summary.scheme(),
        selected.tape_uuid,
        selected.block_size,
        check,
    );
    let contradiction = source.contradiction;
    result.map_err(|error| {
        let mut status = status_from_parity_error(&error, error.to_string());
        if let ParityError::CloseoutCheckMismatch { expected, found } = error {
            status = Status::failed_precondition(format!("CloseoutCheckMismatch: tape {} range [{},{}) differs from commit check {} after two reads (found {}); writes are fenced.", Uuid::from_bytes(selected.tape_uuid), previous.protected, previous.total, expected.0, found.0));
            let barcode = match index.get_tape(&selected.tape_uuid) {
                Ok(tape) => tape.and_then(|t| t.voltag),
                Err(error) => {
                    tracing::error!(%error, tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                        "closeout mismatch barcode lookup failed; fencing by tape UUID");
                    None
                }
            };
            let evidence = serde_json::json!({
                "W": previous.protected, "T": previous.total,
                "commit_ordinal": previous.ordinal,
                "partition": previous.cursor.partition, "lba": previous.cursor.lba,
                "expected": expected.0, "found": found.0, "attempts": 2,
            });
            if let Err(error) = raise_tape_io_fence(
                index,
                cfg,
                remanence_state::TapeIoFenceInput {
                    tape_uuid: selected.tape_uuid,
                    barcode,
                    reason: "CloseoutCheckMismatch".into(),
                    evidence_json: Some(evidence.to_string()),
                },
                cfg.terminal_config().mounted_endpoint.as_ref(),
            ) {
                tracing::error!(%error, tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                    "failed to persist closeout mismatch tape-I/O fence");
                return Status::new(
                    status.code(),
                    format!(
                        "{}; additionally failed to persist the required tape fence: {error}",
                        status.message()
                    ),
                );
            }
        } else if contradiction {
            status = Status::failed_precondition(format!("CloseoutReadContradiction: tape {} committed range [{},{}) contradicts its recorded extents; writes are fenced.", Uuid::from_bytes(selected.tape_uuid), previous.protected, previous.total));
            record_closeout_fence(
                index,
                cfg,
                selected.tape_uuid,
                "CloseoutReadContradiction",
                serde_json::json!({
                    "open_epoch_start": previous.protected,
                    "open_epoch_end": previous.total,
                    "error": status.message(), "phase": "open_epoch",
                })
                .to_string(),
            );
        }
        status
    })
}

pub(super) struct ParityActorAuthority {
    pub(super) scheme: remanence_parity::ParityScheme,
    pub(super) snapshot: FrozenFileRows,
    pub(super) summary: BoundedResumeSummary,
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

/// The refusal a removal gets when a tape-I/O fence keeps it from closing an epoch.
/// The first check runs before the close learns whether an epoch exists.
pub(super) fn raw_write_reached_drive(
    drive: &DriveHandle,
    dispatches_before: u64,
    write_attempted: bool,
) -> bool {
    write_attempted && drive.media_write_dispatch_count() != dispatches_before
}

#[cfg(test)]
pub(super) fn parity_journal_directory(
    cfg: &impl super::terminal_types::TerminalConfig,
    _tape_uuid: TapeUuid,
) -> Result<PathBuf, Status> {
    cfg.terminal_config()
        .checkpoint_journal_dir
        .parent()
        .map(PathBuf::from)
        .ok_or_else(|| Status::internal("checkpoint authority has no journal directory"))
}

pub(super) fn validate_parity_actor_authority(
    _cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    checkpoint: &mut remanence_state::TapeAuthorityLease,
    _checkpoints: &[remanence_state::authority::AuthorityCommit],
) -> Result<ParityActorAuthority, Status> {
    let scheme = match &selected.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity actor session requested for a parity-off tape",
            ));
        }
    };
    let journal =
        resume_record_result(checkpoint.open_structural(selected.block_size, scheme.clone()))
            .map_err(|err| {
                status_from_parity_error(&err, format!("open parity tape journal: {err}"))
            })?;
    if journal.pending_count() != 0 {
        tracing::warn!(
            tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
            orphaned_bundle_count = journal.pending_count(),
            "preserved sink-journal bundles beyond the last checkpoint watermark; reconciliation required"
        );
    }
    let snapshot = resume_record_result(journal.freeze()).map_err(|err| {
        status_from_parity_error(&err, format!("freeze parity tape journal: {err}"))
    })?;
    let summary = checked_bounded_resume_summary(&snapshot)
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
    checkpoint
        .validate_append_rows(&snapshot)
        .map_err(
            |error| match crate::pool_write::ResumeAuthorityRefusal::from(error) {
                crate::pool_write::ResumeAuthorityRefusal::Records(error) => {
                    status_from_parity_error(&error, error.to_string())
                }
                crate::pool_write::ResumeAuthorityRefusal::State(error) => {
                    crate::status_from_state_error(error)
                }
            },
        )?;
    checkpoint
        .retain_structural_publisher(journal)
        .map_err(crate::status_from_state_error)?;
    Ok(ParityActorAuthority {
        scheme,
        snapshot,
        summary,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn open_parity_actor_session(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    _checkpoints: &[remanence_state::authority::AuthorityCommit],
    authority: ParityActorAuthority,
    writer: &mut remanence_state::TapeAuthorityLease,
    rebuild_permit: Option<crate::io_memory::IoMemoryPermit>,
) -> Result<ParityActorSession, Status> {
    let latest = writer
        .latest_commit()
        .map_err(crate::status_from_state_error)?;
    writer
        .with_structural(|_, rows| {
            let rows = rows
                .ok_or_else(|| Status::internal("active parity session has no append journal"))?;
            open_parity_actor_session_with_rows(
                index,
                drive,
                cfg,
                selected,
                latest.as_ref(),
                authority,
                rows,
                rebuild_permit,
            )
        })
        .map_err(crate::status_from_state_error)?
}

#[allow(clippy::too_many_arguments)]
pub(super) fn open_parity_actor_session_with_rows(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    latest: Option<&remanence_state::authority::AuthorityCommit>,
    authority: ParityActorAuthority,
    mut journal: &mut dyn remanence_parity::authority::StructuralAuthority,
    rebuild_permit: Option<crate::io_memory::IoMemoryPermit>,
) -> Result<ParityActorSession, Status> {
    let ParityActorAuthority {
        scheme,
        snapshot,
        summary,
    } = authority;
    let mut rebuild_permit = Some(match rebuild_permit {
        Some(permit) => permit,
        None => reserve_closeout_memory(
            cfg,
            &scheme,
            selected.block_size,
            summary.open_epoch_object_extents.len(),
            selected.tape_uuid,
        )?,
    });
    let mut read_attempts = 0;
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
            Ok(state) => state,
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
        let checkpoint = latest.ok_or_else(|| {
            Status::failed_precondition(
                "non-fresh parity tape has no shared checkpoint journal watermark",
            )
        })?;
        drive
            .prove_cursor(
                crate::append_admission::context(
                    selected.tape_uuid,
                    selected.block_size,
                    checkpoint.cursor.partition,
                    checkpoint.cursor.lba,
                )
                .boundary,
                selected.block_size,
                drive.identity_generation(),
            )
            .map_err(|err| Status::unavailable(format!("locate parity checkpoint EOD: {err}")))?;
        let plan = summary
            .append_plan(&scheme)
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        let live_epoch = if plan.highest_protected_ordinal_before_rebuild < plan.next_data_ordinal {
            let rebuilt = checked_epoch_rebuild(index, drive, cfg, selected, &summary, checkpoint)?;
            read_attempts = rebuilt.read_attempts;
            rebuilt.live_epoch
        } else {
            None
        };
        drive
            .prove_cursor(
                crate::append_admission::context(
                    selected.tape_uuid,
                    selected.block_size,
                    checkpoint.cursor.partition,
                    checkpoint.cursor.lba,
                )
                .boundary,
                selected.block_size,
                drive.identity_generation(),
            )
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
    Ok(ParityActorSession {
        read_attempts,
        scheme,
        sink_state: Some(sink_state),
        permit: rebuild_permit.take(),
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
    pub(super) checkpoint_record: remanence_state::authority::AuthorityCommit,
    pub(super) terminal_checkpoint_record: Option<remanence_state::authority::AuthorityCommit>,
    pub(super) sealed_after_write: bool,
}

pub(super) fn extend_durable_checkpoint_records(
    records: &mut Vec<remanence_state::authority::AuthorityCommit>,
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
    lease: &remanence_state::TapeAuthorityLease,
) -> Result<Vec<remanence_state::authority::AuthorityCommit>, remanence_state::StateError> {
    lease.set_projection_floor(index.projected_through_sequence(lease.tape_uuid())?)?;
    index.project_tape_log(&lease.freeze()?)?;
    Ok(lease.latest_commit()?.into_iter().collect())
}

#[derive(Debug)]
pub(super) struct CheckpointBarrierFailure {
    pub(super) status: Status,
    pub(super) journal_durable: bool,
    /// The append lacks a proven durable journal boundary.
    pub(super) catalog_projected: bool,
}

impl CheckpointBarrierFailure {
    pub(super) fn before_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
            catalog_projected: false,
        }
    }

    pub(super) fn after_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            catalog_projected: false,
        }
    }

    /// Preserve uncertain journal authority separately from a durable projection failure.
    pub(super) fn uncertain_journal(status: Status) -> Self {
        Self {
            ..Self::after_journal(status)
        }
    }

    pub(super) fn after_projection(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
            catalog_projected: true,
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
    checkpoint_record: &remanence_state::authority::AuthorityCommit,
    sealed_after_write: bool,
) -> Result<Vec<pb::ObjectRecord>, CheckpointBarrierFailure> {
    checkpoint_record
        .payloads
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
    journal: &mut remanence_state::TapeAuthorityLease,
    tape_uuid: TapeUuid,
    checkpoint_ordinal: &mut u64,
    tape_committed_object_count: &mut u64,
    batch: &PendingCheckpointBatch,
    parity_session: Option<&mut ParityActorSession>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    cfg: &WriteOwnerConfig,
) -> Result<BarrierOutcome, CheckpointBarrierFailure> {
    journal
        .with_structural_scope(|journal, structural| {
            perform_checkpoint_barrier_with_rows(
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
                structural,
            )
        })
        .map_err(|error| {
            CheckpointBarrierFailure::before_journal(crate::status_from_state_error(error))
        })?
}

#[allow(clippy::too_many_arguments)]
fn perform_checkpoint_barrier_with_rows(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    journal: &mut remanence_state::TapeAuthorityLease,
    tape_uuid: TapeUuid,
    checkpoint_ordinal: &mut u64,
    tape_committed_object_count: &mut u64,
    batch: &PendingCheckpointBatch,
    mut parity_session: Option<&mut ParityActorSession>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    cfg: &WriteOwnerConfig,
    structural: &mut Option<remanence_parity::authority::StructuralCapability>,
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
    let base = journal
        .authority_summary()
        .map_err(|e| CheckpointBarrierFailure::before_journal(crate::status_from_state_error(e)))?;
    let pending_state = parity_session.as_ref().and_then(|p| p.sink_state.as_ref());
    let check = pending_state.map(|s| s.commit_check());
    let draft_next_file = object_tape_file_bundles
        .last()
        .and_then(|b| b.files.last())
        .map(|r| r.tape_file_number)
        .or_else(|| objects.last().map(|o| o.copy.tape_file_number))
        .map(|file| {
            file.checked_add(1).ok_or_else(|| {
                CheckpointBarrierFailure::before_journal(Status::internal(
                    "next tape file overflows u64",
                ))
            })
        })
        .transpose()?
        .unwrap_or(base.next_file);
    let draft = remanence_state::authority::AuthorityCommit {
        session_id: journal.bound_session_id(),
        committed_at_utc: super::restore::now_rfc3339().map_err(|e| {
            CheckpointBarrierFailure::before_journal(Status::internal(e.to_string()))
        })?,
        ordinal: next_ordinal,
        object_count: next_committed_count,
        tape_uuid,
        operation_id: *batch.batch_id.as_bytes(),
        next_file: draft_next_file,
        block_size: selected.block_size,
        scheme: parity_session.as_ref().map(|p| p.scheme.clone()),
        protected: object_tape_file_bundles
            .last()
            .map_or(base.protected, |b| b.protected),
        total: object_tape_file_bundles
            .last()
            .map_or(base.total, |b| b.total),
        cursor: base.cursor.unwrap_or_default(),
        payloads: objects,
        object_files: object_tape_file_bundles,
        barrier: None,
        finalization: None,
        sealed: false,
        seal_due: None,
        check,
    };
    if let Err(error) = journal.stage_report(&draft) {
        if let Some(session) = parity_session.as_deref_mut() {
            if let Some(mut state) = session.sink_state.take() {
                state.poison_publication();
            }
            session.permit = None;
        }
        return Err(CheckpointBarrierFailure::before_journal(
            crate::status_from_state_error(error),
        ));
    }
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
        if parity_session.permit.is_none() {
            return Err(CheckpointBarrierFailure::before_journal(
                Status::resource_exhausted("active session has no parity accumulator reservation"),
            ));
        }
        let parity_journal = structural.as_mut().ok_or_else(|| {
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
                let closed = sink.checkpoint()?;
                let sink_state = sink.into_session_state()?;
                Ok((closed, sink_state))
            })()
        };
        let (closed, sink_state) = match barrier_result {
            Ok(result) => result,
            Err(err) => {
                let error = err.to_string();
                let status = status_from_parity_error(&err, error.clone());
                return Err(CheckpointBarrierFailure::before_journal(status));
            }
        };
        let parity_early_warning = sink_state.hardware_early_warning_seen();
        parity_session.sink_state = Some(sink_state);
        (
            closed.next_tape_file_number,
            selected.block_size,
            closed.barrier_outcome,
            closed.sidecar_publication().map_err(|e| {
                CheckpointBarrierFailure::before_journal(Status::internal(e.to_string()))
            })?,
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
        let last = draft.payloads.last().ok_or_else(|| {
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
    let mut record =
        remanence_state::authority::complete_commit(remanence_state::authority::AuthorityCommit {
            session_id: journal.bound_session_id(),
            committed_at_utc: super::restore::now_rfc3339().map_err(|e| {
                CheckpointBarrierFailure::before_journal(Status::internal(e.to_string()))
            })?,
            ordinal: next_ordinal,
            object_count: next_committed_count,
            tape_uuid,
            operation_id: *batch.batch_id.as_bytes(),
            next_file: next_tape_file_number,
            block_size: checkpoint_block_size,
            payloads: draft.payloads,
            scheme,
            object_files: draft.object_files,
            barrier: barrier_bundle,
            finalization: None,
            sealed: false,
            seal_due: None,
            cursor: remanence_parity::CommitCursor {
                partition: captured.partition,
                lba: captured.lba,
            },
            protected: 0,
            total: 0,
            check: None,
        });
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
    .map_err(|err| {
        CheckpointBarrierFailure::before_journal(status_from_selected_write_error(selected, err))
    })?;
    #[cfg(test)]
    let seal_reason = if matches!(
        cfg.io_memory.epoch_test_action.load(Ordering::Relaxed),
        7 | 11
    ) {
        Some(crate::pool_write::TapeSealReason::HardwareEarlyWarning)
    } else {
        seal_reason
    };
    record.check = parity_session
        .as_ref()
        .and_then(|session| session.sink_state.as_ref())
        .map(|state| state.commit_check());
    record.seal_due = base.seal_due.or_else(|| {
        seal_reason.map(|reason| match reason {
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
        })
    });
    let filemark_drain = drain_started.elapsed();
    let projection_started = Instant::now();
    #[cfg(test)]
    if cfg.io_memory.epoch_test_action.load(Ordering::Relaxed) == 3 {
        if let Some(session) = parity_session.as_deref_mut() {
            if let Some(mut state) = session.sink_state.take() {
                state.poison_publication();
            }
            session.permit = None;
        }
        return Err(CheckpointBarrierFailure::before_journal(Status::internal(
            "injected checkpoint journal append failure",
        )));
    }
    if let Err(err) = journal.complete_report(&record) {
        if let Some(session) = parity_session.as_deref_mut() {
            if let Some(mut state) = session.sink_state.take() {
                state.poison_publication();
            }
            session.permit = None;
        }
        if err.is_checkpoint_append_durable() {
            drive.advance_append_commit(captured).map_err(|error| {
                CheckpointBarrierFailure::after_journal(Status::unavailable(error.to_string()))
            })?;
        }
        return Err(if err.is_checkpoint_append_durable() {
            CheckpointBarrierFailure::after_journal(Status::internal(format!("checkpoint is durable in the tape log but folding or projection failed; retry after journal replay: {err}")))
        } else if err.is_checkpoint_append_authority_uncertain() {
            CheckpointBarrierFailure::uncertain_journal(Status::internal(format!(
                "checkpoint batch {} journal append failed with durable authority uncertain; close the session and retry only after journal reconciliation: {err}",
                batch.batch_id,
            )))
        } else {
            CheckpointBarrierFailure::before_journal(Status::internal(format!(
                "checkpoint batch {} tape-log publication failed; physical tail reconciliation is required before retrying {} WRITTEN objects: {err}",
                batch.batch_id,
                batch.objects.len(),
            )))
        });
    }
    drive.advance_append_commit(captured).map_err(|error| {
        CheckpointBarrierFailure::after_journal(Status::unavailable(error.to_string()))
    })?;
    if let Some(session) = parity_session.as_deref_mut() {
        if let Some(state) = session.sink_state.as_mut() {
            state.acknowledge_publication();
        }
    }
    // WOR's adapter-only crash cut: journal append has fsynced, SQLite has not projected.
    if let Some(fault) = cfg.mounted().and_then(|m| m.direct_replay_fault.as_ref()) {
        fault
            .abort_after_tape_log_append(
                &record,
                journal
                    .log_reference()
                    .map_err(|e| {
                        CheckpointBarrierFailure::after_journal(crate::status_from_state_error(e))
                    })?
                    .0,
            )
            .map_err(|error| {
                CheckpointBarrierFailure::after_journal(status_from_selected_write_error(
                    selected,
                    crate::PoolWriteError::InvalidInput(error),
                ))
            })?;
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
    let projection = journal
        .freeze()
        .and_then(|snapshot| index.project_tape_log(&snapshot));
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
    let sealed_after_write = record.seal_due.is_some();
    if let Some(trigger) = record.seal_due {
        if trigger == remanence_state::TerminalFinalizationTrigger::OperatorCloseOut {
            return Err(CheckpointBarrierFailure::after_projection(
                Status::internal(
                    "automatic checkpoint seal unexpectedly carried an operator trigger",
                ),
            ));
        }
        let spec = TerminalFinalizeSpec::automatic(selected, pool_cfg, trigger);
        super::terminal_finalize::decide_live_finalization(
            index,
            drive,
            journal,
            &record,
            &spec,
            selected,
            parity_session,
            structural.as_mut().map(|rows| &mut **rows),
        )
        .map_err(CheckpointBarrierFailure::after_projection)?;
    }
    let terminal_checkpoint_record = None;
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
    let original = status.clone();
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
    let outcome = match raise_tape_io_fence(
        index,
        cfg,
        remanence_state::TapeIoFenceInput {
            tape_uuid: selected.tape_uuid,
            barcode,
            reason: parity_raw_write_fence_reason(error).to_string(),
            evidence_json: Some(evidence),
        },
        selected.standalone_endpoint.as_ref(),
    ) {
        Ok(_) => (status, true),
        Err(err) => {
            tracing::error!(
                tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                phase,
                error = %err,
                "failed to persist parity raw-write tape-I/O fence"
            );
            (
                Status::internal(format!(
                    "{}; additionally failed to persist the required tape fence: {err}",
                    status.message()
                )),
                false,
            )
        }
    };
    (
        super::ActorFailure::preserve(&original, outcome.0),
        outcome.1,
    )
}

pub(super) fn arm_checkpoint_timer(
    timers: &mut super::timers::SessionTimers,
    actor_tx: mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    batch_id: Uuid,
    max_age: StdDuration,
) -> std::io::Result<()> {
    timers.arm(
        format!("rem-checkpoint-timer-{session_id}"),
        actor_tx,
        max_age,
        DriveCommand::Checkpoint {
            session_id,
            trigger: CheckpointTrigger::Timer,
            expected_batch_id: Some(batch_id),
            reply: None,
        },
    )
}

pub(super) fn arm_checkpoint_idle_close(
    timers: &mut super::timers::SessionTimers,
    actor_tx: mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    checkpoint_batch_id: Uuid,
    idle: StdDuration,
) -> std::io::Result<()> {
    timers.arm(
        format!("rem-checkpoint-idle-{session_id}"),
        actor_tx,
        idle,
        DriveCommand::TimerIdleClose {
            session_id,
            checkpoint_batch_id,
        },
    )
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
            Err(SessionSealed.into())
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

/// The ordinary closeout emitter, shared by clean session close and checked
/// read-back. Its permit belongs to this operation or the live session; it is
/// never carried to the next session. The caller proves the complete suffix
/// before passing the drive's append capability.
pub(super) fn closeout(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    lease: &mut remanence_state::TapeAuthorityLease,
    live: Option<ParityActorSession>,
    permit: Option<crate::io_memory::IoMemoryPermit>,
) -> Result<Option<crate::io_memory::IoMemoryPermit>, Status> {
    let snapshot = lease.freeze().map_err(crate::status_from_state_error)?;
    if !snapshot.tape_summary().closeout_due() {
        return Ok(live
            .and_then(|mut session| session.permit.take())
            .or(permit));
    }
    let progress =
        crate::closeout_progress::CloseoutProgress::new(Uuid::new_v4(), selected.tape_uuid, None)?;
    progress.bind(
        snapshot.tape_summary(),
        if live.is_some() { "live" } else { "rebuilt" },
    );
    progress.phase("proving");
    progress.audit(
        index,
        &cfg.audit_dir,
        &cfg.audit_append_lock,
        remanence_state::AuditEvent::OperationStarted,
        None,
    )?;
    let result = (|| {
        let previous = lease
            .latest_commit()
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::failed_precondition("closeout has no committed boundary"))?;
        let session = match live {
            Some(session) => session,
            None => {
                let scheme = previous
                    .scheme
                    .as_ref()
                    .ok_or_else(|| Status::internal("closeout has no parity scheme"))?;
                let permit = match permit {
                    Some(permit) => permit,
                    None => reserve_closeout_memory(
                        cfg,
                        scheme,
                        selected.block_size,
                        usize::try_from(
                            (previous.total - previous.protected).min(previous.next_file),
                        )
                        .unwrap_or(usize::MAX),
                        selected.tape_uuid,
                    )?,
                };
                progress.reserved(permit.bytes());
                progress.phase("rebuilding");
                let authority = validate_parity_actor_authority(cfg, selected, lease, &[])?;
                open_parity_actor_session(
                    index,
                    drive,
                    cfg,
                    selected,
                    &[],
                    authority,
                    lease,
                    Some(permit),
                )?
            }
        };
        let prepared = prepare_closeout(session, &previous)?;
        write_closeout(index, drive, cfg, selected, lease, prepared, &progress)
    })();
    if let Err(error) = &result {
        let _ = progress.audit(
            index,
            &cfg.audit_dir,
            &cfg.audit_append_lock,
            remanence_state::AuditEvent::OperationFailed,
            Some(error),
        );
    }
    result
}

/// Owns the single epoch reservation and its moved parity buffers while the
/// suffix is proved. No accumulator is copied or retained across sessions.
pub(super) struct PreparedCloseout {
    read_attempts: u32,
    pub(super) encoded: remanence_parity::EncodedSidecarTapeFile,
    permit: Option<crate::io_memory::IoMemoryPermit>,
}

pub(super) fn prepare_closeout(
    mut session: ParityActorSession,
    previous: &remanence_state::authority::AuthorityCommit,
) -> Result<PreparedCloseout, Status> {
    let state = session
        .sink_state
        .take()
        .ok_or_else(|| Status::failed_precondition("closeout accumulator unavailable"))?;
    let check = previous.check.map(|(value, _)| value);
    if !state.is_checkpointed()
        || Some(state.commit_check().0) != check
        || state.used_tape_blocks() != previous.cursor.lba
    {
        return Err(Status::failed_precondition(
            "closeout accumulator disagrees with committed range",
        ));
    }
    let encoded = state
        .into_closeout_sidecar()
        .map_err(|e| status_from_parity_error(&e, e.to_string()))?;
    Ok(PreparedCloseout {
        read_attempts: session.read_attempts,
        encoded,
        permit: session.permit.take(),
    })
}

pub(super) fn write_closeout(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    lease: &mut remanence_state::TapeAuthorityLease,
    prepared: PreparedCloseout,
    progress: &crate::closeout_progress::CloseoutProgress,
) -> Result<Option<crate::io_memory::IoMemoryPermit>, Status> {
    progress.attempt(prepared.read_attempts.max(1));
    progress.reserved(prepared.permit.as_ref().map_or(0, |p| p.bytes()));
    if !progress.watched() {
        progress.audit(
            index,
            &cfg.audit_dir,
            &cfg.audit_append_lock,
            remanence_state::AuditEvent::OperationStarted,
            None,
        )?;
    }
    let result = (|| {
        let snapshot = lease.freeze().map_err(crate::status_from_state_error)?;
        let previous = lease
            .latest_commit()
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::failed_precondition("closeout has no committed boundary"))?;
        let PreparedCloseout {
            encoded, permit, ..
        } = prepared;
        // Rebuild leaves the head in committed data. This restores C and proves its
        // predecessor again before the first sidecar block can be dispatched.
        drive
            .locate_admitted_append(previous.cursor.lba)
            .map_err(|e| Status::unavailable(format!("closeout cursor proof: {e}")))?;
        drive
            .check_append_admission()
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let expected_lba = previous
            .cursor
            .lba
            .checked_add(encoded.blocks.len() as u64)
            .and_then(|lba| lba.checked_add(1))
            .ok_or_else(|| Status::internal("closeout cursor overflow"))?;
        progress.phase("writing_sidecar");
        for block in &encoded.blocks {
            drive
                .check_append_admission()
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            let result = drive
                .write_block(block)
                .map_err(|e| Status::unavailable(format!("closeout write: {e}")))?;
            if result.bytes_written as usize != block.len() || result.end_of_medium {
                return Err(Status::unavailable(
                    "closeout write did not complete a whole block",
                ));
            }
        }
        drive
            .check_append_admission()
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        drive
            .write_filemarks_immediate(1)
            .map_err(|e| Status::unavailable(format!("closeout filemark: {e}")))?;
        progress.phase("barrier");
        drive
            .check_append_admission()
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        let barrier = drive
            .write_filemarks(0)
            .map_err(|e| Status::unavailable(format!("closeout barrier: {e}")))?;
        let position = drive
            .position()
            .map_err(|e| Status::unavailable(format!("closeout position: {e}")))?;
        if barrier.end_of_medium
            || position.partition != previous.cursor.partition
            || position.lba != expected_lba
        {
            return Err(Status::failed_precondition(
                "closeout barrier cursor mismatch",
            ));
        }
        let publication = remanence_parity::authority::FilePublication {
            purpose: remanence_parity::authority::FilePurpose::Sidecars(
                remanence_parity::authority::SidecarProvenance::Fresh,
            ),
            files: vec![remanence_parity::TapeFileEntry {
                tape_file_number: previous.next_file,
                kind: remanence_parity::TapeFileKind::ParitySidecar,
                block_count: encoded.blocks.len() as u64,
                physical_start_hint: Some(previous.cursor.lba),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: Some(encoded.header.epoch_id),
                protected_ordinal_start: Some(previous.protected),
                protected_ordinal_end_exclusive: Some(previous.total),
                canonical_metadata_hash: Some(encoded.header.canonical_metadata_hash),
                object_recovery_row: None,
            }],
            protected: previous.total,
            total: previous.total,
            cursor: Some(remanence_parity::CommitCursor {
                partition: position.partition,
                lba: position.lba,
            }),
            check: Some((
                remanence_parity::CheckValue::EMPTY,
                remanence_parity::CheckProvenance::Empty,
            )),
            seal_due: None,
        };
        let owner = snapshot
            .tape_summary()
            .open_session
            .as_ref()
            .map(|s| s.session_id)
            .or_else(|| {
                snapshot
                    .tape_summary()
                    .last_session_end
                    .as_ref()
                    .map(|s| s.session_id)
            });
        lease
            .publish_closed_out(
                *progress.id.as_bytes(),
                owner,
                &publication,
                super::restore::now_rfc3339().map_err(|e| Status::internal(e.to_string()))?,
            )
            .map_err(crate::status_from_state_error)?;
        let cursor = publication
            .cursor
            .ok_or_else(|| Status::internal("closeout lacks cursor"))?;
        drive
            .advance_append_commit(
                crate::append_admission::context(
                    selected.tape_uuid,
                    selected.block_size,
                    cursor.partition,
                    cursor.lba,
                )
                .boundary,
            )
            .map_err(|e| Status::unavailable(e.to_string()))?;
        progress.phase("projecting");
        lease
            .freeze()
            .and_then(|snapshot| index.project_tape_log(&snapshot))
            .map_err(crate::status_from_state_error)?;
        Ok(permit)
    })();
    if let Err(error) = &result {
        fence_ambiguous_authority(index, cfg, lease, selected.tape_uuid, "closed_out", error);
    }
    if !progress.watched() {
        let event = if result.is_ok() {
            remanence_state::AuditEvent::OperationFinished
        } else {
            remanence_state::AuditEvent::OperationFailed
        };
        if result.is_ok() {
            progress.phase("complete");
        }
        let audit = progress.audit(
            index,
            &cfg.audit_dir,
            &cfg.audit_append_lock,
            event,
            result.as_ref().err(),
        );
        if result.is_ok() {
            audit?;
        }
    }
    result
}

/// Surface an unknown native publication as a fence, without fencing ordinary
/// media failures whose live ending can still name its exact dispatched tail.
pub(super) fn fence_ambiguous_authority(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    lease: &remanence_state::TapeAuthorityLease,
    tape_uuid: TapeUuid,
    phase: &str,
    error: &Status,
) {
    if lease.ensure_abandonment_allowed().is_err() {
        record_closeout_fence(
            index,
            cfg,
            tape_uuid,
            "AuthorityAmbiguous",
            serde_json::json!({"phase": phase, "error": error.to_string()}).to_string(),
        );
    }
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
        AdmissionError::SessionSuffixContradiction {
            detail,
            partition,
            lba,
            sequence,
            incarnation,
            ..
        } => {
            serde_json::json!({"phase":"session_suffix","error":detail,"partition":partition,"lba":lba,"tape_log_sequence":sequence,"tape_incarnation":incarnation})
        }
        _ => return false,
    };
    if matches!(error, AdmissionError::Io(_)) {
        if let Some((w, t)) = range.filter(|(w, t)| w < t) {
            evidence["open_epoch_start"] = w.into();
            evidence["open_epoch_end"] = t.into();
        }
    }
    record_closeout_fence(
        index,
        cfg,
        tape_uuid,
        "SessionSuffixContradiction",
        evidence.to_string(),
    );
    true
}

/// Persist and audit a tape fence with the evidence supplied by its caller.
pub(crate) fn record_closeout_fence(
    index: &mut CatalogIndex,
    cfg: &impl super::terminal_types::TerminalConfig,
    tape_uuid: TapeUuid,
    reason: &str,
    evidence: String,
) {
    let barcode = match index.get_tape(&tape_uuid) {
        Ok(Some(tape)) => tape.voltag,
        other => {
            tracing::error!(
                ?other,
                "cannot resolve barcode for closeout fence; retain UUID identity"
            );
            None
        }
    };
    if index
        .tape_io_admission_conflicts(&tape_uuid, barcode.as_deref())
        .is_ok_and(|fences| !fences.is_empty())
    {
        return;
    }
    if let Err(error) = raise_tape_io_fence(
        index,
        cfg,
        remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode,
            reason: reason.into(),
            evidence_json: Some(evidence),
        },
        cfg.terminal_config().mounted_endpoint.as_ref(),
    ) {
        tracing::error!(%error, "open epoch fence persistence fails");
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
    if matches!(
        error,
        TapeIoError::FilemarkEncountered | TapeIoError::CursorPredecessorNotFilemark
    ) {
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
    fn prove_cursor(
        &mut self,
        cursor: PhysicalPositionHint,
        block_size: u32,
    ) -> Result<(), ParityError> {
        let result = self.inner.prove_cursor(cursor, block_size);
        self.contradiction |= result
            .as_ref()
            .is_err_and(|e| committed_read_contradiction(e, 1, 1));
        result
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

/// A durable seal decision ends append eligibility even while drive-owned
/// terminal work is still running after the checkpoint reply.
#[derive(Debug, thiserror::Error)]
#[error("SessionSealed: the session's tape has a durable finalization decision; reopen against the pool to roll placement")]
pub(crate) struct SessionSealed;
impl From<SessionSealed> for Status {
    fn from(error: SessionSealed) -> Self {
        Status::failed_precondition(error.to_string())
    }
}
