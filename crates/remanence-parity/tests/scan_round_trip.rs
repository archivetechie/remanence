//! Integration coverage for v0.4.4 sidecar writer output and catalog-less
//! filemark-map scan reconstruction.

use std::collections::BTreeSet;

use remanence_library::{BlockSink, TapeIoError, VecBlockSink};
use remanence_parity::bootstrap::{
    parse_bootstrap_block, write_bootstrap_block, BootstrapPayload, ParitySchemeRecord,
};
use remanence_parity::{
    acquire_filemark_map, emit_resume_rebuilt_sidecars_to_raw,
    rebuild_legacy_forensic_open_epoch_from_committed_prefix,
    rebuild_open_epoch_from_committed_prefix, scan_reconstruct_filemark_map, BlockSinkRawTapeSink,
    CapacityReserveInput, CatalogFilemarkMapInput, FilemarkMap, ObjectParityState, ParityError,
    ParityScheme, ParitySink, PhysicalPositionHint, RawReadOutcome, RawTapeSink, RawTapeSource,
    RawWriteOutcome, ResumeLiveEpochState, ResumeWriterSeed, SchemeId, ScopedFilemarkMap,
    SidecarEpochDirectoryEntry, SpaceFilemarksOutcome, TapeFileKind, TapeFileMapEntry,
    TapeFilePosition, SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD,
    SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
};

const BLOCK_SIZE: u32 = 512;
const TAPE_UUID: [u8; 16] = [0x5A; 16];

#[derive(Clone, Debug, PartialEq, Eq)]
enum TapeRecord {
    Block(Vec<u8>),
    Filemark,
}

#[derive(Debug)]
struct RawVecTape {
    records: Vec<TapeRecord>,
    cursor: usize,
    configured_block_size: Option<u32>,
    unreadable_lbas: BTreeSet<usize>,
    read_records: usize,
}

struct FailAfterFixedBlocksRawSink<'a> {
    inner: BlockSinkRawTapeSink<'a>,
    fixed_blocks_before_failure: usize,
    fixed_blocks_written: usize,
}

impl<'a> FailAfterFixedBlocksRawSink<'a> {
    fn new(inner: &'a mut dyn BlockSink, fixed_blocks_before_failure: usize) -> Self {
        Self {
            inner: BlockSinkRawTapeSink::new(inner),
            fixed_blocks_before_failure,
            fixed_blocks_written: 0,
        }
    }
}

impl RawTapeSink for FailAfterFixedBlocksRawSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        if self.fixed_blocks_written == self.fixed_blocks_before_failure {
            return Err(ParityError::ResumeAppend(
                "simulated crash before next resume sidecar block".to_string(),
            ));
        }
        self.fixed_blocks_written += 1;
        self.inner.write_fixed_block(buf)
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.inner.write_filemark()
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.position()
    }
}

struct FailOnNthFilemarkRawSink<'a> {
    inner: BlockSinkRawTapeSink<'a>,
    fail_on_filemark_call: usize,
    filemark_calls: usize,
}

impl<'a> FailOnNthFilemarkRawSink<'a> {
    fn new(inner: &'a mut dyn BlockSink, fail_on_filemark_call: usize) -> Self {
        assert!(
            fail_on_filemark_call > 0,
            "filemark failure target is 1-based"
        );
        Self {
            inner: BlockSinkRawTapeSink::new(inner),
            fail_on_filemark_call,
            filemark_calls: 0,
        }
    }
}

impl RawTapeSink for FailOnNthFilemarkRawSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.inner.write_fixed_block(buf)
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.filemark_calls += 1;
        if self.filemark_calls == self.fail_on_filemark_call {
            return Err(ParityError::ResumeAppend(
                "simulated crash during resume sidecar filemark".to_string(),
            ));
        }
        self.inner.write_filemark()
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.inner.position()
    }
}

impl RawVecTape {
    fn from_sink(sink: &VecBlockSink) -> Self {
        let mut records = Vec::new();
        let mut next_lba = 0u64;

        for (block, &block_lba) in sink.blocks.iter().zip(&sink.block_lbas) {
            while next_lba < block_lba {
                records.push(TapeRecord::Filemark);
                next_lba += 1;
            }
            assert_eq!(
                next_lba, block_lba,
                "VecBlockSink block LBAs must be monotonic and gap-free except filemarks"
            );
            records.push(TapeRecord::Block(block.clone()));
            next_lba += 1;
        }

        while next_lba < sink.next_lba() {
            records.push(TapeRecord::Filemark);
            next_lba += 1;
        }

        Self {
            records,
            cursor: 0,
            configured_block_size: None,
            unreadable_lbas: BTreeSet::new(),
            read_records: 0,
        }
    }

    fn with_unreadable_lba(mut self, lba: u64) -> Self {
        self.unreadable_lbas
            .insert(usize::try_from(lba).expect("test LBA fits usize"));
        self
    }
}

fn truncate_sink_at_lba(sink: &VecBlockSink, lba: u64) -> VecBlockSink {
    let raw = RawVecTape::from_sink(sink);
    let mut truncated = VecBlockSink::new();
    for record in raw
        .records
        .into_iter()
        .take(usize::try_from(lba).expect("test LBA fits usize"))
    {
        match record {
            TapeRecord::Block(block) => {
                truncated
                    .write_block(&block)
                    .expect("test prefix block rewrites");
            }
            TapeRecord::Filemark => {
                truncated
                    .write_filemarks(1)
                    .expect("test prefix filemark rewrites");
            }
        }
    }
    assert_eq!(truncated.next_lba(), lba);
    truncated
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

impl RawTapeSource for RawVecTape {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("test block size is zero"));
        }
        self.configured_block_size = Some(block_size);
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.cursor = usize::try_from(hint.lba)
            .map_err(|_| ParityError::Invariant("test LBA does not fit usize"))?
            .min(self.records.len());
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        if count < 0 {
            return Err(ParityError::Invariant(
                "test raw tape supports forward spacing only",
            ));
        }

        let mut spaced = 0i64;
        while spaced < count {
            match self.records.get(self.cursor) {
                Some(TapeRecord::Filemark) => {
                    self.cursor += 1;
                    spaced += 1;
                }
                Some(TapeRecord::Block(_)) => {
                    self.cursor += 1;
                }
                None => {
                    return Ok(SpaceFilemarksOutcome {
                        filemarks_spaced: spaced,
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                        hit_end_of_data: true,
                    });
                }
            }
        }

        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: spaced,
            position_after: PhysicalPositionHint::new(self.cursor as u64),
            hit_end_of_data: false,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.read_records += 1;
        if self.unreadable_lbas.contains(&self.cursor) {
            return Err(ParityError::TapeIo(TapeIoError::OperationFailed(format!(
                "simulated unreadable raw record at LBA {}",
                self.cursor
            ))));
        }

        let Some(record) = self.records.get(self.cursor) else {
            return Ok(RawReadOutcome::EndOfData {
                position_after: PhysicalPositionHint::new(self.cursor as u64),
            });
        };

        match record {
            TapeRecord::Block(block) => {
                if block.len() > buf.len() {
                    return Err(ParityError::Invariant("test read buffer too small"));
                }
                let bytes = block.len();
                buf[..bytes].copy_from_slice(block);
                self.cursor += 1;
                Ok(RawReadOutcome::Block {
                    bytes,
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                })
            }
            TapeRecord::Filemark => {
                self.cursor += 1;
                Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                })
            }
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor as u64))
    }
}

fn scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("scan-round-trip"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 2,
    }
}

fn capacity_input(projected_object_blocks: u64) -> CapacityReserveInput {
    capacity_input_with_current_fill(projected_object_blocks, 0)
}

fn capacity_input_with_current_fill(
    projected_object_blocks: u64,
    current_epoch_fill_blocks: u64,
) -> CapacityReserveInput {
    CapacityReserveInput {
        projected_object_blocks,
        block_size_bytes: BLOCK_SIZE as u64,
        current_epoch_fill_blocks,
        data_shards_per_epoch: 4,
        parity_shards_per_epoch: 2,
        sidecar_index_block_count: 1,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        bootstrap_filemark_blocks: 1,
        pending_completed_sidecars: 0,
        remaining_bootstrap_count: 1,
        safety_margin_blocks: 4,
        remaining_tape_blocks: 1_000,
        empty_tape_usable_blocks: 1_000,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 1_000_000,
    }
}

fn scheme_record() -> ParitySchemeRecord {
    let scheme = scheme();
    ParitySchemeRecord {
        id: scheme.id.as_str().to_string(),
        data_blocks_per_stripe: scheme.data_blocks_per_stripe,
        parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
        stripes_per_neighborhood: scheme.stripes_per_neighborhood,
        no_parity_flag: false,
    }
}

fn bootstrap_block_for_map(map: &FilemarkMap, is_final_map: bool, sequence: u32) -> Vec<u8> {
    let payload = BootstrapPayload {
        scheme: Some(scheme_record()),
        no_parity_flag: false,
        filemark_map_digest: Some(map.digest(is_final_map).expect("map digest builds")),
        tape_uuid: TAPE_UUID,
        written_by_version: "scan-round-trip-test".to_string(),
        written_at: String::new(),
        sequence,
        block_size_bytes: BLOCK_SIZE,
        drive_compression: false,
        sidecar_epoch_directory: None,
        parity_map_reference: None,
        object_rows: Vec::new(),
    };
    let mut block = vec![0u8; BLOCK_SIZE as usize];
    write_bootstrap_block(&payload, &mut block).expect("bootstrap block encodes");
    block
}

fn object_block(seed: u8) -> Vec<u8> {
    let mut block = vec![seed; BLOCK_SIZE as usize];
    block[0] = seed;
    block[1] = seed.wrapping_mul(31);
    block
}

fn committed_object_prefix(object_blocks: u8) -> (FilemarkMap, VecBlockSink) {
    let bot_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, u64::from(object_blocks), 0),
    ])
    .expect("committed prefix validates");

    let mut sink_blocks = VecBlockSink::new();
    sink_blocks
        .write_block(&bootstrap_block_for_map(&bot_prefix, false, 0))
        .expect("BOT bootstrap writes");
    sink_blocks.write_filemarks(1).expect("BOT filemark");
    for seed in 1..=object_blocks {
        sink_blocks
            .write_block(&object_block(seed))
            .expect("committed object block writes");
    }
    sink_blocks
        .write_filemarks(1)
        .expect("committed object filemark writes");

    (committed_prefix, sink_blocks)
}

#[test]
fn crash_after_object_data_before_filemark_truncates_partial_file_from_catalog_prefix() {
    let committed_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");

    let mut crashed_sink_blocks = VecBlockSink::new();
    crashed_sink_blocks
        .write_block(&bootstrap_block_for_map(&committed_prefix, false, 0))
        .expect("BOT bootstrap writes");
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("BOT filemark");
    for seed in 1..=4 {
        crashed_sink_blocks
            .write_block(&object_block(seed))
            .expect("partial object block writes");
    }

    let append_position = PhysicalPositionHint::new(2);
    assert_eq!(crashed_sink_blocks.next_lba(), 6);

    let mut physical_scan = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("partial physical tape file has no trailing filemark");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }
    assert_eq!(committed_prefix.total_data_ordinals(), 0);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("catalog-only prefix ignores partial object without filemark");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 0);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 0);
    assert_eq!(rebuild.plan.next_data_ordinal, 0);
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut resumed_sink_blocks = truncate_sink_at_lba(&crashed_sink_blocks, append_position.lba);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty resume sidecar plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 0);
    assert_eq!(resume_result.live_epoch_start, 0);
    assert_eq!(resume_result.next_data_ordinal, 0);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("partial-file resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("replacement object reserve fits")
                .0,
            1
        );
        for seed in 41..=44 {
            sink.write_block(&object_block(seed))
                .expect("replacement object block writes");
        }
        let object = sink.finish_object().expect("replacement object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("final bootstrap writes");
    }

    let replacement_raw = RawVecTape::from_sink(&resumed_sink_blocks);
    for (offset, seed) in (41..=44).enumerate() {
        let lba = usize::try_from(append_position.lba).expect("test LBA fits usize") + offset;
        assert_eq!(
            replacement_raw.records[lba],
            TapeRecord::Block(object_block(seed)),
            "replacement object must overwrite the partial object at LBA {lba}"
        );
    }

    let final_bootstrap = parse_bootstrap_block(
        resumed_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&resumed_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 4);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 4);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 4);
    assert_eq!(scoped.map.total_data_ordinals(), 4);
}

