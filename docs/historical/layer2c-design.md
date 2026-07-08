# Layer 2c Design: Hot-plug Watcher

**Status:** Draft for review. Sister documents: `docs/layer2-design.md`
(Layer 2a — discovery), `docs/layer2b-design.md` (Layer 2b — state-
changing operations). Spec reference: `docs/spec-v0.3.md` §3.2, §6.3,
§9.0a.

---

## 1. Scope

### Goals

- Subscribe to operating-system hot-plug events that touch SCSI tape
  devices (drives) and medium changers (libraries).
- Emit a stream of normalised events that the daemon can consume to
  decide when to re-derive its in-memory model.
- Stay strictly **notification-only**. The watcher itself never calls
  `discover()`, never holds a `LibraryHandle`, never mutates state.
  It tells the consumer "something changed"; the consumer chooses
  whether and how to react.
- Coalesce bursty hot-plug storms (a single cable reseat can fire
  dozens of `add`/`change`/`remove` events within seconds) so the
  consumer is not driven into a re-discovery loop.
- Keep the public surface **OS-agnostic** behind a `HotplugSource`
  trait, with a Linux implementation (`udev` backend) in tree. A
  Windows implementation drops in later behind the same trait.
- Be testable without root, without real hot-plug hardware, and
  without a running udev daemon — via a mock source the test suite
  drives directly.

### Non-goals (for this doc)

- **Reacting to events.** That's the daemon's (Layer 5's) decision
  based on which libraries it owns, what's currently in-flight, and
  whether the human asked for a refresh just now. The watcher is the
  event source; the consumer is the policy.
- **Cartridge inventory change detection.** Hot-plug events tell us
  about *device* presence, not about cartridges moving in or out of
  slots. Inventory-change detection requires periodic or operator-
  triggered `LibraryHandle::refresh()`, already in place.
- **State-changing or read operations.** Layer 2c is a sibling to 2a
  and 2b, not a replacement.
- **Windows or macOS implementations.** The trait surface admits them;
  no code in this layer commits to a Linux-only abstraction. But the
  watcher backend shipped in v0.3 is `LinuxUdevSource` only. macOS is
  off the table per spec §2.2.

---

## 2. Background — what 2a/2b leave for 2c

Layer 2a's `discover()` is a one-shot pure function: caller asks,
function walks `/dev/sg*` + sysfs + issues INQUIRY / RES, returns a
`DiscoveryReport`. The Layer 2a doc explicitly defers refresh-on-
event to "Layer 2c" (`docs/layer2-design.md` §9):

> `remanence-library::watch` (Layer 2c) — wraps udev's netlink socket,
> filters for `subsystem == "scsi_generic"` and `subsystem == "scsi_tape"`
> (both are observed because hot-plug events for a drive can arrive on
> either subsystem first), and emits a stream of events. The daemon
> subscribes to that stream and triggers a fresh `discover()` whenever
> something changes.

Layer 2b adds state-changing ops to a `LibraryHandle` and surfaces a
`refresh()` (read-only RES + reconcile, marks snapshot dirty on shape
mismatch) and `rescan()` (INIT + refresh, hard error on shape
mismatch). These give the consumer two cheap "I want to re-derive
from hardware" entry points.

So 2c's job is narrow: *notice that hardware changed, hand the
consumer a structured event, let it decide between `refresh`,
`rescan`, full `discover`, or "do nothing right now."*

The status quo without 2c: discovery runs on process startup and on
explicit `LibraryService.Refresh` invocations (per spec §6.3
"Today"). Cartridge moves done by an operator outside rem — front-
panel button, web UI, dwara2 on the LTO-7 partition — are invisible
until the next manual refresh. 2c closes this gap for *device-level*
changes (cable reseat, drive replacement, library power cycle). 2c
does **not** close the cartridge-inventory gap (a `MOVE MEDIUM` done
by another initiator does not generate a hot-plug event — see §10).

---

## 3. Domain model

### 3.1 `HotplugEvent`

The single event type the watcher emits. Subsystem-tagged so the
consumer can decide whether a given event matters to its allowlist.

```rust
#[derive(Debug, Clone)]
pub struct HotplugEvent {
    /// What kind of change.
    pub kind: HotplugKind,
    /// Which OS subsystem produced the event. Today: `Scsi(ScsiSubsystem)`.
    /// Reserved for future platforms with different taxonomies.
    pub source: EventSource,
    /// Best-effort path to the affected device. On Linux this is the
    /// sysfs path (e.g. `/sys/devices/.../H:C:I:L`). May be absent for
    /// `Remove` events where the kernel pruned the sysfs node before
    /// we read it.
    pub sysfs_path: Option<PathBuf>,
    /// Best-effort `/dev/sgN` (Linux) or analogous device-node path,
    /// if resolvable at event time. Often `None` for `Remove`.
    pub device_node: Option<PathBuf>,
    /// Monotonic clock reading at event observation, for coalescing.
    pub at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HotplugKind {
    Added,
    Removed,
    Changed,
}

// Note: `EventSource` does NOT derive `Copy` because `ScsiSubsystem`
// owns an allocated `String` in its `Other` variant.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventSource {
    Scsi(ScsiSubsystem),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScsiSubsystem {
    ScsiGeneric,
    ScsiTape,
    /// `scsi_changer` doesn't exist as a Linux subsystem name in
    /// practice — the changer arrives as `scsi_generic` with a
    /// `TYPE` attribute of `8`. We keep the enum extensible for
    /// any other kernel subsystem name; `String` (not `&'static str`)
    /// so the Linux source can pass through whatever the kernel
    /// reports at runtime.
    Other(String),
}
```

The consumer correlates `sysfs_path` and `device_node` against the
device addresses it remembers from its last `discover()` snapshot.
A path that no longer exists implies the device is gone; a path
that didn't exist before implies new hardware.

### 3.2 `HotplugSource` trait

```rust
pub trait HotplugSource: Send {
    /// Begin streaming events. The returned receiver wraps a
    /// `tokio::sync::mpsc::Receiver<Result<Coalesced, WatcherError>>`.
    fn subscribe(&mut self) -> Result<HotplugReceiver, WatcherError>;

