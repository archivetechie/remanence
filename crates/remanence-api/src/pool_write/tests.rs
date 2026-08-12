use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use remanence_aead::{RecipientPrivateKey, RecipientPublicKey};
use remanence_chaos::model::{ModelTransport, Record, VirtualTape, VirtualWorld};
use remanence_format::{RemTarObjectOptions, FORMAT_ID};
use remanence_library::{
    BlockSink, PipelinedWriteDiagnostics, TapeConfig, TapeIoError, TapePosition, VecBlockSink,
    WormMediaState, WriteBatchOutcome, WriteFilemarksOutcome, WriteOutcome,
};
use remanence_parity::{
    bootstrap::write_bootstrap_block, BlockSinkRawTapeSink, BootstrapPayload, CapacityReserveCause,
    CommittedBundle, CommittedBundleKind, FileTapeFileJournal, ParityConfig, ParityError,
    ParityScheme, ParitySink, RawTapeSink, SchemeId, TapeFileEntry, TapeFileJournal, TapeFileKind,
    TerminalTripleCapacityRuntimeState,
};
use remanence_state::{
    watermark_floor_bytes, CatalogIndex, StateError, StateHandle, TapePoolConfig, TapeRecord,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::pb;
use crate::pool_selection::{FillOldest, PoolSelectionContext, PoolSelectionPolicy, Selection};

use super::*;
use super::{capacity::*, media::*, no_parity::*, overlap::*, prepare::*, staging::*};

#[test]
fn rewritable_object_media_gate_fails_closed_for_worm_and_unknown() {
    let config = |write_protected, worm| TapeConfig {
        block_size: remanence_library::BlockSize::Variable,
        compression: false,
        max_block_size_bytes: 8 * 1024 * 1024,
        write_protected,
        worm,
    };

    require_rewritable_object_media(config(false, WormMediaState::NotWorm))
        .expect("positive rewritable evidence is admitted");
    assert_eq!(
        require_rewritable_object_media(config(false, WormMediaState::Worm)),
        Err(ObjectWriteMediaError::Worm)
    );
    assert_eq!(
        require_rewritable_object_media(config(false, WormMediaState::Unknown)),
        Err(ObjectWriteMediaError::UnknownWormState)
    );
    assert_eq!(
        require_rewritable_object_media(config(true, WormMediaState::NotWorm)),
        Err(ObjectWriteMediaError::WriteProtected)
    );
}

#[test]
fn direct_write_resources_share_identity_admission_authority() {
    let first = test_pool_write_resources();
    let second = test_pool_write_resources();
    let object_id = [0x5A; 16];
    let held = first
        .write_admissions
        .reserve("direct-shared", "same-caller", Some(object_id))
        .expect("first direct writer owns both identities");

    let replay_conflict = second
        .write_admissions
        .reserve("direct-shared", "same-caller", None)
        .expect_err("a separately constructed resource must share replay-key authority");
    assert_eq!(replay_conflict.code(), tonic::Code::Aborted);
    let uuid_conflict = second
        .write_admissions
        .reserve("other-pool", "other-caller", Some(object_id))
        .expect_err("canonical Object UUID authority is process-wide");
    assert_eq!(uuid_conflict.code(), tonic::Code::Aborted);

    drop(held);
    second
        .write_admissions
        .reserve("direct-shared", "same-caller", Some(object_id))
        .expect("a non-durable claim releases when its owner drops");
}

#[test]
fn uncertain_checkpoint_append_quarantines_direct_identity_until_restart() {
    let resources = test_pool_write_resources();
    let object_id = [0x5B; 16];
    let mut held = resources
        .write_admissions
        .reserve("direct-uncertain", "uncertain-caller", Some(object_id))
        .expect("reserve direct identity");
    let error = StateError::CheckpointAppendAuthorityUncertain(
        "injected append and rollback failure".to_string(),
    );
    quarantine_direct_admission_on_uncertain_append(&error, Some(&mut held));
    drop(held);

    let conflict = resources
        .write_admissions
        .reserve("direct-uncertain", "uncertain-caller", Some(object_id))
        .expect_err("uncertain durable authority must remain quarantined");
    assert_eq!(conflict.code(), tonic::Code::Aborted);
}

#[test]
fn selected_drive_writer_binds_media_check_and_write_to_one_drive() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    const TAPE_UUID: TapeUuid = [0x57; 16];
    const LIBRARY_SERIAL: &str = "LIB-DIRECT-WORM";
    const DRIVE_BAY: u16 = 0x0100;

    let bootstrap = BootstrapPayload {
        scheme: None,
        no_parity_flag: true,
        filemark_map_digest: None,
        tape_uuid: TAPE_UUID,
        written_by_version: "test".to_string(),
        written_at: "2026-08-11T00:00:00Z".to_string(),
        sequence: 0,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
    };
    let mut bootstrap_block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK_SIZE);
    tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK_SIZE);
    tape.worm = true;

    let mut world =
        VirtualWorld::single_drive(LIBRARY_SERIAL, DRIVE_BAY, "DRV-DIRECT-WORM", 0x0400, 1);
    world.put_tape_in_drive(DRIVE_BAY, "WORM002L9", Some(0x0400), tape);
    let world = Arc::new(Mutex::new(world));
    let library = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([LIBRARY_SERIAL]);
    let transport_world = Arc::clone(&world);
    let mut library = library
        .open_with(&policy, move |path| {
            let role = transport_world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            Ok::<_, remanence_library::IoErrorKind>(Box::new(ModelTransport::new(
                Arc::clone(&transport_world),
                role,
            ))
                as Box<dyn remanence_library::SgTransport>)
        })
        .expect("open model library");
    let mut drive = library
        .open_drive(DRIVE_BAY, &policy)
        .expect("open model drive");

    let temp = tempfile::tempdir().expect("tempdir");
    let mut state =
        CatalogIndex::open(temp.path().join("state.sqlite")).expect("open empty test catalog");
    let selected = SelectedTape {
        pool_id: "direct-worm".to_string(),
        tape_uuid: TAPE_UUID,
        block_size: BLOCK_SIZE,
        parity_config: ParityConfig::None,
    };
    state
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: selected.pool_id.clone(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: selected.tape_uuid,
            voltag: "WORM002L9".to_string(),
            block_size: selected.block_size,
            parity: selected.parity_config.clone(),
            force: false,
        })
        .expect("provision selected tape");
    state
        .project_tape_pool_membership(selected.tape_uuid, &selected.pool_id)
        .expect("assign selected tape to pool");
    let pool_cfg = test_capacity_pool_config(&selected);
    let request = WriteObjectToPoolRequest {
        pool_id: selected.pool_id.clone(),
        source: WriteObjectSource::Path(temp.path().join("unreached-source")),
        archive_path: "unreached.bin".into(),
        caller_object_id: "direct-worm-check".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let resources = test_pool_write_resources();

    let mut cross_pool_cfg = pool_cfg.clone();
    cross_pool_cfg.id = "forged-pool".to_string();
    let mut cross_pool_selected = selected.clone();
    cross_pool_selected.pool_id = cross_pool_cfg.id.clone();
    let cross_pool_request = WriteObjectToPoolRequest {
        pool_id: cross_pool_cfg.id.clone(),
        source: WriteObjectSource::Path(temp.path().join("unreached-cross-pool-source")),
        archive_path: "unreached-cross-pool.bin".into(),
        caller_object_id: "direct-cross-pool-check".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let command_start = world.lock().expect("world lock").command_log.len();
    let cross_pool = write_to_selected_drive_checkpointed_with_catalog(
        &mut state,
        &mut drive,
        &cross_pool_cfg,
        cross_pool_request,
        cross_pool_selected,
        &temp.path().join("checkpoints"),
        &temp.path().join("parity.cbor"),
        &resources,
    )
    .expect_err("catalog pool membership must override a forged selection");
    assert!(matches!(cross_pool, PoolWriteError::InvalidInput(_)));
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        command_start,
        "cross-pool selection must fail before drive preparation"
    );

    let mut wrong_parity_selected = selected.clone();
    wrong_parity_selected.parity_config =
        ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(BLOCK_SIZE));
    let wrong_parity_request = WriteObjectToPoolRequest {
        pool_id: selected.pool_id.clone(),
        source: WriteObjectSource::Path(temp.path().join("unreached-parity-source")),
        archive_path: "unreached-parity.bin".into(),
        caller_object_id: "direct-parity-check".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let command_start = world.lock().expect("world lock").command_log.len();
    let wrong_parity = write_to_selected_drive_checkpointed_with_catalog(
        &mut state,
        &mut drive,
        &pool_cfg,
        wrong_parity_request,
        wrong_parity_selected,
        &temp.path().join("checkpoints"),
        &temp.path().join("parity.cbor"),
        &resources,
    )
    .expect_err("catalog parity geometry must override a forged selection");
    assert!(matches!(
        wrong_parity,
        PoolWriteError::MissingTapeGeometry(_)
    ));
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        command_start,
        "parity mismatch must fail before drive preparation"
    );

    let invalid_request = WriteObjectToPoolRequest {
        pool_id: "wrong-pool".to_string(),
        source: WriteObjectSource::Path(temp.path().join("unreached-invalid-source")),
        archive_path: "unreached-invalid.bin".into(),
        caller_object_id: "direct-invalid-check".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let command_start = world.lock().expect("world lock").command_log.len();
    let invalid = write_to_selected_drive_checkpointed_with_catalog(
        &mut state,
        &mut drive,
        &pool_cfg,
        invalid_request,
        selected.clone(),
        &temp.path().join("checkpoints"),
        &temp.path().join("parity.cbor"),
        &resources,
    )
    .expect_err("request validation must precede drive preparation");
    assert!(matches!(invalid, PoolWriteError::InvalidInput(_)));
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        command_start,
        "invalid requests must not issue drive commands"
    );

    let command_start = world.lock().expect("world lock").command_log.len();

    let error = write_to_selected_drive_checkpointed_with_catalog(
        &mut state,
        &mut drive,
        &pool_cfg,
        request,
        selected.clone(),
        &temp.path().join("checkpoints"),
        &temp.path().join("parity.cbor"),
        &resources,
    )
    .expect_err("the drive's WORM report must refuse the write");
    assert!(
        matches!(
            error,
            PoolWriteError::ObjectWriteMedia(ObjectWriteMediaError::Worm)
        ),
        "{error:?}"
    );

    let opcodes = world.lock().expect("world lock").command_log[command_start..]
        .iter()
        .map(|command| command.opcode)
        .collect::<Vec<_>>();
    assert!(
        opcodes.contains(&0x01) && opcodes.contains(&0x08),
        "the same drive must rewind and read the selected tape's BOT identity: {opcodes:02x?}"
    );
    assert!(
        opcodes.contains(&0x1a),
        "the same drive must supply current MODE SENSE media state: {opcodes:02x?}"
    );
    for forbidden in [0x15, 0x0a, 0x10] {
        assert!(
            !opcodes.contains(&forbidden),
            "WORM refusal issued forbidden opcode 0x{forbidden:02x}: {opcodes:02x?}"
        );
    }

    world
        .lock()
        .expect("world lock")
        .tapes
        .get_mut("WORM002L9")
        .expect("loaded model tape")
        .worm = false;
    let source_path = temp.path().join("rewritable-source.bin");
    std::fs::write(&source_path, b"drive-bound write succeeds\n").expect("write test source");
    let request = WriteObjectToPoolRequest {
        pool_id: selected.pool_id.clone(),
        source: WriteObjectSource::Path(source_path.clone()),
        archive_path: "rewritable.bin".into(),
        caller_object_id: "direct-rewritable-check".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let command_start = world.lock().expect("world lock").command_log.len();

    let result = write_to_selected_drive_checkpointed_with_catalog(
        &mut state,
        &mut drive,
        &pool_cfg,
        request,
        selected,
        &temp.path().join("checkpoints"),
        &temp.path().join("parity.cbor"),
        &resources,
    )
    .expect("positive rewritable evidence permits the same drive to write");
    assert_eq!(result.object.copies.len(), 1);

    let opcodes = world.lock().expect("world lock").command_log[command_start..]
        .iter()
        .map(|command| command.opcode)
        .collect::<Vec<_>>();
    let mode_senses = opcodes
        .iter()
        .enumerate()
        .filter_map(|(index, opcode)| (*opcode == 0x1a).then_some(index))
        .collect::<Vec<_>>();
    let mode_sense = *mode_senses
        .first()
        .expect("MODE SENSE must precede admission");
    let mode_select = opcodes
        .iter()
        .position(|opcode| *opcode == 0x15)
        .expect("MODE SELECT must configure the admitted drive");
    let first_write = opcodes
        .iter()
        .position(|opcode| matches!(*opcode, 0x0a | 0x10))
        .expect("the admitted drive must receive media writes");
    let verified_mode_sense = mode_senses
        .iter()
        .copied()
        .find(|index| *index > mode_select)
        .expect("MODE SENSE must verify MODE SELECT");
    assert!(
            mode_sense < mode_select
                && mode_select < verified_mode_sense
                && verified_mode_sense < first_write,
            "drive sequence must be media check, MODE SELECT, verified MODE SENSE, then write: {opcodes:02x?}"
        );

    let replay_request = WriteObjectToPoolRequest {
        pool_id: pool_cfg.id.clone(),
        source: WriteObjectSource::Path(source_path),
        archive_path: "rewritable.bin".into(),
        caller_object_id: "direct-rewritable-check".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let command_start = world.lock().expect("world lock").command_log.len();
    let replay = write_to_selected_drive_checkpointed_with_catalog(
        &mut state,
        &mut drive,
        &pool_cfg,
        replay_request,
        SelectedTape {
            pool_id: pool_cfg.id.clone(),
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::None,
        },
        &temp.path().join("checkpoints"),
        &temp.path().join("parity.cbor"),
        &resources,
    )
    .expect("exact replay returns committed result");
    assert_eq!(replay.object.object_id, result.object.object_id);
    assert_eq!(
        world.lock().expect("world lock").command_log.len(),
        command_start,
        "an exact replay must return before drive preparation"
    );
}

fn test_pool_write_resources() -> PoolWriteResources {
    PoolWriteResources::new(remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES)
        .expect("test pool-write resources")
}

fn test_capacity_pool_config(selected: &SelectedTape) -> TapePoolConfig {
    TapePoolConfig {
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(selected.block_size),
        min_object_size_bytes: 0,
    }
}

/// Build a deterministic public recipient for encrypted pool-write tests.
fn test_recipient(epoch_byte: u8, slot_index: u8) -> RecipientPublicKey {
    RecipientPrivateKey::new(
        [epoch_byte; 16],
        format!("pool-write-{epoch_byte:02x}"),
        [epoch_byte.wrapping_add(1); 32],
    )
    .expect("test private recipient")
    .public_key(slot_index)
    .expect("test public recipient")
}

#[test]
fn canonical_plaintext_object_is_validated_and_written_verbatim() {
    const BLOCK_SIZE: usize = 4096;
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("multi.rem-object");
    let object_uuid = Uuid::from_bytes([0x41; 16]);
    let caller_object_id = "canonical-caller-41";
    let mut options = RemTarObjectOptions::new(
        object_uuid.to_string(),
        caller_object_id,
        "2026-08-11T09:30:00Z",
        Uuid::from_bytes([0x42; 16]).to_string(),
    );
    options.chunk_size = BLOCK_SIZE;
    let first = b"first canonical member\n".repeat(300);
    let second = b"second canonical member\n".repeat(500);
    let files = [
        remanence_format::RemTarFile {
            path: "tree/first.txt",
            file_id: "41414141-4141-4141-4141-414141414142",
            data: &first,
            mtime: None,
            executable: Some(false),
        },
        remanence_format::RemTarFile {
            path: "tree/second.txt",
            file_id: "41414141-4141-4141-4141-414141414143",
            data: &second,
            mtime: None,
            executable: Some(false),
        },
    ];
    let mut file_sink = remanence_library::FileBlockSink::create(&path, BLOCK_SIZE)
        .expect("create canonical object");
    let layout = remanence_format::write_rem_tar_object(&mut file_sink, &options, &files)
        .expect("build canonical object");
    file_sink.sync_all().expect("sync canonical object");
    drop(file_sink);
    let digest = sha256_file(&path).expect("hash canonical object");
    let request = WriteObjectToPoolRequest {
        pool_id: "canonical-pool".to_string(),
        source: WriteObjectSource::Path(path.clone()),
        archive_path: PathBuf::new(),
        caller_object_id: caller_object_id.to_string(),
        expected_content_sha256: Some(digest),
        expected_object_id: Some(*object_uuid.as_bytes()),
        input_kind: WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };

    let prepared = prepare_pool_object(&request, BLOCK_SIZE as u32)
        .expect("validate canonical object before tape motion");
    assert_eq!(prepared.object_uuid, object_uuid);
    assert_eq!(prepared.options.caller_object_id, caller_object_id);
    assert_eq!(prepared.files.len(), 2);
    assert_eq!(
        prepared.plan.layout.projected_size_blocks,
        layout.projected_size_blocks
    );

    let mut written = VecBlockSink::new();
    assert_eq!(
        write_canonical_plaintext_blocks(&mut written, &prepared).expect("write canonical blocks"),
        digest
    );
    assert_eq!(written.blocks.concat(), std::fs::read(&path).unwrap());

    let noncanonical_path = temp.path().join("noncanonical-mode.rem-object");
    let mut noncanonical = std::fs::read(&path).expect("read canonical bytes");
    let header_offset = usize::try_from(layout.files[0].data_offset)
        .expect("header offset fits usize")
        .checked_sub(512)
        .expect("payload follows a ustar header");
    let header = &mut noncanonical[header_offset..header_offset + 512];
    header[100..108].copy_from_slice(b"0000777\0");
    header[148..156].fill(b' ');
    let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
    header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    std::fs::write(&noncanonical_path, noncanonical).expect("write noncanonical object");
    let noncanonical_request = WriteObjectToPoolRequest {
        pool_id: "canonical-pool".to_string(),
        source: WriteObjectSource::Path(noncanonical_path),
        archive_path: PathBuf::new(),
        caller_object_id: caller_object_id.to_string(),
        expected_content_sha256: None,
        expected_object_id: Some(*object_uuid.as_bytes()),
        input_kind: WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let error = match prepare_pool_object(&noncanonical_request, BLOCK_SIZE as u32) {
        Ok(_) => panic!("noncanonical ustar mode must fail exact regeneration"),
        Err(error) => error,
    };
    assert!(matches!(error, PoolWriteError::InvalidInput(_)), "{error}");
    assert!(error.to_string().contains("deterministic writer output"));

    let missing_identity_guard = WriteObjectToPoolRequest {
        pool_id: "canonical-pool".to_string(),
        source: WriteObjectSource::Path(path),
        archive_path: PathBuf::new(),
        caller_object_id: caller_object_id.to_string(),
        expected_content_sha256: Some(digest),
        expected_object_id: None,
        input_kind: WriteObjectInputKind::CanonicalPlaintextRemObject,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let error = match prepare_pool_object(&missing_identity_guard, BLOCK_SIZE as u32) {
        Ok(_) => panic!("canonical object identity guard is mandatory"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("requires expected_object_id"));
}

#[test]
fn canonical_member_ranges_share_one_spool_descriptor() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("many-members.bin");
    std::fs::write(&path, vec![0x5a; 4096]).expect("write shared spool");
    let source = Arc::new(Mutex::new(File::open(&path).expect("open shared spool")));
    let mut readers = (0..4096)
        .map(|offset| SharedFileRangeReader::new(Arc::clone(&source), (offset % 4096) as u64, 1))
        .collect::<Vec<_>>();
    assert_eq!(Arc::strong_count(&source), readers.len() + 1);
    assert!(
        readers
            .iter()
            .all(|reader| Arc::ptr_eq(&reader.source, &source)),
        "every member range must reference the one shared spool File"
    );
    for reader in &mut readers {
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .expect("read shared member range");
        assert_eq!(byte, [0x5a]);
    }
}

#[test]
fn cloned_pool_write_resources_share_one_atomic_budget() {
    let resources = PoolWriteResources::new(10).expect("shared resources");
    let clone = resources.clone();
    let held = resources
        .io_memory()
        .try_reserve(7)
        .expect("first clone reserves bytes");

    assert_eq!(
        clone.io_memory().try_reserve_with_available(4).unwrap_err(),
        3,
        "the second clone must observe the first clone's live grant"
    );
    drop(held);
    assert!(clone.io_memory().try_reserve_with_available(10).is_ok());
}

#[test]
fn pool_write_result_rejects_physical_used_byte_overflow() {
    assert!(matches!(
        checked_physical_used_bytes(u64::MAX, 2),
        Err(PoolWriteError::PhysicalUsedBytesOverflow {
            position_lba: u64::MAX,
            block_size: 2,
        })
    ));
    assert_eq!(checked_physical_used_bytes(u64::MAX, 1).unwrap(), u64::MAX);
}

#[derive(Debug, Default)]
struct LocateCountingSink {
    inner: VecBlockSink,
    locate_calls: u64,
}

impl BlockSink for LocateCountingSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.inner.write_block(buf)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.locate_calls = self.locate_calls.saturating_add(1);
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

#[derive(Debug, Default)]
struct MisdirectedFreshLocateSink {
    inner: VecBlockSink,
}

impl BlockSink for MisdirectedFreshLocateSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.inner.write_block(buf)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let mut position = self.inner.locate(lba)?;
        position.lba = 1;
        position.beginning_of_partition = false;
        Ok(position)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

#[derive(Debug)]
struct FailOnBlockWriteSink {
    inner: VecBlockSink,
    fail_on_write: u64,
    write_calls: u64,
}

impl FailOnBlockWriteSink {
    /// Fail one fixed-block command after earlier writes have completed.
    fn new(fail_on_write: u64) -> Self {
        Self {
            inner: VecBlockSink::new(),
            fail_on_write,
            write_calls: 0,
        }
    }
}

impl BlockSink for FailOnBlockWriteSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.write_calls = self.write_calls.saturating_add(1);
        if self.write_calls == self.fail_on_write {
            return Err(TapeIoError::OperationFailed(
                "injected parity raw write failure".to_string(),
            ));
        }
        self.inner.write_block(buf)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

/// Position-faithful sink for terminal-tail tests that does not retain the
/// two one-GiB separation extents in memory.
#[derive(Debug, Default)]
struct SparseBlockSink {
    next_lba: u64,
    eod_lba: u64,
    block_writes: u64,
    filemark_writes: u64,
}

impl BlockSink for SparseBlockSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.next_lba = self.next_lba.checked_add(1).ok_or_else(|| {
            TapeIoError::OperationFailed("sparse sink LBA overflows u64".to_string())
        })?;
        self.eod_lba = self.eod_lba.max(self.next_lba);
        self.block_writes = self.block_writes.checked_add(1).ok_or_else(|| {
            TapeIoError::OperationFailed("sparse sink write count overflows u64".to_string())
        })?;
        Ok(WriteOutcome::from_device_position(
            u32::try_from(buf.len()).map_err(|_| {
                TapeIoError::OperationFailed(
                    "sparse sink block length does not fit u32".to_string(),
                )
            })?,
            false,
            false,
            TapePosition {
                lba: self.next_lba,
                partition: 0,
                beginning_of_partition: false,
                end_of_partition: false,
                block_position_end_of_warning: false,
            },
        ))
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.next_lba = self.next_lba.checked_add(u64::from(count)).ok_or_else(|| {
            TapeIoError::OperationFailed("sparse sink LBA overflows u64".to_string())
        })?;
        self.eod_lba = self.eod_lba.max(self.next_lba);
        self.filemark_writes = self
            .filemark_writes
            .checked_add(u64::from(count))
            .ok_or_else(|| {
                TapeIoError::OperationFailed("sparse sink filemark count overflows u64".to_string())
            })?;
        Ok(WriteFilemarksOutcome::from_device_position(
            false,
            false,
            TapePosition {
                lba: self.next_lba,
                partition: 0,
                beginning_of_partition: self.next_lba == 0,
                end_of_partition: false,
                block_position_end_of_warning: false,
            },
        ))
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.next_lba = self.eod_lba;
        self.position()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.next_lba = lba;
        self.position()
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(TapePosition {
            lba: self.next_lba,
            partition: 0,
            beginning_of_partition: self.next_lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        })
    }
}

#[derive(Debug)]
struct RejectTerminalReplicaSink {
    inner: SparseBlockSink,
    tape_uuid: [u8; 16],
    terminal_replica_attempts: u64,
}

impl RejectTerminalReplicaSink {
    fn new(tape_uuid: [u8; 16]) -> Self {
        Self {
            inner: SparseBlockSink::default(),
            tape_uuid,
            terminal_replica_attempts: 0,
        }
    }
}

impl BlockSink for RejectTerminalReplicaSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if remanence_parity::parse_tape_index_replica_header(buf, &self.tape_uuid).is_ok() {
            self.terminal_replica_attempts = self
                .terminal_replica_attempts
                .checked_add(1)
                .ok_or_else(|| {
                    TapeIoError::OperationFailed(
                        "terminal replica attempt count overflows u64".to_string(),
                    )
                })?;
            return Err(TapeIoError::OperationFailed(
                "injected terminal replica failure".to_string(),
            ));
        }
        self.inner.write_block(buf)
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

#[derive(Debug)]
struct StagedTestSink {
    inner: VecBlockSink,
    batch_blocks: u32,
    fail_on_batch_call: Option<u64>,
    batch_error: Option<TapeIoError>,
    early_warning_on_batch_call: Option<u64>,
    fail_space_to_eod: bool,
    fail_position: bool,
    fail_filemark: bool,
    pending_deferred_audit: bool,
    audited_partial_sense: bool,
    batch_calls: u64,
    events: Vec<String>,
    ring_buffers: u32,
    cdbs: Vec<Vec<u8>>,
    alignments: Vec<usize>,
    diagnostic_ioctl_samples: u64,
    diagnostic_resets: u64,
    diagnostic_publications: u64,
    ordered_events: Arc<Mutex<Vec<String>>>,
    position_overrides: VecDeque<TapePosition>,
}

impl StagedTestSink {
    fn new(batch_blocks: u32) -> Self {
        assert!(batch_blocks > 1, "staged test must exercise batching");
        Self {
            inner: VecBlockSink::new(),
            batch_blocks,
            fail_on_batch_call: None,
            batch_error: None,
            early_warning_on_batch_call: None,
            fail_space_to_eod: false,
            fail_position: false,
            fail_filemark: false,
            pending_deferred_audit: false,
            audited_partial_sense: false,
            batch_calls: 0,
            events: Vec::new(),
            ring_buffers: remanence_library::DEFAULT_TAPE_IO_STAGING_RING_BUFFERS,
            cdbs: Vec::new(),
            alignments: Vec::new(),
            diagnostic_ioctl_samples: 0,
            diagnostic_resets: 0,
            diagnostic_publications: 0,
            ordered_events: Arc::new(Mutex::new(Vec::new())),
            position_overrides: VecDeque::new(),
        }
    }

    fn failing_on_batch(batch_blocks: u32, fail_on_batch_call: u64) -> Self {
        let mut sink = Self::new(batch_blocks);
        sink.fail_on_batch_call = Some(fail_on_batch_call);
        sink
    }

    fn with_ring(batch_blocks: u32, ring_buffers: u32) -> Self {
        let mut sink = Self::new(batch_blocks);
        sink.ring_buffers = ring_buffers;
        sink
    }
}

impl BlockSink for StagedTestSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.events.push(format!("write_block:{}", buf.len()));
        self.inner.write_block(buf)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let records = records_in_staged_batch(buf, block_size_bytes)
            .expect("test batch contains whole records");
        self.batch_calls = self.batch_calls.saturating_add(1);
        self.diagnostic_ioctl_samples = self.diagnostic_ioctl_samples.saturating_add(1);
        self.events.push(format!("write_batch:{records}"));
        if let Some(error) = self.batch_error.take() {
            return Err(error);
        }
        if self.fail_on_batch_call == Some(self.batch_calls) {
            return Err(TapeIoError::OperationFailed(format!(
                "injected sink failure on batch {}",
                self.batch_calls
            )));
        }
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        if self.early_warning_on_batch_call == Some(self.batch_calls) {
            Ok(WriteBatchOutcome::from_computed_position(
                outcome.records_written,
                outcome.bytes_written,
                true,
                false,
                outcome.position_after,
            ))
        } else {
            Ok(outcome)
        }
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        self.cdbs.push(cdb.to_vec());
        self.alignments
            .push((buf.as_ptr() as usize) % system_page_size());
        self.ordered_events
            .lock()
            .expect("ordered events")
            .push("classify".into());
        self.write_block_batch(buf, block_size_bytes)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.batch_blocks
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.ring_buffers
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        PipelinedWriteDiagnostics {
            ioctl_samples: self.diagnostic_ioctl_samples,
            ioctl_max_us: self.diagnostic_ioctl_samples.saturating_mul(1_000),
            ..PipelinedWriteDiagnostics::default()
        }
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.diagnostic_ioctl_samples = 0;
        self.diagnostic_resets = self.diagnostic_resets.saturating_add(1);
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.diagnostic_publications += 1;
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.events.push(format!(
            "intent:{command_count}:{bytes}:{first_records}:{last_records}"
        ));
    }

    fn finish_pipelined_write_window_success(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        _duration: Duration,
    ) {
        self.events.push(format!(
            "span_ok:{command_count}:{bytes}:{first_records}:{last_records}"
        ));
    }

    fn finish_pipelined_write_window_error(
        &mut self,
        _command_count: u32,
        _bytes: u64,
        _first_records: u32,
        _last_records: u32,
        error: &TapeIoError,
    ) {
        self.audited_partial_sense = matches!(
            error,
            TapeIoError::PartialBatchUncommittable { sense: Some(_), .. }
        );
        self.ordered_events
            .lock()
            .expect("ordered events")
            .push("audit".into());
        self.events.push("span_error".into());
    }

    fn flush_pending_pipeline_audit(&mut self) {
        if self.pending_deferred_audit {
            self.pending_deferred_audit = false;
            self.ordered_events
                .lock()
                .expect("ordered events")
                .push("audit".into());
        }
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.events.push(format!("filemark:{count}"));
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.events.push(format!("filemark_immediate:{count}"));
        self.inner.write_filemarks(count).map(|_| ())
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        if self.fail_filemark {
            self.pending_deferred_audit = true;
            return Err(TapeIoError::OperationFailed(
                "injected WRITE FILEMARKS failure".into(),
            ));
        }
        self.write_filemarks(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.events.push("space_eod".to_string());
        if self.fail_space_to_eod {
            return Err(TapeIoError::OperationFailed(
                "injected space-to-EOD failure".into(),
            ));
        }
        self.inner.space_to_end_of_data()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.events.push(format!("locate:{lba}"));
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.events.push("position".to_string());
        if self.fail_position {
            return Err(TapeIoError::OperationFailed(
                "injected READ POSITION failure".into(),
            ));
        }
        match self.position_overrides.pop_front() {
            Some(position) => Ok(position),
            None => self.inner.position(),
        }
    }
}

