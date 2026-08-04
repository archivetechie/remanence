//! The media-dispatch gate — the single point through which every
//! write-direction CDB reaches a drive's transport, and the
//! media-write fence that runs inside it.
//!
//! Design of record: `design-read-ordering.md` §6.5 and decision D4b
//! (kept in the private journal repo). The wrap map describing a
//! cartridge's geometry is only valid while that cartridge has not
//! been written since the map was harvested. The guarantee is
//! enforced here: before the **first media-modifying CDB of a load**
//! is dispatched, the fence durably advances the volume's
//! `write_epoch`, marks calibration uncalibrated, and allocates a
//! fresh `calibration_generation` in the durable calibration-control
//! store. If that transaction cannot be made durable, **the CDB is
//! not dispatched**. A served wrap map requires
//! `map.write_epoch == volume.write_epoch`
//! ([`wrap_map_is_servable`]), so a fence that ran for a write whose
//! CDB later failed produces false invalidation — deliberate and
//! safe, never the reverse.
//!
//! Why this is a *structural* funnel rather than a convention: the
//! drive transport is wrapped in [`MediaFencedTransport`], whose
//! inner [`SgTransport`] is private to this module. Code elsewhere in
//! the `handle` tree — including Layer 3a's own methods — cannot name
//! `execute_out`, nor `execute_none` for a media-modifying opcode, on
//! the drive transport at all. The only write-direction path is
//! [`MediaFencedTransport::dispatch_media_cdb`] (via its two typed
//! shims), and the fence lives inside it. Adding a ninth independent
//! dispatch site is a compile error, not a review finding.
//!
//! The epoch advance is deliberately **not** attached to
//! `fire_tape_started`: `write_block_batch_pipelined` skips that
//! audit hook on its clean pre-dispatch path by design, so a fence
//! hung there would silently miss the highest-volume write path in
//! the system. That drift happened once already; this module exists
//! so it cannot recur.

use std::fmt;
use std::sync::{Arc, Mutex};

use remanence_scsi::{decode_sense, ScsiError};

use super::TapeIoError;
use crate::transport::{SgTransport, TimeoutClass, TransferOutcome};

// =====================================================================
//  The durable calibration-control interface (implemented by P4)
// =====================================================================

/// The one transaction the media-write fence needs from the durable
/// calibration-control store.
///
/// This is the smallest interface the fence can be written against.
/// The store itself — per-volume `write_epoch` rows, the monotonic
/// `calibration_generation` allocator, `write_path_trust`, and the
/// wrap-map cache keyed by `tape_uuid` — is built by P4. The layer
/// that knows which volume is loaded binds an implementation to the
/// loaded volume and installs it via
/// [`super::super::DriveHandle::install_media_write_fence`] at load
/// time; Layer 3a itself never learns a `tape_uuid`.
pub trait MediaWriteFence: Send {
    /// Durably record that a media-modifying CDB is about to be
    /// dispatched to the loaded volume. One atomic transaction must:
    ///
    /// 1. advance the volume's durable `write_epoch`,
    /// 2. mark the volume's calibration state uncalibrated, and
    /// 3. allocate a fresh, never-reused `calibration_generation`.
    ///
    /// `Ok(())` means the transaction is durable and the CDB may be
    /// dispatched. `Err` means it could not be made durable and the
    /// CDB **must not** be dispatched — the gate enforces that.
    /// Allocator exhaustion fails closed through the same `Err` path
    /// rather than wrapping.
    fn fence_media_write(&mut self) -> Result<(), MediaWriteFenceError>;
}

/// Failure of the fence's durable transaction. Carrying this error
/// means **no CDB was dispatched** for the operation that hit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaWriteFenceError {
    reason: String,
}

impl MediaWriteFenceError {
    /// Build a fence error from a human-readable reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The human-readable reason the transaction was not durable.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for MediaWriteFenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "durable write-epoch advance failed: {reason}",
            reason = self.reason
        )
    }
}

impl std::error::Error for MediaWriteFenceError {}

