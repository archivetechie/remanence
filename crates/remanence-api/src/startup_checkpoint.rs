//! Startup path derivation, calibration loading, and checkpoint-journal replay.

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
    index_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("checkpoints")
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
    for path in remanence_state::list_checkpoint_journals(checkpoint_dir)
        .map_err(status_from_state_error)?
    {
        let tape_uuid = remanence_state::tape_uuid_from_checkpoint_path(path.as_path())
            .map_err(status_from_state_error)?;
        let journal = remanence_state::FileCheckpointJournal::open(checkpoint_dir, tape_uuid)
            .map_err(status_from_state_error)?;
        let pending_intent = journal
            .terminal_finalization_intent()
            .map_err(status_from_state_error)?;
        let mut sealed_completion = None;
        let pending_intent = if pending_intent.is_some() {
            let mut lease = journal
                .acquire_exclusive_for_terminal_recovery()
                .map_err(status_from_state_error)?;
            let retained_intent = if let Some(intent) = lease
                .terminal_finalization_intent()
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
            lease
                .for_each_record_bounded(|record| {
                    index.project_checkpoint_record(record)?;
                    if record.sealed_after_write {
                        sealed_completion = record.terminal_finalization.clone();
                    }
                    Ok(())
                })
                .map_err(status_from_state_error)?;
            if retained_intent {
                let authority = lease
                    .replay_for_terminal_recovery()
                    .map_err(status_from_state_error)?;
                authority.finalization_intent
            } else {
                None
            }
        } else {
            for record in journal.replay().map_err(status_from_state_error)? {
                index
                    .project_checkpoint_record(&record)
                    .map_err(status_from_state_error)?;
                if record.sealed_after_write {
                    sealed_completion = record.terminal_finalization.clone();
                }
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

/// Reconcile every configured checkpoint journal into the locked local state.
///
/// One-shot writers call this before catalog selection and again at the
/// drive-bound admission boundary. The global replay closes the crash window
/// in which a checkpoint became durable on tape A but SQLite had not yet made
/// its Object identities visible before a retry considered tape B.
pub fn reconcile_checkpoint_journal_projections(state: &mut StateHandle) -> Result<(), Status> {
    let checkpoint_dir = state.paths().journal_dir.join("checkpoints");
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