    /// Set a coalescing window. Events arriving within `window` of
    /// the previous emission are collapsed into a single `Coalesced`
    /// event with a count + union of affected paths. Default 500ms.
    /// Pass `Duration::ZERO` to disable coalescing.
    fn set_coalesce_window(&mut self, window: Duration);
}
```

`HotplugReceiver` wraps `tokio::sync::mpsc::Receiver<Result<Coalesced,
WatcherError>>`. Each receive yields one of three things:

| Outcome | Meaning |
|--|--|
| `Some(Ok(burst))` | Normal coalesced burst. Most calls. |
| `Some(Err(WatcherError::SourceClosed))` | The underlying OS source terminated. This is the **last item** the receiver will yield; subsequent `recv()` returns `None`. Consumer must rebuild the source from scratch. |
| `None` | Source task exited without an explicit terminal marker (e.g. consumer dropped the receiver, daemon shutdown). |

We expose this same shape from both `recv().await` and `blocking_recv()`
so synchronous test code and the eventual sync `rem watch` CLI can both
work without a tokio runtime context.

### 3.3 What "coalescing" means

Bursts are real. Reseating a SAS cable on a 4-drive library fires
roughly:

```
add scsi_host
add scsi_target (x4)
add scsi_device (x4)
add scsi_generic sg7 sg8 sg9 sg10
add scsi_tape   st0  st1  st2  st3
change …
```

Inside maybe 300ms. If the watcher fires the consumer for every
event, the consumer either does N+1 redundant `discover()`s or has
to implement its own debouncing. Better: the watcher coalesces.

The coalescing rule:

- An incoming event starts a timer of `coalesce_window` duration.
- Further events received while the timer is running join the same
  burst (and reset the timer — sliding window, not fixed).
- When the timer expires, the watcher emits a *single* `Coalesced`
  event summarising what happened:

```rust
pub struct Coalesced {
    /// Number of raw OS events collapsed into this burst.
    pub raw_event_count: usize,
    /// Subsystems that contributed events.
    pub subsystems: BTreeSet<ScsiSubsystem>,
    /// Union of `kind` values seen.
    pub kinds: BTreeSet<HotplugKind>,
    /// All sysfs paths the kernel mentioned. Best-effort; a path
    /// may appear here even if it has already been pruned by the
    /// time the consumer reads.
    pub touched_paths: BTreeSet<PathBuf>,
    /// First raw event in this burst.
    pub first_at: Instant,
    /// Last raw event in this burst (window-reset instant).
    pub last_at: Instant,
    /// Set if at least one raw event in the burst arrived without
    /// a usable sysfs path or device node — the most common case
    /// being a `Removed` event where the kernel pruned the sysfs
    /// node before our handler read it. When this is set,
    /// `touched_paths` is necessarily incomplete and the consumer
    /// must NOT filter by allowlist alone (see consumer guidance
    /// below).
    pub has_unknown_scope: bool,
}
```

Consumer guidance per burst:

- If `has_unknown_scope == false` and `touched_paths` intersects the
  daemon's allowlist of library sysfs prefixes: refresh the matched
  libraries.
- If `has_unknown_scope == false` and no path matches the allowlist:
  ignore the burst (it's about hardware the daemon doesn't own).
- If `has_unknown_scope == true`: **conservatively refresh every
  library the daemon owns.** A path-less Remove event could have been
  about an allowlisted library, and silently filtering it out by "no
  matching path" is a real bug. The cost (one extra refresh per
  library) is small compared to missing a removal event.

Either way, *one* downstream decision per burst, not N.

Default window: 500ms. Tested in Layer 7's QA pass; tuneable per
deployment if a particular hardware platform burst-fires slower.

### 3.4 What's *not* in the event

- **Vendor / product / serial.** Hot-plug events name a path; you
  need an INQUIRY round-trip to learn the device's identity. That's
  the consumer's job via `discover()`, not the watcher's. Embedding
  identity in the event would tempt the consumer to skip
  re-discovery — bad idea, because device renumbering can happen
  invisibly through a single `change` event.
- **Library-allowlist scope filtering.** The watcher delivers every
  event matching the subsystem filter; the daemon checks each
  burst's `touched_paths` against its allowlist. Pushing allowlist
  knowledge into the watcher would couple this layer to daemon
  configuration; better to keep the watcher dumb.

---

## 4. Linux implementation

### 4.1 Choice of udev wrapper crate

Three real options:

| Crate | Async | Maintenance | Pros | Cons |
|--|--|--|--|--|
| `udev` (libudev-rs) | sync | active | thin libudev binding, stable | sync API forces a dedicated thread |
| `tokio-udev` | async | active | tokio-native Stream API, builds on `udev` | `AsyncMonitorSocket` is `!Send` — still needs a dedicated OS thread to own it, just like the sync option (see §4.2 decision note); adds `futures-util` dep surface |
| raw netlink + manual parse | n/a | n/a | no C dep | reimplements libudev's parsing; not worth it |

**Decision: `tokio-udev`** for the event loop, **`std::thread`** for
isolation. `tokio-udev` is a thin layer over `udev` and inherits its
parsing — the C library handles the quirky-event-attribute parsing
we shouldn't be reimplementing. But `tokio_udev::AsyncMonitorSocket`
wraps libudev raw pointers and is `!Send`; it cannot move between
threads at all. So neither `tokio::spawn` (which requires
`Future: Send`) nor a `std::thread::spawn` closure-move can carry
the monitor across thread boundaries. **The monitor is built and
owned inside a dedicated OS thread, which then runs its own
current-thread tokio runtime.** Bursts cross back via the `Send`
mpsc `Sender`.

This is a slightly heavier-weight arrangement than "just spawn a
tokio task" but it is the only one that compiles. See
`crates/remanence-library/src/watch/linux.rs` for the actual
implementation that emerged from live verification on 2026-05-18.

If `tokio-udev` ever becomes unmaintained we drop to `udev` and
keep the same dedicated-thread shape; the trait surface (§3.2)
doesn't change.

### 4.2 The `LinuxUdevSource` implementation

```rust
// !Send constraint forces config-only storage; no monitor here.
pub struct LinuxUdevSource {
    coalesce: Duration,
    subscribed: bool,
}

