//! Linux udev-backed [`HotplugSource`]. See `docs/layer2c-design.md` §4.
//!
//! Uses `tokio-udev` to subscribe to the `scsi_generic` and `scsi_tape`
//! subsystems. Because `tokio_udev::AsyncMonitorSocket` wraps libudev
//! raw pointers and is `!Send`, the implementation spawns a dedicated
//! **OS thread** (not a tokio task) that builds and owns the monitor
//! inside its own current-thread tokio runtime. Bursts are delivered
//! back to the caller on a tokio mpsc channel; the `Sender` is `Send`
//! even when the monitor is not.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio_udev::{AsyncMonitorSocket, EventType, MonitorBuilder};

use super::coalesce::Coalescer;
use super::error::WatcherError;
use super::event::{Coalesced, EventSource, HotplugEvent, HotplugKind, ScsiSubsystem};
use super::source::{HotplugReceiver, HotplugSource};

/// Udev-backed event source. One per daemon.
///
/// `tokio_udev::AsyncMonitorSocket` wraps raw libudev pointers and is
/// `!Send`, so it cannot be moved between threads. To keep the
/// `HotplugSource` API thread-safe while staying within libudev's
/// single-threaded contract, `LinuxUdevSource` stores only config
/// (not the monitor itself); the monitor is built inside the
/// dedicated watcher thread spawned by [`Self::subscribe`].
///
/// [`Self::new`] does a synchronous probe-and-drop to surface
/// `SourceUnavailable` early, before any tokio runtime work, so the
/// caller can fall back to periodic refresh without ever spinning up
/// a watcher thread.
pub struct LinuxUdevSource {
    coalesce: Duration,
    subscribed: bool,
}

impl LinuxUdevSource {
    /// Build a source filtered for the SCSI subsystems we care about.
    /// Returns [`WatcherError::SourceUnavailable`] if libudev cannot
    /// open a monitor (most commonly inside containers without udev
    /// passthrough).
    ///
    /// Constructor is fully synchronous — it does **not** require a
    /// tokio runtime in the caller's context. The probe builds a
    /// `udev::MonitorSocket` (sync) and drops it; the async
    /// `AsyncMonitorSocket` is built later, inside the watcher
    /// thread's own runtime, because `try_into()` registers the fd
    /// with whichever reactor is current at that moment.
    pub fn new() -> Result<Self, WatcherError> {
        probe_libudev()?;
        Ok(Self {
            coalesce: Duration::from_millis(500),
            subscribed: false,
        })
    }
}

impl HotplugSource for LinuxUdevSource {
    fn subscribe(&mut self) -> Result<HotplugReceiver, WatcherError> {
        if self.subscribed {
            return Err(WatcherError::AlreadySubscribed);
        }

        let (tx, rx) = mpsc::channel(64);
        let window = self.coalesce;
        // libudev's monitor handle holds raw C pointers and is `!Send`,
        // so neither `tokio::spawn` nor a closure-move into
        // `std::thread::spawn` can carry it across thread boundaries.
        // Instead, spawn a dedicated OS thread that builds and owns
        // the monitor INSIDE its own current-thread tokio runtime.
        // The mpsc `Sender` (which is `Send`) crosses the boundary
        // to deliver bursts back to the caller.
        //
        // Only mark this source as `subscribed` after spawn succeeds —
        // a thread-spawn failure (rare, but happens under FD/PID
        // pressure) leaves the source reusable for retry. Codex
        // review 6221138f Low flagged the previous "mark first, spawn
        // second" ordering as a poisoning hazard.
        std::thread::Builder::new()
            .name("rem-udev-watcher".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = tx.blocking_send(Err(WatcherError::SourceUnavailable(format!(
                            "watcher thread runtime: {e}"
                        ))));
                        return;
                    }
                };
                rt.block_on(async move {
                    // AsyncMonitorSocket::try_from() registers its fd
                    // with the *current* tokio reactor — must run
                    // inside block_on, not before.
                    let monitor = match build_async_monitor() {
                        Ok(m) => m,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            return;
                        }
                    };
                    coalescing_loop(monitor, tx, window).await;
                });
            })
            .map_err(|e| WatcherError::SourceUnavailable(format!("spawn watcher thread: {e}")))?;

        self.subscribed = true;
        Ok(HotplugReceiver(rx))
    }

    fn set_coalesce_window(&mut self, window: Duration) {
        self.coalesce = window;
    }
}

