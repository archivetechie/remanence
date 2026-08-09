//! Layer 3c Step 11.18 — Quadstor VTL parity smoke tests.
//!
//! `#[ignore]`-gated by default. Runs only when env vars point
//! at a Quadstor VTL drive that's safe to write to.
//!
//! ## Env vars
//!
//! - `REM_QUADSTOR_PARITY_DRIVE_PATH` (required) — `/dev/sgN`
//!   path of the tape drive.
//! - `REM_QUADSTOR_PARITY_WRITE_LOOP` (optional, `"1"` to
//!   enable) — gate the destructive write/read-back loop.
//! - `REM_QUADSTOR_PARITY_LIBRARY_SERIAL` (optional) — require
//!   discovery to select this logical library serial.
//! - `REM_QUADSTOR_PARITY_DRIVE_BAY` (optional) — require this
//!   drive element address, decimal or `0x`-prefixed hex.
//! - `REM_QUADSTOR_PARITY_ALLOW_DERIVED_DRIVE` (optional, `"1"` to
//!   enable) — allow Layer 2b derived drive identities.
//! - `REM_QUADSTOR_PARITY_BLOCK_SIZE` (optional) — fixed block size
//!   for the smoke tape, default `262144`.
//! - `REM_QUADSTOR_PARITY_JOURNAL_PATH` (optional) — journal file
//!   path for `quadstor_parity_journaled_session`; default is a
//!   unique temp path.
//!
//! ## Invocation
//!
//! ```text
//! REM_QUADSTOR_PARITY_DRIVE_PATH=/dev/sg5 \
//! REM_QUADSTOR_PARITY_WRITE_LOOP=1 \
//! cargo test -p remanence-parity --test quadstor_parity -- \
//!   --ignored --test-threads=1 --nocapture
//! ```
//!
//! Without the env vars the test prints a skip message and
//! returns `Ok(())` — there's no way to validate anything
//! without the hardware.
//!
//! ## What it does
//!
//! Writes one small parity-protected epoch through `ParitySink`
//! over `DriveHandleRawSink`, rewinds, reconstructs the filemark map
//! through `DriveHandleRawSource`, validates the final bootstrap
//! digest, and reads the object back through `ObjectParitySource`.
//! The restart/append tests then reopen through the production resume path:
//! one appends after a clean finalized bootstrap tail, and two rebuild open
//! `W<T` epochs from catalog-committed object-only prefixes before appending
//! (single-epoch and multi-epoch rebuild). The recovery test wraps the real
//! hardware source with a single synthetic transport read failure to prove
//! `ObjectParitySource`
//! reconstructs the protected block from the sidecar.
//! This is deliberately destructive and should be run only on a
//! scratch Quadstor cartridge.
//!
//! The injected failure is above the SCSI sense-code layer: it uses the
//! existing completion-unknown transport-error recovery path instead of
//! inventing a CHECK CONDITION tuple.

#![cfg(target_os = "linux")]

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use remanence_library::{
    BlockRead, BlockSink, DriveHandle, StaticAllowlist, TapeConfig, TapeIoError,
};
use remanence_parity::{
    checked_bounded_resume_summary, emit_resume_rebuilt_sidecars_to_raw,
    rebuild_open_epoch_from_bounded_summary, scan_reconstruct_filemark_map,
    BoundedResumeWriterSeed, CommittedBundle, CommittedBundleKind, CommittedState,
    DriveHandleRawSink, DriveHandleRawSource, FileTapeFileJournal, FilemarkMap, JournalError,
    ObjectParitySource, OpenTrust, ParityError, ParityScheme, ParitySink, PhysicalPositionHint,
    RawReadOutcome, RawTapeSource, SchemeId, ScopedFilemarkMap, SpaceFilemarksOutcome,
    TapeFileJournal, TapeFileKind, TapeFilePosition, TerminalTripleCloseInput,
    DEFAULT_INDEX_SEPARATION_BYTES, DEFAULT_SCHEME_BLOCK_SIZE_BYTES,
};

