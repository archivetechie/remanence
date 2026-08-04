//! DriveHandle-level tests for the media-dispatch gate and the
//! pre-dispatch media-write fence (design `design-read-ordering.md`
//! §6.5 / D4b), plus the source audit that keeps the gate the sole
//! write-direction dispatcher.
//!
//! The failure paths carry the weight here: fence-not-durable refusal
//! (and recovery), CDB failure after a durable fence, allocator
//! exhaustion, unit-attention re-arming, and the never-serve-a-stale-
//! map guarantee are all asserted alongside the once-per-load happy
//! path.

use std::path::Path;

use remanence_scsi::{read_write, ScsiError};

use super::super::DriveHandle;
use super::media_gate::{wrap_map_is_servable, InMemoryCalibrationControl};
use super::{BlockSize, TapeConfig, TapeIoError, WormMediaState};
use crate::transport::{
    FixtureTransport, RecordingLog, RecordingTransport, SgTransport, TransferOutcome,
};

// ---------------------------------------------------------------------
//  Fixtures — the INQUIRY bytes are the captured LTO-9 golden fixture
//  the rest of the handle tests use, not invented data.
// ---------------------------------------------------------------------

fn lto9_inquiry() -> Vec<u8> {
    include_bytes!("../../../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
}

fn vpd80_response(serial: &str) -> Vec<u8> {
    let bytes = serial.as_bytes();
    let mut v = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
    v.extend_from_slice(bytes);
    v
}

fn rp_long_response(flags: u8, partition: u32, lba: u64) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0] = flags;
    v[4..8].copy_from_slice(&partition.to_be_bytes());
    v[8..16].copy_from_slice(&lba.to_be_bytes());
    v
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

/// Fixture transport seeded for `open_standalone_with_transport`
/// (INQUIRY + VPD 0x80) plus `rp_count` canned READ POSITION
/// responses for the write paths' inline/seeding RPs.
fn seeded_fixture(rp_count: usize) -> FixtureTransport {
    let mut fixture =
        FixtureTransport::new().with_responses([lto9_inquiry(), vpd80_response("DRV_FENCE")]);
    for _ in 0..rp_count {
        fixture.push_response(rp_long_response(0, 0, 0));
    }
    fixture
}

/// Open a standalone DriveHandle over `transport`, install a fresh
/// in-memory calibration control as the fence, and hand back the
/// observation clone.
fn fenced_drive_over(transport: Box<dyn SgTransport>) -> (DriveHandle, InMemoryCalibrationControl) {
    let mut drive =
        DriveHandle::open_standalone_with_transport(Path::new("/dev/sg-fence-test"), transport)
            .expect("standalone drive opens over fixture transport");
    let control = InMemoryCalibrationControl::new();
    drive.install_media_write_fence(Box::new(control.clone()));
    (drive, control)
}

/// Standard harness: recording transport over a seeded fixture, so
/// tests can assert exactly which CDBs reached the transport.
fn fenced_drive(rp_count: usize) -> (DriveHandle, InMemoryCalibrationControl, RecordingLog) {
    let (recording, log) = RecordingTransport::new(seeded_fixture(rp_count));
    let (drive, control) = fenced_drive_over(Box::new(recording));
    (drive, control, log)
}

fn opcode_count(log: &RecordingLog, opcode: u8) -> usize {
    log.borrow()
        .iter()
        .filter(|cdb| cdb.first() == Some(&opcode))
        .count()
}

/// Transport wrapper that fails the first WRITE-direction data-out
/// CDB with the given CHECK CONDITION, then forwards cleanly.
struct FailFirstExecuteOut<T: SgTransport> {
    inner: T,
    sense: Option<Vec<u8>>,
}

impl<T: SgTransport> SgTransport for FailFirstExecuteOut<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        if let Some(sense) = self.sense.take() {
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        self.inner.execute_out(cdb, buf)
    }
}

// ---------------------------------------------------------------------
//  Once per load, never once per command — all seven media paths
// ---------------------------------------------------------------------

#[test]
fn write_block_fences_once_per_load_not_once_per_command() {
    let (mut drive, control, log) = fenced_drive(4);
    let payload = vec![0xAAu8; 64];
    drive.write_block(&payload).expect("first write");
    drive.write_block(&payload).expect("second write");

    assert_eq!(
        control.fence_transactions(),
        1,
        "one epoch advance per load"
    );
    assert_eq!(control.write_epoch(), 1);
    assert_eq!(opcode_count(&log, 0x0A), 2, "both WRITEs dispatched");
    assert!(drive.media_write_fenced_this_load());
}

