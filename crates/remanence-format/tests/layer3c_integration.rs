//! Cross-layer smoke tests for composing Layer 3b `rao-v1` with the
//! Layer 3c sidecar-only parity sink.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Cursor;

use remanence_aead::RecipientPrivateKey;
use remanence_format::{
    plan_rem_tar_object, read_encrypted_rao_file_range_to_vec, read_encrypted_rao_object,
    read_rem_tar_object, stream_rem_tar_object, write_encrypted_rao_object, write_rem_tar_object,
    write_rem_tar_object_from_readers, FormatError, RemTarEntrySink, RemTarFile, RemTarFileSpec,
    RemTarFileStream, RemTarObjectOptions, RemTarStreamEntry,
};
use remanence_library::{scsi::ScsiError, TapeIoError, VecBlockSink, VecBlockSource};
use remanence_parity::{
    BlockSinkRawTapeSink, BootstrapObjectRepresentation, BootstrapObjectRow,
    BootstrapObjectRowAdmission, CapacityReserveInput, FilemarkMap, ObjectParitySource, OpenTrust,
    ParityError, ParityScheme, ParitySink, PhysicalPositionHint, RawReadOutcome, RawTapeSource,
    SchemeId, ScopedFilemarkMap, SpaceFilemarksOutcome, TapeFileMapEntry, TapeFilePosition,
};
use sha2::{Digest, Sha256};

const BLOCK_SIZE: u32 = 4096;
const TAPE_UUID: [u8; 16] = [0x3B; 16];