const TAPE_UUID: [u8; 16] = [
    0x52, 0x45, 0x4d, 0x51, 0x55, 0x41, 0x44, 0x53, 0x54, 0x4f, 0x52, 0x33, 0x43, 0x00, 0x01, 0x00,
];

#[derive(Default)]
struct FixtureJournal {
    bundles: Vec<CommittedBundle>,
}

impl TapeFileJournal for FixtureJournal {
    fn tape_uuid(&self) -> [u8; 16] {
        TAPE_UUID
    }

    fn commit_bundle(&mut self, bundle: &CommittedBundle) -> Result<(), JournalError> {
        self.bundles.push(bundle.clone());
        Ok(())
    }

    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        let retained_end = self
            .bundles
            .iter()
            .rposition(|bundle| bundle.kind == CommittedBundleKind::CheckpointedThrough)
            .map_or(0, |index| index + 1);
        let retained = &self.bundles[..retained_end];
        let last = retained
            .iter()
            .rev()
            .find(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough);
        Ok(CommittedState {
            entries: retained
                .iter()
                .filter(|bundle| bundle.kind != CommittedBundleKind::CheckpointedThrough)
                .flat_map(|bundle| bundle.entries.iter().cloned())
                .collect(),
            highest_protected_ordinal: last.map_or(0, |bundle| bundle.highest_protected_ordinal),
            total_committed_ordinals: last.map_or(0, |bundle| bundle.total_committed_ordinals),
            orphaned_bundles: self.bundles[retained_end..].to_vec(),
        })
    }
}

fn fixture_journal() -> &'static mut FixtureJournal {
    Box::leak(Box::new(FixtureJournal::default()))
}

fn drive_path() -> Option<PathBuf> {
    std::env::var("REM_QUADSTOR_PARITY_DRIVE_PATH")
        .ok()
        .map(PathBuf::from)
}

