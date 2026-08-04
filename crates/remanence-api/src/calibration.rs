//! Load-time wrap-map harvest and the serve path — design
//! `design-read-ordering.md` §6.5, prompt P4.
//!
//! **Harvest-and-install is one act.** [`harvest_and_install_calibration`]
//! is the only place in the codebase that installs a real
//! [`remanence_state::VolumeMediaWriteFence`] into a `DriveHandle`, and
//! it does so *before* any map can be stored: the fence install is the
//! first thing the function does after reading the harvest epoch, and
//! the map row is written only after the durable calibrated transition.
//! A harvested map without an installed fence, or a fence installed
//! without a fresh harvest, would break `NoCalibrationControl`'s
//! soundness argument (see its doc comment in the media gate), so
//! neither half is callable on its own from anywhere else.
//!
//! **Harvest timing is dictated by the standard, not chosen.** REOWP
//! data "is valid at load" and "becomes stale on any write operation";
//! re-issuing does not refresh it. So the call sites are the drive
//! actors' session-open paths, gated on `needs_drive_load` — the
//! moment a cartridge newly arrives in a drive, after readiness and
//! identity, before any media-modifying CDB. There is no mid-load
//! refresh: a volume written during a load stays uncalibrated until
//! its next load, and nothing in this module can be reached to
//! re-calibrate it earlier (checkpoint, close and abort recovery do
//! not call in here).
//!
//! **The wire-to-descriptor conversion lives here.** `remanence-scsi`
//! reports `u16` wrap/partition fields as the wire defines them;
//! `remanence-order` models them as `u32` and must stay free of any
//! dependency on the SCSI crate (and vice versa). [`descriptor_from_wire`]
//! is that single mapping.

use remanence_library::{DriveHandle, TapeIoError};
use remanence_order::{lookup_media_code, GeometryLookup, ReowpDescriptor, WrapMap};
use remanence_scsi::read_end_of_wrap_position::WrapDescriptor;
use remanence_state::{
    CalibrationControlStore, CatalogIndex, HarvestTransition, StateError, StoredWrapDescriptor,
    VolumeCalibrationState, WrapMapCacheRecord, WritePathTrust,
};

use crate::TapeUuid;

/// Convert one on-the-wire REOWP descriptor into the planner's shape.
/// This u16-to-u32 widening is owned by the harvest layer precisely so
/// that `remanence-scsi` and `remanence-order` never depend on each
/// other.
pub(crate) fn descriptor_from_wire(descriptor: WrapDescriptor) -> ReowpDescriptor {
    ReowpDescriptor {
        partition: u32::from(descriptor.partition),
        wrap_number: u32::from(descriptor.wrap_number),
        end_loi: descriptor.end_loi,
    }
}

fn stored_from_wire(descriptor: WrapDescriptor) -> StoredWrapDescriptor {
    StoredWrapDescriptor {
        partition: u32::from(descriptor.partition),
        wrap_number: u32::from(descriptor.wrap_number),
        end_loi: descriptor.end_loi,
    }
}

fn order_from_stored(descriptor: StoredWrapDescriptor) -> ReowpDescriptor {
    ReowpDescriptor {
        partition: descriptor.partition,
        wrap_number: descriptor.wrap_number,
        end_loi: descriptor.end_loi,
    }
}

/// What the load harvest concluded for this load. Mirrors the §6.5
/// transition rows; P5 maps these onto `PlanBatchRead` statuses.
#[derive(Debug)]
pub(crate) enum HarvestOutcome {
    /// Map harvested, validated, stored; fence installed; volume
    /// calibrated.
    Calibrated {
        /// Epoch the stored map is bound to.
        write_epoch: u64,
        /// Generation stamped on the calibrated transition.
        calibration_generation: u64,
        /// Wraps in the stored map, EOD wrap included.
        wrap_count: u32,
    },
    /// The format is recognised but unsupported (resolved before
    /// issuing REOWP), or the drive rejected the command at runtime.
    /// Holds for this load.
    UnsupportedFormat {
        /// Generation stamped on the transition.
        calibration_generation: u64,
        /// Human-readable cause, never parsed.
        detail: String,
    },
    /// Transport failure, parse/validation failure, or a
    /// trust/epoch refusal: the volume stays uncalibrated and no map
    /// was stored.
    Uncalibrated {
        /// Generation stamped on the transition.
        calibration_generation: u64,
        /// Human-readable cause, never parsed.
        detail: String,
    },
    /// The durable calibration store itself refused the transition
    /// (I/O failure, allocator exhaustion). Nothing was recorded; the
    /// volume's prior durable state stands. The fence was still
    /// installed, so any write in this load will fail closed rather
    /// than dispatch unfenced.
    StoreUnavailable {
        /// The store error, for the log.
        detail: String,
    },
}