#[test]
fn write_block_unpositioned_fences_once_per_load() {
    let (mut drive, control, log) = fenced_drive(0);
    let payload = vec![0xBBu8; 64];
    drive
        .write_block_unpositioned(&payload)
        .expect("first write");
    drive
        .write_block_unpositioned(&payload)
        .expect("second write");

    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x0A), 2);
}

#[test]
fn write_block_batch_fences_once_per_load() {
    let (mut drive, control, log) = fenced_drive(2);
    let buf = vec![0u8; 512];
    drive.write_block_batch(&buf, 512).expect("first batch");
    drive.write_block_batch(&buf, 512).expect("second batch");

    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x0A), 2);
}

#[test]
fn write_block_batch_pipelined_fences_once_per_load_never_per_command() {
    // The pipelined path is the one the design singles out: it skips
    // fire_tape_started on its clean pre-dispatch path, so a fence
    // hung on that audit hook would miss it entirely. Assert the
    // epoch advances exactly once across several pipelined commands.
    let (mut drive, control, log) = fenced_drive(2);
    let buf = vec![0u8; 512];
    let cdb = read_write::build_write_fixed_cdb(1);
    for i in 0..3 {
        drive
            .write_block_batch_pipelined(&buf, 512, &cdb)
            .unwrap_or_else(|e| panic!("pipelined write {i}: {e}"));
    }

    assert_eq!(
        control.fence_transactions(),
        1,
        "pipelined path advances the epoch once per load, not once per command"
    );
    assert_eq!(control.write_epoch(), 1);
    assert_eq!(opcode_count(&log, 0x0A), 3, "all three WRITEs dispatched");
}

#[test]
fn write_filemarks_fences_once_per_load() {
    let (mut drive, control, log) = fenced_drive(4);
    drive.write_filemarks(1).expect("first filemarks");
    drive.write_filemarks(1).expect("second filemarks");

    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x10), 2);
}

#[test]
fn write_filemarks_zero_count_form_still_fences() {
    // The design names the zero-count WRITE FILEMARKS form (the
    // drive-buffer flush) as part of the media-modifying set.
    let (mut drive, control, log) = fenced_drive(2);
    drive.write_filemarks(0).expect("zero-count filemarks");

    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x10), 1);
}

#[test]
fn write_filemarks_immediate_fences_once_per_load() {
    let (mut drive, control, log) = fenced_drive(0);
    drive.write_filemarks_immediate(1).expect("first immediate");
    drive
        .write_filemarks_immediate(1)
        .expect("second immediate");

    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x10), 2);
}

#[test]
fn write_filemarks_pipelined_fences_once_per_load() {
    let (mut drive, control, log) = fenced_drive(4);
    drive.write_filemarks_pipelined(1).expect("first pipelined");
    drive
        .write_filemarks_pipelined(1)
        .expect("second pipelined");

    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x10), 2);
}

#[test]
fn cross_path_writes_in_one_load_share_one_epoch_advance() {
    let (mut drive, control, _log) = fenced_drive(4);
    let payload = vec![0xCCu8; 64];
    let buf = vec![0u8; 512];
    drive.write_block(&payload).expect("write_block");
    drive.write_filemarks(1).expect("write_filemarks");
    drive
        .write_block_batch(&buf, 512)
        .expect("write_block_batch");

    assert_eq!(
        control.fence_transactions(),
        1,
        "the fence is per load, not per path"
    );
}

// ---------------------------------------------------------------------
//  write_config — through the gate, never through the fence
// ---------------------------------------------------------------------

#[test]
fn write_config_routes_through_the_gate_but_never_trips_the_fence() {
    // MODE SELECT is a write-direction CDB (it routes through the
    // gate — the eighth dispatch site) but it does not modify
    // recorded media, so it must not advance the epoch: the parity
    // read/recovery path sets block size through it, and fencing it
    // would uncalibrate volumes on read. This diverges deliberately
    // from a literal reading of the prompt's test list and follows
    // §6.5's media-modifying set and D4b; see the P3 report.
    let (mut drive, control, log) = fenced_drive(2);
    drive
        .write_config(TapeConfig {
            block_size: BlockSize::Variable,
            compression: false,
            max_block_size_bytes: 0,
            write_protected: false,
            worm: WormMediaState::NotWorm,
        })
        .expect("write_config");

    assert_eq!(opcode_count(&log, 0x15), 1, "MODE SELECT was dispatched");
    assert_eq!(control.fence_transactions(), 0, "no epoch advance");
    assert_eq!(control.write_epoch(), 0);
    assert!(
        !drive.media_write_fenced_this_load(),
        "config dispatch must not mark the load fenced"
    );

    // The first actual media write afterwards still fences.
    drive.write_block(&[0u8; 64]).expect("write after config");
    assert_eq!(control.fence_transactions(), 1);
}

