//! Host-only replay of committed pool writes and configured-pool lookup.

use super::model::{PoolWriteError, PoolWriteResult, WriteObjectToPoolRequest};
use super::no_parity::maybe_replay_pool_write;
use remanence_state::{StateHandle, TapePoolConfig};

/// Resolve an already-committed direct write without selecting or moving tape.
///
/// This globally reconciles configured checkpoint journals first, then checks
/// the exact operator-owned pool/caller replay key and content guards. The
/// drive-bound writer repeats the same checks after selection to close races.
pub fn replay_committed_pool_write_from_state(
    state: &mut StateHandle,
    request: &WriteObjectToPoolRequest,
) -> Result<Option<PoolWriteResult>, PoolWriteError> {
    crate::reconcile_checkpoint_journal_projections(state)
        .map_err(|status| PoolWriteError::CheckpointReconciliation(status.message().to_string()))?;
    replay_committed_pool_write_reconciled(state, request)
}

/// Check the ordinary replay key after the command has reconciled its state lock.
pub(crate) fn replay_committed_pool_write_reconciled(
    state: &mut StateHandle,
    request: &WriteObjectToPoolRequest,
) -> Result<Option<PoolWriteResult>, PoolWriteError> {
    let pool_cfg = configured_direct_pool(state, &request.pool_id)?;
    maybe_replay_pool_write(state.catalog_index(), &pool_cfg, request)
}

pub(super) fn configured_direct_pool(
    state: &StateHandle,
    requested_pool_id: &str,
) -> Result<TapePoolConfig, PoolWriteError> {
    state
        .config()
        .tape_pools
        .iter()
        .find(|pool| pool.id.trim() == requested_pool_id.trim())
        .cloned()
        .ok_or_else(|| {
            PoolWriteError::InvalidInput(format!(
                "request names unconfigured tape pool {}",
                requested_pool_id.trim()
            ))
        })
}
