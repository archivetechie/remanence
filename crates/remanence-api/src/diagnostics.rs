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

/// Measure a host suffix on every exit without changing its error handling.
pub(crate) struct WriteTailTimer {
    started: std::time::Instant,
    object_id: uuid::Uuid,
    pool_id: String,
    tape_uuid: uuid::Uuid,
    parity: &'static str,
    payload_bytes: u64,
}

impl WriteTailTimer {
    pub(crate) fn new(
        object_id: uuid::Uuid,
        pool_id: &str,
        tape_uuid: [u8; 16],
        parity: &'static str,
        payload_bytes: u64,
    ) -> Self {
        Self {
            started: std::time::Instant::now(),
            object_id,
            pool_id: pool_id.to_owned(),
            tape_uuid: uuid::Uuid::from_bytes(tape_uuid),
            parity,
            payload_bytes,
        }
    }
}

impl Drop for WriteTailTimer {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        tracing::info!(target: "remanence_write_diag", phase = "tail", object_id = %self.object_id,
            pool_id = %self.pool_id, tape_uuid = %self.tape_uuid, parity = self.parity,
            payload_bytes = self.payload_bytes, elapsed_ms = duration_ms(elapsed),
            throughput_mib_s = mib_per_s(self.payload_bytes, elapsed), "remanence_write_diag");
    }
}
