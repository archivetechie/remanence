//! Startup path derivation, calibration loading, and checkpoint-journal replay.

use remanence_state::authority::TapeAuthorityWriter as _;
use remanence_state::authority::{TapeAuthoritySnapshot, TapeAuthorityView};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use remanence_state::{CalibrationControlStore, CatalogIndex, RemConfig, StateHandle};
use tonic::Status;
use uuid::Uuid;

use crate::audit_projection::{ensure_manual_finalize_finished_audit, ensure_tape_sealed_audit};
use crate::drive_collection::parse_simple_duration;
use crate::startup_media_readiness::status_from_state_error;

pub(crate) fn default_audit_dir_for_index(index_path: &Path) -> PathBuf {
    let Some(parent) = index_path.parent() else {
        return PathBuf::from("audit");
    };
    if parent.file_name().and_then(|name| name.to_str()) == Some("index") {
        return parent
            .parent()
            .map(|state_dir| state_dir.join("audit"))
            .unwrap_or_else(|| parent.join("audit"));
    }
    parent.join("audit")
}

pub(crate) fn default_checkpoint_journal_dir_for_index(index_path: &Path) -> PathBuf {
    let path = index_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("tapes");
    // This constructor is for index-only fixtures. Configured production owners
    // provision their journal namespace through the authority-layout gate.
    #[cfg(test)]
    std::fs::create_dir_all(&path).expect("provision fixture tape-log namespace");
    path
}

/// Derive the calibration-control directory for index-only (test)
/// constructors, mirroring [`default_audit_dir_for_index`]: when the
/// index sits in the conventional `state_dir/index/`, the store lives
/// at `state_dir/calibration/` — the same directory the config-driven
/// path derives.
pub(crate) fn default_calibration_dir_for_index(index_path: &Path) -> PathBuf {
    let Some(parent) = index_path.parent() else {
        return PathBuf::from("calibration");
    };
    if parent.file_name().and_then(|name| name.to_str()) == Some("index") {
        return parent
            .parent()
            .map(|state_dir| state_dir.join("calibration"))
            .unwrap_or_else(|| parent.join("calibration"));
    }
    parent.join("calibration")
}

/// Open the durable calibration-control store where `StatePaths`
/// places it — one derivation rule, owned by `remanence-state`, not
/// re-derived here.
pub(crate) fn open_calibration_store_for_config(
    config: &RemConfig,
) -> Result<CalibrationControlStore, Status> {
    let calibration_dir =
        remanence_state::StatePaths::from_config(Path::new(""), config).calibration_dir;
    CalibrationControlStore::open(calibration_dir)
        .map_err(|err| Status::internal(format!("open calibration-control store: {err}")))
}

#[cfg(test)]
pub(crate) fn replay_checkpoint_journal_projections(
    index: &mut CatalogIndex,
    checkpoint_dir: &Path,
) -> Result<(), Status> {
    let audit_dir = default_audit_dir_for_index(index.path());
    let audit_append_lock = Arc::new(std::sync::Mutex::new(()));
    replay_checkpoint_journal_projections_with_audit(
        index,
        checkpoint_dir,
        audit_dir.as_path(),
        &audit_append_lock,
    )
}