/// Harvest the loaded volume's wrap map and install its media-write
/// fence — one act, at load, before any write (design §6.5).
///
/// Sequence, load-bearing:
/// 1. read the durable epoch the harvest will be judged against;
/// 2. **install the volume's real fence** — unconditionally, before
///    any command and before any map storage, so there is no state in
///    which a stored map exists without its fence;
/// 3. resolve the barcode's media code: a recognised-but-unsupported
///    format records `UNSUPPORTED_FORMAT` for this load **without
///    issuing REOWP at all**;
/// 4. probe RSOC; a drive that affirms the command is missing takes
///    the same unsupported transition without issuing the command;
/// 5. issue REOWP; drive rejection → unsupported for this load,
///    transport failure → uncalibrated;
/// 6. validate descriptors through `WrapMap::from_descriptors`
///    (`remanence-order` is the single validation funnel) — failure →
///    uncalibrated;
/// 7. record the durable harvest transition, which re-checks epoch
///    and trust inside the store's lock; only a calibrated transition
///    stores the map row, stamped with that epoch and generation.
///
/// The raw descriptors are stored exactly as harvested and
/// `mapped_extent_lba` separately (the EOD descriptor's `end_loi`);
/// wrap starts are derived at planning time and no synthetic EOD
/// boundary exists anywhere in storage.
pub(crate) fn harvest_and_install_calibration(
    drive: &mut DriveHandle,
    index: &mut CatalogIndex,
    store: &CalibrationControlStore,
    tape_uuid: TapeUuid,
    barcode: Option<&str>,
) -> HarvestOutcome {
    let harvest_epoch = store.row(tape_uuid).write_epoch;

    // (2) The fence, first. Installing it is pure in-memory state on
    // the drive handle and cannot fail; from here on, the first
    // media-modifying CDB of this load durably advances the epoch or
    // is not dispatched.
    drive.install_media_write_fence(Box::new(store.volume_fence(tape_uuid)));

    // (3) Recognised-but-unsupported format: no REOWP is issued.
    if let Some(media_code) = barcode.and_then(media_code_of) {
        if let GeometryLookup::Unsupported(row) = lookup_media_code(media_code) {
            let detail = format!(
                "media code {media_code} is recognised but unsupported: {}",
                row.reason
            );
            return match store.record_unsupported_format(tape_uuid) {
                Ok(generation) => HarvestOutcome::UnsupportedFormat {
                    calibration_generation: generation,
                    detail,
                },
                Err(err) => store_unavailable(err),
            };
        }
    }

    // (4) Capability probe. Only an explicit "not supported" answer
    // short-circuits; an indeterminate or failed probe degrades to
    // the runtime-rejection path below rather than guessing.
    if let Ok(remanence_scsi::report_supported_opcodes::OpcodeSupport::NotSupported) =
        drive.probe_read_end_of_wrap_position_support()
    {
        let detail = "drive reports READ END OF WRAP POSITION unsupported (RSOC)".to_string();
        return match store.record_unsupported_format(tape_uuid) {
            Ok(generation) => HarvestOutcome::UnsupportedFormat {
                calibration_generation: generation,
                detail,
            },
            Err(err) => store_unavailable(err),
        };
    }

    // (5) The harvest read.
    let positions = match drive.read_end_of_wrap_position() {
        Ok(positions) => positions,
        Err(err @ TapeIoError::CheckCondition(_)) => {
            // Runtime rejection of a nominally supported format —
            // defensive handling, recorded as unsupported for this
            // load with the command failure in the detail.
            let detail = format!("drive rejected READ END OF WRAP POSITION: {err}");
            return match store.record_unsupported_format(tape_uuid) {
                Ok(generation) => HarvestOutcome::UnsupportedFormat {
                    calibration_generation: generation,
                    detail,
                },
                Err(store_err) => store_unavailable(store_err),
            };
        }
        Err(err) => {
            // Transport or wire-parse failure: uncalibrated, old map
            // not served, retried at the next load.
            let detail = format!("wrap-map harvest failed: {err}");
            return match store.record_harvest_failure(tape_uuid) {
                Ok(generation) => HarvestOutcome::Uncalibrated {
                    calibration_generation: generation,
                    detail,
                },
                Err(store_err) => store_unavailable(store_err),
            };
        }
    };

    // (6) Descriptor validation through the planner's single funnel.
    let order_descriptors: Vec<ReowpDescriptor> = positions
        .descriptors()
        .iter()
        .copied()
        .map(descriptor_from_wire)
        .collect();
    let map = match WrapMap::from_descriptors(&order_descriptors) {
        Ok(map) => map,
        Err(err) => {
            let detail = format!("harvested descriptors failed validation: {err}");
            return match store.record_harvest_failure(tape_uuid) {
                Ok(generation) => HarvestOutcome::Uncalibrated {
                    calibration_generation: generation,
                    detail,
                },
                Err(store_err) => store_unavailable(store_err),
            };
        }
    };

    // (7) The durable transition, judged inside the store's lock.
    match store.record_harvest_success(tape_uuid, harvest_epoch) {
        Ok(HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        }) => {
            let record = WrapMapCacheRecord {
                tape_uuid,
                descriptors: positions
                    .descriptors()
                    .iter()
                    .copied()
                    .map(stored_from_wire)
                    .collect(),
                mapped_extent_lba: map.mapped_extent_lba(),
                write_epoch,
                calibration_generation,
                harvested_at_utc: now_rfc3339_or_epoch(),
            };
            match index.upsert_wrap_map(&record) {
                Ok(()) => HarvestOutcome::Calibrated {
                    write_epoch,
                    calibration_generation,
                    wrap_count: map.wrap_count(),
                },
                Err(err) => {
                    // The control row says calibrated but the map row
                    // is missing; the serve path fails toward
                    // NotServable(NoMap), never toward a wrong map.
                    HarvestOutcome::Uncalibrated {
                        calibration_generation,
                        detail: format!("wrap-map projection store failed: {err}"),
                    }
                }
            }
        }
        Ok(HarvestTransition::RefusedUncalibrated {
            calibration_generation,
            refusal,
        }) => HarvestOutcome::Uncalibrated {
            calibration_generation,
            detail: format!("REOWP succeeded but the result is not entitled to trust: {refusal:?}"),
        },
        Err(err) => store_unavailable(err),
    }
}

