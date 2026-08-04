//! The durable calibration-control store — design
//! `design-read-ordering.md` §§4.3, 6.5 and decision D4b/D13.
//!
//! This store is the authority on whether a cached wrap map may be
//! served. It holds, per volume: the durable `write_epoch`, the
//! calibration state, and the `write_path_trust` marker — and one
//! **global monotonic `calibration_generation` allocator** shared by
//! every volume. It is a different erasure class from everything else
//! in this crate:
//!
//! - the SQLite index is a rebuildable projection — reset deletes it
//!   and rebuild clears its tables;
//! - the audit log and 3c journals are authoritative inputs, but
//!   catalog reset archives them away to `reset-archives/`;
//! - **this store survives both.** It lives in its own directory
//!   (`StatePaths::calibration_dir`, `state_dir/calibration/`), which
//!   [`crate::state::StateHandle::reset_catalog_with_config`]
//!   deliberately does not clear and rebuild does not touch.
//!
//! That placement is what makes `calibration_generation` genuinely
//! monotonic: a generation allocated before a catalog reset can never
//! be re-issued after it, so a caller-cached negative keyed to an old
//! generation cannot be resurrected by numerical coincidence (§4.3).
//! A counter that reset with the store would not be monotonic, which
//! is why this file is not in the SQLite index and not in a directory
//! reset clears. Allocator exhaustion fails closed rather than
//! wrapping.
//!
//! Durability protocol: an append-only, newline-framed JSON journal,
//! one record per state transition, fsynced before the transition is
//! acknowledged — the same discipline as the checkpoint journal. The
//! in-memory fold is updated only after the record is durable, so
//! `Ok` from any transition means "on disk". The media-write fence
//! relies on exactly this: [`VolumeMediaWriteFence::fence_media_write`]
//! returns `Ok` only when the epoch advance is durable, and the media
//! gate refuses to dispatch the CDB otherwise.
//!
//! The wrap maps themselves are **not** here. They are an evicted
//! projection in the SQLite index (`wrap_maps` table): rebuild and
//! reset both discard them, and a fresh load harvest lazily rebuilds
//! them — losing one costs one SCSI command at next load. Validity and
//! coverage stay distinct: an *invalid* map (epoch mismatch, or a
//! control row that is not `Calibrated`) is never served, while a
//! *coverage gap* (a target beyond `mapped_extent_lba`) is a property
//! of a valid map and is decided at planning time, not here.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use remanence_library::{MediaWriteFence, MediaWriteFenceError};
use serde::{Deserialize, Serialize};

use crate::error::StateError;

/// Filename of the calibration-control journal inside
/// `StatePaths::calibration_dir`.
pub const CALIBRATION_CONTROL_FILENAME: &str = "control.remcalibration";

/// Per-volume write-path trust — design D4c. Explicit state, not an
/// assumption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WritePathTrust {
    /// Every media-modifying path is fenced; harvests may calibrate.
    Trusted,
    /// An out-of-band write path may exist. Planning is disabled and a
    /// harvest, however successful, does not calibrate.
    OutOfBandWritePossible,
}

/// The calibration state machine's states — design §6.5.
///
/// `UnsupportedFormat` and `Uncalibrated` are deliberately distinct:
/// they map to different `PlanBatchRead` statuses and different caller
/// remedies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeCalibrationState {
    /// No servable map. The default state, and the result of harvest
    /// failure, fence trips, eviction, trust loss and recovery.
    Uncalibrated,
    /// A map bound to the row's `write_epoch` was stored at the last
    /// load harvest. Serving still requires the epoch equality and
    /// `Trusted` at serve time.
    Calibrated,
    /// The drive rejected REOWP for the loaded format, or format
    /// resolution recognised an unsupported format before issuing it.
    /// Holds for this load; the next load harvest re-decides.
    UnsupportedFormat,
}

/// One volume's durable calibration-control row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeCalibrationRow {
    /// The volume this row governs.
    pub tape_uuid: [u8; 16],
    /// Durable per-volume write epoch. Advanced by the pre-dispatch
    /// media-write fence and by conservative recovery.
    pub write_epoch: u64,
    /// Current calibration state.
    pub state: VolumeCalibrationState,
    /// Out-of-band-write trust marker.
    pub write_path_trust: WritePathTrust,
    /// Generation stamped at this row's most recent transition. Zero
    /// only for a volume that has never had a transition.
    pub calibration_generation: u64,
}