pub(crate) fn replay_checkpoint_journal_projections_with_audit(
    index: &mut CatalogIndex,
    checkpoint_dir: &Path,
    audit_dir: &Path,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
) -> Result<(), Status> {
    let mut identities = index
        .projected_tape_uuids()
        .map_err(status_from_state_error)?;
    for path in remanence_state::TapeAuthority::list_entries(checkpoint_dir)
        .map_err(status_from_state_error)?
    {
        identities.push(
            remanence_state::TapeAuthority::entry_tape_uuid(&path)
                .map_err(status_from_state_error)?,
        );
    }
    identities.sort();
    identities.dedup();
    for tape_uuid in identities {
        // A retired tape's log may be archived (for example by a recycle); its
        // leftover mark is not evidence of a lost log, so it is neither
        // fenced nor projected.
        if index
            .get_tape(&tape_uuid)
            .map_err(status_from_state_error)?
            .is_some_and(|tape| tape.state == "retired")
        {
            tracing::debug!(tape_uuid = %Uuid::from_bytes(tape_uuid), "retired tape skipped by startup tape-log reconciliation");
            continue;
        }
        // A fenced tape is touched only when every active fence is a
        // below-projection fence and its log reaches the recorded mark; the
        // fences are released only after it then projects successfully.
        let restoring = if index
            .tape_io_is_fenced(Some(&tape_uuid), None, None)
            .map_err(status_from_state_error)?
        {
            match restorable_projection_fences(index, checkpoint_dir, tape_uuid)? {
                Some(restoring) => Some(restoring),
                None => continue,
            }
        } else {
            None
        };
        let journal = remanence_state::TapeAuthority::open(checkpoint_dir, tape_uuid, index.path())
            .map_err(status_from_state_error)?;
        let mark = index
            .projected_through_sequence(tape_uuid)
            .map_err(status_from_state_error)?;
        let projected = (|| -> Result<_, remanence_state::StateError> {
            journal.reconcile_restart(mark)?;
            let snapshot = journal.freeze()?;
            index.project_tape_log(&snapshot)?;
            Ok(snapshot)
        })();
        if let Err(error) = projected {
            let classified = match &error {
                remanence_state::StateError::TapeLog(e) if e.is_below_projection() => {
                    Some(remanence_state::fence::TAPE_LOG_BELOW_PROJECTION)
                }
                remanence_state::StateError::TapeLog(e) if e.is_invalid_projection_mark() => {
                    Some(remanence_state::fence::TAPE_LOG_PROJECTION_MARK_INVALID)
                }
                _ => None,
            };
            if restoring.is_some() {
                // A per-tape tape-log condition keeps its fence and startup goes
                // on; anything else (a catalog fault, a conflicting re-projection)
                // stops startup exactly as it would for an unfenced tape.
                if classified.is_none() {
                    return Err(status_from_state_error(error));
                }
                tracing::warn!(tape_uuid = %Uuid::from_bytes(tape_uuid), %error, "restored tape log failed to project; tape stays fenced");
                if classified == Some(remanence_state::fence::TAPE_LOG_PROJECTION_MARK_INVALID) {
                    raise_startup_fence(
                        index,
                        audit_dir,
                        audit_append_lock,
                        tape_uuid,
                        mark,
                        &error,
                        remanence_state::fence::TAPE_LOG_PROJECTION_MARK_INVALID,
                    )?;
                }
                continue;
            }
            let Some(reason) = classified else {
                return Err(status_from_state_error(error));
            };
            raise_startup_fence(
                index,
                audit_dir,
                audit_append_lock,
                tape_uuid,
                mark,
                &error,
                reason,
            )?;
            tracing::error!(tape_uuid = %Uuid::from_bytes(tape_uuid), reason, %error, "tape fenced at startup; startup continues for other tapes");
            continue;
        }
        if let Some(restoring) = restoring {
            if !release_restored_projection_fences(
                index,
                audit_dir,
                audit_append_lock,
                tape_uuid,
                restoring,
            )? {
                continue;
            }
        }
        let pending_intent = journal
            .pending_finalization()
            .map_err(status_from_state_error)?;
        let mut sealed_completion = None;
        let pending_intent = if pending_intent.is_some() {
            let mut lease = journal
                .acquire_exclusive_for_terminal_recovery()
                .map_err(status_from_state_error)?;
            let retained_intent = if let Some(intent) = lease
                .pending_finalization()
                .map_err(status_from_state_error)?
                .filter(|intent| intent.manual.is_some())
            {
                crate::write_owner::reconcile_manual_terminal_acceptance(
                    index, &mut lease, &intent,
                )?
            } else {
                true
            };
            // Project every validated record while the exact companion is
            // still retained. A catalog failure therefore leaves startup
            // routing authority intact; exact replay retires a stale sealed
            // companion only after all projections succeed.
            let snapshot = lease.freeze().map_err(status_from_state_error)?;
            index
                .project_tape_log(&snapshot)
                .map_err(status_from_state_error)?;
            if snapshot.summary().sealed {
                sealed_completion = snapshot.finalization().cloned();
            }
            if retained_intent {
                let authority = lease
                    .recover_finalization()
                    .map_err(status_from_state_error)?;
                authority.finalization
            } else {
                None
            }
        } else {
            let snapshot = journal.freeze().map_err(status_from_state_error)?;
            index
                .project_tape_log(&snapshot)
                .map_err(status_from_state_error)?;
            if snapshot.summary().sealed {
                sealed_completion = snapshot.finalization().cloned();
            }
            None
        };
        if let Some(intent) = pending_intent {
            let operation_id = intent
                .manual
                .as_ref()
                .map(|manual| Uuid::from_bytes(manual.operation_id));
            index
                .project_terminal_finalization(
                    remanence_state::TerminalFinalizationProjectionInput {
                        tape_uuid,
                        trigger: intent.trigger,
                        operation_id,
                        progress: intent.progress,
                        edition_digest: intent.edition_digest,
                        layout_digest: intent.layout.layout_digest,
                        outcome: if intent.recovery_required {
                            remanence_state::TerminalFinalizationOutcome::RecoveryRequired
                        } else {
                            remanence_state::TerminalFinalizationOutcome::InProgress
                        },
                        updated_at_utc: None,
                    },
                )
                .map_err(status_from_state_error)?;
        }
        if let Some(completion) = sealed_completion.as_ref() {
            ensure_tape_sealed_audit(index, audit_dir, audit_append_lock, tape_uuid)?;
            ensure_manual_finalize_finished_audit(
                index,
                audit_dir,
                audit_append_lock,
                tape_uuid,
                completion,
            )?;
        }
    }
    Ok(())
}

