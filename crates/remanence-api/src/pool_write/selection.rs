//! Pool tape selection and pinned-tape admission.

use remanence_state::authority::TapeAuthorityView;
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

/// Scheduling ownership, distinct from the reservation acquired at admission.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TapeSelectionHold {
    LiveWriter,
    Closing,
    WorkerCloseout,
    Finalizing,
    ParkedDebt,
}
impl TapeSelectionHold {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::LiveWriter => "live writer",
            Self::Closing => "closing",
            Self::WorkerCloseout => "worker closeout",
            Self::Finalizing => "finalizing",
            Self::ParkedDebt => "parked debt",
        }
    }
    pub(crate) fn waitable(self) -> bool {
        matches!(self, Self::Closing | Self::WorkerCloseout)
    }
}

/// Skip diagnostics belong to one selection pass; the mount loop publishes the
/// pass that decides admission, while speculative passes remain DEBUG only.
#[derive(Default)]
pub(crate) struct SelectionSkips(Vec<(TapeUuid, String)>);
impl SelectionSkips {
    fn record(&mut self, pool_id: &str, tape: TapeUuid, reason: &str) {
        tracing::debug!(pool_id, tape_uuid = %Uuid::from_bytes(tape), reason, "pool selection skipped tape");
        self.0.push((tape, reason.to_owned()));
    }
    pub(crate) fn log(&self, pool_id: &str) {
        for (tape, reason) in &self.0 {
            tracing::info!(pool_id, tape_uuid = %Uuid::from_bytes(*tape), reason, "pool selection skipped tape");
        }
    }
}

