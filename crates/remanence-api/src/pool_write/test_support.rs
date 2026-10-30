//! Generic-sink fixtures for shared pool-write primitives (tests only).

use super::append::{write_to_selected_tape_inner, write_to_selected_tape_with_live_counter_impl};
use super::capacity::ensure_request_pool_matches_config;
use super::model::{
    PoolWriteDurability, PoolWriteError, PoolWriteResult, SelectedTape, WriteObjectToPoolRequest,
};
use super::no_parity::maybe_replay_pool_write;
use super::selection::select_tape_in_pool;
use remanence_library::BlockSink;
use remanence_state::{CatalogIndex, TapePoolConfig};
use std::{collections::HashSet, sync::Arc};

/// Write one regular file to a caller-named pool using the Phase 1
/// non-hardware-compatible `BlockSink` path, commit catalog rows, and return
/// the resulting object locator.
#[cfg(test)]
pub fn write_object_to_pool(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
) -> Result<PoolWriteResult, PoolWriteError> {
    let fence_cfg = &super::test_fence_config(state.path().parent().expect("catalog parent"));
    ensure_request_pool_matches_config(&request, pool_cfg)?;
    if let Some(result) = maybe_replay_pool_write(state, pool_cfg, &request)? {
        return Ok(result);
    }
    let source_size = request.source.size_bytes()?;
    let reserved_tape_uuids = HashSet::new();
    let selected = select_tape_in_pool(state, pool_cfg, source_size, &reserved_tape_uuids)?;
    write_to_selected_tape_inner(
        state,
        fence_cfg,
        sink,
        pool_cfg,
        request,
        selected,
        false,
        PoolWriteDurability::PerObject,
    )
}

/// Write one regular file to a previously selected tape without re-running
/// pool tape selection.
///
/// This is the select-once entrypoint for callers that already opened a write
/// session against a concrete tape. The selected tape's [`remanence_parity::ParityConfig`]
/// controls whether the write uses the existing parity path or the direct
/// no-parity bootstrap/body/filemark path.
#[cfg(test)]
pub fn write_to_selected_tape(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
) -> Result<PoolWriteResult, PoolWriteError> {
    write_to_selected_tape_with_live_counter(state, sink, pool_cfg, request, selected, None)
}

#[cfg(test)]
pub(crate) fn write_to_selected_tape_with_live_counter(
    state: &mut CatalogIndex,
    sink: &mut dyn BlockSink,
    pool_cfg: &TapePoolConfig,
    request: WriteObjectToPoolRequest,
    selected: SelectedTape,
    live_write_counter: Option<Arc<crate::DriveByteCounters>>,
) -> Result<PoolWriteResult, PoolWriteError> {
    let fence_cfg = &super::test_fence_config(state.path().parent().expect("catalog parent"));
    write_to_selected_tape_with_live_counter_impl(
        state,
        fence_cfg,
        sink,
        pool_cfg,
        request,
        selected,
        live_write_counter,
        true,
        PoolWriteDurability::PerObject,
    )
}