// ---------------------------------------------------------------------
//  Load boundaries
// ---------------------------------------------------------------------

#[test]
fn load_unload_boundary_rearms_the_fence() {
    let (mut drive, control, _log) = fenced_drive(4);
    drive.write_block(&[0u8; 64]).expect("write in load 1");
    assert_eq!(control.fence_transactions(), 1);

    drive.unload().expect("unload");
    assert!(
        !drive.media_write_fenced_this_load(),
        "unload clears the load-local flag"
    );
    drive.load().expect("load");

    drive.write_block(&[0u8; 64]).expect("write in load 2");
    assert_eq!(
        control.fence_transactions(),
        2,
        "the next load's first write advances the epoch again"
    );
    assert_eq!(control.write_epoch(), 2);
}

#[test]
fn installing_a_fence_rearms_the_once_per_load_flag() {
    let (mut drive, control, _log) = fenced_drive(4);
    drive.write_block(&[0u8; 64]).expect("write");
    assert_eq!(control.fence_transactions(), 1);

    // Installation happens at load boundaries (P4's harvest path);
    // it must err toward re-fencing.
    drive.install_media_write_fence(Box::new(control.clone()));
    assert!(!drive.media_write_fenced_this_load());

    drive.write_block(&[0u8; 64]).expect("write after install");
    assert_eq!(control.fence_transactions(), 2);
}

// ---------------------------------------------------------------------
//  Failure paths
// ---------------------------------------------------------------------

#[test]
fn failed_durable_transaction_means_the_cdb_is_not_dispatched() {
    let (mut drive, control, log) = fenced_drive(2);
    control.fail_fence("wal sync failed");

    let err = drive
        .write_block(&[0u8; 64])
        .expect_err("write must be refused while the fence cannot be made durable");
    match &err {
        TapeIoError::WriteFenceNotDurable(inner) => {
            assert!(inner.reason().contains("wal sync failed"));
        }
        other => panic!("expected WriteFenceNotDurable, got {other:?}"),
    }
    assert!(!err.is_completion_unknown(), "nothing was dispatched");
    assert_eq!(opcode_count(&log, 0x0A), 0, "the WRITE CDB never went out");
    assert_eq!(control.write_epoch(), 0, "epoch untouched");
    assert!(!drive.media_write_fenced_this_load());

    // Recovery: once the store is durable again, the same load
    // fences exactly once and the write proceeds.
    control.clear_fence_failure();
    drive.write_block(&[0u8; 64]).expect("write after recovery");
    assert_eq!(control.fence_transactions(), 1);
    assert_eq!(opcode_count(&log, 0x0A), 1);
}

#[test]
fn failed_durable_transaction_refuses_the_pipelined_path_too() {
    let (mut drive, control, log) = fenced_drive(2);
    control.fail_fence("store offline");

    let buf = vec![0u8; 512];
    let cdb = read_write::build_write_fixed_cdb(1);
    let err = drive
        .write_block_batch_pipelined(&buf, 512, &cdb)
        .expect_err("pipelined write must be refused");
    assert!(matches!(err, TapeIoError::WriteFenceNotDurable(_)));
    assert_eq!(
        opcode_count(&log, 0x0A),
        0,
        "no WRITE reached the transport"
    );
    assert_eq!(control.fence_transactions(), 0);
}

#[test]
fn write_whose_cdb_fails_still_leaves_the_epoch_advanced() {
    // False invalidation is deliberate and correct: the fence runs
    // before dispatch, so a WRITE that then dies on the drive leaves
    // the epoch advanced and the map invalid.
    let transport = FailFirstExecuteOut {
        inner: seeded_fixture(2),
        // MEDIUM ERROR / write error — a hard, current CDB failure.
        sense: Some(fixed_sense(0x03, 0x0C, 0x00)),
    };
    let (recording, log) = RecordingTransport::new(transport);
    let (mut drive, control) = fenced_drive_over(Box::new(recording));

    drive
        .write_block(&[0u8; 64])
        .expect_err("the injected medium error surfaces");
    assert_eq!(
        control.fence_transactions(),
        1,
        "epoch advanced even though the CDB failed"
    );
    assert_eq!(control.write_epoch(), 1);
    assert_eq!(opcode_count(&log, 0x0A), 1, "the WRITE was dispatched");
    assert!(
        drive.media_write_fenced_this_load(),
        "the load stays fenced — the epoch is already advanced"
    );

    // A subsequent successful write in the same load does not
    // advance the epoch again.
    drive.write_block(&[0u8; 64]).expect("clean retry");
    assert_eq!(control.fence_transactions(), 1);
}

