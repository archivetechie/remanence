//! `LibraryHandle` + `Library::open(policy)` — the safety scaffold from
//! `docs/layer2-design.md` §5.2 and §7.6.
//!
//! A [`LibraryHandle`] is the *only* type from which Layer 2b will hang
//! state-changing operations. Acquiring one requires, in order:
//!
//! 1. **Policy check.** The provided [`AccessPolicy`] must allow the
//!    library's serial. Discovery surfaces every library on the host;
//!    `open()` is what makes one targetable. (Spec v0.2 §8.2 hard
//!    requirement.)
//!
//! 2. **Derived-identity check.** If any drive bay in the library has
//!    `IdentitySource::Derived` (drive serial inferred from topology
//!    rather than read inline from RES DVCID), the policy must
//!    explicitly opt this library into derived mappings via
//!    [`AccessPolicy::allows_derived_drive_identity`]. Default-denied.
//!
//! 3. **Device open + identity revalidation.** Open the changer's
//!    cached `/dev/sgN` (read/write — see `LinuxSgTransport::open_rw`,
//!    since the handle is the state-changing path) and reissue
//!    *standard* INQUIRY followed by VPD 0x80. The device must
//!    (a) still be a `MediumChanger`, and (b) return the *same*
//!    VPD 0x80 serial we recorded at discovery time. Anything else
//!    (kernel re-enumeration, hot-plug, cable churn, the SG node now
//!    pointing at a tape drive) is [`OpenError::IdentityChanged`] —
//!    the caller should re-run `discover()`.
//!
//! [`LibraryHandle::move_medium`] is the first state-changing
//! operation that hangs off this handle (Layer 2b §7.3). Composed
//! ops (`load` / `unload` / `export` / `import`) and the `rescan` /
//! `refresh` lifecycle follow in subsequent §7.x chunks. Every
//! state-changing operation routes through this handle so the two
//! safety properties (policy gate + identity revalidation) are
//! non-skippable by construction.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Instant, SystemTime};

use remanence_scsi::{
    initialize_element_status as init_cdb, inquiry, move_medium as move_medium_cdb, vpd,
    DeviceType, Inquiry, ScsiError, UnitSerial,
};

use crate::error::{
    AuditEvent, AuditOp, AuditOutcome, DriveOpError, IoErrorKind, LoadError, MoveError, OpenError,
    RescanError, UnloadError,
};
use crate::model::{AccessPolicy, IdentitySource, InstalledDrive, Library};
use crate::ops;
use crate::transport::{SgTransport, TimeoutClass};

// Layer 3a — child module so its `impl DriveHandle { ... }` block
// can see this module's private fields. See `docs/layer3a-design.md`
// §2 for the rationale; codex 97997d71 caught an earlier sibling-
// module attempt that lacked this visibility. Public so the value
// types + error enum re-export cleanly at the crate root.
pub mod tape_io;

/// Audit hook type alias. The hook is called synchronously with a
/// short-lived `&AuditEvent<'_>` reference and may not retain it; it
/// can copy fields it wants to keep. `Send` makes the hook callable
/// from worker threads in the daemon.
type AuditHook = Box<dyn FnMut(&AuditEvent<'_>) + Send>;

/// Transport factory type alias. Stored on [`LibraryHandle`] so
/// [`LibraryHandle::open_drive`] can reuse the same opening
/// mechanism that produced the changer's transport — for tests,
/// the closure routes per-`/dev/sgN` requests to canned fixture
/// transports; for production, it wraps
/// [`crate::transport::LinuxSgTransport::open_rw`]. The `'static`
/// bound is so the closure can outlive the `open_with` call that
/// constructed the handle.
type TransportFactory = Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>;

/// Default fixed-record batch size for tape READ/WRITE CDBs.
pub const DEFAULT_TAPE_IO_BATCH_BLOCKS: u32 = 16;
/// Default number of page-aligned write buffers in the pipelined staging ring.
pub const DEFAULT_TAPE_IO_STAGING_RING_BUFFERS: u32 = 4;
/// Smallest supported staging ring. A depth of one is not a pipeline.
pub const MIN_TAPE_IO_STAGING_RING_BUFFERS: u32 = 2;
/// Largest supported staging ring, bounding per-drive locked working memory.
pub const MAX_TAPE_IO_STAGING_RING_BUFFERS: u32 = 16;
const DEFAULT_TAPE_IO_RECORD_BYTES_FOR_RESERVED_BUFFER: u32 = 256 * 1024;
/// Default arithmetic-position drift tripwire cadence.
pub const DEFAULT_TAPE_IO_POSITION_CHECK_BYTES: u64 = 1024 * 1024 * 1024;

const PIPELINE_HISTOGRAM_UPPER_US: [u64; 12] = [
    10,
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_500,
    5_000,
    10_000,
    50_000,
    u64::MAX,
];

#[derive(Debug, Default)]
struct PipelineHistogram {
    buckets: [u64; PIPELINE_HISTOGRAM_UPPER_US.len()],
    samples: u64,
    max: u64,
}

impl PipelineHistogram {
    fn record(&mut self, sample_us: u64) {
        let bucket = PIPELINE_HISTOGRAM_UPPER_US
            .iter()
            .position(|upper| sample_us <= *upper)
            .unwrap_or(PIPELINE_HISTOGRAM_UPPER_US.len() - 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
        self.max = self.max.max(sample_us);
    }

    fn percentile(&self, numerator: u64, denominator: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let wanted = self
            .samples
            .saturating_mul(numerator)
            .saturating_add(denominator - 1)
            / denominator;
        let mut seen = 0u64;
        for (count, upper) in self.buckets.iter().zip(PIPELINE_HISTOGRAM_UPPER_US) {
            seen = seen.saturating_add(*count);
            if seen >= wanted {
                return if upper == u64::MAX { self.max } else { upper };
            }
        }
        self.max
    }
}

#[derive(Debug, Default)]
struct PipelineDiagnostics {
    gap_us: PipelineHistogram,
    ioctl_us: PipelineHistogram,
    good_commands: u64,
    good_records: u64,
    good_bytes: u64,
    first_submit: Option<Instant>,
    previous_completion: Option<Instant>,
    last_completion: Option<Instant>,
}

struct PendingPipelineAudit {
    operation: AuditOp,
    outcome: AuditOutcome,
}

/// Runtime tape-I/O settings applied when a drive handle is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeIoRuntimeConfig {
    /// Exact legacy behavior: variable-mode single-record I/O and per-block
    /// write-side READ POSITION.
    pub legacy_single_block: bool,
    /// Enable the write-side page-aligned staging ring and hot submitter.
    pub pipelined_submission: bool,
    /// Number of buffers in the write-side staging ring.
    pub staging_ring_buffers: u32,
    /// Requested fixed records per WRITE(6) before sg/HBA clamping.
    pub write_batch_blocks: u32,
    /// Requested fixed records per READ(6) before sg/HBA clamping.
    pub read_batch_blocks: u32,
    /// Bytes advanced between arithmetic-position tripwire READ POSITIONs.
    /// Zero disables mid-stream tripwires.
    pub position_check_bytes: u64,
}

impl Default for TapeIoRuntimeConfig {
    fn default() -> Self {
        Self {
            legacy_single_block: false,
            pipelined_submission: false,
            staging_ring_buffers: DEFAULT_TAPE_IO_STAGING_RING_BUFFERS,
            write_batch_blocks: DEFAULT_TAPE_IO_BATCH_BLOCKS,
            read_batch_blocks: DEFAULT_TAPE_IO_BATCH_BLOCKS,
            position_check_bytes: DEFAULT_TAPE_IO_POSITION_CHECK_BYTES,
        }
    }
}

fn effective_batch_blocks_from_reserved(
    reserved_bytes: u32,
    block_size_bytes: u32,
    requested_batch_blocks: u32,
) -> u32 {
    if block_size_bytes == 0 {
        return 1;
    }
    (reserved_bytes / block_size_bytes).clamp(1, requested_batch_blocks.max(1))
}

/// The library's medium changer plus its inventory snapshot.
///
/// This handle owns the changer transport and issues changer CDBs
/// (`MOVE MEDIUM`, `READ/INITIALIZE ELEMENT STATUS`, and
/// `PREVENT/ALLOW MEDIUM REMOVAL`). It also owns the shared audit and
/// dirty-state cell cloned into drive handles opened by the
/// [`LibraryHandle`] facade.
pub struct ChangerHandle {
    /// Snapshot of the library at the time of `open()`. Re-validated
    /// before this value is constructed; safe to read for operator
    /// display, allowlist comparisons, etc.
    library: Library,
    /// Open transport to the changer's `/dev/sgN`. Stored boxed so the
    /// handle type doesn't carry a transport type parameter; the cost
    /// is one dyn dispatch per CDB, which is dwarfed by the SG_IO
    /// kernel round-trip.
    transport: Box<dyn SgTransport>,
    /// Shared audit hook + dirty state cloned into drive handles.
    /// This lifts the old borrowed `DriveHandle` shape while preserving
    /// one unified audit stream and dirty bit for library + drive ops.
    shared: Arc<Mutex<DriveShared>>,
}

/// A policy-gated, identity-revalidated facade for one library.
///
/// Hold this for the duration of a session against the library. The
/// internal [`SgTransport`] is the *same* transport identity
/// revalidation succeeded on, so subsequent CDBs cannot be sent to a
/// silently-swapped device. If a hot-plug event redirects `/dev/sgN`
/// mid-session, the next CDB through this handle will fail (and the
/// caller should re-discover).
pub struct LibraryHandle {
    /// Pure-changer core: robot transport, inventory snapshot, audit,
    /// and dirty-state handling.
    changer: ChangerHandle,
    /// The transport factory the handle was constructed with. Reused
    /// by [`Self::open_drive`] to open the drive's own `/dev/sgN`
    /// without the caller having to re-supply a factory. Carries a
    /// `'static` bound (see [`TransportFactory`]) so the handle owns
    /// it outright.
    transport_factory: TransportFactory,
}

/// Snapshot-dirty state shared between a library handle and its open
/// drive handles. Mutators are `pub(crate)` so the parent
/// `handle::mod` and the child `handle::tape_io` modules can flip
/// dirty state through [`DriveShared`]; external crates can only read
/// through [`LibraryHandle::is_dirty`] / [`LibraryHandle::dirty_cause`].
#[derive(Debug, Default)]
pub(crate) struct DirtyState {
    /// True when the snapshot is no longer guaranteed to reflect the
    /// physical library — set on partial-failure composed ops, on
    /// `refresh()` shape-mismatch outcomes (per
    /// `docs/layer2b-design.md` §5.1 / §5.3), and on direct
    /// `DriveHandle::*` transport errors (Layer 3a + the
    /// completion-unknown path of Layer 2b's direct
    /// `DriveHandle::{load, unload}`). Cleared by a successful
    /// `refresh()` or `rescan()`.
    is_dirty: bool,
    /// Categorises *why* the snapshot is dirty so an operator-facing
    /// surface (the CLI's recovery hint, the daemon's audit replay)
    /// can pick the right wording without inferring from the error
    /// summary text. `Some(...)` iff `is_dirty` is `true`; the two
    /// are flipped together via [`Self::mark`] / [`Self::clear`].
    cause: Option<DirtyCause>,
}

impl DirtyState {
    /// Flip to dirty with a categorised cause. Pairs `is_dirty` and
    /// `cause` so they can't drift apart.
    pub(crate) fn mark(&mut self, cause: DirtyCause) {
        self.is_dirty = true;
        self.cause = Some(cause);
    }

    /// Clear the dirty state. Used by `refresh()` / `rescan()` on
    /// a successful reconcile.
    pub(crate) fn clear(&mut self) {
        self.is_dirty = false;
        self.cause = None;
    }

    /// Snapshot dirty flag.
    pub(crate) fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    /// Categorised cause; `Some(_)` iff [`Self::is_dirty`] is true.
    pub(crate) fn cause(&self) -> Option<DirtyCause> {
        self.cause
    }
}

/// Mutable state shared between a library handle and every drive
/// handle opened from it. Locks are taken briefly for one audit-fire
/// or dirty-state update and must never be held across CDB execution.
#[derive(Default)]
pub(crate) struct DriveShared {
    pub(crate) audit_hook: Option<AuditHook>,
    pub(crate) dirty: DirtyState,
}

pub(crate) fn lock_drive_shared(shared: &Arc<Mutex<DriveShared>>) -> MutexGuard<'_, DriveShared> {
    shared
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Why a [`LibraryHandle`]'s snapshot is dirty. Returned by
/// [`LibraryHandle::dirty_cause`] so callers can pick a hint /
/// recovery message that matches what actually went wrong.
///
/// The three causes are operationally distinct:
/// - [`Self::PartialFailure`] — a *composed* operation had an
///   earlier CDB succeed and a later CDB fail (e.g. `load`'s MOVE
///   ok, drive LOAD fail). The cartridge moved; the snapshot patch
///   was applied; the next phase didn't run.
/// - [`Self::VendorSemantics`] — a single CDB *succeeded*, but the
///   post-state depends on vendor flavor (the IE-port case: HPE
///   parks visibly, QuadStor vaults). The snapshot's IE-full /
///   slot-full bits can't be trusted without re-reading.
/// - [`Self::CompletionUnknown`] — a state-changing CDB *failed*
///   with a completion-ambiguous transport error (driver timeout,
///   bus reset, host reset). The robot or drive may have actually
///   executed the operation even though we didn't get a clean
///   status back. Also covers `rescan`'s post-INIT failures and
///   `refresh`'s shape-mismatch outcomes: the snapshot is known
///   stale, just from a different mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyCause {
    /// Composed op: earlier CDB succeeded, later CDB failed.
    PartialFailure,
    /// Op succeeded but post-state diverges from the snapshot
    /// model (IE-port flavor; possibly other vendor-specific
    /// surfaces in the future).
    VendorSemantics,
    /// State-changing CDB failed with completion ambiguous, or a
    /// rescan/refresh observed structural divergence from the
    /// cached snapshot. Snapshot must be re-derived.
    CompletionUnknown,
}

