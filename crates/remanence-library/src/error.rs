//! Discovery and handle-acquisition error types.

use std::path::PathBuf;
use thiserror::Error;

/// Fatal errors — discovery cannot produce a meaningful [`super::DiscoveryReport`].
/// Anything per-device / per-library that doesn't doom the whole pass is a
/// [`DiscoveryWarning`] instead.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// Could not enumerate `/dev/sg*` at all — likely wrong privileges.
    #[error("could not enumerate /dev/sg*: {cause}")]
    EnumerationDenied {
        /// Lightweight copy of the enumeration failure.
        cause: IoErrorKind,
    },

    /// The host has no `/dev/sg*` devices, or none classifiable as a tape
    /// library after INQUIRY, or every changer probe failed. The
    /// `warnings` vec carries the per-device diagnostics we collected
    /// during the doomed pass — typically `ScsiError` and
    /// `DeviceUnreachable` entries. The CLI prints these to stderr so
    /// the operator can see *why* discovery returned nothing (most
    /// commonly: missing `CAP_SYS_RAWIO`, surfacing as a per-device
    /// SCSI error on every RES call).
    #[error("no tape libraries reachable on this host ({} warning(s))", warnings.len())]
    NoLibraries {
        /// Per-device / per-library diagnostics gathered during the
        /// pass. Empty if the host genuinely has no `/dev/sg*` at all.
        warnings: Vec<DiscoveryWarning>,
    },

    /// A drive's serial appeared in more than one library's RES response.
    /// Structurally impossible unless firmware is misbehaving or libraries
    /// overlap in their drive-element address ranges.
    #[error("drive serial {serial:?} claimed by multiple libraries: {libraries:?}")]
    SerialAmbiguous {
        /// Serial that matched in more than one library.
        serial: String,
        /// Library serials that claimed it.
        libraries: Vec<String>,
    },
}

/// Cleanly-serialisable replacement for `std::io::Error` inside
/// [`DiscoveryWarning::DeviceUnreachable`]. We deliberately don't store
/// the raw error — the model types are `Clone + PartialEq` value structs
/// (see `docs/layer2-design.md` §3) and `std::io::Error` is neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoErrorKind {
    /// Mapped variant of `std::io::ErrorKind` — string for stability across
    /// Rust versions that grow new variants.
    pub kind: &'static str,
    /// Human-readable message.
    pub message: String,
    /// `errno`, when the error came from the OS.
    pub raw_os_error: Option<i32>,
}

impl From<&std::io::Error> for IoErrorKind {
    fn from(e: &std::io::Error) -> Self {
        Self {
            kind: io_kind_str(e.kind()),
            message: e.to_string(),
            raw_os_error: e.raw_os_error(),
        }
    }
}

impl std::fmt::Display for IoErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.raw_os_error {
            Some(errno) => write!(f, "{}: {} (os error {errno})", self.kind, self.message),
            None => write!(f, "{}: {}", self.kind, self.message),
        }
    }
}

fn io_kind_str(k: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind::*;
    match k {
        NotFound => "NotFound",
        PermissionDenied => "PermissionDenied",
        ConnectionRefused => "ConnectionRefused",
        Interrupted => "Interrupted",
        TimedOut => "TimedOut",
        AlreadyExists => "AlreadyExists",
        InvalidInput => "InvalidInput",
        InvalidData => "InvalidData",
        UnexpectedEof => "UnexpectedEof",
        _ => "Other",
    }
}