#[test]
fn reset_unit_attention_rearms_the_fence() {
    // A reset UA can sit on a load boundary (power cycle, medium
    // change under the handle). The next media write must re-advance
    // the epoch rather than trusting the pre-reset fence.
    let transport = FailFirstExecuteOut {
        inner: seeded_fixture(4),
        sense: Some(fixed_sense(0x06, 0x29, 0x00)),
    };
    let (recording, _log) = RecordingTransport::new(transport);
    let (mut drive, control) = fenced_drive_over(Box::new(recording));

    drive
        .write_block(&[0u8; 64])
        .expect_err("reset UA surfaces as an error");
    assert_eq!(control.fence_transactions(), 1, "fence ran pre-dispatch");
    assert!(
        !drive.media_write_fenced_this_load(),
        "reset UA cleared the load-local flag"
    );

    // Recover position (reset invalidated it), then write again:
    // the fence must run a second time.
    drive.position().expect("re-establish position after reset");
    drive.write_block(&[0u8; 64]).expect("write after reset");
    assert_eq!(
        control.fence_transactions(),
        2,
        "post-reset write re-advanced the epoch"
    );
}

#[test]
fn allocator_exhaustion_fails_closed_at_the_drive_handle_surface() {
    let (mut drive, control, log) = fenced_drive(2);
    control.set_write_epoch(u64::MAX);

    let err = drive
        .write_block(&[0u8; 64])
        .expect_err("exhausted allocator must refuse the write");
    assert!(matches!(err, TapeIoError::WriteFenceNotDurable(_)));
    assert_eq!(opcode_count(&log, 0x0A), 0);
    assert_eq!(control.write_epoch(), u64::MAX, "no wrap-around");
}

// ---------------------------------------------------------------------
//  The map-validity guarantee the fence exists for
// ---------------------------------------------------------------------

#[test]
fn map_with_mismatched_write_epoch_is_never_served_after_a_write() {
    let (mut drive, control, _log) = fenced_drive(2);

    // Simulate P4's load-time harvest: a map bound to the current
    // durable epoch, volume calibrated.
    let map_epoch = control.mark_calibrated();
    assert!(
        wrap_map_is_servable(map_epoch, control.write_epoch()),
        "freshly harvested map is servable"
    );

    drive.write_block(&[0u8; 64]).expect("write");

    assert!(
        !wrap_map_is_servable(map_epoch, control.write_epoch()),
        "after the fence, the pre-write map's epoch no longer matches and it is never served"
    );
    assert!(
        !control.is_calibrated(),
        "fence marked the volume uncalibrated"
    );
    assert_eq!(
        control.calibration_generation(),
        1,
        "a fresh calibration generation was allocated"
    );
}

// ---------------------------------------------------------------------
//  The structural guard bites: source audit
// ---------------------------------------------------------------------

/// How a file may use write-direction transport calls.
enum DispatchPolicy {
    /// Any number of occurrences, with a recorded justification.
    AllowAny(&'static str),
    /// Exactly this many occurrences of each pattern.
    Exact {
        execute_out: usize,
        execute_none: usize,
        why: &'static str,
    },
}

/// Fails when a write-direction transport call (`.execute_out(` or
/// `.execute_none(`) appears anywhere in the workspace outside the
/// sanctioned files, or when the sanctioned production files drift
/// from their pinned counts. Adding a ninth dispatch site — in any
/// crate, production or test — turns up here and must be classified
/// deliberately.
#[test]
fn media_dispatch_gate_is_the_sole_write_direction_dispatcher() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates_root = manifest
        .parent()
        .expect("remanence-library sits under crates/");