fn tape_position_with_warning(block_position_end_of_warning: bool) -> TapePosition {
    TapePosition {
        lba: 0,
        partition: 0,
        beginning_of_partition: false,
        end_of_partition: false,
        block_position_end_of_warning,
    }
}

#[derive(Debug)]
struct SingleBlockOutcomeSink {
    bytes_written: u32,
    early_warning: bool,
    end_of_medium: bool,
    writes: u64,
}

impl BlockSink for SingleBlockOutcomeSink {
    fn write_block(&mut self, _buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.writes = self.writes.saturating_add(1);
        Ok(WriteOutcome::from_computed_position(
            self.bytes_written,
            self.early_warning,
            self.end_of_medium,
            TapePosition {
                lba: self.writes,
                partition: 0,
                beginning_of_partition: false,
                end_of_partition: false,
                block_position_end_of_warning: self.early_warning,
            },
        ))
    }

    fn write_filemarks(&mut self, _count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        Err(TapeIoError::OperationFailed(
            "single-block test sink does not write filemarks".to_string(),
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(TapePosition {
            lba: self.writes,
            partition: 0,
            beginning_of_partition: self.writes == 0,
            end_of_partition: false,
            block_position_end_of_warning: self.early_warning,
        })
    }
}

#[test]
fn fixed_block_helper_rejects_short_bytes_and_hard_eom() {
    for (bytes_written, end_of_medium) in [(3, false), (4, true)] {
        let mut sink = SingleBlockOutcomeSink {
            bytes_written,
            early_warning: false,
            end_of_medium,
            writes: 0,
        };

        let error = write_fixed_blocks(&mut sink, 4, &[0xA5; 4])
            .expect_err("incomplete fixed block must fail");
        let message = error.to_string();
        assert!(
            message.contains("partial fixed batch uncommittable"),
            "{message}"
        );
        assert!(message.contains("requested_bytes=4"), "{message}");
        assert!(
            message.contains(&format!("written_bytes={bytes_written}")),
            "{message}"
        );
        assert!(
            message.contains(&format!("end_of_medium={end_of_medium}")),
            "{message}"
        );
    }
}

#[test]
fn fixed_block_helper_accepts_full_bytes_with_early_warning() {
    let mut sink = SingleBlockOutcomeSink {
        bytes_written: 4,
        early_warning: true,
        end_of_medium: false,
        writes: 0,
    };

    let blocks = write_fixed_blocks(&mut sink, 4, &[0x5A; 8])
        .expect("full fixed blocks remain successful at early warning");

    assert_eq!(blocks, 2);
    assert_eq!(sink.writes, 2);
}

#[tokio::test]
async fn overlap_first_block_gate_requires_high_prefill_then_position_proof() {
    let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).expect("ring");
    producer
        .push(&vec![0x11; crate::append_ring::APPEND_RING_SLAB_BYTES / 2])
        .await
        .expect("sub-high prefill");
    let mut sink = StagedTestSink::new(2);
    {
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control: Arc::clone(&control),
            expected_initial_lba: 0,
            expected_next_lba: 0,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        let error = gated
            .write_block(&[0u8; 4])
            .expect_err("sub-high ring must not reach tape");
        assert!(error.to_string().contains("first-block gate"), "{error}");
        let error = gated
            .space_to_end_of_data()
            .expect_err("sub-high ring must not position an append");
        assert!(error.to_string().contains("first-block gate"), "{error}");
    }
    assert!(
        sink.events.is_empty(),
        "no tape command may precede the high-water gate: {:?}",
        sink.events
    );

    producer
        .push(&vec![0x22; crate::append_ring::APPEND_RING_SLAB_BYTES / 2])
        .await
        .expect("reach high prefill");
    {
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control,
            expected_initial_lba: 0,
            expected_next_lba: 0,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        gated.write_block(&[0u8; 4]).expect("gated first block");
    }
    assert_eq!(sink.events, ["position", "write_block:4"]);
}

#[test]
fn no_parity_append_lba_accepts_tape_files_beyond_u32() {
    let append = NoParityAppendContext {
        tape_file_number: u64::from(u32::MAX) + 1,
        previous_total_committed_ordinals: 11,
        fresh_tape: false,
        expected_append_lba: None,
    };

    assert_eq!(append.expected_append_lba().unwrap(), 4_294_967_308);
}

#[tokio::test]
async fn overlap_append_gate_counts_bootstrap_and_committed_trailing_filemark() {
    let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).expect("ring");
    producer
        .push(&vec![0x21; crate::append_ring::APPEND_RING_SLAB_BYTES])
        .await
        .expect("reach high prefill");