/// Synchronous probe: verify libudev loads and the netlink monitor can
/// be opened with the SCSI subsystem filters. Builds a
/// `udev::MonitorSocket` and drops it; never converts to async.
/// Callable from any context, no runtime required.
fn probe_libudev() -> Result<(), WatcherError> {
    let _sync_monitor = MonitorBuilder::new()
        .map_err(|e| WatcherError::SourceUnavailable(format!("MonitorBuilder::new: {e}")))?
        .match_subsystem("scsi_generic")
        .map_err(|e| WatcherError::SourceUnavailable(format!("match scsi_generic: {e}")))?
        .match_subsystem("scsi_tape")
        .map_err(|e| WatcherError::SourceUnavailable(format!("match scsi_tape: {e}")))?
        .listen()
        .map_err(|e| WatcherError::SourceUnavailable(format!("listen: {e}")))?;
    Ok(())
}

/// Build a tokio-aware `AsyncMonitorSocket` filtered for the SCSI
/// subsystems. **Must be called from inside a tokio runtime** —
/// `try_into()` registers the netlink fd with the current reactor.
/// Also: `AsyncMonitorSocket` is `!Send`, so the caller must keep
/// it on the same thread for the rest of its life.
fn build_async_monitor() -> Result<AsyncMonitorSocket, WatcherError> {
    let builder = MonitorBuilder::new()
        .map_err(|e| WatcherError::SourceUnavailable(format!("MonitorBuilder::new: {e}")))?
        .match_subsystem("scsi_generic")
        .map_err(|e| WatcherError::SourceUnavailable(format!("match scsi_generic: {e}")))?
        .match_subsystem("scsi_tape")
        .map_err(|e| WatcherError::SourceUnavailable(format!("match scsi_tape: {e}")))?;
    let sync_monitor = builder
        .listen()
        .map_err(|e| WatcherError::SourceUnavailable(format!("listen: {e}")))?;
    let async_monitor = sync_monitor.try_into().map_err(|e: std::io::Error| {
        WatcherError::SourceUnavailable(format!("AsyncMonitorSocket: {e}"))
    })?;
    Ok(async_monitor)
}

/// Translate a `tokio_udev::Event` into our internal
/// [`HotplugEvent`]. Returns `None` for events whose subsystem we
/// don't care about (defensive — the monitor should already filter).
fn translate(ev: &tokio_udev::Event) -> Option<HotplugEvent> {
    let subsystem = ev.subsystem().and_then(|s| s.to_str()).map(|s| match s {
        "scsi_generic" => ScsiSubsystem::ScsiGeneric,
        "scsi_tape" => ScsiSubsystem::ScsiTape,
        other => ScsiSubsystem::Other(other.to_string()),
    })?;

    let kind = match ev.event_type() {
        EventType::Add => HotplugKind::Added,
        EventType::Remove => HotplugKind::Removed,
        EventType::Change => HotplugKind::Changed,
        // Bind/Unbind/Online/Offline/Move are not interesting for the
        // device-presence question; drop them.
        _ => return None,
    };

    // `syspath()` is best-effort: returns the path even when the
    // kernel has not (yet) pruned the device, which is the usual
    // case for Add/Change. For Remove events the kernel may
    // already have pruned the sysfs node, in which case
    // `to_path_buf()` still returns the path string the udev event
    // carried — but the path may no longer resolve on disk. The
    // coalescer flags bursts with at least one path-less event via
    // `has_unknown_scope` so the consumer doesn't silently filter
    // them out.
    let sysfs_path = {
        let p = ev.syspath().to_path_buf();
        if p.as_os_str().is_empty() {
            None
        } else {
            Some(p)
        }
    };
    let device_node = ev.devnode().map(|p| p.to_path_buf());

    Some(HotplugEvent {
        kind,
        source: EventSource::Scsi(subsystem),
        sysfs_path,
        device_node,
        at: Instant::now(),
    })
}