impl LinuxUdevSource {
    pub fn new() -> Result<Self, WatcherError> {
        // Synchronous probe: builds + drops a sync `udev::MonitorSocket`
        // to verify libudev loads, without touching tokio. The async
        // `AsyncMonitorSocket` is built later, inside the watcher
        // thread's own runtime.
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
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let window = self.coalesce;
        std::thread::Builder::new()
            .name("rem-udev-watcher".into())
            .spawn(move || {
                // Build a current-thread runtime owned by this thread.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap_or_else(|_| std::process::abort());
                rt.block_on(async move {
                    // AsyncMonitorSocket::try_into() registers its fd
                    // with the *current* tokio reactor — must run inside
                    // block_on, not before.
                    let monitor = match build_async_monitor() {
                        Ok(m) => m,
                        Err(e) => { let _ = tx.send(Err(e)).await; return; }
                    };
                    coalescing_loop(monitor, tx, window).await;
                });
            })
            .map_err(|e| WatcherError::SourceUnavailable(format!("spawn: {e}")))?;
        self.subscribed = true;  // Only after spawn succeeds.
        Ok(HotplugReceiver(rx))
    }

    fn set_coalesce_window(&mut self, window: Duration) {
        self.coalesce = window;
    }
}
```

The implementation in tree handles the construction-time runtime
build error path more gracefully (sends an error on the channel
instead of aborting); the abort here is for the doc snippet's
brevity. The principle is the same: the monitor never crosses a
thread boundary.

The `coalescing_loop` is the only piece with non-trivial logic. It
reads from the monitor, applies the §3.3 sliding-window rule, and
sends a `Coalesced` event each time the window expires with at least
one observation pending.

### 4.3 Why both subsystems

Per the spec: a tape drive announces itself on **both** `scsi_generic`
(the raw passthrough device, `/dev/sgN`) and `scsi_tape` (the
character device, `/dev/stN` / `/dev/nstN`). Which one fires first is
kernel-version dependent and not contractual. Subscribing to both
guarantees we see the event regardless of ordering; coalescing
collapses the redundancy.

The changer (medium changer) only appears on `scsi_generic` (Linux
has no `scsi_changer` class today, despite the design's enum hedging
for it). That's fine — `scsi_generic` is enough.

### 4.4 udev daemon availability

In production deployments udev is always running; the daemon is a
foundational systemd service. But two cases require graceful
behaviour:

1. **Containers (LXC, Docker without `--device-cgroup-rule` and
   udev passthrough).** `MonitorBuilder::new()` will return an
   error. The watcher returns
   `WatcherError::SourceUnavailable(msg)` to the consumer; the
   consumer logs a warning and falls back to periodic refresh (a
   daemon configuration choice, not the watcher's). Layer 2c does
   not try to mask the absence.
2. **udev daemon dies mid-session.** `MonitorSocket` event reads
   start returning errors. The coalescing loop closes the channel;
   the consumer's receiver returns `Err(SourceClosed)`. The
   consumer can restart the watcher (rebuild via `LinuxUdevSource::new()`)
   or escalate to operator alert. Layer 2c does not auto-reconnect
   — re-establishing identity on the producer side without a fresh
   `discover()` cycle is harder than just calling `discover()`
   again.

---

## 5. Public API

The crate adds a new module `remanence_library::watch`:

```rust
// remanence-library/src/watch.rs (new)