    let object_blocks = 2u64;
    let append = NoParityAppendContext {
        tape_file_number: 2,
        previous_total_committed_ordinals: object_blocks,
        fresh_tape: false,
        expected_append_lba: None,
    };
    let expected = append.expected_append_lba().expect("expected append LBA");

    let mut sink = StagedTestSink::new(2);
    sink.write_block(&[0xb0; 4]).expect("bootstrap block");
    sink.write_filemarks(1).expect("bootstrap filemark");
    for _ in 0..object_blocks {
        sink.write_block(&[0x0b; 4])
            .expect("committed object block");
    }
    sink.write_filemarks(1)
        .expect("committed object trailing filemark");
    sink.inner.set_next_lba_for_test(0);
    sink.events.clear();

    {
        let mut gated = OverlapBlockSink {
            inner: &mut sink,
            control: Arc::clone(&control),
            expected_initial_lba: expected,
            expected_next_lba: expected,
            initial_position_proved: false,
            write_started: false,
            low_water_events: 0,
        };
        let observed = position_no_parity_append(&mut gated).expect("position and prove EOD");
        assert_eq!(expected, observed.lba);
        assert_eq!(observed.lba, 5, "block + filemark records define EOD");
    }
    assert_eq!(sink.events, ["space_eod"]);

    sink.inner.set_next_lba_for_test(0);
    sink.events.clear();
    let old_fencepost = expected.checked_sub(1).expect("nonzero append LBA");
    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control,
        expected_initial_lba: old_fencepost,
        expected_next_lba: old_fencepost,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    let error = position_no_parity_append(&mut gated)
        .expect_err("one-LBA fencepost must still fail closed");
    assert!(
        error.to_string().contains(&format!(
            "expected partition 0 lba {old_fencepost}, observed partition 0 lba {expected}"
        )),
        "{error}"
    );
    assert_eq!(sink.events, ["space_eod"]);
}

#[test]
fn batched_recovery_locates_to_journal_eod_and_overwrites_longer_physical_tail() {
    let mut sink = StagedTestSink::new(2);
    for seed in 0..7u8 {
        sink.write_block(&[seed; 4]).expect("seed physical tail");
    }
    sink.events.clear();

    position_no_parity_append_at_checkpoint(&mut sink, 3)
        .expect("journal EOD is authoritative despite longer physical tail");

    assert_eq!(sink.events, ["locate:3", "position"]);
    assert_eq!(sink.position().expect("position after locate").lba, 3);
    sink.write_block(&[0x99; 4])
        .expect("overwrite orphaned tail");
    assert_eq!(sink.position().expect("position after overwrite").lba, 4);
}

#[tokio::test]
async fn overlap_batched_recovery_uses_locate_instead_of_space_eod() {
    let capacity = 2 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, capacity).expect("ring");
    producer
        .push(&vec![0x21; crate::append_ring::APPEND_RING_SLAB_BYTES])
        .await
        .expect("reach high prefill");
    let mut sink = StagedTestSink::new(2);
    for seed in 0..7u8 {
        sink.write_block(&[seed; 4]).expect("seed physical tail");
    }
    sink.events.clear();

    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control,
        expected_initial_lba: 3,
        expected_next_lba: 3,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    position_no_parity_append_at_checkpoint(&mut gated, 3)
        .expect("overlap recovery locates to journal EOD");

    assert_eq!(sink.events, ["locate:3", "position"]);
}

#[test]
fn overlap_low_water_pause_refills_then_reproves_next_lba() {
    let capacity = 4 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, 8 * capacity)
            .expect("ring");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(producer.push(&vec![
            0x31;
            2 * crate::append_ring::APPEND_RING_SLAB_BYTES + 1
        ]))
        .expect("initial high-water fill");

    let mut sink = StagedTestSink::new(2);
    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control: Arc::clone(&control),
        expected_initial_lba: 0,
        expected_next_lba: 0,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    gated.write_block(&[0x41; 4]).expect("first block");

    let mut drained = vec![0u8; crate::append_ring::APPEND_RING_SLAB_BYTES + 1];
    consumer
        .read_exact(&mut drained)
        .expect("drain to low watermark");
    assert!(control.should_pause(), "ring must be at the low watermark");

    let refill = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("refill runtime");
        runtime.block_on(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            producer
                .push(&vec![0x52; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES])
                .await
                .expect("refill to high watermark");
            producer
        })
    });
    gated
        .write_block(&[0x42; 4])
        .expect("resume after fresh proof");
    let producer = refill.join().expect("refill thread");
    drop(producer);
    drop(gated);

    assert_eq!(
        sink.events,
        [
            "position",
            "write_block:4",
            "position",
            "position",
            "write_block:4",
        ],
        "resume must flush/prove, wait, then issue a fresh proof before WRITE"
    );
}

#[test]
fn overlap_resume_refuses_position_drift_before_the_next_write() {
    let capacity = 4 * crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(capacity).expect("manager");
    let (mut producer, mut consumer, control) =
        crate::append_ring::create_append_ring(&manager, capacity, 50, 25, 8 * capacity)
            .expect("ring");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime
        .block_on(producer.push(&vec![
            0x61;
            2 * crate::append_ring::APPEND_RING_SLAB_BYTES + 1
        ]))
        .expect("initial high-water fill");

    let position = |lba| TapePosition {
        lba,
        partition: 0,
        beginning_of_partition: lba == 0,
        end_of_partition: false,
        block_position_end_of_warning: false,
    };
    let mut sink = StagedTestSink::new(2);
    sink.position_overrides = [position(0), position(1), position(9)].into();
    let mut gated = OverlapBlockSink {
        inner: &mut sink,
        control: Arc::clone(&control),
        expected_initial_lba: 0,
        expected_next_lba: 0,
        initial_position_proved: false,
        write_started: false,
        low_water_events: 0,
    };
    gated.write_block(&[0x71; 4]).expect("first block");
    let mut drained = vec![0u8; crate::append_ring::APPEND_RING_SLAB_BYTES + 1];
    consumer.read_exact(&mut drained).expect("drain to low");

    let refill = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("refill runtime");
        runtime.block_on(async move {
            producer
                .push(&vec![0x72; 2 * crate::append_ring::APPEND_RING_SLAB_BYTES])
                .await
                .expect("refill to high");
        });
    });
    let error = gated
        .write_block(&[0x73; 4])
        .expect_err("drifted resume must fail closed");
    refill.join().expect("refill thread");
    drop(gated);
    assert!(error.to_string().contains("position drift"), "{error}");
    assert_eq!(
        sink.events,
        ["position", "write_block:4", "position", "position"],
        "no WRITE may follow a failed resume proof"
    );
}

#[test]
fn overlap_admission_plans_object_larger_than_legacy_64_gib_cap() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-large-overlap-admission-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6b; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "LARGE1L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "large.overlap".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
    let (_producer, consumer, control) = crate::append_ring::create_append_ring(
        &manager,
        ring_bytes,
        90,
        25,
        crate::APPEND_SPOOL_MAX_BYTES + 1,
    )
    .expect("ring");
    let digest = [0x42; 32];
    let request = WriteObjectToPoolRequest {
        pool_id: selected.pool_id.clone(),
        source: WriteObjectSource::Streamed(StreamedWriteSource::new(
            consumer,
            crate::APPEND_SPOOL_MAX_BYTES + 1,
            digest,
            control,
        )),
        archive_path: "large.bin".into(),
        caller_object_id: "overlap-larger-than-spool-cap".into(),
        expected_content_sha256: Some(digest),
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };

    let prepared = prepare_pool_object(&request, selected.block_size)
        .expect("large streamed source must be plannable without payload");
    assert_eq!(
        prepared_payload_bytes(&prepared),
        crate::APPEND_SPOOL_MAX_BYTES + 1
    );
    let stored =
        prepare_stored_object(&prepared, &request.representation).expect("plaintext stored plan");
    let footprint =
        stored_footprint_bytes(&stored, &prepared, selected.block_size).expect("large footprint");
    ensure_selected_tape_has_capacity(
        &state,
        &test_capacity_pool_config(&selected),
        &selected,
        footprint,
        None,
    )
    .expect("mocked LTO-9 admits object beyond the legacy spool cap");
}

#[test]
fn batched_capacity_uses_session_local_physical_extent() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-batched-capacity-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6c; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "BATCH1L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "batched.capacity".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let capacity = raw_capacity_bytes(LtoGen::Lto9);
    let provisional_used_lba = capacity / u64::from(block_size) - 1;
    let object_size = u64::from(block_size) * 2;

    ensure_selected_tape_has_capacity(
        &state,
        &test_capacity_pool_config(&selected),
        &selected,
        object_size,
        None,
    )
    .expect("empty SQLite projection alone would admit the object");
    let err = ensure_selected_tape_has_capacity(
        &state,
        &test_capacity_pool_config(&selected),
        &selected,
        object_size,
        Some(provisional_used_lba),
    )
    .expect_err("the provisional physical prefix consumes capacity");
    assert!(
        matches!(err, PoolWriteError::SelectedTapeInsufficientCapacity { .. }),
        "{err}"
    );
}

#[test]
fn no_parity_exact_close_capacity_rolls_before_whole_object_motion() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-no-parity-terminal-capacity-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x73; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "NCAP01L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "capacity.no-parity".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let pool_cfg = TapePoolConfig {
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let capacity_blocks = raw_capacity_bytes(LtoGen::Lto9) / u64::from(block_size);
    let high_watermark_blocks =
        watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_high).expect("high watermark");
    let context = BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number: 3,
            previous_total_committed_ordinals: 1,
            fresh_tape: false,
            expected_append_lba: Some(high_watermark_blocks),
        },
        position: BatchedAppendPosition::JournalEod(high_watermark_blocks),
        object_row_count: 1,
    };

    let error = ensure_no_parity_terminal_close_capacity(&state, &pool_cfg, &selected, &context, 1)
        .expect_err("the current prefix must close before any block of the Object moves");
    assert!(matches!(
        error,
        PoolWriteError::TerminalCloseRequired { .. }
    ));

    let fresh = BatchedNoParityAppendContext {
        append: NoParityAppendContext {
            tape_file_number: 1,
            previous_total_committed_ordinals: 0,
            fresh_tape: true,
            expected_append_lba: None,
        },
        position: BatchedAppendPosition::FreshTape,
        object_row_count: 0,
    };
    let error = ensure_no_parity_terminal_close_capacity(
        &state,
        &pool_cfg,
        &selected,
        &fresh,
        capacity_blocks,
    )
    .expect_err("an impossible whole Object must be rejected on fresh media");
    assert!(matches!(
        error,
        PoolWriteError::Parity(ParityError::ObjectTooLargeForEmptyTape { .. })
    ));
}

#[test]
fn downward_pool_cap_is_the_shared_selection_and_terminal_capacity_basis() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-downward-capacity-basis-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x74; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "DCAP01L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "capacity.downward".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let cap_bytes = 4 * 1024 * 1024 * 1024_u64;
    let pool_cfg = TapePoolConfig {
        capacity_cap_bytes: Some(cap_bytes),
        ..test_capacity_pool_config(&selected)
    };
    let expected_c = cap_bytes / u64::from(block_size);

    assert_eq!(
        parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capped terminal C"),
        expected_c
    );
    let tape = state
        .get_tape(&tape_uuid)
        .expect("query tape")
        .expect("tape");
    let fit = tape_fit_state_from_record(&tape, &pool_cfg, &selected.pool_id, 0)
        .expect("capped selection projection");
    assert_eq!(
        fit.usable_bytes,
        watermark_floor_bytes(cap_bytes, pool_cfg.watermark_high).expect("capped H")
    );
    ensure_selected_tape_has_capacity(&state, &pool_cfg, &selected, cap_bytes + 1, None)
        .expect_err("general admission must use the same capped C");

    let upward = TapePoolConfig {
        capacity_cap_bytes: Some(raw_capacity_bytes(LtoGen::Lto9)),
        ..pool_cfg
    };
    assert!(parity_capacity_basis_blocks(&state, &upward, &selected).is_err());
}