#[test]
fn crash_after_object_filemark_before_db_commit_uses_catalog_prefix() {
    let committed_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");

    let mut crashed_sink_blocks = VecBlockSink::new();
    crashed_sink_blocks
        .write_block(&bootstrap_block_for_map(&committed_prefix, false, 0))
        .expect("BOT bootstrap writes");
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("BOT filemark");
    for seed in 1..=4 {
        crashed_sink_blocks
            .write_block(&object_block(seed))
            .expect("uncommitted object block writes");
    }
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("uncommitted object filemark writes");

    let append_position = PhysicalPositionHint::new(2);
    assert_eq!(crashed_sink_blocks.next_lba(), 7);

    let mut physical_scan = RawVecTape::from_sink(&crashed_sink_blocks);
    let physical_map = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect("physical tape scans with extra uncommitted object");
    assert_eq!(
        physical_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![TapeFileKind::Bootstrap, TapeFileKind::Object]
    );
    assert_eq!(physical_map.total_data_ordinals(), 4);
    assert_eq!(committed_prefix.total_data_ordinals(), 0);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("catalog-only prefix resumes without rebuilding uncommitted object");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 0);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 0);
    assert_eq!(rebuild.plan.next_data_ordinal, 0);
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());

    let mut resumed_sink_blocks = truncate_sink_at_lba(&crashed_sink_blocks, append_position.lba);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| panic!("catalog-only resume must not commit sidecars"),
        )
        .expect("empty resume sidecar plan completes")
    };
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 0);
    assert_eq!(resume_result.next_data_ordinal, 0);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("catalog-prefix resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("post-crash object reserve fits")
                .0,
            1
        );
        for seed in 11..=14 {
            sink.write_block(&object_block(seed))
                .expect("replacement object block writes");
        }
        let object = sink.finish_object().expect("replacement object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        resumed_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&resumed_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 4);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 4);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 4);
    assert_eq!(scoped.map.total_data_ordinals(), 4);
}

#[test]
fn uncheckpointed_clean_end_resumes_from_journal_prefix_and_tape_scan() {
    let mut uncheckpointed_sink = VecBlockSink::new();
    let object_blocks = (1..=3).map(object_block).collect::<Vec<_>>();
    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut uncheckpointed_sink);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");
        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(object_blocks.len() as u64))
                .expect("object reserve fits")
                .0,
            1
        );
        for block in &object_blocks {
            sink.write_block(block).expect("object block writes");
        }
        let object = sink.finish_object().expect("object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.data_block_count, 3);
        assert!(
            object.sidecars_emitted.is_empty(),
            "three blocks leave a v1 partial epoch, so no sidecar is emitted before checkpoint"
        );
        assert_eq!(object.highest_protected_ordinal, 0);
    }

    assert_eq!(uncheckpointed_sink.next_lba(), 6);
    let journal_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 3, 0),
    ])
    .expect("journal-derived committed prefix validates");

    let mut journal_source = RawVecTape::from_sink(&uncheckpointed_sink);
    let journal_rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut journal_source,
        &journal_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("journal prefix rebuilds partial epoch");
    assert_eq!(journal_rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(
        journal_rebuild
            .plan
            .highest_protected_ordinal_before_rebuild,
        0
    );
    assert_eq!(
        journal_rebuild.plan.highest_protected_ordinal_after_rebuild,
        0
    );
    assert_eq!(journal_rebuild.plan.next_data_ordinal, 3);
    assert!(journal_rebuild.rebuilt_sidecars.is_empty());
    assert_resume_live_epoch_carries_blocks(
        journal_rebuild.live_epoch.as_ref(),
        &object_blocks,
        "journal",
    );

    let mut scan_source = RawVecTape::from_sink(&uncheckpointed_sink);
    let scanned_prefix = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
        .expect("catalog-less scan accepts complete object after old bootstrap");
    assert_eq!(scanned_prefix, journal_prefix);

    let mut tape_only_source = RawVecTape::from_sink(&uncheckpointed_sink);
    let tape_only_rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut tape_only_source,
        &scanned_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("tape-only prefix rebuilds the same partial epoch");
    assert_eq!(tape_only_rebuild.plan, journal_rebuild.plan);
    assert_resume_live_epoch_carries_blocks(
        tape_only_rebuild.live_epoch.as_ref(),
        &object_blocks,
        "tape-only",
    );

    let resume_result = tape_only_rebuild
        .plan
        .clone()
        .complete(Vec::new())
        .expect("partial-epoch rebuild emits no sidecars");
    let append_position = tape_only_rebuild.plan.append_position;
    let mut resumed_sink = truncate_sink_at_lba(&uncheckpointed_sink, append_position.lba);
    assert_eq!(resumed_sink.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &scanned_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &scanned_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: tape_only_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("uncheckpointed-prefix resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(1, 3))
                .expect("post-resume reserve fits")
                .0,
            2
        );
        sink.write_block(&object_block(4))
            .expect("post-resume block writes");
        let object = sink.finish_object().expect("post-resume object closes");
        assert_eq!(object.tape_file_number, 2);
        assert_eq!(object.first_parity_data_ordinal, 3);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 3);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&resumed_sink);
    for (offset, block) in object_blocks.iter().enumerate() {
        let lba = 2 + offset;
        assert_eq!(
            resumed_raw.records[lba],
            TapeRecord::Block(block.clone()),
            "original uncheckpointed object block at LBA {lba} must survive resume"
        );
    }

    let final_bootstrap =
        parse_bootstrap_block(resumed_sink.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);
    assert_eq!(final_digest.tape_file_count, 5);
    assert_eq!(final_digest.map_total_data_ordinals, 4);
    assert_eq!(final_digest.highest_protected_ordinal, 4);

    let mut final_scan = RawVecTape::from_sink(&resumed_sink);
    let final_map = scan_reconstruct_filemark_map(&mut final_scan, &TAPE_UUID, BLOCK_SIZE).unwrap();
    assert_eq!(
        final_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    let scoped = ScopedFilemarkMap::validate_against_digest(final_map, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 4);
    assert_eq!(scoped.map.total_data_ordinals(), 4);
}

fn assert_resume_live_epoch_carries_blocks(
    live_epoch: Option<&ResumeLiveEpochState>,
    expected_blocks: &[Vec<u8>],
    path_name: &str,
) {
    let live_epoch = live_epoch
        .unwrap_or_else(|| panic!("{path_name} resume should carry the partial epoch live"));
    assert_eq!(live_epoch.protected_ordinal_start, 0);
    assert_eq!(live_epoch.next_data_ordinal, expected_blocks.len() as u64);
    assert_eq!(
        live_epoch.data_blocks_in_epoch,
        expected_blocks.len() as u64
    );
    let expected_stripes = vec![
        vec![expected_blocks[0].clone(), expected_blocks[2].clone()],
        vec![expected_blocks[1].clone()],
    ];
    assert_eq!(live_epoch.stripe_buffers, expected_stripes);
}

#[test]
fn crash_after_second_object_data_before_filemark_preserves_committed_prefix() {
    let mut crashed_sink_blocks = VecBlockSink::new();
    let committed_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut crashed_sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("first object reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed))
                .expect("committed object block writes");
        }
        let object = sink.finish_object().expect("committed object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );
        object.sidecars_emitted[0].clone()
    };

    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            committed_sidecar.block_count,
            committed_sidecar.epoch_id,
            committed_sidecar.protected_ordinal_start,
            committed_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("committed prefix validates");
    let append_position = PhysicalPositionHint::new(crashed_sink_blocks.next_lba());
    assert_eq!(
        append_position.lba,
        1 + 1 + 4 + 1 + committed_sidecar.block_count + 1
    );

    for seed in 21..=24 {
        crashed_sink_blocks
            .write_block(&object_block(seed))
            .expect("partial second-object block writes");
    }
    assert_eq!(crashed_sink_blocks.next_lba(), append_position.lba + 4);

    let mut physical_scan = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("partial second object must not scan as a committed object");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("committed prefix ignores partial second object without filemark");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 4);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 4);
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut resumed_sink_blocks = truncate_sink_at_lba(&crashed_sink_blocks, append_position.lba);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty resume sidecar plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 4);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("non-empty-prefix partial-object resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("replacement object reserve fits")
                .0,
            3
        );
        for seed in 31..=34 {
            sink.write_block(&object_block(seed))
                .expect("replacement object block writes");
        }
        let object = sink.finish_object().expect("replacement object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("final bootstrap writes");
    }

    let replacement_raw = RawVecTape::from_sink(&resumed_sink_blocks);
    for (offset, seed) in (31..=34).enumerate() {
        let lba = usize::try_from(append_position.lba).expect("test LBA fits usize") + offset;
        assert_eq!(
            replacement_raw.records[lba],
            TapeRecord::Block(object_block(seed)),
            "replacement object must overwrite the partial second object at LBA {lba}"
        );
    }

    let final_bootstrap = parse_bootstrap_block(
        resumed_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&resumed_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn crash_after_second_object_filemark_before_db_commit_preserves_committed_prefix() {
    let mut crashed_sink_blocks = VecBlockSink::new();
    let committed_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut crashed_sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("first object reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed))
                .expect("committed object block writes");
        }
        let object = sink.finish_object().expect("committed object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );
        object.sidecars_emitted[0].clone()
    };

    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            committed_sidecar.block_count,
            committed_sidecar.epoch_id,
            committed_sidecar.protected_ordinal_start,
            committed_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("committed prefix validates");
    let append_position = PhysicalPositionHint::new(crashed_sink_blocks.next_lba());
    assert_eq!(
        append_position.lba,
        1 + 1 + 4 + 1 + committed_sidecar.block_count + 1
    );

    for seed in 21..=24 {
        crashed_sink_blocks
            .write_block(&object_block(seed))
            .expect("orphan object block writes");
    }
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("orphan object filemark writes");
    assert!(crashed_sink_blocks.next_lba() > append_position.lba);

    let mut physical_scan = RawVecTape::from_sink(&crashed_sink_blocks);
    let physical_map = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect("physical tape scans with orphan second object");
    assert_eq!(
        physical_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
        ]
    );
    assert_eq!(physical_map.total_data_ordinals(), 8);
    assert_eq!(physical_map.max_sidecar_end_exclusive(), 4);
    assert_eq!(committed_prefix.total_data_ordinals(), 4);
    assert_eq!(committed_prefix.max_sidecar_end_exclusive(), 4);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("non-empty catalog prefix resumes without reading orphan object");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 4);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 4);
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut resumed_sink_blocks = truncate_sink_at_lba(&crashed_sink_blocks, append_position.lba);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty resume sidecar plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 4);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("non-empty-prefix resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("replacement object reserve fits")
                .0,
            3
        );
        for seed in 31..=34 {
            sink.write_block(&object_block(seed))
                .expect("replacement object block writes");
        }
        let object = sink.finish_object().expect("replacement object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("final bootstrap writes");
    }

    let second_object_lba = append_position.lba;
    let replacement_raw = RawVecTape::from_sink(&resumed_sink_blocks);
    for (offset, seed) in (31..=34).enumerate() {
        let lba = usize::try_from(second_object_lba).expect("test LBA fits usize") + offset;
        assert_eq!(
            replacement_raw.records[lba],
            TapeRecord::Block(object_block(seed)),
            "replacement object must overwrite the orphan object at LBA {lba}"
        );
    }

    let final_bootstrap = parse_bootstrap_block(
        resumed_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&resumed_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn crash_after_object_db_commit_appends_after_object_and_carries_live_epoch() {
    let (committed_prefix, mut sink_blocks) = committed_object_prefix(2);
    assert_eq!(sink_blocks.next_lba(), 5);

    let mut physical_scan = RawVecTape::from_sink(&sink_blocks);
    let physical_map = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect("object-committed physical tape scans");
    assert_eq!(physical_map, committed_prefix);
    assert_eq!(physical_map.total_data_ordinals(), 2);
    assert_eq!(physical_map.max_sidecar_end_exclusive(), 0);

    let mut raw_source = RawVecTape::from_sink(&sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("object-committed resume rebuild succeeds");

    let append_position = PhysicalPositionHint::new(5);
    assert_eq!(rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 0);
    assert_eq!(rebuild.plan.live_epoch_start, 0);
    assert_eq!(rebuild.plan.next_data_ordinal, 2);
    assert!(rebuild.plan.sidecars_to_emit.is_empty());
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_some());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty object-committed resume sidecar plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 0);
    assert_eq!(resume_result.live_epoch_start, 0);
    assert_eq!(resume_result.next_data_ordinal, 2);
    assert_eq!(sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("object-committed resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-commit resume reserve fits")
                .0,
            2
        );
        for seed in 21..=22 {
            sink.write_block(&object_block(seed))
                .expect("post-commit resumed object block writes");
        }
        let object = sink
            .finish_object()
            .expect("post-commit resumed object closes");
        assert_eq!(object.tape_file_number, 2);
        assert_eq!(object.first_parity_data_ordinal, 2);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 3);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&sink_blocks);
    assert_eq!(
        resumed_raw.records[usize::try_from(append_position.lba).unwrap()],
        TapeRecord::Block(object_block(21)),
        "first resumed object block must start at the post-commit append point"
    );
    assert_eq!(
        resumed_raw.records[usize::try_from(append_position.lba + 1).unwrap()],
        TapeRecord::Block(object_block(22)),
        "second resumed object block must continue at the append point"
    );

    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 4);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 4);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 4);
    assert_eq!(scoped.map.total_data_ordinals(), 4);
}