/// A cached wrap map is servable only while the epoch recorded at
/// harvest equals the volume's current durable `write_epoch`
/// (design §6.5: "a mismatch is an invalid map and is never
/// served"). P4's map cache must route its serve decision through
/// this predicate — one funnel, so the rule cannot fork.
pub fn wrap_map_is_servable(map_write_epoch: u64, volume_write_epoch: u64) -> bool {
    map_write_epoch == volume_write_epoch
}

/// Default fence installed at drive open, before any calibration
/// store exists for the load.
///
/// It succeeds without recording anything, which is sound **only**
/// because harvesting a wrap map and installing the real durable
/// fence are one act (P4's load path): while this placeholder is
/// installed, no map has been harvested under the current load's
/// epoch binding, so there is nothing a missed epoch advance could
/// leave stale. P4 must install the real store's fence at every load
/// harvest; this type is deliberately empty so it can never be
/// mistaken for one.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCalibrationControl;

impl MediaWriteFence for NoCalibrationControl {
    fn fence_media_write(&mut self) -> Result<(), MediaWriteFenceError> {
        Ok(())
    }
}

/// In-memory stand-in for the durable calibration-control store.
///
/// P4 builds the real store; this stub exists so the fence, the gate
/// and their callers can be exercised hermetically today. It models
/// exactly the state the fence transaction touches — `write_epoch`,
/// `calibration_generation`, the calibrated flag — plus test controls
/// for injecting durability failures. Cloning shares the same state,
/// so a test can install one clone into a `DriveHandle` and observe
/// through the other.
#[derive(Debug, Clone, Default)]
pub struct InMemoryCalibrationControl {
    state: Arc<Mutex<CalibrationControlState>>,
}

#[derive(Debug, Default)]
struct CalibrationControlState {
    write_epoch: u64,
    calibration_generation: u64,
    calibrated: bool,
    fence_transactions: u64,
    fail_fence_reason: Option<String>,
}

impl InMemoryCalibrationControl {
    /// Fresh store: epoch 0, generation 0, uncalibrated.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CalibrationControlState> {
        self.state.lock().expect("calibration-control state lock")
    }

    /// Current durable per-volume write epoch.
    pub fn write_epoch(&self) -> u64 {
        self.lock().write_epoch
    }

    /// Current calibration generation.
    pub fn calibration_generation(&self) -> u64 {
        self.lock().calibration_generation
    }

    /// Whether the volume is currently calibrated.
    pub fn is_calibrated(&self) -> bool {
        self.lock().calibrated
    }

    /// Number of successful fence transactions recorded — the direct
    /// "epoch advanced N times" observable for tests.
    pub fn fence_transactions(&self) -> u64 {
        self.lock().fence_transactions
    }

    /// Test control: make every subsequent fence transaction fail
    /// with `reason` until [`Self::clear_fence_failure`] is called.
    pub fn fail_fence(&self, reason: impl Into<String>) {
        self.lock().fail_fence_reason = Some(reason.into());
    }

    /// Test control: stop injecting fence failures.
    pub fn clear_fence_failure(&self) {
        self.lock().fail_fence_reason = None;
    }

    /// Test control: simulate a successful load-time harvest binding
    /// a map to the current epoch. Returns the epoch the simulated
    /// map records, for use with [`wrap_map_is_servable`].
    pub fn mark_calibrated(&self) -> u64 {
        let mut state = self.lock();
        state.calibrated = true;
        state.write_epoch
    }

    /// Test control: place the epoch allocator at `epoch` (for
    /// exhaustion tests).
    pub fn set_write_epoch(&self, epoch: u64) {
        self.lock().write_epoch = epoch;
    }
}

impl MediaWriteFence for InMemoryCalibrationControl {
    fn fence_media_write(&mut self) -> Result<(), MediaWriteFenceError> {
        let mut state = self.lock();
        if let Some(reason) = &state.fail_fence_reason {
            return Err(MediaWriteFenceError::new(reason.clone()));
        }
        // Exhaustion fails closed rather than wrapping (§4.3): a
        // reused epoch or generation could resurrect a stale map or
        // negative by numerical coincidence.
        let next_epoch = state.write_epoch.checked_add(1).ok_or_else(|| {
            MediaWriteFenceError::new("write_epoch allocator exhausted; failing closed")
        })?;
        let next_generation = state.calibration_generation.checked_add(1).ok_or_else(|| {
            MediaWriteFenceError::new("calibration_generation allocator exhausted; failing closed")
        })?;
        state.write_epoch = next_epoch;
        state.calibration_generation = next_generation;
        state.calibrated = false;
        state.fence_transactions += 1;
        Ok(())
    }
}