/// Non-fatal per-device or per-library issue noticed during a discovery
/// pass. Surfaced via `DiscoveryReport.warnings` so programmatic callers
/// can react (refuse operations on libraries with derived identity,
/// raise alerts on unresolved drives, etc.) without rereading log output.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum DiscoveryWarning {
    /// Could not open or read a `/dev/sg*` that the host advertised.
    DeviceUnreachable { path: PathBuf, source: IoErrorKind },
    /// A SCSI command on a specific device returned an error.
    ScsiError {
        path: PathBuf,
        command: &'static str,
        summary: String,
    },
    /// DVCID+CurData didn't yield identifiers; topology was safely
    /// derivable. Affected drives carry `IdentitySource::Derived` and
    /// operations against them require explicit opt-in.
    DriveMappingDerived {
        library: String,
        method: &'static str,
    },
    /// DVCID failed AND the host:channel topology can't safely
    /// disambiguate (multiple logical libraries share the channel —
    /// e.g., the MSL3040 partitioning case). Affected bays have
    /// `installed = None`.
    DriveMappingUnavailable { library: String },
    /// A tape device's VPD 0x80 serial didn't match any library's
    /// drive bay.
    UnclaimedTape { sg_path: PathBuf, serial: String },
    /// A tape device's VPD 0x80 serial matched more than one drive bay.
    /// Discovery leaves all matching bays without a commandable
    /// `/dev/sgN` binding instead of aborting unrelated libraries.
    DriveSerialAmbiguous {
        sg_path: PathBuf,
        serial: String,
        claimants: Vec<String>,
    },
    /// A library reported a drive bay with a known serial, but no
    /// `/dev/sgN` with that serial was reachable on this host.
    UnresolvedDrive {
        library: String,
        serial: String,
        element_address: u16,
    },
    /// MODE SENSE 1Dh returned a layout that disagreed with the layout
    /// derived from RES page headers. RES wins; this surfaces the
    /// inconsistency to the operator.
    LayoutMismatch { library: String },
    /// A slot or IE port returned a voltag that wasn't trimmable ASCII.
    MalformedVoltag {
        library: String,
        element_address: u16,
    },
}

/// Errors from `Library::open()` (and the equivalent drive-handle path).
#[derive(Debug, Error)]
pub enum OpenError {
    /// `AccessPolicy::allows` returned false for this library's serial.
    #[error("library {serial:?} is not on the access policy allowlist")]
    NotAllowed {
        /// The serial that was refused.
        serial: String,
    },

    /// The cached `/dev/sgN` is gone or could not be opened.
    #[error("device {path:?} is unavailable: {cause:?}")]
    DeviceUnavailable {
        /// The cached changer device path.
        path: PathBuf,
        /// The underlying I/O error, in cloneable form.
        cause: IoErrorKind,
    },

    /// The device at the cached path is not the library we discovered.
    /// Kernel re-enumeration, hot-plug, or cable churn since the last
    /// discovery pass — caller should re-run discovery.
    #[error("identity changed at {path}: expected {expected:?}, got {actual:?}")]
    IdentityChanged {
        /// The cached changer device path.
        path: PathBuf,
        /// What discovery saw.
        expected: String,
        /// What's there now (None if the new device didn't respond to INQUIRY).
        actual: Option<String>,
    },

    /// Drives whose identity is `IdentitySource::Derived` require explicit
    /// policy opt-in via `AccessPolicy::allows_derived_drive_identity`.
    #[error(
        "drive serial {serial:?} comes from a derived mapping that this policy does not allow"
    )]
    DerivedIdentityNotOptedIn {
        /// The drive serial that would have been operated on.
        serial: String,
    },

    /// A SCSI command issued during identity revalidation failed.
    #[error("revalidation failed: {0}")]
    ScsiError(#[from] remanence_scsi::ScsiError),

    // -- Drive-handle-specific (LibraryHandle::open_drive) ------------
    /// `bay_address` doesn't match any drive bay in the library
    /// snapshot. Typo, stale caller input, or address from a
    /// different library. (`LibraryHandle::open_drive` only.)
    #[error("drive bay 0x{addr:04x} is not part of this library")]
    BayNotFound {
        /// The unknown bay address.
        addr: u16,
    },

    /// The drive bay exists, but `installed = None` — Layer 2a
    /// couldn't resolve a drive identity for this bay at discovery.
    /// (`LibraryHandle::open_drive` only.) Re-discover after fixing
    /// the host, or use `rescan()` if firmware reseats the drive.
    #[error("drive bay 0x{addr:04x} has unresolved identity — refuse to open")]
    BayUnresolved {
        /// Element address of the unresolved bay.
        addr: u16,
    },

    /// The drive bay has an `installed.serial` but no `sg_path` was
    /// bound at discovery time — Layer 2a's tape-device join didn't
    /// find a `/dev/sgN` matching this drive's VPD 0x80.
    /// (`LibraryHandle::open_drive` only.)
    #[error("drive bay 0x{addr:04x} (serial {serial:?}) has no /dev/sgN bound — drive-side ops impossible")]
    BayMissingDevice {
        /// Element address of the bay.
        addr: u16,
        /// Recorded drive serial.
        serial: String,
    },
}