#[test]
fn crash_after_sidecar_filemark_before_db_commit_truncates_extra_sidecar() {
    let mut crashed_sink_blocks = VecBlockSink::new();
    let orphan_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut crashed_sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(6))
                .expect("reserve fits")
                .0,
            1
        );
        for seed in 1..=6 {
            sink.write_block(&object_block(seed))
                .expect("object block writes");
        }
        let object = sink.finish_object().expect("object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.data_block_count, 6);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );
        object.sidecars_emitted[0].clone()
    };

    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 6, 0),
    ])
    .expect("object-committed prefix validates");
    let append_position = PhysicalPositionHint::new(1 + 1 + 6 + 1);
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        append_position.lba + orphan_sidecar.block_count + 1
    );

    let mut physical_scan = RawVecTape::from_sink(&crashed_sink_blocks);
    let physical_map = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect("physical tape scans with extra uncommitted sidecar");
    assert_eq!(
        physical_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
        ]
    );
    assert_eq!(physical_map.max_sidecar_end_exclusive(), 4);
    assert_eq!(physical_map.total_data_ordinals(), 6);
    assert_eq!(committed_prefix.max_sidecar_end_exclusive(), 0);
    assert_eq!(committed_prefix.total_data_ordinals(), 6);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("catalog prefix ignores the extra uncommitted sidecar");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 6);
    assert_eq!(rebuild.rebuilt_sidecars.len(), 1);
    assert!(rebuild.live_epoch.is_some());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let rebuilt_sidecar = &rebuild.rebuilt_sidecars[0];
    assert_eq!(rebuild.plan.append_after_tape_file_number + 1, 2);
    assert_eq!(rebuilt_sidecar.plan.protected_ordinal_start, 0);
    assert_eq!(rebuilt_sidecar.plan.protected_ordinal_end_exclusive, 4);
    assert_eq!(
        u64::try_from(rebuilt_sidecar.encoded.blocks.len()).expect("sidecar block count fits"),
        orphan_sidecar.block_count
    );
    let crashed_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    for (offset, block) in rebuilt_sidecar.encoded.blocks.iter().enumerate() {
        assert_eq!(
            crashed_raw.records[usize::try_from(append_position.lba).unwrap() + offset],
            TapeRecord::Block(block.clone()),
            "orphan sidecar block {offset} should match the deterministic rebuild"
        );
    }

    let mut resumed_sink_blocks = truncate_sink_at_lba(&crashed_sink_blocks, append_position.lba);
    assert_eq!(resumed_sink_blocks.next_lba(), append_position.lba);

    let mut committed_resume_sidecars = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_resume_sidecars.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("rebuilt sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_resume_sidecars);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.sidecars_emitted[0].tape_file_number, 2);
    assert_eq!(resume_result.sidecars_emitted[0].protected_ordinal_start, 0);
    assert_eq!(
        resume_result.sidecars_emitted[0].protected_ordinal_end_exclusive,
        4
    );
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 6);

    let resume_sidecar = &resume_result.sidecars_emitted[0];
    let prefix_after_resume = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 6, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            resume_sidecar.block_count,
            resume_sidecar.epoch_id,
            resume_sidecar.protected_ordinal_start,
            resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-resume sidecar prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut resumed_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_resume,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_resume,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("post-sidecar-crash resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("resume continuation reserve fits")
                .0,
            3
        );
        for seed in 71..=72 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 6);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&resumed_sink_blocks);
    assert_eq!(
        resumed_raw.records[usize::try_from(append_position.lba).unwrap()],
        TapeRecord::Block(rebuilt_sidecar.encoded.blocks[0].clone()),
        "resume must overwrite from the catalog append point"
    );
    assert_eq!(
        resumed_raw.records
            [usize::try_from(append_position.lba + resume_sidecar.block_count + 1).unwrap()],
        TapeRecord::Block(object_block(71)),
        "new object data must start after the committed rebuilt sidecar"
    );

    let final_bootstrap = parse_bootstrap_block(
        resumed_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&resumed_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);
    assert_eq!(
        ObjectParityState::from_ordinals(0, 6, reconstructed.max_sidecar_end_exclusive()).unwrap(),
        ObjectParityState::Protected
    );
    assert_eq!(
        ObjectParityState::from_ordinals(6, 2, reconstructed.max_sidecar_end_exclusive()).unwrap(),
        ObjectParityState::Protected
    );

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn crash_after_sidecar_db_commit_appends_after_sidecar_and_preserves_partial_state() {
    let mut sink_blocks = VecBlockSink::new();
    let committed_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(6))
                .expect("reserve fits")
                .0,
            1
        );
        for seed in 1..=6 {
            sink.write_block(&object_block(seed)).expect("object block");
        }
        let object = sink.finish_object().expect("object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.data_block_count, 6);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );
        object.sidecars_emitted[0].clone()
    };

    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 6, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            committed_sidecar.block_count,
            committed_sidecar.epoch_id,
            committed_sidecar.protected_ordinal_start,
            committed_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("sidecar-committed prefix validates");
    let committed_watermark = committed_prefix.max_sidecar_end_exclusive();
    assert_eq!(committed_watermark, 4);
    assert_eq!(
        ObjectParityState::from_ordinals(0, 6, committed_watermark).unwrap(),
        ObjectParityState::Partial,
        "the committed sidecar protects only the object's prefix"
    );

    let append_position = PhysicalPositionHint::new(sink_blocks.next_lba());
    assert_eq!(
        append_position.lba,
        1 + 1 + 6 + 1 + committed_sidecar.block_count + 1
    );

    let mut physical_scan = RawVecTape::from_sink(&sink_blocks);
    let physical_map = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect("sidecar-committed physical tape scans");
    assert_eq!(physical_map, committed_prefix);
    assert_eq!(physical_map.total_data_ordinals(), 6);
    assert_eq!(physical_map.max_sidecar_end_exclusive(), 4);

    let mut raw_source = RawVecTape::from_sink(&sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("sidecar-committed resume rebuild succeeds");

    assert_eq!(rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 4);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 6);
    assert!(rebuild.plan.sidecars_to_emit.is_empty());
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_some());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty sidecar-committed resume plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 6);
    assert_eq!(sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("sidecar-committed resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-sidecar-commit reserve fits")
                .0,
            3
        );
        for seed in 61..=62 {
            sink.write_block(&object_block(seed))
                .expect("post-sidecar-commit object block writes");
        }
        let object = sink.finish_object().expect("post-sidecar object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 6);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&sink_blocks);
    assert_eq!(
        resumed_raw.records[usize::try_from(append_position.lba).unwrap()],
        TapeRecord::Block(object_block(61)),
        "first resumed object block must start after the committed sidecar"
    );
    assert_eq!(
        resumed_raw.records[usize::try_from(append_position.lba + 1).unwrap()],
        TapeRecord::Block(object_block(62)),
        "second resumed object block must continue after the committed sidecar"
    );

    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);
    assert_eq!(
        ObjectParityState::from_ordinals(0, 6, reconstructed.max_sidecar_end_exclusive()).unwrap(),
        ObjectParityState::Protected
    );
    assert_eq!(
        ObjectParityState::from_ordinals(6, 2, reconstructed.max_sidecar_end_exclusive()).unwrap(),
        ObjectParityState::Protected
    );

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn crash_mid_sidecar_truncates_provisional_file_and_rebuilds_from_catalog_prefix() {
    let (committed_prefix, base_sink_blocks) = committed_object_prefix(4);
    let append_position = PhysicalPositionHint::new(base_sink_blocks.next_lba());
    assert_eq!(append_position, PhysicalPositionHint::new(7));

    let mut clean_source = RawVecTape::from_sink(&base_sink_blocks);
    let clean_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut clean_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("committed object rebuilds one sidecar");
    assert_eq!(clean_rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(clean_rebuild.plan.append_position, append_position);
    assert_eq!(
        clean_rebuild.plan.highest_protected_ordinal_before_rebuild,
        0
    );
    assert_eq!(
        clean_rebuild.plan.highest_protected_ordinal_after_rebuild,
        4
    );
    assert_eq!(clean_rebuild.rebuilt_sidecars.len(), 1);
    assert!(clean_rebuild.live_epoch.is_none());
    assert!(
        clean_rebuild.rebuilt_sidecars[0].encoded.blocks.len() > 1,
        "test must model a sidecar file with body bytes before the missing filemark"
    );

    let expected_sidecar = clean_rebuild.rebuilt_sidecars[0].clone();
    let mut physical_with_partial_sidecar =
        truncate_sink_at_lba(&base_sink_blocks, append_position.lba);
    physical_with_partial_sidecar
        .write_block(&expected_sidecar.encoded.blocks[0])
        .expect("partial sidecar header block reaches tape");
    let mut damaged_sidecar_body_block = expected_sidecar.encoded.blocks[1].clone();
    damaged_sidecar_body_block[0] ^= 0xA5;
    physical_with_partial_sidecar
        .write_block(&damaged_sidecar_body_block)
        .expect("partial sidecar body block reaches tape");
    assert_eq!(
        physical_with_partial_sidecar.next_lba(),
        append_position.lba + 2
    );

    let mut physical_scan = RawVecTape::from_sink(&physical_with_partial_sidecar);
    let err = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("partial sidecar tape file has no trailing filemark");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(message.contains("physical LBA 7"), "{message}");
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let mut resume_source = RawVecTape::from_sink(&physical_with_partial_sidecar);
    let rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut resume_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("resume ignores the uncommitted partial sidecar suffix");
    assert_eq!(rebuild.plan, clean_rebuild.plan);
    assert_eq!(rebuild.rebuilt_sidecars, vec![expected_sidecar.clone()]);
    assert_eq!(resume_source.position().unwrap(), append_position);

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&physical_with_partial_sidecar, append_position.lba);
    assert_eq!(retried_sink_blocks.next_lba(), append_position.lba);

    let mut committed_resume_sidecars = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_resume_sidecars.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("rebuilt sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_resume_sidecars);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 4);

    let rebuilt_raw = RawVecTape::from_sink(&retried_sink_blocks);
    for (offset, block) in expected_sidecar.encoded.blocks.iter().enumerate() {
        let lba = usize::try_from(append_position.lba).unwrap() + offset;
        assert_eq!(
            rebuilt_raw.records[lba],
            TapeRecord::Block(block.clone()),
            "rebuilt sidecar block {offset} must overwrite the partial sidecar suffix"
        );
    }
    assert_ne!(
        rebuilt_raw.records[usize::try_from(append_position.lba).unwrap() + 1],
        TapeRecord::Block(damaged_sidecar_body_block),
        "the partial sidecar bytes must not survive retry"
    );

    let resume_sidecar = &resume_result.sidecars_emitted[0];
    let prefix_after_resume = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            resume_sidecar.block_count,
            resume_sidecar.epoch_id,
            resume_sidecar.protected_ordinal_start,
            resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-resume sidecar prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_resume,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_resume,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("post-mid-sidecar-crash resumed sink constructs");

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 4);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 4);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 4);
    assert_eq!(scoped.map.total_data_ordinals(), 4);
}