/// The async loop that drives the coalescer.
///
/// Bursts are delivered via `try_send`, not `send().await`. This is
/// deliberate per `docs/layer2c-design.md` §6: the watcher is a
/// **notifier**, not a queue. If the consumer is slow enough to fill
/// the 64-slot channel, we drop the burst rather than blocking the
/// loop and stalling udev intake. The consumer's periodic refresh is
/// the safety net.
///
/// Runs until either:
/// - the udev monitor stream terminates → flush any pending burst,
///   then attempt to deliver a final `Err(SourceClosed)` and exit;
/// - the consumer drops the receiver → `try_send` returns `Closed`
///   and we exit silently.
async fn coalescing_loop(
    mut monitor: AsyncMonitorSocket,
    tx: mpsc::Sender<Result<Coalesced, WatcherError>>,
    window: Duration,
) {
    use futures_util::StreamExt;

    let mut coalescer = Coalescer::new(window);

    loop {
        let next_tick = coalescer.next_tick_at();
        tokio::select! {
            biased;

            udev_ev = monitor.next() => {
                match udev_ev {
                    Some(Ok(ev)) => {
                        if let Some(translated) = translate(&ev) {
                            let now = translated.at;
                            if let Some(burst) = coalescer.push(translated, now) {
                                if !try_deliver(&tx, burst) {
                                    return;
                                }
                            }
                        }
                    }
                    Some(Err(_)) | None => {
                        // Source closed or errored. Flush any pending
                        // burst, then post the terminal marker so the
                        // consumer can react explicitly.
                        if let Some(burst) = coalescer.flush() {
                            if !try_deliver(&tx, burst) {
                                return;
                            }
                        }
                        // Terminal marker: use awaited `send` rather
                        // than `try_send` so a momentarily-full channel
                        // doesn't silently swallow the
                        // `SourceClosed` signal. We've already exited
                        // udev intake, so blocking here is bounded by
                        // the consumer's drain rate — the only way
                        // this stalls is a permanently-stuck consumer,
                        // which is a bug we'd rather observe than
                        // paper over with a silent terminal-loss.
                        let _ = tx.send(Err(WatcherError::SourceClosed)).await;
                        return;
                    }
                }
            }

            _ = sleep_until_opt(next_tick) => {
                if let Some(burst) = coalescer.tick(Instant::now()) {
                    if !try_deliver(&tx, burst) {
                        return;
                    }
                }
            }
        }
    }
}

/// Try to deliver a burst. Returns `true` to keep the loop alive,
/// `false` to exit (consumer dropped the receiver). Dropping the
/// burst on `Full` is the documented notifier-not-queue behaviour.
fn try_deliver(tx: &mpsc::Sender<Result<Coalesced, WatcherError>>, burst: Coalesced) -> bool {
    match tx.try_send(Ok(burst)) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            // Slow consumer; drop the burst. The consumer's periodic
            // refresh will catch up.
            //
            // (Once `tracing` is wired into the workspace this should
            // emit a `warn!` event; for now, silent drop matches the
            // design contract and keeps the daemon-free crate
            // tracing-free.)
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

/// Sleep until `when`, or forever if `None`. tokio's `sleep_until`
/// requires an `Instant`; `forever` is implemented as a `pending`
/// future that the select branch ignores when nothing else is
/// pending either.
async fn sleep_until_opt(when: Option<Instant>) {
    match when {
        Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
        None => std::future::pending::<()>().await,
    }
}