#[test]
fn close_only_authority_uses_capped_c_at_equality_and_below_low() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-close-only-capacity-authority-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x5c; 16];
    let block_size = 256 * 1024;
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP005L9".into(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "capacity.close-only".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::None,
    };
    let capacity_blocks = 1_000_000;
    let pool_cfg = TapePoolConfig {
        watermark_low: 0.1,
        watermark_high: 0.2,
        capacity_cap_bytes: Some(capacity_blocks * u64::from(block_size)),
        ..test_capacity_pool_config(&selected)
    };
    let counts = remanence_parity::TapeIndexReplicaCounts {
        structural_entry_count: 1,
        object_row_count: 0,
    };
    let replica = remanence_parity::checked_tape_index_replica_layout(block_size, counts)
        .expect("replica layout");
    let gap_records = remanence_parity::index_separation_records(
        block_size,
        remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
    )
    .expect("gap layout");
    let tail_charge = 3 * (replica.replica_record_count + 1) + 2 * (gap_records + 1);

    let equality_start = capacity_blocks - tail_charge - TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS;
    let equality_layout = remanence_parity::TerminalTailLayout::new(
        0,
        block_size,
        1,
        equality_start,
        replica.replica_record_count,
        gap_records,
    )
    .expect("equality tail layout");
    let equality = authorize_terminal_close_only_plan(
        &state,
        Some(&pool_cfg),
        &selected,
        equality_start,
        counts,
        equality_layout.expected_eod_lba,
    )
    .expect("C equality succeeds");
    assert_eq!(
        equality.required_tape_blocks,
        capacity_blocks - equality_start
    );

    let one_short_cfg = TapePoolConfig {
        capacity_cap_bytes: Some((capacity_blocks - 1) * u64::from(block_size)),
        ..pool_cfg.clone()
    };
    let one_short = authorize_terminal_close_only_plan(
        &state,
        Some(&one_short_cfg),
        &selected,
        equality_start,
        counts,
        equality_layout.expected_eod_lba,
    )
    .expect_err("one block below the exact capped C must fail");
    assert!(
        matches!(
            one_short,
            PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::TapeCapacity,
                ..
            })
        ),
        "{one_short}"
    );

    let below_low_start = 100;
    assert!(
        below_low_start
            < watermark_floor_bytes(capacity_blocks, pool_cfg.watermark_low)
                .expect("low watermark")
    );
    let below_low_layout = remanence_parity::TerminalTailLayout::new(
        0,
        block_size,
        1,
        below_low_start,
        replica.replica_record_count,
        gap_records,
    )
    .expect("below-low tail layout");
    authorize_terminal_close_only_plan(
        &state,
        Some(&pool_cfg),
        &selected,
        below_low_start,
        counts,
        below_low_layout.expected_eod_lba,
    )
    .expect("manual close below L uses the same exact authority");

    let unpooled_capacity_blocks = raw_capacity_bytes(LtoGen::Lto9) / u64::from(block_size);
    assert_eq!(
        terminal_watermark_blocks(unpooled_capacity_blocks, None)
            .expect("unpooled terminal watermarks"),
        (0, 1)
    );
    let unpooled_equality_start =
        unpooled_capacity_blocks - tail_charge - TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS;
    let unpooled_equality_layout = remanence_parity::TerminalTailLayout::new(
        0,
        block_size,
        1,
        unpooled_equality_start,
        replica.replica_record_count,
        gap_records,
    )
    .expect("unpooled equality tail layout");
    let unpooled = authorize_terminal_close_only_plan(
        &state,
        None,
        &selected,
        unpooled_equality_start,
        counts,
        unpooled_equality_layout.expected_eod_lba,
    )
    .expect("unpooled close uses raw C with the canonical L/H basis");
    assert_eq!(
        unpooled.required_tape_blocks,
        unpooled_capacity_blocks - unpooled_equality_start
    );
}

#[test]
fn parity_capacity_reservation_uses_physical_cursor_exact_layout_and_atomic_spool_grant() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-capacity-runtime-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6d; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-runtime-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP001L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "capacity.runtime".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let capacity_blocks =
        parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capacity basis");
    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity =
        ParitySink::new_with_journal(&mut raw, &mut journal, scheme, tape_uuid, block_size)
            .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    assert_eq!(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state")
            .used_tape_blocks,
        2,
        "physical cursor includes the bootstrap body and trailing filemark"
    );

    let io_memory =
        crate::io_memory::IoMemoryReservation::new(1024 * 1024).expect("spool reservation manager");
    let mismatch = reserve_parity_object_capacity(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state"),
        parity.scheme(),
        &selected,
        (&pool_cfg, 0, 0),
        capacity_blocks,
        2,
        &io_memory,
    )
    .expect_err("journal authority cannot omit the live BOT structural row");
    assert!(
        mismatch
            .to_string()
            .contains("journal/sink authority mismatch"),
        "{mismatch}"
    );
    assert_eq!(io_memory.granted(), 0);

    let reservation = reserve_parity_object_capacity(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state"),
        parity.scheme(),
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &io_memory,
    )
    .expect("exact reservation");
    let report = *reservation.report();
    assert!(report.projected_object_present);
    assert_eq!(report.object_tape_file_blocks, 3);
    assert_eq!(
        report.safety_margin_blocks,
        TERMINAL_CAPACITY_SAFETY_MARGIN_BLOCKS
    );
    assert!(report.final_parity_map_tape_file_blocks > 0);
    assert_eq!(io_memory.granted(), report.required_spool_bytes);

    let (reservation, spool_permit) = reservation.into_parts();
    parity
        .begin_object_with_terminal_triple_reservation(reservation)
        .expect("begin after atomic reservation");
    parity
        .write_block(&vec![0xA5; block_size as usize])
        .expect("first Object block");
    parity
        .write_block(&vec![0x5A; block_size as usize])
        .expect("second Object block");
    assert_eq!(
        io_memory.granted(),
        report.required_spool_bytes,
        "spool grant remains held while completed parity is staged"
    );
    parity.finish_object().expect("emit reserved sidecar");
    assert_eq!(
        io_memory.granted(),
        report.required_spool_bytes,
        "spool grant remains held through sidecar emission"
    );

    drop(spool_permit);
    assert_eq!(io_memory.granted(), 0);
}

#[test]
fn exact_terminal_admission_rejects_high_watermark_crossing_before_spool_grant() {
    let block_size = 1024 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("terminal-admission-test"),
        data_blocks_per_stripe: 8,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 1,
    };
    let selected = SelectedTape {
        pool_id: "terminal.admission".to_string(),
        tape_uuid: [0x6c; 16],
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.1,
        watermark_high: 0.2,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let io_memory = crate::io_memory::IoMemoryReservation::new(1).expect("one-byte spool manager");
    let error = reserve_parity_object_capacity(
        TerminalTripleCapacityRuntimeState {
            current_epoch_fill_blocks: 0,
            pending_completed_sidecars: 0,
            pending_completed_epoch_parity_bytes: 0,
            sidecar_entries_before_object: 0,
            structural_entries_before_object: 1,
            object_rows_before_object: 0,
            used_tape_blocks: 1_900,
        },
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        10_000,
        200,
        &io_memory,
    )
    .expect_err("projected prefix crosses H and must roll before Object motion");
    assert!(
        error
            .to_string()
            .contains("requires finalizing the current prefix"),
        "{error}"
    );
    assert_eq!(
        io_memory.granted(),
        0,
        "rejected admission must precede the atomic parity spool grant"
    );
}

#[test]
fn parity_spool_shortfall_is_rejected_before_object_tape_motion() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-capacity-shortfall-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6e; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-shortfall-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP002L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "capacity.shortfall".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let capacity_blocks =
        parity_capacity_basis_blocks(&state, &pool_cfg, &selected).expect("capacity basis");
    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity =
        ParitySink::new_with_journal(&mut raw, &mut journal, scheme, tape_uuid, block_size)
            .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    let position_before = parity
        .terminal_triple_capacity_runtime_state()
        .expect("runtime state")
        .used_tape_blocks;
    let io_memory = crate::io_memory::IoMemoryReservation::new(1).expect("one-byte spool budget");

    let error = reserve_parity_object_capacity(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state"),
        parity.scheme(),
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &io_memory,
    )
    .expect_err("completed parity epoch exceeds the atomic spool budget");
    assert!(
        matches!(
            error,
            PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::ParitySpoolCapacity,
                remaining_spool_bytes: Some(1),
                ..
            })
        ),
        "{error}"
    );
    assert_eq!(
        parity
            .terminal_triple_capacity_runtime_state()
            .expect("runtime state")
            .used_tape_blocks,
        position_before,
        "capacity rejection performs no Object write or filemark"
    );
    assert_eq!(io_memory.granted(), 0);
}