#[test]
fn crash_after_object_filemark_before_sidecar_cluster_rebuilds_open_epoch() {
    let bot_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 6, 0),
    ])
    .expect("committed prefix validates");

    let mut sink_blocks = VecBlockSink::new();
    sink_blocks
        .write_block(&bootstrap_block_for_map(&bot_prefix, false, 0))
        .expect("BOT bootstrap writes");
    sink_blocks.write_filemarks(1).expect("BOT filemark");
    for seed in 1..=6 {
        sink_blocks
            .write_block(&object_block(seed))
            .expect("committed object block writes");
    }
    sink_blocks
        .write_filemarks(1)
        .expect("committed object filemark writes");
    assert_eq!(sink_blocks.next_lba(), 9);

    let mut raw_source = RawVecTape::from_sink(&sink_blocks);
    let rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("resume rebuild succeeds");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(rebuild.plan.append_position, PhysicalPositionHint::new(9));
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 6);
    assert_eq!(rebuild.rebuilt_sidecars.len(), 1);

    let mut committed_resume_sidecars = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_resume_sidecars.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("resume sidecar writes and commits")
    };

    assert_eq!(resume_result.sidecars_emitted, committed_resume_sidecars);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    let resume_sidecar = &resume_result.sidecars_emitted[0];
    assert_eq!(resume_sidecar.tape_file_number, 2);
    assert_eq!(resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(resume_sidecar.protected_ordinal_end_exclusive, 4);

    let live_epoch = rebuild.live_epoch;
    let prefix_after_resume = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 6, 0),
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
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_resume,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_resume,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("resume continuation reserve fits")
                .0,
            3
        );
        for seed in 7..=8 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 6);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");

    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn crash_after_first_sidecar_in_cluster_appends_after_committed_sidecar() {
    let bot_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
    ])
    .expect("committed prefix validates");

    let mut crashed_sink_blocks = VecBlockSink::new();
    crashed_sink_blocks
        .write_block(&bootstrap_block_for_map(&bot_prefix, false, 0))
        .expect("BOT bootstrap writes");
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("BOT filemark");
    for seed in 1..=10 {
        crashed_sink_blocks
            .write_block(&object_block(seed))
            .expect("committed object block writes");
    }
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("committed object filemark writes");
    assert_eq!(crashed_sink_blocks.next_lba(), 13);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial resume rebuild succeeds");
    assert_eq!(initial_rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(
        initial_rebuild.plan.append_position,
        PhysicalPositionHint::new(13)
    );
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 2);
    assert!(initial_rebuild.live_epoch.is_some());

    let first_sidecar_block_count = initial_rebuild.rebuilt_sidecars[0].encoded.blocks.len();
    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink =
            FailAfterFixedBlocksRawSink::new(&mut crashed_sink_blocks, first_sidecar_block_count);
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash before the second sidecar block aborts resume");

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains("resume sidecar 3 block 0 write failed before filemark"),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash before next resume sidecar block"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert_eq!(committed_before_crash.len(), 1);
    let first_resume_sidecar = &committed_before_crash[0];
    assert_eq!(first_resume_sidecar.tape_file_number, 2);
    assert_eq!(first_resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(first_resume_sidecar.protected_ordinal_end_exclusive, 4);
    let retry_append_position = PhysicalPositionHint::new(
        initial_rebuild.plan.append_position.lba + first_resume_sidecar.block_count + 1,
    );
    assert_eq!(crashed_sink_blocks.next_lba(), retry_append_position.lba);

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let physical_map = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect("physical tape with first committed sidecar scans");
    assert_eq!(
        physical_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
        ]
    );
    assert_eq!(physical_map.max_sidecar_end_exclusive(), 4);
    assert_eq!(physical_map.total_data_ordinals(), 10);

    let retry_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("retry committed prefix validates");

    let mut retry_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &retry_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed sidecar");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_ne!(
        retry_rebuild.plan.append_position, initial_rebuild.plan.append_position,
        "retry must not append back at the object boundary"
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        4
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        8
    );
    assert_eq!(retry_rebuild.plan.live_epoch_start, 8);
    assert_eq!(retry_rebuild.plan.next_data_ordinal, 10);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 1);
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[0].plan,
        initial_rebuild.rebuilt_sidecars[1].plan
    );
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut crashed_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("remaining sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.highest_protected_ordinal, 8);
    assert_eq!(resume_result.live_epoch_start, 8);
    assert_eq!(resume_result.next_data_ordinal, 10);

    let retry_sidecar = &resume_result.sidecars_emitted[0];
    assert_eq!(retry_sidecar.tape_file_number, 3);
    assert_eq!(retry_sidecar.protected_ordinal_start, 4);
    assert_eq!(retry_sidecar.protected_ordinal_end_exclusive, 8);
    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            retry_sidecar.block_count,
            retry_sidecar.epoch_id,
            retry_sidecar.protected_ordinal_start,
            retry_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut crashed_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("sidecar-cluster resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-sidecar-cluster reserve fits")
                .0,
            4
        );
        for seed in 11..=12 {
            sink.write_block(&object_block(seed))
                .expect("post-sidecar-cluster object block writes");
        }
        let object = sink
            .finish_object()
            .expect("post-sidecar-cluster object closes");
        assert_eq!(object.tape_file_number, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 5);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 8);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            12
        );

        sink.finish().expect("final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let first_continued_block_lba = retry_append_position.lba + retry_sidecar.block_count + 1;
    assert_eq!(
        resumed_raw.records[usize::try_from(first_continued_block_lba).unwrap()],
        TapeRecord::Block(object_block(11)),
        "new object data must start after the committed retry sidecar"
    );

    let final_bootstrap = parse_bootstrap_block(
        crashed_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 12);
    assert_eq!(
        ObjectParityState::from_ordinals(0, 10, reconstructed.max_sidecar_end_exclusive()).unwrap(),
        ObjectParityState::Protected
    );
    assert_eq!(
        ObjectParityState::from_ordinals(10, 2, reconstructed.max_sidecar_end_exclusive()).unwrap(),
        ObjectParityState::Protected
    );

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 12);
    assert_eq!(scoped.map.total_data_ordinals(), 12);
}

#[test]
fn crash_mid_first_sidecar_in_cluster_truncates_to_object_boundary() {
    let (committed_prefix, mut crashed_sink_blocks) = committed_object_prefix(14);
    assert_eq!(crashed_sink_blocks.next_lba(), 17);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial three-sidecar resume rebuild succeeds");
    assert_eq!(
        initial_rebuild.plan.append_position,
        PhysicalPositionHint::new(17)
    );
    assert_eq!(initial_rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 3);
    assert!(initial_rebuild.rebuilt_sidecars[0].encoded.blocks.len() > 1);
    assert!(initial_rebuild.live_epoch.is_some());

    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink = FailAfterFixedBlocksRawSink::new(&mut crashed_sink_blocks, 1);
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash after the first first-sidecar block aborts resume");

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains("resume sidecar 2 block 1 write failed before filemark"),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash before next resume sidecar block"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert!(
        committed_before_crash.is_empty(),
        "no resume sidecar may commit before sidecar 1's filemark"
    );
    let retry_append_position = initial_rebuild.plan.append_position;
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        retry_append_position.lba + 1
    );

    let partial_first_sidecar_block = {
        let raw = RawVecTape::from_sink(&crashed_sink_blocks);
        raw.records[usize::try_from(retry_append_position.lba).unwrap()].clone()
    };
    assert_eq!(
        partial_first_sidecar_block,
        TapeRecord::Block(initial_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "the crash fixture must leave exactly the first block of sidecar 1 on tape"
    );

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("partial first sidecar must not scan as a committed sidecar");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", retry_append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&crashed_sink_blocks, retry_append_position.lba);
    let mut retry_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed object");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        0
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        12
    );
    assert_eq!(retry_rebuild.plan.live_epoch_start, 12);
    assert_eq!(retry_rebuild.plan.next_data_ordinal, 14);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 3);
    for (retry_sidecar, initial_sidecar) in retry_rebuild
        .rebuilt_sidecars
        .iter()
        .zip(&initial_rebuild.rebuilt_sidecars)
    {
        assert_eq!(retry_sidecar.plan, initial_sidecar.plan);
    }
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry all sidecars writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 3);
    assert_eq!(resume_result.highest_protected_ordinal, 12);
    assert_eq!(resume_result.live_epoch_start, 12);
    assert_eq!(resume_result.next_data_ordinal, 14);

    let retry_first_sidecar = &resume_result.sidecars_emitted[0];
    let retry_second_sidecar = &resume_result.sidecars_emitted[1];
    let retry_third_sidecar = &resume_result.sidecars_emitted[2];
    assert_eq!(retry_first_sidecar.tape_file_number, 2);
    assert_eq!(retry_first_sidecar.protected_ordinal_start, 0);
    assert_eq!(retry_first_sidecar.protected_ordinal_end_exclusive, 4);
    assert_eq!(retry_second_sidecar.tape_file_number, 3);
    assert_eq!(retry_second_sidecar.protected_ordinal_start, 4);
    assert_eq!(retry_second_sidecar.protected_ordinal_end_exclusive, 8);
    assert_eq!(retry_third_sidecar.tape_file_number, 4);
    assert_eq!(retry_third_sidecar.protected_ordinal_start, 8);
    assert_eq!(retry_third_sidecar.protected_ordinal_end_exclusive, 12);

    let retried_raw = RawVecTape::from_sink(&retried_sink_blocks);
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "retry must overwrite the partial first-sidecar header at the catalog append point"
    );
    let retry_second_sidecar_lba = retry_append_position.lba + retry_first_sidecar.block_count + 1;
    assert_eq!(
        retried_raw.records[usize::try_from(retry_second_sidecar_lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[1].encoded.blocks[0].clone()),
        "retry must continue with sidecar 2 after sidecar 1's durable filemark"
    );

    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            retry_first_sidecar.block_count,
            retry_first_sidecar.epoch_id,
            retry_first_sidecar.protected_ordinal_start,
            retry_first_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            retry_second_sidecar.block_count,
            retry_second_sidecar.epoch_id,
            retry_second_sidecar.protected_ordinal_start,
            retry_second_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            4,
            retry_third_sidecar.block_count,
            retry_third_sidecar.epoch_id,
            retry_third_sidecar.protected_ordinal_start,
            retry_third_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("first-sidecar-crash retry resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-first-sidecar-crash reserve fits")
                .0,
            5
        );
        for seed in 15..=16 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 5);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 6);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 12);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            16
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 16);
    assert_eq!(reconstructed.total_data_ordinals(), 16);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 16);
    assert_eq!(scoped.map.total_data_ordinals(), 16);
}

#[test]
fn crash_on_first_resume_sidecar_filemark_truncates_to_object_boundary() {
    let (committed_prefix, mut crashed_sink_blocks) = committed_object_prefix(14);
    assert_eq!(crashed_sink_blocks.next_lba(), 17);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial three-sidecar resume rebuild succeeds");
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 3);
    assert!(initial_rebuild.live_epoch.is_some());

    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink = FailOnNthFilemarkRawSink::new(&mut crashed_sink_blocks, 1);
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash on the first sidecar filemark aborts resume");
        assert_eq!(raw_sink.filemark_calls, 1);

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains(
                        "resume sidecar 2 synchronous filemark failed before catalog commit"
                    ),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash during resume sidecar filemark"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert!(
        committed_before_crash.is_empty(),
        "sidecar 1 must not commit when its trailing filemark fails"
    );

    let retry_append_position = initial_rebuild.plan.append_position;
    let first_sidecar_block_count = initial_rebuild.rebuilt_sidecars[0].encoded.blocks.len();
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        retry_append_position.lba + u64::try_from(first_sidecar_block_count).unwrap()
    );

    let crashed_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    for (offset, block) in initial_rebuild.rebuilt_sidecars[0]
        .encoded
        .blocks
        .iter()
        .enumerate()
    {
        let lba = usize::try_from(retry_append_position.lba).unwrap() + offset;
        assert_eq!(
            crashed_raw.records[lba],
            TapeRecord::Block(block.clone()),
            "failed filemark must leave the full first-sidecar body at LBA {lba}"
        );
    }

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("first sidecar without filemark must not scan as committed");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", retry_append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&crashed_sink_blocks, retry_append_position.lba);
    let mut retry_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed object");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        0
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        12
    );
    assert_eq!(retry_rebuild.plan.live_epoch_start, 12);
    assert_eq!(retry_rebuild.plan.next_data_ordinal, 14);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 3);
    for (retry_sidecar, initial_sidecar) in retry_rebuild
        .rebuilt_sidecars
        .iter()
        .zip(&initial_rebuild.rebuilt_sidecars)
    {
        assert_eq!(retry_sidecar.plan, initial_sidecar.plan);
    }
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry all sidecars writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 3);
    assert_eq!(resume_result.highest_protected_ordinal, 12);
    assert_eq!(resume_result.live_epoch_start, 12);
    assert_eq!(resume_result.next_data_ordinal, 14);

    let retry_first_sidecar = &resume_result.sidecars_emitted[0];
    let retry_second_sidecar = &resume_result.sidecars_emitted[1];
    let retry_third_sidecar = &resume_result.sidecars_emitted[2];
    assert_eq!(retry_first_sidecar.tape_file_number, 2);
    assert_eq!(retry_first_sidecar.protected_ordinal_start, 0);
    assert_eq!(retry_first_sidecar.protected_ordinal_end_exclusive, 4);
    assert_eq!(retry_second_sidecar.tape_file_number, 3);
    assert_eq!(retry_second_sidecar.protected_ordinal_start, 4);
    assert_eq!(retry_second_sidecar.protected_ordinal_end_exclusive, 8);
    assert_eq!(retry_third_sidecar.tape_file_number, 4);
    assert_eq!(retry_third_sidecar.protected_ordinal_start, 8);
    assert_eq!(retry_third_sidecar.protected_ordinal_end_exclusive, 12);

    let retried_raw = RawVecTape::from_sink(&retried_sink_blocks);
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "retry must overwrite the failed-filemark sidecar body at the object-boundary append point"
    );
    let retry_second_sidecar_lba = retry_append_position.lba + retry_first_sidecar.block_count + 1;
    assert_eq!(
        retried_raw.records[usize::try_from(retry_second_sidecar_lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[1].encoded.blocks[0].clone()),
        "retry must continue with sidecar 2 after sidecar 1's durable filemark"
    );

    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            retry_first_sidecar.block_count,
            retry_first_sidecar.epoch_id,
            retry_first_sidecar.protected_ordinal_start,
            retry_first_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            retry_second_sidecar.block_count,
            retry_second_sidecar.epoch_id,
            retry_second_sidecar.protected_ordinal_start,
            retry_second_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            4,
            retry_third_sidecar.block_count,
            retry_third_sidecar.epoch_id,
            retry_third_sidecar.protected_ordinal_start,
            retry_third_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("first-sidecar-filemark-crash retry resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-first-sidecar-filemark-crash reserve fits")
                .0,
            5
        );
        for seed in 15..=16 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 5);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 6);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 12);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            16
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 16);
    assert_eq!(reconstructed.total_data_ordinals(), 16);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 16);
    assert_eq!(scoped.map.total_data_ordinals(), 16);
}

