//! Thread-local benchmark counters at the actual codec/copy boundaries. Enabled
//! only for fixtures; no counters or allocations enter ordinary production builds.
use std::cell::Cell;

/// Observed work, including one-frame construction decoders and group scans.
/// Byte totals normalize partial/group scans into whole-log pass equivalents.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScanCounts {
    /// Catalog Object/member payloads decoded.
    pub catalog_payloads: u64,
    /// Full-decoder traversals started (a group or one frame also counts).
    pub full_scans: u64,
    /// Structural-decoder traversals started.
    pub row_scans: u64,
    /// Bytes in frames emitted by full decoders, including framing.
    pub full_frame_bytes: u64,
    /// Encoded frame spans emitted by structural decoders (payload skipped).
    pub row_frame_span_bytes: u64,
    /// Whole frames copied from a prepared spool into the active file.
    pub copied_frames: u64,
    /// Bytes copied by those frame writes.
    pub copied_bytes: u64,
}
thread_local! {
    static COUNTS: Cell<ScanCounts> = Cell::new(ScanCounts::default());
}
/// Begin measurement on this thread. Independent tests do not share counters.
pub fn reset() {
    COUNTS.set(ScanCounts::default());
}
/// Read cumulative measured work since reset.
pub fn counts() -> ScanCounts {
    COUNTS.get()
}
pub(super) fn start(row: bool) {
    update(|mut c| {
        if row {
            c.row_scans += 1;
        } else {
            c.full_scans += 1;
        }
        c
    });
}
pub(super) fn frame(row: bool, bytes: u64) {
    update(|mut c| {
        if row {
            c.row_frame_span_bytes += bytes;
        } else {
            c.full_frame_bytes += bytes;
        }
        c
    });
}
pub(super) fn copied(bytes: u64) {
    update(|mut c| {
        c.copied_frames += 1;
        c.copied_bytes += bytes;
        c
    });
}

pub(super) fn catalog_payload() {
    update(|mut c| {
        c.catalog_payloads += 1;
        c
    });
}

fn update(f: impl FnOnce(ScanCounts) -> ScanCounts) {
    COUNTS.set(f(COUNTS.get()));
}

impl ScanCounts {
    /// Sum of emitted full-frame byte spans divided by the measured log's frame
    /// bytes. Includes construction decoders, preflights and validation; it is
    /// deliberately distinct from physical disk reads/page-cache misses.
    pub fn full_pass_equivalents(&self, frame_bytes: u64) -> f64 {
        self.full_frame_bytes as f64 / frame_bytes as f64
    }
    /// Structural cursor spans divided by the log's frame bytes. These passes
    /// skip healthy catalog payloads rather than reading that byte span.
    pub fn row_pass_equivalents(&self, frame_bytes: u64) -> f64 {
        self.row_frame_span_bytes as f64 / frame_bytes as f64
    }
}
