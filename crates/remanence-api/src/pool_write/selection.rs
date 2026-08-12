//! Pool tape selection and pinned-tape admission.

use std::collections::HashSet;
use std::path::Path;

use remanence_state::{CatalogIndex, StateError, TapePoolConfig, TapeRecord};
use thiserror::Error;
use uuid::Uuid;

use crate::pool_selection::{
    CompleteOrFill, FillOldest, PoolSelectionContext, PoolSelectionPolicy, Selection,
};

use super::capacity::{
    compare_tapes_for_pool_selection, selected_tape_from_record, tape_fit_state_from_record,
    tape_uuid_from_vec, validate_pool_capacity_invariant_for_tapes,
};
use super::media::{check_pool_block_size_precondition, check_writability_preconditions};
use super::model::{
    PinnedWriteDisposition, SelectTapeError, SelectedTape, TapeUuid, WritabilityError,
};

/// Select a tape for pool-targeted writes using the configured default policy.
pub fn select_tape_in_pool(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
) -> Result<SelectedTape, SelectTapeError> {
    match pool_cfg.selection_policy {
        remanence_state::PoolSelectionPolicyName::CompleteOrFill => {
            select_tape_in_pool_with_policy(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &CompleteOrFill,
            )
        }
        remanence_state::PoolSelectionPolicyName::FillOldest => select_tape_in_pool_with_policy(
            state,
            pool_cfg,
            object_size,
            reserved_tape_uuids,
            &FillOldest,
        ),
    }
}

/// Select a tape for a write session under the sole checkpointed write mode.
/// Select a tape that is either fresh or carries a durable checkpoint
/// journal, as required by the single checkpointed write mode.
pub fn select_tape_in_pool_for_write_session(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    checkpoint_journal_dir: &Path,
) -> Result<SelectedTape, SelectTapeError> {
    match pool_cfg.selection_policy {
        remanence_state::PoolSelectionPolicyName::CompleteOrFill => {
            select_tape_in_pool_with_policy_and_batched_eligibility(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &CompleteOrFill,
                checkpoint_journal_dir,
                None,
            )
        }
        remanence_state::PoolSelectionPolicyName::FillOldest => {
            select_tape_in_pool_with_policy_and_batched_eligibility(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &FillOldest,
                checkpoint_journal_dir,
                None,
            )
        }
    }
}

/// Select only from tapes whose current physical location is admitted by the
/// caller. Library inventory remains outside the pure pool policy.
pub(crate) fn select_tape_in_pool_for_write_session_scoped(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    checkpoint_journal_dir: &Path,
    allowed_tape_uuids: &HashSet<TapeUuid>,
) -> Result<SelectedTape, SelectTapeError> {
    match pool_cfg.selection_policy {
        remanence_state::PoolSelectionPolicyName::CompleteOrFill => {
            select_tape_in_pool_with_policy_and_batched_eligibility(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &CompleteOrFill,
                checkpoint_journal_dir,
                Some(allowed_tape_uuids),
            )
        }
        remanence_state::PoolSelectionPolicyName::FillOldest => {
            select_tape_in_pool_with_policy_and_batched_eligibility(
                state,
                pool_cfg,
                object_size,
                reserved_tape_uuids,
                &FillOldest,
                checkpoint_journal_dir,
                Some(allowed_tape_uuids),
            )
        }
    }
}