#[test]
fn crash_mid_second_sidecar_in_cluster_truncates_to_committed_first_sidecar() {
    let bot_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
    ])
    .expect("committed prefix validates");

    let mut crashed_sink_blocks = VecBlockSink::new();
    crashed_sink_blocks
        .write_block(&bootstrap_block_for_map(&bot_prefix, false, 0))
        .expect("BOT bootstrap writes");
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("BOT filemark");
    for seed in 1..=10 {
        crashed_sink_blocks
            .write_block(&object_block(seed))
            .expect("committed object block writes");
    }
    crashed_sink_blocks
        .write_filemarks(1)
        .expect("committed object filemark writes");
    assert_eq!(crashed_sink_blocks.next_lba(), 13);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial resume rebuild succeeds");
    assert_eq!(
        initial_rebuild.plan.append_position,
        PhysicalPositionHint::new(13)
    );
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 2);
    assert!(initial_rebuild.rebuilt_sidecars[1].encoded.blocks.len() > 1);

    let first_sidecar_block_count = initial_rebuild.rebuilt_sidecars[0].encoded.blocks.len();
    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink = FailAfterFixedBlocksRawSink::new(
            &mut crashed_sink_blocks,
            first_sidecar_block_count + 1,
        );
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash after the first second-sidecar block aborts resume");

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains("resume sidecar 3 block 1 write failed before filemark"),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash before next resume sidecar block"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert_eq!(committed_before_crash.len(), 1);
    let first_resume_sidecar = &committed_before_crash[0];
    assert_eq!(first_resume_sidecar.tape_file_number, 2);
    assert_eq!(first_resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(first_resume_sidecar.protected_ordinal_end_exclusive, 4);
    let retry_append_position = PhysicalPositionHint::new(
        initial_rebuild.plan.append_position.lba + first_resume_sidecar.block_count + 1,
    );
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        retry_append_position.lba + 1
    );

    let partial_second_sidecar_block = {
        let raw = RawVecTape::from_sink(&crashed_sink_blocks);
        raw.records[usize::try_from(retry_append_position.lba).unwrap()].clone()
    };
    assert_eq!(
        partial_second_sidecar_block,
        TapeRecord::Block(initial_rebuild.rebuilt_sidecars[1].encoded.blocks[0].clone()),
        "the crash fixture must leave exactly the first block of sidecar 2 on tape"
    );

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("partial second sidecar must not scan as a committed sidecar");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", retry_append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let retry_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("retry committed prefix validates");

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&crashed_sink_blocks, retry_append_position.lba);
    let mut retry_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &retry_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed first sidecar");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 1);
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[0].plan,
        initial_rebuild.rebuilt_sidecars[1].plan
    );
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.highest_protected_ordinal, 8);
    assert_eq!(resume_result.live_epoch_start, 8);
    assert_eq!(resume_result.next_data_ordinal, 10);

    let retried_raw = RawVecTape::from_sink(&retried_sink_blocks);
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "retry must overwrite the partial second-sidecar header at the catalog append point"
    );
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba + 1).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[1].clone()),
        "retry must continue with the rebuilt second-sidecar body, not stale crash bytes"
    );

    let retry_sidecar = &resume_result.sidecars_emitted[0];
    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            retry_sidecar.block_count,
            retry_sidecar.epoch_id,
            retry_sidecar.protected_ordinal_start,
            retry_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("retry resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("retry continuation reserve fits")
                .0,
            4
        );
        for seed in 11..=12 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 5);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 8);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            12
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 12);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 12);
    assert_eq!(scoped.map.total_data_ordinals(), 12);
}

#[test]
fn crash_mid_third_sidecar_in_cluster_truncates_to_committed_second_sidecar() {
    let (committed_prefix, mut crashed_sink_blocks) = committed_object_prefix(14);
    assert_eq!(crashed_sink_blocks.next_lba(), 17);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial three-sidecar resume rebuild succeeds");
    assert_eq!(
        initial_rebuild.plan.append_position,
        PhysicalPositionHint::new(17)
    );
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 3);
    assert!(initial_rebuild.rebuilt_sidecars[2].encoded.blocks.len() > 1);
    assert!(initial_rebuild.live_epoch.is_some());

    let first_sidecar_block_count = initial_rebuild.rebuilt_sidecars[0].encoded.blocks.len();
    let second_sidecar_block_count = initial_rebuild.rebuilt_sidecars[1].encoded.blocks.len();
    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink = FailAfterFixedBlocksRawSink::new(
            &mut crashed_sink_blocks,
            first_sidecar_block_count + second_sidecar_block_count + 1,
        );
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash after the first third-sidecar block aborts resume");

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains("resume sidecar 4 block 1 write failed before filemark"),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash before next resume sidecar block"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert_eq!(committed_before_crash.len(), 2);
    let first_resume_sidecar = &committed_before_crash[0];
    let second_resume_sidecar = &committed_before_crash[1];
    assert_eq!(first_resume_sidecar.tape_file_number, 2);
    assert_eq!(first_resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(first_resume_sidecar.protected_ordinal_end_exclusive, 4);
    assert_eq!(second_resume_sidecar.tape_file_number, 3);
    assert_eq!(second_resume_sidecar.protected_ordinal_start, 4);
    assert_eq!(second_resume_sidecar.protected_ordinal_end_exclusive, 8);

    let retry_append_position = PhysicalPositionHint::new(
        initial_rebuild.plan.append_position.lba
            + first_resume_sidecar.block_count
            + 1
            + second_resume_sidecar.block_count
            + 1,
    );
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        retry_append_position.lba + 1
    );

    let partial_third_sidecar_block = {
        let raw = RawVecTape::from_sink(&crashed_sink_blocks);
        raw.records[usize::try_from(retry_append_position.lba).unwrap()].clone()
    };
    assert_eq!(
        partial_third_sidecar_block,
        TapeRecord::Block(initial_rebuild.rebuilt_sidecars[2].encoded.blocks[0].clone()),
        "the crash fixture must leave exactly the first block of sidecar 3 on tape"
    );

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("partial third sidecar must not scan as a committed sidecar");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", retry_append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let retry_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            second_resume_sidecar.block_count,
            second_resume_sidecar.epoch_id,
            second_resume_sidecar.protected_ordinal_start,
            second_resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("retry committed prefix validates");

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&crashed_sink_blocks, retry_append_position.lba);
    let mut retry_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &retry_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed second sidecar");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 3);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        8
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        12
    );
    assert_eq!(retry_rebuild.plan.live_epoch_start, 12);
    assert_eq!(retry_rebuild.plan.next_data_ordinal, 14);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 1);
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[0].plan,
        initial_rebuild.rebuilt_sidecars[2].plan
    );
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry third sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.highest_protected_ordinal, 12);
    assert_eq!(resume_result.live_epoch_start, 12);
    assert_eq!(resume_result.next_data_ordinal, 14);

    let retried_raw = RawVecTape::from_sink(&retried_sink_blocks);
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "retry must overwrite the partial third-sidecar header at the catalog append point"
    );
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba + 1).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[1].clone()),
        "retry must continue with rebuilt sidecar-3 body bytes"
    );

    let retry_sidecar = &resume_result.sidecars_emitted[0];
    assert_eq!(retry_sidecar.tape_file_number, 4);
    assert_eq!(retry_sidecar.protected_ordinal_start, 8);
    assert_eq!(retry_sidecar.protected_ordinal_end_exclusive, 12);
    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            second_resume_sidecar.block_count,
            second_resume_sidecar.epoch_id,
            second_resume_sidecar.protected_ordinal_start,
            second_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            4,
            retry_sidecar.block_count,
            retry_sidecar.epoch_id,
            retry_sidecar.protected_ordinal_start,
            retry_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("three-sidecar-cluster resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-three-sidecar-cluster reserve fits")
                .0,
            5
        );
        for seed in 15..=16 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 5);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 6);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 12);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            16
        );

        sink.finish().expect("final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&retried_sink_blocks);
    let first_continued_block_lba = retry_append_position.lba + retry_sidecar.block_count + 1;
    assert_eq!(
        resumed_raw.records[usize::try_from(first_continued_block_lba).unwrap()],
        TapeRecord::Block(object_block(15)),
        "new object data must start after the committed retry sidecar"
    );

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 16);
    assert_eq!(reconstructed.total_data_ordinals(), 16);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 16);
    assert_eq!(scoped.map.total_data_ordinals(), 16);
}

#[test]
fn crash_on_second_resume_sidecar_filemark_truncates_to_committed_first_sidecar() {
    let (committed_prefix, mut crashed_sink_blocks) = committed_object_prefix(14);
    assert_eq!(crashed_sink_blocks.next_lba(), 17);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial three-sidecar resume rebuild succeeds");
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 3);
    assert!(initial_rebuild.live_epoch.is_some());

    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink = FailOnNthFilemarkRawSink::new(&mut crashed_sink_blocks, 2);
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash on the second sidecar filemark aborts resume");
        assert_eq!(raw_sink.filemark_calls, 2);

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains(
                        "resume sidecar 3 synchronous filemark failed before catalog commit"
                    ),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash during resume sidecar filemark"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert_eq!(committed_before_crash.len(), 1);
    let first_resume_sidecar = &committed_before_crash[0];
    assert_eq!(first_resume_sidecar.tape_file_number, 2);
    assert_eq!(first_resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(first_resume_sidecar.protected_ordinal_end_exclusive, 4);

    let second_sidecar_block_count = initial_rebuild.rebuilt_sidecars[1].encoded.blocks.len();
    let retry_append_position = PhysicalPositionHint::new(
        initial_rebuild.plan.append_position.lba + first_resume_sidecar.block_count + 1,
    );
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        retry_append_position.lba + u64::try_from(second_sidecar_block_count).unwrap()
    );

    let crashed_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    for (offset, block) in initial_rebuild.rebuilt_sidecars[1]
        .encoded
        .blocks
        .iter()
        .enumerate()
    {
        let lba = usize::try_from(retry_append_position.lba).unwrap() + offset;
        assert_eq!(
            crashed_raw.records[lba],
            TapeRecord::Block(block.clone()),
            "failed filemark must leave the full second-sidecar body at LBA {lba}"
        );
    }

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("second sidecar without filemark must not scan as committed");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", retry_append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let retry_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("retry committed prefix validates");

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&crashed_sink_blocks, retry_append_position.lba);
    let mut retry_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &retry_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed first sidecar");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        4
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        12
    );
    assert_eq!(retry_rebuild.plan.live_epoch_start, 12);
    assert_eq!(retry_rebuild.plan.next_data_ordinal, 14);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 2);
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[0].plan,
        initial_rebuild.rebuilt_sidecars[1].plan
    );
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[1].plan,
        initial_rebuild.rebuilt_sidecars[2].plan
    );
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry remaining sidecars write and commit")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 2);
    assert_eq!(resume_result.highest_protected_ordinal, 12);
    assert_eq!(resume_result.live_epoch_start, 12);
    assert_eq!(resume_result.next_data_ordinal, 14);

    let retry_second_sidecar = &resume_result.sidecars_emitted[0];
    let retry_third_sidecar = &resume_result.sidecars_emitted[1];
    assert_eq!(retry_second_sidecar.tape_file_number, 3);
    assert_eq!(retry_second_sidecar.protected_ordinal_start, 4);
    assert_eq!(retry_second_sidecar.protected_ordinal_end_exclusive, 8);
    assert_eq!(retry_third_sidecar.tape_file_number, 4);
    assert_eq!(retry_third_sidecar.protected_ordinal_start, 8);
    assert_eq!(retry_third_sidecar.protected_ordinal_end_exclusive, 12);

    let retried_raw = RawVecTape::from_sink(&retried_sink_blocks);
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "retry must overwrite the failed-filemark sidecar body at the catalog append point"
    );
    let retry_third_sidecar_lba = retry_append_position.lba + retry_second_sidecar.block_count + 1;
    assert_eq!(
        retried_raw.records[usize::try_from(retry_third_sidecar_lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[1].encoded.blocks[0].clone()),
        "retry must continue with sidecar 3 after sidecar 2's durable filemark"
    );

    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            retry_second_sidecar.block_count,
            retry_second_sidecar.epoch_id,
            retry_second_sidecar.protected_ordinal_start,
            retry_second_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            4,
            retry_third_sidecar.block_count,
            retry_third_sidecar.epoch_id,
            retry_third_sidecar.protected_ordinal_start,
            retry_third_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("filemark-crash retry resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-filemark-crash reserve fits")
                .0,
            5
        );
        for seed in 15..=16 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 5);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 6);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 12);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            16
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 16);
    assert_eq!(reconstructed.total_data_ordinals(), 16);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 16);
    assert_eq!(scoped.map.total_data_ordinals(), 16);
}

