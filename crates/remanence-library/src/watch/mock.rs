//! In-memory mock [`HotplugSource`] for unit tests.
//! See `docs/layer2c-design.md` §5, §8.
//!
//! Tests inject pre-built [`Coalesced`] bursts via [`MockHotplugSource::inject`];
//! the mock relays them to the receiver. The coalescer is exercised
//! separately in `super::coalesce` — the mock intentionally does no
//! coalescing of its own.

use std::time::Duration;

use tokio::sync::mpsc;

use super::error::WatcherError;
use super::event::Coalesced;
use super::source::{HotplugReceiver, HotplugSource};

/// Test double for [`HotplugSource`]. Holds an internal mpsc channel;
/// [`Self::inject`] pushes bursts, [`Self::subscribe`] hands the
/// receiver to the consumer.
pub struct MockHotplugSource {
    tx: mpsc::Sender<Result<Coalesced, WatcherError>>,
    rx: Option<mpsc::Receiver<Result<Coalesced, WatcherError>>>,
}

impl MockHotplugSource {
    /// Build a fresh mock with a default channel capacity of 64.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(64);
        Self { tx, rx: Some(rx) }
    }

    /// Push a pre-built burst into the channel. Returns
    /// [`WatcherError::ChannelClosed`] if the consumer dropped the
    /// receiver. Uses `try_send`, so the mock will not block tests if
    /// the channel is full — instead it returns
    /// [`WatcherError::ChannelClosed`] there too (saturated mock
    /// channel is a test-author bug).
    pub fn inject(&self, burst: Coalesced) -> Result<(), WatcherError> {
        self.tx
            .try_send(Ok(burst))
            .map_err(|_| WatcherError::ChannelClosed)
    }

    /// Inject a `SourceClosed` terminal marker, mimicking the
    /// real source going down mid-session. After this, the consumer
    /// will receive `Some(Err(SourceClosed))` and then `None`.
    pub fn inject_source_closed(&self) -> Result<(), WatcherError> {
        self.tx
            .try_send(Err(WatcherError::SourceClosed))
            .map_err(|_| WatcherError::ChannelClosed)
    }
}

impl Default for MockHotplugSource {
    fn default() -> Self {
        Self::new()
    }
}

impl HotplugSource for MockHotplugSource {
    fn subscribe(&mut self) -> Result<HotplugReceiver, WatcherError> {
        let rx = self.rx.take().ok_or(WatcherError::AlreadySubscribed)?;
        Ok(HotplugReceiver(rx))
    }

    fn set_coalesce_window(&mut self, _window: Duration) {
        // No-op: the mock relays pre-coalesced bursts.
    }
}

// -------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::event::{EventSource, HotplugEvent, HotplugKind, ScsiSubsystem};
    use std::path::PathBuf;
    use std::time::Instant;

    fn singleton_burst(path: &str) -> Coalesced {
        Coalesced::singleton(&HotplugEvent {
            kind: HotplugKind::Added,
            source: EventSource::Scsi(ScsiSubsystem::ScsiGeneric),
            sysfs_path: Some(PathBuf::from(path)),
            device_node: None,
            at: Instant::now(),
        })
    }

    #[test]
    fn inject_then_subscribe_delivers_burst() {
        let mut src = MockHotplugSource::new();
        let mut rx = src.subscribe().expect("subscribe");
        src.inject(singleton_burst("/sys/sg7")).expect("inject");

        let item = rx.blocking_recv().expect("item delivered");
        let burst = item.expect("Ok burst");
        assert_eq!(burst.raw_event_count, 1);
        assert!(burst.touched_paths.iter().any(|p| p.ends_with("sg7")));
    }

    #[test]
    fn subscribe_twice_returns_already_subscribed() {
        let mut src = MockHotplugSource::new();
        src.subscribe().expect("first subscribe");
        let err = src.subscribe().expect_err("second subscribe should fail");
        assert!(matches!(err, WatcherError::AlreadySubscribed));
    }

    #[test]
    fn dropping_receiver_makes_inject_return_channel_closed() {
        let mut src = MockHotplugSource::new();
        let rx = src.subscribe().expect("subscribe");
        drop(rx);
        let err = src
            .inject(singleton_burst("/sys/sg7"))
            .expect_err("inject into closed channel should fail");
        assert!(matches!(err, WatcherError::ChannelClosed));
    }

    #[test]
    fn source_closed_is_delivered_as_final_item() {
        let mut src = MockHotplugSource::new();
        let mut rx = src.subscribe().expect("subscribe");

        src.inject(singleton_burst("/sys/sg7"))
            .expect("inject burst");
        src.inject_source_closed().expect("inject terminal");

        // First item: the burst.
        let first = rx.blocking_recv().expect("first item present");
        first.expect("first item is Ok burst");

        // Second item: the terminal SourceClosed.
        let second = rx.blocking_recv().expect("second item present");
        let err = second.expect_err("second item is Err");
        assert!(matches!(err, WatcherError::SourceClosed));

        // Drop the sender → receiver returns None on next recv.
        drop(src);
        assert!(
            rx.blocking_recv().is_none(),
            "after sender drop, recv returns None"
        );
    }
}
