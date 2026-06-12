//! Pure-state-machine coalescer. See `docs/layer2c-design.md` §3.3.
//!
//! The coalescer takes raw `(event, now)` pushes and a separate
//! `tick(now)` poke for time advancement. It returns at most one
//! [`Coalesced`] burst per call; the caller is responsible for
//! actually delivering it.
//!
//! Sliding-window rule: any event observed within `window` of the
//! previous event joins the current burst AND resets the timer. A
//! separate maximum burst age prevents a steady event storm from
//! deferring notification indefinitely.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use super::event::{Coalesced, EventSource, HotplugEvent};

/// Coalescer state. One per [`HotplugSource`](super::source::HotplugSource)
/// task.
pub struct Coalescer {
    window: Duration,
    max_age: Duration,
    pending: Option<Coalesced>,
}

const DEFAULT_MAX_BURST_WINDOWS: u32 = 10;

impl Coalescer {
    /// Build a coalescer with the given sliding window.
    /// [`Duration::ZERO`] disables coalescing: every push immediately
    /// produces a one-event burst.
    pub fn new(window: Duration) -> Self {
        let max_age = window
            .checked_mul(DEFAULT_MAX_BURST_WINDOWS)
            .unwrap_or(Duration::MAX);
        Self::with_max_age(window, max_age)
    }

    /// Build a coalescer with an explicit maximum age for any non-empty
    /// burst. This is mainly for tests and platform tuning; callers that
    /// do not care should use [`Self::new`].
    pub fn with_max_age(window: Duration, max_age: Duration) -> Self {
        Self {
            window,
            max_age,
            pending: None,
        }
    }

    /// Push a raw event observed at `now`.
    ///
    /// Returns `Some(burst)` if this event closed a previous burst
    /// (window expired) — the closed burst is returned and the new
    /// event starts the next burst.
    ///
    /// Returns `None` if the event joined the current burst (window
    /// not yet expired), or if there was no previous burst and this
    /// event started a fresh one.
    pub fn push(&mut self, event: HotplugEvent, now: Instant) -> Option<Coalesced> {
        // Special case: window = 0 → every event becomes its own
        // burst, emitted immediately. No pending state is held.
        if self.window.is_zero() {
            return Some(Coalesced::singleton(&event));
        }

        let emit = match self.pending.as_ref() {
            Some(burst) if self.should_emit(burst, now) => {
                // Window expired between the last event and this one;
                // close the current burst and start a new one with
                // this event.
                self.pending.take()
            }
            _ => None,
        };

        self.merge_event(event, now);
        emit
    }

    /// Tick the clock to `now` without an event. If the current burst
    /// has been idle for at least `window`, emit it. Used by the
    /// driver loop's timer branch.
    pub fn tick(&mut self, now: Instant) -> Option<Coalesced> {
        if self.window.is_zero() {
            return None;
        }
        match self.pending.as_ref() {
            Some(burst) if self.should_emit(burst, now) => self.pending.take(),
            _ => None,
        }
    }

    /// Flush any pending burst regardless of window state. Used on
    /// shutdown to avoid silently dropping a partially-formed burst.
    pub fn flush(&mut self) -> Option<Coalesced> {
        self.pending.take()
    }

    /// When the driver loop should next consider firing a tick, given
    /// the current pending burst (if any). Returns `None` if there is
    /// no pending burst — the loop should park until the next event.
    pub fn next_tick_at(&self) -> Option<Instant> {
        self.pending.as_ref().map(|b| {
            let window_at = b.last_at + self.window;
            let max_age_at = b.first_at + self.max_age;
            window_at.min(max_age_at)
        })
    }

    fn should_emit(&self, burst: &Coalesced, now: Instant) -> bool {
        now.duration_since(burst.last_at) >= self.window
            || now.duration_since(burst.first_at) >= self.max_age
    }