impl std::fmt::Debug for ChangerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shared = lock_drive_shared(&self.shared);
        f.debug_struct("ChangerHandle")
            .field("library", &self.library)
            .field("transport", &"<dyn SgTransport>")
            .field(
                "audit_hook",
                &if shared.audit_hook.is_some() {
                    "Some(<FnMut>)"
                } else {
                    "None"
                },
            )
            .field("dirty", &shared.dirty)
            .finish()
    }
}

impl ChangerHandle {
    /// Read-only access to the snapshot the handle was opened against.
    pub fn library(&self) -> &Library {
        &self.library
    }

    /// Install an audit hook. Replaces any previous hook. The hook
    /// fires per state-changing CDB — see [`AuditEvent`] for the
    /// contract.
    pub fn set_audit_hook<F>(&mut self, hook: F)
    where
        F: FnMut(&AuditEvent<'_>) + Send + 'static,
    {
        lock_drive_shared(&self.shared).audit_hook = Some(Box::new(hook));
    }

    /// True iff the cached snapshot is no longer guaranteed to
    /// reflect the physical library. Set by composed-op partial
    /// failures (Layer 2b §5.1) and by `refresh()` shape-mismatch
    /// outcomes (§5.3). Cleared by a successful [`Self::refresh`].
    /// Callers that need a guaranteed-fresh view should `refresh()`
    /// before reading [`Self::library`].
    pub fn is_dirty(&self) -> bool {
        lock_drive_shared(&self.shared).dirty.is_dirty()
    }

    /// Why the snapshot is currently dirty, if it is. `Some(_)`
    /// exactly when [`Self::is_dirty`] is `true`. Lets a CLI / audit
    /// consumer pick the right operator-facing wording (partial
    /// failure vs vendor-semantic divergence vs completion unknown)
    /// instead of inferring from error summary strings.
    pub fn dirty_cause(&self) -> Option<DirtyCause> {
        lock_drive_shared(&self.shared).dirty.cause()
    }

    /// Internal: flip the snapshot to dirty with a categorised
    /// cause. Delegates to [`DirtyState::mark`].
    fn mark_dirty(&mut self, cause: DirtyCause) {
        lock_drive_shared(&self.shared).dirty.mark(cause);
    }

    /// Internal: clear the snapshot-dirty state. Used by
    /// [`Self::refresh`] / [`Self::rescan`] on a successful
    /// reconcile.
    fn clear_dirty(&mut self) {
        lock_drive_shared(&self.shared).dirty.clear();
    }

    /// Re-read the library's element state by reissuing RES against
    /// the open transport, then reconcile against the prior snapshot
    /// per `docs/layer2b-design.md` §5.2 / §5.3.
    ///
    /// On normal completion the handle's `library` is replaced with
    /// the reconciled value and `is_dirty()` is cleared. On a
    /// **shape mismatch** (drive_count / slot_count / ie_count
    /// differs from the prior snapshot) `refresh` follows the §5.3
    /// contract: the snapshot is *not* replaced, `is_dirty` is set
    /// to `true`, and `Ok(())` is returned. The daemon polls
    /// `is_dirty()` to decide whether to escalate (run a full
    /// `discover()`). Use `rescan()` instead when a shape mismatch
    /// should be a hard error.
    ///
    /// Read-only at the CDB level: refresh does **not** fire
    /// `Started` / `Finished` events for its RES (only state-changing
    /// CDBs do, per §6 property 7). It *does* fire
    /// [`AuditEvent::Warning`] events for reconciliation observations
    /// — drive replaced / appeared / vanished, and (on the §5.3 soft
    /// shape-mismatch path)
    /// [`crate::error::RescanWarning::ShapeMismatch`]. So the audit
    /// hook is still the channel through which an operator learns
    /// "something about this library changed" — whether the change
    /// was surfaced by a state-changing op or by a routine refresh.
    ///
    /// Bubbles `ScsiError` if the RES CDB itself fails.
    pub fn refresh(&mut self) -> Result<(), ScsiError> {
        let new_es = crate::discovery::issue_res(
            self.transport.as_mut(),
            /* element_type */ 0,
            /* dvcid */ true,
            /* curdata */ true,
        )?;
        match ops::reconcile(&self.library, new_es) {
            Ok((new_lib, warnings)) => {
                self.library = new_lib;
                self.clear_dirty();
                // Per §5.2: reconciliation warnings flow to the audit
                // log. Even though refresh() itself is read-only and
                // doesn't fire Started/Finished, per-bay changes
                // (drive replaced/appeared/vanished) are
                // operator-visible events.
                fire_warnings(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &self.library.serial,
                    AuditOp::Rescan,
                    &warnings,
                );
                Ok(())
            }
            Err(mismatch) => {
                // §5.3: shape mismatch in refresh is NOT a hard
                // error. Mark the snapshot dirty AND fire an audit
                // Warning event so the operator can see the
                // structural change in the same log stream as bay-
                // level reconcile observations. The daemon decides
                // whether to escalate (re-discover) based on
                // is_dirty() + the audit-log event.
                self.mark_dirty(DirtyCause::CompletionUnknown);
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Warning {
                        library_serial: &self.library.serial,
                        operation: AuditOp::Rescan,
                        warning: crate::error::RescanWarning::ShapeMismatch { summary: mismatch },
                        at: SystemTime::now(),
                    },
                );
                Ok(())
            }
        }
    }