pub use self::source::{HotplugSource, HotplugReceiver};
pub use self::event::{HotplugEvent, HotplugKind, EventSource, ScsiSubsystem, Coalesced};
pub use self::error::WatcherError;

#[cfg(target_os = "linux")]
pub use self::linux::LinuxUdevSource;

#[cfg(test)]
pub use self::mock::MockHotplugSource;
```

The consumer pattern (this lives in Layer 5 / daemon code, not in
this crate):

```rust
let mut source = LinuxUdevSource::new()?;
source.set_coalesce_window(Duration::from_millis(500));
let mut rx = source.subscribe()?;

while let Some(item) = rx.recv().await {
    let burst = match item {
        Ok(b) => b,
        Err(WatcherError::SourceClosed) => {
            tracing::warn!("hotplug source closed; will not auto-reconnect");
            break;
        }
        Err(e) => {
            tracing::warn!(?e, "unexpected watcher error");
            continue;
        }
    };

    // Conservative refresh policy: if any event had no scope (path-less
    // Remove), refresh every owned library because we can't correlate
    // by path. Otherwise refresh only the matched libraries.
    let targets: Vec<_> = if burst.has_unknown_scope {
        owned_handles.iter_mut().collect()
    } else {
        owned_handles
            .iter_mut()
            .filter(|h| burst_touches(&burst, h))
            .collect()
    };
    for handle in targets {
        if let Err(e) = handle.refresh() {
            tracing::warn!(?e, "refresh after hotplug burst failed");
        }
    }
}
```

### 5.1 `HotplugReceiver`

A newtype wrapping `tokio::sync::mpsc::Receiver<Result<Coalesced,
WatcherError>>`. See §3.2 for the three-state interpretation. We
intentionally don't expose the raw `HotplugEvent` stream — only
coalesced bursts — because emitting raw events past the coalescer
would force every consumer to reimplement debouncing.

Callers that want to *disable* coalescing pass
`set_coalesce_window(Duration::ZERO)`, which short-circuits the
sliding window so every raw event produces its own one-event
`Coalesced` burst. Useful in tests.

### 5.2 Crate re-exports

`remanence-library`'s `lib.rs` adds:

```rust
pub mod watch;
```

No re-export from the crate root; the watcher's public types live
under `watch::`. This keeps `lib.rs` short and signals that the
watcher is an opt-in feature most callers won't touch.

---

## 6. Failure modes

| Failure | Detection | Surfaced as | Recovery |
|--|--|--|--|
| udev daemon unavailable at `new()` | `MonitorBuilder` errors | `WatcherError::SourceUnavailable(msg)` | Consumer falls back to periodic refresh; no auto-retry. |
| `subscribe()` called twice on the same source | internal flag | `WatcherError::AlreadySubscribed` | Build a new source. |
| udev daemon dies mid-session | `MonitorSocket` stream ends or errors | `Some(Err(WatcherError::SourceClosed))` delivered as the **final** item on the receiver, followed by `None` on next recv. Terminal delivery uses `send().await` (not `try_send`) so a momentarily-full channel can't drop the marker. | Consumer rebuilds the source from scratch; coalescer task exits. |
| Coalescing channel backs up (consumer is slow) for normal bursts | `tx.try_send` returns `TrySendError::Full` | Coalescing loop drops the burst and continues draining udev; subsequent bursts proceed normally. **Notifier, not queue.** | The watcher cannot block on `send().await` for bursts because that would stall udev intake and let kernel events back up in the netlink socket. `try_send` + drop-on-full is the only correct shape; the daemon's periodic refresh covers the rare dropped-burst case. (The terminal `SourceClosed` is the documented exception — it uses `send().await` because udev is already closed; no more events to drain.) |
| Coalescing channel disconnected (consumer dropped receiver) | `tx.try_send` returns `TrySendError::Closed` (bursts) or `send().await` returns `Err` (terminal) | Coalescing loop exits silently. | n/a |
| `sysfs_path` and/or `device_node` absent on event | `tokio_udev::Event::syspath()` resolves to an empty path / `devnode()` returns `None` (most common on `Remove` after the kernel pruned the sysfs node) | `HotplugEvent::sysfs_path = None`; coalescer sets `Coalesced::has_unknown_scope = true`. | Consumer must conservatively refresh every owned library when `has_unknown_scope` is set — see §3.3. |
| Window timer fires with zero pending events | shouldn't happen, defensive code | Coalescing loop ignores and resets | n/a |

---

## 7. Implementation plan

| Step | Description |
|--|--|
| 7.1 | Add `tokio-udev` to `remanence-library/Cargo.toml` under `[target.'cfg(target_os = "linux")'.dependencies]`. Keeps non-Linux builds green. |
| 7.2 | `src/watch/event.rs` — the value types from §3 (no I/O). Unit tests for equality, ordering, `Coalesced::touched_paths` set semantics. |
| 7.3 | `src/watch/source.rs` — the `HotplugSource` trait, `HotplugReceiver` newtype. |
| 7.4 | `src/watch/error.rs` — `WatcherError` enum with `thiserror` for source preservation. |
| 7.5 | `src/watch/mock.rs` — `MockHotplugSource` that the test harness drives with `inject(HotplugEvent)`. Implements the same trait. Available behind `#[cfg(test)]` or a `mock` feature. |
| 7.6 | `src/watch/coalesce.rs` — pure-state-machine coalescer: input `(now, event)` → either `None` (still in window) or `Some(Coalesced)` (window expired). No async; testable as a state machine with synthetic timestamps. |
| 7.7 | `src/watch/linux.rs` — `LinuxUdevSource`, the `coalescing_loop` async function that bridges `tokio_udev::AsyncMonitorSocket` and the state machine in 7.6. `#[cfg(target_os = "linux")]`. |
| 7.8 | Integration test (`#[cfg(target_os = "linux")]`, marked `#[ignore]` so CI doesn't run it without explicit opt-in) that subscribes via `LinuxUdevSource` and asserts an event arrives when the test harness `echo add > /sys/.../uevent`s — only works as root. |
| 7.9 | Wire the watcher into the `rem` CLI as `rem watch` — a debugging command that subscribes and pretty-prints bursts to stdout. Useful for live verification on the dev host and on production. |
| 7.10 | Live smoke on QuadStor: hot-add a virtual drive via QuadStor's UI, confirm a burst arrives, confirm `discover()` after the burst sees the new device. Document in JOURNAL with timestamps and event counts. |

Each step is independently testable and ends in green tests +
`cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings`
+ `cargo doc --workspace --no-deps`.

---

## 8. Testing strategy

Three tiers, in order of CI cost:

1. **State machine unit tests** (`coalesce.rs`). Feed the coalescer
   synthetic `(Instant, HotplugEvent)` pairs and assert exactly
   which bursts come out. Covers:
   - Single event → single burst on timer expiry.
   - Two events inside window → one burst with `raw_event_count == 2`.
   - Sliding window: event at t=0, t=300ms, t=600ms with window=500ms
     → burst at t=1100ms (300+800ms from the last event).
   - `Duration::ZERO` window → every event is its own burst.

2. **MockHotplugSource integration**. Drive the trait through the
   mock, assert the consumer logic handles `Coalesced` events
   correctly. Independent of any real udev. Runs in every `cargo
   test` invocation. Covers:
   - Sequence of mock events → expected sequence of bursts.
   - `SourceClosed` propagation.
   - Allowlist filtering at the consumer (logic outside this crate;
     captured here as a docs example only).

3. **Live udev integration** (`#[ignore]` by default). Requires
   root + a Linux host with `udev`. The runbook in `JOURNAL.md`:
   - Subscribe via `rem watch`.
   - In another terminal: `echo change > /sys/class/scsi_generic/sg0/uevent`.
   - Confirm a `Changed` event in the burst.
   - Hot-plug a real device (or QuadStor virtual drive); confirm
     `Added` event.

The mock-based tier is what daily development relies on. The live
tier is the smoke test before each significant Layer 2c change and
before any release.

---

## 9. Interaction with Layer 2b's dirty-state machine

A hot-plug event arriving mid-operation is a real edge case worth
spelling out.

- **`LibraryHandle::move_medium()` is in flight** when the watcher
  fires a burst that touches the same library's changer sysfs path.
  The move's outcome is not affected by the watcher (which doesn't
  call SCSI); but the post-move state of the library may have
  changed for unrelated reasons (another drive came online,
  cartridge removed via IE). The consumer should:
  1. Let the in-flight move complete normally.
  2. After completion, observe the dirty-state of the handle and
     the watcher's burst together: if either reports a change,
     refresh.
