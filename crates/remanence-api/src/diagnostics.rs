//! Cheap Layer 5 diagnostic helpers for write-path throughput timing.
//!
//! These helpers keep the instrumentation format consistent while leaving the
//! write path behavior unchanged. Callers use monotonic elapsed durations and
//! emit structured `tracing` events for the harness/operator.

use std::time::Duration;

pub(crate) fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

pub(crate) fn mib_per_s(bytes: u64, duration: Duration) -> f64 {
    if duration.is_zero() {
        0.0
    } else {
        (bytes as f64 / 1024.0 / 1024.0) / duration.as_secs_f64()
    }
}