    /// Issue INITIALIZE ELEMENT STATUS, then re-read the element
    /// state and reconcile it against the prior snapshot per
    /// `docs/layer2b-design.md` §3.2 / §5.2.
    ///
    /// Unlike [`Self::refresh`], `rescan` returns
    /// [`RescanError::SnapshotMismatch`] when the post-init element
    /// shape (counts or address sets) differs from the prior
    /// snapshot — the design treats a structural change observed via
    /// an *explicit* operator-requested rescan as a hard error
    /// rather than a soft "is_dirty" signal. The caller should
    /// re-run `discover()` from scratch.
    ///
    /// **Audit:** fires `Started` before the INIT CDB and `Finished`
    /// after. Outcome variants:
    /// - INIT succeeded + reconcile clean → `Success`.
    /// - INIT succeeded + shape mismatch → `Other { summary }` with
    ///   the mismatch description; the function returns
    ///   `SnapshotMismatch`.
    /// - INIT failed (or the post-init RES failed) → `ScsiError`.
    ///
    /// The post-init RES is read-only and is *not* separately
    /// audited; the `Finished` event covers the whole rescan
    /// operation.
    pub fn rescan(&mut self) -> Result<(), RescanError> {
        let op = AuditOp::Rescan;
        let cdb = init_cdb::build_cdb();

        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Started {
                library_serial: &self.library.serial,
                operation: op,
                cdb: &cdb,
                at: SystemTime::now(),
            },
        );

        let started = Instant::now();
        // INIT walks the whole library — minutes on big chassis.
        self.transport
            .set_timeout_for(TimeoutClass::InitElementStatus);
        if let Err(e) = self.transport.execute_none(&cdb) {
            // INIT failure with ambiguous completion: the changer
            // may have partially re-derived its element-state cache
            // before we lost the connection. Mark dirty so callers
            // don't trust the snapshot until they've rescanned
            // successfully (or re-`discover()`d).
            let dirty = completion_unknown(&e);
            if dirty {
                self.mark_dirty(DirtyCause::CompletionUnknown);
            }
            let outcome = scsi_outcome(&e, dirty);
            fire_audit(
                &mut lock_drive_shared(&self.shared).audit_hook,
                &AuditEvent::Finished {
                    library_serial: &self.library.serial,
                    operation: op,
                    outcome,
                    at: SystemTime::now(),
                },
            );
            return Err(RescanError::ScsiError(e));
        }

        // Post-init RES (read-only — not separately audited).
        // **The INIT has already re-derived the changer's element
        // state**, so the cached snapshot is definitively stale from
        // this point on. Any error path from here sets is_dirty=true.
        // (`issue_res` sets `TimeoutClass::ReadElementStatus` on the
        // transport for us — its window covers both the probe and
        // the full read.)
        let new_es = match crate::discovery::issue_res(
            self.transport.as_mut(),
            /* element_type */ 0,
            /* dvcid */ true,
            /* curdata */ true,
        ) {
            Ok(es) => es,
            Err(e) => {
                self.mark_dirty(DirtyCause::CompletionUnknown);
                let outcome = scsi_outcome(&e, /* dirty */ true);
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome,
                        at: SystemTime::now(),
                    },
                );
                return Err(RescanError::ScsiError(e));
            }
        };

        match ops::reconcile(&self.library, new_es) {
            Ok((new_lib, warnings)) => {
                let duration = started.elapsed();
                self.library = new_lib;
                self.clear_dirty();
                // Fire one Warning event per reconciliation
                // observation, before the Finished event closes out
                // the operation. Per §5.2 these reach the audit log
                // so an operator can see a hot-swapped drive even
                // when the rescan itself succeeded.
                fire_warnings(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &self.library.serial,
                    op,
                    &warnings,
                );
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome: AuditOutcome::Success {
                            duration,
                            snapshot_patched: true,
                            dirty: false,
                        },
                        at: SystemTime::now(),
                    },
                );
                Ok(())
            }
            Err(mismatch) => {
                // The INIT succeeded but the post-init shape doesn't
                // match what we knew. The changer's element state has
                // already been re-derived, so the snapshot is stale —
                // set is_dirty=true alongside returning the hard
                // error. AuditOutcome::Other captures the semantic
                // outcome without conflating with a SCSI-level
                // failure.
                self.mark_dirty(DirtyCause::CompletionUnknown);
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome: AuditOutcome::Other {
                            summary: format!("shape mismatch: {mismatch}"),
                        },
                        at: SystemTime::now(),
                    },
                );
                Err(RescanError::SnapshotMismatch(mismatch))
            }
        }
    }

    /// Issue a MOVE MEDIUM CDB to the changer, moving the cartridge
    /// at element `src` to element `dst`. Robot address is taken from
    /// `library.layout.robot_address`; INVERT is always 0.
    ///
    /// **Validation order** (matches `docs/layer2b-design.md` §3.1):
    /// snapshot-level preflight (via the `ops::plan_move` helper) →
    /// derived-identity policy check → CDB. On preflight failure no
    /// CDB is issued, an [`AuditEvent::Refused`] event is fired, and
    /// the snapshot is unchanged.
    ///
    /// On successful CDB completion, the snapshot is patched per
    /// §5.1 (cartridge moves from `src` to `dst`, `source_slot` set
    /// only when `src` was a Storage slot). [`AuditEvent::Started`]
    /// fires before the CDB and [`AuditEvent::Finished`] after.
    ///
    /// **Dirty-state on failure:** a CHECK CONDITION leaves the
    /// snapshot clean ([`is_dirty()`] stays `false`); a transport
    /// error / driver timeout sets `is_dirty()` with cause
    /// [`DirtyCause::CompletionUnknown`] because the cartridge may
    /// have moved partway. **Dirty-state on success:** when either
    /// endpoint is an IE port, `is_dirty()` is `true` with cause
    /// [`DirtyCause::VendorSemantics`] (HPE parks visibly,
    /// QuadStor vaults — the snapshot patch may not match
    /// reality). See `docs/layer2b-design.md` §5.1.
    ///
    /// [`is_dirty()`]: Self::is_dirty
    pub fn move_medium(
        &mut self,
        src: u16,
        dst: u16,
        policy: &dyn AccessPolicy,
    ) -> Result<(), MoveError> {
        self.move_medium_as(src, dst, policy, AuditOp::Move { src, dst })
    }

    /// Internal: same as [`Self::move_medium`] but lets callers tag
    /// the audit events with a composed-op context (e.g.
    /// `AuditOp::Load { slot, bay }`). The CDB itself is always
    /// MOVE MEDIUM; the `op` parameter only changes the
    /// `operation` field on `Started` / `Refused` / `Finished`
    /// events. Used by [`LibraryHandle::load`] /
    /// [`LibraryHandle::unload`] / [`Self::export`] / [`Self::import`]
    /// so the audit log can filter by operator-level intent rather
    /// than always seeing `Move` for every changer CDB.
    pub(crate) fn move_medium_as(
        &mut self,
        src: u16,
        dst: u16,
        policy: &dyn AccessPolicy,
        op: AuditOp,
    ) -> Result<(), MoveError> {
        // Snapshot-level preflight.
        let plan = match ops::plan_move(&self.library, src, dst) {
            Ok(p) => p,
            Err(e) => {
                fire_refused(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &self.library.serial,
                    op,
                    &e,
                );
                return Err(e);
            }
        };

        // Derived-identity policy gate. Defense-in-depth on top of the
        // open-time check: refresh/rescan may have introduced a new
        // derived bay since open. `plan.derived_drive_bay` returns the
        // *bay's* element address, so the error correctly names the
        // bay regardless of whether it was the source or destination.
        if let Some((bay_addr, installed)) = plan.derived_drive_bay(&self.library) {
            if !policy.allows_derived_drive_identity(&self.library.serial) {
                let e = MoveError::DerivedDriveBay {
                    addr: bay_addr,
                    serial: installed.serial.clone(),
                };
                fire_refused(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &self.library.serial,
                    op,
                    &e,
                );
                return Err(e);
            }
        }

        // Build CDB + Started event.
        let cdb = move_medium_cdb::build_cdb(
            self.library.layout.robot_address,
            src,
            dst,
            /* invert */ false,
        );
        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Started {
                library_serial: &self.library.serial,
                operation: op,
                cdb: &cdb,
                at: SystemTime::now(),
            },
        );

        // Execute. MOVE on a real chassis can take 8–20 s; allow
        // the long window so a healthy operation doesn't get torn
        // down by the SG_IO timeout (which would then look like an
        // unknown-completion transport failure to us).
        self.transport.set_timeout_for(TimeoutClass::Move);
        let started = Instant::now();
        let result = self.transport.execute_none(&cdb);
        match result {
            Ok(()) => {
                let duration = started.elapsed();
                ops::apply_planned_move(&mut self.library, &plan);
                // IE-port endpoints make the patch vendor-specific:
                // HPE physical libraries park the cartridge in the IE
                // port (snapshot is correct), QuadStor's VTL vaults
                // the cartridge immediately (IE port returns to
                // empty). The MOVE MEDIUM CDB succeeded either way,
                // but the snapshot patch may not reflect reality.
                // Mark the snapshot dirty so callers refresh before
                // trusting the IE/slot state on those sides.
                let touches_ie = self
                    .library
                    .ie_ports
                    .iter()
                    .any(|p| p.element_address == src || p.element_address == dst);
                if touches_ie {
                    self.mark_dirty(DirtyCause::VendorSemantics);
                }
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome: AuditOutcome::Success {
                            duration,
                            snapshot_patched: true,
                            dirty: touches_ie,
                        },
                        at: SystemTime::now(),
                    },
                );
                Ok(())
            }
            Err(e) => {
                // Transport-level failure on a state-changing CDB
                // means we don't know whether the cartridge moved.
                // The kernel may have given up waiting (driver
                // timeout) while the robot is still in motion;
                // a bus reset would have the same shape. Mark the
                // snapshot dirty so the caller refreshes before
                // trusting any address near `src` or `dst`.
                let dirty = completion_unknown(&e);
                if dirty {
                    self.mark_dirty(DirtyCause::CompletionUnknown);
                }
                let outcome = scsi_outcome(&e, dirty);
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome,
                        at: SystemTime::now(),
                    },
                );
                Err(MoveError::ScsiError(e))
            }
        }
    }
}