- **`refresh()` is in flight** when the watcher fires. The consumer
  may receive the burst on a different task than the one running
  `refresh()`. If the consumer's pattern is "one outstanding
  refresh per handle at a time," it should mark a pending-refresh
  flag and call `refresh()` again after the in-flight one completes.

These are consumer-side policies. The watcher itself is unaware of
in-flight operations.

---

## 10. What hot-plug *doesn't* tell us

Worth being explicit because this is a common misconception:

- **MOVE MEDIUM by another initiator.** Another SCSI initiator on
  the same SAS fabric issuing a MOVE MEDIUM does not generate a
  Linux hot-plug event. The kernel's view of the device hasn't
  changed; only the library's internal element state has. Detection
  requires periodic `refresh()` or operator-triggered `rescan()`.
- **Cartridge insertion via IE port.** The IE port firing inserts
  a cartridge into a slot; the changer notes the inventory change
  internally, but the kernel doesn't generate a hot-plug event.
  Same detection model as above.
- **Tape media errors mid-read.** These surface as SCSI sense data
  on the active CDB; they don't generate hot-plug events.
- **Drive firmware updates.** Generally do not trigger hot-plug
  events; the drive reboots and re-INQUIRYs with the new firmware
  revision. If the SCSI ID changes, that *would* fire a burst.