/// Raise one audited startup fence. Evidence records the catalog mark at the
/// time, so a later automatic release cannot trust a lost or rebuilt mark.
fn raise_startup_fence(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    tape_uuid: [u8; 16],
    mark: u64,
    error: &remanence_state::StateError,
    reason: &str,
) -> Result<(), Status> {
    let barcode = index
        .get_tape(&tape_uuid)
        .map_err(status_from_state_error)?
        .and_then(|t| t.voltag);
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("audit append lock poisoned"))?;
    remanence_state::fence::raise(
        index,
        audit_dir,
        remanence_state::TapeIoFenceInput {
            tape_uuid,
            barcode,
            reason: reason.into(),
            evidence_json: Some(
                serde_json::json!({
                    "error": error.to_string(),
                    "projected_through_sequence": mark.to_string(),
                })
                .to_string(),
            ),
        },
        None,
        remanence_state::AuditActor::System,
    )
    .map_err(status_from_state_error)?;
    Ok(())
}

/// Below-projection fences that may end once the tape's log projects.
struct RestorableFences {
    fences: Vec<remanence_state::TapeIoFenceRecord>,
    sequence: u64,
    required: u64,
}

/// Read-only check of a fenced tape. Only below-projection fences ever release
/// themselves, and only when the tape has no other active fence, every such
/// fence recorded the mark it was raised at, and a log file exists and
/// validates through a sequence at or above the greater of that recorded mark
/// and the current catalog mark. A missing log never qualifies, even when the
/// catalog has lost or rebuilt its mark. The fenced log is neither reconciled
/// nor projected here, and its ambiguity marker, if any, is not cleared.
fn restorable_projection_fences(
    index: &mut CatalogIndex,
    checkpoint_dir: &Path,
    tape_uuid: [u8; 16],
) -> Result<Option<RestorableFences>, Status> {
    let fences: Vec<_> = index
        .list_active_tape_io_fences()
        .map_err(status_from_state_error)?
        .into_iter()
        .filter(|fence| fence.tape_uuid.as_slice() == tape_uuid)
        .collect();
    if fences.is_empty()
        || fences
            .iter()
            .any(|fence| fence.reason != remanence_state::fence::TAPE_LOG_BELOW_PROJECTION)
    {
        return Ok(None);
    }
    let mut required = index
        .projected_through_sequence(tape_uuid)
        .map_err(status_from_state_error)?;
    for fence in &fences {
        let recorded = fence
            .evidence_json
            .as_deref()
            .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
            .and_then(|evidence| {
                evidence["projected_through_sequence"]
                    .as_str()
                    .and_then(|mark| mark.parse::<u64>().ok())
            });
        let Some(recorded) = recorded else {
            tracing::warn!(quarantine_id = %fence.quarantine_id, tape_uuid = %Uuid::from_bytes(tape_uuid), "fence evidence records no projection mark; tape stays fenced");
            return Ok(None);
        };
        required = required.max(recorded);
    }
    let present = remanence_state::TapeAuthority::exists(checkpoint_dir, tape_uuid)
        .map_err(status_from_state_error)?;
    let restored = if present {
        remanence_state::TapeAuthority::open(checkpoint_dir, tape_uuid, index.path())
            .and_then(|journal| journal.freeze())
            .map(|snapshot| snapshot.log_reference().0)
    } else {
        Err(remanence_state::StateError::TapeLog(Box::new(
            remanence_state::tape_log::store::StoreError::Refused(
                remanence_state::tape_log::store::BELOW_PROJECTION_MARK,
            ),
        )))
    };
    match restored {
        Ok(sequence) if sequence >= required => Ok(Some(RestorableFences {
            fences,
            sequence,
            required,
        })),
        Ok(sequence) => {
            tracing::warn!(tape_uuid = %Uuid::from_bytes(tape_uuid), sequence, projected_through_sequence = required, "tape log is still below its projection mark; tape stays fenced");
            Ok(None)
        }
        Err(error) => {
            tracing::warn!(tape_uuid = %Uuid::from_bytes(tape_uuid), %error, "tape log does not validate; tape stays fenced");
            Ok(None)
        }
    }
}