impl VolumeCalibrationRow {
    fn default_for(tape_uuid: [u8; 16]) -> Self {
        Self {
            tape_uuid,
            write_epoch: 0,
            state: VolumeCalibrationState::Uncalibrated,
            write_path_trust: WritePathTrust::Trusted,
            calibration_generation: 0,
        }
    }
}

/// Why a durable harvest-success transition did **not** calibrate the
/// volume. Succeeding at the command is not entitlement to trust it
/// (§6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarvestRefusal {
    /// The epoch observed when the harvest began no longer matches the
    /// durable epoch — a write fenced in between. The map belongs to a
    /// dead epoch and is invalid.
    EpochMismatch {
        /// Epoch the harvest was performed under.
        harvested_epoch: u64,
        /// Durable epoch at transition time.
        durable_epoch: u64,
    },
    /// `write_path_trust` is not `Trusted`.
    Untrusted,
}

/// Outcome of [`CalibrationControlStore::record_harvest_success`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HarvestTransition {
    /// The volume is calibrated. Store the map under exactly this
    /// epoch and generation.
    Calibrated {
        /// Epoch the map is bound to.
        write_epoch: u64,
        /// Fresh generation stamped on the transition.
        calibration_generation: u64,
    },
    /// The volume stays uncalibrated; no map may be stored.
    RefusedUncalibrated {
        /// Fresh generation stamped on the refusal.
        calibration_generation: u64,
        /// Why the successful command did not calibrate.
        refusal: HarvestRefusal,
    },
}

/// One journal record: the post-transition row plus the transition
/// kind, replayed last-record-wins per volume.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CalibrationControlRecord {
    kind: String,
    tape_uuid_hex: String,
    write_epoch: u64,
    state: VolumeCalibrationState,
    write_path_trust: WritePathTrust,
    calibration_generation: u64,
}

#[derive(Debug)]
struct StoreInner {
    path: PathBuf,
    /// Highest generation ever issued — the global monotonic
    /// allocator. Never decreases; replay folds it as the max over
    /// all records so even out-of-order per-volume history cannot
    /// lower it.
    last_generation: u64,
    rows: BTreeMap<[u8; 16], VolumeCalibrationRow>,
}

/// Cloneable handle to the durable calibration-control store. Clones
/// share state; a per-volume [`VolumeMediaWriteFence`] minted from one
/// handle is observed through any other.
#[derive(Clone, Debug)]
pub struct CalibrationControlStore {
    inner: Arc<Mutex<StoreInner>>,
}

impl CalibrationControlStore {
    /// Open (or create) the store beneath `dir`, replaying the journal
    /// into the in-memory fold. A torn final line — a crash mid-append
    /// — is truncated away; every acknowledged transition was fsynced
    /// before acknowledgement, so a torn line was never relied upon.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, StateError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)
            .map_err(|err| StateError::io_at("create calibration-control directory", dir, err))?;
        let path = dir.join(CALIBRATION_CONTROL_FILENAME);
        let mut inner = StoreInner {
            path,
            last_generation: 0,
            rows: BTreeMap::new(),
        };
        replay_into(&mut inner)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    fn lock(&self) -> MutexGuard<'_, StoreInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The volume's current durable row. A volume with no history gets
    /// the default row: epoch 0, uncalibrated, trusted, generation 0.
    pub fn row(&self, tape_uuid: [u8; 16]) -> VolumeCalibrationRow {
        self.lock()
            .rows
            .get(&tape_uuid)
            .copied()
            .unwrap_or_else(|| VolumeCalibrationRow::default_for(tape_uuid))
    }

    /// Every volume with recorded calibration history.
    pub fn rows(&self) -> Vec<VolumeCalibrationRow> {
        self.lock().rows.values().copied().collect()
    }

    /// Highest generation the allocator has ever issued.
    pub fn last_generation(&self) -> u64 {
        self.lock().last_generation
    }