// =====================================================================
//  CDB classification
// =====================================================================

/// What a write-direction CDB does to the medium. The gate refuses
/// anything it cannot classify — an unknown opcode is a fail-closed
/// error, never a silent dispatch, so extending the write surface
/// forces a deliberate entry here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteDirectionClass {
    /// Modifies recorded media: the map-invalidating set. WRITE(6)
    /// and every WRITE FILEMARKS(6) form — synchronous, IMMED,
    /// pipelined and zero-count all share opcode `0x10`.
    MediaModifying,
    /// Write-direction but configuration-only: MODE SELECT(6). rem
    /// only ever sends the data-compression page (0x0F) built by
    /// `build_compression_param_list`, which changes drive session
    /// config and never recorded media, so it must not trip the
    /// fence — the parity read/recovery path sets block size through
    /// it, and fencing it would uncalibrate volumes on read.
    ConfigOnly,
}

fn classify_write_direction_cdb(cdb: &[u8]) -> Option<WriteDirectionClass> {
    match cdb.first() {
        // WRITE(6), fixed or variable mode.
        Some(0x0A) => Some(WriteDirectionClass::MediaModifying),
        // WRITE FILEMARKS(6), all forms.
        Some(0x10) => Some(WriteDirectionClass::MediaModifying),
        // MODE SELECT(6).
        Some(0x15) => Some(WriteDirectionClass::ConfigOnly),
        // Everything else — including ERASE (0x19), FORMAT MEDIUM
        // (0x04) and any 10/16-byte write form rem does not build
        // today — is refused until classified here deliberately.
        _ => None,
    }
}

/// Opcodes [`MediaFencedTransport::execute_none_nonmedia`] refuses:
/// every opcode the gate classifies (write-direction) plus the
/// unimplemented media-destructive commands, so a WRITE FILEMARKS —
/// or a future ERASE — routed around the gate fails loudly instead of
/// reaching the medium unfenced.
fn opcode_requires_media_gate(opcode: u8) -> bool {
    matches!(
        opcode,
        0x04 // FORMAT MEDIUM
        | 0x0A // WRITE(6)
        | 0x10 // WRITE FILEMARKS(6)
        | 0x15 // MODE SELECT(6)
        | 0x19 // ERASE(6)
        | 0x55 // MODE SELECT(10)
        | 0x80 // WRITE FILEMARKS(16)
        | 0x8A // WRITE(16)
        | 0x93 // ERASE(16)
    )
}

// =====================================================================
//  The fenced transport and the gate
// =====================================================================

/// How a gate dispatch failed. `NotDispatched` means no CDB reached
/// the transport — device and position state are unchanged; `Scsi` is
/// the transport's own error for a CDB that was dispatched, handled
/// by each call site exactly as before.
#[derive(Debug)]
pub(in crate::handle) enum MediaDispatchError {
    /// The CDB was refused before dispatch (fence not durable, or an
    /// unclassified/malformed write-direction request). Carries the
    /// fully mapped Layer 3a error.
    NotDispatched(TapeIoError),
    /// The transport dispatched the CDB and returned this error.
    Scsi(ScsiError),
}

/// The drive transport, wrapped so that write-direction dispatch is
/// only reachable through the media gate. See the module docs for
/// why the inner transport is private to this module.
pub(in crate::handle) struct MediaFencedTransport {
    /// The raw drive transport. Private to this module — that
    /// privacy IS the structural enforcement.
    inner: Box<dyn SgTransport>,
    /// The installed durable-fence hook. [`NoCalibrationControl`]
    /// until the load path (P4) installs the real store's fence.
    fence: Box<dyn MediaWriteFence>,
    /// Load-local flag: true once the epoch has been advanced for the
    /// current load, so the fence runs once per load rather than once
    /// per command. Cleared on every load boundary (SSC LOAD/UNLOAD,
    /// reset UNIT ATTENTION, fence installation) — clearing too often
    /// costs a harmless extra epoch advance (false invalidation);
    /// clearing too rarely is the silent stale-map failure, so every
    /// ambiguous boundary clears.
    fenced_this_load: bool,
}