fn write_loop_enabled() -> bool {
    matches!(
        std::env::var("REM_QUADSTOR_PARITY_WRITE_LOOP").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn allow_derived_drive_identity() -> bool {
    matches!(
        std::env::var("REM_QUADSTOR_PARITY_ALLOW_DERIVED_DRIVE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn block_size() -> u32 {
    std::env::var("REM_QUADSTOR_PARITY_BLOCK_SIZE")
        .ok()
        .map(|value| {
            value.parse::<u32>().unwrap_or_else(|err| {
                panic!("invalid REM_QUADSTOR_PARITY_BLOCK_SIZE={value:?}: {err}")
            })
        })
        .unwrap_or(DEFAULT_SCHEME_BLOCK_SIZE_BYTES)
}

fn configure_parity_write_session(
    drive: &mut DriveHandle,
    block_size: u32,
    label: &str,
) -> TapeConfig {
    let original_config = drive.read_config().expect("read original tape config");
    assert!(
        original_config.max_block_size_bytes >= block_size,
        "drive max block size {} is smaller than requested parity block size {block_size}",
        original_config.max_block_size_bytes
    );
    {
        let mut raw_sink = DriveHandleRawSink::new(drive);
        raw_sink
            .configure_parity_write_session(block_size)
            .unwrap_or_else(|err| {
                panic!("configure fixed block size and read-back-verified compression-off for {label}: {err}")
            });
    }
    original_config
}

fn journal_path(name: &str) -> PathBuf {
    if let Ok(path) = std::env::var("REM_QUADSTOR_PARITY_JOURNAL_PATH") {
        return PathBuf::from(path);
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "remanence-quadstor-{name}-{}-{stamp}.remjournal",
        std::process::id()
    ))
}

fn parse_bay_address(value: &str) -> u16 {
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        u16::from_str_radix(hex, 16)
    } else {
        value.parse::<u16>()
    }
    .unwrap_or_else(|err| panic!("invalid REM_QUADSTOR_PARITY_DRIVE_BAY={value:?}: {err}"))
}

fn skip_if_no_hardware() -> Option<PathBuf> {
    match drive_path() {
        Some(p) if p.exists() => match OpenOptions::new().read(true).write(true).open(&p) {
            Ok(_) => Some(p),
            Err(e) => {
                eprintln!(
                    "quadstor_parity: skipping — cannot open {p:?}: {e}. Need tape group + \
                         CAP_SYS_RAWIO or root."
                );
                None
            }
        },
        Some(p) => {
            eprintln!("quadstor_parity: skipping — {p:?} does not exist");
            None
        }
        None => {
            eprintln!(
                "quadstor_parity: skipping — REM_QUADSTOR_PARITY_DRIVE_PATH not set. \
                 See module docs for invocation."
            );
            None
        }
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn resolve_library_drive_for_path(drive_path: &Path) -> (remanence_library::Library, u16) {
    let desired_library = std::env::var("REM_QUADSTOR_PARITY_LIBRARY_SERIAL").ok();
    let desired_bay = std::env::var("REM_QUADSTOR_PARITY_DRIVE_BAY")
        .ok()
        .map(|value| parse_bay_address(&value));

    let report = remanence_library::discover().unwrap_or_else(|err| {
        panic!("quadstor_parity_roundtrip: discovery failed before opening {drive_path:?}: {err}")
    });

    let mut matches = Vec::new();
    for library in report.libraries {
        if desired_library
            .as_ref()
            .is_some_and(|serial| serial != &library.serial)
        {
            continue;
        }
        for bay in &library.drive_bays {
            if desired_bay.is_some_and(|expected| expected != bay.element_address) {
                continue;
            }
            let Some(installed) = bay.installed.as_ref() else {
                continue;
            };
            let Some(sg_path) = installed.sg_path.as_deref() else {
                continue;
            };
            if same_path(sg_path, drive_path) {
                matches.push((library.clone(), bay.element_address));
            }
        }
    }

    match matches.len() {
        1 => matches.pop().expect("one match"),
        0 => panic!(
            "quadstor_parity_roundtrip: discovery found no library drive matching {drive_path:?}. \
             Set REM_QUADSTOR_PARITY_LIBRARY_SERIAL and REM_QUADSTOR_PARITY_DRIVE_BAY if the host has multiple VTLs."
        ),
        n => panic!(
            "quadstor_parity_roundtrip: discovery found {n} drives matching {drive_path:?}; \
             set REM_QUADSTOR_PARITY_LIBRARY_SERIAL and REM_QUADSTOR_PARITY_DRIVE_BAY"
        ),
    }
}

fn smoke_scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("quadstor-parity-smoke"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    }
}

fn capacity_input(
    block_size: u32,
    sidecar_entries_before_object: u64,
    structural_entries_before_object: u64,
    object_rows_before_object: u64,
) -> TerminalTripleCloseInput {
    TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks: 2,
        block_size_bytes: block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 2,
        parity_shards_per_epoch: 1,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object,
        structural_entries_before_object,
        object_rows_before_object,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: 8,
        remaining_tape_blocks: 1_000_000,
        capacity_basis_blocks: 1_000_000,
        low_watermark_blocks: 0,
        high_watermark_blocks: 1,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: u64::MAX,
    }
}

fn begin_two_block_object(
    sink: &mut ParitySink<'_>,
    block_size: u32,
    sidecars: u64,
    structural: u64,
    objects: u64,
) {
    let reservation = capacity_input(block_size, sidecars, structural, objects)
        .reserve_object()
        .expect("terminal-triple Object reservation");
    let (tape_file_number, _) = sink
        .begin_object_with_terminal_triple_reservation(reservation)
        .expect("begin reserved Object");
    assert_eq!(tape_file_number, structural);
}

fn block(seed: u8, block_size: u32) -> Vec<u8> {
    let mut out = vec![0u8; block_size as usize];
    let mut x = u32::from(seed).wrapping_mul(0x045d_9f3b);
    for chunk in out.chunks_mut(4) {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        for (dst, src) in chunk.iter_mut().zip(x.to_le_bytes()) {
            *dst = src;
        }
    }
    out
}

fn assert_map_kinds(map: &FilemarkMap, expected: &[TapeFileKind]) {
    assert_eq!(
        map.entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        expected
    );
}

fn read_object_blocks(
    source: &mut dyn RawTapeSource,
    scheme: ParityScheme,
    scoped: ScopedFilemarkMap,
    block_size: u32,
    tape_file_number: u32,
    expected_blocks: &[Vec<u8>],
) {
    let mut object_source = ObjectParitySource::open(
        source,
        scheme,
        TAPE_UUID,
        scoped,
        block_size,
        u64::from(tape_file_number),
        OpenTrust::RequireValidated,
    )
    .expect("open hardware object source");
    let mut read_buf = vec![0u8; block_size as usize];
    for (index, expected) in expected_blocks.iter().enumerate() {
        object_source
            .read_block(&mut read_buf)
            .unwrap_or_else(|err| panic!("read object {tape_file_number} block {index}: {err}"));
        assert_eq!(&read_buf, expected);
    }
}

struct InjectReadFaultOnce<'a> {
    inner: &'a mut dyn RawTapeSource,
    fail_at: PhysicalPositionHint,
    injected: bool,
}

impl<'a> InjectReadFaultOnce<'a> {
    fn new(inner: &'a mut dyn RawTapeSource, fail_at: PhysicalPositionHint) -> Self {
        Self {
            inner,
            fail_at,
            injected: false,
        }
    }

    fn injected(&self) -> bool {
        self.injected
    }
}

impl RawTapeSource for InjectReadFaultOnce<'_> {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        self.inner.configure_fixed_block_size(block_size)
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.inner.locate_physical(hint)
    }

    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.locate_end_of_data()
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        self.inner.space_filemarks(count)
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let position = self.inner.position()?;
        if !self.injected && position == self.fail_at {
            self.injected = true;
            return Err(ParityError::TapeIo(synthetic_transport_read_error()));
        }
        self.inner.read_record(buf)
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.position()
    }
}