// =====================================================================
//  Layer 2b error vocabulary — see docs/layer2b-design.md §4.2
// =====================================================================

/// Errors from `LibraryHandle::move_medium` and other single-CDB
/// changer operations.
#[derive(Debug, Error)]
pub enum MoveError {
    /// The given element address isn't part of this library's snapshot.
    /// Typo, stale CLI input, or address from a different library.
    #[error("element address 0x{addr:04x} is not part of library {library:?}")]
    AddressUnknown {
        /// Library serial whose snapshot was searched.
        library: String,
        /// The unknown element address.
        addr: u16,
    },

    /// Source slot/IE/drive bay has no cartridge.
    #[error("source element 0x{addr:04x} is empty")]
    SourceEmpty {
        /// Element address of the source.
        addr: u16,
    },

    /// Destination is already occupied.
    #[error("destination element 0x{addr:04x} is full")]
    DestinationFull {
        /// Element address of the destination.
        addr: u16,
    },

    /// src == dst.
    #[error("source and destination are the same element 0x{addr:04x}")]
    SameElement {
        /// The duplicated element address.
        addr: u16,
    },

    /// A drive bay involved in the move has `installed = None` — Layer
    /// 2a couldn't resolve the drive's identity at discovery time. We
    /// refuse to operate on it regardless of `loaded_tape`. Re-discover
    /// after fixing the host (cap, drivers, hot-plug), or use
    /// `rescan()` to force a changer re-scan.
    #[error("drive bay 0x{addr:04x} has unresolved identity — refuse to operate")]
    DriveBayUnresolved {
        /// Element address of the unresolved drive bay.
        addr: u16,
    },

    /// A drive bay involved in the move has `installed.is_some()` but
    /// `installed.sg_path.is_none()` — the drive's serial is known but
    /// no `/dev/sgN` was bound to it at discovery time. Only matters
    /// for ops that also need to talk to the drive's own SG node
    /// (composed `load` / `unload`).
    #[error("drive bay 0x{addr:04x} (serial {serial:?}) has no /dev/sgN bound — drive-side ops impossible")]
    DriveBayMissingDevice {
        /// Element address of the bay.
        addr: u16,
        /// Recorded drive serial.
        serial: String,
    },

    /// A drive bay's identity is `Derived` (topology-inferred, not read
    /// inline from RES DVCID) and the [`super::AccessPolicy`] hasn't
    /// opted into derived mappings for this library. Duplicates the
    /// open-time check because operations validate against the current
    /// snapshot, which can be more granular than the handle.
    #[error(
        "drive bay 0x{addr:04x} has derived identity ({serial:?}) and the policy does not allow it"
    )]
    DerivedDriveBay {
        /// Element address of the bay.
        addr: u16,
        /// Drive serial whose identity is derived.
        serial: String,
    },

    /// The SCSI changer returned CHECK CONDITION or some other error.
    /// Sense bytes (when available) are preserved verbatim inside
    /// [`remanence_scsi::ScsiError`].
    #[error("SCSI error during move: {0}")]
    ScsiError(#[from] remanence_scsi::ScsiError),
}

/// Errors from `DriveHandle::unload` / `load`. Thin wrapper today;
/// distinct from [`MoveError`] so composed operations can carry both
/// without conflation.
#[derive(Debug, Error)]
pub enum DriveOpError {
    /// The SCSI drive returned CHECK CONDITION or a transport error.
    #[error("SCSI error on drive: {0}")]
    ScsiError(#[from] remanence_scsi::ScsiError),
}

/// Errors from the composed `LibraryHandle::load`. Each variant names
/// the phase that failed and (in the docstring) the resulting snapshot
/// state — see `docs/layer2b-design.md` §5.1 for the full table.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The requested barcode is not currently visible in any drive bay or slot.
    #[error("requested tape is not present in the library inventory")]
    NotInLibrary,