#[test]
fn crash_on_third_resume_sidecar_filemark_truncates_to_committed_second_sidecar() {
    let (committed_prefix, mut crashed_sink_blocks) = committed_object_prefix(14);
    assert_eq!(crashed_sink_blocks.next_lba(), 17);

    let mut raw_source = RawVecTape::from_sink(&crashed_sink_blocks);
    let initial_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial three-sidecar resume rebuild succeeds");
    assert_eq!(initial_rebuild.rebuilt_sidecars.len(), 3);
    assert!(initial_rebuild.live_epoch.is_some());

    let mut committed_before_crash = Vec::new();
    {
        let mut raw_sink = FailOnNthFilemarkRawSink::new(&mut crashed_sink_blocks, 3);
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            initial_rebuild.plan.clone(),
            &initial_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_before_crash.push(sidecar.clone());
                Ok(())
            },
        )
        .expect_err("crash on the third sidecar filemark aborts resume");
        assert_eq!(raw_sink.filemark_calls, 3);

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains(
                        "resume sidecar 4 synchronous filemark failed before catalog commit"
                    ),
                    "{message}"
                );
                assert!(
                    message.contains("simulated crash during resume sidecar filemark"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }

    assert_eq!(committed_before_crash.len(), 2);
    let first_resume_sidecar = &committed_before_crash[0];
    let second_resume_sidecar = &committed_before_crash[1];
    assert_eq!(first_resume_sidecar.tape_file_number, 2);
    assert_eq!(first_resume_sidecar.protected_ordinal_start, 0);
    assert_eq!(first_resume_sidecar.protected_ordinal_end_exclusive, 4);
    assert_eq!(second_resume_sidecar.tape_file_number, 3);
    assert_eq!(second_resume_sidecar.protected_ordinal_start, 4);
    assert_eq!(second_resume_sidecar.protected_ordinal_end_exclusive, 8);

    let third_sidecar_block_count = initial_rebuild.rebuilt_sidecars[2].encoded.blocks.len();
    let retry_append_position = PhysicalPositionHint::new(
        initial_rebuild.plan.append_position.lba
            + first_resume_sidecar.block_count
            + 1
            + second_resume_sidecar.block_count
            + 1,
    );
    assert_eq!(
        crashed_sink_blocks.next_lba(),
        retry_append_position.lba + u64::try_from(third_sidecar_block_count).unwrap()
    );

    let crashed_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    for (offset, block) in initial_rebuild.rebuilt_sidecars[2]
        .encoded
        .blocks
        .iter()
        .enumerate()
    {
        let lba = usize::try_from(retry_append_position.lba).unwrap() + offset;
        assert_eq!(
            crashed_raw.records[lba],
            TapeRecord::Block(block.clone()),
            "failed filemark must leave the full third-sidecar body at LBA {lba}"
        );
    }

    let mut physical_raw = RawVecTape::from_sink(&crashed_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_raw, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("third sidecar without filemark must not scan as committed");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", retry_append_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let retry_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            second_resume_sidecar.block_count,
            second_resume_sidecar.epoch_id,
            second_resume_sidecar.protected_ordinal_start,
            second_resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("retry committed prefix validates");

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&crashed_sink_blocks, retry_append_position.lba);
    let mut retry_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_source,
        &retry_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild resumes after the committed second sidecar");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 3);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        8
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        12
    );
    assert_eq!(retry_rebuild.plan.live_epoch_start, 12);
    assert_eq!(retry_rebuild.plan.next_data_ordinal, 14);
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 1);
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[0].plan,
        initial_rebuild.rebuilt_sidecars[2].plan
    );
    assert_eq!(retry_rebuild.live_epoch, initial_rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry third sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.highest_protected_ordinal, 12);
    assert_eq!(resume_result.live_epoch_start, 12);
    assert_eq!(resume_result.next_data_ordinal, 14);

    let retried_raw = RawVecTape::from_sink(&retried_sink_blocks);
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[0].clone()),
        "retry must overwrite the failed-filemark third-sidecar body at the catalog append point"
    );
    assert_eq!(
        retried_raw.records[usize::try_from(retry_append_position.lba + 1).unwrap()],
        TapeRecord::Block(retry_rebuild.rebuilt_sidecars[0].encoded.blocks[1].clone()),
        "retry must continue with rebuilt sidecar-3 body bytes"
    );

    let retry_sidecar = &resume_result.sidecars_emitted[0];
    assert_eq!(retry_sidecar.tape_file_number, 4);
    assert_eq!(retry_sidecar.protected_ordinal_start, 8);
    assert_eq!(retry_sidecar.protected_ordinal_end_exclusive, 12);

    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            second_resume_sidecar.block_count,
            second_resume_sidecar.epoch_id,
            second_resume_sidecar.protected_ordinal_start,
            second_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            4,
            retry_sidecar.block_count,
            retry_sidecar.epoch_id,
            retry_sidecar.protected_ordinal_start,
            retry_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("third-sidecar-filemark-crash retry resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("post-third-sidecar-filemark-crash reserve fits")
                .0,
            5
        );
        for seed in 15..=16 {
            sink.write_block(&object_block(seed))
                .expect("continued object block writes");
        }
        let object = sink.finish_object().expect("continued object closes");
        assert_eq!(object.tape_file_number, 5);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 6);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 12);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            16
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 16);
    assert_eq!(reconstructed.total_data_ordinals(), 16);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 16);
    assert_eq!(scoped.map.total_data_ordinals(), 16);
}

#[test]
fn failed_second_resume_sidecar_commit_is_abandoned_on_next_resume() {
    let bot_prefix =
        FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT prefix validates");
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
    ])
    .expect("committed prefix validates");

    let mut failed_sink_blocks = VecBlockSink::new();
    failed_sink_blocks
        .write_block(&bootstrap_block_for_map(&bot_prefix, false, 0))
        .expect("BOT bootstrap writes");
    failed_sink_blocks.write_filemarks(1).expect("BOT filemark");
    for seed in 1..=10 {
        failed_sink_blocks
            .write_block(&object_block(seed))
            .expect("committed object block writes");
    }
    failed_sink_blocks
        .write_filemarks(1)
        .expect("committed object filemark writes");
    assert_eq!(failed_sink_blocks.next_lba(), 13);

    let mut raw_source = RawVecTape::from_sink(&failed_sink_blocks);
    let rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("initial resume rebuild succeeds");
    assert_eq!(rebuild.plan.append_position, PhysicalPositionHint::new(13));
    assert_eq!(rebuild.rebuilt_sidecars.len(), 2);
    assert!(rebuild.live_epoch.is_some());

    let mut committed_before_failure = Vec::new();
    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut failed_sink_blocks);
        let err = emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                if sidecar.tape_file_number == 2 {
                    assert_eq!(sidecar.protected_ordinal_start, 0);
                    assert_eq!(sidecar.protected_ordinal_end_exclusive, 4);
                    committed_before_failure.push(sidecar.clone());
                    Ok(())
                } else {
                    assert_eq!(sidecar.tape_file_number, 3);
                    assert_eq!(sidecar.protected_ordinal_start, 4);
                    assert_eq!(sidecar.protected_ordinal_end_exclusive, 8);
                    Err(ParityError::ResumeAppend(
                        "catalog sidecar commit unavailable".to_string(),
                    ))
                }
            },
        )
        .expect_err("second catalog commit failure aborts resume");

        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message
                        .contains("resume sidecar 3 catalog commit failed after filemark barrier"),
                    "{message}"
                );
                assert!(
                    message.contains("catalog sidecar commit unavailable"),
                    "{message}"
                );
            }
            other => panic!("expected resume append error, got {other:?}"),
        }
    }
    assert_eq!(committed_before_failure.len(), 1);
    assert!(failed_sink_blocks.next_lba() > rebuild.plan.append_position.lba);

    let mut failed_physical_raw = RawVecTape::from_sink(&failed_sink_blocks);
    let abandoned_physical_map =
        scan_reconstruct_filemark_map(&mut failed_physical_raw, &TAPE_UUID, BLOCK_SIZE)
            .expect("failed physical tape still scans structurally");
    assert_eq!(
        abandoned_physical_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
        ]
    );
    assert_eq!(abandoned_physical_map.max_sidecar_end_exclusive(), 8);

    let first_resume_sidecar = &committed_before_failure[0];
    let retry_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("retry committed prefix validates");
    let retry_append_position = PhysicalPositionHint::new(
        rebuild.plan.append_position.lba + first_resume_sidecar.block_count + 1,
    );
    assert!(failed_sink_blocks.next_lba() > retry_append_position.lba);

    let mut retried_sink_blocks =
        truncate_sink_at_lba(&failed_sink_blocks, retry_append_position.lba);
    let mut retry_raw_source = RawVecTape::from_sink(&retried_sink_blocks);
    let retry_rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut retry_raw_source,
        &retry_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("retry rebuild from catalog prefix succeeds");
    assert_eq!(retry_rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(retry_rebuild.plan.append_position, retry_append_position);
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_before_rebuild,
        4
    );
    assert_eq!(
        retry_rebuild.plan.highest_protected_ordinal_after_rebuild,
        8
    );
    assert_eq!(retry_rebuild.rebuilt_sidecars.len(), 1);
    assert_eq!(
        retry_rebuild.rebuilt_sidecars[0].plan,
        rebuild.rebuilt_sidecars[1].plan
    );
    assert_eq!(retry_rebuild.live_epoch, rebuild.live_epoch);

    let mut committed_after_retry = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            retry_rebuild.plan.clone(),
            &retry_rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_after_retry.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("retry resume sidecar writes and commits")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_after_retry);
    assert_eq!(resume_result.sidecars_emitted.len(), 1);
    assert_eq!(resume_result.highest_protected_ordinal, 8);

    let retry_sidecar = &resume_result.sidecars_emitted[0];
    let prefix_after_retry = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            first_resume_sidecar.block_count,
            first_resume_sidecar.epoch_id,
            first_resume_sidecar.protected_ordinal_start,
            first_resume_sidecar.protected_ordinal_end_exclusive,
        ),
        TapeFileMapEntry::parity_sidecar(
            3,
            retry_sidecar.block_count,
            retry_sidecar.epoch_id,
            retry_sidecar.protected_ordinal_start,
            retry_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("post-retry committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut retried_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_retry,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_retry,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: retry_rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("retry resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(2, 2))
                .expect("retry continuation reserve fits")
                .0,
            4
        );
        for seed in 11..=12 {
            sink.write_block(&object_block(seed))
                .expect("retry continued object block writes");
        }
        let object = sink.finish_object().expect("retry continued object closes");
        assert_eq!(object.tape_file_number, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 5);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 8);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            12
        );

        sink.finish().expect("retry final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        retried_sink_blocks
            .blocks
            .last()
            .expect("final bootstrap block"),
    )
    .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&retried_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 12);
    assert_eq!(scoped.map.total_data_ordinals(), 12);
}

#[test]
fn unreadable_resume_rebuild_aborts_then_clean_copy_finishes() {
    let (committed_prefix, damaged_sink_blocks) = committed_object_prefix(10);
    let append_position = PhysicalPositionHint::new(damaged_sink_blocks.next_lba());
    assert_eq!(append_position, PhysicalPositionHint::new(13));

    let unreadable_ordinal = 5;
    let unreadable_lba = 2 + unreadable_ordinal;
    let mut damaged_source =
        RawVecTape::from_sink(&damaged_sink_blocks).with_unreadable_lba(unreadable_lba);
    let err = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut damaged_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect_err("damaged copy cannot rebuild its open epoch");

    match err {
        ParityError::ResumeAppend(message) => {
            assert!(message.contains("ordinal 5"), "{message}");
            assert!(message.contains("physical LBA 7"), "{message}");
            assert!(
                message.contains("simulated unreadable raw record at LBA 7"),
                "{message}"
            );
        }
        other => panic!("expected resume append error, got {other:?}"),
    }
    assert_eq!(damaged_sink_blocks.next_lba(), append_position.lba);

    let mut damaged_scan_source = RawVecTape::from_sink(&damaged_sink_blocks);
    let damaged_map =
        scan_reconstruct_filemark_map(&mut damaged_scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("damaged copy still scans to the committed prefix");
    assert_eq!(
        damaged_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![TapeFileKind::Bootstrap, TapeFileKind::Object]
    );
    assert_eq!(damaged_map.max_sidecar_end_exclusive(), 0);

    let (_, mut fallback_sink_blocks) = committed_object_prefix(10);
    let mut fallback_source = RawVecTape::from_sink(&fallback_sink_blocks);
    let rebuild = rebuild_legacy_forensic_open_epoch_from_committed_prefix(
        &mut fallback_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("clean fallback copy rebuilds");
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.rebuilt_sidecars.len(), 2);
    assert_eq!(
        rebuild
            .live_epoch
            .as_ref()
            .expect("tail remains live")
            .next_data_ordinal,
        10
    );

    let mut committed_resume_sidecars = Vec::new();
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut fallback_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |sidecar| {
                committed_resume_sidecars.push(sidecar.clone());
                Ok(())
            },
        )
        .expect("fallback resume sidecars write and commit")
    };
    assert_eq!(resume_result.sidecars_emitted, committed_resume_sidecars);
    assert_eq!(resume_result.sidecars_emitted.len(), 2);
    assert_eq!(resume_result.highest_protected_ordinal, 8);
    assert_eq!(resume_result.live_epoch_start, 8);
    assert_eq!(resume_result.next_data_ordinal, 10);

    let mut prefix_entries = vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 10, 0),
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
        FilemarkMap::new(prefix_entries).expect("fallback committed prefix validates");

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut fallback_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &prefix_after_resume,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &prefix_after_resume,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("fallback resumed sink constructs");

        sink.finish().expect("fallback final bootstrap writes");
    }

    let final_bootstrap = parse_bootstrap_block(
        fallback_sink_blocks
            .blocks
            .last()
            .expect("fallback final bootstrap block"),
    )
    .expect("fallback final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("fallback final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&fallback_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 10);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 10);
    assert_eq!(reconstructed.entries()[4].protected_ordinal_start, Some(8));
    assert_eq!(
        reconstructed.entries()[4].protected_ordinal_end_exclusive,
        Some(10)
    );

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 10);
    assert_eq!(scoped.map.total_data_ordinals(), 10);
}

