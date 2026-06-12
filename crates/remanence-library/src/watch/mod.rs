//! Layer 2c — hot-plug watcher.
//!
//! See `docs/layer2c-design.md` for the full design. This module is
//! notification-only: it emits [`Coalesced`] bursts when OS hot-plug
//! events touch the SCSI subsystems
//! (`scsi_generic` + `scsi_tape` on Linux). Consumers (Layer 5) decide
//! whether to `refresh`, `rescan`, or re-`discover` in response.
//!
//! The watcher does not call SCSI, does not hold a [`LibraryHandle`],
//! and does not mutate state. It is a pure event source.
//!
//! [`LibraryHandle`]: crate::handle::LibraryHandle

pub mod coalesce;
pub mod error;
pub mod event;
pub mod mock;
pub mod source;

/// Linux udev-backed event source. Gated behind the `linux-udev`
/// Cargo feature, which requires `pkg-config` and `libudev-dev`
/// system packages at build time.
#[cfg(all(target_os = "linux", feature = "linux-udev"))]
pub mod linux;

pub use coalesce::Coalescer;
pub use error::WatcherError;
pub use event::{Coalesced, EventSource, HotplugEvent, HotplugKind, ScsiSubsystem};
pub use mock::MockHotplugSource;
pub use source::{HotplugReceiver, HotplugSource};

#[cfg(all(target_os = "linux", feature = "linux-udev"))]
pub use linux::LinuxUdevSource;