    /// The tape is present, but every drive bay is occupied.
    #[error("no free drive bay is available")]
    NoFreeDrive,

    /// The changer MOVE MEDIUM phase failed. No drive operation
    /// attempted. Snapshot patch was not applied.
    ///
    /// - **CHECK CONDITION** (device explicitly rejected the CDB):
    ///   physical state unchanged, `is_dirty()` stays `false`.
    /// - **Transport error / driver timeout** (completion
    ///   ambiguous — the cartridge may or may not have moved):
    ///   `is_dirty()` is `true` with cause
    ///   `DirtyCause::CompletionUnknown`. Caller should `refresh()`
    ///   or `rescan()` before assuming the cartridge is still where
    ///   the snapshot says.
    #[error("changer MOVE phase failed: {0}")]
    Move(MoveError),

    /// MOVE MEDIUM succeeded, but opening the drive at its new bay
    /// failed (identity mismatch, missing capability, etc.).
    /// **Snapshot is *patched*** — the cartridge is in the bay now.
    /// `is_dirty()` is `true` with cause `DirtyCause::PartialFailure`.
    /// Caller should `refresh()` and retry the drive LOAD via
    /// `open_drive(...).load()`.
    #[error("MOVE succeeded but drive open failed: {0}")]
    OpenDrive(OpenError),

    /// MOVE succeeded, drive opened, but SSC LOAD returned an error.
    /// **Snapshot is *patched*** — cartridge in bay.
    ///
    /// - **CHECK CONDITION** (drive explicitly rejected LOAD): the
    ///   cartridge is in the bay but unloaded; cause is
    ///   `DirtyCause::PartialFailure`. Operator can retry LOAD
    ///   directly via `open_drive(...).load()` or call `refresh()`.
    /// - **Transport error / driver timeout** (drive may have
    ///   actually loaded — we just lost the status): cause is
    ///   `DirtyCause::CompletionUnknown`. Operator should
    ///   `refresh()` or `rescan()` before re-running LOAD.
    #[error("MOVE succeeded but drive LOAD failed: {0}")]
    DriveLoad(DriveOpError),
}

/// Errors from the composed `LibraryHandle::unload`.
#[derive(Debug, Error)]
pub enum UnloadError {
    /// Opening the drive for UNLOAD failed. No MOVE attempted.
    /// **Snapshot unchanged**; `is_dirty()` stays `false` (no CDB
    /// reached the device).
    #[error("drive open failed: {0}")]
    OpenDrive(OpenError),

    /// Drive UNLOAD CDB failed. No MOVE attempted.
    ///
    /// - **CHECK CONDITION** (drive explicitly rejected UNLOAD):
    ///   cartridge still mechanically held by the drive; snapshot
    ///   unchanged; `is_dirty()` stays `false`. Operator may retry
    ///   directly via `open_drive(...).unload()`; SSC UNLOAD is
    ///   idempotent.
    /// - **Transport error / driver timeout** (drive may have
    ///   ejected the cartridge mechanically even though we lost
    ///   the status): snapshot unchanged but `is_dirty()` is `true`
    ///   with cause `DirtyCause::CompletionUnknown`. Bay state
    ///   (`loaded = true`) can no longer be trusted; refresh before
    ///   the next op.
    #[error("drive UNLOAD failed: {0}")]
    DriveUnload(DriveOpError),

    /// Drive UNLOAD succeeded but the changer MOVE failed.
    ///
    /// - **CHECK CONDITION** (changer rejected MOVE): cartridge
    ///   still in the bay (mechanically released but not physically
    ///   moved). Snapshot's bay-loaded state matches reality;
    ///   `is_dirty()` stays `false`. Operator can retry the MOVE
    ///   phase via `move_medium(bay, dst)` directly.
    /// - **Transport error / driver timeout** (cartridge may have
    ///   moved partway, may be back in the bay, may be in flight):
    ///   `is_dirty()` is `true` with cause
    ///   `DirtyCause::CompletionUnknown`. Refresh / rescan before
    ///   acting on either endpoint.
    #[error("drive UNLOAD succeeded but changer MOVE failed: {0}")]
    Move(MoveError),
}

