//! Layer 3a Step 9.8 — QuadStor VTL smoke test.
//!
//! `#[ignore]`-gated by default. Runs only when:
//!
//! - On a Linux host that can open `/dev/sg*` for the QuadStor
//!   VTL.
//! - The user has set env vars pointing at the test drive's SCSI
//!   generic path and (for the round-trip variant) at a loaded
//!   scratch tape.
//!
//! ## Env vars
//!
//! - `REM_QUADSTOR_DRIVE_PATH` (required) — the `/dev/sgN` path of
//!   the tape drive to exercise. Layer 3a CDBs go straight at it.
//! - `REM_QUADSTOR_WRITE_LOOP` (optional, `"1"` to enable) — if
//!   set, the test issues the 100×1 MiB write / read-back loop
//!   from the Step 9.8 design. **This writes to the loaded
//!   cartridge — only enable on a scratch tape.**
//!
//! ## Invocation
//!
//! ```text
//! REM_QUADSTOR_DRIVE_PATH=/dev/sg5 \
//! cargo test -p remanence-library --test quadstor_smoke -- \
//!   --ignored --test-threads=1 --nocapture
//! ```
//!
//! Without `REM_QUADSTOR_DRIVE_PATH` the test prints a skip
//! message and returns `Ok(())` rather than failing — there's no
//! way to assert anything without the hardware, so we don't
//! pretend to.

#![cfg(target_os = "linux")]

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::time::Duration;

use remanence_library::scsi::{self, ScsiError};
use remanence_library::transport::{LinuxSgTransport, SgTransport, TimeoutClass};

fn drive_path() -> Option<PathBuf> {
    std::env::var("REM_QUADSTOR_DRIVE_PATH")
        .ok()
        .map(PathBuf::from)
}