impl std::fmt::Debug for LibraryHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryHandle")
            .field("changer", &self.changer)
            .field("transport_factory", &"<transport factory>")
            .finish()
    }
}

impl LibraryHandle {
    /// Borrow the pure-changer core behind this facade.
    pub fn changer(&self) -> &ChangerHandle {
        &self.changer
    }

    /// Mutably borrow the pure-changer core behind this facade.
    pub fn changer_mut(&mut self) -> &mut ChangerHandle {
        &mut self.changer
    }

    /// Consume the facade and return the pure-changer core.
    pub fn into_changer(self) -> ChangerHandle {
        self.changer
    }

    /// Read-only access to the snapshot the handle was opened against.
    pub fn library(&self) -> &Library {
        self.changer.library()
    }

    /// Install an audit hook. Replaces any previous hook.
    pub fn set_audit_hook<F>(&mut self, hook: F)
    where
        F: FnMut(&AuditEvent<'_>) + Send + 'static,
    {
        self.changer.set_audit_hook(hook);
    }

    /// True iff the cached snapshot is no longer guaranteed to
    /// reflect the physical library.
    pub fn is_dirty(&self) -> bool {
        self.changer.is_dirty()
    }

    /// Why the snapshot is currently dirty, if it is.
    pub fn dirty_cause(&self) -> Option<DirtyCause> {
        self.changer.dirty_cause()
    }

    /// Re-read the library's element state and reconcile the cached
    /// snapshot.
    pub fn refresh(&mut self) -> Result<(), ScsiError> {
        self.changer.refresh()
    }

    /// Initialize element status, then re-read and reconcile the
    /// cached snapshot.
    pub fn rescan(&mut self) -> Result<(), RescanError> {
        self.changer.rescan()
    }

    /// Issue a MOVE MEDIUM CDB to the changer.
    pub fn move_medium(
        &mut self,
        src: u16,
        dst: u16,
        policy: &dyn AccessPolicy,
    ) -> Result<(), MoveError> {
        self.changer.move_medium(src, dst, policy)
    }

    /// Move from `slot` to the first available IE port.
    pub fn export(&mut self, slot: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> {
        self.changer.export(slot, policy)
    }

    /// Move from the first occupied IE port to `slot`.
    pub fn import(&mut self, slot: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> {
        self.changer.import(slot, policy)
    }

    /// Issue PREVENT MEDIUM REMOVAL and return a guard that releases it.
    pub fn lock_removal(&mut self) -> Result<RemovalLockGuard<'_>, ScsiError> {
        self.changer.lock_removal()
    }

    /// Issue ALLOW MEDIUM REMOVAL.
    pub fn allow_removal(&mut self) -> Result<(), ScsiError> {
        self.changer.allow_removal()
    }

    /// Open a drive handle for the bay at `bay_address`. Runs the
    /// same four-stage gate as [`Library::open`], adapted for drives:
    ///
    /// 1. **Library allowlist check.** Refuse with
    ///    [`OpenError::NotAllowed`] if `policy.allows(library_serial)`
    ///    is false. Defense-in-depth on top of `Library::open`'s
    ///    open-time check — the caller may pass a *stricter* policy
    ///    to a long-lived handle (or a policy reload may have removed
    ///    the library from the allowlist mid-session).
    /// 2. **Bay-resolution checks** against the snapshot
    ///    ([`OpenError::BayNotFound`] / [`OpenError::BayUnresolved`] /
    ///    [`OpenError::BayMissingDevice`]).
    /// 3. **Derived-identity policy gate** — if the bay's
    ///    `identity_source` is `Derived`, the policy must opt the
    ///    library in via `allows_derived_drive_identity`.
    /// 4. **Drive transport open + identity revalidation** — open the
    ///    bay's recorded `installed.sg_path` (read/write, via the
    ///    library's stored transport factory) and confirm standard
    ///    INQUIRY shows `SequentialAccess` AND VPD 0x80 matches the
    ///    recorded `installed.serial`. Anything else is
    ///    [`OpenError::IdentityChanged`] (the daemon should re-run
    ///    `discover()`).
    ///
    /// The returned [`DriveHandle`] owns its drive transport and a
    /// clone of the shared audit/dirty cell, so it does not keep a
    /// borrow of the library handle alive after this call returns.
    ///
    /// Layer 3a does not enforce process-local drive exclusivity: two
    /// direct callers can open the same bay if they bypass the Layer 5
    /// drive/session reservation machinery. Production callers must
    /// serialize drive ownership above this API; tests and composed
    /// load/unload flows intentionally reopen a bay after the previous
    /// handle has been dropped.
    pub fn open_drive(
        &mut self,
        bay_address: u16,
        policy: &dyn AccessPolicy,
    ) -> Result<DriveHandle, OpenError> {
        self.open_drive_with_tape_io(bay_address, policy, TapeIoRuntimeConfig::default())
    }

    /// Open a drive with explicit tape-I/O batching settings.
    pub fn open_drive_with_tape_io(
        &mut self,
        bay_address: u16,
        policy: &dyn AccessPolicy,
        tape_io: TapeIoRuntimeConfig,
    ) -> Result<DriveHandle, OpenError> {
        // -- 1. Library allowlist ------------------------------------
        // Refuse early — before consuming a fixture transport in
        // tests, before any I/O in production. A caller passing a
        // stricter policy than the one used at `Library::open` time
        // (or a policy reload mid-session) must not be able to
        // dispatch CDBs against a now-disallowed library.
        if !policy.allows(&self.changer.library.serial) {
            return Err(OpenError::NotAllowed {
                serial: self.changer.library.serial.clone(),
            });
        }

        // -- 2. Bay-resolution checks --------------------------------
        let bay = self
            .changer
            .library
            .drive_bays
            .iter()
            .find(|b| b.element_address == bay_address)
            .ok_or(OpenError::BayNotFound { addr: bay_address })?;
        let installed = bay
            .installed
            .as_ref()
            .ok_or(OpenError::BayUnresolved { addr: bay_address })?;
        let sg_path = installed
            .sg_path
            .as_ref()
            .ok_or_else(|| OpenError::BayMissingDevice {
                addr: bay_address,
                serial: installed.serial.clone(),
            })?
            .clone();
        let expected_serial = installed.serial.clone();
        let identity_source = installed.identity_source;
        let installed_clone = installed.clone();

        // -- 3. Derived-identity policy gate -------------------------
        if matches!(identity_source, IdentitySource::Derived)
            && !policy.allows_derived_drive_identity(&self.changer.library.serial)
        {
            return Err(OpenError::DerivedIdentityNotOptedIn {
                serial: expected_serial,
            });
        }

        // -- 4. Open transport + revalidate identity -----------------
        let mut transport =
            (self.transport_factory)(&sg_path).map_err(|cause| OpenError::DeviceUnavailable {
                path: sg_path.clone(),
                cause,
            })?;
        // Standard INQUIRY: device behind this path must still be a
        // SequentialAccess (tape drive). A swapped device type at the
        // same path is just as bad as a swapped drive.
        let inq = revalidate_inquiry(transport.as_mut())?;
        if !matches!(inq.device_type, DeviceType::SequentialAccess) {
            return Err(OpenError::IdentityChanged {
                path: sg_path,
                expected: expected_serial,
                actual: None,
            });
        }
        let actual_serial = revalidate_serial(transport.as_mut())?;
        if actual_serial.as_deref() != Some(expected_serial.as_str()) {
            return Err(OpenError::IdentityChanged {
                path: sg_path,
                expected: expected_serial,
                actual: actual_serial,
            });
        }
        let requested_batch_blocks = tape_io
            .write_batch_blocks
            .max(tape_io.read_batch_blocks)
            .max(1);
        let requested_reserved_size_bytes =
            requested_batch_blocks.saturating_mul(DEFAULT_TAPE_IO_RECORD_BYTES_FOR_RESERVED_BUFFER);
        let sg_reserved_size_bytes =
            transport.configure_reserved_buffer(requested_reserved_size_bytes)?;
        let effective_write_batch_blocks = effective_batch_blocks_from_reserved(
            sg_reserved_size_bytes,
            DEFAULT_TAPE_IO_RECORD_BYTES_FOR_RESERVED_BUFFER,
            tape_io.write_batch_blocks,
        );
        let effective_read_batch_blocks = effective_batch_blocks_from_reserved(
            sg_reserved_size_bytes,
            DEFAULT_TAPE_IO_RECORD_BYTES_FOR_RESERVED_BUFFER,
            tape_io.read_batch_blocks,
        );

        let library_serial = self.changer.library.serial.clone();
        Ok(DriveHandle {
            bay_address,
            drive: installed_clone,
            library_serial,
            transport,
            max_write_block_size_bytes: None,
            position_known: true,
            expected_position: None,
            bytes_since_position_check: 0,
            position_check_bytes: tape_io.position_check_bytes,
            legacy_single_block: tape_io.legacy_single_block,
            pipelined_submission: tape_io.pipelined_submission && !tape_io.legacy_single_block,
            staging_ring_buffers: tape_io.staging_ring_buffers,
            requested_write_batch_blocks: tape_io.write_batch_blocks.max(1),
            requested_read_batch_blocks: tape_io.read_batch_blocks.max(1),
            effective_write_batch_blocks,
            effective_read_batch_blocks,
            sg_reserved_size_bytes,
            pipeline_diagnostics: PipelineDiagnostics::default(),
            pending_pipeline_audit: None,
            validated_fixed_block_size: None,
            mode_reverification_required: None,
            shared: self.changer.shared.clone(),
        })
    }

    // =================================================================
    //  Composed operations — Layer 2b §7.7
    // =================================================================

    /// Composed: changer MOVE MEDIUM from `slot` to `bay`, then SSC
    /// `LOAD` on the drive at `bay`. Returns [`LoadError`] with
    /// phase-aware variants and per-variant snapshot semantics.
    ///
    /// Dirty-state breakdown — see [`LoadError`] variant docs and
    /// `docs/layer2b-design.md` §5.1 for the canonical table:
    ///
    /// - [`LoadError::Move`] — changer MOVE phase failed. The
    ///   physical state matches what [`Self::move_medium`] would
    ///   leave it in:
    ///   - **CHECK CONDITION** (changer rejected MOVE): cartridge
    ///     still in `slot`, snapshot unchanged, `is_dirty()` stays
    ///     `false`.
    ///   - **Transport error / driver timeout** (cartridge may have
    ///     moved; we lost the final status): `is_dirty()` is `true`
    ///     with [`DirtyCause::CompletionUnknown`]. Refresh or rescan
    ///     before acting on either endpoint.
    /// - [`LoadError::OpenDrive`] — MOVE succeeded; opening the
    ///   target drive failed (identity mismatch, capability not
    ///   granted, etc.). Snapshot is *patched* (cartridge in `bay`),
    ///   `is_dirty()` is `true` with
    ///   [`DirtyCause::PartialFailure`]. Drive LOAD was never
    ///   attempted.
    /// - [`LoadError::DriveLoad`] — MOVE + drive open succeeded; SSC
    ///   LOAD on the drive failed. Snapshot is *patched* (cartridge
    ///   in `bay`):
    ///   - **CHECK CONDITION** (drive rejected LOAD): cartridge in
    ///     bay but unloaded; `is_dirty()` is `true` with
    ///     [`DirtyCause::PartialFailure`]. Caller can retry LOAD
    ///     directly via `open_drive(...).load()` after addressing
    ///     the sense-data condition.
    ///   - **Transport error / driver timeout** (drive may have
    ///     actually loaded; status was lost in transit): `is_dirty()`
    ///     is `true` with [`DirtyCause::CompletionUnknown`].
    ///     Refresh or rescan before re-running LOAD.
    ///
    /// On success, the cartridge is in `bay` and loaded; snapshot
    /// is patched accordingly and `is_dirty()` is `false`.
    ///
    /// All audit events from both phases carry
    /// `operation = AuditOp::Load { slot, bay }` so the audit log
    /// can correlate per-CDB events back to one operator-level
    /// request.
    pub fn load(
        &mut self,
        slot: u16,
        bay: u16,
        policy: &dyn AccessPolicy,
    ) -> Result<(), LoadError> {
        let op = AuditOp::Load { slot, bay };
        // Phase 1: changer MOVE
        self.changer
            .move_medium_as(slot, bay, policy, op)
            .map_err(LoadError::Move)?;
        // MOVE succeeded — snapshot patched (cartridge in bay). If
        // anything below fails, mark the snapshot dirty. Two
        // distinct flavors:
        //  - Drive LOAD that fails with a completion-ambiguous
        //    transport error (timeout / bus reset): the cartridge
        //    moved AND the drive may have actually executed the
        //    LOAD even though we didn't get clean status back.
        //    That's `CompletionUnknown` — the stronger signal.
        //  - Everything else (open_drive failed, drive returned
        //    CHECK CONDITION, etc.): the MOVE succeeded but the
        //    LOAD didn't — classic post-MOVE partial failure.
        let result = match self.open_drive(bay, policy) {
            Ok(mut drive) => drive.load_as(op).map_err(LoadError::DriveLoad),
            Err(e) => Err(LoadError::OpenDrive(e)),
        };
        if let Err(ref err) = result {
            let cause = match err {
                LoadError::DriveLoad(DriveOpError::ScsiError(scsi_err))
                    if completion_unknown(scsi_err) =>
                {
                    DirtyCause::CompletionUnknown
                }
                _ => DirtyCause::PartialFailure,
            };
            self.changer.mark_dirty(cause);
        }
        result
    }

    /// Composed: SSC `UNLOAD` on the drive at `bay`, then changer
    /// MOVE MEDIUM `bay → destination`. If `destination` is `None`,
    /// the bay's `source_slot` (recorded by Layer 2a from RES
    /// SVALID) is used — the cartridge goes back to its natural
    /// home. If both `destination` and `source_slot` are `None`,
    /// returns [`UnloadError::Move`]`(`[`MoveError::SourceEmpty`]`)`
    /// (and fires a `Refused` audit event with `op = Unload { bay,
    /// dst: None }`).
    ///
    /// Phase-aware errors:
    /// - [`UnloadError::OpenDrive`] — drive open failed; no CDB
    ///   went out; `is_dirty()` stays `false`.
    /// - [`UnloadError::DriveUnload`] — drive UNLOAD CDB failed; no
    ///   MOVE attempted. A CHECK CONDITION leaves the snapshot
    ///   clean (cartridge still held by the drive); a transport
    ///   error / driver timeout sets `is_dirty()` with cause
    ///   `DirtyCause::CompletionUnknown` because the drive may
    ///   have actually ejected the cartridge mechanically.
    /// - [`UnloadError::Move`] — drive UNLOAD succeeded, MOVE
    ///   failed. A CHECK CONDITION leaves the snapshot clean
    ///   (cartridge still in the bay, matching the snapshot);
    ///   a transport error / driver timeout sets `is_dirty()` with
    ///   cause `DirtyCause::CompletionUnknown` because the
    ///   cartridge may have moved partway. See
    ///   `docs/layer2b-design.md` §5.1 for the full table.
    pub fn unload(
        &mut self,
        bay: u16,
        destination: Option<u16>,
        policy: &dyn AccessPolicy,
    ) -> Result<(), UnloadError> {
        // Resolve destination: explicit override → caller-supplied;
        // otherwise the bay's source_slot. The lookup is *split* so
        // an unknown bay address (caller typo) surfaces distinctly
        // from a known bay whose source_slot is None — per §3.1,
        // unknown addresses are refused with AddressUnknown.
        let dst = match destination {
            Some(d) => d,
            None => {
                let op = AuditOp::Unload { bay, dst: None };
                let bay_entry = self
                    .changer
                    .library
                    .drive_bays
                    .iter()
                    .find(|b| b.element_address == bay);
                match bay_entry.and_then(|b| b.source_slot) {
                    Some(d) => d,
                    None => {
                        // Two failure shapes; pick the right MoveError
                        // variant so the audit log and the caller's
                        // error handling can tell them apart.
                        let e = if bay_entry.is_none() {
                            MoveError::AddressUnknown {
                                library: self.changer.library.serial.clone(),
                                addr: bay,
                            }
                        } else {
                            MoveError::SourceEmpty { addr: bay }
                        };
                        fire_refused(
                            &mut lock_drive_shared(&self.changer.shared).audit_hook,
                            &self.changer.library.serial,
                            op,
                            &e,
                        );
                        return Err(UnloadError::Move(e));
                    }
                }
            }
        };
        let op = AuditOp::Unload {
            bay,
            dst: Some(dst),
        };

        // Phase 1: drive open + SSC UNLOAD. Scope the DriveHandle so
        // it drops before phase 2 needs &mut self.
        let drive_result = match self.open_drive(bay, policy) {
            Ok(mut drive) => drive.unload_as(op).map_err(UnloadError::DriveUnload),
            Err(e) => Err(UnloadError::OpenDrive(e)),
        };
        if let Err(UnloadError::DriveUnload(DriveOpError::ScsiError(ref e))) = drive_result {
            // SSC UNLOAD ambiguous: the drive may have ejected the
            // cartridge mechanically even though we didn't get the
            // status back. Bay state ("loaded = true") can no
            // longer be trusted; mark snapshot dirty so the
            // operator refreshes before the next op.
            if completion_unknown(e) {
                self.changer.mark_dirty(DirtyCause::CompletionUnknown);
            }
        }
        drive_result?;

        // Phase 2: changer MOVE bay → dst. Per §5.1, a failure here
        // leaves the snapshot unchanged (the bay still holds the
        // cartridge per the snapshot, and per reality too — the
        // drive's UNLOAD just released the cartridge mechanically;
        // it didn't move it).
        self.changer
            .move_medium_as(bay, dst, policy, op)
            .map_err(UnloadError::Move)
    }
}

impl ChangerHandle {
    /// Composed: changer MOVE MEDIUM from `slot` to the first
    /// available (`!full`) IE port. Returns
    /// [`MoveError::DestinationFull`] if all IE ports already hold a
    /// cartridge or the library has none.
    ///
    /// All audit events carry `operation = AuditOp::Export { slot,
    /// ie }`. When no IE port is resolvable, fires a `Refused` event
    /// with `ie: None` before returning the error.
    pub fn export(&mut self, slot: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> {
        let ie_addr = self
            .library
            .ie_ports
            .iter()
            .find(|p| !p.full)
            .map(|p| p.element_address);
        match ie_addr {
            Some(ie) => {
                let op = AuditOp::Export { slot, ie: Some(ie) };
                self.move_medium_as(slot, ie, policy, op)
            }
            None => {
                let representative = self
                    .library
                    .ie_ports
                    .first()
                    .map(|p| p.element_address)
                    .unwrap_or(0);
                let e = MoveError::DestinationFull {
                    addr: representative,
                };
                let op = AuditOp::Export { slot, ie: None };
                fire_refused(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &self.library.serial,
                    op,
                    &e,
                );
                Err(e)
            }
        }
    }

    /// Composed: changer MOVE MEDIUM from the first occupied
    /// (`full`) IE port to `slot`. Returns [`MoveError::SourceEmpty`]
    /// if no IE port is occupied (or the library has none).
    ///
    /// All audit events carry `operation = AuditOp::Import { ie,
    /// slot }`. When no IE port is resolvable, fires a `Refused`
    /// event with `ie: None` before returning the error.
    pub fn import(&mut self, slot: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> {
        let ie_addr = self
            .library
            .ie_ports
            .iter()
            .find(|p| p.full)
            .map(|p| p.element_address);
        match ie_addr {
            Some(ie) => {
                let op = AuditOp::Import { ie: Some(ie), slot };
                self.move_medium_as(ie, slot, policy, op)
            }
            None => {
                let representative = self
                    .library
                    .ie_ports
                    .first()
                    .map(|p| p.element_address)
                    .unwrap_or(0);
                let e = MoveError::SourceEmpty {
                    addr: representative,
                };
                let op = AuditOp::Import { ie: None, slot };
                fire_refused(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &self.library.serial,
                    op,
                    &e,
                );
                Err(e)
            }
        }
    }

    // =================================================================
    //  PREVENT / ALLOW MEDIUM REMOVAL — Layer 2b §7.8
    // =================================================================

    /// Issue PREVENT MEDIUM REMOVAL (SPC-5 `0x1E` with byte-4
    /// bit-0 set) to the changer. Locks the changer's
    /// front-panel eject and operator-initiated mailslot eject for
    /// the duration of an operator-defined critical section.
    /// Returns a [`RemovalLockGuard`] whose `Drop` does best-effort
    /// `allow_removal()` — see the guard's docs for the caveats.
    ///
    /// Audit: fires `Started{cdb=0x1E…01}` + `Finished` events
    /// tagged `AuditOp::LockRemoval`.
    pub fn lock_removal(&mut self) -> Result<RemovalLockGuard<'_>, ScsiError> {
        self.issue_prevent_allow(/* prevent */ true, AuditOp::LockRemoval)?;
        Ok(RemovalLockGuard { handle: Some(self) })
    }

    /// Issue ALLOW MEDIUM REMOVAL (SPC-5 `0x1E` with byte 4 = 0).
    /// Surfaces SCSI failure to the caller. Use this when the
    /// caller already explicitly tracks the lock via
    /// [`Self::lock_removal`] without holding its guard, or in the
    /// daemon's success-path cleanup alongside the guard's Drop
    /// (defense in depth).
    pub fn allow_removal(&mut self) -> Result<(), ScsiError> {
        self.issue_prevent_allow(/* prevent */ false, AuditOp::AllowRemoval)
    }

    /// Shared implementation of [`Self::lock_removal`] and
    /// [`Self::allow_removal`]. Builds the CDB, fires Started, runs
    /// the CDB, fires Finished with the right outcome shape.
    fn issue_prevent_allow(&mut self, prevent: bool, op: AuditOp) -> Result<(), ScsiError> {
        use remanence_scsi::prevent_allow as pa;
        let cdb = pa::build_cdb(prevent);

        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Started {
                library_serial: &self.library.serial,
                operation: op,
                cdb: &cdb,
                at: SystemTime::now(),
            },
        );

        // PREVENT/ALLOW is a config-only CDB the firmware applies
        // immediately; 5 s is generous. We don't mark the snapshot
        // dirty on transport failure because the snapshot doesn't
        // track lock state — operator can re-issue idempotently.
        self.transport.set_timeout_for(TimeoutClass::PreventAllow);
        let started = Instant::now();
        match self.transport.execute_none(&cdb) {
            Ok(()) => {
                let duration = started.elapsed();
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome: AuditOutcome::Success {
                            duration,
                            snapshot_patched: false,
                            dirty: false,
                        },
                        at: SystemTime::now(),
                    },
                );
                Ok(())
            }
            Err(e) => {
                let outcome = scsi_outcome(&e, /* dirty */ false);
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library.serial,
                        operation: op,
                        outcome,
                        at: SystemTime::now(),
                    },
                );
                Err(e)
            }
        }
    }
}

