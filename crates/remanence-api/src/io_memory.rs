//! Atomic daemon-wide reservation of pipeline I/O memory.
//!
//! Reservations precede allocation.  The RAII permit rolls the reservation
//! back on every allocation or `mlock` failure path and when the owning
//! stream/spool closes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Fixed-ceiling reservation manager shared by spools and read reservoirs.
#[derive(Debug)]
pub(crate) struct IoMemoryReservation {
    ceiling: u64,
    granted: AtomicU64,
}

impl IoMemoryReservation {
    pub(crate) fn new(ceiling: u64) -> Result<Arc<Self>, String> {
        if ceiling == 0 {
            return Err("daemon.io_memory_ceiling must be non-zero".to_string());
        }
        Ok(Arc::new(Self {
            ceiling,
            granted: AtomicU64::new(0),
        }))
    }

    #[cfg(test)]
    pub(crate) fn granted(&self) -> u64 {
        self.granted.load(Ordering::Acquire)
    }

    pub(crate) fn try_reserve(self: &Arc<Self>, bytes: u64) -> Option<IoMemoryPermit> {
        self.try_reserve_with_available(bytes).ok()
    }

    /// Atomically reserve `bytes`, or return the available byte count observed
    /// at the failed admission decision.
    ///
    /// The returned shortfall value is diagnostic evidence from the same
    /// atomic loop that denied the request. Callers must not use a separate
    /// "free bytes" read as an admission grant because another session could
    /// consume that capacity before its reservation.
    pub(crate) fn try_reserve_with_available(
        self: &Arc<Self>,
        bytes: u64,
    ) -> Result<IoMemoryPermit, u64> {
        if bytes == 0 {
            return Ok(IoMemoryPermit {
                manager: Arc::clone(self),
                bytes: 0,
            });
        }
        let mut current = self.granted.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return Err(self.ceiling.saturating_sub(current));
            };
            if next > self.ceiling {
                return Err(self.ceiling.saturating_sub(current));
            }
            match self.granted.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    debug_assert!(next <= self.ceiling);
                    return Ok(IoMemoryPermit {
                        manager: Arc::clone(self),
                        bytes,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

/// RAII ownership of bytes granted from [`IoMemoryReservation`].
#[derive(Debug)]
pub(crate) struct IoMemoryPermit {
    manager: Arc<IoMemoryReservation>,
    bytes: u64,
}

impl IoMemoryPermit {
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for IoMemoryPermit {
    fn drop(&mut self) {
        let previous = self.manager.granted.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_stream_and_spool_growth_never_exceeds_ceiling() {
        const CEILING: u64 = 31;
        let manager = IoMemoryReservation::new(CEILING).expect("manager");
        let peak = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let handles = [11, 13, 17].map(|bytes| {
            let manager = Arc::clone(&manager);
            let peak = Arc::clone(&peak);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let permit = manager.try_reserve(bytes);
                peak.fetch_max(manager.granted(), Ordering::AcqRel);
                permit
            })
        });
        barrier.wait();
        let permits = handles.map(|handle| handle.join().expect("growth thread"));
        assert!(peak.load(Ordering::Acquire) <= CEILING);
        assert!(manager.granted() <= CEILING);
        drop(permits);
        assert_eq!(manager.granted(), 0);
    }

    #[test]
    fn permit_drop_rolls_back_failed_follow_on_work() {
        let manager = IoMemoryReservation::new(8).expect("manager");
        let permit = manager.try_reserve(7).expect("reserve");
        assert_eq!(permit.bytes(), 7);
        assert_eq!(manager.granted(), 7);
        drop(permit);
        assert_eq!(manager.granted(), 0);
        assert!(manager.try_reserve(8).is_some());
    }

    #[test]
    fn denied_reservation_reports_available_bytes_from_atomic_decision() {
        let manager = IoMemoryReservation::new(10).expect("manager");
        let held = manager.try_reserve(7).expect("initial reservation");

        assert_eq!(manager.try_reserve_with_available(4).unwrap_err(), 3);
        assert_eq!(manager.granted(), 7);

        drop(held);
        assert!(manager.try_reserve_with_available(10).is_ok());
    }
}