#[test]
fn direct_encrypted_parity_write_uses_configured_spool_ceiling_before_object_motion() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-direct-encrypted-parity-spool-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "capacity.direct-encrypted";
    let tape_uuid = [0x71; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("direct-encrypted-capacity-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP005L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    state
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let pool_cfg = TapePoolConfig {
        id: pool_id.into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected = select_tape_in_pool(&state, &pool_cfg, 32 * 1024, &HashSet::new())
        .expect("fresh parity tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, vec![0x5A; 32 * 1024]).expect("write payload");
    let checkpoint_dir = temp.path().join("checkpoints");
    let parity_journal_path = temp.path().join("parity.remjournal");
    let resources = PoolWriteResources::new(1).expect("one-byte configured spool ceiling");
    let mut sink = VecBlockSink::new();

    let error = write_to_selected_tape_checkpointed(
        &mut state,
        &mut sink,
        &pool_cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.into(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "direct-encrypted-capacity-caller".into(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Encrypted {
                recipients: vec![test_recipient(0x71, 0), test_recipient(0x72, 1)],
            },
        },
        selected,
        &checkpoint_dir,
        &parity_journal_path,
        &resources,
    )
    .expect_err("configured spool ceiling must reject the encrypted Object");
    assert!(
        matches!(
            error,
            PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::ParitySpoolCapacity,
                remaining_spool_bytes: Some(1),
                ..
            })
        ),
        "{error}"
    );
    assert_eq!(
        sink.blocks.len(),
        1,
        "only the identity bootstrap is written"
    );
    assert_eq!(sink.filemarks, vec![1]);
    assert!(
        state
            .get_native_object_by_caller_object_id("direct-encrypted-capacity-caller")
            .expect("query caller object")
            .is_none(),
        "capacity rejection must not publish the Object"
    );
    assert!(
        remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open checkpoint journal")
            .replay()
            .expect("replay checkpoint journal")
            .is_empty(),
        "capacity rejection must not checkpoint the Object"
    );
}

#[test]
fn direct_checkpointed_parity_raw_write_failure_persists_fence_and_blocks_retry() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-direct-parity-raw-fence-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "capacity.direct-fence";
    let tape_uuid = [0x72; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("direct-parity-fence-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP006L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    state
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let pool_cfg = TapePoolConfig {
        id: pool_id.into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected = select_tape_in_pool(&state, &pool_cfg, 16 * 1024, &HashSet::new())
        .expect("fresh parity tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, vec![0xA6; 16 * 1024]).expect("write payload");
    let checkpoint_dir = temp.path().join("checkpoints");
    let parity_journal_path = temp.path().join("parity.remjournal");
    let mut sink = FailOnBlockWriteSink::new(2);

    let error = write_to_selected_tape_checkpointed(
        &mut state,
        &mut sink,
        &pool_cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.into(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "direct-parity-fence-caller".into(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected.clone(),
        &checkpoint_dir,
        &parity_journal_path,
        &test_pool_write_resources(),
    )
    .expect_err("completion-unknown Object command must fail and fence");
    assert!(error
        .to_string()
        .contains("injected parity raw write failure"));
    assert_eq!(
        sink.inner.blocks.len(),
        1,
        "identity bootstrap completed first"
    );
    assert_eq!(sink.inner.filemarks, vec![1]);
    assert!(
        remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open checkpoint journal")
            .replay()
            .expect("replay checkpoint journal")
            .is_empty(),
        "failed Object is not checkpointed"
    );
    let fences = state
        .tape_io_admission_conflicts(&tape_uuid, Some("CAP006L9"))
        .expect("query persisted fence");
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "parity_append");
    let retry_error = ensure_selected_tape_accepts_session_write(&state, &pool_cfg, &selected)
        .expect_err("persisted fence blocks retry before tape motion");
    assert!(matches!(retry_error, PoolWriteError::InvalidInput(_)));
    assert!(retry_error.to_string().contains("active tape-I/O fence"));
}

#[test]
fn parity_capacity_distinguishes_fresh_media_limit_from_current_tape_shortfall() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-capacity-boundary-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x6f; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-boundary-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP003L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    let selected = SelectedTape {
        pool_id: "capacity.boundary".into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let pool_cfg = TapePoolConfig {
        id: selected.pool_id.clone(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.97,
        watermark_high: 0.98,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        scheme.clone(),
        tape_uuid,
        block_size,
    )
    .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    let fresh_runtime = parity
        .terminal_triple_capacity_runtime_state()
        .expect("fresh runtime state");
    assert_eq!(
        fresh_runtime.used_tape_blocks,
        PARITY_INITIAL_BOOTSTRAP_PREFIX_BLOCKS
    );

    let baseline_memory =
        crate::io_memory::IoMemoryReservation::new(1024 * 1024).expect("baseline spool manager");
    let capacity_blocks = 1_000_000;
    let baseline = reserve_parity_object_capacity(
        fresh_runtime,
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &baseline_memory,
    )
    .expect("fresh media admits the object under a valid exact profile");
    let required = baseline.report().required_tape_blocks;
    drop(baseline);

    let oversized_memory =
        crate::io_memory::IoMemoryReservation::new(1024 * 1024).expect("oversized spool manager");
    let error = reserve_parity_object_capacity(
        fresh_runtime,
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        capacity_blocks,
        &oversized_memory,
    )
    .expect_err("the Object is impossible on every fresh replacement tape");
    assert!(
        matches!(
            error,
            PoolWriteError::Parity(ParityError::ObjectTooLargeForEmptyTape { .. })
        ),
        "{error}"
    );

    let current_runtime = TerminalTripleCapacityRuntimeState {
        used_tape_blocks: capacity_blocks - required + 1,
        ..fresh_runtime
    };
    let current_memory = crate::io_memory::IoMemoryReservation::new(1024 * 1024)
        .expect("current-tape spool manager");
    let error = reserve_parity_object_capacity(
        current_runtime,
        &scheme,
        &selected,
        (&pool_cfg, 1, 0),
        capacity_blocks,
        2,
        &current_memory,
    )
    .expect_err("the same Object no longer fits the current nonempty tape");
    assert!(
        matches!(error, PoolWriteError::TerminalCloseRequired { .. }),
        "{error}"
    );
}

#[test]
fn batched_parity_post_motion_projection_failure_sets_dirty_after_retryable_spool_rejection() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-session-retention-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "capacity.session";
    let tape_uuid = [0x70; 16];
    let block_size = 256 * 1024;
    let scheme = ParityScheme {
        id: SchemeId::new_static("capacity-session-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "CAP004L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision mocked LTO-9");
    state
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign mocked tape to its selected pool");
    let pool_cfg = TapePoolConfig {
        id: pool_id.into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected = SelectedTape {
        pool_id: pool_id.into(),
        tape_uuid,
        block_size,
        parity_config: ParityConfig::Scheme(scheme.clone()),
    };
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(
        &payload_path,
        b"session retention after no-motion rejection",
    )
    .expect("write payload");
    let request = || WriteObjectToPoolRequest {
        pool_id: pool_id.into(),
        source: WriteObjectSource::Path(payload_path.clone()),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "capacity-session-caller".into(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };

    let mut backing = VecBlockSink::new();
    let mut raw = BlockSinkRawTapeSink::new(&mut backing);
    let mut journal = PerObjectTestJournal {
        tape_uuid,
        bundles: Vec::new(),
    };
    let mut parity =
        ParitySink::new_with_journal(&mut raw, &mut journal, scheme, tape_uuid, block_size)
            .expect("parity sink");
    parity.write_bootstrap().expect("initial bootstrap");
    let position_before = parity
        .terminal_triple_capacity_runtime_state()
        .expect("runtime state")
        .used_tape_blocks;
    let mut session_state = Some(parity.into_session_state().expect("detach session"));

    let short_memory =
        crate::io_memory::IoMemoryReservation::new(1).expect("one-byte shared spool budget");
    let mut raw_write_attempted = false;
    let error = write_batched_parity_to_selected_tape_after_replay_check(
        &state,
        &mut raw,
        &mut journal,
        &mut session_state,
        &pool_cfg,
        request(),
        selected.clone(),
        &short_memory,
        &mut raw_write_attempted,
    )
    .expect_err("atomic spool shortfall must reject before Object motion");
    assert!(
        matches!(
            error,
            PoolWriteError::Parity(ParityError::CapacityReserveExceeded {
                cause: CapacityReserveCause::ParitySpoolCapacity,
                ..
            })
        ),
        "{error}"
    );
    assert!(!raw_write_attempted);
    assert!(
        session_state.is_some(),
        "no-motion rejection retains the session"
    );
    assert_eq!(
        raw.position().expect("position after rejection").lba,
        position_before
    );

    let ample_memory = crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024)
        .expect("ample shared spool budget");
    let result = write_batched_parity_to_selected_tape_after_replay_check(
        &state,
        &mut raw,
        &mut journal,
        &mut session_state,
        &pool_cfg,
        request(),
        selected.clone(),
        &ample_memory,
        &mut raw_write_attempted,
    )
    .expect("the same retained session accepts a later retry");
    assert!(!result.is_replay());
    assert!(raw_write_attempted);
    assert!(session_state.is_some());

    raw_write_attempted = false;
    FAIL_PARITY_POST_WRITE_PROJECTION.with(|flag| flag.set(true));
    let error = write_batched_parity_to_selected_tape_after_replay_check(
        &state,
        &mut raw,
        &mut journal,
        &mut session_state,
        &pool_cfg,
        request(),
        selected,
        &ample_memory,
        &mut raw_write_attempted,
    )
    .expect_err("post-motion projection failure is injected");
    assert!(
        error
            .to_string()
            .contains("injected post-write parity projection failure"),
        "{error}"
    );
    assert!(
        raw_write_attempted,
        "the caller must see raw motion before projection failure"
    );
    assert!(
        session_state.is_none(),
        "post-motion failure consumes the uncertain session for fencing"
    );
}

fn capacity_tape_record() -> TapeRecord {
    TapeRecord {
        tape_uuid: vec![0x71; 16],
        voltag: Some("CAP005L9".into()),
        kind: "data".into(),
        pool_id: Some("capacity.projection".into()),
        assignment_generation: 0,
        body_format: None,
        block_size: Some(4096),
        scheme_id: None,
        data_blocks_per_stripe: None,
        parity_blocks_per_stripe: None,
        stripes_per_neighborhood: None,
        last_committed_tape_file: None,
        total_committed_ordinals: 0,
        written_extent_lba: None,
        terminal_finalization: None,
        state: "ready".into(),
        updated_at_utc: "2026-08-09T00:00:00Z".into(),
    }
}

#[test]
fn selected_tape_geometry_rejects_partial_parity_columns() {
    let partial = TapeRecord {
        data_blocks_per_stripe: Some(16),
        ..capacity_tape_record()
    };
    let error = selected_tape_geometry(&partial, "capacity.projection")
        .expect_err("stray parity geometry must not be interpreted as parity-off");
    assert!(
        error
            .to_string()
            .contains("parity scheme columns must be either all present or all null"),
        "{error}"
    );
}

#[test]
fn physical_extent_projection_prefers_authority_and_fails_closed_on_legacy_overflow() {
    let authoritative = TapeRecord {
        written_extent_lba: Some(73),
        last_committed_tape_file: Some(u64::MAX),
        total_committed_ordinals: u64::MAX,
        ..capacity_tape_record()
    };
    assert_eq!(tape_physical_used_blocks(&authoritative).unwrap(), 73);

    let empty = capacity_tape_record();
    assert_eq!(tape_physical_used_blocks(&empty).unwrap(), 0);

    let ordinal_only = TapeRecord {
        total_committed_ordinals: 9,
        ..capacity_tape_record()
    };
    assert_eq!(tape_physical_used_blocks(&ordinal_only).unwrap(), 20);

    let with_file_count = TapeRecord {
        total_committed_ordinals: 9,
        last_committed_tape_file: Some(3),
        ..capacity_tape_record()
    };
    assert_eq!(tape_physical_used_blocks(&with_file_count).unwrap(), 17);

    let ordinal_overflow = TapeRecord {
        total_committed_ordinals: u64::MAX,
        ..capacity_tape_record()
    };
    assert!(tape_physical_used_blocks(&ordinal_overflow).is_err());

    let file_count_overflow = TapeRecord {
        last_committed_tape_file: Some(u64::MAX),
        ..capacity_tape_record()
    };
    assert!(tape_physical_used_blocks(&file_count_overflow).is_err());
}

#[test]
fn selector_fit_uses_barrier_proved_physical_extent_at_the_boundary() {
    let cfg = TapePoolConfig {
        id: "capacity.projection".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.92,
        watermark_high: 0.97,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let capacity = raw_capacity_bytes(LtoGen::Lto9);
    let usable = watermark_floor_bytes(capacity, cfg.watermark_high).expect("usable bytes");
    let boundary_lba = usable / cfg.block_size_bytes;
    let full = TapeRecord {
        written_extent_lba: Some(boundary_lba),
        ..capacity_tape_record()
    };
    let fitting = TapeRecord {
        tape_uuid: vec![0x72; 16],
        written_extent_lba: Some(boundary_lba - 1),
        ..capacity_tape_record()
    };
    let full_fit = tape_fit_state_from_record(&full, &cfg, &cfg.id, 0).expect("full fit");
    let fitting_fit = tape_fit_state_from_record(&fitting, &cfg, &cfg.id, 1).expect("fitting fit");
    let candidates = [full_fit, fitting_fit];
    let selection = FillOldest.select(&PoolSelectionContext {
        candidates: &candidates,
        projected_footprint: cfg.block_size_bytes,
    });
    assert_eq!(
        selection,
        Selection::UseTape {
            tape_uuid: [0x72; 16]
        }
    );
}

#[test]
fn l3_ordering_filemark_waits_for_final_clean_staged_batch() {
    let mut sink = StagedTestSink::new(2);

    run_staged_transfer(&mut sink, 4, |staged| {
        staged.write_block(&[1; 4])?;
        staged.write_block(&[2; 4])?;
        staged.write_block(&[3; 4])?;
        staged.write_filemarks(1)?;
        Ok(())
    })
    .expect("staged transfer succeeds");

    assert_eq!(
        sink.events,
        vec![
            "position",
            "intent:2:12:2:1",
            "write_batch:2",
            "write_batch:1",
            "span_ok:2:12:2:1",
            "filemark:1",
        ],
        "WRITE FILEMARKS must be actor-ordered after the final clean data batch"
    );
}

#[test]
fn l3_crash_after_producer_read_before_batch_write_leaves_no_tape_bytes() {
    let mut sink = StagedTestSink::with_ring(2, 2);

    let err = run_staged_transfer(&mut sink, 4, |staged| {
        staged.write_block(&[1; 4])?;
        Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
            "kill after producer read before batch write".to_string(),
        ))
    })
    .expect_err("source-side kill must fail transfer");

    assert!(err.to_string().contains("producer read"));
    assert!(
        sink.inner.blocks.is_empty(),
        "pending process-local buffer must not reach tape after source-side kill"
    );
    assert!(
        !sink
            .events
            .iter()
            .any(|event| event.starts_with("filemark")),
        "source-side failure must not emit filemark: {:?}",
        sink.events
    );
}

#[test]
fn l3_source_error_discards_unsubmitted_window_without_filemark() {
    let mut sink = StagedTestSink::new(2);

    let err = run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..4u8 {
            staged.write_block(&[value; 4])?;
        }
        Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
            "injected source error after first batch".to_string(),
        ))
    })
    .expect_err("source-side error must fail transfer");

    assert!(err.to_string().contains("source error"));
    assert_eq!(
        sink.events,
        vec!["position"],
        "an unsubmitted partial ring window is discarded after producer failure"
    );
}

#[test]
fn l3_sink_error_with_queued_buffers_drains_and_poisons_filemark() {
    let mut sink = StagedTestSink::failing_on_batch(2, 2);

    let err = run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..5u8 {
            staged.write_block(&[value; 4])?;
        }
        staged.write_filemarks(1)?;
        Ok(())
    })
    .expect_err("sink failure must fail transfer");

    assert!(err.to_string().contains("injected sink failure"));
    assert_eq!(
        sink.events,
        vec![
            "position",
            "intent:3:20:2:1",
            "write_batch:2",
            "write_batch:2",
            "span_error",
        ],
        "queued producer buffers are drained after poison, but no filemark reaches the sink"
    );
}

fn fence_test_fixture() -> (tempfile::TempDir, CatalogIndex, SelectedTape) {
    let temp = tempfile::Builder::new()
        .prefix("remanence-transfer-fence-")
        .tempdir()
        .expect("tempdir");
    let mut state = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let tape_uuid = [0x5a; 16];
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "FENCE1L9".into(),
            block_size: 4,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    (
        temp,
        state,
        SelectedTape {
            pool_id: "fence.test".into(),
            tape_uuid,
            block_size: 4,
            parity_config: ParityConfig::None,
        },
    )
}

fn assert_one_transfer_fence(state: &CatalogIndex, expected_error: &str) {
    let fences = state
        .list_active_tape_io_fences()
        .expect("list active tape-I/O fences");
    assert_eq!(
        fences.len(),
        1,
        "exactly one safety funnel persists a fence"
    );
    assert!(
        fences[0]
            .evidence_json
            .as_deref()
            .is_some_and(|evidence| evidence.contains(expected_error)),
        "fence evidence must retain the transfer failure: {fences:?}"
    );
}

#[test]
fn overlap_recovery_cut_matrix_never_projects_partial_object() {
    for cut in [
        "before-first-block",
        "payload",
        "finish-validation",
        "filemark",
    ] {
        let (temp, mut state, selected) = fence_test_fixture();
        let mut sink = StagedTestSink::new(2);
        if cut == "filemark" {
            sink.fail_filemark = true;
        }
        let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
        let manager =
            crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("memory manager");
        let (_producer, _consumer, control) =
            crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
                .expect("ring");
        let cut_control = Arc::clone(&control);
        let mut counted = CountingBlockSink::new(&mut sink, selected.block_size);
        let result = run_counted_fenced_staged_transfer(
            &mut state,
            &selected,
            &mut counted,
            4,
            Some(control),
            |staged| match cut {
                "before-first-block" => Err(PoolWriteError::InvalidInput(
                    "injected disconnect before first block".into(),
                )),
                "payload" => {
                    cut_control.mark_tape_started();
                    staged.write_block(&[0x11; 4])?;
                    Err(PoolWriteError::InvalidInput(
                        "injected disconnect during payload".into(),
                    ))
                }
                "finish-validation" => {
                    cut_control.mark_tape_started();
                    staged.write_block(&[0x22; 4])?;
                    Err(PoolWriteError::InvalidInput(
                        "injected Finish digest disagreement".into(),
                    ))
                }
                "filemark" => {
                    cut_control.mark_tape_started();
                    staged.write_block(&[0x33; 4])?;
                    staged.write_filemarks(1)?;
                    Ok(())
                }
                other => panic!("unhandled cut {other}"),
            },
        );
        assert!(result.is_err(), "cut {cut} must fail closed");
        assert!(
            state
                .list_native_objects()
                .expect("list native objects")
                .is_empty(),
            "cut {cut} must not project an object"
        );
        let fences = state
            .list_active_tape_io_fences()
            .expect("list tape-I/O fences");
        if cut == "before-first-block" {
            assert!(
                fences.is_empty(),
                "pre-write failure has no uncertain tape tail"
            );
        } else {
            assert_eq!(fences.len(), 1, "cut {cut} must fence the tape");
        }

        let retry_pool = "fence.retry";
        let retry_tape = [0x6d; 16];
        state
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: retry_pool.into(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project retry pool");
        state
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: retry_tape,
                voltag: "RETRY1L9".into(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision clean retry tape");
        state
            .project_tape_pool_membership(retry_tape, retry_pool)
            .expect("assign clean retry tape");
        let retry_cfg = TapePoolConfig {
            id: retry_pool.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: Default::default(),
            watermark_low: 0.0001,
            watermark_high: 1.0,
            capacity_cap_bytes: None,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let retry_payload = format!("replay from zero after {cut}").into_bytes();
        let retry_digest: [u8; 32] = Sha256::digest(&retry_payload).into();
        let retry_path = temp.path().join("retry.bin");
        fs::write(&retry_path, &retry_payload).expect("write retained caller source");
        let retry_request = || WriteObjectToPoolRequest {
            pool_id: retry_pool.into(),
            source: WriteObjectSource::Path(retry_path.clone()),
            archive_path: "retry.bin".into(),
            caller_object_id: format!("retry-after-{cut}"),
            expected_content_sha256: Some(retry_digest),
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        };
        let mut retry_sink = VecBlockSink::new();
        let landed = write_object_to_pool(&mut state, &mut retry_sink, &retry_cfg, retry_request())
            .expect("re-send from byte zero lands on a clean tape");
        assert!(!landed.is_replay());
        let blocks_after_landing = retry_sink.blocks.len();
        let replayed =
            write_object_to_pool(&mut state, &mut retry_sink, &retry_cfg, retry_request())
                .expect("same caller id and digest replays idempotently");
        assert!(replayed.is_replay());
        assert_eq!(retry_sink.blocks.len(), blocks_after_landing);
    }
}

#[test]
fn overlap_staged_only_failure_does_not_raise_a_false_tape_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::new(2);
    let ring_bytes = crate::append_ring::APPEND_RING_SLAB_BYTES as u64;
    let manager = crate::io_memory::IoMemoryReservation::new(ring_bytes).expect("manager");
    let (_producer, _consumer, control) =
        crate::append_ring::create_append_ring(&manager, ring_bytes, 90, 25, ring_bytes)
            .expect("ring");
    let mut counted = CountingBlockSink::new(&mut sink, selected.block_size);
    let error = run_counted_fenced_staged_transfer(
        &mut state,
        &selected,
        &mut counted,
        4,
        Some(Arc::clone(&control)),
        |staged| {
            staged.write_block(&[0x91; 4])?;
            Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
                "source failed while first block was still process-local".into(),
            ))
        },
    )
    .expect_err("source failure aborts staged transfer");
    assert!(error.to_string().contains("process-local"), "{error}");
    assert!(!control.tape_started());
    assert!(sink.inner.blocks.is_empty());
    assert!(
        state
            .list_active_tape_io_fences()
            .expect("list fences")
            .is_empty(),
        "no physical WRITE attempt means there is no uncertain tape tail"
    );
}

#[test]
fn producer_error_after_committed_window_records_tape_io_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::with_ring(2, 2);
    let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
        for value in 0..4u8 {
            staged.write_block(&[value; 4])?;
        }
        staged.position()?;
        Err::<(), PoolWriteError>(PoolWriteError::InvalidInput(
            "producer source read failed after committed window".into(),
        ))
    })
    .expect_err("producer failure stops transfer");
    assert!(error.to_string().contains("producer source read failed"));
    assert_eq!(sink.batch_calls, 2, "one full ring window reached tape");
    assert_one_transfer_fence(&state, "producer source read failed");
}

#[test]
fn space_to_eod_error_records_tape_io_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::new(2);
    sink.fail_space_to_eod = true;
    let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
        staged.space_to_end_of_data().map_err(PoolWriteError::from)
    })
    .expect_err("SPACE(EOD) failure stops transfer");
    assert!(error.to_string().contains("space-to-EOD failure"));
    assert_one_transfer_fence(&state, "space-to-EOD failure");
}

#[test]
fn position_error_records_tape_io_fence() {
    let (_temp, mut state, selected) = fence_test_fixture();
    let mut sink = StagedTestSink::new(2);
    sink.fail_position = true;
    let error = run_fenced_staged_transfer(&mut state, &selected, &mut sink, 4, |staged| {
        staged.write_block(&[1; 4]).map_err(PoolWriteError::from)
    })
    .expect_err("READ POSITION failure stops transfer");
    assert!(error.to_string().contains("READ POSITION failure"));
    assert_one_transfer_fence(&state, "READ POSITION failure");
}