fn synthetic_transport_read_error() -> TapeIoError {
    TapeIoError::Transport(remanence_library::scsi::ScsiError::TransportError {
        status: 0,
        host_status: 0,
        driver_status: 0,
        info: 0,
        sense: Vec::new(),
    })
}

#[test]
#[ignore]
fn quadstor_parity_roundtrip() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_parity_roundtrip: skipping — REM_QUADSTOR_PARITY_WRITE_LOOP not set to 1. \
             This test writes to the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let first_block = block(0x11, block_size);
    let second_block = block(0x22, block_size);
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    eprintln!(
        "quadstor_parity_roundtrip: selected library {} bay 0x{bay_address:04x} at {path:?}",
        library.serial
    );

    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open selected Quadstor library for state-changing parity smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected Quadstor drive");
    let original_config = configure_parity_write_session(&mut drive, block_size, "parity smoke");
    drive.rewind().expect("rewind before destructive write");

    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_with_journal(
            &mut raw_sink,
            fixture_journal(),
            scheme.clone(),
            TAPE_UUID,
            block_size,
        )
        .expect("construct hardware parity sink");
        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        begin_two_block_object(&mut sink, block_size, 0, 1, 0);
        sink.write_block(&first_block)
            .expect("write object block 0");
        sink.write_block(&second_block)
            .expect("write object block 1");
        let object = sink.finish_object().expect("finish object");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        sink.finish().expect("finish parity session");
    }

    drive.rewind().expect("rewind before verification read");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less hardware scan reconstructs filemark map");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 2);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 2);

        let scoped = ScopedFilemarkMap::from_catalog(scanned.clone(), 2);
        read_object_blocks(
            &mut raw_source,
            scheme,
            scoped,
            block_size,
            1,
            &[first_block, second_block],
        );
    }

    drive.rewind().expect("rewind after verification");
    drive
        .write_config(original_config)
        .expect("restore original tape config after parity smoke");
}