    fn merge_event(&mut self, event: HotplugEvent, now: Instant) {
        let event_has_no_scope = event.sysfs_path.is_none() && event.device_node.is_none();
        match &mut self.pending {
            Some(burst) => {
                burst.raw_event_count += 1;
                burst.last_at = now;
                if event_has_no_scope {
                    burst.has_unknown_scope = true;
                }
                match event.source {
                    EventSource::Scsi(s) => {
                        burst.subsystems.insert(s);
                    }
                }
                burst.kinds.insert(event.kind);
                if let Some(p) = event.sysfs_path {
                    burst.touched_paths.insert(p);
                }
                if let Some(p) = event.device_node {
                    burst.touched_paths.insert(p);
                }
            }
            None => {
                let mut subsystems = BTreeSet::new();
                match event.source {
                    EventSource::Scsi(s) => {
                        subsystems.insert(s);
                    }
                }
                let mut kinds = BTreeSet::new();
                kinds.insert(event.kind);
                let mut touched = BTreeSet::new();
                if let Some(p) = event.sysfs_path {
                    touched.insert(p);
                }
                if let Some(p) = event.device_node {
                    touched.insert(p);
                }
                self.pending = Some(Coalesced {
                    raw_event_count: 1,
                    subsystems,
                    kinds,
                    touched_paths: touched,
                    first_at: now,
                    last_at: now,
                    has_unknown_scope: event_has_no_scope,
                });
            }
        }
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::event::{EventSource, HotplugKind, ScsiSubsystem};
    use std::path::PathBuf;

    /// Make a synthetic event at time `at` with subsystem and kind.
    fn ev(at: Instant, kind: HotplugKind, sub: ScsiSubsystem, path: &str) -> HotplugEvent {
        HotplugEvent {
            kind,
            source: EventSource::Scsi(sub),
            sysfs_path: Some(PathBuf::from(path)),
            device_node: None,
            at,
        }
    }

    #[test]
    fn single_event_pending_until_tick() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(500));
        let r = c.push(
            ev(
                base,
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base,
        );
        assert!(r.is_none(), "first push should not emit");

        // Inside the window: no emit.
        let r = c.tick(base + Duration::from_millis(300));
        assert!(r.is_none());

        // After the window: emit.
        let r = c.tick(base + Duration::from_millis(500));
        let burst = r.expect("tick after window should emit");
        assert_eq!(burst.raw_event_count, 1);
        assert!(burst.subsystems.contains(&ScsiSubsystem::ScsiGeneric));
        assert!(burst.kinds.contains(&HotplugKind::Added));
        assert_eq!(burst.touched_paths.len(), 1);
    }