/// Errors from `LibraryHandle::rescan` (INITIALIZE ELEMENT STATUS +
/// reconciliation).
#[derive(Debug, Error)]
pub enum RescanError {
    /// A SCSI command (INIT ELEMENT STATUS or the follow-up RES)
    /// failed.
    #[error("SCSI error during rescan: {0}")]
    ScsiError(#[from] remanence_scsi::ScsiError),

    /// The post-init RES disagrees with the existing snapshot on
    /// structural shape (drive_count differs, layout addresses
    /// moved). Caller must re-run `discover()` from scratch — the
    /// handle's library is no longer trustworthy.
    #[error("post-init shape disagrees with prior snapshot: {0}")]
    SnapshotMismatch(String),
}

/// Non-fatal observations produced by reconciliation. Both
/// `LibraryHandle::rescan` and `LibraryHandle::refresh` route these
/// through the audit hook as [`AuditEvent::Warning`] events (per
/// `docs/layer2b-design.md` §5.2 / §5.3). They do **not** flow back
/// through the return value of either entry point — the operator-
/// facing surface for both is plain `Result<…>`, and per-bay change
/// detail lives in the audit log.
///
/// The bay-level variants ([`Self::DriveReplaced`],
/// [`Self::DriveAppeared`], [`Self::DriveVanished`]) come out of
/// successful reconciliations where individual bays differ. The
/// library-level [`Self::ShapeMismatch`] is fired by `refresh()` only
/// — its soft-error path (per §5.3) where structural change is
/// surfaced via the audit log and `is_dirty()` rather than a hard
/// error. `rescan()` instead returns
/// [`RescanError::SnapshotMismatch`] for the same condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RescanWarning {
    /// A bay holds a different drive than the prior snapshot recorded
    /// at the same address — operator hot-swapped the drive, or a
    /// firmware reset reassigned identities. The new identity comes
    /// from RES DVCID; any prior host-side fields (sg_path, vendor,
    /// etc.) are dropped.
    DriveReplaced {
        /// Drive-bay element address.
        addr: u16,
        /// Serial recorded by the prior snapshot.
        old_serial: String,
        /// Serial reported by the new RES DVCID.
        new_serial: String,
    },
    /// A bay that had no resolved identity in the prior snapshot now
    /// has one (e.g., initial DVCID was partial; or firmware
    /// recovered). `identity_source` is `DvcidInline`, `sg_path` is
    /// `None`; a full `discover()` is needed to re-bind the drive's
    /// `/dev/sgN`.
    DriveAppeared {
        /// Drive-bay element address.
        addr: u16,
        /// Serial newly reported by RES DVCID.
        serial: String,
    },
    /// A bay that had a resolved identity in the prior snapshot no
    /// longer has one (the new RES DVCID block was absent or
    /// truncated for that bay).
    DriveVanished {
        /// Drive-bay element address.
        addr: u16,
        /// Serial the prior snapshot recorded.
        old_serial: String,
    },
    /// `refresh()` saw structural change — drive/slot/IE counts or
    /// element addresses differ from the prior snapshot. Fired only
    /// on the refresh path (which per §5.3 doesn't escalate); the
    /// rescan path returns [`RescanError::SnapshotMismatch`] for the
    /// same condition.
    ShapeMismatch {
        /// Human-readable description of how the shape differed.
        summary: String,
    },
}

// =====================================================================
//  Audit event vocabulary — docs/layer2b-design.md §6 property 7
// =====================================================================