/// Why an operator-pinned tape cannot open a write session.
///
/// Pinning replaces pool *selection*, never *admission*: every check that
/// makes a tape a valid pool-mode candidate still gates here, plus the
/// mandatory pool guard. Pools carry copy-class segregation, so a silent
/// cross-pool write is policy corruption — a guard mismatch is a refusal
/// that names both pools, never a warning.
#[derive(Debug, Error)]
pub enum PinnedTapeError {
    /// No catalog row exists for the pinned UUID. An uninitialized cartridge
    /// has no tape UUID at all (identity is minted by tape init), so this
    /// also covers "that tape was never initialized".
    #[error("tape {tape_uuid} is not in the catalog; an uninitialized cartridge has no tape UUID — run `rem tape init` first")]
    UnknownTape {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
    },
    /// The pinned tape is not a data tape (e.g. a cleaning cartridge).
    #[error("tape {tape_uuid} is a {kind} tape, not a data tape")]
    NotADataTape {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// Catalog tape kind.
        kind: String,
    },
    /// The tape's catalog pool assignment does not match the caller's guard.
    #[error("tape {tape_uuid} is assigned to pool {}, not the required pool {required_pool_id}; pools carry copy-class segregation, so the guard must name the tape's actual pool", actual_pool_id.as_deref().unwrap_or("(none)"))]
    PoolGuardMismatch {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// Pool the caller claimed the tape belongs to.
        required_pool_id: String,
        /// Pool the catalog actually assigns the tape to, if any.
        actual_pool_id: Option<String>,
    },
    /// The tape fails the same hard writability preconditions pool-mode
    /// candidates must pass (lifecycle state, geometry, capacity, parity
    /// append rules, pool block size).
    #[error("tape {tape_uuid} is not writable: {reason}")]
    NotWritable {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// The failed precondition.
        reason: WritabilityError,
    },
    /// A media-readiness quarantine or io fence blocks this tape.
    #[error("tape {tape_uuid} is fenced by quarantine {quarantine_id}: {reason}")]
    Fenced {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
        /// Owning quarantine id.
        quarantine_id: String,
        /// Fence reason.
        reason: String,
    },
    /// The tape carries committed data but no adopted checkpoint journal, so
    /// batched append positioning would be unsafe — the same rule pool-mode
    /// candidates must pass.
    #[error("tape {tape_uuid} carries committed data but no adopted checkpoint journal; batched append positioning would be unsafe")]
    NotBatchEligible {
        /// The pinned tape UUID.
        tape_uuid: Uuid,
    },
    /// UUID/geometry projection failure (shared with pool selection).
    #[error(transparent)]
    Select(#[from] SelectTapeError),
    /// Catalog access failure.
    #[error(transparent)]
    State(#[from] remanence_state::StateError),
}

pub(super) fn tape_is_fresh_for_checkpoint_admission(
    state: &CatalogIndex,
    tape: &TapeRecord,
    tape_uuid: &TapeUuid,
) -> Result<bool, StateError> {
    if tape.total_committed_ordinals != 0 {
        return Ok(false);
    }
    match tape.last_committed_tape_file {
        None => Ok(state.list_tape_files(tape_uuid)?.is_empty()),
        Some(0) if tape.scheme_id.is_some() => {
            let files = state.list_tape_files(tape_uuid)?;
            Ok(matches!(
                files.as_slice(),
                [file]
                    if file.tape_file_number == 0
                        && file.kind == "bootstrap"
                        && file.block_count == 1
                        && file.object_id.is_none()
            ))
        }
        _ => Ok(false),
    }
}

/// Admit one operator-pinned tape for a write session.
///
/// `pool_cfg` is the configuration of `required_pool_id`, resolved by the
/// caller; guard-shape validation (empty guard, allow_unpooled semantics)
/// happens before config resolution and therefore also caller-side.
pub fn admit_pinned_tape_for_write_session(
    state: &CatalogIndex,
    tape_uuid: TapeUuid,
    required_pool_id: &str,
    pool_cfg: &TapePoolConfig,
    checkpoint_journal_dir: &Path,
) -> Result<PinnedWriteDisposition, PinnedTapeError> {
    let uuid_text = Uuid::from_bytes(tape_uuid);
    let tape = state
        .get_tape(&tape_uuid)?
        .ok_or(PinnedTapeError::UnknownTape {
            tape_uuid: uuid_text,
        })?;
    if tape.kind != "data" {
        return Err(PinnedTapeError::NotADataTape {
            tape_uuid: uuid_text,
            kind: tape.kind.clone(),
        });
    }
    let actual_pool = tape
        .pool_id
        .as_deref()
        .map(str::trim)
        .filter(|pool| !pool.is_empty());
    if actual_pool != Some(required_pool_id) {
        return Err(PinnedTapeError::PoolGuardMismatch {
            tape_uuid: uuid_text,
            required_pool_id: required_pool_id.to_string(),
            actual_pool_id: actual_pool.map(str::to_string),
        });
    }
    let checkpoint_journal_tapes = checkpoint_journal_tape_uuids(checkpoint_journal_dir)?;
    let host_only_after_replica_c = if checkpoint_journal_tapes.contains(&tape_uuid) {
        let journal =
            remanence_state::FileCheckpointJournal::open(checkpoint_journal_dir, tape_uuid)?;
        if let Some(intent) = journal.terminal_finalization_intent()? {
            intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC
                && intent.manual.is_none()
        } else {
            let checkpoint = journal.acquire_exclusive()?;
            checkpoint.last_record_bounded()?.is_some_and(|record| {
                record.sealed_after_write
                    && record.terminal_finalization.is_some_and(|completion| {
                        completion.progress
                            == remanence_state::TerminalFinalizationProgress::AfterReplicaC
                            && completion.manual.is_none()
                    })
            })
        }
    } else {
        false
    };
    match check_writability_preconditions(&tape, 0)
        .and_then(|_| check_pool_block_size_precondition(&tape, pool_cfg))
    {
        Ok(()) => {}
        Err(_) if host_only_after_replica_c => {
            // This admission exists only so mount preflight can finish the
            // sealed host suffix and reject the requested Object session.
            // No write capability is resolved on that path.
        }
        Err(WritabilityError::ParityAppendUnsupported { .. }) => {
            // The gate below requires a durable checkpoint record. Session
            // open compares that record with the sink journal before LOCATE.
        }
        Err(reason) => {
            return Err(PinnedTapeError::NotWritable {
                tape_uuid: uuid_text,
                reason,
            });
        }
    }
    if !host_only_after_replica_c {
        let conflicts = state.tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())?;
        if let Some(conflict) = conflicts.first() {
            return Err(PinnedTapeError::Fenced {
                tape_uuid: uuid_text,
                quarantine_id: conflict.quarantine_id.clone(),
                reason: conflict.reason.clone(),
            });
        }
    }
    let fresh = tape_is_fresh_for_checkpoint_admission(state, &tape, &tape_uuid)?;
    if !fresh
        && !host_only_after_replica_c
        && !tape_carries_checkpoint(checkpoint_journal_dir, &checkpoint_journal_tapes, tape_uuid)?
    {
        return Err(PinnedTapeError::NotBatchEligible {
            tape_uuid: uuid_text,
        });
    }
    let selected = selected_tape_from_record(tape, required_pool_id)?;
    if host_only_after_replica_c {
        Ok(PinnedWriteDisposition::HostOnlyTerminalRecovery(selected))
    } else {
        Ok(PinnedWriteDisposition::Writable(selected))
    }
}

/// Select an eligible tape from a pool using a caller-supplied pure policy.
///
/// This is the narrow integration adapter for the current non-hardware path:
/// catalog rows are projected into [`TapeFitState`] values and the policy
/// remains free of catalog/session/hardware access. Live-session reservations
/// are caller-projected and filtered out before the policy ranks candidates.
pub fn select_tape_in_pool_with_policy(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    policy: &dyn PoolSelectionPolicy,
) -> Result<SelectedTape, SelectTapeError> {
    select_tape_in_pool_with_policy_and_eligibility(
        state,
        pool_cfg,
        object_size,
        reserved_tape_uuids,
        policy,
        None,
        None,
    )
}

pub(super) fn select_tape_in_pool_with_policy_and_batched_eligibility(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    policy: &dyn PoolSelectionPolicy,
    checkpoint_journal_dir: &Path,
    allowed_tape_uuids: Option<&HashSet<TapeUuid>>,
) -> Result<SelectedTape, SelectTapeError> {
    select_tape_in_pool_with_policy_and_eligibility(
        state,
        pool_cfg,
        object_size,
        reserved_tape_uuids,
        policy,
        Some(checkpoint_journal_dir),
        allowed_tape_uuids,
    )
}

pub(super) fn select_tape_in_pool_with_policy_and_eligibility(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    reserved_tape_uuids: &HashSet<TapeUuid>,
    policy: &dyn PoolSelectionPolicy,
    checkpoint_journal_dir: Option<&Path>,
    allowed_tape_uuids: Option<&HashSet<TapeUuid>>,
) -> Result<SelectedTape, SelectTapeError> {
    let requested_pool_id = pool_cfg.id.trim();
    let pool =
        state
            .get_tape_pool(requested_pool_id)?
            .ok_or_else(|| SelectTapeError::UnknownPool {
                pool_id: requested_pool_id.to_string(),
            })?;
    let pool_id = pool.pool_id;

    let tapes = state.list_tapes(
        Some(pool_id.as_str()),
        remanence_state::TapeKindFilter::Data,
    )?;
    if tapes.is_empty() {
        return Err(SelectTapeError::EmptyPool { pool_id });
    }
    validate_pool_capacity_invariant_for_tapes(pool_cfg, &tapes)?;
    let checkpoint_journal_tapes = checkpoint_journal_dir
        .map(checkpoint_journal_tape_uuids)
        .transpose()?;

    // 2a-2 owns the hard writability precondition (state/geometry/capacity fit);
    // the policy ranks only the tapes that pass it (design §6 boundary).
    let mut reasons = Vec::new();
    let mut eligible = Vec::new();
    let mut batched_ineligible = Vec::new();
    for tape in tapes {
        match check_writability_preconditions(&tape, object_size)
            .and_then(|_| check_pool_block_size_precondition(&tape, pool_cfg))
        {
            Ok(()) => {}
            Err(WritabilityError::ParityAppendUnsupported { .. })
                if checkpoint_journal_dir.is_some() =>
            {
                // The checkpoint-specific eligibility gate below requires a
                // durable record before admission. Session open proves that
                // it agrees with the sink journal before LOCATE.
            }
            Err(err) => {
                reasons.push(err);
                continue;
            }
        }
        let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id.as_str())?;
        if allowed_tape_uuids.is_some_and(|allowed| !allowed.contains(&tape_uuid)) {
            continue;
        }
        let conflicts = state
            .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
            .map_err(SelectTapeError::State)?;
        if let Some(conflict) = conflicts.first() {
            reasons.push(WritabilityError::TapeIoFence {
                quarantine_id: conflict.quarantine_id.clone(),
                reason: conflict.reason.clone(),
            });
            continue;
        }
        if let (Some(checkpoint_journal_dir), Some(checkpoint_journal_tapes)) =
            (checkpoint_journal_dir, checkpoint_journal_tapes.as_ref())
        {
            let fresh = tape_is_fresh_for_checkpoint_admission(state, &tape, &tape_uuid)?;
            let carries_checkpoint = fresh
                || tape_carries_checkpoint(
                    checkpoint_journal_dir,
                    checkpoint_journal_tapes,
                    tape_uuid,
                )?;
            if !carries_checkpoint {
                batched_ineligible.push(format!(
                    "{} ({})",
                    tape.voltag.as_deref().unwrap_or("<no-voltag>"),
                    Uuid::from_bytes(tape_uuid)
                ));
                continue;
            }
        }
        eligible.push(tape);
    }
    if eligible.is_empty() {
        if !batched_ineligible.is_empty() {
            return Err(SelectTapeError::NoBatchedEligibleTapes {
                pool_id,
                ineligible_candidates: batched_ineligible,
            });
        }
        return Err(SelectTapeError::NoWritableTapes { pool_id, reasons });
    }
    eligible.sort_by(compare_tapes_for_pool_selection);