#[test]
#[ignore]
fn quadstor_parity_journaled_session() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_parity_journaled_session: skipping — REM_QUADSTOR_PARITY_WRITE_LOOP not set to 1. \
             This test writes to the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let first_block = block(0x71, block_size);
    let second_block = block(0x72, block_size);
    let journal_path = journal_path("journaled-session");
    let _ = std::fs::remove_file(&journal_path);
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    eprintln!(
        "quadstor_parity_journaled_session: selected library {} bay 0x{bay_address:04x} at {path:?}, journal {journal_path:?}",
        library.serial
    );

    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open selected Quadstor library for journaled parity smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected Quadstor drive");
    let original_config = configure_parity_write_session(&mut drive, block_size, "journaled smoke");
    drive.rewind().expect("rewind before destructive write");

    {
        let mut journal =
            FileTapeFileJournal::open(&journal_path, TAPE_UUID, block_size, scheme.clone())
                .expect("open trusted local FileTapeFileJournal");
        {
            let mut raw_sink = DriveHandleRawSink::new(&mut drive);
            let mut sink = ParitySink::new_with_journal(
                &mut raw_sink,
                &mut journal,
                scheme.clone(),
                TAPE_UUID,
                block_size,
            )
            .expect("construct journaled hardware parity sink");
            assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
            begin_two_block_object(&mut sink, block_size, 0, 1, 0);
            sink.write_block(&first_block)
                .expect("write journaled object block 0");
            sink.write_block(&second_block)
                .expect("write journaled object block 1");
            let object = sink.finish_object().expect("finish journaled object");
            assert_eq!(object.tape_file_number, 1);
            assert_eq!(object.sidecars_emitted.len(), 1);
            assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
            let checkpoint = sink.checkpoint().expect("checkpoint journaled session");
            assert_eq!(checkpoint.next_tape_file_number, 3);
        }

        let state = journal
            .load_committed()
            .expect("journal replay after checkpoint");
        assert_eq!(state.highest_protected_ordinal, 2);
        assert_eq!(state.total_committed_ordinals, 2);
        let committed_map = state.filemark_map().expect("journal map validates");
        assert_map_kinds(
            &committed_map,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
            ],
        );
    }

    let reopened = FileTapeFileJournal::open(&journal_path, TAPE_UUID, block_size, scheme.clone())
        .expect("reopen journal for bounded resume planning");
    let snapshot = reopened
        .committed_snapshot_bounded()
        .expect("freeze bounded journal checkpoint");
    let summary =
        checked_bounded_resume_summary(&snapshot).expect("summarize bounded journal checkpoint");
    assert_eq!(summary.last_committed_tape_file_number, Some(2));

    drive.rewind().expect("rewind after journaled smoke");
    drive
        .write_config(original_config)
        .expect("restore original tape config after journaled smoke");
    let _ = std::fs::remove_file(journal_path);
}

#[test]
#[ignore]
fn quadstor_parity_recovers_from_injected_read_fault() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_parity_recovers_from_injected_read_fault: skipping — \
             REM_QUADSTOR_PARITY_WRITE_LOOP not set to 1. This test writes \
             to the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let first_block = block(0x51, block_size);
    let second_block = block(0x52, block_size);
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    eprintln!(
        "quadstor_parity_recovers_from_injected_read_fault: selected library {} bay 0x{bay_address:04x} at {path:?}",
        library.serial
    );

    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open selected Quadstor library for recovery smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected Quadstor drive");
    let original_config = configure_parity_write_session(&mut drive, block_size, "recovery smoke");
    drive.rewind().expect("rewind before destructive write");

    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_with_journal(
            &mut raw_sink,
            fixture_journal(),
            scheme.clone(),
            TAPE_UUID,
            block_size,
        )
        .expect("construct hardware parity sink");
        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        begin_two_block_object(&mut sink, block_size, 0, 1, 0);
        sink.write_block(&first_block)
            .expect("write object block 0");
        sink.write_block(&second_block)
            .expect("write object block 1");
        let object = sink.finish_object().expect("finish object");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        sink.finish().expect("finish parity session");
    }

    drive.rewind().expect("rewind before recovery scan");
    let scanned = {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs recovery-smoke map");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 2);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 2);
        scanned
    };
    let fault_position = scanned
        .physical_position(TapeFilePosition {
            tape_file_number: 1,
            block_within_file: 1,
        })
        .expect("object block 1 physical position resolves");

    {
        let mut drive_source = DriveHandleRawSource::new(&mut drive);
        let scoped = ScopedFilemarkMap::from_catalog(scanned.clone(), 2);
        let mut faulting_source = InjectReadFaultOnce::new(&mut drive_source, fault_position);
        read_object_blocks(
            &mut faulting_source,
            scheme,
            scoped,
            block_size,
            1,
            &[first_block, second_block],
        );
        assert!(
            faulting_source.injected(),
            "targeted read fault must be injected at object tape_file 1 body LBA 1"
        );
    }

    drive.rewind().expect("rewind after recovery verification");
    drive
        .write_config(original_config)
        .expect("restore original tape config after recovery smoke");
}