/// One audit-relevant event observed by the audit hook. The four kinds:
///
/// - [`AuditEvent::Started`] — preflight succeeded; the CDB is about
///   to be issued. Carries the raw CDB bytes.
/// - [`AuditEvent::Finished`] — the CDB returned (success or failure).
///   Carries the [`AuditOutcome`].
/// - [`AuditEvent::Refused`] — preflight refused at the *public op*
///   level. **No CDB issued.** A single event for the whole public
///   op; no `Started` / `Finished` follow.
/// - [`AuditEvent::Warning`] — a non-fatal observation produced by
///   reconciliation (drive replaced, appeared, vanished — see
///   [`RescanWarning`]). Fires between `Started` and `Finished` for
///   `rescan()`, and standalone for `refresh()` (which doesn't fire
///   `Started` / `Finished` itself — it's read-only).
///
/// **Composed operations emit multiple Started/Finished pairs**, one
/// per CDB they issue, every event carrying the same
/// [`AuditEvent::Started::operation`] context. For example,
/// `LibraryHandle::load(slot=0x0400, bay=0x0100)` produces four
/// events: `Started{cdb=0xA5…}`, `Finished{outcome=Success}`,
/// `Started{cdb=0x1B…}`, `Finished{outcome=Success}`, all with
/// `operation = AuditOp::Load { slot, bay }`. The `cdb` bytes (or
/// `cdb[0]`) tell the hook which primitive each pair refers to.
///
/// `LibraryHandle::refresh()` is read-only at the CDB level — it
/// doesn't fire `Started`/`Finished` for its RES — but it **does**
/// fire `Warning` events for reconciliation observations
/// (drive replaced / appeared / vanished) and for the §5.3 soft
/// shape-mismatch path ([`RescanWarning::ShapeMismatch`]). The audit
/// hook stays the operator-visible surface for "something changed on
/// this library," regardless of which entry point surfaced it.
///
/// The flat-enum shape (instead of a struct with an overloaded
/// `phase` field and `Option`-typed payloads) is deliberate: every
/// event has exactly the fields its kind needs, and pattern matching
/// tells the hook what to do without surprise `Option` unwraps.
///
/// The `Send` bound on hooks (where the daemon adds one) is so it can
/// call them from a worker thread, but the borrowed `&'a str` and
/// `&'a [u8]` mean the hook is *synchronous* — it can copy fields it
/// wants to keep but cannot retain the event itself.
#[derive(Debug)]
pub enum AuditEvent<'a> {
    /// Preflight succeeded; the CDB is about to be issued.
    Started {
        /// Library serial the operation targets.
        library_serial: &'a str,
        /// What operation is starting.
        operation: AuditOp,
        /// Raw CDB bytes about to go over SG_IO.
        cdb: &'a [u8],
        /// Wall-clock time at the moment of dispatch.
        at: std::time::SystemTime,
    },
    /// Preflight refused. No CDB ever issued.
    Refused {
        /// Library serial that was targeted.
        library_serial: &'a str,
        /// What operation was attempted.
        operation: AuditOp,
        /// Variant name of the refusing [`MoveError`] (e.g.
        /// `"DriveBayUnresolved"`). Static strings so the audit log
        /// has a stable, low-cardinality tag for filtering.
        reason: &'static str,
        /// Wall-clock time at refusal.
        at: std::time::SystemTime,
    },
    /// CDB returned. `outcome` carries success or failure detail.
    Finished {
        /// Library serial the operation targeted.
        library_serial: &'a str,
        /// What operation finished.
        operation: AuditOp,
        /// What happened.
        outcome: AuditOutcome,
        /// Wall-clock time at completion.
        at: std::time::SystemTime,
    },
    /// Reconciliation observed a per-bay change (drive replaced,
    /// appeared, or vanished) against the prior snapshot. Fires
    /// between `Started` and `Finished` for `rescan()`, and
    /// standalone for `refresh()`. One event per warning — operators
    /// can filter the log for `Warning { warning: DriveReplaced .. }`.
    Warning {
        /// Library serial the operation targeted.
        library_serial: &'a str,
        /// What operation produced this warning. For `rescan()` this
        /// is `AuditOp::Rescan`; for `refresh()` we surface it as
        /// `Rescan` too, since the operator-visible effect (a drive
        /// reconciliation event on a library) is the same.
        operation: AuditOp,
        /// The reconciliation observation.
        warning: RescanWarning,
        /// Wall-clock time at the moment the warning was emitted.
        at: std::time::SystemTime,
    },
}