#[test]
fn rem_tar_writer_composes_with_parity_sink_and_reads_back_object_blocks() {
    let opts = options();
    let files = [
        RemTarFile {
            path: "camera/a.txt",
            file_id: "file-a",
            data: b"hello from layer 3b through 3c",
            mtime: Some("0"),
            executable: Some(false),
        },
        RemTarFile {
            path: "vidéo/clip.bin",
            file_id: "file-b",
            data: &[0xA7; 7000],
            mtime: None,
            executable: Some(true),
        },
    ];

    let mut planning_sink = VecBlockSink::new();
    let planned_layout = write_rem_tar_object(&mut planning_sink, &opts, &files)
        .expect("planning fixture writes without parity");

    let mut tape = VecBlockSink::new();
    let layout;
    let close;
    {
        let mut raw = BlockSinkRawTapeSink::new(&mut tape);
        let mut parity = ParitySink::new_sidecar_only(&mut raw, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("parity sink constructs");
        assert_eq!(parity.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            parity
                .begin_object_with_capacity_reserve(capacity_input(
                    planned_layout.projected_size_blocks
                ))
                .expect("object reserve fits")
                .0,
            1
        );
        layout = write_rem_tar_object(&mut parity, &opts, &files)
            .expect("RAO writes through parity sink");
        close = parity.finish_object().expect("parity object closes");
    }

    assert_eq!(
        layout.projected_size_blocks,
        planned_layout.projected_size_blocks
    );
    assert_eq!(close.tape_file_number, 1);
    assert_eq!(close.data_block_count, layout.projected_size_blocks);
    assert_eq!(close.first_parity_data_ordinal, 0);

    let object_start = 1usize; // block 0 is the BOT bootstrap; LBA 1 is its filemark.
    let object_end = object_start + usize::try_from(layout.projected_size_blocks).unwrap();
    let object_blocks = tape.blocks[object_start..object_end].to_vec();
    let mut source = VecBlockSource::new(object_blocks);
    let read = read_rem_tar_object(&mut source, opts.chunk_size, layout.projected_size_blocks)
        .expect("RAO reads object blocks after 3c write");

    assert_eq!(
        read.entry("camera/a.txt").unwrap().data,
        b"hello from layer 3b through 3c"
    );
    assert_eq!(read.entry("vidéo/clip.bin").unwrap().data, vec![0xA7; 7000]);
    assert_eq!(
        read.entry("vidéo/clip.bin").unwrap().first_chunk_lba,
        layout.files[1].first_chunk_lba
    );
}

#[test]
fn streaming_rem_tar_roundtrips_through_parity_object_source() {
    let opts = options();
    let camera = b"streamed through layer 3b into layer 3c".to_vec();
    let empty = Vec::new();
    let clip = vec![0xCD; 9000];
    let specs = vec![
        file_spec("camera/a.txt", "file-a", &camera, Some("0"), Some(false)),
        file_spec("empty.bin", "file-empty", &empty, None, Some(false)),
        file_spec("vidéo/clip.bin", "file-b", &clip, None, Some(true)),
    ];
    let planned_layout = plan_rem_tar_object(&opts, &specs).expect("streaming layout plans");

    let mut camera_reader = Cursor::new(camera.clone());
    let mut empty_reader = Cursor::new(empty.clone());
    let mut clip_reader = Cursor::new(clip.clone());
    let mut streams = [
        RemTarFileStream::new(specs[0].clone(), &mut camera_reader),
        RemTarFileStream::new(specs[1].clone(), &mut empty_reader),
        RemTarFileStream::new(specs[2].clone(), &mut clip_reader),
    ];

    let mut tape = VecBlockSink::new();
    let layout;
    let close;
    {
        let mut raw = BlockSinkRawTapeSink::new(&mut tape);
        let mut parity = ParitySink::new_sidecar_only(&mut raw, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("parity sink constructs");
        assert_eq!(parity.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            parity
                .begin_object_with_capacity_reserve(capacity_input(
                    planned_layout.projected_size_blocks
                ))
                .expect("object reserve fits")
                .0,
            1
        );
        layout = write_rem_tar_object_from_readers(&mut parity, &opts, &mut streams)
            .expect("streaming RAO writes through parity sink");
        close = parity.finish_object().expect("parity object closes");
    }

    assert_eq!(
        layout.projected_size_blocks,
        planned_layout.projected_size_blocks
    );
    assert_eq!(close.tape_file_number, 1);
    assert_eq!(close.data_block_count, layout.projected_size_blocks);
    assert_eq!(camera_reader.position(), camera.len() as u64);
    assert_eq!(empty_reader.position(), 0);
    assert_eq!(clip_reader.position(), clip.len() as u64);

    let scoped = scoped_map_from_close(&close);
    let mut physical = PhysicalVecTapeSource::from_sink(&tape);
    let mut object_source = ObjectParitySource::open(
        &mut physical,
        scheme(),
        TAPE_UUID,
        scoped,
        BLOCK_SIZE,
        close.tape_file_number,
        OpenTrust::RequireValidated,
    )
    .expect("object parity source opens");
    let mut restored = CollectingEntrySink::default();

    let report = stream_rem_tar_object(
        &mut object_source,
        opts.chunk_size,
        layout.projected_size_blocks,
        &mut restored,
    )
    .expect("streaming RAO restores through ObjectParitySource");

    assert_eq!(restored.data.get("camera/a.txt").unwrap(), &camera);
    assert_eq!(restored.data.get("empty.bin").unwrap(), &empty);
    assert_eq!(restored.data.get("vidéo/clip.bin").unwrap(), &clip);
    assert_eq!(
        report
            .entries
            .iter()
            .find(|entry| entry.path == "empty.bin")
            .unwrap()
            .first_chunk_lba,
        None
    );
    assert_eq!(
        report
            .entries
            .iter()
            .find(|entry| entry.path == "vidéo/clip.bin")
            .unwrap()
            .first_chunk_lba,
        layout.files[2].first_chunk_lba
    );
    assert_eq!(report.entries, restored.entries);
    assert_eq!(
        report.manifest_cbor.as_ref().unwrap(),
        &layout.manifest_cbor
    );
}

#[test]
fn encrypted_rao_ciphertext_recovers_through_parity_before_keyed_open() {
    let opts = options();
    let primary = RecipientPrivateKey::new([0x24; 16], "archive-primary", [0x42; 32])
        .expect("primary recipient key");
    let recovery = RecipientPrivateKey::new([0x25; 16], "recovery", [0x43; 32])
        .expect("recovery recipient key");
    let recipients = vec![
        primary.public_key(0).expect("primary public key"),
        recovery.public_key(1).expect("recovery public key"),
    ];
    let photo = vec![0xA5; 18_000];
    let sidecar = b"encrypted parity acceptance".to_vec();
    let files = [
        RemTarFile {
            path: "photos/raw.bin",
            file_id: "photo-file",
            data: &photo,
            mtime: Some("0"),
            executable: Some(false),
        },
        RemTarFile {
            path: "notes/sidecar.txt",
            file_id: "sidecar-file",
            data: &sidecar,
            mtime: None,
            executable: Some(false),
        },
    ];

    let mut planning_sink = VecBlockSink::new();
    let planned_report = write_encrypted_rao_object(&mut planning_sink, &opts, &files, &recipients)
        .expect("encrypted planning fixture writes without parity");
    let planned_ciphertext = planning_sink
        .blocks
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    let pfr = read_encrypted_rao_file_range_to_vec(
        &planned_ciphertext,
        &recovery,
        planned_report.plaintext_layout.files[0].first_chunk_lba,
        photo.len() as u64,
        257,
        8193,
    )
    .expect("recipient-envelope PFR opens through the format funnel");
    assert_eq!(pfr.bytes, photo[257..8450]);

    let mut tape = VecBlockSink::new();
    let report;
    let close;
    {
        let mut raw = BlockSinkRawTapeSink::new(&mut tape);
        let mut parity = ParitySink::new_sidecar_only(&mut raw, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("parity sink constructs");
        assert_eq!(parity.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            parity
                .begin_object_with_capacity_reserve_and_bootstrap_object_row(
                    capacity_input(planned_report.envelope.stored_size_blocks),
                    BootstrapObjectRowAdmission::EncryptedRao,
                )
                .expect("object reserve fits")
                .0,
            1
        );
        report = write_encrypted_rao_object(&mut parity, &opts, &files, &recipients)
            .expect("encrypted RAO writes through parity sink");
        parity
            .record_bootstrap_object_row(
                BootstrapObjectRow::encrypted(
                    1,
                    report.envelope.stored_size_blocks,
                    vec![[0x24; 16], [0x25; 16]],
                    report.envelope.metadata_frame_len,
                    report.envelope.header.key_frame_len,
                )
                .with_object_id([0x77; 16]),
            )
            .expect("encrypted bootstrap row records");
        close = parity.finish_object().expect("parity object closes");
    }

    assert_eq!(
        report.envelope.stored_size_blocks,
        planned_report.envelope.stored_size_blocks
    );
    assert_eq!(close.data_block_count, report.envelope.stored_size_blocks);
    let bootstrap_object_row = close
        .bootstrap_object_row
        .as_ref()
        .expect("encrypted parity close carries bootstrap row");
    assert_eq!(bootstrap_object_row.tape_file_number, 1);
    assert_eq!(
        bootstrap_object_row.stored_block_count,
        report.envelope.stored_size_blocks
    );
    match &bootstrap_object_row.representation {
        BootstrapObjectRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => {
            assert_eq!(recipient_epoch_ids, &vec![[0x24; 16], [0x25; 16]]);
            assert_eq!(*metadata_frame_len, report.envelope.metadata_frame_len);
            assert_eq!(*key_frame_len, report.envelope.header.key_frame_len);
        }
        BootstrapObjectRepresentation::Plaintext { .. } => {
            panic!("encrypted parity write emitted plaintext bootstrap row")
        }
    }
    assert!(
        !close.sidecars_emitted.is_empty(),
        "fixture must emit a completed-epoch sidecar at object close"
    );

    let target_body_lba = 1u64;
    let target_ordinal = close
        .first_parity_data_ordinal
        .checked_add(target_body_lba)
        .expect("target ordinal does not overflow");
    assert!(
        target_ordinal < close.highest_protected_ordinal,
        "target block must be below the protected watermark"
    );

    let scoped = scoped_map_from_close(&close);
    let target_physical_lba = scoped
        .map
        .physical_position(TapeFilePosition {
            tape_file_number: close.tape_file_number,
            block_within_file: target_body_lba,
        })
        .expect("target body LBA maps to physical LBA")
        .lba;
    let target_block_index = tape
        .block_lbas
        .iter()
        .position(|lba| *lba == target_physical_lba)
        .expect("target physical LBA is present in the in-memory tape");

    let mut damaged_blocks = tape.blocks.clone();
    damaged_blocks[target_block_index][17] ^= 0x5A;
    let object_start = 1usize; // block 0 is the BOT bootstrap; LBA 1 is its filemark.
    let object_end = object_start + usize::try_from(report.envelope.stored_size_blocks).unwrap();
    let tampered_object_blocks = damaged_blocks[object_start..object_end].to_vec();
    let mut tampered_source = VecBlockSource::new(tampered_object_blocks);
    read_encrypted_rao_object(
        &mut tampered_source,
        opts.chunk_size,
        report.envelope.stored_size_blocks,
        &primary,
    )
    .expect_err("clean-read ciphertext tamper must fail authentication before repair");

    let mut physical = PhysicalVecTapeSource::from_sink_blocks(&tape, damaged_blocks.clone())
        .with_unreadable_lbas([target_physical_lba]);
    let mut object_source = ObjectParitySource::open(
        &mut physical,
        scheme(),
        TAPE_UUID,
        scoped.clone(),
        BLOCK_SIZE,
        close.tape_file_number,
        OpenTrust::RequireValidated,
    )
    .expect("object parity source opens");
    let recovered = object_source
        .recover_block_at(target_body_lba)
        .expect("ciphertext block recovers without a RAO key");
    assert_eq!(recovered, tape.blocks[target_block_index]);

    let mut physical = PhysicalVecTapeSource::from_sink_blocks(&tape, damaged_blocks)
        .with_unreadable_lbas([target_physical_lba]);
    let mut object_source = ObjectParitySource::open(
        &mut physical,
        scheme(),
        TAPE_UUID,
        scoped,
        BLOCK_SIZE,
        close.tape_file_number,
        OpenTrust::RequireValidated,
    )
    .expect("object parity source opens for keyed read");
    let read = read_encrypted_rao_object(
        &mut object_source,
        opts.chunk_size,
        report.envelope.stored_size_blocks,
        &primary,
    )
    .expect("keyed open succeeds after parity restores ciphertext");

    assert_eq!(read.object.entry("photos/raw.bin").unwrap().data, photo);
    assert_eq!(
        read.object.entry("notes/sidecar.txt").unwrap().data,
        sidecar
    );
}

fn options() -> RemTarObjectOptions {
    let mut opts = RemTarObjectOptions::new(
        "77777777-7777-7777-7777-777777777777",
        "caller-layer3c-integration",
        "2026-05-27T22:20:00+05:30",
        "88888888-8888-8888-8888-888888888888",
    );
    opts.chunk_size = BLOCK_SIZE as usize;
    opts
}

fn scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("format-integration-test"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 2,
    }
}

fn capacity_input(projected_object_blocks: u64) -> CapacityReserveInput {
    CapacityReserveInput {
        projected_object_blocks,
        block_size_bytes: BLOCK_SIZE as u64,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 4,
        parity_shards_per_epoch: 2,
        sidecar_index_block_count: 1,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        bootstrap_filemark_blocks: 1,
        pending_completed_sidecars: 0,
        remaining_bootstrap_count: 1,
        safety_margin_blocks: 4,
        remaining_tape_blocks: 10_000,
        empty_tape_usable_blocks: 10_000,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 10_000_000,
    }
}

fn file_spec(
    path: &str,
    file_id: &str,
    data: &[u8],
    mtime: Option<&str>,
    executable: Option<bool>,
) -> RemTarFileSpec {
    let digest = Sha256::digest(data);
    let mut file_sha256 = [0u8; 32];
    file_sha256.copy_from_slice(&digest);
    let mut spec = RemTarFileSpec::new(
        path.to_string(),
        file_id.to_string(),
        data.len() as u64,
        file_sha256,
    );
    spec.mtime = mtime.map(str::to_string);
    spec.executable = executable;
    spec
}

fn scoped_map_from_close(close: &remanence_parity::ObjectWriteSummary) -> ScopedFilemarkMap {
    let mut entries = vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(
            close.tape_file_number,
            close.data_block_count,
            close.first_parity_data_ordinal,
        ),
    ];
    entries.extend(
        close
            .sidecars_emitted
            .iter()
            .map(|sidecar| sidecar.tape_file_entry().to_map_entry()),
    );
    entries.extend(
        close
            .control_tape_files_emitted
            .iter()
            .map(|entry| entry.to_map_entry()),
    );
    let map = FilemarkMap::new(entries).expect("filemark map from close summary validates");
    ScopedFilemarkMap::from_catalog(map, close.highest_protected_ordinal)
}

#[derive(Default)]
struct CollectingEntrySink {
    active: Option<String>,
    entries: Vec<RemTarStreamEntry>,
    data: BTreeMap<String, Vec<u8>>,
}

impl RemTarEntrySink for CollectingEntrySink {
    fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        assert!(self.active.is_none(), "nested begin_file");
        self.active = Some(entry.path.clone());
        self.entries.push(entry.clone());
        self.data.insert(entry.path.clone(), Vec::new());
        Ok(())
    }

    fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
        let active = self.active.as_ref().expect("active entry");
        self.data
            .get_mut(active)
            .expect("active data")
            .extend_from_slice(bytes);
        Ok(())
    }

    fn end_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        assert_eq!(self.active.as_deref(), Some(entry.path.as_str()));
        self.active = None;
        Ok(())
    }
}