    #[test]
    fn events_within_window_collapse_into_one_burst() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(500));

        c.push(
            ev(
                base,
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base,
        );
        c.push(
            ev(
                base + Duration::from_millis(100),
                HotplugKind::Changed,
                ScsiSubsystem::ScsiTape,
                "/sys/st0",
            ),
            base + Duration::from_millis(100),
        );
        c.push(
            ev(
                base + Duration::from_millis(200),
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg8",
            ),
            base + Duration::from_millis(200),
        );

        // Three events arrived at t=0, 100, 200ms. Window is 500ms.
        // Last event was at 200ms; window expires at 700ms.
        let r = c.tick(base + Duration::from_millis(699));
        assert!(r.is_none(), "tick before window expiry should not emit");

        let r = c.tick(base + Duration::from_millis(700));
        let burst = r.expect("tick at window expiry should emit");
        assert_eq!(burst.raw_event_count, 3);
        assert_eq!(burst.subsystems.len(), 2); // ScsiGeneric + ScsiTape
        assert_eq!(burst.kinds.len(), 2); // Added + Changed
        assert_eq!(burst.touched_paths.len(), 3); // sg7, st0, sg8
    }

    #[test]
    fn sliding_window_resets_on_each_event() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(500));

        // Events at 0, 300, 600 — each within 500ms of the previous.
        for offset in [0, 300, 600] {
            c.push(
                ev(
                    base + Duration::from_millis(offset),
                    HotplugKind::Added,
                    ScsiSubsystem::ScsiGeneric,
                    &format!("/sys/sg{offset}"),
                ),
                base + Duration::from_millis(offset),
            );
        }

        // Last event was at t=600ms. Window expires at t=1100ms.
        let r = c.tick(base + Duration::from_millis(1099));
        assert!(
            r.is_none(),
            "tick before sliding-window expiry should not emit"
        );

        let r = c.tick(base + Duration::from_millis(1100));
        let burst = r.expect("sliding-window expiry should emit");
        assert_eq!(burst.raw_event_count, 3);
    }

    #[test]
    fn event_after_window_closes_previous_burst() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(500));

        c.push(
            ev(
                base,
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base,
        );

        // Event arrives 1 second later — well past the window.
        let r = c.push(
            ev(
                base + Duration::from_secs(1),
                HotplugKind::Removed,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg8",
            ),
            base + Duration::from_secs(1),
        );
        let burst = r.expect("event past window should close the previous burst");
        assert_eq!(burst.raw_event_count, 1);
        assert!(burst.kinds.contains(&HotplugKind::Added));

        // The second event is now pending. Flush to confirm.
        let pending = c.flush().expect("pending burst should exist");
        assert_eq!(pending.raw_event_count, 1);
        assert!(pending.kinds.contains(&HotplugKind::Removed));
    }

    #[test]
    fn tick_emits_when_max_age_expires_despite_recent_event() {
        let base = Instant::now();
        let mut c =
            Coalescer::with_max_age(Duration::from_millis(500), Duration::from_millis(1_000));

        for offset in [0, 300, 600, 900] {
            assert!(c
                .push(
                    ev(
                        base + Duration::from_millis(offset),
                        HotplugKind::Added,
                        ScsiSubsystem::ScsiGeneric,
                        &format!("/sys/sg{offset}"),
                    ),
                    base + Duration::from_millis(offset),
                )
                .is_none());
        }

        // The most recent event was only 100ms ago, but the burst's
        // first event is now 1000ms old.
        let burst = c
            .tick(base + Duration::from_millis(1_000))
            .expect("max age should emit");

        assert_eq!(burst.raw_event_count, 4);
        assert!(c.flush().is_none());
    }

    #[test]
    fn push_after_max_age_closes_previous_burst_before_merging_new_event() {
        let base = Instant::now();
        let mut c =
            Coalescer::with_max_age(Duration::from_millis(500), Duration::from_millis(1_000));

        for offset in [0, 300, 600, 900] {
            assert!(c
                .push(
                    ev(
                        base + Duration::from_millis(offset),
                        HotplugKind::Added,
                        ScsiSubsystem::ScsiGeneric,
                        &format!("/sys/sg{offset}"),
                    ),
                    base + Duration::from_millis(offset),
                )
                .is_none());
        }

        let emitted = c
            .push(
                ev(
                    base + Duration::from_millis(1_200),
                    HotplugKind::Removed,
                    ScsiSubsystem::ScsiTape,
                    "/sys/st0",
                ),
                base + Duration::from_millis(1_200),
            )
            .expect("max-age push should close previous burst");

        assert_eq!(emitted.raw_event_count, 4);
        assert!(emitted.kinds.contains(&HotplugKind::Added));

        let pending = c.flush().expect("new event should be pending");
        assert_eq!(pending.raw_event_count, 1);
        assert!(pending.kinds.contains(&HotplugKind::Removed));
    }

    #[test]
    fn zero_window_emits_every_event_immediately() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::ZERO);

        for offset in [0, 50, 100] {
            let r = c.push(
                ev(
                    base + Duration::from_millis(offset),
                    HotplugKind::Added,
                    ScsiSubsystem::ScsiGeneric,
                    &format!("/sys/sg{offset}"),
                ),
                base + Duration::from_millis(offset),
            );
            let burst = r.expect("zero-window push must emit immediately");
            assert_eq!(burst.raw_event_count, 1);
        }

        // No pending state.
        assert!(c.flush().is_none());
        assert!(c.tick(base + Duration::from_secs(10)).is_none());
    }

    #[test]
    fn flush_returns_pending_regardless_of_window() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(500));
        c.push(
            ev(
                base,
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base,
        );

        // Far before window expiry — flush still returns the pending burst.
        let burst = c.flush().expect("flush should return any pending burst");
        assert_eq!(burst.raw_event_count, 1);

        // Subsequent flush is empty.
        assert!(c.flush().is_none());
    }

    #[test]
    fn next_tick_at_tracks_pending() {
        let base = Instant::now();
        let mut c = Coalescer::new(Duration::from_millis(500));
        assert!(c.next_tick_at().is_none(), "no pending → no next tick");

        c.push(
            ev(
                base,
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base,
        );
        let expected = base + Duration::from_millis(500);
        assert_eq!(c.next_tick_at(), Some(expected));

        // Sliding update.
        c.push(
            ev(
                base + Duration::from_millis(200),
                HotplugKind::Changed,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base + Duration::from_millis(200),
        );
        let expected = base + Duration::from_millis(700);
        assert_eq!(c.next_tick_at(), Some(expected));
    }

    #[test]
    fn next_tick_at_uses_max_age_when_earlier_than_sliding_window() {
        let base = Instant::now();
        let mut c = Coalescer::with_max_age(Duration::from_millis(500), Duration::from_millis(700));

        c.push(
            ev(
                base,
                HotplugKind::Added,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base,
        );
        c.push(
            ev(
                base + Duration::from_millis(300),
                HotplugKind::Changed,
                ScsiSubsystem::ScsiGeneric,
                "/sys/sg7",
            ),
            base + Duration::from_millis(300),
        );

        assert_eq!(c.next_tick_at(), Some(base + Duration::from_millis(700)));
    }
}