These belong to the **inventory-change problem**, separate from the
**device-presence problem** that 2c solves. Closing the inventory
gap is a different mechanism: periodic refresh on a configurable
cadence, plus per-operation MOVE-aware patching (already in Layer
2b). 2c does not attempt to address it.

---

## 11. Open questions

1. **Default coalescing window.** 500ms feels right based on
   anecdote (typical cable-reseat burst < 300ms). Will be tuned
   under live observation in step 7.10 and recorded in the
   journal. If real-world bursts span longer, bump default.
2. **`tokio-udev` vs `udev` (sync).** Decision recorded in §4.1 as
   `tokio-udev`. Reconsider if the daemon's async runtime ever
   changes shape (unlikely).
3. **Per-library scoping vs flat event stream.** Today the watcher
   emits a flat stream; the consumer correlates against its
   allowlist of library serials by sysfs path lookup. An
   alternative would have the watcher take an allowlist at
   construction and pre-filter. Lean: keep flat — keeps the
   watcher dumb and avoids coupling to a daemon-side concept.
4. **Recovery from `SourceClosed`.** Today: consumer is responsible
   for rebuilding the source. Reconsider if real-world failures
   suggest auto-reconnect would be safer.
5. **Test-tier coverage of live udev.** Step 7.10 currently relies
   on a manual runbook. Worth automating against QuadStor in CI
   eventually; not for v0.3.

---

## 12. Out of scope

- Windows / macOS hot-plug backends.
- Cartridge-inventory change detection (see §10).
- Auto-refresh policy. The watcher emits events; the consumer
  decides whether and how to refresh.
- Persistent state. Coalescing state is in-memory only. After a
  daemon restart, the watcher starts fresh.
- Filtering on vendor / product / serial. The watcher uses kernel
  subsystem only; identity discrimination is the consumer's job.
- Reacting to media-removal-prevented states or sense data — those
  are operational concerns, not hot-plug concerns.

---

*End of design v0.1. Comments and corrections welcome — please
annotate inline rather than rewriting.*