fn store_unavailable(err: StateError) -> HarvestOutcome {
    HarvestOutcome::StoreUnavailable {
        detail: err.to_string(),
    }
}

/// The barcode's two-character media-code suffix, or `None` when the
/// barcode is too short or not ASCII. One rule, shared by the load
/// harvest and the `PlanBatchRead` cartridge-fact resolution.
pub(crate) fn media_code_of(barcode: &str) -> Option<&str> {
    let trimmed = barcode.trim();
    if trimmed.len() < 2 || !trimmed.is_ascii() {
        return None;
    }
    Some(&trimmed[trimmed.len() - 2..])
}

fn now_rfc3339_or_epoch() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Why a wrap map was not served. **Invalid** (`InvalidEpochMismatch`)
/// and a *coverage gap* are different things with different remedies:
/// invalidity is decided here and the map is never served; a coverage
/// gap is a per-target property of a *valid, served* map, surfaced by
/// `WrapMap::locate` / the planner as a `CoverageError` against
/// `mapped_extent_lba`, and never by this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WrapMapServeRefusal {
    /// The control row is `UnsupportedFormat` for the current load.
    UnsupportedFormat,
    /// `write_path_trust` is `OutOfBandWritePossible`.
    Untrusted,
    /// The control row is `Uncalibrated` — no harvest has succeeded
    /// under the current epoch.
    NotCalibrated,
    /// No map row exists in the projection (evicted, or never
    /// harvested).
    NoMap,
    /// A map row exists but its epoch does not equal the durable
    /// epoch: the map is **invalid** and is never served.
    InvalidEpochMismatch,
    /// The cached row exists but no longer decodes/validates. Treated
    /// as absent; the next load harvest rewrites it.
    CorruptMapRow,
}

/// Result of asking for a volume's servable wrap map.
#[derive(Debug)]
pub(crate) enum WrapMapServeOutcome {
    /// The map may be used for planning. Coverage of individual
    /// targets is still the planner's job.
    Servable {
        /// The validated map, rebuilt from the raw stored
        /// descriptors at serve time (wrap starts are derived here,
        /// never stored).
        map: WrapMap,
        /// Epoch the map is bound to. Asserted by the lifecycle
        /// tests; the RPC serve path does not re-check it because
        /// `is_map_servable` already did, inside this function.
        #[allow(dead_code)]
        write_epoch: u64,
        /// Generation for calibration-derived caching.
        calibration_generation: u64,
    },
    /// The map may not be used; `PlanBatchRead` maps the refusal onto
    /// its status vocabulary.
    NotServable {
        /// Current generation of the volume's control row.
        calibration_generation: u64,
        /// Why serving was refused.
        refusal: WrapMapServeRefusal,
    },
}

/// Decide whether the volume's cached wrap map may be served, and
/// rebuild it from the raw descriptors when it may. **Never touches a
/// drive** — the inputs are the projection and the durable control
/// row, nothing else; a coverage gap in particular is detected against
/// the map's `mapped_extent_lba` without any device access.
///
/// The epoch comparison routes through
/// [`CalibrationControlStore::is_map_servable`], which itself routes
/// through `remanence_library::wrap_map_is_servable` — one predicate,
/// not a reimplementation.
pub(crate) fn servable_wrap_map(
    index: &CatalogIndex,
    store: &CalibrationControlStore,
    tape_uuid: TapeUuid,
) -> Result<WrapMapServeOutcome, StateError> {
    let row = store.row(tape_uuid);
    let refuse = |refusal| {
        Ok(WrapMapServeOutcome::NotServable {
            calibration_generation: row.calibration_generation,
            refusal,
        })
    };
    if row.state == VolumeCalibrationState::UnsupportedFormat {
        return refuse(WrapMapServeRefusal::UnsupportedFormat);
    }
    if row.write_path_trust != WritePathTrust::Trusted {
        return refuse(WrapMapServeRefusal::Untrusted);
    }
    if row.state != VolumeCalibrationState::Calibrated {
        return refuse(WrapMapServeRefusal::NotCalibrated);
    }
    let Some(cached) = index.get_wrap_map(&tape_uuid)? else {
        return refuse(WrapMapServeRefusal::NoMap);
    };
    if !store.is_map_servable(tape_uuid, cached.write_epoch) {
        return refuse(WrapMapServeRefusal::InvalidEpochMismatch);
    }
    let order_descriptors: Vec<ReowpDescriptor> = cached
        .descriptors
        .iter()
        .copied()
        .map(order_from_stored)
        .collect();
    match WrapMap::from_descriptors(&order_descriptors) {
        Ok(map) => Ok(WrapMapServeOutcome::Servable {
            map,
            write_epoch: cached.write_epoch,
            calibration_generation: row.calibration_generation,
        }),
        Err(_) => refuse(WrapMapServeRefusal::CorruptMapRow),
    }
}