#[test]
#[ignore]
fn quadstor_parity_bounded_resume_append_roundtrip() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!("quadstor_parity_bounded_resume_append_roundtrip: destructive gate is disabled");
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let journal_path = journal_path("bounded-resume");
    let _ = std::fs::remove_file(&journal_path);
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open library for bounded resume smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open drive for bounded resume smoke");
    let original_config =
        configure_parity_write_session(&mut drive, block_size, "bounded resume smoke");
    drive.rewind().expect("rewind before bounded resume write");

    let mut journal =
        FileTapeFileJournal::open(&journal_path, TAPE_UUID, block_size, scheme.clone())
            .expect("open bounded resume journal");
    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_with_journal(
            &mut raw_sink,
            &mut journal,
            scheme.clone(),
            TAPE_UUID,
            block_size,
        )
        .expect("construct initial bounded-resume sink");
        assert_eq!(sink.write_bootstrap().expect("write sole BOT Bootstrap"), 0);
        begin_two_block_object(&mut sink, block_size, 0, 1, 0);
        sink.write_block(&block(0x81, block_size))
            .expect("write first prefix block");
        sink.write_block(&block(0x82, block_size))
            .expect("write second prefix block");
        sink.finish_object()
            .expect("commit first Object and sidecar");
        sink.checkpoint().expect("checkpoint first bounded prefix");
    }

    let snapshot = journal
        .committed_snapshot_bounded()
        .expect("freeze bounded prefix");
    let summary = checked_bounded_resume_summary(&snapshot).expect("validate bounded summary");
    assert_eq!(summary.committed_tape_file_count, 3);
    assert!(summary.open_epoch_object_extents.is_empty());

    let rebuild = {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        rebuild_open_epoch_from_bounded_summary(
            &mut raw_source,
            &summary,
            &scheme,
            TAPE_UUID,
            block_size,
        )
        .expect("rebuild bounded open epoch")
    };
    let resume_result = {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            &mut journal,
            rebuild.plan,
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| Ok(()),
        )
        .expect("complete bounded resume plan")
    };
    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_sidecar_only_from_bounded_resume(
            &mut raw_sink,
            &mut journal,
            scheme.clone(),
            TAPE_UUID,
            block_size,
            BoundedResumeWriterSeed {
                committed_prefix_snapshot: snapshot,
                committed_prefix_summary: summary,
                resume_result: &resume_result,
                live_epoch: rebuild.live_epoch,
            },
        )
        .expect("open bounded append sink");
        begin_two_block_object(&mut sink, block_size, 1, 3, 1);
        sink.write_block(&block(0x91, block_size))
            .expect("write resumed block one");
        sink.write_block(&block(0x92, block_size))
            .expect("write resumed block two");
        sink.finish_object()
            .expect("commit resumed Object and sidecar");
        sink.checkpoint().expect("checkpoint resumed prefix");
    }

    drive
        .rewind()
        .expect("rewind before bounded resume verification");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("scan bounded-resume physical prefix");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
            ],
        );
    }

    drive.rewind().expect("rewind after bounded resume smoke");
    drive
        .write_config(original_config)
        .expect("restore tape config");
    let _ = std::fs::remove_file(journal_path);
}