/// Release below-projection fences with audit evidence once the restored log
/// has projected. Returns whether the tape is now unfenced.
fn release_restored_projection_fences(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    tape_uuid: [u8; 16],
    restoring: RestorableFences,
) -> Result<bool, Status> {
    let RestorableFences {
        fences,
        sequence,
        required,
    } = restoring;
    let evidence = serde_json::json!({
        "tape_log_sequence": sequence.to_string(),
        "projected_through_sequence": required.to_string(),
    })
    .to_string();
    {
        let _guard = audit_append_lock
            .lock()
            .map_err(|_| Status::internal("audit append lock poisoned"))?;
        for fence in fences {
            remanence_state::fence::release_restored_projection(
                index,
                audit_dir,
                &fence.quarantine_id,
                evidence.clone(),
            )
            .map_err(status_from_state_error)?;
            tracing::info!(quarantine_id = %fence.quarantine_id, tape_uuid = %Uuid::from_bytes(tape_uuid), sequence, projected_through_sequence = required, "tape log restored at or above its projection mark; fence released");
        }
    }
    index
        .tape_io_is_fenced(Some(&tape_uuid), None, None)
        .map(|fenced| !fenced)
        .map_err(status_from_state_error)
}

#[cfg(test)]
thread_local! {
    pub(crate) static RECONCILIATION_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reconcile durable checkpoint authority under the exclusive state lock.
/// Standalone commands reconcile once; daemon entry points retain their own guard.
pub fn reconcile_checkpoint_journal_projections(state: &mut StateHandle) -> Result<(), Status> {
    #[cfg(test)]
    RECONCILIATION_COUNT.with(|count| count.set(count.get() + 1));
    let checkpoint_dir = state.paths().journal_dir.join("tapes");
    let audit_dir = state.paths().audit_dir.clone();
    let audit_append_lock = Arc::new(std::sync::Mutex::new(()));
    let replay = replay_checkpoint_journal_projections_with_audit(
        state.catalog_index(),
        checkpoint_dir.as_path(),
        audit_dir.as_path(),
        &audit_append_lock,
    );
    let refresh = state.refresh_audit_append_cursor();
    match (replay, refresh) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(refresh)) => Err(Status::internal(format!(
            "refresh audit append cursor after checkpoint reconciliation: {refresh}"
        ))),
        (Err(primary), Err(refresh)) => Err(Status::new(
            primary.code(),
            format!(
                "{}; secondary audit-cursor refresh failure: {refresh}",
                primary.message()
            ),
        )),
    }
}

pub(crate) fn live_status_config_from(config: &remanence_state::LiveStatusConfig) -> Duration {
    parse_simple_duration(config.min_poll_interval.as_str())
        .unwrap_or_else(|| Duration::from_millis(250))
}