#[test]
fn disconnected_free_ring_cannot_mask_inflight_tape_failure() {
    let mut sink = StagedTestSink::with_ring(2, 2);
    sink.batch_error = Some(TapeIoError::PartialBatchUncommittable {
        requested_records: 2,
        written_records: 1,
        requested_bytes: 8,
        written_bytes: 4,
        end_of_medium: false,
        sense: Some(vec![0x70, 0, 0x40]),
    });
    let ordered = Arc::clone(&sink.ordered_events);
    let accounting = Arc::new(RingAccounting::default());
    let mut buffer = PageAlignedBuffer::try_new(8, accounting).expect("test ring buffer");
    buffer.append(&[1; 8]).expect("fill test ring buffer");
    let mut window = PipelinedWindow::new();
    window
        .push(PipelinedBatch {
            buffer,
            cdb: remanence_scsi::read_write::build_write_fixed_cdb(2),
            records: 2,
            block_size_bytes: 4,
        })
        .expect("one in-flight batch");
    let (free_tx, free_rx) = std_mpsc::sync_channel(2);
    drop(free_rx); // producer-side staged sink disappeared mid-window
    let error = execute_pipelined_window(&mut sink, window, &free_tx, None, &mut |_error| {
        ordered.lock().expect("ordered events").push("fence".into());
        Ok(())
    })
    .expect_err("in-flight tape WRITE fails after producer drops receiver");
    let message = error.to_string();
    assert!(
        message.contains("partial fixed batch uncommittable"),
        "{message}"
    );
    assert!(message.contains("staging buffer return"), "{message}");
    assert!(
        sink.audited_partial_sense,
        "deferred WRITE sense must be audited"
    );
    let ordered = sink.ordered_events.lock().expect("ordered events");
    assert_eq!(&ordered[..3], ["classify", "fence", "audit"]);
}

#[test]
fn filemark_fence_failure_still_flushes_deferred_audit_and_reports_both() {
    let mut sink = StagedTestSink::new(2);
    sink.fail_filemark = true;
    let ordered = Arc::clone(&sink.ordered_events);
    let error = run_staged_transfer_with_safety(
        &mut sink,
        4,
        |staged| staged.write_filemarks(1).map_err(PoolWriteError::from),
        |_error| {
            ordered.lock().expect("ordered events").push("fence".into());
            Err(PoolWriteError::InvalidInput(
                "injected fence callback failure".into(),
            ))
        },
    )
    .expect_err("filemark and fence both fail");
    let message = error.to_string();
    assert!(message.contains("WRITE FILEMARKS failure"), "{message}");
    assert!(message.contains("fence callback failure"), "{message}");
    assert_eq!(
        sink.ordered_events
            .lock()
            .expect("ordered events")
            .as_slice(),
        ["fence", "audit"]
    );
}

#[test]
fn pipelined_ring_rebuilds_trailing_cdb_and_uses_page_aligned_buffers() {
    let mut sink = StagedTestSink::with_ring(2, 4);

    run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..5u8 {
            staged.write_block(&[value; 4])?;
        }
        Ok(())
    })
    .expect("pipelined transfer succeeds");

    assert_eq!(sink.diagnostic_publications, 1);

    assert_eq!(
        sink.cdbs,
        vec![
            remanence_scsi::read_write::build_write_fixed_cdb(2).to_vec(),
            remanence_scsi::read_write::build_write_fixed_cdb(2).to_vec(),
            remanence_scsi::read_write::build_write_fixed_cdb(1).to_vec(),
        ],
        "the trailing partial buffer must rebuild TRANSFER LENGTH"
    );
    assert!(
        sink.alignments.iter().all(|alignment| *alignment == 0),
        "all submitted payload slices must be page aligned: {:?}",
        sink.alignments
    );
    assert!(sink.events.contains(&"intent:3:20:2:1".to_string()));
    assert!(sink.events.contains(&"span_ok:3:20:2:1".to_string()));
}

#[test]
fn hot_phase_histogram_reports_exact_mean_and_bucketed_tail() {
    let mut histogram = HotPhaseHistogram::default();
    histogram.record(Duration::from_micros(11));
    histogram.record(Duration::from_micros(39));

    assert_eq!(histogram.mean(), 25);
    assert_eq!(histogram.percentile(50, 100), 25);
    assert_eq!(histogram.percentile(95, 100), 50);
    assert_eq!(histogram.max_us, 39);
}

#[test]
fn transfer_stats_include_staging_wait_and_refill_histograms() {
    let mut diagnostics = StagingPhaseDiagnostics::default();
    diagnostics.wait_us.record(Duration::from_micros(11));
    diagnostics.wait_us.record(Duration::from_micros(39));
    diagnostics.refill_us.record(Duration::from_micros(101));

    let mut stats = BlockSinkStats::default();
    stats.record_staging(&diagnostics);

    assert_eq!(stats.staging_wait_samples, 2);
    assert_eq!(stats.staging_wait_mean_us, 25);
    assert_eq!(stats.staging_wait_p95_us, 50);
    assert_eq!(stats.refill_samples, 1);
    assert_eq!(stats.refill_mean_us, 101);
}

#[test]
fn pipelined_synchronous_batch_propagates_successful_early_warning() {
    let mut sink = StagedTestSink::with_ring(2, 4);
    sink.early_warning_on_batch_call = Some(1);

    let outcome = run_staged_transfer(&mut sink, 4, |staged| {
        staged
            .write_block_batch(&[7; 8], 4)
            .map_err(PoolWriteError::from)
    })
    .expect("full-record EW remains successful");

    assert_eq!(outcome.records_written, 2);
    assert_eq!(outcome.bytes_written, 8);
    assert!(outcome.early_warning);
    assert!(!outcome.end_of_medium);
}

#[test]
fn pipelined_terminal_poison_fences_before_audit_and_discards_queued_batches() {
    let mut sink = StagedTestSink::with_ring(2, 4);
    sink.fail_on_batch_call = Some(2);
    let ordered = Arc::clone(&sink.ordered_events);

    let err = run_staged_transfer_with_safety(
        &mut sink,
        4,
        |staged| {
            for value in 0..8u8 {
                staged.write_block(&[value; 4])?;
            }
            staged.write_filemarks(1)?;
            Ok(())
        },
        |error| {
            assert!(error.to_string().contains("injected sink failure"));
            ordered.lock().expect("ordered events").push("fence".into());
            Ok(())
        },
    )
    .expect_err("second hot submission fails");

    assert!(err.to_string().contains("injected sink failure"));
    assert_eq!(
        sink.batch_calls, 2,
        "queued batches must not issue after poison"
    );
    assert!(!sink
        .events
        .iter()
        .any(|event| event.starts_with("filemark")));
    let ordered = sink.ordered_events.lock().expect("ordered events");
    let fence = ordered.iter().position(|event| event == "fence").unwrap();
    let audit = ordered.iter().position(|event| event == "audit").unwrap();
    assert!(
        fence < audit,
        "safety persistence must precede audit: {ordered:?}"
    );
}

#[test]
fn pipelined_diagnostics_reset_for_each_staged_transfer() {
    let mut sink = StagedTestSink::with_ring(2, 4);

    run_staged_transfer(&mut sink, 4, |staged| {
        for value in 0..4u8 {
            staged.write_block(&[value; 4])?;
        }
        Ok(())
    })
    .expect("first transfer succeeds");
    assert_eq!(sink.pipelined_write_diagnostics().ioctl_samples, 2);
    assert_eq!(sink.pipelined_write_diagnostics().ioctl_max_us, 2_000);

    run_staged_transfer(&mut sink, 4, |staged| {
        staged.write_block(&[9; 4])?;
        Ok(())
    })
    .expect("second transfer succeeds");

    assert_eq!(sink.diagnostic_resets, 2);
    assert_eq!(sink.diagnostic_publications, 2);
    assert_eq!(sink.pipelined_write_diagnostics().ioctl_samples, 1);
    assert_eq!(
        sink.pipelined_write_diagnostics().ioctl_max_us,
        1_000,
        "the prior transfer maximum must not survive"
    );
}

#[test]
fn pipelined_ring_rejects_invalid_runtime_depths_and_checked_size_overflow() {
    for ring_buffers in [0, 1, 17] {
        let mut sink = StagedTestSink::with_ring(2, ring_buffers);
        let err = run_staged_transfer(&mut sink, 4, |_staged| Ok::<_, PoolWriteError>(()))
            .expect_err("invalid ring depth rejects before spawning");
        assert!(err.to_string().contains("staging ring depth"), "{err}");
    }

    let mut sink = StagedTestSink::with_ring(u32::MAX, 16);
    let err = run_staged_transfer(&mut sink, usize::MAX, |_staged| Ok::<_, PoolWriteError>(()))
        .expect_err("ring byte multiplication must be checked");
    assert!(
        err.to_string().contains("staging batch bytes overflow"),
        "{err}"
    );
}

#[test]
fn block_sink_stats_latches_hardware_early_warning() {
    let mut stats = BlockSinkStats::default();
    stats.record_block(256 * 1024, true);
    assert!(stats.early_warning);

    let mut stats = BlockSinkStats::default();
    stats.record_filemarks(1, true, Duration::from_millis(7));
    assert!(stats.early_warning);
    assert_eq!(stats.filemark_write_drain, Duration::from_millis(7));

    let mut stats = BlockSinkStats::default();
    stats.record_position(tape_position_with_warning(true));
    assert!(stats.early_warning);
}

#[test]
fn write_failure_with_position_secondary_keeps_partial_batch_fence_reason() {
    let error = TapeIoError::WriteFailureWithPositionError {
        write_error: Box::new(TapeIoError::PartialBatchUncommittable {
            requested_records: 4,
            written_records: 2,
            requested_bytes: 16,
            written_bytes: 8,
            end_of_medium: true,
            sense: Some(vec![0x70, 0, 0x40]),
        }),
        position_error: Box::new(TapeIoError::OperationFailed(
            "injected arbitration READ POSITION failure".into(),
        )),
    };
    let message = error.to_string();
    assert_eq!(
        tape_io_fence_reason_for_transfer_error(&message),
        "partial_batch"
    );
    assert!(message.contains("arbitration READ POSITION failure"));
}

#[test]
fn live_write_counter_advances_during_transfer() {
    let counter = Arc::new(crate::DriveByteCounters::new(0));
    let mut sink = VecBlockSink::new();
    let mut live_sink = LiveCounterBlockSink::new(&mut sink, Arc::clone(&counter), 4);

    let first = live_sink.write_block(b"abc").expect("first write");
    assert_eq!(first.bytes_written, 3);
    assert_eq!(counter.write_bytes(), 3);
    assert!(counter.write_bytes() > 0);
    assert!(counter.write_bytes() < 8);

    live_sink.write_filemarks(1).expect("filemark write");
    assert_eq!(counter.write_bytes(), 3);

    let second = live_sink.write_block(b"defgh").expect("second write");
    assert_eq!(second.bytes_written, 5);
    assert_eq!(counter.write_bytes(), 8);
}

#[test]
fn pool_write_record_to_proto_carries_append_commit_info() {
    let object = PoolWriteObjectRecord {
        object_id: [0x11; 16],
        caller_object_id: "caller-object".to_string(),
        content_sha256: [0x22; 32],
        logical_size_bytes: 123,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: vec![PoolWriteObjectCopyRecord {
            tape_uuid: [0x44; 16],
            tape_file_number: 3,
            first_body_lba: 9,
            pool_id: "camera.copy-a".to_string(),
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some([0x33; 32]),
            stored_digest: Some([0x44; 32]),
        }],
    };

    let proto = object.to_proto();
    let info = proto
        .append_commit_info
        .expect("append commit info from first copy");
    assert_eq!(info.append_mode, pb::AppendMode::Append as i32);
    assert_eq!(info.tape_uuid, vec![0x44; 16]);
    assert_eq!(info.tape_file_number, Some(3));
    assert_eq!(info.first_body_lba, 9);
    assert_eq!(info.position_before_lba, None);
    assert_eq!(info.position_after_lba, None);
    assert_eq!(info.journal_record_ordinal, None);
    assert_eq!(
        proto.copies[0]
            .plaintext_digest
            .as_ref()
            .map(|digest| digest.value.as_slice()),
        Some(&[0x33; 32][..])
    );
    assert_eq!(
        proto.copies[0]
            .stored_digest
            .as_ref()
            .map(|digest| digest.algorithm.as_str()),
        Some("sha256")
    );
}

#[test]
fn written_ack_never_exposes_provisional_tape_locators() {
    let object = PoolWriteObjectRecord {
        object_id: [0x11; 16],
        caller_object_id: "caller-object".to_string(),
        content_sha256: [0x22; 32],
        logical_size_bytes: 123,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: vec![PoolWriteObjectCopyRecord {
            tape_uuid: [0x44; 16],
            tape_file_number: 37,
            first_body_lba: 9,
            pool_id: "camera.copy-a".to_string(),
            representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
            recipient_epoch_ids: None,
            metadata_frame_len: None,
            plaintext_digest: Some([0x33; 32]),
            stored_digest: Some([0x44; 32]),
        }],
    };
    let batch_id = Uuid::from_bytes([0x55; 16]);

    let proto = object.to_written_proto(batch_id, 4);
    let info = proto.append_commit_info.expect("WRITTEN append info");

    assert!(proto.copies.is_empty(), "copy locators remain invisible");
    assert_eq!(info.durability, pb::AppendDurability::Written as i32);
    assert!(info.tape_uuid.is_empty());
    assert_eq!(info.tape_file_number, None);
    assert_eq!(info.first_body_lba, 0);
    assert_eq!(info.batch_id, batch_id.as_bytes());
    assert_eq!(info.provisional_ordinal, Some(4));
}

#[test]
fn pool_write_record_to_proto_leaves_append_info_absent_without_copies() {
    let object = PoolWriteObjectRecord {
        object_id: [0x11; 16],
        caller_object_id: "caller-object".to_string(),
        content_sha256: [0x22; 32],
        logical_size_bytes: 123,
        body_format: FORMAT_ID.to_string(),
        created_at_utc: "2026-07-05T00:00:00Z".to_string(),
        copies: Vec::new(),
    };

    let proto = object.to_proto();
    assert!(proto.copies.is_empty());
    assert!(proto.append_commit_info.is_none());
}

#[test]
fn append_finish_does_not_double_count() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-pool-write-live-counter")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = CatalogIndex::open(&index_path).expect("open test index");
    let pool_id = "camera.copy-a";
    let tape_uuid = [4u8; 16];
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "RMN001L1".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("project tape membership");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: Some("copy-a".to_string()),
        content_class: Some("camera".to_string()),
        selection_policy: Default::default(),
        watermark_low: 0.0001,
        watermark_high: 1.0,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let selected = select_tape_in_pool(&index, &cfg, 6, &HashSet::new()).expect("select tape");

    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"abcdef").expect("write payload");
    let request = WriteObjectToPoolRequest {
        pool_id: pool_id.to_string(),
        source: WriteObjectSource::Path(payload_path.clone()),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "caller-object".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let counter = Arc::new(crate::DriveByteCounters::new(0));
    let mut sink = VecBlockSink::new();
    let result = write_to_selected_tape_with_live_counter(
        &mut index,
        &mut sink,
        &cfg,
        request,
        selected,
        Some(counter.clone()),
    )
    .expect("write object");

    let physical_bytes = sink
        .blocks
        .iter()
        .map(|block| block.len() as u64)
        .sum::<u64>();
    assert!(physical_bytes > 0);
    assert_eq!(counter.write_bytes(), physical_bytes);
    assert_eq!(result.object.logical_size_bytes, 6);
}

#[test]
fn batch_of_one_core_journals_tape_for_later_daemon_admission() {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-batch-one-journal-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "batch.one";
    let tape_uuid = [0x8E; 16];
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "BAT001L9".to_string(),
            block_size: BLOCK_SIZE,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(BLOCK_SIZE),
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"payload").expect("write payload");
    let request = || WriteObjectToPoolRequest {
        pool_id: pool_id.to_string(),
        source: WriteObjectSource::Path(payload_path.clone()),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "batch-one-caller".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let checkpoint_dir = temp.path().join("checkpoints");
    let checkpoint_handle =
        remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open checkpoint authority");
    let held_lease = checkpoint_handle
        .acquire_exclusive()
        .expect("hold competing checkpoint lease");
    let mut blocked_sink = LocateCountingSink::default();
    write_to_selected_tape_checkpointed(
        &mut index,
        &mut blocked_sink,
        &cfg,
        request(),
        selected.clone(),
        &checkpoint_dir,
        &temp.path().join("unused-parity.remjournal"),
        &test_pool_write_resources(),
    )
    .expect_err("a competing checkpoint writer must reject before tape positioning");
    assert_eq!(blocked_sink.locate_calls, 0);
    drop(held_lease);

    let mut sink = VecBlockSink::new();
    let result = write_to_selected_tape_checkpointed(
        &mut index,
        &mut sink,
        &cfg,
        request(),
        selected,
        &checkpoint_dir,
        &temp.path().join("unused-parity.remjournal"),
        &test_pool_write_resources(),
    )
    .expect("batch-of-one checkpoint succeeds");

    let checkpoint = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("reopen shared checkpoint journal")
        .last()
        .expect("replay checkpoint journal")
        .expect("batch-of-one record exists");
    assert_eq!(checkpoint.committed_object_count, 1);
    assert_eq!(checkpoint.objects.len(), 1);
    assert!(index
        .get_native_object(&Uuid::from_bytes(result.object.object_id).to_string())
        .expect("query projected object")
        .is_some());

    let admitted =
        select_tape_in_pool_for_write_session(&index, &cfg, 7, &HashSet::new(), &checkpoint_dir)
            .expect("daemon selector accepts CLI-journaled non-fresh tape");
    assert_eq!(admitted.tape_uuid, tape_uuid);
}