// =====================================================================
//  RemovalLockGuard — Layer 2b §7.8
// =====================================================================

/// Scope guard returned by [`ChangerHandle::lock_removal`] or
/// [`LibraryHandle::lock_removal`]. While the
/// guard is alive, the changer is locked for operator-initiated
/// removal (front panel + mailslot eject buttons refused by the
/// firmware). Dropping the guard attempts a best-effort
/// `allow_removal()` to release the lock.
///
/// **Use the guard like the changer handle.** [`RemovalLockGuard`]
/// implements [`Deref`](std::ops::Deref)`<Target = ChangerHandle>`
/// and [`DerefMut`](std::ops::DerefMut), so every [`ChangerHandle`]
/// method is callable directly on the guard:
///
/// ```ignore
/// let mut guard = handle.lock_removal()?;
/// guard.move_medium(slot, bay, &policy)?;   // protected
/// guard.release()?;                         // consume guard, surface ALLOW errors
/// ```
///
/// This is the standard `MutexGuard` shape. The borrow checker keeps
/// the ChangerHandle exclusive to the guard for the duration of the
/// critical section, and auto-deref makes the guard ergonomic to use
/// directly.
///
/// **Caveats** (per `docs/layer2b-design.md` §3.3 / §6 property 8):
/// - `Drop` is best-effort, **not** a guarantee — it doesn't run on
///   `SIGKILL`, on aborts, on host crashes, or on power loss. Daemon
///   normal paths should ALSO call [`Self::release`] so the ALLOW
///   result is surfaced and escalatable.
/// - `Drop` discards the result of the ALLOW CDB. Failure is logged
///   only via the audit hook's `Finished{ScsiError}` event for
///   `AuditOp::AllowRemoval`. Operators recover stranded locks via
///   `rem unlock <library>` or a power cycle.
///
/// `#[must_use]` is set so that an unbound guard (`let _ =
/// handle.lock_removal()?;` or just `handle.lock_removal()?;`)
/// produces a compiler warning. An unbound guard drops immediately,
/// firing ALLOW at the end of the statement and leaving any
/// subsequent operations unprotected — exactly the bug the lock is
/// meant to prevent.
#[must_use = "RemovalLockGuard releases the lock on Drop — an unbound \
              guard drops at the end of the statement, immediately \
              releasing the lock. Bind it to a named variable (or \
              call release() explicitly) to keep the critical \
              section alive."]