fn write_loop_enabled() -> bool {
    matches!(
        std::env::var("REM_QUADSTOR_WRITE_LOOP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn skip_if_no_hardware() -> Option<PathBuf> {
    match drive_path() {
        Some(p) => {
            if !p.exists() {
                eprintln!(
                    "quadstor_smoke: skipping — REM_QUADSTOR_DRIVE_PATH={p:?} does not exist"
                );
                return None;
            }
            // Open-check (non-destructive).
            match OpenOptions::new().read(true).write(true).open(&p) {
                Ok(_) => Some(p),
                Err(e) => {
                    eprintln!(
                        "quadstor_smoke: skipping — cannot open {p:?}: {e}. \
                         Need read+write access (tape group + CAP_SYS_RAWIO, \
                         or root)."
                    );
                    None
                }
            }
        }
        None => {
            eprintln!(
                "quadstor_smoke: skipping — REM_QUADSTOR_DRIVE_PATH not set. \
                 To run: REM_QUADSTOR_DRIVE_PATH=/dev/sgN \
                 cargo test -p remanence-library --test quadstor_smoke -- \
                 --ignored --test-threads=1 --nocapture"
            );
            None
        }
    }
}

fn open_transport(path: &PathBuf) -> Box<dyn SgTransport> {
    // open_rw, not open: the smoke tests issue state-changing
    // CDBs (REWIND, MODE SENSE, WRITE) that need RW access.
    // LinuxSgTransport::open is reserved for read-only discovery
    // paths and may open O_RDONLY internally — codex 20:30
    // (idref=8915b570 Medium) caught the wrong constructor.
    Box::new(
        LinuxSgTransport::open_rw(path).unwrap_or_else(|e| panic!("failed to open {path:?}: {e}")),
    )
}

/// Step 9.8a — basic CDB round-trip smoke. No writes to tape:
/// just REWIND + READ POSITION + READ BLOCK LIMITS + MODE SENSE.
/// Verifies the full sg_io + Layer 1 + transport stack works end
/// to end on real hardware without touching cartridge state.
#[test]
#[ignore]
fn quadstor_basic_smoke() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    let mut t = open_transport(&path);

    // 1. REWIND. The QuadStor VTL accepts REWIND even with no
    // tape (it's a virtual drive); on a hardware drive this would
    // need a loaded cartridge.
    t.set_timeout_for(TimeoutClass::Rewind);
    let rewind_cdb = scsi::rewind::build_cdb();
    match t.execute_none(&rewind_cdb) {
        Ok(()) => eprintln!("REWIND ok"),
        Err(ScsiError::CheckCondition { sense, .. }) => {
            eprintln!(
                "REWIND CHECK CONDITION (likely no tape loaded): \
                 sense={sense:02x?}"
            );
        }
        Err(e) => panic!("REWIND transport error: {e}"),
    }

    // 2. READ POSITION (long form). Drive may return NOT READY
    // if no tape is loaded; that's expected on a fresh VTL.
    t.set_timeout_for(TimeoutClass::TapeStatus);
    let rp_cdb = scsi::read_position::build_cdb_long();
    let mut rp_buf = [0u8; 32];
    match t.execute_in(&rp_cdb, &mut rp_buf) {
        Ok(outcome) => {
            eprintln!(
                "READ POSITION ok: bytes={}, flags=0x{:02x}",
                outcome.bytes_transferred, rp_buf[0]
            );
        }
        Err(ScsiError::CheckCondition { sense, .. }) => {
            eprintln!("READ POSITION CHECK CONDITION (no tape?): {sense:02x?}");
        }
        Err(e) => panic!("READ POSITION transport error: {e}"),
    }

    // 3. READ BLOCK LIMITS. Always succeeds — the drive reports
    // its block-size capabilities regardless of cartridge state.
    t.set_timeout_for(TimeoutClass::TapeStatus);
    let rbl_cdb = scsi::read_block_limits::build_cdb();
    let mut rbl_buf = [0u8; 6];
    match t.execute_in(&rbl_cdb, &mut rbl_buf) {
        Ok(_) => {
            let limits =
                scsi::read_block_limits::parse_response(&rbl_buf).expect("RBL response parses");
            eprintln!(
                "READ BLOCK LIMITS: max={} bytes, min={} bytes, granularity={}",
                limits.max_block_length, limits.min_block_length, limits.granularity
            );
            assert!(
                limits.max_block_length > 0,
                "drive reports zero max block length"
            );
        }
        Err(e) => panic!("READ BLOCK LIMITS error: {e}"),
    }

    // 4. MODE SENSE(6) page 0x0F. Should succeed regardless of
    // cartridge state.
    t.set_timeout_for(TimeoutClass::ModeConfig);
    let ms_cdb = scsi::mode::build_mode_sense6_cdb(
        scsi::mode::PageControl::Current,
        scsi::mode::PAGE_DATA_COMPRESSION,
        64,
    );
    let mut ms_buf = [0u8; 64];
    match t.execute_in(&ms_cdb, &mut ms_buf) {
        Ok(outcome) => {
            eprintln!(
                "MODE SENSE 0x0F ok: bytes={}, BDL={}",
                outcome.bytes_transferred, ms_buf[3]
            );
        }
        Err(e) => panic!("MODE SENSE error: {e}"),
    }

    eprintln!("quadstor_basic_smoke: all checks passed");
}

/// Step 9.8b — the write/read-back loop from the design. 100 ×
/// 1 MiB variable-block writes, REWIND, 100 reads, byte-for-byte
/// compare. **Destructive: writes to the loaded cartridge.**
/// Off by default; enable with `REM_QUADSTOR_WRITE_LOOP=1`.
#[test]
#[ignore]
fn quadstor_write_read_round_trip() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_write_read_round_trip: skipping — \
             REM_QUADSTOR_WRITE_LOOP not set to 1. This test writes \
             to the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }
    let mut t = open_transport(&path);

    const N_BLOCKS: usize = 100;
    const BLOCK_SIZE: usize = 1024 * 1024;

    // Rewind first.
    t.set_timeout_for(TimeoutClass::Rewind);
    t.execute_none(&scsi::rewind::build_cdb())
        .expect("REWIND ok");

    // Generate 100 distinct payloads with a pseudo-random pattern
    // (LCG seeded by block index) so we can byte-compare on read-back.
    let payloads: Vec<Vec<u8>> = (0..N_BLOCKS)
        .map(|i| {
            let mut buf = vec![0u8; BLOCK_SIZE];
            let mut x = 0x12345678u32.wrapping_add(i as u32);
            for chunk in buf.chunks_mut(4) {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                let bytes = x.to_le_bytes();
                for (b, src) in chunk.iter_mut().zip(bytes.iter()) {
                    *b = *src;
                }
            }
            buf
        })
        .collect();

    // WRITE loop.
    t.set_timeout_for(TimeoutClass::TapeIo);
    for (i, payload) in payloads.iter().enumerate() {
        let cdb = scsi::read_write::build_write_variable_cdb(payload.len() as u32);
        t.execute_out(&cdb, payload)
            .unwrap_or_else(|e| panic!("WRITE block {i} failed: {e}"));
    }
    eprintln!("wrote {N_BLOCKS} × {BLOCK_SIZE} bytes");

    // WRITE FILEMARKS(1) to commit + separate.
    t.set_timeout_for(TimeoutClass::WriteFilemarks);
    t.execute_none(&scsi::write_filemarks::build_cdb_6(1))
        .expect("WRITE FILEMARKS ok");

    // REWIND, then READ back and compare.
    t.set_timeout_for(TimeoutClass::Rewind);
    t.execute_none(&scsi::rewind::build_cdb())
        .expect("REWIND ok");

    t.set_timeout_for(TimeoutClass::TapeIo);
    for (i, expected) in payloads.iter().enumerate() {
        let cdb = scsi::read_write::build_read_variable_cdb(BLOCK_SIZE as u32);
        let mut buf = vec![0u8; BLOCK_SIZE];
        let outcome = t
            .execute_in(&cdb, &mut buf)
            .unwrap_or_else(|e| panic!("READ block {i} failed: {e}"));
        assert_eq!(
            outcome.bytes_transferred as usize, BLOCK_SIZE,
            "block {i} returned {} bytes",
            outcome.bytes_transferred
        );
        assert_eq!(&buf[..], &expected[..], "block {i} content mismatch");
    }
    eprintln!("read back {N_BLOCKS} × {BLOCK_SIZE} bytes, all byte-for-byte equal");

    // Sanity sleep so the operator can see the message before
    // the test harness clears the terminal.
    std::thread::sleep(Duration::from_millis(100));
}