struct PhysicalVecTapeSource {
    blocks_by_lba: HashMap<u64, Vec<u8>>,
    unreadable_lbas: BTreeSet<u64>,
    cursor_lba: u64,
    end_lba: u64,
    configured_block_size: Option<u32>,
}

impl PhysicalVecTapeSource {
    fn from_sink(sink: &VecBlockSink) -> Self {
        Self::from_sink_blocks(sink, sink.blocks.clone())
    }

    fn from_sink_blocks(sink: &VecBlockSink, blocks: Vec<Vec<u8>>) -> Self {
        assert_eq!(
            sink.block_lbas.len(),
            blocks.len(),
            "test fixture block payload count must match recorded LBAs"
        );
        let blocks_by_lba = sink.block_lbas.iter().copied().zip(blocks).collect();
        Self {
            blocks_by_lba,
            unreadable_lbas: BTreeSet::new(),
            cursor_lba: 0,
            end_lba: sink.next_lba(),
            configured_block_size: None,
        }
    }

    fn with_unreadable_lbas(mut self, lbas: impl IntoIterator<Item = u64>) -> Self {
        self.unreadable_lbas.extend(lbas);
        self
    }
}

impl RawTapeSource for PhysicalVecTapeSource {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }
        self.configured_block_size = Some(block_size);
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.cursor_lba = hint.lba;
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        if count < 0 {
            return Err(ParityError::TapeIo(TapeIoError::OperationFailed(
                "backward filemark spacing is not implemented in this fixture".to_string(),
            )));
        }
        let mut remaining = count;
        while remaining > 0 && self.cursor_lba < self.end_lba {
            if self.blocks_by_lba.contains_key(&self.cursor_lba) {
                self.cursor_lba += 1;
                continue;
            }
            self.cursor_lba += 1;
            remaining -= 1;
        }
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: count - remaining,
            position_after: PhysicalPositionHint::new(self.cursor_lba),
            hit_end_of_data: remaining > 0,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        if self.unreadable_lbas.contains(&self.cursor_lba) {
            return Err(ParityError::TapeIo(medium_error()));
        }
        if let Some(block) = self.blocks_by_lba.get(&self.cursor_lba) {
            if buf.len() < block.len() {
                return Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall {
                    actual: block.len() as u32,
                    provided: buf.len() as u32,
                }));
            }
            buf[..block.len()].copy_from_slice(block);
            self.cursor_lba += 1;
            return Ok(RawReadOutcome::Block {
                bytes: block.len(),
                position_after: PhysicalPositionHint::new(self.cursor_lba),
            });
        }
        if self.cursor_lba < self.end_lba {
            self.cursor_lba += 1;
            return Ok(RawReadOutcome::Filemark {
                position_after: PhysicalPositionHint::new(self.cursor_lba),
            });
        }
        Ok(RawReadOutcome::EndOfData {
            position_after: PhysicalPositionHint::new(self.cursor_lba),
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor_lba))
    }
}

fn medium_error() -> TapeIoError {
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = 0x03;
    sense[7] = 24;
    sense[12] = 0x11;
    TapeIoError::CheckCondition(ScsiError::CheckCondition {
        sense,
        bytes_transferred: 0,
    })
}