/// One ownership snapshot for pool ranking; closeout holds remain candidates.
#[derive(Default)]
pub(crate) struct TapeSelectionHolds(
    pub(crate) std::collections::HashMap<TapeUuid, TapeSelectionHold>,
);
impl From<&HashSet<TapeUuid>> for TapeSelectionHolds {
    fn from(reserved: &HashSet<TapeUuid>) -> Self {
        Self(
            reserved
                .iter()
                .map(|id| (*id, TapeSelectionHold::LiveWriter))
                .collect(),
        )
    }
}

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
    holds: &TapeSelectionHolds,
    checkpoint_journal_dir: &Path,
    allowed_tape_uuids: &HashSet<TapeUuid>,
    skips: &mut SelectionSkips,
) -> Result<SelectedTape, SelectTapeError> {
    let policy: &dyn PoolSelectionPolicy = match pool_cfg.selection_policy {
        remanence_state::PoolSelectionPolicyName::CompleteOrFill => &CompleteOrFill,
        remanence_state::PoolSelectionPolicyName::FillOldest => &FillOldest,
    };
    select_tape_in_pool_with_policy_and_eligibility(
        state,
        pool_cfg,
        object_size,
        holds,
        policy,
        Some(checkpoint_journal_dir),
        Some(allowed_tape_uuids),
        skips,
    )
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
            remanence_state::TapeAuthority::open(checkpoint_journal_dir, tape_uuid, state.path())?;
        if let Some(intent) = journal.pending_finalization()? {
            intent.progress == remanence_state::TerminalFinalizationProgress::AfterReplicaC
                && intent.manual.is_none()
        } else {
            let checkpoint = journal.acquire_exclusive()?;
            checkpoint.latest_commit()?.is_some_and(|record| {
                record.sealed
                    && record.finalization.is_some_and(|completion| {
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
        check_pool_block_size_precondition(&tape, pool_cfg).map_err(|reason| {
            PinnedTapeError::NotWritable {
                tape_uuid: uuid_text,
                reason,
            }
        })?;
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
        && !tape_carries_checkpoint(
            state,
            checkpoint_journal_dir,
            &checkpoint_journal_tapes,
            tape_uuid,
        )?
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
        &TapeSelectionHolds::from(reserved_tape_uuids),
        policy,
        None,
        None,
        &mut SelectionSkips::default(),
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
        &TapeSelectionHolds::from(reserved_tape_uuids),
        policy,
        Some(checkpoint_journal_dir),
        allowed_tape_uuids,
        &mut SelectionSkips::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn select_tape_in_pool_with_policy_and_eligibility(
    state: &CatalogIndex,
    pool_cfg: &TapePoolConfig,
    object_size: u64,
    holds: &TapeSelectionHolds,
    policy: &dyn PoolSelectionPolicy,
    checkpoint_journal_dir: Option<&Path>,
    allowed_tape_uuids: Option<&HashSet<TapeUuid>>,
    skips: &mut SelectionSkips,
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
    let mut closeout_overhead = std::collections::HashMap::new();
    let mut batched_ineligible = Vec::new();
    let mut reserved_tape_count = 0usize;
    for mut tape in tapes {
        let tape_uuid = tape_uuid_from_vec(tape.tape_uuid.clone(), pool_id.as_str())?;
        if allowed_tape_uuids.is_some_and(|allowed| !allowed.contains(&tape_uuid)) {
            continue;
        }
        let hold = holds.0.get(&tape_uuid).copied();
        let waiting = hold.is_some_and(TapeSelectionHold::waitable);
        if let Some(hold) = hold.filter(|h| {
            matches!(
                h,
                TapeSelectionHold::LiveWriter | TapeSelectionHold::Finalizing
            )
        }) {
            skips.record(&pool_id, tape_uuid, hold.reason());
            reserved_tape_count += 1;
            continue;
        }
        match check_writability_preconditions(&tape, object_size) {
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
        if let Err(error) = check_pool_block_size_precondition(&tape, pool_cfg) {
            reasons.push(error);
            continue;
        }
        let conflicts = state
            .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
            .map_err(SelectTapeError::State)?;
        if let Some(conflict) = conflicts.first() {
            skips.record(
                &pool_id,
                tape_uuid,
                &format!("fenced by {}: {}", conflict.quarantine_id, conflict.reason),
            );
            reasons.push(WritabilityError::TapeIoFence {
                quarantine_id: conflict.quarantine_id.clone(),
                reason: conflict.reason.clone(),
            });
            continue;
        }
        // Ranking observes the durable fold without taking a competing lease.
        // Actual admission still requires this tape's exclusive reservation.
        let observed = checkpoint_journal_dir
            .filter(|_| {
                checkpoint_journal_tapes
                    .as_ref()
                    .is_some_and(|ids| ids.contains(&tape_uuid))
            })
            .map(|dir| {
                remanence_state::TapeAuthority::open(dir, tape_uuid, state.path())?
                    .observe_summary()
            })
            .transpose()?;
        if waiting && observed.is_none() {
            skips.record(&pool_id, tape_uuid, "closing without durable authority");
            reserved_tape_count += 1;
            continue;
        }
        if checkpoint_journal_dir.is_some() {
            let fresh = tape_is_fresh_for_checkpoint_admission(state, &tape, &tape_uuid)?;
            if !fresh && !observed.as_ref().is_some_and(summary_carries_checkpoint) {
                batched_ineligible.push(format!(
                    "{} ({})",
                    tape.voltag.as_deref().unwrap_or("<no-voltag>"),
                    Uuid::from_bytes(tape_uuid)
                ));
                continue;
            }
        }
        if let Some(summary) = &observed {
            if summary.sealed || summary.seal_due.is_some() || summary.finalization.is_some() {
                let reason = if summary.sealed {
                    "sealed"
                } else if summary.finalization.is_some() {
                    "finalizing"
                } else {
                    "seal due"
                };
                skips.record(&pool_id, tape_uuid, reason);
                reasons.push(WritabilityError::MaintenanceRequired {
                    tape_uuid: Uuid::from_bytes(tape_uuid).to_string(),
                });
                continue;
            }
            if summary.incarnation > 0 {
                let (block, parity) = super::capacity::selected_tape_geometry(&tape, &pool_id)?;
                if block != summary.block_size
                    || (match &parity {
                        remanence_parity::ParityConfig::Scheme(s) => Some(s),
                        remanence_parity::ParityConfig::None => None,
                    }) != summary.scheme.as_ref()
                {
                    reasons.push(super::capacity::missing_geometry(
                        "catalog geometry disagrees with native authority",
                    ));
                    continue;
                }
                tape.written_extent_lba =
                    Some(tape.written_extent_lba.unwrap_or(0).max(summary.cursor.lba));
                tape.highest_protected_ordinal = summary.protected;
                tape.total_committed_ordinals = summary.total;
            }
            if summary.closeout_due() {
                closeout_overhead.insert(tape_uuid, closeout_charge(summary)?);
            }
        }
        eligible.push(tape);
    }
    if eligible.is_empty() {
        // A reserved tape is held by a live checkpointed writer; report the
        // reservation ahead of the batched-eligibility of the other tapes.
        if reserved_tape_count > 0 {
            return Err(SelectTapeError::NoUnreservedWritableTapes {
                pool_id,
                reserved_tape_count,
            });
        }
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
    for (index, tape) in eligible.into_iter().enumerate() {
        match tape_fit_state_from_record(&tape, pool_cfg, pool_id.as_str(), index as u64) {
            Ok(mut candidate) => {
                if let Some(charge) = closeout_overhead.get(&candidate.tape_uuid) {
                    let Some(used) = candidate.used_bytes.checked_add(*charge) else {
                        continue;
                    };
                    candidate.used_bytes = used;
                }
                ranked.push((tape, candidate));
            }
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

    // Parked debt remains eligible for the real admission refusal when it is
    // the only option. Every other fitting candidate takes precedence.
    let (parked, available): (Vec<_>, Vec<_>) = candidates.into_iter().partition(|c| {
        matches!(
            holds.0.get(&c.tape_uuid),
            Some(TapeSelectionHold::ParkedDebt)
        )
    });
    let due = available
        .iter()
        .filter(|c| closeout_overhead.contains_key(&c.tape_uuid))
        .cloned()
        .collect::<Vec<_>>();
    let preferred = policy.select(&PoolSelectionContext {
        candidates: &due,
        projected_footprint: object_size,
    });
    let selection = if matches!(preferred, Selection::UseTape { .. }) {
        preferred
    } else {
        let ordinary = policy.select(&PoolSelectionContext {
            candidates: &available,
            projected_footprint: object_size,
        });
        if matches!(ordinary, Selection::UseTape { .. }) {
            ordinary
        } else {
            policy.select(&PoolSelectionContext {
                candidates: &parked,
                projected_footprint: object_size,
            })
        }
    };
    match selection {
        Selection::UseTape { tape_uuid } => {
            let selected = ranked
                .into_iter()
                .find(|(_, candidate)| candidate.tape_uuid == tape_uuid)
                .map(|(tape, _)| selected_tape_from_record(tape, pool_id.as_str()))
                .unwrap_or_else(|| {
                    Err(SelectTapeError::NoWritableTapes {
                        pool_id: pool_id.clone(),
                        reasons: Vec::new(),
                    })
                })?;
            Ok(selected)
        }
        Selection::NeedFreshTape => Err(SelectTapeError::NoWritableTapes { pool_id, reasons }),
    }
}

pub(super) fn checkpoint_journal_tape_uuids(
    checkpoint_journal_dir: &Path,
) -> Result<HashSet<TapeUuid>, StateError> {
    let mut journal_tapes = HashSet::new();
    for path in remanence_state::TapeAuthority::list_entries(checkpoint_journal_dir)? {
        let tape_uuid = remanence_state::TapeAuthority::entry_tape_uuid(path.as_path())?;
        journal_tapes.insert(tape_uuid);
    }
    Ok(journal_tapes)
}

pub(super) fn tape_carries_checkpoint(
    index: &CatalogIndex,
    checkpoint_journal_dir: &Path,
    checkpoint_journal_tapes: &HashSet<TapeUuid>,
    tape_uuid: TapeUuid,
) -> Result<bool, StateError> {
    if !checkpoint_journal_tapes.contains(&tape_uuid) {
        return Ok(false);
    }
    remanence_state::TapeAuthority::open(checkpoint_journal_dir, tape_uuid, index.path())?
        .observe_summary()
        .map(|summary| summary_carries_checkpoint(&summary))
}

/// Initialized authority is resumable even before the first Object checkpoint.
/// Both pinned admission and pool selection use this same predicate.
fn summary_carries_checkpoint(summary: &remanence_state::tape_log::fold::TapeSummary) -> bool {
    !summary.is_fresh()
}

/// Exact ordinary closeout sidecar charge, including its filemark. The fixed
/// parity rows include zero-padded stripes, just as the shared emitter does.
pub(crate) fn closeout_charge(
    s: &remanence_state::tape_log::fold::TapeSummary,
) -> Result<u64, StateError> {
    let scheme = s.scheme.as_ref().ok_or_else(|| {
        StateError::JournalReplayFailed("closeout charge lacks parity scheme".into())
    })?;
    let parity =
        u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.parity_blocks_per_stripe);
    let layout = remanence_parity::sidecar::checked_sidecar_index_capacity_layout(
        u64::from(s.block_size),
        parity,
        s.total.checked_sub(s.protected).ok_or_else(|| {
            StateError::JournalReplayFailed("closeout protected range exceeds total".into())
        })?,
    )
    .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
    layout
        .block_count
        .checked_mul(2)
        .and_then(|n| n.checked_add(parity))
        .and_then(|n| n.checked_add(2))
        .and_then(|n| n.checked_mul(u64::from(s.block_size)))
        .ok_or_else(|| {
            StateError::JournalReplayFailed("closeout capacity charge overflows u64".into())
        })
}

#[cfg(test)]
mod b7_fix_tests {
    use super::*;
    #[test]
    fn b7_fix_closeout_charge_checks_geometry_and_range() {
        let mut summary = remanence_state::tape_log::fold::TapeSummary::default();
        assert!(closeout_charge(&summary)
            .unwrap_err()
            .to_string()
            .contains("lacks parity"));
        summary.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("b7-test"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        });
        summary.protected = 1;
        assert!(closeout_charge(&summary)
            .unwrap_err()
            .to_string()
            .contains("exceeds total"));
    }
}