/// What public operation an audit event belongs to. Composed ops
/// (`Load`, `Unload`, `Export`, `Import`) keep this same variant
/// across every primitive CDB they issue, so the audit log can
/// correlate per-CDB events back to one operator-level request.
///
/// Variants whose destination/source is *resolved at runtime*
/// (`Unload`'s `dst` from the bay's `source_slot`, `Export`/`Import`'s
/// `ie` from the first available IE port) use `Option<u16>`: `None`
/// means "the public op was refused at preflight before resolution
/// completed" — see [`AuditEvent::Refused`].
///
/// `LibraryHandle::refresh()` doesn't fire `Started`/`Finished`
/// itself (the RES is read-only), but it *does* fire `Warning`
/// events for reconciliation observations and for the §5.3 soft
/// shape-mismatch path. Those events carry `operation =
/// AuditOp::Rescan` — operators care about the per-library effect,
/// not which entry point surfaced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOp {
    /// Single MOVE MEDIUM via `LibraryHandle::move_medium`. Both
    /// endpoints are caller-supplied so always concrete.
    Move {
        /// Source element address.
        src: u16,
        /// Destination element address.
        dst: u16,
    },
    /// Composed slot → drive bay: issues MOVE MEDIUM then SSC LOAD.
    /// Both endpoints are caller-supplied.
    Load {
        /// Source slot address.
        slot: u16,
        /// Destination drive bay address.
        bay: u16,
    },
    /// Composed drive bay → slot: issues SSC UNLOAD then MOVE MEDIUM.
    /// `dst` is `None` when the caller passed `None` *and* the bay's
    /// recorded `source_slot` was also `None` — preflight refuses such
    /// requests, and the [`AuditEvent::Refused`] event carries the
    /// `None` here.
    Unload {
        /// Source drive bay address.
        bay: u16,
        /// Destination slot address — `None` for the unresolved /
        /// preflight-refused case.
        dst: Option<u16>,
    },
    /// Composed slot → IE port. `ie` is `None` when no IE port was
    /// available at preflight time.
    Export {
        /// Source slot address.
        slot: u16,
        /// Destination IE port address — `None` when no IE port was
        /// available.
        ie: Option<u16>,
    },
    /// Composed IE port → slot. `ie` is `None` when no IE port was
    /// occupied at preflight time.
    Import {
        /// Source IE port address — `None` when no IE port was
        /// occupied.
        ie: Option<u16>,
        /// Destination slot address.
        slot: u16,
    },
    /// INITIALIZE ELEMENT STATUS + post-init re-RES + reconciliation.
    /// The audited CDB is the INIT; the post-init RES is read-only
    /// and not separately audited.
    Rescan,
    /// PREVENT MEDIUM REMOVAL on the changer.
    LockRemoval,
    /// ALLOW MEDIUM REMOVAL on the changer.
    AllowRemoval,
    /// Single SSC UNLOAD CDB issued directly through `DriveHandle`,
    /// not as the first phase of a composed `Unload`.
    DriveUnload {
        /// Drive bay address whose drive received UNLOAD.
        bay: u16,
    },
    /// Single SSC LOAD CDB issued directly through `DriveHandle`,
    /// not as the second phase of a composed `Load`.
    DriveLoad {
        /// Drive bay address whose drive received LOAD.
        bay: u16,
    },
    /// Layer 3a: SSC REWIND on a tape drive.
    TapeRewind {
        /// Drive bay address whose drive received REWIND.
        bay: u16,
    },
    /// Layer 3a: SSC READ POSITION (long form, service action 6) on a
    /// tape drive. Read-only; never marks the snapshot dirty on
    /// successful completion.
    TapeReadPosition {
        /// Drive bay address whose drive received READ POSITION.
        bay: u16,
    },
    /// Layer 3a: SSC LOG SENSE TapeAlert page on a tape drive.
    TapeReadAlerts {
        /// Drive bay address whose drive received LOG SENSE.
        bay: u16,
    },
    /// Layer 3a: SSC LOCATE(16) on a tape drive — seek to LBA.
    TapeLocate {
        /// Drive bay address whose drive received LOCATE(16).
        bay: u16,
        /// Target logical block address.
        lba: u64,
    },
    /// Layer 3a: SSC SPACE(6) or SPACE(16) on a tape drive — relative
    /// motion by blocks, file marks, sequential file marks, or to
    /// End-of-Data.
    TapeSpace {
        /// Drive bay address whose drive received SPACE.
        bay: u16,
        /// Signed count of units. Negative is backward; ignored for
        /// `SpaceKind::EndOfData`.
        count: i64,
        /// Motion-type code.
        kind: crate::handle::tape_io::SpaceKind,
    },
    /// Layer 3a: SSC READ(6) on a tape drive (variable-block).
    TapeRead {
        /// Drive bay address whose drive received READ.
        bay: u16,
        /// Host buffer length the caller passed (the per-CDB
        /// transfer limit). The drive returns ≤ this many bytes
        /// in variable-block mode.
        len: u32,
    },
    /// Layer 3a: SSC WRITE(6) on a tape drive (variable-block).
    TapeWrite {
        /// Drive bay address whose drive received WRITE.
        bay: u16,
        /// Buffer length the caller passed (the block size the
        /// drive will write).
        len: u32,
    },
    /// One coalesced pipelined fixed-WRITE staging window. Its Started
    /// event is the durable pre-ioctl intent marker for the planned range.
    TapeWriteWindow {
        /// Drive bay receiving the window.
        bay: u16,
        /// Planned WRITE(6) command count.
        command_count: u32,
        /// Planned bytes across all commands.
        bytes: u64,
        /// Record count in the first command's fixed-mode CDB.
        first_records: u32,
        /// Record count in the last command's fixed-mode CDB.
        last_records: u32,
    },
    /// Layer 3a: SSC WRITE FILEMARKS(6) on a tape drive.
    TapeWriteFilemarks {
        /// Drive bay address whose drive received WRITE FILEMARKS.
        bay: u16,
        /// Number of file marks to write. IMMED is always 0.
        count: u32,
    },
    /// Layer 3a: query current block-size + compression + max
    /// block size via MODE SENSE(6) page 0x0F + READ BLOCK LIMITS.
    TapeReadConfig {
        /// Drive bay address whose drive was queried.
        bay: u16,
    },
    /// Layer 3a: write block-size + compression configuration via
    /// MODE SELECT(6) with page 0x0F + block descriptor.
    TapeWriteConfig {
        /// Drive bay address whose drive received MODE SELECT.
        bay: u16,
    },
}