pub struct RemovalLockGuard<'a> {
    /// `None` after [`Self::release`] consumes the guard — Drop
    /// then has nothing to do, since release already issued the
    /// ALLOW.
    handle: Option<&'a mut ChangerHandle>,
}

impl<'a> std::ops::Deref for RemovalLockGuard<'a> {
    type Target = ChangerHandle;
    fn deref(&self) -> &ChangerHandle {
        // The Option is only `None` between `release()` taking the
        // handle and `Drop` running — neither of which permits an
        // outside caller to deref. So `expect` is unreachable in
        // practice; if it ever fires it indicates a bug here, not
        // in the caller.
        self.handle.as_ref().expect("guard already released")
    }
}

impl<'a> std::ops::DerefMut for RemovalLockGuard<'a> {
    fn deref_mut(&mut self) -> &mut ChangerHandle {
        self.handle.as_mut().expect("guard already released")
    }
}

impl<'a> RemovalLockGuard<'a> {
    /// Issue ALLOW MEDIUM REMOVAL explicitly, consuming the guard
    /// and returning the result. After this, the guard's `Drop` is
    /// a no-op.
    pub fn release(mut self) -> Result<(), ScsiError> {
        match self.handle.take() {
            Some(h) => h.allow_removal(),
            None => Ok(()),
        }
    }
}

impl<'a> std::fmt::Debug for RemovalLockGuard<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemovalLockGuard")
            .field("released", &self.handle.is_none())
            .finish()
    }
}

impl<'a> Drop for RemovalLockGuard<'a> {
    fn drop(&mut self) {
        // Best-effort: if the guard wasn't already released, attempt
        // an ALLOW. Result is discarded; failure surfaces via the
        // audit hook's Finished{ScsiError} event (see the type
        // docstring).
        if let Some(h) = self.handle.take() {
            let _ = h.allow_removal();
        }
    }
}

// =====================================================================
//  DriveHandle — Layer 2b §7.6
// =====================================================================

/// A policy-gated, identity-revalidated handle to one drive in one
/// library. Returned by [`LibraryHandle::open_drive`].
///
/// Dropping the `DriveHandle` closes the drive's `/dev/sgN`. Audit
/// and dirty state are shared with the parent library through a
/// short-held mutex.
///
/// State-changing primitives:
/// - [`Self::unload`] — SSC `LOAD/UNLOAD` with `load=0`.
/// - [`Self::load`] — SSC `LOAD/UNLOAD` with `load=1`.
///
/// Both fire audit `Started` / `Finished` pairs using the parent's
/// audit hook; the [`AuditOp`] is `DriveUnload { bay }` /
/// `DriveLoad { bay }` so the audit log distinguishes direct
/// drive-handle calls from composed `LibraryHandle::unload` /
/// `LibraryHandle::load` operations (which will tag their CDBs with
/// the composed op's variant once §7.7 lands).
pub struct DriveHandle {
    /// Element address of the bay this drive sits in. Carried so
    /// audit events can name the bay alongside the library serial.
    bay_address: u16,
    /// Snapshot of the drive's `InstalledDrive` at the time of open.
    drive: InstalledDrive,
    /// Library this drive belongs to. Carried for audit context.
    library_serial: String,
    /// Open transport to the drive's `/dev/sgN`. Same kind of
    /// boxed-dyn shape as `LibraryHandle::transport`.
    transport: Box<dyn SgTransport>,
    /// Drive-reported variable WRITE block limit, populated by
    /// `read_config()` from READ BLOCK LIMITS and reused when the
    /// drive later rejects a WRITE with INVALID FIELD IN CDB.
    max_write_block_size_bytes: Option<u32>,
    /// False after a completion-unknown drive transport failure until
    /// a positioning command or READ POSITION succeeds. Destructive
    /// writes are refused while false.
    position_known: bool,
    /// Arithmetic cursor seeded from READ POSITION and advanced by
    /// clean fixed-mode batched operations.
    expected_position: Option<tape_io::TapePosition>,
    /// Bytes advanced since the last tripwire READ POSITION.
    bytes_since_position_check: u64,
    /// Tripwire cadence for arithmetic cursor checks. Zero disables
    /// mid-stream checks.
    position_check_bytes: u64,
    /// Exact legacy variable-mode single-block behavior switch.
    legacy_single_block: bool,
    /// Effective write-side submission mode, after the legacy override.
    pipelined_submission: bool,
    /// Fixed page-aligned write-buffer ring depth.
    staging_ring_buffers: u32,
    /// Configured write batch in fixed records before sg/HBA clamp.
    requested_write_batch_blocks: u32,
    /// Configured read batch in fixed records before sg/HBA clamp.
    requested_read_batch_blocks: u32,
    /// Effective write batch after sg reserved-buffer clamp.
    effective_write_batch_blocks: u32,
    /// Effective read batch after sg reserved-buffer clamp.
    effective_read_batch_blocks: u32,
    /// Actual sg reserved buffer size reported by the transport.
    sg_reserved_size_bytes: u32,
    /// Allocation-free hot-submitter counters and fixed histograms.
    pipeline_diagnostics: PipelineDiagnostics,
    /// Safety-relevant completion audit held until its fence is durable.
    pending_pipeline_audit: Option<PendingPipelineAudit>,
    /// Fixed block size most recently applied or reverified for this open.
    validated_fixed_block_size: Option<u32>,
    /// Tape-sourced fixed block size that must be MODE SENSE reverified after UA.
    mode_reverification_required: Option<u32>,
    /// Shared audit hook + dirty-state cell cloned from the parent
    /// library handle.
    shared: Arc<Mutex<DriveShared>>,
}

