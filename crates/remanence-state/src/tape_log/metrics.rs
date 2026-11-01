//! Bounded replay measurements for native authority and terminal snapshots.

pub use remanence_parity::authority::TerminalSourceSummary as TerminalIndexAuthoritySummary;

/// Deterministic per-pass replay metrics captured while freezing terminal
/// authority.
///
/// These values describe the single bounded validation scan used to freeze
/// each snapshot. They are not cumulative counters for the additional full
/// scans used to validate source agreement during construction or to emit
/// replica rows later. Peak-live values therefore remain directly comparable
/// between journals regardless of how many replica passes a caller performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalIndexAuthorityReplayMetrics {
    /// Checkpoint-journal validation scan.
    pub checkpoint: CheckpointBoundedReplayMetrics,
    /// Parity-journal validation scan, absent on parity-off tapes.
    pub parity: Option<remanence_parity::BoundedJournalReplayMetrics>,
}

/// Deterministic allocation and pass counters for bounded checkpoint replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckpointBoundedReplayMetrics {
    /// Complete journal passes performed.
    pub replay_passes: u64,
    /// Number of length-and-integrity-protected frames decoded.
    pub frame_count: u64,
    /// Largest encoded frame payload decoded in bytes.
    pub peak_frame_payload_bytes: u64,
    /// Largest number of checkpoint records simultaneously retained.
    pub peak_live_record_count: u64,
    /// Largest number of Object projections simultaneously retained.
    pub peak_live_object_rows: u64,
}