    let mut ranked = Vec::with_capacity(eligible.len());
    let mut reserved_tape_count = 0usize;
    for (index, tape) in eligible.into_iter().enumerate() {
        match tape_fit_state_from_record(&tape, pool_cfg, pool_id.as_str(), index as u64) {
            Ok(candidate) if reserved_tape_uuids.contains(&candidate.tape_uuid) => {
                reserved_tape_count += 1;
            }
            Ok(candidate) => ranked.push((tape, candidate)),
            Err(err) => reasons.push(err),
        }
    }
    if ranked.is_empty() {
        if reserved_tape_count > 0 {
            return Err(SelectTapeError::NoUnreservedWritableTapes {
                pool_id,
                reserved_tape_count,
            });
        }
        return Err(SelectTapeError::NoWritableTapes { pool_id, reasons });
    }

    let candidates = ranked
        .iter()
        .map(|(_, candidate)| candidate.clone())
        .collect::<Vec<_>>();

    let ctx = PoolSelectionContext {
        candidates: &candidates,
        projected_footprint: object_size,
    };
    match policy.select(&ctx) {
        Selection::UseTape { tape_uuid } => ranked
            .into_iter()
            .find(|(_, candidate)| candidate.tape_uuid == tape_uuid)
            .map(|(tape, _)| selected_tape_from_record(tape, pool_id.as_str()))
            .unwrap_or_else(|| {
                Err(SelectTapeError::NoWritableTapes {
                    pool_id: pool_id.clone(),
                    reasons: Vec::new(),
                })
            }),
        Selection::NeedFreshTape => Err(SelectTapeError::NoWritableTapes { pool_id, reasons }),
    }
}

pub(super) fn checkpoint_journal_tape_uuids(
    checkpoint_journal_dir: &Path,
) -> Result<HashSet<TapeUuid>, StateError> {
    let mut journal_tapes = HashSet::new();
    for path in remanence_state::list_checkpoint_journals(checkpoint_journal_dir)? {
        let tape_uuid = remanence_state::tape_uuid_from_checkpoint_path(path.as_path())?;
        journal_tapes.insert(tape_uuid);
    }
    Ok(journal_tapes)
}

pub(super) fn tape_carries_checkpoint(
    checkpoint_journal_dir: &Path,
    checkpoint_journal_tapes: &HashSet<TapeUuid>,
    tape_uuid: TapeUuid,
) -> Result<bool, StateError> {
    if !checkpoint_journal_tapes.contains(&tape_uuid) {
        return Ok(false);
    }
    remanence_state::FileCheckpointJournal::open(checkpoint_journal_dir, tape_uuid)?
        .last()
        .map(|record| record.is_some())
}
