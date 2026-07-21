//! Layer 3c Step 11.18 — QuadStor VTL parity smoke tests.
//!
//! `#[ignore]`-gated by default. Runs only when env vars point
//! at a QuadStor VTL drive that's safe to write to.
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
//! scratch QuadStor cartridge.
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
use remanence_parity::bootstrap::{parse_bootstrap_block, write_bootstrap_block};
use remanence_parity::{
    emit_resume_rebuilt_sidecars_to_raw as emit_resume_sidecars_journaled,
    plan_resume_append_from_journal, rebuild_open_epoch_from_committed_prefix,
    scan_reconstruct_filemark_map, BootstrapPayload, CapacityReserveInput, CommittedBundle,
    CommittedBundleKind, CommittedState, DriveHandleRawSink, DriveHandleRawSource,
    FileTapeFileJournal, FilemarkMap, JournalError, ObjectParitySource, OpenTrust, ParityError,
    ParityScheme, ParitySchemeRecord, ParitySink, PhysicalPositionHint, RawReadOutcome,
    RawTapeSink, RawTapeSource, ResumeWriterSeed, SchemeId, ScopedFilemarkMap,
    SidecarEpochDirectoryEntry, SpaceFilemarksOutcome, TapeFileJournal, TapeFileKind,
    TapeFileMapEntry, TapeFilePosition, DEFAULT_SCHEME_BLOCK_SIZE_BYTES,
    SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
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

fn emit_resume_sidecars_with_fixture_journal<F>(
    sink: &mut dyn RawTapeSink,
    plan: remanence_parity::ResumeAppendPlan,
    rebuilt_sidecars: &[remanence_parity::ResumeRebuiltSidecar],
    expected_tape_uuid: [u8; 16],
    commit_sidecar: F,
) -> Result<remanence_parity::ResumeAppendResult, ParityError>
where
    F: FnMut(&remanence_parity::SidecarTapeFile) -> Result<(), ParityError>,
{
    let mut journal = FixtureJournal::default();
    emit_resume_sidecars_journaled(
        sink,
        &mut journal,
        plan,
        rebuilt_sidecars,
        expected_tape_uuid,
        commit_sidecar,
    )
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

fn capacity_input_for_object(
    block_size: u32,
    projected_object_blocks: u64,
    current_epoch_fill_blocks: u64,
) -> CapacityReserveInput {
    CapacityReserveInput {
        projected_object_blocks,
        block_size_bytes: u64::from(block_size),
        current_epoch_fill_blocks,
        data_shards_per_epoch: 2,
        parity_shards_per_epoch: 1,
        sidecar_index_block_count: 1,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        bootstrap_filemark_blocks: 1,
        pending_completed_sidecars: 0,
        remaining_bootstrap_count: 1,
        safety_margin_blocks: 8,
        remaining_tape_blocks: 10_000,
        empty_tape_usable_blocks: 10_000,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: u64::from(block_size) * 16,
    }
}

fn capacity_input(block_size: u32) -> CapacityReserveInput {
    capacity_input_for_object(block_size, 2, 0)
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

fn bootstrap_block_for_map(
    map: &FilemarkMap,
    scheme: &ParityScheme,
    block_size: u32,
    sequence: u32,
    is_final_map: bool,
) -> Vec<u8> {
    let payload = BootstrapPayload {
        scheme: Some(ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(map.digest(is_final_map).expect("map digest builds")),
        tape_uuid: TAPE_UUID,
        written_by_version: env!("CARGO_PKG_VERSION").to_string(),
        written_at: String::new(),
        sequence,
        block_size_bytes: block_size,
        drive_compression: false,
        sidecar_epoch_directory: None,
        parity_map_reference: None,
        object_rows: Vec::new(),
    };
    let mut out = vec![0u8; block_size as usize];
    write_bootstrap_block(&payload, &mut out).expect("bootstrap block encodes");
    out
}

fn write_raw_block(sink: &mut dyn RawTapeSink, block: &[u8], label: &str) {
    let outcome = sink
        .write_fixed_block(block)
        .unwrap_or_else(|err| panic!("{label}: {err}"));
    assert!(
        !outcome.end_of_medium(),
        "{label}: unexpected end-of-medium on QuadStor scratch tape"
    );
}

fn write_raw_filemark(sink: &mut dyn RawTapeSink, label: &str) {
    let outcome = sink
        .write_filemarks(1, false)
        .unwrap_or_else(|err| panic!("{label}: {err}"));
    assert!(
        !outcome.end_of_medium(),
        "{label}: unexpected end-of-medium on QuadStor scratch tape"
    );
}

fn write_object_only_prefix(
    sink: &mut dyn RawTapeSink,
    scheme: &ParityScheme,
    block_size: u32,
    object_blocks: &[Vec<u8>],
) -> FilemarkMap {
    let bot_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT map validates");
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(
            1,
            u64::try_from(object_blocks.len()).expect("object block count fits u64"),
            0,
        ),
    ])
    .expect("object-only prefix validates");

    let bootstrap = bootstrap_block_for_map(&bot_prefix, scheme, block_size, 0, false);
    write_raw_block(sink, &bootstrap, "write BOT bootstrap block");
    write_raw_filemark(sink, "write BOT bootstrap filemark");
    for (index, block) in object_blocks.iter().enumerate() {
        assert_eq!(
            block.len(),
            block_size as usize,
            "object-only prefix block {index} length mismatch"
        );
        write_raw_block(
            sink,
            block,
            &format!("write object-only prefix block {index}"),
        );
    }
    write_raw_filemark(sink, "write object-only prefix filemark");

    committed_prefix
}

fn final_bootstrap_scope(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    block_size: u32,
) -> ScopedFilemarkMap {
    let final_entry = map.entries().last().expect("map has final bootstrap");
    assert_eq!(final_entry.kind, TapeFileKind::Bootstrap);
    let final_position = map
        .physical_position(TapeFilePosition {
            tape_file_number: final_entry.tape_file_number,
            block_within_file: 0,
        })
        .expect("final bootstrap position resolves");
    source
        .locate_physical(final_position)
        .expect("locate final bootstrap");
    let mut buf = vec![0u8; block_size as usize];
    match source
        .read_record(&mut buf)
        .expect("read final bootstrap block")
    {
        RawReadOutcome::Block { bytes, .. } => {
            assert_eq!(bytes, block_size as usize);
        }
        other => panic!("expected final bootstrap block, got {other:?}"),
    }
    let payload = parse_bootstrap_block(&buf).expect("final bootstrap parses");
    let digest = payload
        .filemark_map_digest
        .as_ref()
        .expect("final bootstrap carries filemark-map digest");
    assert!(digest.is_final_map);
    ScopedFilemarkMap::validate_against_digest(map.clone(), digest)
        .expect("final bootstrap validates scanned map")
}

fn bootstrap_count(map: &FilemarkMap) -> u32 {
    u32::try_from(
        map.entries()
            .iter()
            .filter(|entry| entry.kind == TapeFileKind::Bootstrap)
            .count(),
    )
    .expect("bootstrap count fits u32")
}

fn committed_prefix_sidecar_directory_entries(
    map: &FilemarkMap,
) -> Vec<SidecarEpochDirectoryEntry> {
    map.entries()
        .iter()
        .filter(|entry| entry.kind == TapeFileKind::ParitySidecar)
        .map(|entry| SidecarEpochDirectoryEntry {
            tape_file_number: entry.tape_file_number,
            epoch_id: entry.epoch_id.expect("test sidecar has epoch id"),
            protected_ordinal_start: entry
                .protected_ordinal_start
                .expect("test sidecar has start ordinal"),
            protected_ordinal_end_exclusive: entry
                .protected_ordinal_end_exclusive
                .expect("test sidecar has end ordinal"),
            sidecar_total_block_count: entry.block_count,
            sidecar_header_block_count: 1,
            parity_shard_block_count: 1,
            canonical_metadata_hash: [entry.tape_file_number as u8; 32],
            flags: SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
        })
        .collect()
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
        tape_file_number,
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
        .expect("open selected QuadStor library for state-changing parity smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected QuadStor drive");
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
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(block_size))
                .expect("object reserve")
                .0,
            1
        );
        sink.write_block(&first_block)
            .expect("write object block 0");
        sink.write_block(&second_block)
            .expect("write object block 1");
        let object = sink.finish_object().expect("finish object");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        sink.finish().expect("finish final bootstrap");
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
                TapeFileKind::Bootstrap,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 2);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 2);

        let scoped = final_bootstrap_scope(&mut raw_source, &scanned, block_size);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
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
        .expect("open selected QuadStor library for journaled parity smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected QuadStor drive");
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
            assert_eq!(
                sink.begin_object_with_capacity_reserve(capacity_input(block_size))
                    .expect("object reserve")
                    .0,
                1
            );
            sink.write_block(&first_block)
                .expect("write journaled object block 0");
            sink.write_block(&second_block)
                .expect("write journaled object block 1");
            let object = sink.finish_object().expect("finish journaled object");
            assert_eq!(object.tape_file_number, 1);
            assert_eq!(object.sidecars_emitted.len(), 1);
            assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
            let checkpoint = sink.checkpoint().expect("checkpoint journaled session");
            assert_eq!(checkpoint.bootstrap_tape_file_number, 3);
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
                TapeFileKind::Bootstrap,
            ],
        );
    }

    let reopened = FileTapeFileJournal::open(&journal_path, TAPE_UUID, block_size, scheme.clone())
        .expect("reopen journal for resume planning");
    let plan = plan_resume_append_from_journal(&reopened, &scheme)
        .expect("plan resume append from journal");
    assert_eq!(plan.append_after_tape_file_number, 3);

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
        .expect("open selected QuadStor library for recovery smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected QuadStor drive");
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
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(block_size))
                .expect("object reserve")
                .0,
            1
        );
        sink.write_block(&first_block)
            .expect("write object block 0");
        sink.write_block(&second_block)
            .expect("write object block 1");
        let object = sink.finish_object().expect("finish object");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        sink.finish().expect("finish final bootstrap");
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
                TapeFileKind::Bootstrap,
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
        let scoped = final_bootstrap_scope(&mut drive_source, &scanned, block_size);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
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
fn quadstor_parity_resume_append_roundtrip() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_parity_resume_append_roundtrip: skipping — \
             REM_QUADSTOR_PARITY_WRITE_LOOP not set to 1. This test writes \
             and appends on the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let first_object = [block(0x31, block_size), block(0x32, block_size)];
    let second_object = [block(0x41, block_size), block(0x42, block_size)];
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    eprintln!(
        "quadstor_parity_resume_append_roundtrip: selected library {} bay 0x{bay_address:04x} at {path:?}",
        library.serial
    );

    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open selected QuadStor library for state-changing append smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected QuadStor drive");
    let original_config = configure_parity_write_session(&mut drive, block_size, "append smoke");
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
        .expect("construct first-session hardware parity sink");
        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(block_size))
                .expect("first object reserve")
                .0,
            1
        );
        for block in &first_object {
            sink.write_block(block).expect("write first-session block");
        }
        let object = sink.finish_object().expect("finish first object");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        sink.finish().expect("finish first final bootstrap");
    }

    drive.rewind().expect("rewind before first-session scan");
    let first_map = {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs first-session map");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 2);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 2);
        let scoped = final_bootstrap_scope(&mut raw_source, &scanned, block_size);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
        scanned
    };

    let resume = {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        rebuild_open_epoch_from_committed_prefix(
            &mut raw_source,
            &first_map,
            &scheme,
            TAPE_UUID,
            block_size,
        )
        .expect("resume positions to committed final-bootstrap append point")
    };
    assert!(resume.rebuilt_sidecars.is_empty());
    assert!(resume.live_epoch.is_none());
    assert_eq!(resume.plan.append_after_tape_file_number, 3);
    assert_eq!(
        resume.plan.append_position,
        first_map
            .append_position_after_prefix()
            .expect("first map append position")
    );
    let resume_result = resume
        .plan
        .clone()
        .complete(Vec::new())
        .expect("no sidecars are emitted for a clean finalized prefix");

    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            fixture_journal(),
            scheme.clone(),
            TAPE_UUID,
            block_size,
            ResumeWriterSeed {
                committed_prefix: &first_map,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&first_map),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: resume.live_epoch,
                next_bootstrap_sequence: bootstrap_count(&first_map),
            },
        )
        .expect("construct resumed parity sink at catalog append point");
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(block_size))
                .expect("second object reserve")
                .0,
            4
        );
        for block in &second_object {
            sink.write_block(block).expect("write resumed object block");
        }
        let object = sink.finish_object().expect("finish resumed object");
        assert_eq!(object.tape_file_number, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 5);
        sink.finish().expect("finish appended final bootstrap");
    }

    drive.rewind().expect("rewind before appended verification");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs appended hardware map");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 4);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 4);

        let scoped = final_bootstrap_scope(&mut raw_source, &scanned, block_size);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 4);
        read_object_blocks(
            &mut raw_source,
            scheme.clone(),
            scoped.clone(),
            block_size,
            1,
            &first_object,
        );
        read_object_blocks(
            &mut raw_source,
            scheme,
            scoped,
            block_size,
            4,
            &second_object,
        );
    }

    drive.rewind().expect("rewind after append verification");
    drive
        .write_config(original_config)
        .expect("restore original tape config after append smoke");
}

