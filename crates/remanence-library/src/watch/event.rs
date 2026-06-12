//! Value types for hot-plug events. See `docs/layer2c-design.md` §3.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

/// One raw hot-plug event from the OS.
///
/// The watcher coalesces these into [`Coalesced`] bursts before delivery
/// to the consumer; this type is exposed primarily for unit tests and
/// the mock source. Production consumers receive [`Coalesced`].
#[derive(Debug, Clone)]
pub struct HotplugEvent {
    /// What kind of change.
    pub kind: HotplugKind,
    /// Which OS subsystem produced the event.
    pub source: EventSource,
    /// Best-effort path to the affected device. On Linux this is the
    /// sysfs path (e.g. `/sys/devices/.../H:C:I:L`). May be `None` for
    /// `Remove` events where the kernel pruned the sysfs node before
    /// we read it.
    pub sysfs_path: Option<PathBuf>,
    /// Best-effort device-node path (Linux: `/dev/sgN`, `/dev/nstN`)
    /// if resolvable at event time. Often `None` for `Remove` events.
    pub device_node: Option<PathBuf>,
    /// Monotonic clock reading at observation time. Used by the
    /// coalescer for sliding-window decisions.
    pub at: Instant,
}

/// The kind of device change a hot-plug event reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HotplugKind {
    /// Device appeared.
    Added,
    /// Device disappeared.
    Removed,
    /// Attributes of a present device changed (e.g. mode-page state,
    /// re-INQUIRY result). Does not imply presence change.
    Changed,
}

/// Which OS event source produced the event. Reserved as an enum so
/// non-SCSI sources can be added on other platforms without breaking
/// the trait surface.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventSource {
    /// A Linux SCSI subsystem netlink event.
    Scsi(ScsiSubsystem),
}

/// Which SCSI subsystem the event arrived on. Layer 2c subscribes to
/// `scsi_generic` and `scsi_tape` — a tape drive announces itself on
/// either depending on kernel version, and the changer arrives on
/// `scsi_generic` only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScsiSubsystem {
    /// `/sys/class/scsi_generic/sgN` — the SCSI generic passthrough.
    ScsiGeneric,
    /// `/sys/class/scsi_tape/stN` — the SSC tape character device.
    ScsiTape,
    /// Catch-all for kernel subsystems we don't yet recognise.
    /// Owns its name so the Linux source can pass through whatever
    /// the kernel reported.
    Other(String),
}

/// One coalesced burst of raw events. The watcher emits exactly this
/// type to consumers (raw [`HotplugEvent`]s never cross the channel).
#[derive(Debug, Clone)]
pub struct Coalesced {
    /// Number of raw OS events collapsed into this burst.
    pub raw_event_count: usize,
    /// Subsystems that contributed events. Sorted for deterministic
    /// printing.
    pub subsystems: BTreeSet<ScsiSubsystem>,
    /// Union of `kind` values observed in the burst.
    pub kinds: BTreeSet<HotplugKind>,
    /// All sysfs paths the kernel mentioned across the burst.
    /// Best-effort; a path may appear here even if it has already
    /// been pruned by the time the consumer reads.
    pub touched_paths: BTreeSet<PathBuf>,
    /// Observation time of the first raw event in the burst.
    pub first_at: Instant,
    /// Observation time of the last raw event in the burst (the
    /// instant the coalescer's sliding window last reset).
    pub last_at: Instant,
    /// Set if at least one raw event in the burst arrived without a
    /// usable `sysfs_path` or `device_node`. The most common case is
    /// a `Removed` event where the kernel pruned the sysfs node
    /// before our handler read it. **When this flag is set, the
    /// consumer cannot reliably correlate the burst against an
    /// allowlist by paths alone** — it must conservatively
    /// refresh/rescan every library it owns (or rebuild discovery
    /// outright). Filtering "no path means not mine" is a bug.
    pub has_unknown_scope: bool,
}

impl Coalesced {
    /// Construct a single-event burst directly. Useful for tests
    /// against the mock source and for the `ZERO` window case where
    /// each event becomes its own burst.
    pub fn singleton(ev: &HotplugEvent) -> Self {
        let mut subsystems = BTreeSet::new();
        match &ev.source {
            EventSource::Scsi(s) => {
                subsystems.insert(s.clone());
            }
        }
        let mut kinds = BTreeSet::new();
        kinds.insert(ev.kind);
        let mut touched = BTreeSet::new();
        if let Some(p) = &ev.sysfs_path {
            touched.insert(p.clone());
        }
        if let Some(p) = &ev.device_node {
            touched.insert(p.clone());
        }
        let has_unknown_scope = ev.sysfs_path.is_none() && ev.device_node.is_none();
        Self {
            raw_event_count: 1,
            subsystems,
            kinds,
            touched_paths: touched,
            first_at: ev.at,
            last_at: ev.at,
            has_unknown_scope,
        }
    }
}