#[test]
fn direct_watermark_seal_journals_exact_terminal_authority() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-terminal-success-")
        .tempdir()
        .expect("tempdir");
    let pool_id = "terminal.success";
    let tape_uuid = [0x99; 16];
    let block_size = 1024 * 1024;
    let config_text = format!(
        r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false

[[tape_pools]]
id = "{1}"

[journal]
dir = "{0}/journals"
require_trusted_volume = false

[audit]
dir = "{0}/audit"
fsync = true

[index]
sqlite_path = "{0}/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "{0}/cache/tapes"
"#,
        temp.path().display(),
        pool_id
    );
    let config = remanence_state::parse_config_toml(&config_text).expect("parse config");
    let paths = remanence_state::StatePaths::from_config(temp.path().join("config.toml"), &config);
    let checkpoint_dir = paths.journal_dir.join("checkpoints");
    let audit_dir = paths.audit_dir.clone();
    let mut state = StateHandle::open_with_config(paths, config).expect("open locked state");
    let index = state.catalog_index();
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "TER000L9".to_string(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.000_000_000_001,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"terminal authority payload").expect("write payload");
    let mut sink = SparseBlockSink::default();
    let result = write_to_selected_tape_checkpointed(
        index,
        &mut sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "terminal-success-caller".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
        &checkpoint_dir,
        &temp.path().join("unused-parity.remjournal"),
        &test_pool_write_resources(),
    )
    .expect("write, checkpoint, and terminalize tape");
    assert!(result.sealed_after_write());

    let records = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("reopen checkpoint authority")
        .replay()
        .expect("replay checkpoint authority");
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].objects.len(), 1);
    assert!(!records[0].sealed_after_write);
    let terminal = &records[1];
    assert!(terminal.objects.is_empty());
    assert!(terminal.sealed_after_write);
    assert_eq!(terminal.committed_object_count, 1);
    assert_eq!(terminal.ordinal, records[0].ordinal + 1);
    assert_eq!(
        terminal.barrier_bundle.as_ref().map(|bundle| bundle.kind),
        Some(CommittedBundleKind::TerminalComponent)
    );
    let finalization = terminal
        .terminal_finalization
        .as_ref()
        .expect("structured terminal intent is final authority");
    assert_eq!(
        finalization.progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaC
    );
    assert_eq!(finalization.layout.components[4].ordinal, 3);
    assert_eq!(
        terminal.next_tape_file_number,
        finalization.layout.components[4].tape_file_number + 1
    );
    let physical_eod = sink.position().expect("read terminal position");
    assert_eq!(terminal.eod_partition, physical_eod.partition);
    assert_eq!(terminal.eod_lba, physical_eod.lba);
    let tape = index
        .get_tape(&tape_uuid)
        .expect("query tape")
        .expect("tape exists");
    assert_eq!(tape.state, "sealed");
    assert_eq!(tape.written_extent_lba, Some(terminal.eod_lba));

    finish_direct_write_host_suffix(&mut state, &result)
        .expect("publish direct sealed host suffix");
    finish_direct_write_host_suffix(&mut state, &result)
        .expect("direct sealed host suffix is idempotent");

    let session_id = Uuid::from_u128(0x9901);
    state
        .audit()
        .append(remanence_state::AuditEventRecord {
            actor: remanence_state::AuditActor::System,
            source_layer: remanence_state::SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(session_id),
            idempotency_key: None,
            event: remanence_state::AuditEvent::SessionOpened,
            subject: remanence_state::AuditSubject {
                kind: "write".to_string(),
                id: Some(session_id.to_string()),
            },
            detail: std::collections::BTreeMap::from([(
                "session_kind".to_string(),
                ciborium::value::Value::Text("write".to_string()),
            )]),
        })
        .expect("held state audit cursor remains append-safe after external seal fact");
    let audit = remanence_state::FileAuditLog::replay(&audit_dir)
        .expect("replay direct sealing audit chain");
    assert_eq!(
        audit
            .iter()
            .filter(|record| {
                record.event == remanence_state::AuditEvent::TapeSealed
                    && crate::audit_projection::audit_subject_matches_tape(record, tape_uuid)
            })
            .count(),
        1,
        "direct completion must publish exactly one TapeSealed fact before returning"
    );
    assert!(audit
        .windows(2)
        .all(|pair| pair[1].sequence == pair[0].sequence + 1));
    assert_eq!(
        audit.last().map(|record| &record.event),
        Some(&remanence_state::AuditEvent::SessionOpened),
        "refresh must keep StateHandle's cached append cursor on the durable chain"
    );
}

#[test]
fn direct_parity_watermark_seal_commits_prefix_and_terminal_components_before_final_c() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-terminal-success-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "parity.terminal.success";
    let tape_uuid = [0x98; 16];
    let block_size = 1024 * 1024;
    let scheme = ParityScheme {
        id: remanence_parity::SchemeId::new_static("parity-terminal-success-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    };
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "TER998L9".to_string(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.000_000_000_001,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"parity terminal authority payload").expect("write payload");
    let checkpoint_dir = temp.path().join("checkpoints");
    let parity_journal_path = temp.path().join("parity.remjournal");
    let mut sink = SparseBlockSink::default();
    let result = write_to_selected_tape_checkpointed(
        &mut index,
        &mut sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "parity-terminal-success-caller".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
        &checkpoint_dir,
        &parity_journal_path,
        &test_pool_write_resources(),
    )
    .expect("write, checkpoint, and terminalize parity tape");
    assert!(result.sealed_after_write());

    let records = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("reopen checkpoint authority")
        .replay()
        .expect("replay checkpoint authority");
    assert_eq!(records.len(), 2);
    assert!(!records[0].sealed_after_write);
    let terminal = &records[1];
    assert!(terminal.sealed_after_write);
    assert!(terminal.objects.is_empty());
    assert_eq!(terminal.scheme.as_ref(), Some(&scheme));
    assert_eq!(
        terminal.barrier_bundle.as_ref().map(|bundle| bundle.kind),
        Some(CommittedBundleKind::TerminalComponent)
    );
    let finalization = terminal
        .terminal_finalization
        .as_ref()
        .expect("structured terminal intent is final authority");
    assert_eq!(
        finalization.progress,
        remanence_state::TerminalFinalizationProgress::AfterReplicaC
    );
    assert!(finalization.terminal_prefix.is_some());
    let physical_eod = sink.position().expect("read terminal position");
    assert_eq!(terminal.eod_lba, physical_eod.lba);
    let committed = FileTapeFileJournal::open(&parity_journal_path, tape_uuid, block_size, scheme)
        .and_then(|journal| journal.load_committed())
        .expect("replay terminal sink authority");
    assert!(committed.orphaned_bundles.is_empty());
    assert_eq!(
        committed.entries.last().map(|entry| entry.kind),
        Some(TapeFileKind::TapeIndexReplica)
    );
    let tape = index
        .get_tape(&tape_uuid)
        .expect("query tape")
        .expect("tape exists");
    assert_eq!(tape.state, "sealed");
    assert_eq!(tape.written_extent_lba, Some(terminal.eod_lba));
}

#[test]
fn direct_terminal_failure_preserves_object_checkpoint_and_structured_intent() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-terminal-authority-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "terminal.authority";
    let tape_uuid = [0x9A; 16];
    let block_size = 1024 * 1024;
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "TER001L9".to_string(),
            block_size,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.000_000_000_001,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: u64::from(block_size),
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"terminal authority payload").expect("write payload");
    let checkpoint_dir = temp.path().join("checkpoints");
    let mut sink = RejectTerminalReplicaSink::new(tape_uuid);
    let error = write_to_selected_tape_checkpointed(
        &mut index,
        &mut sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "terminal-authority-caller".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
        &checkpoint_dir,
        &temp.path().join("unused-parity.remjournal"),
        &test_pool_write_resources(),
    )
    .expect_err("terminal media failure must be fatal");
    assert!(error
        .to_string()
        .contains("injected terminal replica failure"));
    assert_eq!(sink.terminal_replica_attempts, 1);

    let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
        .expect("reopen checkpoint authority");
    let recovery = journal
        .replay()
        .expect_err("terminal failure must retain a structured finalization lock");
    assert!(
        recovery
            .to_string()
            .contains("pending terminal finalization intent"),
        "{recovery}"
    );
    let recovery_authority = journal
        .acquire_exclusive_for_terminal_recovery()
        .and_then(|mut lease| lease.replay_for_terminal_recovery())
        .expect("source-capable owner can recover structured authority");
    assert_eq!(recovery_authority.records.len(), 1);
    assert_eq!(recovery_authority.records[0].objects.len(), 1);
    assert_eq!(
        recovery_authority
            .finalization_intent
            .as_ref()
            .map(|intent| intent.progress),
        Some(remanence_state::TerminalFinalizationProgress::BeforeReplicaA)
    );
    assert_eq!(
        recovery_authority
            .finalization_intent
            .as_ref()
            .map(|intent| intent.recovery_required),
        Some(true)
    );
    let tape = index
        .get_tape(&tape_uuid)
        .expect("query tape")
        .expect("tape exists");
    assert_eq!(tape.state, "recovery_required");
    let fences = index
        .tape_io_admission_conflicts(&tape_uuid, Some("TER001L9"))
        .expect("query terminal fence");
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "terminal_finalization");
    assert!(
        index
            .get_native_object_by_caller_object_id("terminal-authority-caller")
            .expect("query caller object")
            .is_some(),
        "ordinary Object authority must precede structured terminal intent"
    );
}

#[test]
fn direct_fresh_parity_requires_exact_bot_position_before_bootstrap_write() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-fresh-parity-bot-proof-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "parity.bot.proof";
    let tape_uuid = [0x9C; 16];
    let scheme = ParityScheme {
        id: remanence_parity::SchemeId::new_static("parity-bot-proof-test"),
        data_blocks_per_stripe: 8,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 1,
    };
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "BOTPRF1L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"payload").expect("write payload");
    let checkpoint_dir = temp.path().join("checkpoints");
    let parity_journal_path = temp.path().join("parity.remjournal");
    let mut sink = MisdirectedFreshLocateSink::default();

    let error = write_to_selected_tape_checkpointed(
        &mut index,
        &mut sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(payload_path),
            archive_path: PathBuf::from("payload.bin"),
            caller_object_id: "parity-bot-proof-caller".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        selected,
        &checkpoint_dir,
        &parity_journal_path,
        &test_pool_write_resources(),
    )
    .expect_err("misdirected BOT locate must reject");
    assert!(
        error.to_string().contains("expected partition 0 lba 0"),
        "{error}"
    );
    assert!(sink.inner.blocks.is_empty());
    assert!(sink.inner.filemarks.is_empty());
    assert!(
        remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open checkpoint journal")
            .replay()
            .expect("replay empty checkpoint journal")
            .is_empty()
    );
    let parity_state = FileTapeFileJournal::open(&parity_journal_path, tape_uuid, 4096, scheme)
        .and_then(|journal| journal.load_committed())
        .expect("replay empty parity journal");
    assert!(parity_state.entries.is_empty());
    assert!(parity_state.orphaned_bundles.is_empty());
    let tape = index
        .get_tape(&tape_uuid)
        .expect("query tape")
        .expect("tape exists");
    assert_eq!(tape.last_committed_tape_file, None);
    assert_eq!(tape.written_extent_lba, None);
}

