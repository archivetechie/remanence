//! Pool-targeted object write core for the Phase 1 non-hardware path.
//!
//! This module composes Layer 4 catalog state, Layer 3b `rem-object-v1`
//! streaming, Layer 3c parity, and the existing in-memory-compatible
//! `BlockSink` adapter. It intentionally contains the tape-selection boundary
//! so the later policy workstream can replace that one function without
//! changing the write engine.

mod media;
mod overlap;
mod prepare;
mod staging;
#[cfg(test)]
pub(crate) use prepare::canonical_admission_format_error;
mod capacity;
pub(crate) use capacity::{
    batched_append_context_after_checkpoint, ensure_empty_checkpoint_matches_catalog_freshness,
    ensure_selected_tape_accepts_session_write, first_batched_append_context,
    next_batched_append_context, selected_tape_geometry, selected_tape_seal_reason_at_barrier,
};

pub use media::{
    can_read, can_write, check_writability_preconditions, lto_generation_from_drive_product,
    lto_generation_from_voltag, raw_capacity_bytes, LtoGen,
};

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const VERIFY_BOOTSTRAP_READ_BYTES: usize = 1024 * 1024;
const NO_PARITY_BOOTSTRAP_BLOCKS: u64 = 1;
/// Fresh media begins with the one-block identity bootstrap and its trailing
/// filemark before any Object can be admitted.
const PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS: u64 = 2;
const TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS: u64 = 4;
/// Unpooled media has no ordinary fill policy. Its close-only authority uses
/// the maximally conservative nonempty band `L=0, H=1`, reserving every block
/// above the first for a possible terminal close.
const UNPOOLED_TERMINAL_LOW_WATERMARK_BLOCKS: u64 = 0;
const UNPOOLED_TERMINAL_HIGH_WATERMARK_BLOCKS: u64 = 1;

/// Exact terminal-triple authority paired with its atomic parity-spool grant.
mod model;
pub use model::*;

mod selection;
pub use selection::*;

mod direct;
pub use direct::*;

mod no_parity;
pub(crate) use no_parity::maybe_replay_pool_write;

#[cfg(test)]
mod tests;
