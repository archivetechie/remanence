//! Remanence Layer 2 — logical-library discovery, identity, and
//! policy-gated handles.
//!
//! Discovery is read-only and returns a [`DiscoveryReport`]. The
//! handle type that gates state-changing operations behind an
//! [`AccessPolicy`] check and live identity revalidation against the
//! changer's VPD 0x80 serial is `LibraryHandle`, which lands in
//! `docs/layer2-design.md` §7.6 alongside the orchestrating
//! `discover()` entry point.
//!
//! This crate currently exposes the value types (`Library`,
//! `DriveBay`, `InstalledDrive`, `Slot`, `IePort`, `ElementLayout`,
//! `ElementException`) and the pure `Library::from_captures(...)` builder. The discovery
//! orchestration that issues live SCSI calls and walks sysfs lands
//! incrementally — see `docs/layer2-design.md` §7.

#![warn(missing_docs)]

pub use remanence_scsi as scsi;

pub mod block_io;
pub mod discovery;
pub mod error;
pub mod handle;
pub mod model;
pub mod ops;
pub mod physical_io;
pub mod sysfs;
pub mod transport;
pub mod watch;

pub use block_io::{
    BlockRead, BlockSink, BlockSource, DriveHandleSink, DriveHandleSource, FileBlockSink,
    FileBlockSource, VecBlockSink, VecBlockSource, VecBlockSourceCall,
};
#[cfg(target_os = "linux")]
pub use discovery::discover;
pub use discovery::discover_with;
pub use error::{
    AuditEvent, AuditOp, AuditOutcome, DiscoveryError, DiscoveryWarning, DriveOpError, IoErrorKind,
    LoadError, MoveError, OpenError, RescanError, RescanWarning, UnloadError,
};
pub use handle::tape_io::{
    BlockSize, ComputedPosition, DevicePositionProof, DriveErrorCounters, MediaFamily,
    MediaReadiness, PipelinedReadDiagnostics, PipelinedWriteDiagnostics, PositionAfter,
    ReadBatchOutcome, ReadBuffer, ReadBufferHandoff, ReadDelivery, ReadHandoffOutcome,
    ReadTerminalFlags, SequencedHandoff, SpaceKind, SpaceResult, TapeConfig, TapeIoError,
    TapePosition, WormMediaState, WriteBatchOutcome, WriteFilemarksOutcome, WriteOutcome,
    WriteUnpositionedOutcome,
};
pub use handle::{
    ChangerHandle, DirtyCause, DriveHandle, LibraryHandle, RemovalLockGuard, TapeIoRuntimeConfig,
    DEFAULT_TAPE_IO_BATCH_BLOCKS, DEFAULT_TAPE_IO_POSITION_CHECK_BYTES,
    DEFAULT_TAPE_IO_STAGING_RING_BUFFERS, MAX_TAPE_IO_STAGING_RING_BUFFERS,
    MIN_TAPE_IO_STAGING_RING_BUFFERS,
};
pub use model::{
    resolve_load_target, AccessPolicy, DeviceCaptures, DiscoveryReport, DriveBay, ElementException,
    ElementLayout, IdentitySource, IePort, InstalledDrive, Library, LoadPlan, Slot,
    StaticAllowlist,
};
pub use ops::{apply_move, MovePatch};
pub use physical_io::{
    DriveHandlePhysicalSource, PhysicalFilemarkSpace, PhysicalReadOutcome, PhysicalTapePosition,
    PhysicalTapeSource,
};
pub use remanence_scsi::decode_sense as decode_scsi_sense;
pub use remanence_scsi::log_sense as drive_log_sense;
pub use remanence_scsi::log_sense::{flag_name as tape_alert_flag_name, TapeAlerts};
pub use remanence_scsi::ScsiError;
pub use sysfs::DeviceAttachment;
#[cfg(target_os = "linux")]
pub use transport::LinuxSgTransport;
pub use transport::{
    FixtureTransport, ForeignDriveTransport, RecordingLog, RecordingTransport, SgTransport,
    TimeoutClass,
};