    let policies: &[(&str, DispatchPolicy)] = &[
        (
            "remanence-library/src/handle/tape_io/media_gate.rs",
            DispatchPolicy::Exact {
                // The gate's data-out dispatch + the test-only
                // UnitAttentionOnce forwarder in its unit tests.
                execute_out: 2,
                // The gate's no-data dispatch, the non-media
                // passthrough, and the test-only forwarder.
                execute_none: 3,
                why: "the media-dispatch gate itself",
            },
        ),
        (
            "remanence-library/src/handle/mod.rs",
            DispatchPolicy::Exact {
                execute_out: 0,
                // The three CHANGER-transport sites (MOVE MEDIUM
                // path probe, move_medium, PREVENT/ALLOW). The
                // changer is a medium-changer device: rem builds no
                // tape-media CDB for it, and the drive transport in
                // this file is the fenced wrapper.
                execute_none: 3,
                why: "changer-transport dispatch only",
            },
        ),
        (
            "remanence-library/src/transport.rs",
            DispatchPolicy::Exact {
                execute_out: 4,
                execute_none: 8,
                why: "the SgTransport abstraction: trait plumbing, forwarders \
                      (Box/Recording/Foreign) and its own unit tests",
            },
        ),
        (
            "remanence-api/src/write_owner.rs",
            DispatchPolicy::Exact {
                execute_out: 1,
                execute_none: 1,
                why: "cfg(test) TurScriptTransport forwarder only; production \
                      write_owner code dispatches through DriveHandle",
            },
        ),
        (
            "remanence-cli/src/lib.rs",
            DispatchPolicy::Exact {
                execute_out: 1,
                execute_none: 1,
                why: "cfg(test) TurSequenceTransport forwarder only; production \
                      CLI code dispatches through DriveHandle",
            },
        ),
        (
            "remanence-library/src/handle/tests.rs",
            DispatchPolicy::AllowAny("test-only SgTransport forwarders"),
        ),
        (
            "remanence-library/src/handle/tape_io/tests.rs",
            DispatchPolicy::AllowAny("test-only SgTransport forwarders"),
        ),
        (
            "remanence-library/src/handle/tape_io/media_gate_tests.rs",
            DispatchPolicy::AllowAny("this file's failure-injection transports"),
        ),
        (
            "remanence-library/tests/quadstor_smoke.rs",
            DispatchPolicy::AllowAny(
                "hardware smoke test writing through the raw public surface; \
                 ignored by default, needs REM_QUADSTOR_DRIVE_PATH; the \
                 out-of-band-writer case is covered by write_path_trust (D4c)",
            ),
        ),
        (
            "remanence-chaos/src/lib.rs",
            DispatchPolicy::Exact {
                execute_out: 4,
                execute_none: 13,
                why: "fault-injection SgTransport wrapper and its tests; it \
                      wraps a transport owned by the handles and initiates \
                      nothing",
            },
        ),
        (
            "remanence-chaos/src/model.rs",
            DispatchPolicy::Exact {
                execute_out: 12,
                execute_none: 9,
                why: "in-memory virtual-world SgTransport model and its \
                      tests; no real device behind it",
            },
        ),
    ];

    let mut rust_files = Vec::new();
    collect_rust_files(crates_root, &mut rust_files);
    assert!(
        rust_files.len() > 100,
        "audit walked implausibly few files ({}); wrong root?",
        rust_files.len()
    );

    let mut violations = Vec::new();
    for path in &rust_files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("audit failed to read {}: {e}", path.display()));
        let out_count = count_occurrences(&text, ".execute_out(");
        let none_count = count_occurrences(&text, ".execute_none(");
        if out_count == 0 && none_count == 0 {
            continue;
        }
        let normalized = path.to_string_lossy().replace('\\', "/");
        match policies
            .iter()
            .find(|(suffix, _)| normalized.ends_with(suffix))
        {
            Some((_, DispatchPolicy::AllowAny(why))) => {
                assert!(
                    !why.is_empty(),
                    "every allowlisted dispatch file needs a recorded justification"
                );
            }
            Some((
                suffix,
                DispatchPolicy::Exact {
                    execute_out,
                    execute_none,
                    why,
                },
            )) => {
                if out_count != *execute_out || none_count != *execute_none {
                    violations.push(format!(
                        "{suffix}: expected exactly {execute_out} `.execute_out(` and \
                         {execute_none} `.execute_none(` ({why}); found {out_count} and \
                         {none_count} — a write-direction dispatch site changed; route it \
                         through the media gate or update this audit deliberately"
                    ));
                }
            }
            None => {
                violations.push(format!(
                    "{normalized}: {out_count} `.execute_out(` / {none_count} `.execute_none(` \
                     outside the sanctioned dispatch files — every write-direction transport \
                     call must go through MediaFencedTransport::dispatch_media_cdb"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "write-direction dispatch outside the media gate:\n{}",
        violations.join("\n")
    );
}

fn collect_rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip build output; everything else under crates/ is
            // source (src/, tests/, benches/, examples/).
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn count_occurrences(text: &str, needle: &str) -> usize {
    text.matches(needle).count()
}
