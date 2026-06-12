//! Watcher error type. See `docs/layer2c-design.md` §6.

use thiserror::Error;

/// Errors a [`HotplugSource`](super::source::HotplugSource) may return.
#[derive(Debug, Error)]
pub enum WatcherError {
    /// The OS event source (e.g. udev) is not available. On Linux this
    /// is most commonly seen inside containers without udev passthrough.
    /// The consumer should fall back to periodic refresh.
    #[error("OS hot-plug event source unavailable: {0}")]
    SourceUnavailable(String),

    /// [`subscribe`](super::source::HotplugSource::subscribe) was
    /// called twice on the same source. Build a fresh source to
    /// resubscribe.
    #[error("source already subscribed; build a new source to resubscribe")]
    AlreadySubscribed,

    /// The underlying event source closed mid-session (e.g. the
    /// udev daemon died). Delivered as the final item on the
    /// receiver. The consumer is responsible for rebuilding the
    /// source from scratch; the watcher does not auto-reconnect.
    #[error("event source closed mid-session")]
    SourceClosed,

    /// The watcher's internal channel was closed because the
    /// receiving consumer was dropped. Should be invisible in
    /// well-behaved code paths.
    #[error("internal channel closed")]
    ChannelClosed,
}