    /// Whether a map harvested at `map_write_epoch` may be served for
    /// this volume — **the** serve predicate. Epoch equality routes
    /// through [`remanence_library::wrap_map_is_servable`], the single
    /// funnel P3 established; this adds the calibration-state and
    /// trust requirements on top. An epoch mismatch is an *invalid*
    /// map, never served; coverage gaps are a separate, per-target
    /// question answered at planning time against `mapped_extent_lba`.
    pub fn is_map_servable(&self, tape_uuid: [u8; 16], map_write_epoch: u64) -> bool {
        let row = self.row(tape_uuid);
        row.state == VolumeCalibrationState::Calibrated
            && row.write_path_trust == WritePathTrust::Trusted
            && remanence_library::wrap_map_is_servable(map_write_epoch, row.write_epoch)
    }

    /// Mint the [`MediaWriteFence`] binding this volume's durable row
    /// to a drive. P4's load harvest installs it via
    /// `DriveHandle::install_media_write_fence` in the same act that
    /// stores the volume's map — see the harvest layer.
    pub fn volume_fence(&self, tape_uuid: [u8; 16]) -> VolumeMediaWriteFence {
        VolumeMediaWriteFence {
            store: self.clone(),
            tape_uuid,
        }
    }

    /// The pre-dispatch media-write fence transaction (§6.5): advance
    /// the volume's durable `write_epoch`, mark it uncalibrated, and
    /// allocate a fresh generation — one durable record. `Ok` means
    /// the record is fsynced; on `Err` nothing changed and the media
    /// gate must not dispatch the CDB.
    fn fence_media_write_for(&self, tape_uuid: [u8; 16]) -> Result<(), StateError> {
        self.transition(tape_uuid, "media_write_fenced", |row, _| {
            row.write_epoch = row.write_epoch.checked_add(1).ok_or_else(|| {
                StateError::CalibrationControlCorrupt(
                    "write_epoch allocator exhausted; failing closed".to_string(),
                )
            })?;
            row.state = VolumeCalibrationState::Uncalibrated;
            Ok(())
        })
        .map(|_| ())
    }

    /// Durable transition for a REOWP command that **succeeded**. The
    /// epoch observed when the harvest began and the trust marker are
    /// judged inside the store's lock: only a matching epoch on a
    /// trusted volume calibrates. The §6.5 rows this implements:
    /// "Harvest succeeds, the stored epoch matches and
    /// `write_path_trust == TRUSTED`" and "REOWP succeeds but
    /// trust/epoch disagrees → still uncalibrated".
    pub fn record_harvest_success(
        &self,
        tape_uuid: [u8; 16],
        harvested_epoch: u64,
    ) -> Result<HarvestTransition, StateError> {
        let mut refusal = None;
        let generation = self.transition(tape_uuid, "harvest_succeeded", |row, _| {
            if row.write_path_trust != WritePathTrust::Trusted {
                row.state = VolumeCalibrationState::Uncalibrated;
                refusal = Some(HarvestRefusal::Untrusted);
            } else if row.write_epoch != harvested_epoch {
                row.state = VolumeCalibrationState::Uncalibrated;
                refusal = Some(HarvestRefusal::EpochMismatch {
                    harvested_epoch,
                    durable_epoch: row.write_epoch,
                });
            } else {
                row.state = VolumeCalibrationState::Calibrated;
            }
            Ok(())
        })?;
        Ok(match refusal {
            None => HarvestTransition::Calibrated {
                write_epoch: harvested_epoch,
                calibration_generation: generation,
            },
            Some(refusal) => HarvestTransition::RefusedUncalibrated {
                calibration_generation: generation,
                refusal,
            },
        })
    }

    /// Durable transition for a format recognised as unsupported —
    /// either before issuing REOWP (cartridge-fact resolution) or by
    /// the drive rejecting the command at runtime. Returns the fresh
    /// generation carried by `UNAVAILABLE_UNSUPPORTED_FORMAT`.
    pub fn record_unsupported_format(&self, tape_uuid: [u8; 16]) -> Result<u64, StateError> {
        self.transition(tape_uuid, "unsupported_format", |row, _| {
            row.state = VolumeCalibrationState::UnsupportedFormat;
            Ok(())
        })
    }