impl std::fmt::Debug for DriveHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shared = lock_drive_shared(&self.shared);
        f.debug_struct("DriveHandle")
            .field("bay_address", &self.bay_address)
            .field("drive", &self.drive)
            .field("library_serial", &self.library_serial)
            .field("transport", &"<dyn SgTransport>")
            .field(
                "max_write_block_size_bytes",
                &self.max_write_block_size_bytes,
            )
            .field("position_known", &self.position_known)
            .field("expected_position", &self.expected_position)
            .field("legacy_single_block", &self.legacy_single_block)
            .field("pipelined_submission", &self.pipelined_submission)
            .field("staging_ring_buffers", &self.staging_ring_buffers)
            .field(
                "effective_write_batch_blocks",
                &self.effective_write_batch_blocks,
            )
            .field(
                "effective_read_batch_blocks",
                &self.effective_read_batch_blocks,
            )
            .field("sg_reserved_size_bytes", &self.sg_reserved_size_bytes)
            .field(
                "audit_hook",
                &if shared.audit_hook.is_some() {
                    "Some(<FnMut>)"
                } else {
                    "None"
                },
            )
            .finish()
    }
}

impl DriveHandle {
    /// The bay address this drive sits in (the changer element
    /// address that addresses this drive).
    pub fn bay_address(&self) -> u16 {
        self.bay_address
    }

    /// Snapshot of the drive's metadata at the time of open.
    pub fn drive(&self) -> &InstalledDrive {
        &self.drive
    }

    /// Library serial this drive belongs to.
    pub fn library_serial(&self) -> &str {
        &self.library_serial
    }

    /// Requested write batch size before sg/HBA clamping.
    pub fn requested_write_batch_blocks(&self) -> u32 {
        self.requested_write_batch_blocks
    }

    /// Requested read batch size before sg/HBA clamping.
    pub fn requested_read_batch_blocks(&self) -> u32 {
        self.requested_read_batch_blocks
    }

    /// Effective write batch size after sg reserved-buffer clamping.
    pub fn effective_write_batch_blocks(&self) -> u32 {
        self.effective_write_batch_blocks
    }

    /// Effective read batch size after sg reserved-buffer clamping.
    pub fn effective_read_batch_blocks(&self) -> u32 {
        self.effective_read_batch_blocks
    }

    /// True when callers should preserve legacy single-record behavior.
    pub fn legacy_single_block(&self) -> bool {
        self.legacy_single_block
    }

    /// Effective write-side pipeline mode after the legacy override.
    pub fn pipelined_submission(&self) -> bool {
        self.pipelined_submission && !self.legacy_single_block
    }

    /// Fixed staging-ring depth snapshotted at drive open.
    pub fn staging_ring_buffers(&self) -> u32 {
        self.staging_ring_buffers
    }

    /// Force the handle back to legacy single-record behavior for this open
    /// session. Used when a read-side fixed-mode setup cannot be established
    /// from the tape's own catalog/bootstrap geometry.
    pub fn set_legacy_single_block(&mut self, legacy_single_block: bool) {
        self.legacy_single_block = legacy_single_block;
    }

    /// Configured drift tripwire cadence in bytes.
    pub fn position_check_bytes(&self) -> u64 {
        self.position_check_bytes
    }

    /// Actual sg reserved buffer size reported by the transport.
    pub fn sg_reserved_size_bytes(&self) -> u32 {
        self.sg_reserved_size_bytes
    }

    /// Issue SSC `UNLOAD` (`0x1B` with byte 4 = 0) to the drive.
    /// Required before the changer can pluck the cartridge from the
    /// bay — modern LTO drives hold the cartridge mechanically until
    /// the host explicitly releases it.
    pub fn unload(&mut self) -> Result<(), DriveOpError> {
        self.issue_load_unload(
            /* load */ false,
            /* immed */ false,
            AuditOp::DriveUnload {
                bay: self.bay_address,
            },
        )
    }

    /// Issue SSC `LOAD` (`0x1B` with byte 4 = 1) to the drive. Modern
    /// LTO drives load automatically on insert; this is the polite
    /// explicit form Layer 2b uses after a changer MOVE places a
    /// cartridge in the bay.
    pub fn load(&mut self) -> Result<(), DriveOpError> {
        self.issue_load_unload(
            /* load */ true,
            /* immed */ false,
            AuditOp::DriveLoad {
                bay: self.bay_address,
            },
        )
    }

    /// Issue SSC `LOAD` with `IMMED=1`. Readiness-aware workflows use this
    /// after a changer MOVE and then poll TEST UNIT READY, instead of blocking
    /// inside the drive LOAD while LTO-9 media calibrates.
    pub fn load_immediate(&mut self) -> Result<(), DriveOpError> {
        self.issue_load_unload(
            /* load */ true,
            /* immed */ true,
            AuditOp::DriveLoad {
                bay: self.bay_address,
            },
        )
    }

    /// Internal: same as [`Self::unload`] but lets composed
    /// `LibraryHandle::unload` tag the audit events with the outer
    /// op context (`AuditOp::Unload { bay, dst }`).
    pub(crate) fn unload_as(&mut self, op: AuditOp) -> Result<(), DriveOpError> {
        self.issue_load_unload(/* load */ false, /* immed */ false, op)
    }

    /// Internal: same as [`Self::load`] but lets composed
    /// `LibraryHandle::load` tag the audit events with the outer op
    /// context (`AuditOp::Load { slot, bay }`).
    pub(crate) fn load_as(&mut self, op: AuditOp) -> Result<(), DriveOpError> {
        self.issue_load_unload(/* load */ true, /* immed */ false, op)
    }

    fn issue_load_unload(
        &mut self,
        load: bool,
        immed: bool,
        op: AuditOp,
    ) -> Result<(), DriveOpError> {
        use remanence_scsi::load_unload as lu;
        let cdb = lu::build_cdb_with_immed(load, immed);

        fire_audit(
            &mut lock_drive_shared(&self.shared).audit_hook,
            &AuditEvent::Started {
                library_serial: &self.library_serial,
                operation: op,
                cdb: &cdb,
                at: SystemTime::now(),
            },
        );

        // SSC LOAD/UNLOAD on an LTO drive includes mechanical
        // unload + tape positioning; LOAD from cold can take
        // multiple minutes. Allow the 10-minute window so a
        // healthy operation isn't torn down by SG_IO timeout.
        self.transport.set_timeout_for(TimeoutClass::LoadUnload);
        let started = Instant::now();
        let result = self.transport.execute_none(&cdb);
        match result {
            Ok(()) => {
                let duration = started.elapsed();
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library_serial,
                        operation: op,
                        outcome: AuditOutcome::Success {
                            duration,
                            snapshot_patched: false,
                            dirty: false,
                        },
                        at: SystemTime::now(),
                    },
                );
                Ok(())
            }
            Err(e) => {
                // Step 9.1d (codex 97997d71) closed the TODO that
                // used to sit here: `DriveHandle` shares the parent
                // `LibraryHandle`'s dirty state, so completion-
                // unknown transport errors flip the parent dirty bit
                // directly. The audit event still carries the
                // `dirty: …` field for callers that consume the
                // audit stream independently of the snapshot.
                let dirty = completion_unknown(&e);
                if dirty {
                    self.position_known = false;
                    lock_drive_shared(&self.shared)
                        .dirty
                        .mark(DirtyCause::CompletionUnknown);
                }
                let outcome = scsi_outcome(&e, dirty);
                fire_audit(
                    &mut lock_drive_shared(&self.shared).audit_hook,
                    &AuditEvent::Finished {
                        library_serial: &self.library_serial,
                        operation: op,
                        outcome,
                        at: SystemTime::now(),
                    },
                );
                Err(DriveOpError::ScsiError(e))
            }
        }
    }
}

/// Fire the installed audit hook (if any) with `event`. Free function
/// rather than a method on [`LibraryHandle`] / [`DriveHandle`] so the
/// borrow checker can split the shared audit cell apart from the
/// serial fields that `AuditEvent` borrows from.
fn fire_audit(hook: &mut Option<AuditHook>, event: &AuditEvent<'_>) {
    if let Some(h) = hook.as_mut() {
        h(event);
    }
}