impl MediaFencedTransport {
    /// Wrap a drive transport. The fence starts as
    /// [`NoCalibrationControl`]; the load path installs the real one.
    pub(in crate::handle) fn new(inner: Box<dyn SgTransport>) -> Self {
        Self {
            inner,
            fence: Box::new(NoCalibrationControl),
            fenced_this_load: false,
        }
    }

    /// Install the durable calibration-control fence for the current
    /// load, replacing the previous hook. Re-arms the once-per-load
    /// flag: installation happens at load boundaries, and re-arming
    /// errs toward false invalidation, never staleness.
    pub(in crate::handle) fn install_media_write_fence(&mut self, fence: Box<dyn MediaWriteFence>) {
        self.fence = fence;
        self.fenced_this_load = false;
    }

    /// Clear the once-per-load flag at a load boundary (SSC
    /// LOAD/UNLOAD attempt, reset UNIT ATTENTION).
    pub(in crate::handle) fn reset_media_write_fence_for_load_boundary(&mut self) {
        self.fenced_this_load = false;
    }

    /// Whether the epoch has been advanced (and a media-modifying CDB
    /// handed to the transport) in the current load.
    pub(in crate::handle) fn media_write_fenced_this_load(&self) -> bool {
        self.fenced_this_load
    }

    /// Read-direction pass-through (`SG_DXFER_FROM_DEV`). Cannot
    /// modify media by construction of the data direction.
    pub(in crate::handle) fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<TransferOutcome, ScsiError> {
        let result = self.inner.execute_in(cdb, buf);
        if let Err(error) = &result {
            self.note_unit_attention(error);
        }
        result
    }

    /// No-data-phase dispatch for CDBs that do **not** modify media
    /// (REWIND, LOCATE, SPACE, TEST UNIT READY, LOAD/UNLOAD).
    /// Refuses the media-gate opcode set loudly, so a media-modifying
    /// CDB routed around the gate fails closed instead of reaching
    /// the medium unfenced.
    pub(in crate::handle) fn execute_none_nonmedia(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb.first().copied().is_some_and(opcode_requires_media_gate) {
            return Err(ScsiError::InvalidInput(
                "media-modifying CDB refused on the non-media dispatch path; \
                 route it through the media-dispatch gate",
            ));
        }
        let result = self.inner.execute_none(cdb);
        if let Err(error) = &result {
            self.note_unit_attention(error);
        }
        result
    }

