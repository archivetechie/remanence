//! Trait surface for hot-plug event sources, plus the
//! [`HotplugReceiver`] newtype. See `docs/layer2c-design.md` §3.2 / §5.

use std::time::Duration;

use tokio::sync::mpsc;

use super::error::WatcherError;
use super::event::Coalesced;

/// Channel handle delivered to the consumer by
/// [`HotplugSource::subscribe`]. Wraps
/// `tokio::sync::mpsc::Receiver<Result<Coalesced, WatcherError>>`,
/// which supports both `recv().await` (inside a tokio runtime) and
/// `blocking_recv()` (outside one — useful in tests and synchronous
/// CLI tools).
///
/// Each receive yields:
/// - `Some(Ok(burst))` — normal coalesced burst.
/// - `Some(Err(WatcherError::SourceUnavailable(_)))` — the OS event
///   source could not be initialized inside the source task. The
///   consumer should fall back to periodic refresh or build a fresh
///   source after the environment changes.
/// - `Some(Err(WatcherError::SourceClosed))` — the underlying event
///   source terminated (e.g. udev daemon died). This is the **last
///   item** the receiver will yield; after it, `recv()` returns
///   `None`. The consumer should rebuild the source from scratch if
///   it wants to resume hot-plug detection.
/// - `None` — the source task exited without an explicit
///   `SourceClosed` marker (e.g. consumer dropped the receiver
///   first, or daemon shutting down). No further items.
///
/// Drop the receiver to signal that the consumer is done; the source
/// task will exit on its next send attempt.
#[derive(Debug)]
pub struct HotplugReceiver(pub(crate) mpsc::Receiver<Result<Coalesced, WatcherError>>);

impl HotplugReceiver {
    /// Receive the next item, awaiting if necessary. See
    /// [`HotplugReceiver`] for the three-state result interpretation.
    /// Use from async contexts.
    pub async fn recv(&mut self) -> Option<Result<Coalesced, WatcherError>> {
        self.0.recv().await
    }

    /// Receive the next item, blocking the current thread until one
    /// is available or the source terminates. Must be called from
    /// **outside** a tokio runtime — tokio's mpsc panics if blocking
    /// is attempted from inside one.
    pub fn blocking_recv(&mut self) -> Option<Result<Coalesced, WatcherError>> {
        self.0.blocking_recv()
    }

    /// Try to receive without blocking. Returns `Err` (TryRecvError)
    /// if no item is currently available or if the source has
    /// terminated.
    pub fn try_recv(
        &mut self,
    ) -> Result<Result<Coalesced, WatcherError>, mpsc::error::TryRecvError> {
        self.0.try_recv()
    }
}

/// An OS event source that produces coalesced hot-plug bursts.
///
/// Implementations:
/// - [`super::mock::MockHotplugSource`] — pure in-memory; tests inject
///   bursts directly.
/// - `super::linux::LinuxUdevSource` (Linux only, behind the
///   `linux-udev` Cargo feature) — wraps `tokio-udev`.
pub trait HotplugSource: Send {
    /// Begin streaming bursts. Returns a [`HotplugReceiver`] that
    /// will yield bursts until the source terminates.
    ///
    /// May be called at most once per source; subsequent calls return
    /// [`WatcherError::AlreadySubscribed`].
    fn subscribe(&mut self) -> Result<HotplugReceiver, WatcherError>;

    /// Set the coalescing window. Events arriving within `window` of
    /// the previous emission are collapsed into a single burst
    /// (sliding window — each new event resets the timer). Pass
    /// [`Duration::ZERO`] to disable coalescing entirely; each raw
    /// event then becomes its own one-event burst.
    ///
    /// Must be called *before* [`Self::subscribe`]; calling after has
    /// no effect on the running source.
    fn set_coalesce_window(&mut self, window: Duration);
}