/// Fire one [`AuditEvent::Warning`] per `RescanWarning`, in order.
/// Used by `rescan` and `refresh` to route reconciliation
/// observations to the audit log per `docs/layer2b-design.md` §5.2.
fn fire_warnings(
    hook: &mut Option<AuditHook>,
    library_serial: &str,
    operation: AuditOp,
    warnings: &[crate::error::RescanWarning],
) {
    for w in warnings {
        fire_audit(
            hook,
            &AuditEvent::Warning {
                library_serial,
                operation,
                warning: w.clone(),
                at: SystemTime::now(),
            },
        );
    }
}

/// Build an [`AuditOutcome::ScsiError`] from a [`ScsiError`], pulling
/// sense bytes when the variant carries them. Used by both
/// `move_medium` and `rescan` to keep the Finished-event construction
/// consistent.
fn scsi_outcome(err: &ScsiError, dirty: bool) -> AuditOutcome {
    let sense = match err {
        ScsiError::CheckCondition { sense, .. } => Some(sense.clone()),
        ScsiError::TransportError { sense, .. } => Some(sense.clone()),
        _ => None,
    };
    AuditOutcome::ScsiError {
        sense,
        summary: err.to_string(),
        dirty,
    }
}

/// True when a failed [`SgTransport::execute_none`] of a
/// state-changing CDB leaves the operation's completion state
/// **ambiguous** — i.e. the CDB may have actually executed on the
/// device side even though we don't know for sure. Callers use
/// this to decide whether to mark the cached snapshot dirty.
///
/// - [`ScsiError::TransportError`] — driver timeout, host adapter
///   reset, bus reset. Completion unknown; treat as dirty.
/// - [`ScsiError::Io`] — the ioctl itself failed (very rare). The
///   request usually never reached the device, but we can't
///   prove it; safer default is dirty.
/// - [`ScsiError::CheckCondition`] — the device received the CDB,
///   chose not to execute it, and returned sense. Completion
///   known: didn't happen. Not dirty.
/// - [`ScsiError::UnexpectedStatus`] — the device returned a target
///   status such as BUSY/RESERVATION CONFLICT without a host/driver
///   failure. Completion known: command was not accepted. Not dirty.
/// - [`ScsiError::InvalidInput`] / `Truncated` / `InvalidResponse`
///   — pre-flight or post-flight parse failures. Not dirty.
fn completion_unknown(err: &ScsiError) -> bool {
    match err {
        ScsiError::TransportError { .. } => true,
        #[cfg(target_os = "linux")]
        ScsiError::Io(_) => true,
        #[cfg(target_os = "linux")]
        ScsiError::CheckCondition { .. } | ScsiError::UnexpectedStatus { .. } => false,
        ScsiError::InvalidInput(_)
        | ScsiError::Truncated { .. }
        | ScsiError::InvalidResponse { .. } => false,
    }
}

/// Fire an [`AuditEvent::Refused`] whose `reason` is the variant name
/// of the [`MoveError`] that caused the refusal.
fn fire_refused(hook: &mut Option<AuditHook>, library_serial: &str, op: AuditOp, err: &MoveError) {
    let reason: &'static str = match err {
        MoveError::AddressUnknown { .. } => "AddressUnknown",
        MoveError::SourceEmpty { .. } => "SourceEmpty",
        MoveError::DestinationFull { .. } => "DestinationFull",
        MoveError::SameElement { .. } => "SameElement",
        MoveError::DriveBayUnresolved { .. } => "DriveBayUnresolved",
        MoveError::DriveBayMissingDevice { .. } => "DriveBayMissingDevice",
        MoveError::DerivedDriveBay { .. } => "DerivedDriveBay",
        MoveError::ScsiError(_) => "ScsiError",
    };
    fire_audit(
        hook,
        &AuditEvent::Refused {
            library_serial,
            operation: op,
            reason,
            at: SystemTime::now(),
        },
    );
}

impl Library {
    /// Open a [`LibraryHandle`] against this library's cached device.
    /// Linux-only convenience that uses [`LinuxSgTransport`] internally.
    ///
    /// See `Library::open_with` for the testable form (and for
    /// platforms that need a custom transport).
    ///
    /// [`LinuxSgTransport`]: crate::transport::LinuxSgTransport
    #[cfg(target_os = "linux")]
    pub fn open(&self, policy: &dyn AccessPolicy) -> Result<LibraryHandle, OpenError> {
        use crate::transport::LinuxSgTransport;
        // Read/write — this is the state-changing handle path. The
        // Layer 2b primitives (MOVE MEDIUM, INIT ELEMENT STATUS,
        // PREVENT/ALLOW, LOAD/UNLOAD) are all SG_DXFER_NONE, but the
        // Linux SG layer requires write access on the fd before it
        // will authorise several of them. open_rw refuses early
        // rather than letting the first state-changing CDB
        // surprise-fail with EACCES. Capability checks
        // (CAP_SYS_RAWIO) are a separate gate — see INSTALL.md
        // "Host privileges".
        self.open_with(policy, |path| {
            LinuxSgTransport::open_rw(path)
                .map(|t| Box::new(t) as Box<dyn SgTransport>)
                .map_err(|e| IoErrorKind::from(&e))
        })
    }

    /// Open a [`LibraryHandle`] using a caller-provided transport
    /// factory. This is the testable form: pass a closure that returns
    /// a [`FixtureTransport`] (or any other `SgTransport`) so the open
    /// flow can be exercised without touching `/dev/sg*`.
    ///
    /// [`FixtureTransport`]: crate::transport::FixtureTransport
    pub fn open_with<F>(
        &self,
        policy: &dyn AccessPolicy,
        mut transport_for: F,
    ) -> Result<LibraryHandle, OpenError>
    where
        F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind> + 'static,
    {
        // -- 1. Policy: library on the allowlist? ---------------------
        if !policy.allows(&self.serial) {
            return Err(OpenError::NotAllowed {
                serial: self.serial.clone(),
            });
        }

        // -- 2. Derived-identity bays require explicit opt-in ---------
        // Refuse to even open the handle if any drive in this library
        // has a topology-derived identity AND the policy hasn't said
        // "yes, that's fine for this library." Drive-level state
        // changes are gated separately in Layer 2b, but the design
        // doc treats this as a library-acquisition concern too — the
        // operator should not be able to forget that a library has
        // unsafe drive mappings.
        if !policy.allows_derived_drive_identity(&self.serial) {
            for bay in &self.drive_bays {
                if let Some(installed) = &bay.installed {
                    if matches!(installed.identity_source, IdentitySource::Derived) {
                        return Err(OpenError::DerivedIdentityNotOptedIn {
                            serial: installed.serial.clone(),
                        });
                    }
                }
            }
        }

        // -- 3. Open transport ----------------------------------------
        let mut transport =
            transport_for(&self.changer_sg).map_err(|cause| OpenError::DeviceUnavailable {
                path: self.changer_sg.clone(),
                cause,
            })?;

        // -- 4. Identity revalidation: standard INQUIRY + VPD 0x80 ----
        // Standard INQUIRY first: the device behind the cached
        // /dev/sgN must still be a medium changer. A different SCSI
        // device type sitting at the same path (a tape drive after
        // hot-plug, or a disk after pass-through reordering) is just
        // as bad as a different changer — refuse with IdentityChanged.
        let inq = revalidate_inquiry(transport.as_mut())?;
        if !matches!(inq.device_type, DeviceType::MediumChanger) {
            return Err(OpenError::IdentityChanged {
                path: self.changer_sg.clone(),
                expected: self.serial.clone(),
                // Stand-in payload — the new device isn't a changer at
                // all, so there's no meaningful serial to surface.
                actual: None,
            });
        }
        let actual_serial = revalidate_serial(transport.as_mut())?;
        if actual_serial.as_deref() != Some(self.serial.as_str()) {
            return Err(OpenError::IdentityChanged {
                path: self.changer_sg.clone(),
                expected: self.serial.clone(),
                actual: actual_serial,
            });
        }

        let shared = Arc::new(Mutex::new(DriveShared::default()));
        let changer = ChangerHandle {
            library: self.clone(),
            transport,
            shared,
        };

        Ok(LibraryHandle {
            changer,
            transport_factory: Box::new(transport_for),
        })
    }
}

/// Reissue standard INQUIRY against the open transport. Hard-errors
/// bubble as `OpenError::ScsiError`; the caller is expected to inspect
/// `device_type` and refuse if it's no longer a medium changer.
fn revalidate_inquiry(transport: &mut dyn SgTransport) -> Result<Inquiry, ScsiError> {
    let cdb = inquiry::build_cdb(inquiry::ALLOC_LEN);
    let mut buf = vec![0u8; inquiry::ALLOC_LEN as usize];
    let n = transport.execute_in(&cdb, &mut buf)?.bytes_transferred as usize;
    Inquiry::parse(&buf[..n])
}

/// Reissue INQUIRY VPD 0x80 against the open transport. SCSI-layer
/// errors bubble as `OpenError::ScsiError`; a malformed VPD response
/// is treated as "we can't confirm identity" — the caller turns that
/// into `IdentityChanged { actual: None }`.
fn revalidate_serial(transport: &mut dyn SgTransport) -> Result<Option<String>, ScsiError> {
    let cdb = inquiry::build_cdb_vpd(vpd::PAGE_UNIT_SERIAL, vpd::ALLOC_LEN);
    let mut buf = vec![0u8; vpd::ALLOC_LEN as usize];
    let n = transport.execute_in(&cdb, &mut buf)?.bytes_transferred as usize;
    Ok(UnitSerial::parse(&buf[..n])
        .ok()
        .map(|us| us.as_str().to_string()))
}

// =====================================================================
//  Tests — drive each gate independently
// =====================================================================

#[cfg(test)]
mod tests;
