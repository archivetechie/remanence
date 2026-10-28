//! Atomic daemon-wide reservation of pipeline I/O memory.
//!
//! Reservations precede allocation.  The RAII permit rolls the reservation
//! back on refused allocations and when the owning stream/spool closes.
//! Unlocked read-reservoir slabs retain their reservations.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Fixed-ceiling reservation manager shared by spools and read reservoirs.
#[derive(Debug)]
pub(crate) struct IoMemoryReservation {
    ceiling: u64,
    granted: AtomicU64,
    retained_epochs: Mutex<BTreeMap<String, usize>>,
    epoch_discards: AtomicU64,
    #[cfg(test)]
    pub(crate) epoch_test_action: AtomicU64,
}

impl IoMemoryReservation {
    pub(crate) fn new(ceiling: u64) -> Result<Arc<Self>, String> {
        if ceiling == 0 {
            return Err("daemon.io_memory_ceiling must be non-zero".to_string());
        }
        Ok(Arc::new(Self {
            ceiling,
            granted: AtomicU64::new(0),
            retained_epochs: Mutex::new(BTreeMap::new()),
            epoch_discards: AtomicU64::new(0),
            #[cfg(test)]
            epoch_test_action: AtomicU64::new(0),
        }))
    }

    /// Return the configured ceiling for admission diagnostics.
    pub(crate) fn ceiling(&self) -> u64 {
        self.ceiling
    }

    #[cfg(test)]
    pub(crate) fn granted(&self) -> u64 {
        self.granted.load(Ordering::Acquire)
    }

    /// Count rejected take-over state independently of device command batching.
    pub(crate) fn record_epoch_discard(&self) {
        self.epoch_discards.fetch_add(1, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn epoch_discards(&self) -> u64 {
        self.epoch_discards.load(Ordering::Relaxed)
    }

    /// Name retained epochs and drives in later allocation refusals.
    pub(crate) fn retained_diagnostics(&self) -> String {
        self.retained_epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Attach an epoch owner to a granted reservation for its full lifetime.
    pub(crate) fn try_reserve_epoch(
        self: &Arc<Self>,
        bytes: u64,
        owner: String,
    ) -> Option<IoMemoryPermit> {
        let mut permit = self.try_reserve(bytes)?;
        *self
            .retained_epochs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(owner.clone())
            .or_default() += 1;
        permit.epoch_owner = Some(owner);
        Some(permit)
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
                epoch_owner: None,
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
                        epoch_owner: None,
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
    epoch_owner: Option<String>,
}

impl IoMemoryPermit {
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl Drop for IoMemoryPermit {
    fn drop(&mut self) {
        if let Some(owner) = &self.epoch_owner {
            let mut owners = self
                .manager
                .retained_epochs
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(count) = owners.get_mut(owner) {
                *count -= 1;
                if *count == 0 {
                    owners.remove(owner);
                }
            }
        }
        let previous = self.manager.granted.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal keeps every retained epoch owner visible until its permit drops.
    #[test]
    fn retained_epoch_refusal_names_tapes_and_drives() {
        let manager = IoMemoryReservation::new(10).unwrap();
        let first = manager
            .try_reserve_epoch(4, "tape 01 drive A".into())
            .unwrap();
        let second = manager
            .try_reserve_epoch(4, "tape 02 drive B".into())
            .unwrap();
        assert_eq!(manager.try_reserve_with_available(3).unwrap_err(), 2);
        assert_eq!(
            manager.retained_diagnostics(),
            "tape 01 drive A, tape 02 drive B"
        );
        drop(first);
        assert_eq!(manager.retained_diagnostics(), "tape 02 drive B");
        drop(second);
        assert!(manager.retained_diagnostics().is_empty());
    }

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