#[test]
#[ignore]
fn quadstor_parity_resume_rebuilds_open_epoch_then_appends() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_parity_resume_rebuilds_open_epoch_then_appends: skipping — \
             REM_QUADSTOR_PARITY_WRITE_LOOP not set to 1. This test writes \
             and appends on the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let prefix_object = [
        block(0x61, block_size),
        block(0x62, block_size),
        block(0x63, block_size),
    ];
    let continued_object = [block(0x64, block_size)];
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    eprintln!(
        "quadstor_parity_resume_rebuilds_open_epoch_then_appends: selected library {} bay 0x{bay_address:04x} at {path:?}",
        library.serial
    );

    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open selected QuadStor library for W<T resume smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected QuadStor drive");
    let original_config =
        configure_parity_write_session(&mut drive, block_size, "W<T resume smoke");
    drive
        .rewind()
        .expect("rewind before destructive object-only prefix write");

    let committed_prefix = {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        write_object_only_prefix(&mut raw_sink, &scheme, block_size, &prefix_object)
    };

    drive
        .rewind()
        .expect("rewind before object-only prefix scan");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs object-only prefix");
        assert_eq!(scanned, committed_prefix);
        assert_map_kinds(&scanned, &[TapeFileKind::Bootstrap, TapeFileKind::Object]);
        assert_eq!(scanned.total_data_ordinals(), 3);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 0);
    }

    let resume = {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        rebuild_open_epoch_from_committed_prefix(
            &mut raw_source,
            &committed_prefix,
            &scheme,
            TAPE_UUID,
            block_size,
        )
        .expect("resume rereads W<T object blocks and positions to append point")
    };
    assert_eq!(resume.plan.append_after_tape_file_number, 1);
    assert_eq!(
        resume.plan.append_position,
        committed_prefix
            .append_position_after_prefix()
            .expect("object-only prefix append position")
    );
    assert_eq!(resume.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(resume.plan.highest_protected_ordinal_after_rebuild, 2);
    assert_eq!(resume.plan.live_epoch_start, 2);
    assert_eq!(resume.plan.next_data_ordinal, 3);
    assert_eq!(resume.rebuilt_sidecars.len(), 1);
    let live_epoch = resume.live_epoch.clone().expect("one shard remains live");
    assert_eq!(live_epoch.protected_ordinal_start, 2);
    assert_eq!(live_epoch.next_data_ordinal, 3);
    assert_eq!(live_epoch.data_blocks_in_epoch, 1);

    let resume_result = {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        emit_resume_sidecars_with_fixture_journal(
            &mut raw_sink,
            resume.plan.clone(),
            &resume.rebuilt_sidecars,
            TAPE_UUID,
            |_| Ok(()),
        )
        .expect("resume-generated sidecar writes through ordinary raw barrier")
    };
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    let resume_sidecar = &resume_result.sidecars_emitted[0];
    assert_eq!(resume_sidecar.tape_file_number, 2);
    assert_eq!(resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(resume_sidecar.protected_ordinal_end_exclusive, 2);
    assert_eq!(resume_result.highest_protected_ordinal, 2);
    assert_eq!(resume_result.next_data_ordinal, 3);

    let prefix_after_resume = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 3, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            resume_sidecar.block_count,
            resume_sidecar.epoch_id,
            resume_sidecar.protected_ordinal_start,
            resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-resume committed prefix validates");

    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            fixture_journal(),
            scheme.clone(),
            TAPE_UUID,
            block_size,
            ResumeWriterSeed {
                committed_prefix: &prefix_after_resume,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&prefix_after_resume),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: resume.live_epoch,
                next_bootstrap_sequence: 1,
            },
        )
        .expect("construct resumed W<T parity sink");
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_for_object(block_size, 1, 1))
                .expect("continued object reserve")
                .0,
            3
        );
        for block in &continued_object {
            sink.write_block(block)
                .expect("write continued live-epoch block");
        }
        let object = sink.finish_object().expect("finish continued object");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 3);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 2);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );
        sink.finish().expect("finish appended final bootstrap");
    }

    drive
        .rewind()
        .expect("rewind before W<T resume verification");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs W<T resumed hardware map");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 4);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 4);

        let scoped = final_bootstrap_scope(&mut raw_source, &scanned, block_size);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 4);
        read_object_blocks(
            &mut raw_source,
            scheme.clone(),
            scoped.clone(),
            block_size,
            1,
            &prefix_object,
        );
        read_object_blocks(
            &mut raw_source,
            scheme,
            scoped,
            block_size,
            3,
            &continued_object,
        );
    }

    drive
        .rewind()
        .expect("rewind after W<T resume verification");
    drive
        .write_config(original_config)
        .expect("restore original tape config after W<T resume smoke");
}