    /// Set the per-CDB timeout on the wrapped transport.
    pub(in crate::handle) fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }

    /// Data-out media dispatch: WRITE(6) and MODE SELECT(6). Typed
    /// shim over [`Self::dispatch_media_cdb`]; it does not touch the
    /// transport itself.
    pub(in crate::handle) fn dispatch_media_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<TransferOutcome, MediaDispatchError> {
        self.dispatch_media_cdb(cdb, Some(buf)).map(|outcome| {
            outcome.expect("data-out media dispatch always returns a transfer outcome")
        })
    }

    /// No-data-phase media dispatch: WRITE FILEMARKS(6). Typed shim
    /// over [`Self::dispatch_media_cdb`]; it does not touch the
    /// transport itself.
    pub(in crate::handle) fn dispatch_media_none(
        &mut self,
        cdb: &[u8],
    ) -> Result<(), MediaDispatchError> {
        self.dispatch_media_cdb(cdb, None).map(|_| ())
    }

    /// **The media-dispatch gate** (design §6.5, D4b): the single
    /// function through which every write-direction CDB reaches the
    /// drive transport, and the only caller of `execute_out` — or of
    /// `execute_none` for a media-modifying opcode — for a drive.
    ///
    /// Order of operations, load-bearing:
    /// 1. Classify the CDB; refuse anything unclassified (fail
    ///    closed, nothing dispatched).
    /// 2. If it modifies media and this load has not fenced yet, run
    ///    the durable fence transaction. Failure ⇒ the CDB is **not**
    ///    dispatched and the flag stays clear so a retry re-runs the
    ///    fence.
    /// 3. Mark the load fenced, then dispatch. A dispatch failure
    ///    after a successful fence leaves the epoch advanced — false
    ///    invalidation, deliberate and correct.
    fn dispatch_media_cdb(
        &mut self,
        cdb: &[u8],
        data_out: Option<&[u8]>,
    ) -> Result<Option<TransferOutcome>, MediaDispatchError> {
        let Some(class) = classify_write_direction_cdb(cdb) else {
            return Err(MediaDispatchError::NotDispatched(
                TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                    "media-dispatch gate refused an unclassified write-direction CDB; \
                     classify the opcode in the gate before dispatching it",
                )),
            ));
        };
        // Phase sanity: WRITE(6)/MODE SELECT(6) carry a data-out
        // phase; WRITE FILEMARKS(6) has no data phase.
        let phase_valid = match cdb.first() {
            Some(0x0A) | Some(0x15) => data_out.is_some(),
            Some(0x10) => data_out.is_none(),
            _ => false,
        };
        if !phase_valid {
            return Err(MediaDispatchError::NotDispatched(
                TapeIoError::InvalidRequest(ScsiError::InvalidInput(
                    "media-dispatch gate refused a CDB whose data phase does not match its opcode",
                )),
            ));
        }

        if class == WriteDirectionClass::MediaModifying && !self.fenced_this_load {
            if let Err(fence_error) = self.fence.fence_media_write() {
                return Err(MediaDispatchError::NotDispatched(
                    TapeIoError::WriteFenceNotDurable(fence_error),
                ));
            }
            self.fenced_this_load = true;
        }

        let result = match data_out {
            Some(buf) => self.inner.execute_out(cdb, buf).map(Some),
            None => self.inner.execute_none(cdb).map(|()| None),
        };
        result.map_err(|error| {
            self.note_unit_attention(&error);
            MediaDispatchError::Scsi(error)
        })
    }

    /// Any UNIT ATTENTION observed on this transport means device
    /// state may have changed underneath the handle (reset, medium
    /// change, mode change). Clearing the once-per-load flag makes
    /// the next media write re-fence — false invalidation, the safe
    /// direction.
    fn note_unit_attention(&mut self, error: &ScsiError) {
        if let ScsiError::CheckCondition { sense, .. } = error {
            if decode_sense(sense).is_some_and(|decoded| decoded.key == 0x06) {
                self.fenced_this_load = false;
            }
        }
    }
}