#[test]
fn clean_aligned_resume_emits_no_sidecars_then_writer_appends_next_epoch() {
    let mut sink_blocks = VecBlockSink::new();
    let initial_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed)).expect("object block");
        }
        let object = sink.finish_object().expect("object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );
        object.sidecars_emitted[0].clone()
    };

    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            initial_sidecar.block_count,
            initial_sidecar.epoch_id,
            initial_sidecar.protected_ordinal_start,
            initial_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("clean-aligned prefix validates");
    let append_position = PhysicalPositionHint::new(sink_blocks.next_lba());
    assert_eq!(
        append_position.lba,
        1 + 1 + 4 + 1 + initial_sidecar.block_count + 1
    );

    let mut raw_source = RawVecTape::from_sink(&sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("clean-aligned resume rebuild succeeds");

    assert_eq!(rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 4);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 4);
    assert!(rebuild.plan.sidecars_to_emit.is_empty());
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty resume sidecar plan completes")
    };

    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 4);
    assert_eq!(sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("clean-aligned resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("post-resume reserve fits")
                .0,
            3
        );
        for seed in 5..=8 {
            sink.write_block(&object_block(seed))
                .expect("post-resume object block");
        }
        let object = sink.finish_object().expect("post-resume object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("final bootstrap writes");
    }

    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");

    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn resume_from_committed_final_bootstrap_appends_after_bootstrap_tail() {
    let mut sink_blocks = VecBlockSink::new();
    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("initial reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed))
                .expect("initial object block");
        }
        let object = sink.finish_object().expect("initial object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("first final bootstrap writes");
    }

    let old_final_bootstrap = parse_bootstrap_block(
        sink_blocks
            .blocks
            .last()
            .expect("old final bootstrap block"),
    )
    .expect("old final bootstrap parses");
    assert_eq!(old_final_bootstrap.sequence, 1);
    let old_final_digest = old_final_bootstrap
        .filemark_map_digest
        .expect("old final bootstrap carries map digest");
    assert!(old_final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let committed_prefix =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        committed_prefix
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    let scoped_prefix =
        ScopedFilemarkMap::validate_against_digest(committed_prefix.clone(), &old_final_digest)
            .expect("old final bootstrap validates the committed prefix");
    assert_eq!(scoped_prefix.validated_prefix_tape_files, None);

    let append_position = committed_prefix
        .append_position_after_prefix()
        .expect("append position computes after bootstrap tail");
    assert_eq!(append_position.lba, sink_blocks.next_lba());

    let mut raw_source = RawVecTape::from_sink(&sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("bootstrap-tail resume rebuild succeeds");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 3);
    assert_eq!(rebuild.plan.append_position, append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 4);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 4);
    assert!(rebuild.plan.sidecars_to_emit.is_empty());
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), append_position);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty bootstrap-tail resume sidecar plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.append_after_tape_file_number, 3);
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 4);
    assert_eq!(sink_blocks.next_lba(), append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 2,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("bootstrap-tail resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("post-bootstrap reserve fits")
                .0,
            4
        );
        for seed in 5..=8 {
            sink.write_block(&object_block(seed))
                .expect("post-bootstrap object block");
        }
        let object = sink.finish_object().expect("post-bootstrap object closes");
        assert_eq!(object.tape_file_number, 4);
        assert_eq!(object.first_parity_data_ordinal, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 5);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("new final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&sink_blocks);
    assert_eq!(
        resumed_raw.records[usize::try_from(append_position.lba).unwrap()],
        TapeRecord::Block(object_block(5)),
        "resumed object must start immediately after the committed bootstrap tail"
    );

    let new_final_bootstrap = parse_bootstrap_block(
        sink_blocks
            .blocks
            .last()
            .expect("new final bootstrap block"),
    )
    .expect("new final bootstrap parses");
    assert_eq!(new_final_bootstrap.sequence, 2);
    let new_final_digest = new_final_bootstrap
        .filemark_map_digest
        .expect("new final bootstrap carries map digest");
    assert!(new_final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &new_final_digest)
        .expect("new final bootstrap validates the appended tape");
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn catalog_prefix_wins_over_scanned_final_bootstrap_tail_on_resume() {
    let mut sink_blocks = VecBlockSink::new();
    let committed_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("initial reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed))
                .expect("initial object block");
        }
        let object = sink.finish_object().expect("initial object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);

        sink.finish().expect("uncommitted final bootstrap writes");

        object.sidecars_emitted[0].clone()
    };

    let catalog_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            committed_sidecar.block_count,
            committed_sidecar.epoch_id,
            committed_sidecar.protected_ordinal_start,
            committed_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("catalog prefix validates");

    let final_bootstrap = parse_bootstrap_block(
        sink_blocks
            .blocks
            .last()
            .expect("physical tail final bootstrap block"),
    )
    .expect("physical tail final bootstrap parses");
    assert_eq!(final_bootstrap.sequence, 1);
    let final_digest = final_bootstrap
        .filemark_map_digest
        .clone()
        .expect("physical tail final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut full_scan_source = RawVecTape::from_sink(&sink_blocks);
    let scanned_tail_map =
        scan_reconstruct_filemark_map(&mut full_scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("catalog-less scan sees the physical bootstrap tail");
    assert_eq!(
        scanned_tail_map
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_ne!(scanned_tail_map, catalog_prefix);
    let scoped_scanned =
        ScopedFilemarkMap::validate_against_digest(scanned_tail_map.clone(), &final_digest)
            .expect("physical bootstrap tail validates its own scanned map");
    assert_eq!(scoped_scanned.validated_prefix_tape_files, None);

    let mut catalog_source = RawVecTape::from_sink(&sink_blocks);
    let catalog_scoped = acquire_filemark_map(
        &mut catalog_source,
        &final_bootstrap,
        Some(CatalogFilemarkMapInput::new(
            TAPE_UUID,
            catalog_prefix.clone(),
            catalog_prefix.max_sidecar_end_exclusive(),
        )),
    )
    .expect("catalog map wins over the scanned physical tail");
    assert_eq!(catalog_scoped.map, catalog_prefix);
    assert_eq!(catalog_scoped.scope.watermark(), 4);
    assert_eq!(catalog_scoped.validated_prefix_tape_files, None);

    let catalog_append_position = catalog_prefix
        .append_position_after_prefix()
        .expect("catalog append point computes");
    assert_eq!(
        catalog_append_position.lba + 2,
        sink_blocks.next_lba(),
        "the physical tail is exactly one uncommitted bootstrap tape file"
    );
    let physical_tail = RawVecTape::from_sink(&sink_blocks);
    match &physical_tail.records[usize::try_from(catalog_append_position.lba).unwrap()] {
        TapeRecord::Block(block) => {
            let tail = parse_bootstrap_block(block).expect("uncommitted tail is a bootstrap");
            assert_eq!(tail.sequence, 1);
        }
        other => {
            panic!("expected uncommitted bootstrap block at catalog append point, got {other:?}")
        }
    }

    let mut raw_source = RawVecTape::from_sink(&sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &catalog_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("resume rebuild uses the catalog prefix");
    assert_eq!(rebuild.plan.append_after_tape_file_number, 2);
    assert_eq!(rebuild.plan.append_position, catalog_append_position);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 4);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 4);
    assert_eq!(rebuild.plan.live_epoch_start, 4);
    assert_eq!(rebuild.plan.next_data_ordinal, 4);
    assert!(rebuild.plan.sidecars_to_emit.is_empty());
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), catalog_append_position);

    let mut append_sink_blocks = truncate_sink_at_lba(&sink_blocks, catalog_append_position.lba);
    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut append_sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty catalog-prefix resume completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(resume_result.append_after_tape_file_number, 2);
    assert_eq!(resume_result.highest_protected_ordinal, 4);
    assert_eq!(resume_result.live_epoch_start, 4);
    assert_eq!(resume_result.next_data_ordinal, 4);
    assert_eq!(append_sink_blocks.next_lba(), catalog_append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut append_sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix: &catalog_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &catalog_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: 1,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("catalog-prefix resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("post-catalog-prefix reserve fits")
                .0,
            3
        );
        for seed in 5..=8 {
            sink.write_block(&object_block(seed))
                .expect("post-catalog-prefix object block");
        }
        let object = sink
            .finish_object()
            .expect("post-catalog-prefix object closes");
        assert_eq!(object.tape_file_number, 3);
        assert_eq!(object.first_parity_data_ordinal, 4);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 4);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 4);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            8
        );

        sink.finish().expect("new final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(&append_sink_blocks);
    assert_eq!(
        resumed_raw.records[usize::try_from(catalog_append_position.lba).unwrap()],
        TapeRecord::Block(object_block(5)),
        "catalog-prefix append must overwrite the uncommitted bootstrap tail"
    );

    let new_final_bootstrap = parse_bootstrap_block(
        append_sink_blocks
            .blocks
            .last()
            .expect("new final bootstrap block"),
    )
    .expect("new final bootstrap parses");
    assert_eq!(new_final_bootstrap.sequence, 1);
    let new_final_digest = new_final_bootstrap
        .filemark_map_digest
        .expect("new final bootstrap carries map digest");
    assert!(new_final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&append_sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.total_data_ordinals(), 8);
    assert_eq!(reconstructed.max_sidecar_end_exclusive(), 8);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &new_final_digest)
        .expect("new final bootstrap validates the catalog-prefix append");
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 8);
    assert_eq!(scoped.map.total_data_ordinals(), 8);
}

#[test]
fn catalog_claims_missing_sidecar_resume_rejects_short_physical_tape() {
    let mut full_sink_blocks = VecBlockSink::new();
    let committed_sidecar = {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut full_sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("initial reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed))
                .expect("initial object block");
        }
        let object = sink.finish_object().expect("initial object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        object.sidecars_emitted[0].clone()
    };

    let physical_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
    ])
    .expect("physical prefix validates");
    let catalog_map = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 4, 0),
        TapeFileMapEntry::parity_sidecar(
            2,
            committed_sidecar.block_count,
            committed_sidecar.epoch_id,
            committed_sidecar.protected_ordinal_start,
            committed_sidecar.protected_ordinal_end_exclusive,
        ),
    ])
    .expect("catalog map with sidecar validates");

    let physical_append_position = physical_prefix
        .append_position_after_prefix()
        .expect("physical append point computes");
    let catalog_append_position = catalog_map
        .append_position_after_prefix()
        .expect("catalog append point computes");
    assert!(
        catalog_append_position.lba > physical_append_position.lba,
        "catalog must claim a sidecar past the scanned physical prefix"
    );

    let short_sink_blocks = truncate_sink_at_lba(&full_sink_blocks, physical_append_position.lba);
    assert_eq!(short_sink_blocks.next_lba(), physical_append_position.lba);

    let mut physical_scan = RawVecTape::from_sink(&short_sink_blocks);
    let scanned_map = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect("short physical tape scans as bootstrap plus object");
    assert_eq!(scanned_map, physical_prefix);
    assert_ne!(scanned_map, catalog_map);

    let bot_bootstrap = parse_bootstrap_block(
        full_sink_blocks
            .blocks
            .first()
            .expect("BOT bootstrap block"),
    )
    .expect("BOT bootstrap parses");
    let mut catalog_source = RawVecTape::from_sink(&short_sink_blocks);
    let catalog_scoped = acquire_filemark_map(
        &mut catalog_source,
        &bot_bootstrap,
        Some(CatalogFilemarkMapInput::new(
            TAPE_UUID,
            catalog_map.clone(),
            catalog_map.max_sidecar_end_exclusive(),
        )),
    )
    .expect("catalog map remains authoritative at acquisition time");
    assert_eq!(catalog_scoped.map, catalog_map);
    assert_eq!(catalog_scoped.scope.watermark(), 4);
    assert_eq!(catalog_scoped.validated_prefix_tape_files, None);

    let mut raw_source = RawVecTape::from_sink(&short_sink_blocks);
    let err = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &catalog_map,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect_err("resume must reject a catalog append point beyond physical EOD");
    match err {
        ParityError::ResumeAppend(message) => {
            assert!(
                message.contains("could not return to append position"),
                "{message}"
            );
            assert!(
                message.contains(&catalog_append_position.lba.to_string()),
                "{message}"
            );
            assert!(
                message.contains(&format!(
                    "actual position was LBA {}",
                    physical_append_position.lba
                )),
                "{message}"
            );
        }
        other => panic!("expected resume append error, got {other:?}"),
    }
}