#[cfg(test)]
mod tests {
    //! The §6.5 calibration transition table, one named test per row
    //! exercised at this layer, plus the P4 prompt's specific tests.
    //! Rows owned by `remanence-state` (projection rebuild, catalog
    //! reset, startup/orphan recovery) are tested there:
    //! `projection_rebuild_evicts_wrap_maps_and_uncalibrates`,
    //! `catalog_reset_evicts_maps_and_never_reissues_generations`,
    //! `startup_replay_invalidates_possibly_written_volumes`.

    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use remanence_library::transport::TransferOutcome;
    use remanence_library::{DriveHandle, ScsiError, SgTransport};
    use remanence_order::EodDenominatorBasis;
    use remanence_state::{
        CalibrationControlStore, CatalogIndex, VolumeCalibrationState, WritePathTrust,
    };

    use super::*;

    const TAPE: TapeUuid = [0xA4; 16];

    fn lto9_inquiry() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
    }

    fn vpd80_response(serial: &str) -> Vec<u8> {
        let bytes = serial.as_bytes();
        let mut v = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
        v.extend_from_slice(bytes);
        v
    }

    fn rp_long_response() -> Vec<u8> {
        vec![0u8; 32]
    }

    fn rsoc_response(support: u8) -> Vec<u8> {
        let mut buf = vec![0x00, support, 0x00, 0x0C];
        buf.extend_from_slice(&[0u8; 12]);
        buf
    }

    /// Build a long-form REOWP response from `(wrap, end_loi)` pairs.
    fn reowp_response(descriptors: &[(u16, u64)]) -> Vec<u8> {
        let data_len = 2 + descriptors.len() * 12;
        let mut buf = Vec::new();
        buf.extend_from_slice(&(data_len as u16).to_be_bytes());
        buf.extend_from_slice(&[0x00, 0x00]);
        for (wrap, end_loi) in descriptors {
            buf.extend_from_slice(&wrap.to_be_bytes());
            buf.extend_from_slice(&0u16.to_be_bytes());
            buf.extend_from_slice(&[0x00, 0x00]);
            buf.extend_from_slice(&end_loi.to_be_bytes()[2..8]);
        }
        buf
    }

    fn illegal_request_sense() -> Vec<u8> {
        let mut v = vec![0u8; 32];
        v[0] = 0x70;
        v[2] = 0x05; // ILLEGAL REQUEST
        v[7] = 24;
        v[12] = 0x20; // INVALID COMMAND OPERATION CODE
        v
    }

    /// How the scripted drive answers a REOWP read.
    #[derive(Clone)]
    enum ReowpScript {
        Respond(Vec<u8>),
        RejectCheckCondition,
        FailTransport,
    }

    /// Shared controls for [`ScriptedTransport`], visible to the test
    /// after the drive takes ownership of the transport.
    #[derive(Clone)]
    struct ScriptControls {
        inner: Arc<Mutex<ScriptState>>,
    }

    struct ScriptState {
        reowp: ReowpScript,
        rsoc: Vec<u8>,
        reowp_reads: usize,
        rsoc_probes: usize,
    }

    impl ScriptControls {
        fn new(reowp: ReowpScript) -> Self {
            Self {
                inner: Arc::new(Mutex::new(ScriptState {
                    reowp,
                    rsoc: rsoc_response(0b011),
                    reowp_reads: 0,
                    rsoc_probes: 0,
                })),
            }
        }

        fn set_reowp(&self, script: ReowpScript) {
            self.inner.lock().expect("script lock").reowp = script;
        }

        fn set_rsoc(&self, response: Vec<u8>) {
            self.inner.lock().expect("script lock").rsoc = response;
        }

        fn reowp_reads(&self) -> usize {
            self.inner.lock().expect("script lock").reowp_reads
        }

        fn rsoc_probes(&self) -> usize {
            self.inner.lock().expect("script lock").rsoc_probes
        }
    }

    /// Answers by opcode instead of a fragile response queue: INQUIRY
    /// and VPD from the golden LTO-9 fixture, READ POSITION with a
    /// zeroed long response, RSOC and REOWP from the script, every
    /// no-data CDB with success.
    struct ScriptedTransport {
        controls: ScriptControls,
        vpd_pending: bool,
    }

    impl SgTransport for ScriptedTransport {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            let response: Vec<u8> = match cdb.first().copied() {
                Some(0x12) => {
                    // INQUIRY: standard first, then VPD page 0x80.
                    if cdb[1] & 0x01 == 0x01 {
                        vpd80_response("DRV_CAL_TEST")
                    } else {
                        self.vpd_pending = true;
                        lto9_inquiry()
                    }
                }
                Some(0x34) => rp_long_response(),
                Some(0xA3) if cdb[1] & 0x1F == 0x0C => {
                    let mut state = self.controls.inner.lock().expect("script lock");
                    state.rsoc_probes += 1;
                    state.rsoc.clone()
                }
                Some(0xA3) if cdb[1] & 0x1F == 0x1F => {
                    let mut state = self.controls.inner.lock().expect("script lock");
                    state.reowp_reads += 1;
                    match state.reowp.clone() {
                        ReowpScript::Respond(bytes) => bytes,
                        ReowpScript::RejectCheckCondition => {
                            return Err(ScsiError::CheckCondition {
                                sense: illegal_request_sense(),
                                bytes_transferred: 0,
                            });
                        }
                        ReowpScript::FailTransport => {
                            return Err(ScsiError::InvalidInput(
                                "injected transport failure for REOWP",
                            ));
                        }
                    }
                }
                _ => Vec::new(),
            };
            let n = response.len().min(buf.len());
            buf[..n].copy_from_slice(&response[..n]);
            Ok(TransferOutcome::clean(n as u32))
        }

        fn execute_none(&mut self, _cdb: &[u8]) -> Result<(), ScsiError> {
            Ok(())
        }

        fn execute_out(&mut self, _cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            Ok(TransferOutcome::clean(buf.len() as u32))
        }
    }

    struct World {
        _dir: tempfile::TempDir,
        store: CalibrationControlStore,
        index: CatalogIndex,
    }

    fn world() -> World {
        let dir = tempfile::Builder::new()
            .prefix("rem-api-calibration")
            .tempdir()
            .expect("tempdir");
        let store =
            CalibrationControlStore::open(dir.path().join("calibration")).expect("open store");
        let index = CatalogIndex::open(dir.path().join("index.sqlite")).expect("open index");
        World {
            _dir: dir,
            store,
            index,
        }
    }

    fn drive(controls: &ScriptControls) -> DriveHandle {
        DriveHandle::open_standalone_with_transport(
            Path::new("/dev/sg-cal-test"),
            Box::new(ScriptedTransport {
                controls: controls.clone(),
                vpd_pending: false,
            }),
        )
        .expect("standalone drive opens over scripted transport")
    }

    /// Three wraps: two completed (documented LTO-8 end LOIs) plus the
    /// EOD wrap at 500,000.
    fn three_wrap_script() -> ScriptControls {
        ScriptControls::new(ReowpScript::Respond(reowp_response(&[
            (0, 207_516),
            (1, 415_522),
            (2, 500_000),
        ])))
    }

    fn harvest(
        world: &mut World,
        drive: &mut DriveHandle,
        barcode: Option<&str>,
    ) -> HarvestOutcome {
        harvest_and_install_calibration(drive, &mut world.index, &world.store, TAPE, barcode)
    }

    // -----------------------------------------------------------------
    //  §6.5 row 1: harvest succeeds, epoch matches, trust TRUSTED
    // -----------------------------------------------------------------

    #[test]
    fn row1_harvest_success_calibrates_and_map_is_served() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);

        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        let HarvestOutcome::Calibrated {
            write_epoch,
            calibration_generation,
            wrap_count,
        } = outcome
        else {
            panic!("expected Calibrated, got {outcome:?}");
        };
        assert_eq!(write_epoch, 0);
        assert!(calibration_generation > 0);
        assert_eq!(wrap_count, 3);
        assert_eq!(controls.reowp_reads(), 1);

        // Next PlanBatchRead: normal planning — the serve path hands
        // out the validated map under the same epoch and generation.
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::Servable {
                map,
                write_epoch: served_epoch,
                calibration_generation: served_generation,
            } => {
                assert_eq!(served_epoch, write_epoch);
                assert_eq!(served_generation, calibration_generation);
                assert_eq!(map.wrap_count(), 3);
                assert_eq!(map.mapped_extent_lba(), 500_000);
                // Wrap starts are derived at serve time, not stored:
                // wrap 1 starts one past wrap 0's inclusive end.
                assert_eq!(map.wrap_starts(), &[0, 207_517, 415_523]);
            }
            other => panic!("expected Servable, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    //  §6.5 row 2: drive rejects REOWP for the loaded format
    // -----------------------------------------------------------------

    #[test]
    fn row2_drive_rejection_is_unsupported_format_for_this_load() {
        let mut world = world();
        let controls = ScriptControls::new(ReowpScript::RejectCheckCondition);
        let mut drive = drive(&controls);

        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        let HarvestOutcome::UnsupportedFormat {
            calibration_generation,
            detail,
        } = outcome
        else {
            panic!("expected UnsupportedFormat, got {outcome:?}");
        };
        assert!(calibration_generation > 0, "fresh non-zero generation");
        assert!(detail.contains("rejected"), "detail names the rejection");
        assert_eq!(controls.reowp_reads(), 1, "the command was issued once");
        assert!(
            world.index.get_wrap_map(&TAPE).expect("get map").is_none(),
            "no map is stored"
        );

        // Next PlanBatchRead: UNAVAILABLE_UNSUPPORTED_FORMAT with the
        // new generation.
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable {
                calibration_generation: served_generation,
                refusal,
            } => {
                assert_eq!(refusal, WrapMapServeRefusal::UnsupportedFormat);
                assert_eq!(served_generation, calibration_generation);
            }
            other => panic!("expected NotServable, got {other:?}"),
        }
    }

    /// The earlier of the two unsupported-format paths: cartridge-fact
    /// resolution recognises an unsupported format and **no REOWP is
    /// issued at all**.
    #[test]
    fn row2b_recognised_unsupported_format_never_issues_reowp() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);

        // M8: recognised but unsupported (conflicting published
        // geometry, D4a).
        let outcome = harvest(&mut world, &mut drive, Some("ARC001M8"));
        let HarvestOutcome::UnsupportedFormat {
            calibration_generation,
            detail,
        } = outcome
        else {
            panic!("expected UnsupportedFormat, got {outcome:?}");
        };
        assert!(calibration_generation > 0);
        assert!(detail.contains("M8"));
        assert_eq!(
            controls.reowp_reads(),
            0,
            "REOWP must not be issued for a recognised-unsupported format"
        );
        assert_eq!(
            controls.rsoc_probes(),
            0,
            "not even the capability probe goes out"
        );
        assert_eq!(
            world.store.row(TAPE).state,
            VolumeCalibrationState::UnsupportedFormat
        );
    }

    /// The RSOC probe variant: a drive that answers "not supported"
    /// takes the unsupported transition without the doomed command.
    #[test]
    fn row2c_rsoc_not_supported_short_circuits_without_reowp() {
        let mut world = world();
        let controls = three_wrap_script();
        controls.set_rsoc(rsoc_response(0b001)); // NOT SUPPORTED
        let mut drive = drive(&controls);

        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        assert!(
            matches!(outcome, HarvestOutcome::UnsupportedFormat { .. }),
            "got {outcome:?}"
        );
        assert_eq!(controls.rsoc_probes(), 1);
        assert_eq!(controls.reowp_reads(), 0, "REOWP never issued");
    }

    // -----------------------------------------------------------------
    //  §6.5 row 3: transport / parse / validation failure
    // -----------------------------------------------------------------

    #[test]
    fn row3_transport_failure_leaves_volume_uncalibrated() {
        let mut world = world();
        let controls = ScriptControls::new(ReowpScript::FailTransport);
        let mut drive = drive(&controls);

        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        let HarvestOutcome::Uncalibrated {
            calibration_generation,
            ..
        } = outcome
        else {
            panic!("expected Uncalibrated, got {outcome:?}");
        };
        assert!(calibration_generation > 0);
        assert!(world.index.get_wrap_map(&TAPE).expect("get").is_none());
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::NotCalibrated);
            }
            other => panic!("expected NotServable, got {other:?}"),
        }
    }

    #[test]
    fn row3b_descriptor_validation_failure_leaves_volume_uncalibrated() {
        let mut world = world();
        // Wire-valid response whose descriptors fail §6.4 validation:
        // the EOD wrap has no positive span (extent == wrap start).
        let controls = ScriptControls::new(ReowpScript::Respond(reowp_response(&[
            (0, 207_516),
            (1, 207_516),
        ])));
        let mut drive = drive(&controls);

        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        let HarvestOutcome::Uncalibrated { detail, .. } = outcome else {
            panic!("expected Uncalibrated, got {outcome:?}");
        };
        assert!(
            detail.contains("validation"),
            "detail names validation: {detail}"
        );
        assert!(world.index.get_wrap_map(&TAPE).expect("get").is_none());
    }

    /// The prompt's transient-failure test: a failed harvest must not
    /// fall back to serving the map from a previous load, even though
    /// that map's epoch still matches (no write happened).
    #[test]
    fn transient_harvest_failure_does_not_serve_the_old_map() {
        let mut world = world();
        let controls = three_wrap_script();

        // Load 1: calibrate.
        let mut drive1 = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive1, Some("ACM001L8")),
            HarvestOutcome::Calibrated { .. }
        ));
        drop(drive1);
        assert!(matches!(
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve"),
            WrapMapServeOutcome::Servable { .. }
        ));

        // Load 2: the harvest fails in transport. The old map row is
        // still cached and its epoch still matches — and it must NOT
        // be served, because the harvest-failure transition durably
        // uncalibrated the volume.
        controls.set_reowp(ReowpScript::FailTransport);
        let mut drive2 = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive2, Some("ACM001L8")),
            HarvestOutcome::Uncalibrated { .. }
        ));
        assert!(
            world.index.get_wrap_map(&TAPE).expect("get").is_some(),
            "the stale row still exists in the projection"
        );
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::NotCalibrated);
            }
            other => panic!("old map must not be served, got {other:?}"),
        }

        // Load 3: a clean harvest recalibrates.
        controls.set_reowp(ReowpScript::Respond(reowp_response(&[
            (0, 207_516),
            (1, 415_522),
            (2, 500_000),
        ])));
        let mut drive3 = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive3, Some("ACM001L8")),
            HarvestOutcome::Calibrated { .. }
        ));
        assert!(matches!(
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve"),
            WrapMapServeOutcome::Servable { .. }
        ));
    }

    // -----------------------------------------------------------------
    //  §6.5 "REOWP succeeds but trust disagrees" row
    // -----------------------------------------------------------------

    #[test]
    fn successful_harvest_on_untrusted_volume_stays_uncalibrated() {
        let mut world = world();
        world
            .store
            .set_write_path_trust(TAPE, WritePathTrust::OutOfBandWritePossible)
            .expect("set trust");
        let controls = three_wrap_script();
        let mut drive = drive(&controls);

        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        let HarvestOutcome::Uncalibrated { detail, .. } = outcome else {
            panic!("succeeding at the command is not entitlement to trust it; got {outcome:?}");
        };
        assert!(detail.contains("Untrusted"), "detail: {detail}");
        assert_eq!(controls.reowp_reads(), 1, "the command did succeed");
        assert!(
            world.index.get_wrap_map(&TAPE).expect("get").is_none(),
            "no map is stored for an untrusted volume"
        );
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::Untrusted);
            }
            other => panic!("expected NotServable, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    //  §6.5 row 4: the pre-dispatch write fence trips
    // -----------------------------------------------------------------

    #[test]
    fn row4_write_fence_trip_invalidates_for_the_rest_of_the_load() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);
        let outcome = harvest(&mut world, &mut drive, Some("ACM001L8"));
        let HarvestOutcome::Calibrated { write_epoch, .. } = outcome else {
            panic!("expected Calibrated, got {outcome:?}");
        };

        // The first media-modifying CDB of the load runs the real
        // fence installed by the harvest — one act, so the write
        // durably advances THIS volume's epoch.
        drive.write_block(&[0u8; 64]).expect("write");

        let row = world.store.row(TAPE);
        assert_eq!(row.write_epoch, write_epoch + 1, "durable epoch advanced");
        assert_eq!(row.state, VolumeCalibrationState::Uncalibrated);
        assert!(
            !world.store.is_map_servable(TAPE, write_epoch),
            "the pre-write map is invalid"
        );
        // Next PlanBatchRead for the rest of the load: uncalibrated.
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::NotCalibrated);
            }
            other => panic!("expected NotServable after a write, got {other:?}"),
        }

        // No same-load refresh exists: nothing recalibrates until the
        // next load's harvest (modelled by a fresh harvest call).
        let mut next_load = super::tests::drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut next_load, Some("ACM001L8")),
            HarvestOutcome::Calibrated { .. }
        ));
        assert!(matches!(
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve"),
            WrapMapServeOutcome::Servable { .. }
        ));
    }

    // -----------------------------------------------------------------
    //  §6.5 row 5: wrap-map eviction
    // -----------------------------------------------------------------

    #[test]
    fn row5_single_map_eviction_uncalibrates_but_control_row_remains() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);
        let HarvestOutcome::Calibrated {
            write_epoch,
            calibration_generation,
            ..
        } = harvest(&mut world, &mut drive, Some("ACM001L8"))
        else {
            panic!("expected Calibrated");
        };

        assert!(world.index.delete_wrap_map(&TAPE).expect("evict"));
        let generation = world.store.record_map_evicted(TAPE).expect("record");
        assert!(generation > calibration_generation);

        let row = world.store.row(TAPE);
        assert_eq!(row.state, VolumeCalibrationState::Uncalibrated);
        assert_eq!(
            row.write_epoch, write_epoch,
            "the control row remains, epoch untouched"
        );
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::NotCalibrated);
            }
            other => panic!("expected NotServable, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------
    //  Harvest-and-install is ONE act — the P3-named gate criterion
    // -----------------------------------------------------------------

    /// A write after a *failed* harvest still advances the volume's
    /// durable epoch: the fence is installed unconditionally at the
    /// harvest, before any outcome branches, so there is no path on
    /// which a load has a map (or later gets one) without its fence.
    #[test]
    fn fence_is_installed_even_when_the_harvest_fails() {
        let mut world = world();
        let controls = ScriptControls::new(ReowpScript::FailTransport);
        let mut drive = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive, Some("ACM001L8")),
            HarvestOutcome::Uncalibrated { .. }
        ));
        let epoch_before = world.store.row(TAPE).write_epoch;

        drive.write_block(&[0u8; 64]).expect("write");

        assert_eq!(
            world.store.row(TAPE).write_epoch,
            epoch_before + 1,
            "the real fence was installed by the failed harvest and ran pre-dispatch"
        );
    }

    /// A map is never stored without the fence having been installed
    /// first: the install precedes the harvest read in program order,
    /// and this test pins the observable consequence — the very first
    /// write after a calibrated harvest advances the durable epoch
    /// (see `row4_...` for the serve-side effect).
    #[test]
    fn harvested_map_and_installed_fence_are_the_same_act() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);
        let HarvestOutcome::Calibrated { write_epoch, .. } =
            harvest(&mut world, &mut drive, Some("ACM001L8"))
        else {
            panic!("expected Calibrated");
        };
        assert!(
            world.index.get_wrap_map(&TAPE).expect("get").is_some(),
            "map stored"
        );
        assert!(
            !drive.media_write_fenced_this_load(),
            "no write yet — the fence is armed, not tripped"
        );
        drive.write_block(&[0u8; 64]).expect("write");
        assert!(drive.media_write_fenced_this_load());
        assert_eq!(world.store.row(TAPE).write_epoch, write_epoch + 1);
    }

    // -----------------------------------------------------------------
    //  Prompt tests: appending, coverage gap, EOD span, raw storage
    // -----------------------------------------------------------------

    /// Appending extends the map rather than invalidating the earlier
    /// part: the next load's harvest returns a longer descriptor list
    /// whose earlier wraps are unchanged, and early blocks locate
    /// identically under the new map.
    #[test]
    fn appending_extends_the_map_rather_than_invalidating_it() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive1 = drive(&controls);
        let HarvestOutcome::Calibrated { .. } = harvest(&mut world, &mut drive1, Some("ACM001L8"))
        else {
            panic!("expected Calibrated");
        };
        let WrapMapServeOutcome::Servable { map: map1, .. } =
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve")
        else {
            panic!("servable");
        };

        // The append happens: the write fences, the load ends, and the
        // next load's snapshot has grown by a wrap.
        drive1.write_block(&[0u8; 64]).expect("append write");
        controls.set_reowp(ReowpScript::Respond(reowp_response(&[
            (0, 207_516),
            (1, 415_522),
            (2, 623_000),
            (3, 700_000),
        ])));
        let mut drive2 = drive(&controls);
        let HarvestOutcome::Calibrated { .. } = harvest(&mut world, &mut drive2, Some("ACM001L8"))
        else {
            panic!("expected recalibration at the next load");
        };
        let WrapMapServeOutcome::Servable { map: map2, .. } =
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve")
        else {
            panic!("servable");
        };

        assert_eq!(map2.wrap_count(), 4, "the map grew");
        assert!(map2.mapped_extent_lba() > map1.mapped_extent_lba());
        // The earlier part is extended, not invalidated: blocks inside
        // the old completed wraps locate identically.
        for block in [0u64, 100_000, 207_516, 207_517, 400_000] {
            let before = map1.locate(block).expect("in old map");
            let after = map2.locate(block).expect("in new map");
            assert_eq!(before.wrap_index, after.wrap_index, "block {block}");
            assert_eq!(before.direction, after.direction, "block {block}");
            assert_eq!(before.physical_lpos, after.physical_lpos, "block {block}");
        }
    }

    /// A coverage gap is detected against `mapped_extent_lba` without
    /// touching the drive — the drive handle is dropped before the
    /// check, and the serve path takes no drive at all. The map stays
    /// valid: coverage is a property of the request, not the map.
    #[test]
    fn coverage_gap_detected_without_touching_the_drive() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive, Some("ACM001L8")),
            HarvestOutcome::Calibrated { .. }
        ));
        drop(drive); // no device exists from here on

        let WrapMapServeOutcome::Servable { map, .. } =
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve")
        else {
            panic!("servable");
        };
        let gap = map
            .locate(500_000)
            .expect_err("at the exclusive extent is outside coverage");
        assert_eq!(gap.mapped_extent_lba, 500_000);
        assert_eq!(gap.block_lba, 500_000);
        map.locate(499_999).expect("inside coverage still fine");
        // The map remains valid and served; only the request was out
        // of range.
        assert!(matches!(
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve"),
            WrapMapServeOutcome::Servable { .. }
        ));
    }

    /// Invalid map versus coverage gap, in naming and storage: an
    /// epoch-mismatched row is `InvalidEpochMismatch` and never
    /// served; a coverage gap is `CoverageError` from a served map.
    #[test]
    fn invalid_map_and_coverage_gap_are_distinct() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);
        let HarvestOutcome::Calibrated { write_epoch, .. } =
            harvest(&mut world, &mut drive, Some("ACM001L8"))
        else {
            panic!("expected Calibrated");
        };

        // Defence in depth: a map row stamped with another epoch while
        // the control row still says Calibrated must be refused as
        // invalid — the serve path re-checks through the single
        // wrap_map_is_servable predicate rather than trusting the
        // row's presence.
        let mut cached = world
            .index
            .get_wrap_map(&TAPE)
            .expect("get")
            .expect("present");
        cached.write_epoch = write_epoch + 7;
        world.index.upsert_wrap_map(&cached).expect("re-stamp");
        match servable_wrap_map(&world.index, &world.store, TAPE).expect("serve") {
            WrapMapServeOutcome::NotServable { refusal, .. } => {
                assert_eq!(refusal, WrapMapServeRefusal::InvalidEpochMismatch);
            }
            other => panic!("expected InvalidEpochMismatch, got {other:?}"),
        }
    }

    /// No completed wrap: the EOD wrap's own observed span is the
    /// denominator, and `mapped_extent_lba > wrap_start[eod_wrap]` is
    /// enforced (its violation is `row3b_...`).
    #[test]
    fn no_completed_wrap_uses_the_eod_observed_span() {
        let mut world = world();
        let controls = ScriptControls::new(ReowpScript::Respond(reowp_response(&[(0, 90_000)])));
        let mut drive = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive, Some("ACM001L8")),
            HarvestOutcome::Calibrated { .. }
        ));
        let WrapMapServeOutcome::Servable { map, .. } =
            servable_wrap_map(&world.index, &world.store, TAPE).expect("serve")
        else {
            panic!("servable");
        };
        assert_eq!(map.wrap_count(), 1);
        let denominator = map.eod_denominator();
        assert_eq!(denominator.basis, EodDenominatorBasis::EodObservedSpan);
        assert_eq!(denominator.span_lba, 90_000);
        assert_eq!(denominator.completed_span_sample_count, 0);
        assert!(map.mapped_extent_lba() > map.wrap_starts()[0]);
    }

    /// Raw descriptors are stored exactly as harvested, the extent is
    /// stored separately, and no synthetic EOD boundary is
    /// materialised anywhere.
    #[test]
    fn stored_map_is_raw_descriptors_plus_separate_extent() {
        let mut world = world();
        let controls = three_wrap_script();
        let mut drive = drive(&controls);
        assert!(matches!(
            harvest(&mut world, &mut drive, Some("ACM001L8")),
            HarvestOutcome::Calibrated { .. }
        ));
        let cached = world
            .index
            .get_wrap_map(&TAPE)
            .expect("get")
            .expect("present");
        assert_eq!(cached.descriptors.len(), 3, "exactly the harvested list");
        assert_eq!(
            cached
                .descriptors
                .iter()
                .map(|d| (d.partition, d.wrap_number, d.end_loi))
                .collect::<Vec<_>>(),
            vec![(0, 0, 207_516), (0, 1, 415_522), (0, 2, 500_000)],
            "descriptors unchanged from the wire"
        );
        assert_eq!(
            cached.mapped_extent_lba, 500_000,
            "the extent is its own column, copied from the EOD descriptor"
        );
    }

    /// The wire-to-descriptor conversion widens u16 fields losslessly.
    #[test]
    fn wire_conversion_widens_u16_fields() {
        let converted = descriptor_from_wire(WrapDescriptor {
            wrap_number: u16::MAX,
            partition: 3,
            end_loi: 0x0000_FFEE_DDCC_BBAA,
        });
        assert_eq!(converted.wrap_number, 65_535u32);
        assert_eq!(converted.partition, 3u32);
        assert_eq!(converted.end_loi, 0x0000_FFEE_DDCC_BBAA);
    }
}