impl fmt::Debug for MediaFencedTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaFencedTransport")
            .field("inner", &"<dyn SgTransport>")
            .field("fence", &"<dyn MediaWriteFence>")
            .field("fenced_this_load", &self.fenced_this_load)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FixtureTransport;

    fn fenced_fixture() -> (MediaFencedTransport, InMemoryCalibrationControl) {
        let control = InMemoryCalibrationControl::new();
        let mut transport = MediaFencedTransport::new(Box::new(FixtureTransport::new()));
        transport.install_media_write_fence(Box::new(control.clone()));
        (transport, control)
    }

    fn fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut v = vec![0u8; 32];
        v[0] = 0x70;
        v[2] = key & 0x0F;
        v[7] = 24;
        v[12] = asc;
        v[13] = ascq;
        v
    }

    #[test]
    fn classifier_covers_the_rem_write_surface_and_refuses_the_rest() {
        assert_eq!(
            classify_write_direction_cdb(&[0x0A, 0, 0, 0, 1, 0]),
            Some(WriteDirectionClass::MediaModifying),
            "WRITE(6)"
        );
        assert_eq!(
            classify_write_direction_cdb(&[0x10, 0, 0, 0, 0, 0]),
            Some(WriteDirectionClass::MediaModifying),
            "WRITE FILEMARKS(6), zero count included"
        );
        assert_eq!(
            classify_write_direction_cdb(&[0x15, 0x10, 0, 0, 16, 0]),
            Some(WriteDirectionClass::ConfigOnly),
            "MODE SELECT(6)"
        );
        for refused in [0x01u8, 0x04, 0x19, 0x55, 0x80, 0x8A, 0x93, 0xFF] {
            assert_eq!(
                classify_write_direction_cdb(&[refused, 0, 0, 0, 0, 0]),
                None,
                "opcode {refused:#04x} must be refused until classified deliberately"
            );
        }
        assert_eq!(classify_write_direction_cdb(&[]), None, "empty CDB");
    }

    #[test]
    fn nonmedia_path_refuses_the_media_gate_opcode_set() {
        let (mut transport, control) = fenced_fixture();
        for opcode in [0x04u8, 0x0A, 0x10, 0x15, 0x19, 0x55, 0x80, 0x8A, 0x93] {
            let err = transport
                .execute_none_nonmedia(&[opcode, 0, 0, 0, 1, 0])
                .expect_err("media opcode must be refused on the non-media path");
            assert!(matches!(err, ScsiError::InvalidInput(_)));
        }
        // Nothing reached the transport and nothing was fenced.
        assert_eq!(control.fence_transactions(), 0);

        // Genuinely non-media CDBs pass through: TUR and REWIND.
        transport
            .execute_none_nonmedia(&[0x00, 0, 0, 0, 0, 0])
            .expect("TEST UNIT READY passes");
        transport
            .execute_none_nonmedia(&[0x01, 0, 0, 0, 0, 0])
            .expect("REWIND passes");
    }

    #[test]
    fn gate_refuses_unclassified_write_direction_opcodes_without_dispatch() {
        let (mut transport, control) = fenced_fixture();
        let err = transport
            .dispatch_media_out(&[0x19, 0, 0, 0, 0, 0], &[])
            .expect_err("ERASE(6) is unclassified and must be refused");
        match err {
            MediaDispatchError::NotDispatched(TapeIoError::InvalidRequest(_)) => {}
            other => panic!("expected NotDispatched(InvalidRequest), got {other:?}"),
        }
        assert_eq!(control.fence_transactions(), 0, "no fence ran");
        assert!(!transport.media_write_fenced_this_load());
    }

    #[test]
    fn gate_refuses_mismatched_data_phase_without_dispatch_or_fence() {
        let (mut transport, control) = fenced_fixture();
        // WRITE FILEMARKS with a data-out phase.
        let err = transport
            .dispatch_media_out(&[0x10, 0, 0, 0, 1, 0], &[0u8; 4])
            .expect_err("filemarks with data-out must be refused");
        assert!(matches!(
            err,
            MediaDispatchError::NotDispatched(TapeIoError::InvalidRequest(_))
        ));
        // WRITE(6) with no data phase.
        let err = transport
            .dispatch_media_none(&[0x0A, 0, 0, 0, 1, 0])
            .expect_err("WRITE with no data phase must be refused");
        assert!(matches!(
            err,
            MediaDispatchError::NotDispatched(TapeIoError::InvalidRequest(_))
        ));
        assert_eq!(control.fence_transactions(), 0);
    }

    #[test]
    fn fence_failure_means_the_cdb_is_not_dispatched_and_retry_refences() {
        let control = InMemoryCalibrationControl::new();
        control.fail_fence("journal write failed");
        let fixture = FixtureTransport::new();
        let mut transport = MediaFencedTransport::new(Box::new(fixture));
        transport.install_media_write_fence(Box::new(control.clone()));

        let err = transport
            .dispatch_media_out(&[0x0A, 0, 0, 0, 1, 0], &[0u8; 1])
            .expect_err("failed fence must refuse dispatch");
        match err {
            MediaDispatchError::NotDispatched(TapeIoError::WriteFenceNotDurable(inner)) => {
                assert!(inner.reason().contains("journal write failed"));
            }
            other => panic!("expected WriteFenceNotDurable, got {other:?}"),
        }
        assert_eq!(control.write_epoch(), 0, "epoch untouched");
        assert!(
            !transport.media_write_fenced_this_load(),
            "flag stays clear so the next attempt re-runs the fence"
        );

        // Recovery: clear the injected failure and the same load
        // fences exactly once on the next write.
        control.clear_fence_failure();
        transport
            .dispatch_media_out(&[0x0A, 0, 0, 0, 1, 0], &[0u8; 1])
            .expect("write proceeds once the fence is durable");
        assert_eq!(control.fence_transactions(), 1);
        assert_eq!(control.write_epoch(), 1);
    }

    #[test]
    fn allocator_exhaustion_fails_closed_and_refuses_dispatch() {
        let control = InMemoryCalibrationControl::new();
        control.set_write_epoch(u64::MAX);
        let mut transport = MediaFencedTransport::new(Box::new(FixtureTransport::new()));
        transport.install_media_write_fence(Box::new(control.clone()));

        let err = transport
            .dispatch_media_out(&[0x0A, 0, 0, 0, 1, 0], &[0u8; 1])
            .expect_err("exhausted allocator must fail closed");
        assert!(matches!(
            err,
            MediaDispatchError::NotDispatched(TapeIoError::WriteFenceNotDurable(_))
        ));
        assert_eq!(control.write_epoch(), u64::MAX, "no wrap");
        assert!(!transport.media_write_fenced_this_load());
    }

    #[test]
    fn unit_attention_on_dispatch_clears_the_load_local_flag() {
        struct UnitAttentionOnce {
            inner: FixtureTransport,
            sense: Option<Vec<u8>>,
        }
        impl SgTransport for UnitAttentionOnce {
            fn execute_in(
                &mut self,
                cdb: &[u8],
                buf: &mut [u8],
            ) -> Result<TransferOutcome, ScsiError> {
                self.inner.execute_in(cdb, buf)
            }
            fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
                self.inner.execute_none(cdb)
            }
            fn execute_out(
                &mut self,
                cdb: &[u8],
                buf: &[u8],
            ) -> Result<TransferOutcome, ScsiError> {
                if let Some(sense) = self.sense.take() {
                    return Err(ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: 0,
                    });
                }
                self.inner.execute_out(cdb, buf)
            }
        }

        let control = InMemoryCalibrationControl::new();
        let mut transport = MediaFencedTransport::new(Box::new(UnitAttentionOnce {
            inner: FixtureTransport::new(),
            sense: Some(fixed_sense(0x06, 0x29, 0x00)),
        }));
        transport.install_media_write_fence(Box::new(control.clone()));

        // First write: fence runs (epoch 1), dispatch returns UNIT
        // ATTENTION — the flag must clear so the next write, which
        // may land on different media, re-fences.
        let err = transport
            .dispatch_media_out(&[0x0A, 0, 0, 0, 1, 0], &[0u8; 1])
            .expect_err("UA surfaces as a dispatch error");
        assert!(matches!(err, MediaDispatchError::Scsi(_)));
        assert_eq!(
            control.fence_transactions(),
            1,
            "epoch advanced pre-dispatch"
        );
        assert!(
            !transport.media_write_fenced_this_load(),
            "UA cleared the flag"
        );

        // Next write re-fences.
        transport
            .dispatch_media_out(&[0x0A, 0, 0, 0, 1, 0], &[0u8; 1])
            .expect("clean write after UA");
        assert_eq!(control.fence_transactions(), 2);
    }

    #[test]
    fn wrap_map_servability_is_exact_epoch_equality() {
        assert!(wrap_map_is_servable(0, 0));
        assert!(wrap_map_is_servable(7, 7));
        assert!(!wrap_map_is_servable(6, 7), "stale map is never served");
        assert!(
            !wrap_map_is_servable(7, 6),
            "a map from a future epoch is equally invalid"
        );
    }

    #[test]
    fn stub_fence_transaction_advances_epoch_and_generation_and_uncalibrates() {
        let control = InMemoryCalibrationControl::new();
        let map_epoch = control.mark_calibrated();
        assert!(control.is_calibrated());
        assert!(wrap_map_is_servable(map_epoch, control.write_epoch()));

        let mut fence: Box<dyn MediaWriteFence> = Box::new(control.clone());
        fence.fence_media_write().expect("fence transaction");

        assert_eq!(control.write_epoch(), 1);
        assert_eq!(control.calibration_generation(), 1);
        assert!(!control.is_calibrated(), "fence marks uncalibrated");
        assert!(
            !wrap_map_is_servable(map_epoch, control.write_epoch()),
            "the pre-write map is never served after the fence"
        );
    }
}