/// What [`AuditEvent::Finished`] carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    /// CDB completed successfully.
    Success {
        /// How long the CDB took (from `Started` to `Finished`).
        duration: std::time::Duration,
        /// Whether the operation patched the in-memory snapshot.
        snapshot_patched: bool,
        /// Whether the library snapshot is now dirty (composed-op
        /// partial success that left the snapshot's
        /// "is-this-fresh?" assumption invalidated). When `true`,
        /// callers should consider calling `refresh()` before
        /// reading the snapshot.
        dirty: bool,
    },
    /// Current-sense RECOVERED ERROR. The command succeeded, while the
    /// original sense is retained for drive-health correlation.
    Recovered {
        /// Command duration.
        duration: std::time::Duration,
        /// Raw current-sense bytes.
        sense: Vec<u8>,
        /// Stable human-readable classification.
        summary: String,
    },
    /// CDB returned CHECK CONDITION or a transport error.
    ScsiError {
        /// Raw sense bytes when the target returned them.
        sense: Option<Vec<u8>>,
        /// Human-readable summary (the `Display` of `ScsiError`).
        summary: String,
        /// Whether the failure left the cached snapshot in a state
        /// that may not match physical reality. Set to `true` when
        /// the CDB is state-changing and the failure mode leaves
        /// completion ambiguous (transport-level error / driver
        /// timeout — the CDB may have executed on the device side
        /// without us getting a clean status back). `false` for
        /// CHECK CONDITION (device explicitly rejected the CDB,
        /// physical state unchanged) and for non-state-changing
        /// ops where dirtiness doesn't apply (PREVENT/ALLOW).
        dirty: bool,
    },
    /// Catch-all for outcomes we don't yet classify (parse error
    /// post-CDB, unexpected snapshot state, etc.). Keeps the audit
    /// log forward-compatible without adding a variant per quirk.
    Other {
        /// Human-readable summary.
        summary: String,
    },
}