#[test]
#[ignore]
fn quadstor_parity_resume_rebuilds_multiple_open_epochs_then_appends() {
    let Some(path) = skip_if_no_hardware() else {
        return;
    };
    if !write_loop_enabled() {
        eprintln!(
            "quadstor_parity_resume_rebuilds_multiple_open_epochs_then_appends: skipping - \
             REM_QUADSTOR_PARITY_WRITE_LOOP not set to 1. This test writes \
             and appends on the loaded cartridge; only enable on a scratch tape."
        );
        return;
    }

    let block_size = block_size();
    let scheme = smoke_scheme();
    let prefix_object = [
        block(0x71, block_size),
        block(0x72, block_size),
        block(0x73, block_size),
        block(0x74, block_size),
        block(0x75, block_size),
    ];
    let continued_object = [block(0x76, block_size)];
    let (library, bay_address) = resolve_library_drive_for_path(&path);
    eprintln!(
        "quadstor_parity_resume_rebuilds_multiple_open_epochs_then_appends: selected library {} bay 0x{bay_address:04x} at {path:?}",
        library.serial
    );

    let mut policy = StaticAllowlist::new([library.serial.clone()]);
    if allow_derived_drive_identity() {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    let mut handle = library
        .open(&policy)
        .expect("open selected QuadStor library for multi-epoch W<T resume smoke");
    let mut drive = handle
        .open_drive(bay_address, &policy)
        .expect("open selected QuadStor drive");
    let original_config =
        configure_parity_write_session(&mut drive, block_size, "multi-epoch W<T resume smoke");
    drive
        .rewind()
        .expect("rewind before destructive multi-epoch object-only prefix write");

    let committed_prefix = {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        write_object_only_prefix(&mut raw_sink, &scheme, block_size, &prefix_object)
    };

    drive
        .rewind()
        .expect("rewind before multi-epoch object-only prefix scan");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs multi-epoch object-only prefix");
        assert_eq!(scanned, committed_prefix);
        assert_map_kinds(&scanned, &[TapeFileKind::Bootstrap, TapeFileKind::Object]);
        assert_eq!(scanned.total_data_ordinals(), 5);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 0);
    }

    let resume = {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        rebuild_open_epoch_from_committed_prefix(
            &mut raw_source,
            &committed_prefix,
            &scheme,
            TAPE_UUID,
            block_size,
        )
        .expect("resume rereads multi-epoch W<T object blocks and positions to append point")
    };
    assert_eq!(resume.plan.append_after_tape_file_number, 1);
    assert_eq!(
        resume.plan.append_position,
        committed_prefix
            .append_position_after_prefix()
            .expect("multi-epoch object-only prefix append position")
    );
    assert_eq!(resume.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(resume.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(resume.plan.live_epoch_start, 4);
    assert_eq!(resume.plan.next_data_ordinal, 5);
    assert_eq!(resume.rebuilt_sidecars.len(), 2);
    assert_eq!(resume.rebuilt_sidecars[0].plan.protected_ordinal_start, 0);
    assert_eq!(
        resume.rebuilt_sidecars[0]
            .plan
            .protected_ordinal_end_exclusive,
        2
    );
    assert_eq!(resume.rebuilt_sidecars[1].plan.protected_ordinal_start, 2);
    assert_eq!(
        resume.rebuilt_sidecars[1]
            .plan
            .protected_ordinal_end_exclusive,
        4
    );
    let live_epoch = resume.live_epoch.clone().expect("one shard remains live");
    assert_eq!(live_epoch.protected_ordinal_start, 4);
    assert_eq!(live_epoch.next_data_ordinal, 5);
    assert_eq!(live_epoch.data_blocks_in_epoch, 1);

    let resume_result = {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        emit_resume_sidecars_with_fixture_journal(
            &mut raw_sink,
            resume.plan.clone(),
            &resume.rebuilt_sidecars,
            TAPE_UUID,
            |_| Ok(()),
        )
        .expect("two resume-generated sidecars write through ordinary raw barriers")
    };
    assert_eq!(resume_result.sidecars_emitted.len(), 2);
    assert_eq!(resume_result.sidecars_emitted[0].tape_file_number, 2);
    assert_eq!(resume_result.sidecars_emitted[0].protected_ordinal_start, 0);
    assert_eq!(
        resume_result.sidecars_emitted[0].protected_ordinal_end_exclusive,
        2
    );
    assert_eq!(resume_result.sidecars_emitted[1].tape_file_number, 3);
    assert_eq!(resume_result.sidecars_emitted[1].protected_ordinal_start, 2);
    assert_eq!(
        resume_result.sidecars_emitted[1].protected_ordinal_end_exclusive,
        4
    );
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.next_data_ordinal, 5);

    let mut prefix_entries = vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 5, 0),
    ];
    for sidecar in &resume_result.sidecars_emitted {
        prefix_entries.push(TapeFileMapEntry::parity_sidecar(
            sidecar.tape_file_number,
            sidecar.block_count,
            sidecar.epoch_id,
            sidecar.protected_ordinal_start,
            sidecar.protected_ordinal_end_exclusive,
        ));
    }
    let prefix_after_resume =
        FilemarkMap::new(prefix_entries).expect("multi-sidecar post-resume prefix validates");

    {
        let mut raw_sink = DriveHandleRawSink::new(&mut drive);
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            fixture_journal(),
            scheme.clone(),
            TAPE_UUID,
            block_size,
            ResumeWriterSeed {
                committed_prefix: &prefix_after_resume,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&prefix_after_resume),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: resume.live_epoch,
                next_bootstrap_sequence: 1,
            },
        )
        .expect("construct resumed multi-epoch W<T parity sink");
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_for_object(block_size, 1, 1))
                .expect("continued object reserve")
                .0,
            4
        );
        for block in &continued_object {
            sink.write_block(block)
                .expect("write continued multi-epoch live block");
        }
        let object = sink
            .finish_object()
            .expect("finish continued multi-epoch object");
        assert_eq!(object.tape_file_number, 4);
        assert_eq!(object.first_parity_data_ordinal, 5);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 5);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            6
        );
        sink.finish().expect("finish appended final bootstrap");
    }

    drive
        .rewind()
        .expect("rewind before multi-epoch W<T resume verification");
    {
        let mut raw_source = DriveHandleRawSource::new(&mut drive);
        let scanned = scan_reconstruct_filemark_map(&mut raw_source, &TAPE_UUID, block_size)
            .expect("catalog-less scan reconstructs multi-epoch W<T resumed hardware map");
        assert_map_kinds(
            &scanned,
            &[
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
        );
        assert_eq!(scanned.total_data_ordinals(), 6);
        assert_eq!(scanned.max_sidecar_end_exclusive(), 6);

        let scoped = final_bootstrap_scope(&mut raw_source, &scanned, block_size);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 6);
        read_object_blocks(
            &mut raw_source,
            scheme.clone(),
            scoped.clone(),
            block_size,
            1,
            &prefix_object,
        );
        read_object_blocks(
            &mut raw_source,
            scheme,
            scoped,
            block_size,
            4,
            &continued_object,
        );
    }

    drive
        .rewind()
        .expect("rewind after multi-epoch W<T resume verification");
    drive
        .write_config(original_config)
        .expect("restore original tape config after multi-epoch W<T resume smoke");
}