    /// Durable transition for a harvest that failed in transport,
    /// parse or descriptor validation: the volume is uncalibrated —
    /// the old map is not served — and no map is stored.
    pub fn record_harvest_failure(&self, tape_uuid: [u8; 16]) -> Result<u64, StateError> {
        self.transition(tape_uuid, "harvest_failed", |row, _| {
            row.state = VolumeCalibrationState::Uncalibrated;
            Ok(())
        })
    }

    /// Durable transition for a single volume's wrap-map eviction. The
    /// control row remains; the state is uncalibrated until a later
    /// load harvest succeeds.
    pub fn record_map_evicted(&self, tape_uuid: [u8; 16]) -> Result<u64, StateError> {
        self.transition(tape_uuid, "map_evicted", |row, _| {
            row.state = VolumeCalibrationState::Uncalibrated;
            Ok(())
        })
    }

    /// Durable transitions for a whole-store eviction — catalog
    /// projection rebuild or catalog reset, both of which discard the
    /// `wrap_maps` projection. Every known volume becomes
    /// uncalibrated with its own fresh generation; the rows and the
    /// allocator remain (§6.5's rebuild and reset rows). Returns how
    /// many volumes transitioned.
    pub fn record_all_maps_evicted(&self, kind_detail: &str) -> Result<u64, StateError> {
        let kind = format!("all_maps_evicted:{kind_detail}");
        let mut guard = self.lock();
        let tapes: Vec<[u8; 16]> = guard.rows.keys().copied().collect();
        let mut count = 0u64;
        for tape_uuid in tapes {
            transition_locked(&mut guard, tape_uuid, &kind, |row, _| {
                row.state = VolumeCalibrationState::Uncalibrated;
                Ok(())
            })?;
            count += 1;
        }
        Ok(count)
    }

    /// Durable transition for conservative startup / orphan-session
    /// recovery: the volume **may** have had a media-modifying CDB
    /// dispatched outside a proven fence, so the epoch is advanced and
    /// the volume left uncalibrated until a fresh load harvest.
    /// Uncertainty resolves as false invalidation, never as serving
    /// the old map (§6.5).
    pub fn record_possible_write_recovery(&self, tape_uuid: [u8; 16]) -> Result<u64, StateError> {
        self.transition(tape_uuid, "possible_write_recovery", |row, _| {
            row.write_epoch = row.write_epoch.checked_add(1).ok_or_else(|| {
                StateError::CalibrationControlCorrupt(
                    "write_epoch allocator exhausted; failing closed".to_string(),
                )
            })?;
            row.state = VolumeCalibrationState::Uncalibrated;
            Ok(())
        })
    }

    /// Durable transition setting the out-of-band-write trust marker
    /// (D4c). Setting `OutOfBandWritePossible` uncalibrates the
    /// volume. Clearing it back to `Trusted` records only that the
    /// modifying paths are fenced again — it does **not** restore
    /// calibration; a fresh load harvest is still required.
    pub fn set_write_path_trust(
        &self,
        tape_uuid: [u8; 16],
        trust: WritePathTrust,
    ) -> Result<u64, StateError> {
        self.transition(tape_uuid, "write_path_trust", |row, _| {
            row.write_path_trust = trust;
            // Both directions leave the volume uncalibrated: setting
            // the marker invalidates, and clearing it must not
            // resurrect a map harvested while the path was untrusted.
            row.state = VolumeCalibrationState::Uncalibrated;
            Ok(())
        })
    }

    /// Test/ops control: place the allocator near exhaustion.
    #[doc(hidden)]
    pub fn force_last_generation(&self, value: u64) {
        self.lock().last_generation = value;
    }

    /// One durable transition: allocate the next generation, apply
    /// `apply` to the row, append + fsync the record, then commit the
    /// fold in memory. On any error the in-memory state is unchanged.
    fn transition(
        &self,
        tape_uuid: [u8; 16],
        kind: &str,
        apply: impl FnOnce(&mut VolumeCalibrationRow, u64) -> Result<(), StateError>,
    ) -> Result<u64, StateError> {
        let mut guard = self.lock();
        transition_locked(&mut guard, tape_uuid, kind, apply)
    }
}