#[test]
fn catalog_claims_object_filemark_resume_rebuild_rejects_short_physical_tape() {
    let (catalog_map, full_sink_blocks) = committed_object_prefix(4);
    assert_eq!(catalog_map.total_data_ordinals(), 4);
    assert_eq!(catalog_map.max_sidecar_end_exclusive(), 0);

    let catalog_append_position = catalog_map
        .append_position_after_prefix()
        .expect("catalog append point computes");
    let object_start_position = catalog_map
        .physical_position(TapeFilePosition {
            tape_file_number: 1,
            block_within_file: 0,
        })
        .expect("object start physical position computes");
    let physical_append_position = PhysicalPositionHint::new(catalog_append_position.lba - 1);
    assert_eq!(full_sink_blocks.next_lba(), catalog_append_position.lba);

    let short_sink_blocks = truncate_sink_at_lba(&full_sink_blocks, physical_append_position.lba);
    assert_eq!(short_sink_blocks.next_lba(), physical_append_position.lba);

    let mut physical_scan = RawVecTape::from_sink(&short_sink_blocks);
    let err = scan_reconstruct_filemark_map(&mut physical_scan, &TAPE_UUID, BLOCK_SIZE)
        .expect_err("object body without trailing filemark must not scan as committed");
    match err {
        ParityError::FilemarkMapReconstruct(message) => {
            assert!(message.contains("missing a trailing filemark"), "{message}");
            assert!(
                message.contains(&format!("physical LBA {}", object_start_position.lba)),
                "{message}"
            );
        }
        other => panic!("expected filemark-map reconstruction error, got {other:?}"),
    }

    let mut raw_source = RawVecTape::from_sink(&short_sink_blocks);
    let err = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        &catalog_map,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect_err("v1 resume must reject full-epoch committed W/T gap before tape I/O");
    assert_eq!(
        raw_source.read_records, 0,
        "v1 restart-bound validation must fire before any W<T rebuild tape I/O"
    );
    match err {
        ParityError::ResumeAppend(message) => {
            assert!(message.contains("committed v1 prefix"), "{message}");
            assert!(message.contains("one full epoch"), "{message}");
            assert!(message.contains("legacy/forensic"), "{message}");
        }
        other => panic!("expected resume append error, got {other:?}"),
    }
}

/// Validate the current tape against its final bootstrap and return the
/// reconstructed committed map plus the next append position.
fn scan_final_bootstrap_prefix(
    sink_blocks: &VecBlockSink,
    expected_sequence: u32,
    expected_kinds: &[TapeFileKind],
    expected_total_data_ordinals: u64,
) -> (FilemarkMap, PhysicalPositionHint) {
    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    assert_eq!(final_bootstrap.sequence, expected_sequence);
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");
    let actual_kinds = reconstructed
        .entries()
        .iter()
        .map(|entry| entry.kind)
        .collect::<Vec<_>>();
    assert_eq!(actual_kinds.as_slice(), expected_kinds);
    assert_eq!(
        reconstructed.total_data_ordinals(),
        expected_total_data_ordinals
    );
    assert_eq!(
        reconstructed.max_sidecar_end_exclusive(),
        expected_total_data_ordinals
    );

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed.clone(), &final_digest)
        .expect("final bootstrap validates the current tape");
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), expected_total_data_ordinals);
    assert_eq!(
        scoped.map.total_data_ordinals(),
        expected_total_data_ordinals
    );

    let append_position = reconstructed
        .append_position_after_prefix()
        .expect("append position computes after final bootstrap");
    assert_eq!(append_position.lba, sink_blocks.next_lba());
    (reconstructed, append_position)
}

#[derive(Clone, Copy, Debug)]
struct FinalBootstrapAppendCase {
    append_position: PhysicalPositionHint,
    expected_append_after_tape_file_number: u32,
    expected_object_tape_file_number: u32,
    expected_sidecar_tape_file_number: u32,
    expected_first_ordinal: u64,
    first_seed: u8,
    next_bootstrap_sequence: u32,
}

/// Resume from a final-bootstrap committed prefix, append one full epoch of
/// object data, and finish with the next final bootstrap.
fn append_four_block_object_after_final_bootstrap(
    sink_blocks: &mut VecBlockSink,
    committed_prefix: &FilemarkMap,
    case: FinalBootstrapAppendCase,
) {
    let mut raw_source = RawVecTape::from_sink(sink_blocks);
    let rebuild = rebuild_open_epoch_from_committed_prefix(
        &mut raw_source,
        committed_prefix,
        &scheme(),
        TAPE_UUID,
        BLOCK_SIZE,
    )
    .expect("final-bootstrap-tail resume rebuild succeeds");
    assert_eq!(
        rebuild.plan.append_after_tape_file_number,
        case.expected_append_after_tape_file_number
    );
    assert_eq!(rebuild.plan.append_position, case.append_position);
    assert_eq!(
        rebuild.plan.highest_protected_ordinal_before_rebuild,
        case.expected_first_ordinal
    );
    assert_eq!(
        rebuild.plan.highest_protected_ordinal_after_rebuild,
        case.expected_first_ordinal
    );
    assert_eq!(rebuild.plan.live_epoch_start, case.expected_first_ordinal);
    assert_eq!(rebuild.plan.next_data_ordinal, case.expected_first_ordinal);
    assert!(rebuild.plan.sidecars_to_emit.is_empty());
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert!(rebuild.live_epoch.is_none());
    assert_eq!(raw_source.position().unwrap(), case.append_position);

    let mut commit_calls = 0usize;
    let resume_result = {
        let mut raw_sink = BlockSinkRawTapeSink::new(sink_blocks);
        emit_resume_rebuilt_sidecars_to_raw(
            &mut raw_sink,
            rebuild.plan.clone(),
            &rebuild.rebuilt_sidecars,
            TAPE_UUID,
            |_| {
                commit_calls += 1;
                Ok(())
            },
        )
        .expect("empty final-bootstrap-tail resume sidecar plan completes")
    };
    assert_eq!(commit_calls, 0);
    assert!(resume_result.sidecars_emitted.is_empty());
    assert_eq!(
        resume_result.append_after_tape_file_number,
        case.expected_append_after_tape_file_number
    );
    assert_eq!(
        resume_result.highest_protected_ordinal,
        case.expected_first_ordinal
    );
    assert_eq!(resume_result.live_epoch_start, case.expected_first_ordinal);
    assert_eq!(resume_result.next_data_ordinal, case.expected_first_ordinal);
    assert_eq!(sink_blocks.next_lba(), case.append_position.lba);

    {
        let mut raw_sink = BlockSinkRawTapeSink::new(sink_blocks);
        let resume_seed = ResumeWriterSeed {
            committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: rebuild.live_epoch,
            next_bootstrap_sequence: case.next_bootstrap_sequence,
        };
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw_sink,
            scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            resume_seed,
        )
        .expect("final-bootstrap-tail resumed sink constructs");

        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("post-bootstrap reserve fits")
                .0,
            case.expected_object_tape_file_number
        );
        for offset in 0..4 {
            sink.write_block(&object_block(case.first_seed + offset))
                .expect("post-bootstrap object block");
        }
        let object = sink.finish_object().expect("post-bootstrap object closes");
        assert_eq!(
            object.tape_file_number,
            case.expected_object_tape_file_number
        );
        assert_eq!(
            object.first_parity_data_ordinal,
            case.expected_first_ordinal
        );
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(
            object.sidecars_emitted[0].tape_file_number,
            case.expected_sidecar_tape_file_number
        );
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_start,
            case.expected_first_ordinal
        );
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            case.expected_first_ordinal + 4
        );

        sink.finish().expect("next final bootstrap writes");
    }

    let resumed_raw = RawVecTape::from_sink(sink_blocks);
    assert_eq!(
        resumed_raw.records[usize::try_from(case.append_position.lba).unwrap()],
        TapeRecord::Block(object_block(case.first_seed)),
        "resumed object must start immediately after the latest final bootstrap"
    );
}

#[test]
fn repeated_resume_from_final_bootstraps_uses_latest_bootstrap_tail() {
    let mut sink_blocks = VecBlockSink::new();
    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("initial reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed))
                .expect("initial object block");
        }
        let object = sink.finish_object().expect("initial object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.first_parity_data_ordinal, 0);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("first final bootstrap writes");
    }

    let (first_prefix, first_append_position) = scan_final_bootstrap_prefix(
        &sink_blocks,
        1,
        &[
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ],
        4,
    );
    append_four_block_object_after_final_bootstrap(
        &mut sink_blocks,
        &first_prefix,
        FinalBootstrapAppendCase {
            append_position: first_append_position,
            expected_append_after_tape_file_number: 3,
            expected_object_tape_file_number: 4,
            expected_sidecar_tape_file_number: 5,
            expected_first_ordinal: 4,
            first_seed: 5,
            next_bootstrap_sequence: 2,
        },
    );

    let (second_prefix, second_append_position) = scan_final_bootstrap_prefix(
        &sink_blocks,
        2,
        &[
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ],
        8,
    );
    append_four_block_object_after_final_bootstrap(
        &mut sink_blocks,
        &second_prefix,
        FinalBootstrapAppendCase {
            append_position: second_append_position,
            expected_append_after_tape_file_number: 6,
            expected_object_tape_file_number: 7,
            expected_sidecar_tape_file_number: 8,
            expected_first_ordinal: 8,
            first_seed: 9,
            next_bootstrap_sequence: 3,
        },
    );

    let (final_prefix, final_append_position) = scan_final_bootstrap_prefix(
        &sink_blocks,
        3,
        &[
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ],
        12,
    );
    assert_eq!(final_append_position.lba, sink_blocks.next_lba());
    assert_eq!(
        final_prefix
            .entries()
            .iter()
            .filter(|entry| entry.kind == TapeFileKind::Bootstrap)
            .map(|entry| entry.tape_file_number)
            .collect::<Vec<_>>(),
        vec![0, 3, 6, 9]
    );
}

#[test]
fn sidecar_writer_output_scans_back_to_final_bootstrap_digest() {
    let mut sink_blocks = VecBlockSink::new();
    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(4))
                .expect("reserve fits")
                .0,
            1
        );
        for seed in 1..=4 {
            sink.write_block(&object_block(seed)).expect("object block");
        }
        let object = sink.finish_object().expect("object closes");
        assert_eq!(object.tape_file_number, 1);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert_eq!(object.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(object.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            object.sidecars_emitted[0].protected_ordinal_end_exclusive,
            4
        );

        sink.finish().expect("final bootstrap");
    }

    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(final_digest.is_final_map);

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");

    assert_eq!(reconstructed.tape_file_count(), 4);
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.entries()[1].block_count, 4);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.validated_prefix_tape_files, None);
    assert_eq!(scoped.scope.watermark(), 4);
    assert_eq!(scoped.map.total_data_ordinals(), 4);
}

#[test]
fn sidecar_only_finish_scans_partial_epoch_without_physical_padding_file() {
    let mut sink_blocks = VecBlockSink::new();
    {
        let mut raw_sink = BlockSinkRawTapeSink::new(&mut sink_blocks);
        let mut sink = ParitySink::new_sidecar_only(&mut raw_sink, scheme(), TAPE_UUID, BLOCK_SIZE)
            .expect("sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);
        assert_eq!(
            sink.begin_object_with_capacity_reserve(capacity_input(3))
                .expect("reserve fits")
                .0,
            1
        );
        for seed in 1..=3 {
            sink.write_block(&object_block(seed)).expect("object block");
        }
        let object = sink.finish_object().expect("object closes");
        assert!(object.sidecars_emitted.is_empty());

        let final_geometry = sink.finish().expect("final bootstrap");
        assert_eq!(final_geometry.data_area_end_lba, 5);
    }

    let final_bootstrap =
        parse_bootstrap_block(sink_blocks.blocks.last().expect("final bootstrap block"))
            .expect("final bootstrap parses");
    let final_digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");

    let mut raw = RawVecTape::from_sink(&sink_blocks);
    let reconstructed =
        scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan succeeds");

    assert_eq!(reconstructed.tape_file_count(), 4);
    assert_eq!(
        reconstructed
            .entries()
            .iter()
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        vec![
            TapeFileKind::Bootstrap,
            TapeFileKind::Object,
            TapeFileKind::ParitySidecar,
            TapeFileKind::Bootstrap,
        ]
    );
    assert_eq!(reconstructed.entries()[1].block_count, 3);

    let scoped = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    assert_eq!(scoped.scope.watermark(), 3);
    assert_eq!(scoped.map.total_data_ordinals(), 3);
}
