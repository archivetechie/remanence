//! Rung D-R (design D r2, D1 and D1b): the verified whole-object read answers a
//! stored-byte integrity failure with DATA_LOSS, for objects of any length.
//!
//! `stream_one_object` is driven directly over a model drive. The drive-actor
//! fixtures in `write_owner/tests.rs` are private to that module, so the small
//! model-library and configuration fixtures below stand alone.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use remanence_chaos::model::{DeviceRole, ModelTransport, Record, VirtualTape, VirtualWorld};
use remanence_format::{write_rem_tar_object, RemTarFile, RemTarObjectLayout, RemTarObjectOptions};
use remanence_library::{TapeIoRuntimeConfig, VecBlockSink};
use remanence_parity::{FileRowsKind, FileRowsReport, ParityConfig, TapeFileEntry, TapeFileKind};
use remanence_state::{
    NativeObjectFileProjectionInput, NativeObjectInstanceProjectionInput,
    NativeObjectProjectionInput, ProvisionTapeInput, TapeJournalIndexInput,
    OBJECT_INSTANCE_REPRESENTATION_PLAINTEXT,
};
use tonic::Code;

use super::*;

const BLOCK: usize = 512;
const OBJECT_ID: &str = "d0d0d0d0-d0d0-4d0d-8d0d-d0d0d0d0d0d0";
const TAPE_UUID: [u8; 16] = [0xd5; 16];
const VOLTAG: &str = "DRCORR01";
const BAY: u16 = 0x0100;
const SLOT: u16 = 0x0400;
const MEMBER_1: &str = "member-1.bin";
const MEMBER_2: &str = "member-2.bin";

/// The same two-member object as `read_core`'s D-R tests: member 1 (the first
/// payload member, 3 chunks) has the first byte of its last chunk flipped, and
/// member 2 (12 chunks), the manifest and the tar EOF records follow it.
fn two_member_object_with_flipped_member_1() -> (Vec<Vec<u8>>, RemTarObjectLayout) {
    let mut opts = RemTarObjectOptions::new(
        OBJECT_ID,
        "caller-stored-corrupt",
        "2026-11-08T00:00:00Z",
        "d1d1d1d1-d1d1-4d1d-8d1d-d1d1d1d1d1d1",
    );
    opts.chunk_size = BLOCK;
    let member_1 = (0..1300)
        .map(|value| u8::try_from(value % 251).unwrap())
        .collect::<Vec<_>>();
    let member_2 = (0..12 * BLOCK)
        .map(|value| u8::try_from(value % 241).unwrap())
        .collect::<Vec<_>>();
    let files = [
        RemTarFile {
            path: MEMBER_1,
            file_id: "file-member-1",
            data: member_1.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        },
        RemTarFile {
            path: MEMBER_2,
            file_id: "file-member-2",
            data: member_2.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        },
    ];
    let mut sink = VecBlockSink::new();
    let layout = write_rem_tar_object(&mut sink, &opts, &files).expect("write object");
    let member = &layout.files[0];
    assert_eq!(member.path, MEMBER_1);
    let last = member
        .first_chunk_lba
        .expect("member 1 has payload chunks")
        .0
        + member.chunk_count
        - 1;
    let mut blocks = sink.blocks;
    blocks[usize::try_from(last).unwrap()][0] ^= 0x01;
    (blocks, layout)
}