fn transition_locked(
    guard: &mut StoreInner,
    tape_uuid: [u8; 16],
    kind: &str,
    apply: impl FnOnce(&mut VolumeCalibrationRow, u64) -> Result<(), StateError>,
) -> Result<u64, StateError> {
    let generation = guard.last_generation.checked_add(1).ok_or_else(|| {
        StateError::CalibrationControlCorrupt(
            "calibration_generation allocator exhausted; failing closed".to_string(),
        )
    })?;
    let mut row = guard
        .rows
        .get(&tape_uuid)
        .copied()
        .unwrap_or_else(|| VolumeCalibrationRow::default_for(tape_uuid));
    apply(&mut row, generation)?;
    row.calibration_generation = generation;

    let record = CalibrationControlRecord {
        kind: kind.to_string(),
        tape_uuid_hex: hex_uuid(tape_uuid),
        write_epoch: row.write_epoch,
        state: row.state,
        write_path_trust: row.write_path_trust,
        calibration_generation: generation,
    };
    append_record(&guard.path, &record)?;

    // Only after the record is durable does the fold move.
    guard.last_generation = generation;
    guard.rows.insert(tape_uuid, row);
    Ok(generation)
}

fn append_record(path: &Path, record: &CalibrationControlRecord) -> Result<(), StateError> {
    let mut encoded = serde_json::to_vec(record).map_err(|err| {
        StateError::CalibrationControlCorrupt(format!("encode calibration record: {err}"))
    })?;
    encoded.push(b'\n');
    let created = !path.exists();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| StateError::io_at("open calibration-control journal", path, err))?;
    file.write_all(&encoded)
        .map_err(|err| StateError::io_at("append calibration record", path, err))?;
    file.flush()
        .map_err(|err| StateError::io_at("flush calibration record", path, err))?;
    file.sync_all()
        .map_err(|err| StateError::io_at("fsync calibration record", path, err))?;
    if created {
        let parent = path.parent().ok_or_else(|| {
            StateError::CalibrationControlCorrupt(
                "calibration-control journal path has no parent".to_string(),
            )
        })?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|err| StateError::io_at("fsync calibration-control directory", parent, err))?;
    }
    Ok(())
}

fn replay_into(inner: &mut StoreInner) -> Result<(), StateError> {
    if !inner.path.exists() {
        return Ok(());
    }
    let mut bytes = Vec::new();
    File::open(&inner.path)
        .and_then(|mut file| file.read_to_end(&mut bytes))
        .map_err(|err| StateError::io_at("read calibration-control journal", &inner.path, err))?;
    let complete_len = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    if complete_len != bytes.len() {
        // Torn tail from a crash mid-append: it was never acknowledged
        // (acknowledgement follows fsync), so truncate it away.
        let file = OpenOptions::new()
            .write(true)
            .open(&inner.path)
            .map_err(|err| {
                StateError::io_at(
                    "open calibration-control journal for repair",
                    &inner.path,
                    err,
                )
            })?;
        file.set_len(complete_len as u64)
            .map_err(|err| StateError::io_at("truncate torn calibration tail", &inner.path, err))?;
        file.sync_all()
            .map_err(|err| StateError::io_at("fsync calibration tail repair", &inner.path, err))?;
    }
    for (line_index, line) in bytes[..complete_len]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let record: CalibrationControlRecord = serde_json::from_slice(line).map_err(|err| {
            StateError::CalibrationControlCorrupt(format!(
                "decode calibration record {} in {}: {err}",
                line_index + 1,
                inner.path.display()
            ))
        })?;
        let tape_uuid = uuid_from_hex(&record.tape_uuid_hex).ok_or_else(|| {
            StateError::CalibrationControlCorrupt(format!(
                "calibration record {} has a malformed tape UUID {:?}",
                line_index + 1,
                record.tape_uuid_hex
            ))
        })?;
        // The allocator folds as the maximum ever seen, so it can
        // never move backward even against a damaged history.
        inner.last_generation = inner.last_generation.max(record.calibration_generation);
        inner.rows.insert(
            tape_uuid,
            VolumeCalibrationRow {
                tape_uuid,
                write_epoch: record.write_epoch,
                state: record.state,
                write_path_trust: record.write_path_trust,
                calibration_generation: record.calibration_generation,
            },
        );
    }
    Ok(())
}