#[test]
fn bootstrap_only_parity_orphan_requires_physical_reconciliation_before_positioning() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-daemon-resume-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "parity.resume";
    let tape_uuid = [0x8F; 16];
    let scheme = ParityScheme {
        id: remanence_parity::SchemeId::new_static("parity-daemon-resume-test"),
        data_blocks_per_stripe: 8,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 1,
    };
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "PAR001L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let checkpoint_dir = temp.path().join("checkpoints");
    let parity_journal_path = temp.path().join("parity.remjournal");

    // Model a crash after the fresh BOT was durably written and projected,
    // but before the first object checkpoint existed. The BOT bundle is
    // intentionally still orphaned in the sink journal.
    {
        let mut journal = FileTapeFileJournal::open(
            &parity_journal_path,
            tape_uuid,
            selected.block_size,
            scheme.clone(),
        )
        .expect("open fresh parity journal");
        journal
            .commit_bundle(&CommittedBundle {
                kind: CommittedBundleKind::BotBootstrap,
                entries: vec![TapeFileEntry {
                    tape_file_number: 0,
                    kind: TapeFileKind::Bootstrap,
                    block_count: 1,
                    physical_start_hint: Some(0),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
            })
            .expect("journal fresh BOT bundle");
        project_fresh_parity_bootstrap_bundle(&mut index, &selected, &scheme)
            .expect("project fresh BOT before simulated crash");
        let state = journal.load_committed().expect("load orphaned BOT");
        assert!(state.entries.is_empty());
        assert_eq!(state.orphaned_bundles.len(), 1);
    }
    let bootstrap_only = index
        .get_tape(&tape_uuid)
        .expect("query bootstrap-only tape")
        .expect("bootstrap-only tape exists");
    assert_eq!(bootstrap_only.total_committed_ordinals, 0);
    assert_eq!(bootstrap_only.last_committed_tape_file, Some(0));

    let recovered =
        select_tape_in_pool_for_write_session(&index, &cfg, 7, &HashSet::new(), &checkpoint_dir)
            .expect("pool selector admits a parity BOT without an object checkpoint");
    let fresh_pinned = writable_pinned_disposition(
        admit_pinned_tape_for_write_session(&index, tape_uuid, pool_id, &cfg, &checkpoint_dir)
            .expect("pinned selector admits a parity BOT without an object checkpoint"),
    );
    assert_eq!(fresh_pinned.tape_uuid, tape_uuid);

    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"payload").expect("write payload");
    let request = WriteObjectToPoolRequest {
        pool_id: pool_id.to_string(),
        source: WriteObjectSource::Path(payload_path),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "parity-resume-caller".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let mut sink = LocateCountingSink::default();
    let error = write_to_selected_tape_checkpointed(
        &mut index,
        &mut sink,
        &cfg,
        request,
        recovered,
        &checkpoint_dir,
        &parity_journal_path,
        &test_pool_write_resources(),
    )
    .expect_err("an orphaned BOT must require physical reconciliation");
    assert!(error.to_string().contains("physical-tail reconciliation"));
    assert_eq!(
        sink.locate_calls, 0,
        "orphaned evidence must be rejected before positioning tape"
    );

    let preserved = FileTapeFileJournal::open(
        &parity_journal_path,
        tape_uuid,
        u32::try_from(cfg.block_size_bytes).expect("test block size fits u32"),
        scheme,
    )
    .and_then(|journal| journal.load_committed())
    .expect("reopen preserved orphan evidence");
    assert!(preserved.entries.is_empty());
    assert_eq!(preserved.orphaned_bundles.len(), 1);
}

#[test]
fn checkpointed_parity_tape_is_admitted_for_daemon_resume() {
    let temp = tempfile::Builder::new()
        .prefix("remanence-parity-daemon-resume-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    let pool_id = "parity.resume";
    let tape_uuid = [0x90; 16];
    let scheme = ParityScheme {
        id: remanence_parity::SchemeId::new_static("parity-daemon-resume-test"),
        data_blocks_per_stripe: 8,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 1,
    };
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid,
            voltag: "PAR002L9".to_string(),
            block_size: 256 * 1024,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .expect("provision parity tape");
    index
        .project_tape_pool_membership(tape_uuid, pool_id)
        .expect("assign tape");
    let cfg = TapePoolConfig {
        id: pool_id.to_string(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: Default::default(),
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: 256 * 1024,
        min_object_size_bytes: 0,
    };
    let selected =
        select_tape_in_pool(&index, &cfg, 7, &HashSet::new()).expect("fresh tape selects");
    let checkpoint_dir = temp.path().join("checkpoints");
    let parity_journal_path = temp.path().join("parity.remjournal");

    let payload_path = temp.path().join("payload.bin");
    std::fs::write(&payload_path, b"payload").expect("write payload");
    let request = WriteObjectToPoolRequest {
        pool_id: pool_id.to_string(),
        source: WriteObjectSource::Path(payload_path),
        archive_path: PathBuf::from("payload.bin"),
        caller_object_id: "parity-resume-caller".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let mut sink = LocateCountingSink::default();
    write_to_selected_tape_checkpointed(
        &mut index,
        &mut sink,
        &cfg,
        request,
        selected,
        &checkpoint_dir,
        &parity_journal_path,
        &test_pool_write_resources(),
    )
    .expect("fresh parity write reaches the first checkpoint");
    assert_eq!(sink.locate_calls, 1, "fresh write positions at BOT once");

    let admitted =
        select_tape_in_pool_for_write_session(&index, &cfg, 7, &HashSet::new(), &checkpoint_dir)
            .expect("daemon selector admits checkpointed parity tape");
    assert_eq!(admitted.tape_uuid, tape_uuid);

    let pinned = writable_pinned_disposition(
        admit_pinned_tape_for_write_session(&index, tape_uuid, pool_id, &cfg, &checkpoint_dir)
            .expect("pinned selector admits checkpoint-authorized parity tape"),
    );
    assert_eq!(pinned.tape_uuid, tape_uuid);

    let committed = FileTapeFileJournal::open(
        &parity_journal_path,
        tape_uuid,
        u32::try_from(cfg.block_size_bytes).expect("test block size fits u32"),
        scheme,
    )
    .and_then(|mut journal| {
        let state = journal.load_committed()?;
        let next_file = state
            .entries
            .last()
            .expect("checkpointed journal has a tape-file prefix")
            .tape_file_number
            .checked_add(1)
            .expect("test tape-file number");
        journal.commit_bundle(&CommittedBundle {
            kind: CommittedBundleKind::Object,
            entries: vec![TapeFileEntry {
                tape_file_number: next_file,
                kind: TapeFileKind::Object,
                block_count: 1,
                physical_start_hint: Some(100),
                object_id: Some("sink-ahead".to_string()),
                first_parity_data_ordinal: Some(state.total_committed_ordinals),
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                object_recovery_row: None,
            }],
            highest_protected_ordinal: state.highest_protected_ordinal,
            total_committed_ordinals: state
                .total_committed_ordinals
                .checked_add(1)
                .expect("test ordinal"),
        })?;
        journal.commit_bundle(&CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: state.highest_protected_ordinal,
            total_committed_ordinals: state
                .total_committed_ordinals
                .checked_add(1)
                .expect("test ordinal"),
        })?;
        journal.load_committed()
    })
    .expect("advance only the sink journal past the shared checkpoint");
    assert!(committed.orphaned_bundles.is_empty());

    let second_payload_path = temp.path().join("second-payload.bin");
    std::fs::write(&second_payload_path, b"second payload").expect("write second payload");
    let second_request = WriteObjectToPoolRequest {
        pool_id: pool_id.to_string(),
        source: WriteObjectSource::Path(second_payload_path),
        archive_path: PathBuf::from("second-payload.bin"),
        caller_object_id: "parity-resume-second-caller".to_string(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    };
    let mut mismatch_sink = LocateCountingSink::default();
    let error = write_to_selected_tape_checkpointed(
        &mut index,
        &mut mismatch_sink,
        &cfg,
        second_request,
        pinned.clone(),
        &checkpoint_dir,
        &parity_journal_path,
        &test_pool_write_resources(),
    )
    .expect_err("disagreeing durable resume authorities must fail closed");
    assert!(
        error
            .to_string()
            .contains("bounded parity terminal authority mismatch"),
        "{error}"
    );
    assert_eq!(
        mismatch_sink.locate_calls, 0,
        "authority disagreement must be rejected before positioning tape"
    );

    let missing_journal_path = temp.path().join("missing-parity.remjournal");
    let mut missing_journal_sink = LocateCountingSink::default();
    let error = write_to_selected_tape_checkpointed(
        &mut index,
        &mut missing_journal_sink,
        &cfg,
        WriteObjectToPoolRequest {
            pool_id: pool_id.to_string(),
            source: WriteObjectSource::Path(temp.path().join("second-payload.bin")),
            archive_path: PathBuf::from("missing-journal-payload.bin"),
            caller_object_id: "parity-resume-missing-journal".to_string(),
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
            representation: PoolWriteRepresentation::Plaintext,
        },
        pinned,
        &checkpoint_dir,
        &missing_journal_path,
        &test_pool_write_resources(),
    )
    .expect_err("a missing sink journal must not turn a checkpointed tape into fresh media");
    assert!(
        error
            .to_string()
            .contains("bounded parity terminal authority mismatch"),
        "{error}"
    );
    assert_eq!(
        missing_journal_sink.locate_calls, 0,
        "a missing sink authority must be rejected before append positioning"
    );
}

#[test]
fn lto_generation_parses_m8_type_m_suffix() {
    assert_eq!(lto_generation_from_voltag("RMN001M8"), Some(LtoGen::M8));
    assert_eq!(lto_generation_from_voltag("rmn001m8"), Some(LtoGen::M8));
    assert_eq!(raw_capacity_bytes(LtoGen::M8), 9_000_000_000_000);
}

#[test]
fn lto_generation_treats_lz_as_lto9_media_class() {
    assert_eq!(lto_generation_from_voltag("RMN001LZ"), Some(LtoGen::Lto9));
    assert_eq!(lto_generation_from_voltag("rmn001lz"), Some(LtoGen::Lto9));
    assert_eq!(raw_capacity_bytes(LtoGen::Lto9), 18_000_000_000_000);
}

#[test]
fn lto_generation_rejects_non_ascii_without_panic() {
    assert_eq!(lto_generation_from_voltag("éX"), None);
}

#[test]
fn lto_drive_generation_parses_common_inquiry_products() {
    assert_eq!(
        lto_generation_from_drive_product("Ultrium 9-SCSI"),
        Some(LtoGen::Lto9)
    );
    assert_eq!(
        lto_generation_from_drive_product("LTO-8 HH"),
        Some(LtoGen::Lto8)
    );
    assert_eq!(lto_generation_from_drive_product("unknown"), None);
}

#[test]
fn lto_read_compatibility_uses_design_table() {
    let cases = [
        (
            LtoGen::Lto5,
            &[LtoGen::Lto5, LtoGen::Lto4, LtoGen::Lto3][..],
        ),
        (
            LtoGen::Lto6,
            &[LtoGen::Lto6, LtoGen::Lto5, LtoGen::Lto4][..],
        ),
        (
            LtoGen::Lto7,
            &[LtoGen::Lto7, LtoGen::Lto6, LtoGen::Lto5][..],
        ),
        (LtoGen::Lto8, &[LtoGen::Lto8, LtoGen::Lto7, LtoGen::M8][..]),
        (LtoGen::Lto9, &[LtoGen::Lto9, LtoGen::Lto8][..]),
    ];
    let all_tapes = [
        LtoGen::Lto1,
        LtoGen::Lto2,
        LtoGen::Lto3,
        LtoGen::Lto4,
        LtoGen::Lto5,
        LtoGen::Lto6,
        LtoGen::Lto7,
        LtoGen::M8,
        LtoGen::Lto8,
        LtoGen::Lto9,
    ];

    for (drive, readable) in cases {
        for tape in all_tapes {
            assert_eq!(
                can_read(drive, tape),
                readable.contains(&tape),
                "drive={drive:?} tape={tape:?}"
            );
        }
    }
    assert!(!can_read(LtoGen::Lto8, LtoGen::Lto6));
    assert!(!can_read(LtoGen::Lto9, LtoGen::Lto7));
    assert!(!can_read(LtoGen::Lto9, LtoGen::M8));
}

#[test]
fn lto_write_compatibility_uses_design_table() {
    let cases = [
        (LtoGen::Lto5, &[LtoGen::Lto5, LtoGen::Lto4][..]),
        (LtoGen::Lto6, &[LtoGen::Lto6, LtoGen::Lto5][..]),
        (LtoGen::Lto7, &[LtoGen::Lto7, LtoGen::Lto6][..]),
        (LtoGen::Lto8, &[LtoGen::Lto8, LtoGen::Lto7, LtoGen::M8][..]),
        (LtoGen::Lto9, &[LtoGen::Lto9, LtoGen::Lto8][..]),
    ];
    let all_tapes = [
        LtoGen::Lto1,
        LtoGen::Lto2,
        LtoGen::Lto3,
        LtoGen::Lto4,
        LtoGen::Lto5,
        LtoGen::Lto6,
        LtoGen::Lto7,
        LtoGen::M8,
        LtoGen::Lto8,
        LtoGen::Lto9,
    ];

    for (drive, writable) in cases {
        for tape in all_tapes {
            assert_eq!(
                can_write(drive, tape),
                writable.contains(&tape),
                "drive={drive:?} tape={tape:?}"
            );
        }
    }
    assert!(!can_write(LtoGen::Lto8, LtoGen::Lto6));
    assert!(!can_write(LtoGen::Lto9, LtoGen::Lto7));
    assert!(!can_write(LtoGen::Lto9, LtoGen::M8));
}

// -- pinned-tape admission matrix ---------------------------------------
//
// Pinning replaces selection, never admission: these tests pin the
// refusals. The batch-eligibility branch (committed tape without an
// adopted checkpoint journal) reuses the same helpers the pool-mode
// selection tests already exercise and needs a written tape to stage, so
// it is not duplicated here.

struct PinnedFixture {
    index: CatalogIndex,
    pool_cfg: TapePoolConfig,
    journal_dir: std::path::PathBuf,
    _temp: tempfile::TempDir,
}

const PINNED_POOL: &str = "camera.copy-a";
const PINNED_TAPE: TapeUuid = [0x5a; 16];

fn pinned_fixture() -> PinnedFixture {
    let temp = tempfile::Builder::new()
        .prefix("remanence-pinned-admission-")
        .tempdir()
        .expect("tempdir");
    let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open catalog");
    index
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: PINNED_POOL.to_string(),
            display_name: None,
            copy_class: Some("copy-a".to_string()),
            content_class: Some("camera".to_string()),
            created_at_utc: None,
        })
        .expect("project pool");
    index
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: PINNED_TAPE,
            voltag: "PIN001L9".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    index
        .project_tape_pool_membership(PINNED_TAPE, PINNED_POOL)
        .expect("project membership");
    let pool_cfg = TapePoolConfig {
        id: PINNED_POOL.to_string(),
        display_name: None,
        copy_class: Some("copy-a".to_string()),
        content_class: Some("camera".to_string()),
        selection_policy: Default::default(),
        watermark_low: 0.98,
        watermark_high: 1.0,
        capacity_cap_bytes: None,
        block_size_bytes: 4096,
        min_object_size_bytes: 0,
    };
    let journal_dir = temp.path().join("checkpoint-journals");
    std::fs::create_dir_all(&journal_dir).expect("journal dir");
    PinnedFixture {
        index,
        pool_cfg,
        journal_dir,
        _temp: temp,
    }
}

fn admit(
    fixture: &PinnedFixture,
    tape_uuid: TapeUuid,
    guard: &str,
) -> Result<SelectedTape, PinnedTapeError> {
    match admit_pinned_tape_for_write_session(
        &fixture.index,
        tape_uuid,
        guard,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )? {
        PinnedWriteDisposition::Writable(selected) => Ok(selected),
        PinnedWriteDisposition::HostOnlyTerminalRecovery(_) => {
            panic!("ordinary pinned-admission helper received recovery-only disposition")
        }
    }
}

fn writable_pinned_disposition(disposition: PinnedWriteDisposition) -> SelectedTape {
    match disposition {
        PinnedWriteDisposition::Writable(selected) => selected,
        PinnedWriteDisposition::HostOnlyTerminalRecovery(_) => {
            panic!("expected ordinary writable pinned disposition")
        }
    }
}

#[test]
fn pinned_admission_accepts_matching_pooled_tape() {
    let fixture = pinned_fixture();
    let selected = admit(&fixture, PINNED_TAPE, PINNED_POOL).expect("admit pinned tape");
    assert_eq!(selected.tape_uuid, PINNED_TAPE);
    assert_eq!(selected.pool_id, PINNED_POOL);
    assert_eq!(selected.block_size, 4096);
}

#[test]
fn pinned_admission_refuses_unknown_tape() {
    let fixture = pinned_fixture();
    let error = admit(&fixture, [0x77; 16], PINNED_POOL).unwrap_err();
    assert!(
        matches!(error, PinnedTapeError::UnknownTape { .. }),
        "{error}"
    );
    // The message must teach the uninitialized-cartridge case.
    assert!(error.to_string().contains("rem tape init"), "{error}");
}

#[test]
fn pinned_admission_refuses_pool_guard_mismatch_naming_both_pools() {
    let fixture = pinned_fixture();
    let error = admit(&fixture, PINNED_TAPE, "offsite.copy-b").unwrap_err();
    match &error {
        PinnedTapeError::PoolGuardMismatch {
            required_pool_id,
            actual_pool_id,
            ..
        } => {
            assert_eq!(required_pool_id, "offsite.copy-b");
            assert_eq!(actual_pool_id.as_deref(), Some(PINNED_POOL));
        }
        other => panic!("expected PoolGuardMismatch, got {other}"),
    }
    let message = error.to_string();
    assert!(message.contains("offsite.copy-b"), "{message}");
    assert!(message.contains(PINNED_POOL), "{message}");
}

#[test]
fn pinned_admission_refuses_unpooled_tape_under_a_guard() {
    let fixture = pinned_fixture();
    let unpooled: TapeUuid = [0x5b; 16];
    {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: unpooled,
                voltag: "PIN002L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision unpooled tape");
    }
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        unpooled,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    match &error {
        PinnedTapeError::PoolGuardMismatch { actual_pool_id, .. } => {
            assert_eq!(actual_pool_id, &None);
        }
        other => panic!("expected PoolGuardMismatch, got {other}"),
    }
}

#[test]
fn pinned_admission_refuses_cleaning_cartridge() {
    let fixture = pinned_fixture();
    let cleaning = {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        let record = index
            .ensure_cleaning_cartridge("CLN901L9")
            .expect("cleaning cartridge");
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&record.tape_uuid);
        // A cleaning cartridge must refuse even if someone projected it
        // into the guarded pool.
        index
            .project_tape_pool_membership(uuid, PINNED_POOL)
            .expect("project cleaning membership");
        uuid
    };
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        cleaning,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    assert!(
        matches!(error, PinnedTapeError::NotADataTape { .. }),
        "{error}"
    );
}

#[test]
fn pinned_admission_refuses_sealed_tape() {
    let fixture = pinned_fixture();
    {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        index.seal_tape(PINNED_TAPE).expect("seal tape");
    }
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        PINNED_TAPE,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    match &error {
        PinnedTapeError::NotWritable { reason, .. } => {
            assert!(
                matches!(reason, WritabilityError::NotReady { .. }),
                "{reason}"
            );
        }
        other => panic!("expected NotWritable, got {other}"),
    }
}

#[test]
fn pinned_admission_refuses_fenced_tape() {
    let fixture = pinned_fixture();
    {
        let mut index =
            CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
        index
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid: PINNED_TAPE,
                barcode: Some("PIN001L9".to_string()),
                reason: "partial_batch".to_string(),
                evidence_json: None,
            })
            .expect("record fence");
    }
    let fresh = CatalogIndex::open(fixture._temp.path().join("rem-state.sqlite")).expect("reopen");
    let error = admit_pinned_tape_for_write_session(
        &fresh,
        PINNED_TAPE,
        PINNED_POOL,
        &fixture.pool_cfg,
        &fixture.journal_dir,
    )
    .unwrap_err();
    match &error {
        PinnedTapeError::Fenced { reason, .. } => {
            assert_eq!(reason, "partial_batch");
        }
        other => panic!("expected Fenced, got {other}"),
    }
}