/// Catalog the object as tape file 1 of a provisioned no-parity tape.
fn project_object(index: &mut CatalogIndex, layout: &RemTarObjectLayout) {
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: TAPE_UUID,
            voltag: VOLTAG.to_string(),
            block_size: BLOCK as u32,
            parity: ParityConfig::None,
            force: false,
        })
        .expect("provision tape");
    let files = layout
        .files
        .iter()
        .map(|file| NativeObjectFileProjectionInput {
            object_id: OBJECT_ID.to_string(),
            file_id: file.file_id.clone(),
            path: file.path.clone(),
            size_bytes: file.size_bytes,
            file_sha256: file.file_sha256.expect("regular member hash").to_vec(),
            first_chunk_lba: file.first_chunk_lba.map(|lba| lba.0),
            chunk_count: file.chunk_count,
            mtime: Some("0".to_string()),
            executable: Some(false),
        })
        .collect::<Vec<_>>();
    index
        .project_native_object_and_committed_tape_file_bundle(
            NativeObjectProjectionInput {
                object_id: OBJECT_ID.to_string(),
                caller_object_id: Some("caller-stored-corrupt".to_string()),
                body_format: "rem-object-v1".to_string(),
                logical_size_bytes: Some(layout.files.iter().map(|file| file.size_bytes).sum()),
                content_hash: None,
                metadata_hash: Some(layout.manifest_sha256.to_vec()),
                created_at_utc: Some("2026-11-08T00:00:00Z".to_string()),
            },
            &files,
            &[NativeObjectInstanceProjectionInput {
                object_id: OBJECT_ID.to_string(),
                tape_uuid: TAPE_UUID,
                tape_file_number: 1,
                first_body_lba: 0,
                first_parity_data_ordinal: None,
                protected_until_ordinal: None,
                status: "committed".to_string(),
                representation: OBJECT_INSTANCE_REPRESENTATION_PLAINTEXT.to_string(),
                recipient_epoch_ids: None,
                metadata_frame_len: None,
                plaintext_digest: None,
                stored_digest: None,
            }],
            TapeJournalIndexInput {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK as u32,
                scheme: None,
                journal_offset_bytes: 0,
            },
            &(&FileRowsReport {
                kind: FileRowsKind::Object,
                entries: vec![TapeFileEntry {
                    tape_file_number: 1,
                    kind: TapeFileKind::Object,
                    block_count: layout.projected_size_blocks,
                    physical_start_hint: Some(2),
                    object_id: Some(OBJECT_ID.to_string()),
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
                .try_into()
                .expect("file publication"),
        )
        .expect("project object");
}

fn open_model_library(world: Arc<Mutex<VirtualWorld>>) -> remanence_library::LibraryHandle {
    let library = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    library
        .open_with(&policy, move |path| {
            let role = world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            Ok(Box::new(ModelTransport::new(Arc::clone(&world), role)))
        })
        .expect("open model library")
}

fn discovery_report(
    library: &remanence_library::LibraryHandle,
) -> remanence_library::DiscoveryReport {
    remanence_library::DiscoveryReport {
        unattached_drives: Vec::new(),
        complete: true,
        incomplete_evidence: Vec::new(),
        libraries: vec![library.library().clone()],
        warnings: Vec::new(),
    }
}

/// A library-placed actor configuration; `stream_one_object` reads only its
/// `tape_io` and `io_memory`.
fn write_owner_config(
    root: &Path,
    index_path: PathBuf,
    library: &remanence_library::LibraryHandle,
    tape_io: TapeIoConfig,
) -> WriteOwnerConfig {
    let serial = library.library().serial.clone();
    let audit_dir = root.join("audit");
    let checkpoint_journal_dir = root.join("tapes");
    std::fs::create_dir_all(&audit_dir).expect("audit dir");
    std::fs::create_dir_all(&checkpoint_journal_dir).expect("checkpoint dir");
    WriteOwnerConfig {
        authority: crate::write_owner::TerminalAuthority::fixture(
            None,
            &audit_dir,
            false,
            &Arc::new(Mutex::new(())),
        ),
        catalog: remanence_state::projector::open(&index_path).expect("open catalog client"),
        index_path,
        report: discovery_report(library),
        policy: remanence_library::StaticAllowlist::new([serial.as_str()]),
        reservations: Arc::new(HashMap::new()),
        placement: crate::write_owner::ActorPlacement::Library(
            crate::write_owner::LibraryActorContext {
                serial: serial.clone(),
                snapshot: Arc::new(RwLock::new(Arc::new(crate::LibrarySnapshot {
                    report: discovery_report(library),
                    captured_at: OffsetDateTime::UNIX_EPOCH,
                }))),
            },
        ),
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([serial])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io,
        tape_pools: Vec::new(),
        verification_route: remanence_parity::VerificationRoute::Walk,
        io_memory: crate::io_memory::IoMemoryReservation::new(64 * 1024 * 1024)
            .expect("I/O memory manager"),
        write_admissions: crate::write_owner::WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir,
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: None,
        changer_ready_wait: remanence_library::ChangerReadyWait::default(),
        calibration_store: remanence_state::CalibrationControlStore::open(root.join("calibration"))
            .expect("open calibration store"),
    }
}

struct ModelObjectRead {
    result: Result<(), Status>,
    /// READ(6) commands the drive saw during `stream_one_object`.
    reads: usize,
    block_count: u64,
}

/// Write the corrupt object to tape file 1 of a model cartridge, open the
/// drive with `read_batch_blocks` per READ and a two-buffer staging ring, set
/// it to fixed reads as a read session does, and run `stream_one_object` with
/// the minimum reservoir for that batch.
fn read_corrupt_object_through_model_drive(read_batch_blocks: u32) -> ModelObjectRead {
    let (blocks, layout) = two_member_object_with_flipped_member_1();
    let temp = tempfile::Builder::new()
        .prefix("rem-dr-")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("rem-state.sqlite");
    let mut index = remanence_state::projector::open(&index_path)
        .map(|catalog| catalog.write_handle())
        .expect("open catalog");
    project_object(&mut index, &layout);

    let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK as u32);
    tape.records = std::iter::once(Record::Block(vec![0; BLOCK]))
        .chain([Record::Filemark])
        .chain(blocks.into_iter().map(Record::Block))
        .chain([Record::Filemark])
        .collect();
    tape.written_bytes = tape
        .records
        .iter()
        .filter(|record| matches!(record, Record::Block(_)))
        .count() as u64
        * BLOCK as u64;
    let mut world =
        VirtualWorld::single_drive("LIB-STORED-CORRUPT", BAY, "DRV-STORED-CORRUPT", SLOT, 1);
    world.put_tape_in_drive(BAY, VOLTAG, Some(SLOT), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(Arc::clone(&world));
    let serial = library.library().serial.clone();
    let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
    let ring_buffers = 2;
    let mut drive = library
        .open_drive_with_tape_io(
            BAY,
            &policy,
            TapeIoRuntimeConfig {
                staging_ring_buffers: ring_buffers,
                write_batch_blocks: read_batch_blocks,
                read_batch_blocks,
                ..TapeIoRuntimeConfig::default()
            },
        )
        .expect("open model drive");
    crate::write_owner::terminal_inventory::prepare_drive_for_read(
        &index,
        &mut drive,
        &TAPE_UUID,
        Uuid::new_v4(),
    )
    .expect("fixed read mode");
    let tape_io = TapeIoConfig {
        read_reservoir_bytes: BLOCK as u64 * u64::from(read_batch_blocks) * u64::from(ring_buffers),
        ..TapeIoConfig::default()
    };
    let cfg = write_owner_config(temp.path(), index_path, &library, tape_io);
    world.lock().expect("world lock").command_log.clear();

    let (chunk_tx, _chunk_rx) = crate::read_core::read_stream_channel(0);
    let result = stream_one_object(
        &mut index,
        &mut drive,
        &cfg,
        Uuid::new_v4(),
        &TAPE_UUID,
        OBJECT_ID,
        0,
        chunk_tx,
    );
    let reads = world
        .lock()
        .expect("world lock")
        .command_log
        .iter()
        .filter(|command| command.role == DeviceRole::Drive { bay: BAY } && command.opcode == 0x08)
        .count();
    ModelObjectRead {
        result,
        reads,
        block_count: layout.projected_size_blocks,
    }
}

fn assert_stored_corrupt_data_loss(test: &str, read: ModelObjectRead) {
    let status = read
        .result
        .expect_err("a flipped stored byte must refuse the whole-object read");
    eprintln!(
        "{test}: READ commands {} of {} blocks; status {:?} {:?}; remanence-retry {:?}",
        read.reads,
        read.block_count,
        status.code(),
        status.message(),
        status.metadata().get("remanence-retry")
    );
    assert_eq!(status.code(), Code::DataLoss, "{test}: {status:?}");
    assert!(
        status
            .message()
            .starts_with(&format!("read object: file digest mismatch for {MEMBER_1}")),
        "{test}: {}",
        status.message()
    );
    assert!(
        status.metadata().get("remanence-retry").is_none(),
        "{test}: DATA_LOSS carries no retry metadata"
    );
}

/// Rung D-R, T3 (D1 and D1b): one block per READ and the minimum reservoir, so
/// the decoder fails on member 1 while the submitter is still feeding; the
/// client is sent DATA_LOSS with the digest text.
#[test]
fn whole_object_read_answers_data_loss_when_the_object_spans_read_batches() {
    let read = read_corrupt_object_through_model_drive(1);
    assert!(
        read.reads > 1 && (read.reads as u64) < read.block_count,
        "T3 must span READ batches and stop early: {} of {}",
        read.reads,
        read.block_count
    );
    assert_stored_corrupt_data_loss("T3", read);
}

/// Rung D-R, T5 (D1): the whole object in one READ batch; the digest failure
/// reaches the mapper and the client is sent DATA_LOSS.
#[test]
fn whole_object_read_answers_data_loss_when_the_object_is_one_read_batch() {
    let read = read_corrupt_object_through_model_drive(64);
    assert_eq!(read.reads, 1, "T5 reads the object in one batch");
    assert_stored_corrupt_data_loss("T5", read);
}

/// Names every `FormatError` variant; a new variant fails to compile here
/// until T1 constructs it.
fn format_error_variant(err: &FormatError) -> &'static str {
    match err {
        FormatError::InvalidInput(_) => "InvalidInput",
        FormatError::Layout(_) => "Layout",
        FormatError::Parse(_) => "Parse",
        FormatError::UstarChecksumMismatch { .. } => "UstarChecksumMismatch",
        FormatError::UnsupportedTarTypeflag { .. } => "UnsupportedTarTypeflag",
        FormatError::ChunkAlignmentViolation { .. } => "ChunkAlignmentViolation",
        FormatError::ChunkSizeMismatch { .. } => "ChunkSizeMismatch",
        FormatError::InvalidPath(_) => "InvalidPath",
        FormatError::InvalidHardlinkTarget { .. } => "InvalidHardlinkTarget",
        FormatError::TruncatedPayload => "TruncatedPayload",
        FormatError::PaxRecordMalformed(_) => "PaxRecordMalformed",
        FormatError::Cbor(_) => "Cbor",
        FormatError::ManifestDigestMismatch => "ManifestDigestMismatch",
        FormatError::ManifestInvalid(_) => "ManifestInvalid",
        FormatError::FileDigestMismatch { .. } => "FileDigestMismatch",
        FormatError::InnerObjectMismatch { .. } => "InnerObjectMismatch",
        FormatError::Aead(_) => "Aead",
        FormatError::UnsupportedOperation(_) => "UnsupportedOperation",
        FormatError::UnsupportedFeature(_) => "UnsupportedFeature",
        FormatError::UnsupportedFormatGate { .. } => "UnsupportedFormatGate",
        FormatError::IncompleteBlockWrite { .. } => "IncompleteBlockWrite",
        FormatError::SourceIo { .. } => "SourceIo",
        FormatError::RestoreDestination(_) => "RestoreDestination",
        FormatError::TapeIo(_) => "TapeIo",
    }
}

const FORMAT_ERROR_VARIANTS: [&str; 24] = [
    "InvalidInput",
    "Layout",
    "Parse",
    "UstarChecksumMismatch",
    "UnsupportedTarTypeflag",
    "ChunkAlignmentViolation",
    "ChunkSizeMismatch",
    "InvalidPath",
    "InvalidHardlinkTarget",
    "TruncatedPayload",
    "PaxRecordMalformed",
    "Cbor",
    "ManifestDigestMismatch",
    "ManifestInvalid",
    "FileDigestMismatch",
    "InnerObjectMismatch",
    "Aead",
    "UnsupportedOperation",
    "UnsupportedFeature",
    "UnsupportedFormatGate",
    "IncompleteBlockWrite",
    "SourceIo",
    "RestoreDestination",
    "TapeIo",
];

fn every_format_error() -> Vec<FormatError> {
    use remanence_format::error::FormatGate;
    vec![
        FormatError::invalid("caller input"),
        FormatError::layout("layout overflow"),
        FormatError::parse("bad header"),
        FormatError::UstarChecksumMismatch {
            stored: 4321,
            computed: 4322,
        },
        FormatError::UnsupportedTarTypeflag { typeflag: 0x58 },
        FormatError::ChunkAlignmentViolation {
            path: MEMBER_1.to_string(),
            data_offset: 7,
        },
        FormatError::ChunkSizeMismatch {
            advertised: 512,
            supplied: 4096,
        },
        FormatError::invalid_path("../escape"),
        FormatError::InvalidHardlinkTarget {
            path: "link".to_string(),
            target: "missing".to_string(),
        },
        FormatError::TruncatedPayload,
        FormatError::PaxRecordMalformed("12 path=".to_string()),
        FormatError::cbor("encode"),
        FormatError::ManifestDigestMismatch,
        FormatError::manifest_invalid("files missing"),
        FormatError::file_digest_mismatch(MEMBER_1, &[0x11; 32], &[0x22; 32]),
        FormatError::inner_object_mismatch(FormatGate::ObjectId, "inner object id"),
        FormatError::Aead(remanence_aead::RemObjectAeadError::InvalidMagicBytes),
        FormatError::unsupported("ranged write"),
        FormatError::UnsupportedFeature("sparse files".to_string()),
        FormatError::UnsupportedFormatGate {
            gate: FormatGate::FormatId,
            message: "format id".to_string(),
        },
        FormatError::IncompleteBlockWrite {
            expected_bytes: 512,
            bytes_written: 0,
            early_warning: false,
            end_of_medium: true,
        },
        FormatError::SourceIo {
            context: "write payload".to_string(),
            source: std::io::Error::other("client went away"),
        },
        FormatError::RestoreDestination("exists".to_string()),
        FormatError::TapeIo(remanence_library::TapeIoError::new(
            remanence_library::TapeIoErrorKind::OperationFailed("drive refused".to_string()),
        )),
    ]
}

/// Rung D-R, T1 (D1): the read-path mapper answers DATA_LOSS, with the message
/// unchanged and no retry metadata, for exactly the three stored-byte integrity
/// failures, and delegates every other variant to `status_from_format_error`.
#[test]
fn read_object_mapper_answers_data_loss_only_for_stored_byte_integrity_failures() {
    let errors = every_format_error();
    let constructed = errors.iter().map(format_error_variant).collect::<Vec<_>>();
    assert_eq!(
        constructed, FORMAT_ERROR_VARIANTS,
        "T1 constructs every FormatError variant once"
    );
    for err in &errors {
        let message = format!("read object: {err}");
        let status = status_from_read_object_error(err, message.clone());
        match err {
            FormatError::FileDigestMismatch { .. }
            | FormatError::ManifestDigestMismatch
            | FormatError::UstarChecksumMismatch { .. } => {
                assert_eq!(status.code(), Code::DataLoss, "{err}");
                assert_eq!(status.message(), message, "{err}");
                assert!(
                    status.metadata().is_empty(),
                    "{err}: DATA_LOSS carries no retry metadata: {:?}",
                    status.metadata()
                );
            }
            _ => {
                let shared = status_from_format_error(err, message.clone());
                assert_eq!(status.code(), shared.code(), "{err}");
                assert_eq!(status.message(), shared.message(), "{err}");
                assert_eq!(
                    status.metadata().get("remanence-retry"),
                    shared.metadata().get("remanence-retry"),
                    "{err}"
                );
                assert_eq!(status.metadata().len(), shared.metadata().len(), "{err}");
            }
        }
    }
}