/// The per-volume [`MediaWriteFence`] over the durable store. The
/// load harvest installs one of these into the `DriveHandle` for the
/// loaded volume in the same act that produces the volume's map.
#[derive(Clone, Debug)]
pub struct VolumeMediaWriteFence {
    store: CalibrationControlStore,
    tape_uuid: [u8; 16],
}

impl MediaWriteFence for VolumeMediaWriteFence {
    fn fence_media_write(&mut self) -> Result<(), MediaWriteFenceError> {
        self.store
            .fence_media_write_for(self.tape_uuid)
            .map_err(|err| MediaWriteFenceError::new(err.to_string()))
    }
}

fn hex_uuid(tape_uuid: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in tape_uuid {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn uuid_from_hex(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 16];
    for (i, chunk) in bytes.chunks_exact(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_library::wrap_map_is_servable;

    const TAPE_A: [u8; 16] = [0xAA; 16];
    const TAPE_B: [u8; 16] = [0xBB; 16];

    fn temp_store() -> (tempfile::TempDir, CalibrationControlStore) {
        let dir = tempfile::Builder::new()
            .prefix("rem-calibration")
            .tempdir()
            .expect("tempdir");
        let store = CalibrationControlStore::open(dir.path().join("calibration"))
            .expect("open calibration store");
        (dir, store)
    }

    #[test]
    fn default_row_is_uncalibrated_trusted_epoch_zero() {
        let (_dir, store) = temp_store();
        let row = store.row(TAPE_A);
        assert_eq!(row.write_epoch, 0);
        assert_eq!(row.state, VolumeCalibrationState::Uncalibrated);
        assert_eq!(row.write_path_trust, WritePathTrust::Trusted);
        assert_eq!(row.calibration_generation, 0);
        assert!(
            !store.is_map_servable(TAPE_A, 0),
            "an uncalibrated volume never serves, even with matching epochs"
        );
    }

    #[test]
    fn harvest_success_with_matching_epoch_calibrates() {
        let (_dir, store) = temp_store();
        let outcome = store
            .record_harvest_success(TAPE_A, 0)
            .expect("durable transition");
        let HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = outcome
        else {
            panic!("expected Calibrated, got {outcome:?}");
        };
        assert_eq!(write_epoch, 0);
        assert_eq!(calibration_generation, 1, "generations start at 1");
        assert!(store.is_map_servable(TAPE_A, write_epoch));
        assert!(
            !store.is_map_servable(TAPE_A, write_epoch + 1),
            "a map from another epoch is invalid and never served"
        );
    }

    #[test]
    fn harvest_success_with_stale_epoch_stays_uncalibrated() {
        // A write fenced between the harvest read and the store
        // transition: the command succeeded, the trust in it did not
        // survive.
        let (_dir, store) = temp_store();
        let mut fence = store.volume_fence(TAPE_A);
        fence.fence_media_write().expect("fence transaction");
        assert_eq!(store.row(TAPE_A).write_epoch, 1);

        let outcome = store
            .record_harvest_success(TAPE_A, 0)
            .expect("durable transition");
        match outcome {
            HarvestTransition::RefusedUncalibrated {
                refusal:
                    HarvestRefusal::EpochMismatch {
                        harvested_epoch: 0,
                        durable_epoch: 1,
                    },
                calibration_generation,
            } => {
                assert!(calibration_generation > 0);
            }
            other => panic!("expected epoch-mismatch refusal, got {other:?}"),
        }
        assert_eq!(
            store.row(TAPE_A).state,
            VolumeCalibrationState::Uncalibrated
        );
        assert!(!store.is_map_servable(TAPE_A, 0));
        assert!(!store.is_map_servable(TAPE_A, 1));
    }

    #[test]
    fn harvest_success_on_untrusted_volume_stays_uncalibrated() {
        let (_dir, store) = temp_store();
        store
            .set_write_path_trust(TAPE_A, WritePathTrust::OutOfBandWritePossible)
            .expect("set trust");
        let outcome = store
            .record_harvest_success(TAPE_A, 0)
            .expect("durable transition");
        assert!(
            matches!(
                outcome,
                HarvestTransition::RefusedUncalibrated {
                    refusal: HarvestRefusal::Untrusted,
                    ..
                }
            ),
            "got {outcome:?}"
        );
        assert!(!store.is_map_servable(TAPE_A, 0));
    }

    #[test]
    fn clearing_trust_does_not_restore_calibration() {
        let (_dir, store) = temp_store();
        store.record_harvest_success(TAPE_A, 0).expect("calibrate");
        assert!(store.is_map_servable(TAPE_A, 0));
        store
            .set_write_path_trust(TAPE_A, WritePathTrust::OutOfBandWritePossible)
            .expect("mark out-of-band");
        assert!(!store.is_map_servable(TAPE_A, 0));
        store
            .set_write_path_trust(TAPE_A, WritePathTrust::Trusted)
            .expect("clear marker");
        assert!(
            !store.is_map_servable(TAPE_A, 0),
            "clearing the marker records the paths are fenced again; a fresh load harvest is still required"
        );
        assert_eq!(
            store.row(TAPE_A).state,
            VolumeCalibrationState::Uncalibrated
        );
    }

    #[test]
    fn fence_uncalibrates_and_survives_reopen() {
        let (dir, store) = temp_store();
        store.record_harvest_success(TAPE_A, 0).expect("calibrate");
        let mut fence = store.volume_fence(TAPE_A);
        fence.fence_media_write().expect("fence transaction");
        let row = store.row(TAPE_A);
        assert_eq!(row.write_epoch, 1);
        assert_eq!(row.state, VolumeCalibrationState::Uncalibrated);
        assert!(!wrap_map_is_servable(0, row.write_epoch));

        let generation_before = store.last_generation();
        drop(store);
        let reopened = CalibrationControlStore::open(dir.path().join("calibration"))
            .expect("reopen calibration store");
        let row = reopened.row(TAPE_A);
        assert_eq!(row.write_epoch, 1, "epoch advance is durable");
        assert_eq!(row.state, VolumeCalibrationState::Uncalibrated);
        assert_eq!(
            reopened.last_generation(),
            generation_before,
            "the allocator's high-water mark is durable"
        );
    }

    #[test]
    fn generation_allocator_is_global_and_monotonic_across_volumes() {
        let (_dir, store) = temp_store();
        let g1 = store
            .record_harvest_failure(TAPE_A)
            .expect("durable transition");
        let g2 = store
            .record_unsupported_format(TAPE_B)
            .expect("durable transition");
        let g3 = store
            .record_map_evicted(TAPE_A)
            .expect("durable transition");
        assert!(g1 < g2 && g2 < g3, "one allocator, strictly increasing");
    }

    #[test]
    fn generation_allocator_never_reissues_across_reopen() {
        let (dir, store) = temp_store();
        let g1 = store.record_harvest_failure(TAPE_A).expect("transition");
        drop(store);
        let reopened =
            CalibrationControlStore::open(dir.path().join("calibration")).expect("reopen");
        let g2 = reopened.record_harvest_failure(TAPE_A).expect("transition");
        assert!(
            g2 > g1,
            "a reopened allocator continues after the highest issued value"
        );
    }

    #[test]
    fn allocator_exhaustion_fails_closed() {
        let (_dir, store) = temp_store();
        store.force_last_generation(u64::MAX);
        let err = store
            .record_harvest_failure(TAPE_A)
            .expect_err("exhausted allocator fails closed");
        assert!(matches!(err, StateError::CalibrationControlCorrupt(_)));
        // Nothing changed.
        assert_eq!(store.row(TAPE_A).calibration_generation, 0);
    }

    #[test]
    fn epoch_exhaustion_fails_closed_through_the_fence() {
        let (_dir, store) = temp_store();
        // Drive the epoch to the ceiling via the recovery transition
        // path being given an exhausted row.
        let mut fence = store.volume_fence(TAPE_A);
        fence.fence_media_write().expect("first fence");
        // Force epoch to MAX by direct journal transitions.
        {
            let mut guard = store.lock();
            let mut row = guard.rows.get(&TAPE_A).copied().expect("row exists");
            row.write_epoch = u64::MAX;
            guard.rows.insert(TAPE_A, row);
        }
        let err = fence
            .fence_media_write()
            .expect_err("epoch exhaustion fails closed");
        assert!(err.reason().contains("write_epoch allocator exhausted"));
        assert_eq!(store.row(TAPE_A).write_epoch, u64::MAX, "no wrap-around");
    }

    #[test]
    fn all_maps_evicted_uncalibrates_every_known_volume_with_fresh_generations() {
        let (_dir, store) = temp_store();
        store
            .record_harvest_success(TAPE_A, 0)
            .expect("calibrate A");
        store
            .record_harvest_success(TAPE_B, 0)
            .expect("calibrate B");
        let before_a = store.row(TAPE_A).calibration_generation;
        let before_b = store.row(TAPE_B).calibration_generation;

        let count = store
            .record_all_maps_evicted("catalog_reset")
            .expect("bulk eviction");
        assert_eq!(count, 2);
        let after_a = store.row(TAPE_A);
        let after_b = store.row(TAPE_B);
        assert_eq!(after_a.state, VolumeCalibrationState::Uncalibrated);
        assert_eq!(after_b.state, VolumeCalibrationState::Uncalibrated);
        assert!(after_a.calibration_generation > before_a);
        assert!(after_b.calibration_generation > before_b);
        assert_ne!(
            after_a.calibration_generation, after_b.calibration_generation,
            "each transition allocates its own generation"
        );
        assert!(!store.is_map_servable(TAPE_A, 0));
    }

    #[test]
    fn possible_write_recovery_advances_epoch_and_uncalibrates() {
        let (_dir, store) = temp_store();
        store.record_harvest_success(TAPE_A, 0).expect("calibrate");
        assert!(store.is_map_servable(TAPE_A, 0));
        store
            .record_possible_write_recovery(TAPE_A)
            .expect("recovery transition");
        let row = store.row(TAPE_A);
        assert_eq!(row.write_epoch, 1, "epoch durably advanced");
        assert_eq!(row.state, VolumeCalibrationState::Uncalibrated);
        assert!(
            !store.is_map_servable(TAPE_A, 0),
            "the old map is invalid until a fresh load harvest"
        );
    }

    #[test]
    fn torn_tail_is_truncated_and_replay_survives() {
        let (dir, store) = temp_store();
        store.record_harvest_success(TAPE_A, 0).expect("calibrate");
        let path = dir
            .path()
            .join("calibration")
            .join(CALIBRATION_CONTROL_FILENAME);
        drop(store);
        // Simulate a crash mid-append: a partial record with no
        // trailing newline.
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open journal");
            file.write_all(b"{\"kind\":\"harvest_succeeded\",\"tape_uuid_hex\":\"")
                .expect("write torn tail");
            file.sync_all().expect("sync torn tail");
        }
        let reopened =
            CalibrationControlStore::open(dir.path().join("calibration")).expect("reopen");
        assert_eq!(
            reopened.row(TAPE_A).state,
            VolumeCalibrationState::Calibrated,
            "complete records survive; the torn tail is discarded"
        );
        // The file is repaired: reopening again decodes cleanly.
        let again = CalibrationControlStore::open(dir.path().join("calibration"))
            .expect("reopen after repair");
        assert_eq!(again.last_generation(), reopened.last_generation());
    }

    #[test]
    fn corrupt_complete_record_is_an_error_not_a_guess() {
        let (dir, store) = temp_store();
        store.record_harvest_failure(TAPE_A).expect("transition");
        let path = dir
            .path()
            .join("calibration")
            .join(CALIBRATION_CONTROL_FILENAME);
        drop(store);
        {
            let mut file = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open journal");
            file.write_all(b"{\"not\":\"a calibration record\"}\n")
                .expect("write corrupt record");
            file.sync_all().expect("sync");
        }
        let err = CalibrationControlStore::open(dir.path().join("calibration"))
            .expect_err("corrupt complete record fails closed");
        assert!(matches!(err, StateError::CalibrationControlCorrupt(_)));
    }
}
