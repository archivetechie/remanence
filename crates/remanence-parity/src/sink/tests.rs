use super::*;
use crate::filemark_map::TapeFileMapEntry;
use crate::model::SchemeId;
use crate::raw::{RawReadOutcome, RawTapeSource, SpaceFilemarksOutcome};

fn small_scheme() -> ParityScheme {
    // k=4, m=2, S=3 → 12 data slots (3 stripes × 4 data rows)
    // then 6 parity slots per neighborhood (Step 11.8 emits).
    ParityScheme {
        id: SchemeId::new_static("test"),
        data_blocks_per_stripe: 4,
        parity_blocks_per_stripe: 2,
        stripes_per_neighborhood: 3,
    }
}

fn sample_uuid() -> [u8; 16] {
    [0u8; 16]
}

fn capacity_input_with_block_size(
    projected_object_blocks: u64,
    remaining_tape_blocks: u64,
    block_size_bytes: u32,
) -> CapacityReserveInput {
    capacity_input_with_current_fill(
        projected_object_blocks,
        remaining_tape_blocks,
        block_size_bytes,
        0,
    )
}

fn capacity_input_with_current_fill(
    projected_object_blocks: u64,
    remaining_tape_blocks: u64,
    block_size_bytes: u32,
    current_epoch_fill_blocks: u64,
) -> CapacityReserveInput {
    CapacityReserveInput {
        projected_object_blocks,
        block_size_bytes: block_size_bytes as u64,
        current_epoch_fill_blocks,
        data_shards_per_epoch: 12,
        parity_shards_per_epoch: 6,
        sidecar_index_block_count: 2,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        bootstrap_filemark_blocks: 1,
        pending_completed_sidecars: 0,
        remaining_bootstrap_count: 1,
        safety_margin_blocks: 3,
        remaining_tape_blocks,
        empty_tape_usable_blocks: u64::MAX,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: 1024 * 1024,
    }
}

fn capacity_input(
    projected_object_blocks: u64,
    remaining_tape_blocks: u64,
) -> CapacityReserveInput {
    capacity_input_with_block_size(projected_object_blocks, remaining_tape_blocks, 8)
}

fn start_object(sink: &mut ParitySink<'_>, projected_object_blocks: u64, block_size_bytes: u32) {
    sink.begin_object_with_capacity_reserve(capacity_input_with_block_size(
        projected_object_blocks,
        10_000,
        block_size_bytes,
    ))
    .expect("object reserve fits");
}

fn exhaust_runtime_tape_reserve(sink: &mut ParitySink<'_>) {
    sink.early_warning_reserve
        .as_mut()
        .expect("object start installs an EW reserve guard")
        .input
        .remaining_tape_blocks = 0;
}

fn fixed_block(seed: u8, block_size_bytes: u32) -> Vec<u8> {
    let mut block = vec![seed; block_size_bytes as usize];
    block[0] = seed;
    block[1] = seed.wrapping_mul(17);
    block
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

fn expected_epoch_parity(
    scheme: &ParityScheme,
    blocks: &[Vec<u8>],
    block_size: u32,
) -> Vec<Vec<u8>> {
    let codec = ReedSolomonCodec::new(scheme).unwrap();
    let s = scheme.stripes_per_neighborhood as usize;
    let k = codec.data_blocks();
    let zero = vec![0u8; block_size as usize];
    let mut flattened = Vec::new();
    for stripe_index in 0..s {
        let mut data = vec![zero.clone(); k];
        for (ordinal, block) in blocks.iter().enumerate() {
            if ordinal % s == stripe_index {
                data[ordinal / s] = block.clone();
            }
        }
        flattened.extend(codec.encode(&data).unwrap());
    }
    flattened
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawSinkEvent {
    WriteBlock(usize),
    WriteFilemark,
    Position,
}

#[derive(Debug)]
struct EwEomTripwireRawTapeSink {
    cursor: u64,
    block_count: usize,
    filemark_count: usize,
    ew_eom_on_block: Option<usize>,
    ew_eom_on_filemark: Option<usize>,
    ew_eom_blocks_seen: Vec<usize>,
    ew_eom_filemarks_seen: Vec<usize>,
    events: Vec<RawSinkEvent>,
}

impl EwEomTripwireRawTapeSink {
    fn on_block(block_number: usize) -> Self {
        Self {
            cursor: 0,
            block_count: 0,
            filemark_count: 0,
            ew_eom_on_block: Some(block_number),
            ew_eom_on_filemark: None,
            ew_eom_blocks_seen: Vec::new(),
            ew_eom_filemarks_seen: Vec::new(),
            events: Vec::new(),
        }
    }

    fn on_filemark(filemark_number: usize) -> Self {
        Self {
            cursor: 0,
            block_count: 0,
            filemark_count: 0,
            ew_eom_on_block: None,
            ew_eom_on_filemark: Some(filemark_number),
            ew_eom_blocks_seen: Vec::new(),
            ew_eom_filemarks_seen: Vec::new(),
            events: Vec::new(),
        }
    }
}

impl RawTapeSink for EwEomTripwireRawTapeSink {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteBlock(buf.len()));
        self.block_count += 1;
        let ew_eom = self.ew_eom_on_block == Some(self.block_count);
        if ew_eom {
            self.ew_eom_blocks_seen.push(self.block_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteBlock {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: ew_eom,
            end_of_medium: ew_eom,
        })
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteFilemark);
        self.filemark_count += 1;
        let ew_eom = self.ew_eom_on_filemark == Some(self.filemark_count);
        if ew_eom {
            self.ew_eom_filemarks_seen.push(self.filemark_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: ew_eom,
            end_of_medium: ew_eom,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.events.push(RawSinkEvent::Position);
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

#[derive(Debug, Default)]
struct RecordingRawTapeSink {
    cursor: u64,
    events: Vec<RawSinkEvent>,
    blocks: Vec<Vec<u8>>,
}

impl RawTapeSink for RecordingRawTapeSink {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteBlock(buf.len()));
        self.blocks.push(buf.to_vec());
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteBlock {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: false,
            end_of_medium: false,
        })
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteFilemark);
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: false,
            end_of_medium: false,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.events.push(RawSinkEvent::Position);
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecordedTapeRecord {
    Block(Vec<u8>),
    Filemark,
}

#[derive(Debug)]
struct RecordingRawTapeSource {
    records: Vec<RecordedTapeRecord>,
    cursor: u64,
    configured_block_size: Option<u32>,
}

impl RecordingRawTapeSource {
    fn from_sink(raw: &RecordingRawTapeSink) -> Self {
        let mut block_iter = raw.blocks.iter();
        let mut records = Vec::new();
        for event in &raw.events {
            match event {
                RawSinkEvent::WriteBlock(_) => {
                    let block = block_iter
                        .next()
                        .expect("recorded block event has matching bytes")
                        .clone();
                    records.push(RecordedTapeRecord::Block(block));
                }
                RawSinkEvent::WriteFilemark => records.push(RecordedTapeRecord::Filemark),
                RawSinkEvent::Position => {}
            }
        }
        assert!(
            block_iter.next().is_none(),
            "recorded raw sink has unreferenced block bytes"
        );
        Self {
            records,
            cursor: 0,
            configured_block_size: None,
        }
    }
}

impl RawTapeSource for RecordingRawTapeSource {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        self.configured_block_size = Some(block_size);
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        if hint.partition != 0 {
            return Err(ParityError::Invariant(
                "recording raw source only supports partition 0",
            ));
        }
        self.cursor = hint.lba;
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        if count != 0 {
            return Err(ParityError::Invariant(
                "recording raw source test fixture does not space filemarks",
            ));
        }
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: 0,
            position_after: PhysicalPositionHint::new(self.cursor),
            hit_end_of_data: false,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let index = usize::try_from(self.cursor).map_err(|_| {
            ParityError::Invariant("recording raw source cursor does not fit usize")
        })?;
        let Some(record) = self.records.get(index) else {
            return Ok(RawReadOutcome::EndOfData {
                position_after: PhysicalPositionHint::new(self.cursor),
            });
        };

        match record {
            RecordedTapeRecord::Block(block) => {
                if let Some(configured) = self.configured_block_size {
                    if block.len() != configured as usize {
                        return Err(ParityError::Invariant(
                            "recorded block length does not match fixed block size",
                        ));
                    }
                }
                if block.len() > buf.len() {
                    return Err(ParityError::Invariant(
                        "recording raw source read buffer is too small",
                    ));
                }
                buf[..block.len()].copy_from_slice(block);
                self.cursor += 1;
                Ok(RawReadOutcome::Block {
                    bytes: block.len(),
                    position_after: PhysicalPositionHint::new(self.cursor),
                })
            }
            RecordedTapeRecord::Filemark => {
                self.cursor += 1;
                Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor),
                })
            }
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

#[derive(Debug)]
struct RecordingJournal {
    tape_uuid: [u8; 16],
    bundles: Vec<CommittedBundle>,
}

impl RecordingJournal {
    fn new(tape_uuid: [u8; 16]) -> Self {
        Self {
            tape_uuid,
            bundles: Vec::new(),
        }
    }
}

impl TapeFileJournal for RecordingJournal {
    fn tape_uuid(&self) -> [u8; 16] {
        self.tape_uuid
    }

    fn commit_bundle(
        &mut self,
        bundle: &CommittedBundle,
    ) -> Result<(), crate::journal::JournalError> {
        self.bundles.push(bundle.clone());
        Ok(())
    }

    fn load_committed(
        &self,
    ) -> Result<crate::journal::CommittedState, crate::journal::JournalError> {
        let mut entries = Vec::new();
        let mut highest_protected_ordinal = 0;
        let mut total_committed_ordinals = 0;
        for bundle in &self.bundles {
            entries.extend(bundle.entries.clone());
            highest_protected_ordinal = bundle.highest_protected_ordinal;
            total_committed_ordinals = bundle.total_committed_ordinals;
        }
        Ok(crate::journal::CommittedState {
            entries,
            highest_protected_ordinal,
            total_committed_ordinals,
        })
    }
}

#[derive(Debug)]
struct EarlyWarningRawTapeSink {
    cursor: u64,
    block_count: usize,
    filemark_count: usize,
    ew_on_blocks: Vec<usize>,
    ew_on_filemarks: Vec<usize>,
    ew_blocks_seen: Vec<usize>,
    ew_filemarks_seen: Vec<usize>,
    blocks: Vec<Vec<u8>>,
}

impl EarlyWarningRawTapeSink {
    fn new(ew_on_blocks: Vec<usize>, ew_on_filemarks: Vec<usize>) -> Self {
        Self {
            cursor: 0,
            block_count: 0,
            filemark_count: 0,
            ew_on_blocks,
            ew_on_filemarks,
            ew_blocks_seen: Vec::new(),
            ew_filemarks_seen: Vec::new(),
            blocks: Vec::new(),
        }
    }
}

impl RawTapeSink for EarlyWarningRawTapeSink {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.block_count += 1;
        let early_warning = self.ew_on_blocks.contains(&self.block_count);
        if early_warning {
            self.ew_blocks_seen.push(self.block_count);
        }
        self.blocks.push(buf.to_vec());
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteBlock {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning,
            end_of_medium: false,
        })
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.filemark_count += 1;
        let early_warning = self.ew_on_filemarks.contains(&self.filemark_count);
        if early_warning {
            self.ew_filemarks_seen.push(self.filemark_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning,
            end_of_medium: false,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

#[derive(Debug)]
struct FailingRawTapeSink {
    block_error: Option<ParityError>,
    cursor: u64,
}

impl RawTapeSink for FailingRawTapeSink {
    fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        if let Some(err) = self.block_error.take() {
            return Err(err);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteBlock {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: false,
            end_of_medium: false,
        })
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: false,
            end_of_medium: false,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

#[test]
fn sidecar_only_write_path_accumulates_without_retaining_data_shards() {
    let scheme = small_scheme();
    let block_size: u32 = 32;
    let mut raw = RecordingRawTapeSink::default();
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, scheme.clone(), sample_uuid(), block_size).unwrap();

    start_object(&mut sink, 12, block_size);
    let mut blocks = Vec::new();
    for i in 0..5 {
        let block = fixed_block(i as u8 + 1, block_size);
        sink.write_block(&block).unwrap();
        blocks.push(block);
    }

    let expected_partial = expected_epoch_parity(&scheme, &blocks, block_size);
    let expected_partial_by_stripe = expected_partial
        .chunks(scheme.parity_blocks_per_stripe as usize)
        .map(|stripe| stripe.to_vec())
        .collect::<Vec<_>>();
    assert_eq!(sink.parity_accumulators, expected_partial_by_stripe);
    assert_eq!(sink.current_epoch_data_crc64s.len(), 5);

    for i in 5..12 {
        let block = fixed_block(i as u8 + 1, block_size);
        sink.write_block(&block).unwrap();
        blocks.push(block);
    }

    assert_eq!(sink.data_blocks_in_neighborhood(), 0);
    assert_eq!(sink.pending_sidecars.len(), 1);
    assert_eq!(
        sink.pending_sidecars[0].parity_shards,
        expected_epoch_parity(&scheme, &blocks, block_size)
    );
    assert!(
        sink.parity_accumulators
            .iter()
            .flatten()
            .all(|shard| shard.iter().all(|byte| *byte == 0)),
        "new epoch accumulators should be reset after queuing a sidecar"
    );
    assert!(sink.current_epoch_data_crc64s.is_empty());
}

#[test]
fn completed_sidecar_queue_is_bounded_by_object_reserve() {
    let block_size: u32 = 32;
    let mut raw = RecordingRawTapeSink::default();
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size).unwrap();

    start_object(&mut sink, 12, block_size);
    sink.active_object
        .as_mut()
        .expect("active object")
        .pending_sidecar_limit = 0;

    for i in 0..11 {
        let block = fixed_block(i as u8 + 1, block_size);
        sink.write_block(&block).unwrap();
    }
    let err = sink
        .write_block(&fixed_block(12, block_size))
        .expect_err("unbudgeted completed sidecar must fail");

    assert!(
        err.to_string()
            .contains("completed sidecar count exceeded object-start capacity reserve"),
        "{err}"
    );
    assert!(sink.poisoned);
    assert!(sink.pending_sidecars.is_empty());
}

#[test]
fn wrong_length_first_object_block_does_not_pin_session_block_size() {
    let block_size: u32 = 32;
    let mut raw = RecordingRawTapeSink::default();
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size).unwrap();

    start_object(&mut sink, 1, block_size);
    let err = sink.write_block(&[0xAA; 8]).expect_err("wrong block size");
    match err {
        TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
            assert!(message.contains("configured fixed block size"));
        }
        other => panic!("expected fixed block size rejection, got {other:?}"),
    }
    assert_eq!(sink.active_object_blocks_written(), Some(0));

    sink.write_block(&fixed_block(0xBB, block_size))
        .expect("correct first block should still write");

    assert_eq!(sink.active_object_blocks_written(), Some(1));
    assert_eq!(raw.blocks.len(), 1);
}

#[test]
fn emit_parity_before_first_data_write_returns_invariant_error() {
    let block_size: u32 = 32;
    let mut raw = RecordingRawTapeSink::default();
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size).unwrap();

    let err = sink
        .emit_parity_for_neighborhood()
        .expect_err("pre-data parity emission returns an invariant");

    assert!(matches!(err, ParityError::Invariant(_)));
    assert!(sink.poisoned);
}

#[test]
fn new_rejects_invalid_scheme() {
    let bad = ParityScheme {
        id: SchemeId::new_static("bad"),
        data_blocks_per_stripe: 4,
        parity_blocks_per_stripe: 5, // m > k
        stripes_per_neighborhood: 1,
    };
    let mut raw = RecordingRawTapeSink::default();
    match ParitySink::new(&mut raw, bad, sample_uuid(), 64) {
        Err(ParityError::InvalidScheme(_)) => {}
        Err(other) => panic!("expected InvalidScheme, got {other:?}"),
        Ok(_) => panic!("expected error"),
    }
}

#[test]
fn new_rejects_zero_block_size() {
    let mut raw = RecordingRawTapeSink::default();
    match ParitySink::new(&mut raw, small_scheme(), sample_uuid(), 0) {
        Err(ParityError::InvalidScheme(msg)) => {
            assert!(msg.contains("block_size_bytes"));
        }
        Err(other) => panic!("expected InvalidScheme, got {other:?}"),
        Ok(_) => panic!("expected error, got ok"),
    }
}

#[test]
fn journaled_sink_commits_object_bundle_as_one_record() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        start_object(&mut sink, 12, block_size);
        for i in 0..12 {
            sink.write_block(&fixed_block(i + 1, block_size))
                .expect("object block writes");
        }
        let summary = sink.finish_object().expect("object closes");
        assert_eq!(summary.sidecars_emitted.len(), 1);
    }

    assert_eq!(journal.bundles.len(), 1);
    let bundle = &journal.bundles[0];
    assert_eq!(bundle.kind, CommittedBundleKind::Object);
    assert_eq!(bundle.highest_protected_ordinal, 12);
    assert_eq!(bundle.total_committed_ordinals, 12);
    assert_eq!(bundle.entries.len(), 2);
    assert_eq!(bundle.entries[0].kind, TapeFileKind::Object);
    assert_eq!(bundle.entries[0].block_count, 12);
    assert_eq!(bundle.entries[1].kind, TapeFileKind::ParitySidecar);
    assert!(bundle.entries[1].canonical_metadata_hash.is_some());
}

#[test]
fn journaled_write_bootstrap_commits_control_bundle() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        assert_eq!(sink.write_bootstrap().expect("bootstrap writes"), 0);
    }

    assert_eq!(journal.bundles.len(), 1);
    let bundle = &journal.bundles[0];
    assert_eq!(bundle.kind, CommittedBundleKind::Control);
    assert_eq!(bundle.highest_protected_ordinal, 0);
    assert_eq!(bundle.total_committed_ordinals, 0);
    assert_eq!(bundle.entries.len(), 1);
    assert_eq!(bundle.entries[0].kind, TapeFileKind::Bootstrap);
}

#[test]
fn checkpoint_returns_committed_prefix_summary() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    let checkpoint = {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        start_object(&mut sink, 3, block_size);
        for i in 0..3 {
            sink.write_block(&fixed_block(i + 1, block_size))
                .expect("object block writes");
        }
        sink.finish_object().expect("object closes");
        sink.checkpoint().expect("checkpoint writes")
    };

    assert_eq!(checkpoint.bootstrap_tape_file_number, 1);
    assert_eq!(checkpoint.tape_file_count, 2);
    assert_eq!(checkpoint.highest_protected_ordinal, 0);
    assert_eq!(checkpoint.total_committed_ordinals, 3);
    assert_eq!(journal.bundles.len(), 2);
    assert_eq!(journal.bundles[1].kind, CommittedBundleKind::Control);
    assert_eq!(journal.bundles[1].total_committed_ordinals, 3);
}

#[test]
fn checkpoint_rejects_mid_object() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    let mut sink = ParitySink::new_with_journal(
        &mut raw,
        &mut journal,
        small_scheme(),
        sample_uuid(),
        block_size,
    )
    .expect("journaled sink opens");
    start_object(&mut sink, 3, block_size);
    sink.write_block(&fixed_block(1, block_size))
        .expect("object block writes");

    let err = sink
        .checkpoint()
        .expect_err("checkpoint must reject active object");
    match err {
        ParityError::Invariant(message) => {
            assert!(message.contains("object is active"), "{message}");
        }
        other => panic!("expected active-object invariant, got {other:?}"),
    }
}

#[test]
fn checkpoint_resume_rebuilds_open_epoch_and_finish_protects_everything() {
    let block_size: u32 = 1024;
    let scheme = small_scheme();
    let pre_checkpoint_blocks = (1..=5)
        .map(|seed| fixed_block(seed, block_size))
        .collect::<Vec<_>>();
    let pre_resume_row =
        BootstrapObjectRow::plaintext(0, pre_checkpoint_blocks.len() as u64, 0, 1, 1, [0x11; 32])
            .with_object_id([0x11; 16]);
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    let checkpoint = {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            scheme.clone(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        sink.begin_object_with_capacity_reserve_and_bootstrap_object_row(
            capacity_input_with_block_size(pre_checkpoint_blocks.len() as u64, 10_000, block_size),
            BootstrapObjectRowAdmission::PlaintextRao,
        )
        .expect("pre-checkpoint object reserve fits");
        for block in &pre_checkpoint_blocks {
            sink.write_block(block)
                .expect("pre-checkpoint object block");
        }
        sink.record_bootstrap_object_row(pre_resume_row.clone())
            .expect("pre-resume object row records");
        let summary = sink.finish_object().expect("pre-checkpoint object closes");
        assert_eq!(summary.bootstrap_object_row.as_ref(), Some(&pre_resume_row));
        assert_eq!(summary.highest_protected_ordinal, 0);
        assert!(
            summary.sidecars_emitted.is_empty(),
            "checkpointed prefix intentionally leaves one partial epoch live"
        );
        sink.checkpoint()
            .expect("checkpoint writes clean resume point")
    };

    assert_eq!(checkpoint.bootstrap_tape_file_number, 1);
    assert_eq!(checkpoint.total_committed_ordinals, 5);
    assert_eq!(checkpoint.highest_protected_ordinal, 0);

    let (committed_state, committed_prefix) =
        crate::resume::committed_prefix_from_journal(&journal, &scheme)
            .expect("journal prefix replays");
    assert_eq!(committed_state.highest_protected_ordinal, 0);
    assert_eq!(committed_state.total_committed_ordinals, 5);
    assert_eq!(committed_prefix.tape_file_count(), 2);

    let mut source = RecordingRawTapeSource::from_sink(&raw);
    let rebuild = crate::resume::rebuild_open_epoch_from_committed_prefix(
        &mut source,
        &committed_prefix,
        &scheme,
        sample_uuid(),
        block_size,
    )
    .expect("checkpointed partial epoch rebuilds");
    let journal_plan = crate::resume::plan_resume_append_from_journal(&journal, &scheme)
        .expect("journal resume plan builds");
    assert_eq!(journal_plan, rebuild.plan);
    assert!(rebuild.rebuilt_sidecars.is_empty());
    assert_eq!(rebuild.plan.append_after_tape_file_number, 1);
    assert_eq!(rebuild.plan.highest_protected_ordinal_before_rebuild, 0);
    assert_eq!(rebuild.plan.highest_protected_ordinal_after_rebuild, 0);
    assert_eq!(rebuild.plan.live_epoch_start, 0);
    assert_eq!(rebuild.plan.next_data_ordinal, 5);

    let live_epoch = rebuild
        .live_epoch
        .clone()
        .expect("checkpoint resume keeps partial epoch live");
    assert_eq!(live_epoch.protected_ordinal_start, 0);
    assert_eq!(live_epoch.next_data_ordinal, 5);
    assert_eq!(live_epoch.data_blocks_in_epoch, 5);
    let stripes = scheme.stripes_per_neighborhood as usize;
    let expected_stripe_buffers = (0..stripes)
        .map(|stripe_index| {
            pre_checkpoint_blocks
                .iter()
                .enumerate()
                .filter_map(|(ordinal, block)| {
                    (ordinal % stripes == stripe_index).then_some(block.clone())
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(live_epoch.stripe_buffers, expected_stripe_buffers);
    assert_eq!(
        live_epoch.data_shard_crc64s,
        pre_checkpoint_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect::<Vec<_>>()
    );

    let resume_plan = rebuild.plan.clone();
    let resume_result = resume_plan
        .complete(Vec::new())
        .expect("no sidecars are emitted for a partial-epoch rebuild");
    let append_position = committed_prefix
        .append_position_after_prefix()
        .expect("checkpoint prefix append position computes");
    let mut resumed_raw = RecordingRawTapeSink {
        cursor: append_position.lba,
        events: Vec::new(),
        blocks: Vec::new(),
    };
    {
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut resumed_raw,
            scheme.clone(),
            sample_uuid(),
            block_size,
            ResumeWriterSeed {
                committed_prefix: &committed_prefix,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&committed_prefix),
                committed_prefix_object_rows: vec![pre_resume_row.clone()],
                resume_result: &resume_result,
                live_epoch: rebuild.live_epoch,
                next_bootstrap_sequence: 1,
            },
        )
        .expect("resumed writer opens at checkpoint append point");
        assert_eq!(sink.neighborhood_idx(), 0);
        assert_eq!(sink.data_blocks_in_neighborhood(), 5);

        sink.begin_object_with_capacity_reserve_and_bootstrap_object_row(
            capacity_input_with_current_fill(
                7,
                10_000,
                block_size,
                sink.data_blocks_in_neighborhood(),
            ),
            BootstrapObjectRowAdmission::PlaintextRao,
        )
        .expect("post-resume object reserve fits");
        for seed in 6..=12 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("post-resume object block");
        }
        let post_resume_row =
            BootstrapObjectRow::plaintext(2, 7, 0, 1, 1, [0x22; 32]).with_object_id([0x22; 16]);
        sink.record_bootstrap_object_row(post_resume_row.clone())
            .expect("post-resume object row records");
        let summary = sink.finish_object().expect("post-resume object closes");
        assert_eq!(
            summary.bootstrap_object_row.as_ref(),
            Some(&post_resume_row)
        );
        assert_eq!(summary.tape_file_number, 2);
        assert_eq!(summary.first_parity_data_ordinal, 5);
        assert_eq!(summary.data_block_count, 7);
        assert_eq!(summary.highest_protected_ordinal, 12);
        assert_eq!(summary.sidecars_emitted.len(), 1);
        assert_eq!(summary.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            summary.sidecars_emitted[0].protected_ordinal_end_exclusive,
            12
        );

        let geometry = sink
            .finish()
            .expect("resumed finish writes final bootstrap");
        assert_eq!(geometry.data_area_end_lba, append_position.lba + 7);
    }

    let sidecar_starts = resumed_raw
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            matches!(
                crate::sidecar::classify_sidecar_header_block(block, &sample_uuid()),
                Ok(Some(_))
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(sidecar_starts.len(), 1);
    let sidecar = crate::sidecar::parse_sidecar_tape_file(
        &resumed_raw.blocks[sidecar_starts[0]..resumed_raw.blocks.len() - 1],
        &sample_uuid(),
    )
    .expect("post-resume sidecar parses");
    assert_eq!(sidecar.header.protected_ordinal_start, 0);
    assert_eq!(sidecar.header.protected_ordinal_end_exclusive, 12);

    let final_bootstrap =
        crate::bootstrap::parse_bootstrap_block(resumed_raw.blocks.last().unwrap())
            .expect("final bootstrap parses");
    let digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(digest.is_final_map);
    assert_eq!(digest.tape_file_count, 5);
    assert_eq!(digest.map_total_data_ordinals, 12);
    assert_eq!(digest.highest_protected_ordinal, 12);
    assert_eq!(
        final_bootstrap.object_rows,
        vec![
            pre_resume_row,
            BootstrapObjectRow::plaintext(2, 7, 0, 1, 1, [0x22; 32]).with_object_id([0x22; 16])
        ]
    );
}

#[test]
fn resume_rejects_object_rows_that_cannot_fit_bootstrap() {
    let block_size: u32 = 512;
    let object_count = 120u32;
    let entries = (0..object_count)
        .map(|tape_file_number| {
            TapeFileMapEntry::object(tape_file_number, 1, u64::from(tape_file_number))
        })
        .collect::<Vec<_>>();
    let committed_prefix = FilemarkMap::new(entries).expect("object-only prefix validates");
    let committed_object_rows = (0..object_count)
        .map(|tape_file_number| {
            BootstrapObjectRow::plaintext(
                tape_file_number,
                1,
                0,
                1,
                1,
                [tape_file_number as u8; 32],
            )
            .with_object_id([tape_file_number as u8; 16])
        })
        .collect::<Vec<_>>();
    let append_position = committed_prefix
        .append_position_after_prefix()
        .expect("object-only append position computes");
    let mut raw = RecordingRawTapeSink {
        cursor: append_position.lba,
        events: Vec::new(),
        blocks: Vec::new(),
    };
    let resume_result = ResumeAppendResult {
        append_after_tape_file_number: object_count - 1,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 0,
        live_epoch_start: u64::from(object_count),
        next_data_ordinal: u64::from(object_count),
    };

    let result = ParitySink::new_sidecar_only_from_resume(
        &mut raw,
        small_scheme(),
        sample_uuid(),
        block_size,
        ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: Vec::new(),
            committed_prefix_object_rows: committed_object_rows,
            resume_result: &resume_result,
            live_epoch: None,
            next_bootstrap_sequence: 0,
        },
    );
    let err = match result {
        Ok(_) => panic!("oversized object-row set must be rejected before append"),
        Err(err) => err,
    };

    assert!(
        matches!(err, ParityError::BootstrapPayloadTooLarge { .. }),
        "{err:?}"
    );
}

#[test]
fn bootstrap_placement_policy_bundle_floor_folds_into_object_bundle() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    let summary = {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        sink.set_bootstrap_placement_policy(BootstrapPlacementPolicy {
            bundles_per_bootstrap: 1,
            ordinals_per_bootstrap: u64::MAX,
            eom_taper: Vec::new(),
            min_physical_separation_blocks: 0,
        })
        .expect("placement policy installs");
        start_object(&mut sink, 3, block_size);
        for seed in 1..=3 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("object block writes");
        }
        sink.finish_object().expect("object closes")
    };

    assert_eq!(summary.tape_file_number, 1);
    assert_eq!(summary.control_tape_files_emitted.len(), 1);
    assert_eq!(
        summary.control_tape_files_emitted[0].to_map_entry(),
        TapeFileMapEntry::bootstrap(2, 1)
    );
    assert_eq!(journal.bundles.len(), 2);
    assert_eq!(journal.bundles[1].kind, CommittedBundleKind::Object);
    assert_eq!(journal.bundles[1].entries.len(), 2);
    assert_eq!(journal.bundles[1].entries[0].kind, TapeFileKind::Object);
    assert_eq!(journal.bundles[1].entries[1].kind, TapeFileKind::Bootstrap);
    assert_eq!(journal.bundles[1].total_committed_ordinals, 3);
}

#[test]
fn bootstrap_placement_policy_min_separation_defers_tripped_floor() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
            .expect("sink opens");
    assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
    sink.set_bootstrap_placement_policy(BootstrapPlacementPolicy {
        bundles_per_bootstrap: 1,
        ordinals_per_bootstrap: u64::MAX,
        eom_taper: Vec::new(),
        min_physical_separation_blocks: 5,
    })
    .expect("placement policy installs");

    start_object(&mut sink, 1, block_size);
    sink.write_block(&fixed_block(1, block_size))
        .expect("first object block");
    let first = sink.finish_object().expect("first object closes");
    assert!(first.control_tape_files_emitted.is_empty());

    sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(
        1,
        10_000,
        block_size,
        sink.data_blocks_in_neighborhood(),
    ))
    .expect("second object reserve fits");
    sink.write_block(&fixed_block(2, block_size))
        .expect("second object block");
    let second = sink.finish_object().expect("second object closes");
    assert_eq!(second.control_tape_files_emitted.len(), 1);
    assert_eq!(
        second.control_tape_files_emitted[0].to_map_entry(),
        TapeFileMapEntry::bootstrap(3, 1)
    );
}

#[test]
fn bootstrap_placement_policy_eom_taper_tightens_floor() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
            .expect("sink opens");
    assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
    sink.set_bootstrap_placement_policy(BootstrapPlacementPolicy {
        bundles_per_bootstrap: 4,
        ordinals_per_bootstrap: u64::MAX,
        eom_taper: vec![(0.9, 2)],
        min_physical_separation_blocks: 0,
    })
    .expect("placement policy installs");

    sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(1, 25, block_size, 0))
        .expect("first object reserve fits");
    sink.write_block(&fixed_block(1, block_size))
        .expect("first object block");
    let first = sink.finish_object().expect("first object closes");
    assert!(
        first.control_tape_files_emitted.is_empty(),
        "taper alone should not emit until the tightened bundle floor is reached"
    );

    sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(
        1,
        23,
        block_size,
        sink.data_blocks_in_neighborhood(),
    ))
    .expect("second object reserve fits");
    sink.write_block(&fixed_block(2, block_size))
        .expect("second object block");
    let second = sink.finish_object().expect("second object closes");
    assert_eq!(
        second.control_tape_files_emitted.len(),
        1,
        "remaining fraction crossed 90%, so bundles floor tightens from 4 to 2"
    );
    assert_eq!(
        second.control_tape_files_emitted[0].to_map_entry(),
        TapeFileMapEntry::bootstrap(3, 1)
    );
}

#[test]
fn bootstrap_placement_policy_rejects_non_monotone_eom_taper() {
    let unordered_fraction = BootstrapPlacementPolicy {
        bundles_per_bootstrap: 4,
        ordinals_per_bootstrap: 64,
        eom_taper: vec![(0.01, 4), (0.10, 8)],
        min_physical_separation_blocks: 0,
    };
    let err = unordered_fraction
        .validate()
        .expect_err("fractions must be descending");
    match err {
        ParityError::Invariant(message) => {
            assert!(
                message.contains("descending remaining_fraction"),
                "{message}"
            );
        }
        other => panic!("expected taper ordering invariant, got {other:?}"),
    }

    let unordered_divisor = BootstrapPlacementPolicy {
        bundles_per_bootstrap: 4,
        ordinals_per_bootstrap: 64,
        eom_taper: vec![(0.10, 4), (0.01, 2)],
        min_physical_separation_blocks: 0,
    };
    let err = unordered_divisor
        .validate()
        .expect_err("divisors must be increasing");
    match err {
        ParityError::Invariant(message) => {
            assert!(
                message.contains("strictly increasing divisors"),
                "{message}"
            );
        }
        other => panic!("expected taper ordering invariant, got {other:?}"),
    }

    BootstrapPlacementPolicy {
        bundles_per_bootstrap: 4,
        ordinals_per_bootstrap: 64,
        eom_taper: vec![(0.10, 2), (0.01, 4)],
        min_physical_separation_blocks: 0,
    }
    .validate()
    .expect("design example ordering is valid");
}

#[test]
fn checkpoint_resets_bootstrap_placement_counters() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    let second = {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        assert_eq!(sink.write_bootstrap().expect("BOT bootstrap"), 0);
        sink.set_bootstrap_placement_policy(BootstrapPlacementPolicy {
            bundles_per_bootstrap: 2,
            ordinals_per_bootstrap: u64::MAX,
            eom_taper: Vec::new(),
            min_physical_separation_blocks: 0,
        })
        .expect("placement policy installs");

        start_object(&mut sink, 1, block_size);
        sink.write_block(&fixed_block(1, block_size))
            .expect("first object block");
        let first = sink.finish_object().expect("first object closes");
        assert!(first.control_tape_files_emitted.is_empty());

        let checkpoint = sink.checkpoint().expect("checkpoint writes");
        assert_eq!(checkpoint.bootstrap_tape_file_number, 2);

        sink.begin_object_with_capacity_reserve(capacity_input_with_current_fill(
            1,
            10_000,
            block_size,
            sink.data_blocks_in_neighborhood(),
        ))
        .expect("second object reserve fits");
        sink.write_block(&fixed_block(2, block_size))
            .expect("second object block");
        sink.finish_object().expect("second object closes")
    };

    assert!(
        second.control_tape_files_emitted.is_empty(),
        "checkpoint reset the placement counters; the second object alone should not trip floor=2"
    );
    assert_eq!(
        journal
            .bundles
            .iter()
            .filter(|bundle| bundle.kind == CommittedBundleKind::Control)
            .count(),
        2,
        "only BOT and checkpoint control bundles should have been written"
    );
}

#[test]
fn journaled_finish_commits_final_sidecar_and_bootstrap_bundle() {
    let block_size: u32 = 1024;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled sink opens");
        start_object(&mut sink, 5, block_size);
        for i in 0..5 {
            sink.write_block(&fixed_block(i + 1, block_size))
                .expect("object block writes");
        }
        sink.finish_object().expect("object closes");
        sink.finish().expect("finish writes final bundle");
    }

    assert_eq!(journal.bundles.len(), 2);
    assert_eq!(journal.bundles[0].kind, CommittedBundleKind::Object);
    assert_eq!(journal.bundles[0].entries.len(), 1);
    assert_eq!(journal.bundles[0].entries[0].kind, TapeFileKind::Object);

    let finish_bundle = &journal.bundles[1];
    assert_eq!(finish_bundle.kind, CommittedBundleKind::Finish);
    assert_eq!(finish_bundle.highest_protected_ordinal, 5);
    assert_eq!(finish_bundle.total_committed_ordinals, 5);
    assert!(
        finish_bundle
            .entries
            .iter()
            .any(|entry| entry.kind == TapeFileKind::ParitySidecar),
        "final partial epoch sidecar should be journaled"
    );
    assert_eq!(
        finish_bundle.entries.last().map(|entry| entry.kind),
        Some(TapeFileKind::Bootstrap)
    );
}

#[test]
fn sidecar_only_writer_uses_raw_sink_filemark_barriers() {
    let block_size: u32 = 512;
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        sink.write_bootstrap().expect("initial bootstrap");
        start_object(&mut sink, 3, block_size);
        for seed in 1..=3 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("object block");
        }
        sink.finish_object().expect("object filemark");
        sink.finish().expect("final sidecar and bootstrap");
    }

    let filemark_count = raw
        .events
        .iter()
        .filter(|event| matches!(event, RawSinkEvent::WriteFilemark))
        .count();
    assert_eq!(
            filemark_count, 4,
            "initial bootstrap, object, final partial sidecar, and final bootstrap each need a raw filemark barrier"
        );
    assert!(raw.events.iter().all(|event| match event {
        RawSinkEvent::WriteBlock(len) => *len == block_size as usize,
        RawSinkEvent::WriteFilemark | RawSinkEvent::Position => true,
    }));
    assert!(matches!(
        raw.events.as_slice(),
        [
            RawSinkEvent::WriteBlock(512),
            RawSinkEvent::WriteFilemark,
            RawSinkEvent::WriteBlock(512),
            RawSinkEvent::WriteBlock(512),
            RawSinkEvent::WriteBlock(512),
            RawSinkEvent::WriteFilemark,
            ..
        ]
    ));
    assert!(matches!(
        raw.events.last(),
        Some(RawSinkEvent::WriteFilemark)
    ));
}

#[test]
fn sidecar_and_bootstrap_tape_files_do_not_advance_data_ordinals() {
    let block_size: u32 = 512;
    let mut raw = RecordingRawTapeSink::default();
    let _final_geometry = {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        assert_eq!(sink.write_bootstrap().expect("initial bootstrap"), 0);

        start_object(&mut sink, 12, block_size);
        for seed in 0..12 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("first object block");
        }
        let first = sink.finish_object().expect("first object closes");
        assert_eq!(first.tape_file_number, 1);
        assert_eq!(first.first_parity_data_ordinal, 0);
        assert_eq!(first.data_block_count, 12);
        assert_eq!(first.sidecars_emitted.len(), 1);
        assert_eq!(first.sidecars_emitted[0].tape_file_number, 2);
        assert_eq!(first.sidecars_emitted[0].protected_ordinal_start, 0);
        assert_eq!(
            first.sidecars_emitted[0].protected_ordinal_end_exclusive,
            12
        );
        assert_eq!(first.highest_protected_ordinal, 12);
        let first_bundle = first.committed_bundle().expect("first bundle builds");
        assert_eq!(first_bundle.kind, CommittedBundleKind::Object);
        assert_eq!(first_bundle.entries.len(), 2);
        assert_eq!(
            first_bundle.entries[0].to_map_entry(),
            TapeFileMapEntry::object(1, 12, 0)
        );
        assert_eq!(
            first_bundle.entries[1],
            first.sidecars_emitted[0].tape_file_entry()
        );
        assert_eq!(first_bundle.highest_protected_ordinal, 12);
        assert_eq!(first_bundle.total_committed_ordinals, 12);

        assert_eq!(sink.write_bootstrap().expect("intermediate bootstrap"), 3);

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                2, 10_000, block_size,
            ))
            .expect("second object reserve fits");
        assert_eq!(second_tape_file, 4);
        for seed in 12..14 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("second object block");
        }
        let second = sink.finish_object().expect("second object closes");
        assert_eq!(second.tape_file_number, 4);
        assert_eq!(
            second.first_parity_data_ordinal, 12,
            "bootstrap and sidecar tape files between objects must not consume data ordinals"
        );
        assert_eq!(second.data_block_count, 2);
        assert!(second.sidecars_emitted.is_empty());
        assert_eq!(second.highest_protected_ordinal, 12);
        let second_bundle = second.committed_bundle().expect("second bundle builds");
        assert_eq!(second_bundle.kind, CommittedBundleKind::Object);
        assert_eq!(second_bundle.entries.len(), 1);
        assert_eq!(
            second_bundle.entries[0].to_map_entry(),
            TapeFileMapEntry::object(4, 2, 12)
        );
        assert_eq!(second_bundle.highest_protected_ordinal, 12);
        assert_eq!(second_bundle.total_committed_ordinals, 14);
        assert!(
            second_bundle.total_committed_ordinals - second_bundle.highest_protected_ordinal < 12,
            "v1 object bundles may leave only a partial epoch unprotected"
        );

        sink.finish().expect("final partial sidecar and bootstrap")
    };

    assert_eq!(
        raw.events
            .iter()
            .filter(|event| matches!(event, RawSinkEvent::WriteFilemark))
            .count(),
        7,
        "each bootstrap, object, and sidecar tape file is delimited by its own filemark"
    );

    let sidecar_starts = raw
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            matches!(
                crate::sidecar::classify_sidecar_header_block(block, &sample_uuid()),
                Ok(Some(_))
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(sidecar_starts.len(), 2);

    let final_sidecar = crate::sidecar::parse_sidecar_tape_file(
        &raw.blocks[sidecar_starts[1]..raw.blocks.len() - 1],
        &sample_uuid(),
    )
    .expect("final partial sidecar parses");
    assert_eq!(final_sidecar.header.protected_ordinal_start, 12);
    assert_eq!(final_sidecar.header.protected_ordinal_end_exclusive, 14);
    assert_eq!(final_sidecar.header.real_data_shard_count, 2);

    let final_bootstrap = crate::bootstrap::parse_bootstrap_block(raw.blocks.last().unwrap())
        .expect("final bootstrap parses");
    let digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(digest.is_final_map);
    assert_eq!(digest.tape_file_count, 7);
    assert_eq!(
        digest.map_total_data_ordinals, 14,
        "only object blocks, not sidecar/bootstrap blocks, contribute to ParityDataOrdinal"
    );
    assert_eq!(digest.highest_protected_ordinal, 14);
}

#[test]
fn object_bundle_bound_rejects_full_unprotected_epoch() {
    let block_size: u32 = 512;
    let mut raw = RecordingRawTapeSink::default();
    let sink = ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
        .expect("sidecar-only raw sink constructs");

    let err = sink
        .validate_v1_post_object_bundle_bound(0, 12)
        .expect_err("one full unprotected epoch violates v1 committed state");

    match err {
        ParityError::Invariant(message) => {
            assert!(message.contains("bounded restart invariant"), "{message}");
        }
        other => panic!("expected invariant error, got {other:?}"),
    }
}

#[test]
fn final_bootstrap_carries_inline_sidecar_epoch_directory_when_it_fits() {
    let block_size: u32 = 2048;
    let mut raw = RecordingRawTapeSink::default();
    let sidecar_summary = {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        sink.write_bootstrap().expect("initial bootstrap");
        start_object(&mut sink, 12, block_size);
        for seed in 0..12 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("object block");
        }
        let object = sink.finish_object().expect("object closes");
        assert_eq!(object.sidecars_emitted.len(), 1);
        let sidecar_summary = object.sidecars_emitted[0].clone();
        sink.finish().expect("final bootstrap writes");
        sidecar_summary
    };

    let final_bootstrap = crate::bootstrap::parse_bootstrap_block(raw.blocks.last().unwrap())
        .expect("final bootstrap parses");
    assert!(final_bootstrap.parity_map_reference.is_none());
    let directory = final_bootstrap
        .sidecar_epoch_directory
        .expect("final bootstrap carries inline sidecar directory");
    assert_eq!(directory.directory_scope_tape_file_count, 4);
    assert_eq!(directory.directory_scope_total_data_ordinals, 12);
    assert_eq!(directory.directory_scope_highest_protected_ordinal, 12);
    assert!(directory.is_final_directory);
    assert_eq!(directory.entries.len(), 1);
    let entry = &directory.entries[0];
    assert_eq!(entry.tape_file_number, sidecar_summary.tape_file_number);
    assert_eq!(entry.epoch_id, sidecar_summary.epoch_id);
    assert_eq!(
        entry.canonical_metadata_hash,
        sidecar_summary.canonical_metadata_hash
    );
    assert_eq!(
        entry.flags,
        SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD
    );
}

#[test]
fn final_bootstrap_references_parity_map_when_directory_overflows_inline_space() {
    let block_size: u32 = 512;
    let object_count = 48u8;
    let mut raw = RecordingRawTapeSink::default();
    let mut journal = RecordingJournal::new(sample_uuid());
    {
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            small_scheme(),
            sample_uuid(),
            block_size,
        )
        .expect("journaled raw sink constructs");
        sink.write_bootstrap().expect("initial bootstrap");
        for object_index in 0..object_count {
            start_object(&mut sink, 12, block_size);
            for offset in 0..12 {
                sink.write_block(&fixed_block(
                    object_index.wrapping_mul(13).wrapping_add(offset),
                    block_size,
                ))
                .expect("object block");
            }
            let object = sink.finish_object().expect("object closes");
            assert_eq!(object.sidecars_emitted.len(), 1);
        }
        sink.finish().expect("final parity_map and bootstrap write");
    }

    let final_bootstrap = crate::bootstrap::parse_bootstrap_block(raw.blocks.last().unwrap())
        .expect("final bootstrap parses");
    assert!(final_bootstrap.sidecar_epoch_directory.is_none());
    let reference = final_bootstrap
        .parity_map_reference
        .expect("final bootstrap references external parity_map");
    assert_eq!(reference.tape_file_number, u32::from(object_count) * 2 + 1);
    assert_eq!(
        reference.directory_scope_tape_file_count,
        u32::from(object_count) * 2 + 3
    );
    assert_eq!(
        reference.directory_scope_total_data_ordinals,
        u64::from(object_count) * 12
    );
    assert_eq!(
        reference.directory_scope_highest_protected_ordinal,
        u64::from(object_count) * 12
    );
    assert!(reference.is_final_directory);
    let final_bundle = journal.bundles.last().expect("final bundle was journaled");
    assert_eq!(final_bundle.kind, CommittedBundleKind::Finish);
    let parity_map_entry = final_bundle
        .entries
        .iter()
        .find(|entry| entry.kind == TapeFileKind::ParityMap)
        .expect("final bundle includes parity_map row");
    assert_eq!(
        parity_map_entry.canonical_metadata_hash,
        Some(reference.parity_map_payload_sha256)
    );

    let parity_map_start = raw
        .blocks
        .iter()
        .position(|block| {
            matches!(
                crate::parity_map::classify_parity_map_header_block(block, &sample_uuid()),
                Ok(Some(header))
                    if header.copy_kind == crate::parity_map::ParityMapCopyKind::Primary
            )
        })
        .expect("parity_map primary header block is present");
    let parity_map_end = parity_map_start + reference.block_count as usize;
    let decoded = crate::parity_map::parse_parity_map_tape_file(
        &raw.blocks[parity_map_start..parity_map_end],
        &sample_uuid(),
    )
    .expect("referenced parity_map parses");
    assert_eq!(
        decoded.header.payload_sha256,
        reference.parity_map_payload_sha256
    );
    assert_eq!(
        decoded.payload.canonical_map_digest,
        reference.canonical_map_digest
    );
    assert_eq!(
        decoded.payload.directory.entries.len(),
        usize::from(object_count)
    );
}

#[test]
fn raw_sink_error_context_survives_block_sink_wrapper() {
    let block_size: u32 = 512;
    let mut raw = FailingRawTapeSink {
        block_error: Some(ParityError::ResumeAppend(
            "catalog commit callback failed for tape_file 42".into(),
        )),
        cursor: 0,
    };
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
            .expect("sidecar-only raw sink constructs");

    let err = sink
        .write_bootstrap()
        .expect_err("raw fixed-block error should bubble");
    match err {
        ParityError::TapeIo(TapeIoError::OperationFailed(message)) => {
            assert!(message.contains("RawTapeSink operation failed"));
            assert!(message.contains("resume append error"));
            assert!(message.contains("catalog commit callback failed"));
            assert!(message.contains("tape_file 42"));
        }
        other => panic!("expected OperationFailed TapeIo error, got {other:?}"),
    }
}

#[test]
fn raw_sink_transport_error_remains_completion_unknown() {
    let block_size: u32 = 512;
    let mut raw = FailingRawTapeSink {
        block_error: Some(ParityError::TapeIo(TapeIoError::Transport(
            ScsiError::TransportError {
                status: 0,
                host_status: 0,
                driver_status: 0x06,
                info: 1,
                sense: Vec::new(),
            },
        ))),
        cursor: 0,
    };
    let mut sink =
        ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
            .expect("sidecar-only raw sink constructs");

    let err = sink
        .write_bootstrap()
        .expect_err("raw transport error should bubble");
    match err {
        ParityError::TapeIo(TapeIoError::Transport(scsi)) => {
            assert!(TapeIoError::Transport(scsi).is_completion_unknown());
        }
        other => panic!("expected completion-unknown transport error, got {other:?}"),
    }
}

#[test]
fn raw_backed_body_facing_write_filemarks_is_rejected_before_raw_barrier() {
    let block_size: u32 = 512;
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        for count in [0, 1, 2] {
            let err = BlockSink::write_filemarks(&mut sink, count)
                .expect_err("body-facing filemark writes must be disabled");
            match err {
                TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                    assert!(message.contains("body-facing write_filemarks"), "{message}");
                }
                other => panic!("expected InvalidInput for body filemark write, got {other:?}"),
            }
        }
    }
    assert!(raw.events.is_empty());
}

#[test]
fn sidecar_only_early_warning_on_object_and_sidecar_writes_does_not_abort() {
    let block_size: u32 = 512;
    // Blocks 1..12 are object data; block 13 is the first sidecar block.
    // Filemark 1 closes the object; filemark 2 closes the sidecar.
    let mut raw = EarlyWarningRawTapeSink::new(vec![1, 13], vec![1, 2]);
    let _final_geometry = {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);

        sink.write_block(&fixed_block(1, block_size))
            .expect("object data EW does not abort");
        for seed in 2..=12 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("remaining object blocks write");
        }

        let object = sink
            .finish_object()
            .expect("object and completed sidecar close despite EW");
        assert!(object.filemark_outcome.early_warning);
        assert!(!object.filemark_outcome.end_of_medium);
        assert_eq!(object.sidecars_emitted.len(), 1);
        assert!(object.sidecars_emitted[0].filemark_outcome.early_warning);
        assert!(!object.sidecars_emitted[0].filemark_outcome.end_of_medium);
        assert_eq!(object.highest_protected_ordinal, 12);

        sink.finish()
            .expect("final bootstrap still writes after EW-only sidecar close")
    };

    assert_eq!(raw.ew_blocks_seen, vec![1, 13]);
    assert_eq!(raw.ew_filemarks_seen, vec![1, 2]);
    assert!(
        raw.block_count > 13,
        "final bootstrap must still be written after EW-only object and sidecar phases"
    );
}

#[test]
fn sidecar_filemark_early_warning_does_not_mask_later_object_data_eom() {
    #[derive(Debug)]
    struct SidecarFilemarkEwThenObjectEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        events: Vec<RawSinkEvent>,
        ew_filemarks_seen: Vec<usize>,
        eom_blocks_seen: Vec<usize>,
    }

    impl RawTapeSink for SidecarFilemarkEwThenObjectEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            let end_of_medium = self.filemark_count >= 2 && self.eom_blocks_seen.is_empty();
            if end_of_medium {
                self.eom_blocks_seen.push(self.block_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            let early_warning = self.filemark_count == 2;
            if early_warning {
                self.ew_filemarks_seen.push(self.filemark_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = SidecarFilemarkEwThenObjectEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        events: Vec::new(),
        ew_filemarks_seen: Vec::new(),
        eom_blocks_seen: Vec::new(),
    };
    let sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("first object data writes before sidecar EW");
            assert!(!outcome.early_warning, "first object block {seed}");
            assert!(!outcome.end_of_medium, "first object block {seed}");
        }

        let first = sink
            .finish_object()
            .expect("sidecar filemark EW alone must still commit the sidecar");
        assert_eq!(first.sidecars_emitted.len(), 1);
        let sidecar = &first.sidecars_emitted[0];
        assert!(sidecar.filemark_outcome.early_warning);
        assert!(!sidecar.filemark_outcome.end_of_medium);
        assert_eq!(first.highest_protected_ordinal, 12);
        sidecar_block_count = sidecar.block_count;

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                1, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar EW");
        assert_eq!(second_tape_file, 2);

        let err = sink
            .write_block(&fixed_block(0xE3, block_size))
            .expect_err("later object data EOM must hard-abort despite prior sidecar EW");
        match err {
            TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                assert!(
                    message.contains("object data block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-data EOM invalid-input error, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("first object and sidecar map remains valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, sidecar_block_count, 0, 0, 12),
            ],
            "the later object-data EOM must not commit a second object map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not write a second object filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_filemarks_seen, vec![2]);
    assert_eq!(raw.eom_blocks_seen.len(), 1);
    assert!(
        raw.eom_blocks_seen[0] > 12 + sidecar_block_count as usize,
        "EOM must land on the next object after the committed sidecar"
    );

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    assert_eq!(
            raw.events, expected_events,
            "sidecar-filemark EW must not let a later object-data EOM write a filemark, sidecar, or bootstrap"
        );
}

#[test]
fn sidecar_filemark_early_warning_does_not_mask_later_object_filemark_eom() {
    #[derive(Debug)]
    struct SidecarFilemarkEwThenObjectFilemarkEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        events: Vec<RawSinkEvent>,
        ew_filemarks_seen: Vec<usize>,
        eom_filemarks_seen: Vec<usize>,
    }

    impl RawTapeSink for SidecarFilemarkEwThenObjectFilemarkEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            let early_warning = self.filemark_count == 2;
            if early_warning {
                self.ew_filemarks_seen.push(self.filemark_count);
            }
            let end_of_medium = self.filemark_count == 3;
            if end_of_medium {
                self.eom_filemarks_seen.push(self.filemark_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning,
                end_of_medium,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = SidecarFilemarkEwThenObjectFilemarkEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        events: Vec::new(),
        ew_filemarks_seen: Vec::new(),
        eom_filemarks_seen: Vec::new(),
    };
    let sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("first object data writes before sidecar EW");
            assert!(!outcome.early_warning, "first object block {seed}");
            assert!(!outcome.end_of_medium, "first object block {seed}");
        }

        let first = sink
            .finish_object()
            .expect("sidecar filemark EW alone must still commit the sidecar");
        assert_eq!(first.sidecars_emitted.len(), 1);
        let sidecar = &first.sidecars_emitted[0];
        assert!(sidecar.filemark_outcome.early_warning);
        assert!(!sidecar.filemark_outcome.end_of_medium);
        assert_eq!(first.highest_protected_ordinal, 12);
        sidecar_block_count = sidecar.block_count;

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                1, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar EW");
        assert_eq!(second_tape_file, 2);
        let outcome = sink
            .write_block(&fixed_block(0xE4, block_size))
            .expect("second object data writes before filemark EOM");
        assert!(!outcome.early_warning);
        assert!(!outcome.end_of_medium);

        let err = sink
            .finish_object()
            .expect_err("later object filemark EOM must hard-abort despite prior sidecar EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("object trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-filemark EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("first object and sidecar map remains valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, sidecar_block_count, 0, 0, 12),
            ],
            "the later object-filemark EOM must not commit a second object map entry"
        );

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_filemarks_seen, vec![2]);
    assert_eq!(raw.eom_filemarks_seen, vec![3]);
    assert_eq!(
        raw.block_count,
        12 + sidecar_block_count as usize + 1,
        "second object data block must be the only post-sidecar block before object-filemark EOM"
    );

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    expected_events.push(RawSinkEvent::WriteFilemark);
    assert_eq!(
            raw.events, expected_events,
            "sidecar-filemark EW must not let a later object-filemark EOM commit the object, write sidecars, or write bootstrap"
        );
}

#[test]
fn sidecar_filemark_early_warning_does_not_mask_later_sidecar_body_eom() {
    #[derive(Debug)]
    struct SidecarFilemarkEwThenSidecarBodyEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        events: Vec<RawSinkEvent>,
        ew_filemarks_seen: Vec<usize>,
        eom_blocks_seen: Vec<usize>,
    }

    impl RawTapeSink for SidecarFilemarkEwThenSidecarBodyEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            let end_of_medium = self.filemark_count >= 3 && self.eom_blocks_seen.is_empty();
            if end_of_medium {
                self.eom_blocks_seen.push(self.block_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            let early_warning = self.filemark_count == 2;
            if early_warning {
                self.ew_filemarks_seen.push(self.filemark_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = SidecarFilemarkEwThenSidecarBodyEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        events: Vec::new(),
        ew_filemarks_seen: Vec::new(),
        eom_blocks_seen: Vec::new(),
    };
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("first object data writes before sidecar EW");
            assert!(!outcome.early_warning, "first object block {seed}");
            assert!(!outcome.end_of_medium, "first object block {seed}");
        }

        let first = sink
            .finish_object()
            .expect("sidecar filemark EW alone must still commit the first sidecar");
        assert_eq!(first.sidecars_emitted.len(), 1);
        let first_sidecar = &first.sidecars_emitted[0];
        assert!(first_sidecar.filemark_outcome.early_warning);
        assert!(!first_sidecar.filemark_outcome.end_of_medium);
        assert_eq!(first.highest_protected_ordinal, 12);
        first_sidecar_block_count = first_sidecar.block_count;

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                12, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar EW");
        assert_eq!(second_tape_file, 2);
        for seed in 13..=24 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("second object data writes before sidecar-body EOM");
            assert!(!outcome.early_warning, "second object block {seed}");
            assert!(!outcome.end_of_medium, "second object block {seed}");
        }

        let err = sink
            .finish_object()
            .expect_err("later sidecar body EOM must hard-abort despite prior sidecar EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar-body EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("objects and first sidecar map remain valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
                TapeFileMapEntry::object(2, 12, 12),
            ],
            "the later sidecar-body EOM must not commit the second sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry the second sidecar");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_filemarks_seen, vec![2]);
    assert_eq!(raw.filemark_count, 3);
    assert_eq!(raw.eom_blocks_seen.len(), 1);
    assert_eq!(
        raw.eom_blocks_seen[0],
        12 + first_sidecar_block_count as usize + 12 + 1,
        "EOM must land on the first second-sidecar body block after the second object filemark"
    );

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![RawSinkEvent::WriteBlock(block_size as usize); 12]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    assert_eq!(
            raw.events, expected_events,
            "sidecar-filemark EW must not let a later sidecar-body EOM write the second sidecar filemark or final bootstrap"
        );
}

#[test]
fn sidecar_filemark_early_warning_does_not_mask_later_sidecar_filemark_eom() {
    #[derive(Debug)]
    struct SidecarFilemarkEwThenSidecarFilemarkEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        events: Vec<RawSinkEvent>,
        ew_filemarks_seen: Vec<usize>,
        eom_filemarks_seen: Vec<usize>,
    }

    impl RawTapeSink for SidecarFilemarkEwThenSidecarFilemarkEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            let early_warning = self.filemark_count == 2;
            if early_warning {
                self.ew_filemarks_seen.push(self.filemark_count);
            }
            let end_of_medium = self.filemark_count == 4;
            if end_of_medium {
                self.eom_filemarks_seen.push(self.filemark_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning,
                end_of_medium,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = SidecarFilemarkEwThenSidecarFilemarkEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        events: Vec::new(),
        ew_filemarks_seen: Vec::new(),
        eom_filemarks_seen: Vec::new(),
    };
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("first object data writes before sidecar EW");
            assert!(!outcome.early_warning, "first object block {seed}");
            assert!(!outcome.end_of_medium, "first object block {seed}");
        }

        let first = sink
            .finish_object()
            .expect("sidecar filemark EW alone must still commit the first sidecar");
        assert_eq!(first.sidecars_emitted.len(), 1);
        let first_sidecar = &first.sidecars_emitted[0];
        assert!(first_sidecar.filemark_outcome.early_warning);
        assert!(!first_sidecar.filemark_outcome.end_of_medium);
        assert_eq!(first.highest_protected_ordinal, 12);
        first_sidecar_block_count = first_sidecar.block_count;

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                12, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar EW");
        assert_eq!(second_tape_file, 2);
        for seed in 13..=24 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("second object data writes before sidecar-filemark EOM");
            assert!(!outcome.early_warning, "second object block {seed}");
            assert!(!outcome.end_of_medium, "second object block {seed}");
        }

        let err = sink
            .finish_object()
            .expect_err("later sidecar filemark EOM must hard-abort despite prior sidecar EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar-filemark EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("objects and first sidecar map remain valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
                TapeFileMapEntry::object(2, 12, 12),
            ],
            "the later sidecar-filemark EOM must not commit the second sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry the second sidecar filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    let second_sidecar_body_blocks = raw.block_count - 12 - first_sidecar_block_count as usize - 12;
    assert_eq!(
        second_sidecar_body_blocks, first_sidecar_block_count as usize,
        "the second sidecar body must be fully written before its EOM filemark aborts"
    );
    assert_eq!(raw.ew_filemarks_seen, vec![2]);
    assert_eq!(raw.eom_filemarks_seen, vec![4]);
    assert_eq!(raw.filemark_count, 4);

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![RawSinkEvent::WriteBlock(block_size as usize); 12]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    assert_eq!(
            raw.events, expected_events,
            "sidecar-filemark EW must not let a later sidecar-filemark EOM commit the sidecar or write final bootstrap"
        );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarBodyEwLaterEomTarget {
    ObjectData,
    ObjectFilemark,
    SidecarBody,
}

#[derive(Debug)]
struct SidecarBodyEwThenLaterEomRawTapeSink {
    target: SidecarBodyEwLaterEomTarget,
    sidecar_body_eom_after_blocks: usize,
    later_sidecar_body_blocks_seen: usize,
    cursor: u64,
    block_count: usize,
    filemark_count: usize,
    events: Vec<RawSinkEvent>,
    ew_blocks_seen: Vec<usize>,
    eom_blocks_seen: Vec<usize>,
    eom_filemarks_seen: Vec<usize>,
}

impl SidecarBodyEwThenLaterEomRawTapeSink {
    fn new(target: SidecarBodyEwLaterEomTarget) -> Self {
        Self {
            target,
            sidecar_body_eom_after_blocks: 1,
            later_sidecar_body_blocks_seen: 0,
            cursor: 0,
            block_count: 0,
            filemark_count: 0,
            events: Vec::new(),
            ew_blocks_seen: Vec::new(),
            eom_blocks_seen: Vec::new(),
            eom_filemarks_seen: Vec::new(),
        }
    }

    fn with_sidecar_body_eom_after_blocks(mut self, blocks: usize) -> Self {
        assert!(blocks > 0, "sidecar-body EOM target is 1-based");
        self.sidecar_body_eom_after_blocks = blocks;
        self
    }
}

impl RawTapeSink for SidecarBodyEwThenLaterEomRawTapeSink {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteBlock(buf.len()));
        self.block_count += 1;
        let early_warning = self.filemark_count == 1 && self.ew_blocks_seen.is_empty();
        if early_warning {
            self.ew_blocks_seen.push(self.block_count);
        }
        let end_of_medium = match self.target {
            SidecarBodyEwLaterEomTarget::ObjectData => {
                self.filemark_count >= 2 && self.eom_blocks_seen.is_empty()
            }
            SidecarBodyEwLaterEomTarget::SidecarBody => {
                if self.filemark_count >= 3 {
                    self.later_sidecar_body_blocks_seen += 1;
                    self.later_sidecar_body_blocks_seen == self.sidecar_body_eom_after_blocks
                        && self.eom_blocks_seen.is_empty()
                } else {
                    false
                }
            }
            SidecarBodyEwLaterEomTarget::ObjectFilemark => false,
        };
        if end_of_medium {
            self.eom_blocks_seen.push(self.block_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteBlock {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning,
            end_of_medium,
        })
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteFilemark);
        self.filemark_count += 1;
        let end_of_medium = matches!(self.target, SidecarBodyEwLaterEomTarget::ObjectFilemark)
            && self.filemark_count == 3;
        if end_of_medium {
            self.eom_filemarks_seen.push(self.filemark_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning: false,
            end_of_medium,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.events.push(RawSinkEvent::Position);
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

fn commit_first_full_object_with_sidecar_body_ew(
    sink: &mut ParitySink<'_>,
    block_size: u32,
) -> u64 {
    start_object(sink, 12, block_size);
    for seed in 1..=12 {
        let outcome = sink
            .write_block(&fixed_block(seed, block_size))
            .expect("first object data writes before sidecar-body EW");
        assert!(!outcome.early_warning, "first object block {seed}");
        assert!(!outcome.end_of_medium, "first object block {seed}");
    }

    let first = sink
        .finish_object()
        .expect("sidecar body EW alone must still commit the first sidecar");
    assert_eq!(first.sidecars_emitted.len(), 1);
    let first_sidecar = &first.sidecars_emitted[0];
    assert!(
        !first_sidecar.filemark_outcome.early_warning,
        "the EW is on the first sidecar body, not its filemark"
    );
    assert!(!first_sidecar.filemark_outcome.end_of_medium);
    assert_eq!(first.highest_protected_ordinal, 12);
    first_sidecar.block_count
}

#[test]
fn sidecar_body_early_warning_does_not_mask_later_object_data_eom() {
    let block_size: u32 = 512;
    let mut raw =
        SidecarBodyEwThenLaterEomRawTapeSink::new(SidecarBodyEwLaterEomTarget::ObjectData);
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        first_sidecar_block_count =
            commit_first_full_object_with_sidecar_body_ew(&mut sink, block_size);

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                1, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar-body EW");
        assert_eq!(second_tape_file, 2);

        let err = sink
            .write_block(&fixed_block(0xE5, block_size))
            .expect_err("later object data EOM must hard-abort despite prior sidecar-body EW");
        match err {
            TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                assert!(
                    message.contains("object data block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-data EOM invalid-input error, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("first object and sidecar map remains valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
            ],
            "the later object-data EOM must not commit a second object map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not write a second object filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.ew_blocks_seen,
        vec![13],
        "EW must land on the first body block of the first sidecar"
    );
    assert_eq!(
        raw.eom_blocks_seen,
        vec![12 + first_sidecar_block_count as usize + 1],
        "EOM must land on the first block of the next object"
    );
    assert!(raw.eom_filemarks_seen.is_empty());
    assert_eq!(raw.filemark_count, 2);

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    assert_eq!(
            raw.events, expected_events,
            "sidecar-body EW must not let a later object-data EOM write a filemark, sidecar, or bootstrap"
        );
}

#[test]
fn sidecar_body_early_warning_does_not_mask_later_object_filemark_eom() {
    let block_size: u32 = 512;
    let mut raw =
        SidecarBodyEwThenLaterEomRawTapeSink::new(SidecarBodyEwLaterEomTarget::ObjectFilemark);
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        first_sidecar_block_count =
            commit_first_full_object_with_sidecar_body_ew(&mut sink, block_size);

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                1, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar-body EW");
        assert_eq!(second_tape_file, 2);
        let outcome = sink
            .write_block(&fixed_block(0xE6, block_size))
            .expect("second object data writes before object-filemark EOM");
        assert!(!outcome.early_warning);
        assert!(!outcome.end_of_medium);

        let err = sink
            .finish_object()
            .expect_err("later object filemark EOM must hard-abort despite prior sidecar-body EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("object trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-filemark EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("first object and sidecar map remains valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
            ],
            "the later object-filemark EOM must not commit a second object map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry the second object filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.ew_blocks_seen,
        vec![13],
        "EW must land on the first body block of the first sidecar"
    );
    assert_eq!(raw.eom_filemarks_seen, vec![3]);
    assert!(raw.eom_blocks_seen.is_empty());
    assert_eq!(
        raw.block_count,
        12 + first_sidecar_block_count as usize + 1,
        "second object data block must be the only post-sidecar block before object-filemark EOM"
    );

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    expected_events.push(RawSinkEvent::WriteFilemark);
    assert_eq!(
            raw.events, expected_events,
            "sidecar-body EW must not let a later object-filemark EOM commit the object, write sidecars, or write bootstrap"
        );
}

#[test]
fn sidecar_body_early_warning_does_not_mask_later_sidecar_body_eom() {
    let block_size: u32 = 512;
    let mut raw =
        SidecarBodyEwThenLaterEomRawTapeSink::new(SidecarBodyEwLaterEomTarget::SidecarBody);
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        first_sidecar_block_count =
            commit_first_full_object_with_sidecar_body_ew(&mut sink, block_size);

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                12, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar-body EW");
        assert_eq!(second_tape_file, 2);
        for seed in 13..=24 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("second object data writes before sidecar-body EOM");
            assert!(!outcome.early_warning, "second object block {seed}");
            assert!(!outcome.end_of_medium, "second object block {seed}");
        }

        let err = sink
            .finish_object()
            .expect_err("later sidecar body EOM must hard-abort despite prior sidecar-body EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar-body EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("objects and first sidecar map remain valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
                TapeFileMapEntry::object(2, 12, 12),
            ],
            "the later sidecar-body EOM must not commit the second sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry the second sidecar");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.ew_blocks_seen,
        vec![13],
        "EW must land on the first body block of the first sidecar"
    );
    assert_eq!(raw.filemark_count, 3);
    assert_eq!(
        raw.eom_blocks_seen,
        vec![12 + first_sidecar_block_count as usize + 12 + 1],
        "EOM must land on the first second-sidecar body block after the second object filemark"
    );
    assert!(raw.eom_filemarks_seen.is_empty());

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![RawSinkEvent::WriteBlock(block_size as usize); 12]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    assert_eq!(
            raw.events, expected_events,
            "sidecar-body EW must not let a later sidecar-body EOM write the second sidecar filemark or final bootstrap"
        );
}

#[test]
fn sidecar_body_early_warning_does_not_mask_later_sidecar_second_body_block_eom() {
    let block_size: u32 = 512;
    let mut raw =
        SidecarBodyEwThenLaterEomRawTapeSink::new(SidecarBodyEwLaterEomTarget::SidecarBody)
            .with_sidecar_body_eom_after_blocks(2);
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        first_sidecar_block_count =
            commit_first_full_object_with_sidecar_body_ew(&mut sink, block_size);
        assert!(
            first_sidecar_block_count >= 2,
            "fixture needs a multi-block later sidecar body"
        );

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                12, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar-body EW");
        assert_eq!(second_tape_file, 2);
        for seed in 13..=24 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("second object data writes before later sidecar-body EOM");
            assert!(!outcome.early_warning, "second object block {seed}");
            assert!(!outcome.end_of_medium, "second object block {seed}");
        }

        let err = sink.finish_object().expect_err(
            "later sidecar second body-block EOM must hard-abort despite prior sidecar-body EW",
        );
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar-body EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("objects and first sidecar map remain valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
                TapeFileMapEntry::object(2, 12, 12),
            ],
            "later non-first sidecar-body EOM must not commit the second sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry the second sidecar");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.ew_blocks_seen,
        vec![13],
        "EW must land on the first body block of the first sidecar"
    );
    assert_eq!(raw.filemark_count, 3);
    assert_eq!(raw.later_sidecar_body_blocks_seen, 2);
    assert_eq!(
        raw.eom_blocks_seen,
        vec![12 + first_sidecar_block_count as usize + 12 + 2],
        "EOM must land on the second second-sidecar body block"
    );
    assert!(raw.eom_filemarks_seen.is_empty());

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![RawSinkEvent::WriteBlock(block_size as usize); 12]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![RawSinkEvent::WriteBlock(block_size as usize); 2]);
    assert_eq!(
            raw.events, expected_events,
            "sidecar-body EW must not let later non-first sidecar-body EOM write the second sidecar filemark or final bootstrap"
        );
}

#[test]
fn sidecar_body_early_warning_does_not_mask_later_sidecar_filemark_eom() {
    #[derive(Debug)]
    struct SidecarBodyEwThenSidecarFilemarkEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        events: Vec<RawSinkEvent>,
        ew_blocks_seen: Vec<usize>,
        eom_filemarks_seen: Vec<usize>,
    }

    impl RawTapeSink for SidecarBodyEwThenSidecarFilemarkEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            let early_warning = self.filemark_count == 1 && self.ew_blocks_seen.is_empty();
            if early_warning {
                self.ew_blocks_seen.push(self.block_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            let end_of_medium = self.filemark_count == 4;
            if end_of_medium {
                self.eom_filemarks_seen.push(self.filemark_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = SidecarBodyEwThenSidecarFilemarkEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        events: Vec::new(),
        ew_blocks_seen: Vec::new(),
        eom_filemarks_seen: Vec::new(),
    };
    let first_sidecar_block_count;
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("first object data writes before sidecar-body EW");
            assert!(!outcome.early_warning, "first object block {seed}");
            assert!(!outcome.end_of_medium, "first object block {seed}");
        }

        let first = sink
            .finish_object()
            .expect("sidecar body EW alone must still commit the first sidecar");
        assert_eq!(first.sidecars_emitted.len(), 1);
        let first_sidecar = &first.sidecars_emitted[0];
        assert!(
            !first_sidecar.filemark_outcome.early_warning,
            "the EW is on the first sidecar body, not its filemark"
        );
        assert!(!first_sidecar.filemark_outcome.end_of_medium);
        assert_eq!(first.highest_protected_ordinal, 12);
        first_sidecar_block_count = first_sidecar.block_count;

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                12, 10_000, block_size,
            ))
            .expect("second object reserve fits after sidecar-body EW");
        assert_eq!(second_tape_file, 2);
        for seed in 13..=24 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("second object data writes before sidecar-filemark EOM");
            assert!(!outcome.early_warning, "second object block {seed}");
            assert!(!outcome.end_of_medium, "second object block {seed}");
        }

        let err = sink
            .finish_object()
            .expect_err("later sidecar filemark EOM must hard-abort despite prior sidecar-body EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar-filemark EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("objects and first sidecar map remain valid");
        assert_eq!(
            map.entries(),
            &[
                TapeFileMapEntry::object(0, 12, 0),
                TapeFileMapEntry::parity_sidecar(1, first_sidecar_block_count, 0, 0, 12),
                TapeFileMapEntry::object(2, 12, 12),
            ],
            "the later sidecar-filemark EOM must not commit the second sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry the second sidecar filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after later sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.ew_blocks_seen,
        vec![13],
        "EW must land on the first body block of the first sidecar"
    );
    let second_sidecar_body_blocks = raw.block_count - 12 - first_sidecar_block_count as usize - 12;
    assert_eq!(
        second_sidecar_body_blocks, first_sidecar_block_count as usize,
        "the second sidecar body must be fully written before its EOM filemark aborts"
    );
    assert_eq!(raw.eom_filemarks_seen, vec![4]);
    assert_eq!(raw.filemark_count, 4);

    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![RawSinkEvent::WriteBlock(block_size as usize); 12]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        first_sidecar_block_count as usize
    ]);
    expected_events.push(RawSinkEvent::WriteFilemark);
    assert_eq!(
            raw.events, expected_events,
            "sidecar-body EW must not let a later sidecar-filemark EOM commit the sidecar or write final bootstrap"
        );
}

#[test]
fn sidecar_only_object_data_and_filemark_early_warning_still_commits() {
    let block_size: u32 = 512;
    let mut raw = EarlyWarningRawTapeSink::new(vec![1, 5, 10], vec![1]);
    let sidecar_block_count;
    let _final_geometry = {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);

        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("EW-only object data blocks remain committed");
            assert_eq!(
                outcome.early_warning,
                matches!(seed, 1 | 5 | 10),
                "block {seed}"
            );
            assert!(!outcome.end_of_medium, "block {seed}");
            assert_eq!(sink.active_object_blocks_written(), Some(u64::from(seed)));
        }

        let object = sink
            .finish_object()
            .expect("object closes despite EW on data blocks and object filemark");
        assert_eq!(object.data_block_count, 12);
        assert!(object.filemark_outcome.early_warning);
        assert!(!object.filemark_outcome.end_of_medium);
        assert_eq!(object.sidecars_emitted.len(), 1);
        let sidecar = &object.sidecars_emitted[0];
        assert!(!sidecar.filemark_outcome.early_warning);
        assert!(!sidecar.filemark_outcome.end_of_medium);
        assert_eq!(object.highest_protected_ordinal, 12);
        sidecar_block_count = sidecar.block_count;

        sink.finish()
            .expect("final bootstrap still writes after EW-only object close")
    };

    assert_eq!(raw.ew_blocks_seen, vec![1, 5, 10]);
    assert_eq!(raw.ew_filemarks_seen, vec![1]);
    assert_eq!(raw.filemark_count, 3);
    assert_eq!(
        raw.block_count as u64,
        12 + sidecar_block_count + 1,
        "object, sidecar, and final bootstrap blocks must all be written"
    );

    let final_bootstrap = crate::bootstrap::parse_bootstrap_block(
        raw.blocks
            .last()
            .expect("final bootstrap block was written"),
    )
    .expect("final bootstrap parses after EW-only object close");
    let digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(digest.is_final_map);
    assert_eq!(digest.tape_file_count, 3);
    assert_eq!(digest.map_total_data_ordinals, 12);
}

#[test]
fn object_data_early_warning_uses_single_runtime_reserve_predicate() {
    let block_size: u32 = 512;
    let mut raw = EarlyWarningRawTapeSink::new(vec![1], vec![]);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 2, block_size);
        sink.early_warning_reserve
            .as_mut()
            .expect("begin_object installs an EW reserve guard")
            .input
            .remaining_tape_blocks = 0;

        let err = sink
            .write_block(&fixed_block(1, block_size))
            .expect_err("EW with an exhausted runtime reserve must abort");
        match err {
            TapeIoError::OperationFailed(message) => {
                assert!(message.contains("required reserve"), "{message}");
                assert!(message.contains("TapeCapacity"), "{message}");
            }
            other => panic!("expected reserve failure through the body sink, got {other:?}"),
        }

        let err = sink
            .finish_object()
            .expect_err("reserve failure poisons the write session");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant after reserve failure, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, vec![1]);
    assert_eq!(
        raw.filemark_count, 0,
        "EW reserve failure must not write the object filemark or commit a map entry"
    );
}

#[test]
fn object_data_eom_bypasses_runtime_reserve_predicate_when_ew_cofires() {
    let block_size: u32 = 512;
    let mut raw = EwEomTripwireRawTapeSink::on_block(1);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 2, block_size);
        exhaust_runtime_tape_reserve(&mut sink);

        let err = sink
            .write_block(&fixed_block(0xC1, block_size))
            .expect_err("object data EW+EOM must hard-abort before reserve failure");
        match err {
            TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                assert!(
                    message.contains("object data block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-data EOM invalid-input error, got {other:?}"),
        }

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not write an object filemark after EW+EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant after object-data EOM, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_eom_blocks_seen, vec![1]);
    assert_eq!(raw.filemark_count, 0);
    assert_eq!(
        raw.events,
        vec![RawSinkEvent::WriteBlock(block_size as usize)],
        "co-fired object-data EW+EOM must not write a filemark, sidecar, or bootstrap"
    );
}

#[test]
fn object_filemark_eom_bypasses_runtime_reserve_predicate_when_ew_cofires() {
    let block_size: u32 = 512;
    let mut raw = EwEomTripwireRawTapeSink::on_filemark(1);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 1, block_size);
        sink.write_block(&fixed_block(0xC2, block_size))
            .expect("object data writes before object-filemark EW+EOM");
        exhaust_runtime_tape_reserve(&mut sink);

        let err = sink
            .finish_object()
            .expect_err("object filemark EW+EOM must hard-abort before reserve failure");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("object trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after filemark EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => {
                panic!("expected poisoned invariant after object-filemark EOM, got {other:?}")
            }
        }
    }

    assert_eq!(raw.ew_eom_filemarks_seen, vec![1]);
    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "co-fired object-filemark EW+EOM must not commit the object or emit sidecars"
    );
}

#[test]
fn sidecar_filemark_eom_bypasses_runtime_reserve_predicate_when_ew_cofires() {
    let block_size: u32 = 512;
    let mut raw = EwEomTripwireRawTapeSink::on_filemark(2);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("object data writes before sidecar-filemark EW+EOM");
        }
        exhaust_runtime_tape_reserve(&mut sink);

        let err = sink
            .finish_object()
            .expect_err("sidecar filemark EW+EOM must hard-abort before reserve failure");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar-filemark EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("object-only map remains valid after sidecar-filemark EOM");
        assert_eq!(
            map.entries(),
            &[TapeFileMapEntry::object(0, 12, 0)],
            "co-fired sidecar-filemark EW+EOM must not commit the failed sidecar"
        );

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => {
                panic!("expected poisoned invariant after sidecar-filemark EOM, got {other:?}")
            }
        }
    }

    assert_eq!(raw.ew_eom_filemarks_seen, vec![2]);
    assert_eq!(raw.filemark_count, 2);
    assert_eq!(
        raw.events.last(),
        Some(&RawSinkEvent::WriteFilemark),
        "co-fired sidecar-filemark EW+EOM must stop before final bootstrap"
    );
    assert!(
        raw.events
            .iter()
            .filter(|event| matches!(event, RawSinkEvent::WriteBlock(_)))
            .count()
            > 12,
        "the test must reach sidecar body writes before the EW+EOM filemark"
    );
}

#[test]
fn bootstrap_block_eom_bypasses_runtime_reserve_predicate_when_ew_cofires() {
    let block_size: u32 = 512;
    let mut raw = EwEomTripwireRawTapeSink::on_block(2);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 1, block_size);
        sink.write_block(&fixed_block(0xC3, block_size))
            .expect("object data writes before bootstrap EW+EOM");
        sink.finish_object()
            .expect("object closes before bootstrap");
        exhaust_runtime_tape_reserve(&mut sink);

        let err = sink
            .write_bootstrap()
            .expect_err("bootstrap block EW+EOM must hard-abort before reserve failure");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap-block EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after bootstrap EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => {
                panic!("expected poisoned invariant after bootstrap-block EOM, got {other:?}")
            }
        }
    }

    assert_eq!(raw.ew_eom_blocks_seen, vec![2]);
    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
            RawSinkEvent::WriteBlock(block_size as usize),
        ],
        "co-fired bootstrap-block EW+EOM must stop before the bootstrap filemark"
    );
}

#[test]
fn bootstrap_filemark_eom_bypasses_runtime_reserve_predicate_when_ew_cofires() {
    let block_size: u32 = 512;
    let mut raw = EwEomTripwireRawTapeSink::on_filemark(2);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 1, block_size);
        sink.write_block(&fixed_block(0xC4, block_size))
            .expect("object data writes before bootstrap-filemark EW+EOM");
        sink.finish_object()
            .expect("object closes before bootstrap");
        exhaust_runtime_tape_reserve(&mut sink);

        let err = sink
            .write_bootstrap()
            .expect_err("bootstrap filemark EW+EOM must hard-abort before reserve failure");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap-filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after bootstrap EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => {
                panic!("expected poisoned invariant after bootstrap-filemark EOM, got {other:?}")
            }
        }
    }

    assert_eq!(raw.ew_eom_filemarks_seen, vec![2]);
    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "co-fired bootstrap-filemark EW+EOM must not be followed by final bootstrap"
    );
}

#[test]
fn sidecar_body_early_warning_detects_natural_runtime_reserve_exhaustion() {
    let block_size: u32 = 256;
    let scheme = small_scheme();
    let object_blocks: Vec<_> = (1..=12).map(|seed| fixed_block(seed, block_size)).collect();
    let parity_shards = expected_epoch_parity(&scheme, &object_blocks, block_size);
    let data_crcs = object_blocks
        .iter()
        .map(|block| data_shard_crc64(block))
        .collect();
    let descriptor = SidecarDescriptor {
        tape_uuid: sample_uuid(),
        epoch_id: 0,
        k: scheme.data_blocks_per_stripe,
        m: scheme.parity_blocks_per_stripe,
        stripes_per_epoch: scheme.stripes_per_neighborhood,
        block_size,
        protected_ordinal_start: 0,
        protected_ordinal_end_exclusive: object_blocks.len() as u64,
    };
    let encoded = encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
        .expect("test sidecar encodes");
    assert!(
        encoded.header.shard_index_block_count > 1,
        "this fixture needs a multi-block sidecar index to under-model body reserve only"
    );
    let sidecar_block_count = encoded.blocks.len();
    let ew_on_last_sidecar_body_block = object_blocks.len() + sidecar_block_count;

    let mut reserve_input =
        capacity_input_with_block_size(object_blocks.len() as u64, 10_000, block_size);
    reserve_input.sidecar_index_block_count = 0;
    reserve_input.remaining_bootstrap_count = 0;
    reserve_input.safety_margin_blocks = 0;
    reserve_input.remaining_tape_blocks = reserve_input
        .evaluate()
        .expect("under-modeled reserve still admits the object at start")
        .required_tape_blocks;

    let mut raw = EarlyWarningRawTapeSink::new(vec![ew_on_last_sidecar_body_block], vec![]);
    {
        let mut sink = ParitySink::new_sidecar_only(&mut raw, scheme, sample_uuid(), block_size)
            .expect("sidecar-only raw sink constructs");
        sink.begin_object_with_capacity_reserve(reserve_input)
            .expect("start reserve admits the object");
        for block in &object_blocks {
            sink.write_block(block).expect("object data writes");
        }

        let err = sink
            .finish_object()
            .expect_err("late sidecar EW must detect consumed reserve shortfall");
        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::TapeCapacity);
                assert_eq!(projected_object_blocks, object_blocks.len() as u64);
                assert_eq!(remaining_blocks, Some(0));
                assert_eq!(reserve_blocks, Some(0));
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected runtime tape-capacity reserve failure, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("object map remains valid after sidecar reserve abort");
        assert_eq!(
            map.entries(),
            &[TapeFileMapEntry::object(0, object_blocks.len() as u64, 0)],
            "sidecar reserve exhaustion must not commit the failed sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry after reserve exhaustion");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant after reserve failure, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, vec![ew_on_last_sidecar_body_block]);
    assert_eq!(raw.filemark_count, 1, "only the object filemark committed");
    assert_eq!(
        raw.block_count, ew_on_last_sidecar_body_block,
        "sidecar emission must stop at the EW reserve failure"
    );
}

#[test]
fn final_bootstrap_early_warning_detects_under_modeled_bootstrap_reserve() {
    let block_size: u32 = 256;
    let scheme = small_scheme();
    let object_blocks: Vec<_> = (1..=2).map(|seed| fixed_block(seed, block_size)).collect();
    let parity_shards = expected_epoch_parity(&scheme, &object_blocks, block_size);
    let data_crcs = object_blocks
        .iter()
        .map(|block| data_shard_crc64(block))
        .collect();
    let descriptor = SidecarDescriptor {
        tape_uuid: sample_uuid(),
        epoch_id: 0,
        k: scheme.data_blocks_per_stripe,
        m: scheme.parity_blocks_per_stripe,
        stripes_per_epoch: scheme.stripes_per_neighborhood,
        block_size,
        protected_ordinal_start: 0,
        protected_ordinal_end_exclusive: object_blocks.len() as u64,
    };
    let encoded = encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
        .expect("test final partial sidecar encodes");
    let final_bootstrap_block = object_blocks.len() + encoded.blocks.len() + 1;

    let mut reserve_input =
        capacity_input_with_block_size(object_blocks.len() as u64, 10_000, block_size);
    reserve_input.sidecar_index_block_count = u64::from(encoded.header.shard_index_block_count);
    reserve_input.parity_shards_per_epoch = u64::from(encoded.header.parity_block_count);
    reserve_input.remaining_bootstrap_count = 0;
    reserve_input.safety_margin_blocks = 0;
    reserve_input.remaining_tape_blocks = reserve_input
        .evaluate()
        .expect("under-modeled bootstrap reserve still admits the object")
        .required_tape_blocks;

    let mut raw = EarlyWarningRawTapeSink::new(vec![final_bootstrap_block], vec![]);
    {
        let mut sink = ParitySink::new_sidecar_only(&mut raw, scheme, sample_uuid(), block_size)
            .expect("sidecar-only raw sink constructs");
        sink.begin_object_with_capacity_reserve(reserve_input)
            .expect("start reserve admits the object");
        for block in &object_blocks {
            sink.write_block(block).expect("object data writes");
        }
        sink.finish_object()
            .expect("partial object closes before final sidecar/bootstrap flush");

        let err = sink
            .finish()
            .expect_err("final bootstrap EW must detect the missing bootstrap reserve");
        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::TapeCapacity);
                assert_eq!(projected_object_blocks, object_blocks.len() as u64);
                assert_eq!(remaining_blocks, Some(0));
                assert_eq!(reserve_blocks, Some(0));
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected final-bootstrap tape reserve failure, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, vec![final_bootstrap_block]);
    assert_eq!(
        raw.filemark_count, 2,
        "object and final sidecar filemarks commit, but bootstrap filemark must not"
    );
    assert_eq!(
        raw.block_count, final_bootstrap_block,
        "the final bootstrap block is the first write beyond the under-modeled reserve"
    );
}

#[test]
fn sidecar_filemark_early_warning_detects_under_modeled_filemark_reserve() {
    let block_size: u32 = 256;
    let scheme = small_scheme();
    let object_blocks: Vec<_> = (1..=12).map(|seed| fixed_block(seed, block_size)).collect();
    let parity_shards = expected_epoch_parity(&scheme, &object_blocks, block_size);
    let data_crcs = object_blocks
        .iter()
        .map(|block| data_shard_crc64(block))
        .collect();
    let descriptor = SidecarDescriptor {
        tape_uuid: sample_uuid(),
        epoch_id: 0,
        k: scheme.data_blocks_per_stripe,
        m: scheme.parity_blocks_per_stripe,
        stripes_per_epoch: scheme.stripes_per_neighborhood,
        block_size,
        protected_ordinal_start: 0,
        protected_ordinal_end_exclusive: object_blocks.len() as u64,
    };
    let encoded = encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
        .expect("test sidecar encodes");
    let sidecar_block_count = encoded.blocks.len();

    let mut reserve_input =
        capacity_input_with_block_size(object_blocks.len() as u64, 10_000, block_size);
    reserve_input.sidecar_index_block_count = u64::from(encoded.header.shard_index_block_count);
    reserve_input.parity_shards_per_epoch = u64::from(encoded.header.parity_block_count);
    reserve_input.sidecar_filemark_blocks = 0;
    reserve_input.remaining_bootstrap_count = 0;
    reserve_input.safety_margin_blocks = 0;
    reserve_input.remaining_tape_blocks = reserve_input
        .evaluate()
        .expect("under-modeled sidecar-filemark reserve still admits the object")
        .required_tape_blocks;

    let mut raw = EarlyWarningRawTapeSink::new(vec![], vec![2]);
    {
        let mut sink = ParitySink::new_sidecar_only(&mut raw, scheme, sample_uuid(), block_size)
            .expect("sidecar-only raw sink constructs");
        sink.begin_object_with_capacity_reserve(reserve_input)
            .expect("start reserve admits the object");
        for block in &object_blocks {
            sink.write_block(block).expect("object data writes");
        }

        let err = sink
            .finish_object()
            .expect_err("sidecar filemark EW must detect the missing filemark reserve");
        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::TapeCapacity);
                assert_eq!(projected_object_blocks, object_blocks.len() as u64);
                assert_eq!(remaining_blocks, Some(0));
                assert_eq!(reserve_blocks, Some(0));
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => {
                panic!("expected sidecar-filemark tape reserve failure, got {other:?}")
            }
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("object map remains valid after sidecar filemark reserve abort");
        assert_eq!(
            map.entries(),
            &[TapeFileMapEntry::object(0, object_blocks.len() as u64, 0)],
            "sidecar filemark reserve exhaustion must not commit the sidecar map entry"
        );

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not retry after sidecar filemark reserve failure");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant after reserve failure, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_filemarks_seen, vec![2]);
    assert_eq!(
        raw.filemark_count, 2,
        "object and sidecar filemarks were written, but only the object may be cataloged"
    );
    assert_eq!(
        raw.block_count,
        object_blocks.len() + sidecar_block_count,
        "sidecar emission must stop immediately after the EW-bearing filemark"
    );
}

#[test]
fn sidecar_body_eom_bypasses_runtime_reserve_predicate_when_ew_cofires() {
    #[derive(Debug)]
    struct EarlyWarningEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        ew_eom_on_block: usize,
        ew_eom_blocks_seen: Vec<usize>,
        events: Vec<RawSinkEvent>,
    }

    impl RawTapeSink for EarlyWarningEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            let ew_eom = self.block_count == self.ew_eom_on_block;
            if ew_eom {
                self.ew_eom_blocks_seen.push(self.block_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: ew_eom,
                end_of_medium: ew_eom,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 256;
    let scheme = small_scheme();
    let object_blocks: Vec<_> = (1..=12).map(|seed| fixed_block(seed, block_size)).collect();
    let parity_shards = expected_epoch_parity(&scheme, &object_blocks, block_size);
    let data_crcs = object_blocks
        .iter()
        .map(|block| data_shard_crc64(block))
        .collect();
    let descriptor = SidecarDescriptor {
        tape_uuid: sample_uuid(),
        epoch_id: 0,
        k: scheme.data_blocks_per_stripe,
        m: scheme.parity_blocks_per_stripe,
        stripes_per_epoch: scheme.stripes_per_neighborhood,
        block_size,
        protected_ordinal_start: 0,
        protected_ordinal_end_exclusive: object_blocks.len() as u64,
    };
    let encoded = encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
        .expect("test sidecar encodes");
    assert!(
        encoded.header.shard_index_block_count > 1,
        "this fixture needs a multi-block sidecar index to under-model body reserve only"
    );
    let sidecar_block_count = encoded.blocks.len();
    let ew_eom_on_last_sidecar_body_block = object_blocks.len() + sidecar_block_count;

    let mut reserve_input =
        capacity_input_with_block_size(object_blocks.len() as u64, 10_000, block_size);
    reserve_input.sidecar_index_block_count = 0;
    reserve_input.remaining_bootstrap_count = 0;
    reserve_input.safety_margin_blocks = 0;
    reserve_input.remaining_tape_blocks = reserve_input
        .evaluate()
        .expect("under-modeled reserve still admits the object at start")
        .required_tape_blocks;

    let mut raw = EarlyWarningEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        ew_eom_on_block: ew_eom_on_last_sidecar_body_block,
        ew_eom_blocks_seen: Vec::new(),
        events: Vec::new(),
    };
    {
        let mut sink = ParitySink::new_sidecar_only(&mut raw, scheme, sample_uuid(), block_size)
            .expect("sidecar-only raw sink constructs");
        sink.begin_object_with_capacity_reserve(reserve_input)
            .expect("start reserve admits the object");
        for block in &object_blocks {
            sink.write_block(block).expect("object data writes");
        }

        let err = sink
            .finish_object()
            .expect_err("sidecar EOM must win over co-fired EW reserve shortfall");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("object map remains valid after sidecar EOM");
        assert_eq!(
            map.entries(),
            &[TapeFileMapEntry::object(0, object_blocks.len() as u64, 0)],
            "co-fired EW+EOM must not commit the failed sidecar map entry"
        );

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant after sidecar EOM, got {other:?}"),
        }
    }

    assert_eq!(
        raw.ew_eom_blocks_seen,
        vec![ew_eom_on_last_sidecar_body_block]
    );
    assert_eq!(raw.filemark_count, 1, "only the object filemark committed");
    assert_eq!(
        raw.block_count, ew_eom_on_last_sidecar_body_block,
        "sidecar emission must stop at the EOM block"
    );
    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.extend(vec![
        RawSinkEvent::WriteBlock(block_size as usize);
        sidecar_block_count
    ]);
    assert_eq!(
            raw.events, expected_events,
            "EOM must bypass the EW reserve predicate and stop before sidecar filemark or final bootstrap"
        );
}

#[test]
fn sidecar_only_multi_object_early_warning_interleaves_without_state_leakage() {
    let block_size: u32 = 512;
    // Blocks 1..5 are object 1 data; blocks 6..12 are object 2 data.
    // Filemark 1 closes object 1. The completed sidecar is emitted only
    // after object 2's clean trailing filemark.
    let mut raw = EarlyWarningRawTapeSink::new(vec![6, 12], vec![1]);
    let sidecar_block_count;
    let _final_geometry = {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        start_object(&mut sink, 5, block_size);
        for seed in 1u8..=5 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("first object data writes");
            assert!(!outcome.early_warning, "first object block {seed}");
            assert!(!outcome.end_of_medium, "first object block {seed}");
            assert_eq!(sink.active_object_blocks_written(), Some(u64::from(seed)));
        }
        let first = sink
            .finish_object()
            .expect("first object closes despite EW on its filemark");
        assert_eq!(first.tape_file_number, 0);
        assert_eq!(first.first_parity_data_ordinal, 0);
        assert_eq!(first.data_block_count, 5);
        assert!(first.filemark_outcome.early_warning);
        assert!(!first.filemark_outcome.end_of_medium);
        assert!(first.sidecars_emitted.is_empty());
        assert_eq!(first.highest_protected_ordinal, 0);

        let (second_tape_file, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_current_fill(
                7, 10_000, block_size, 5,
            ))
            .expect("second object reserve fits across a partial epoch");
        assert_eq!(second_tape_file, 1);
        for local in 1u8..=7 {
            let seed = 5 + local;
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("second object data EW does not abort");
            assert_eq!(
                outcome.early_warning,
                matches!(local, 1 | 7),
                "second object block {local}"
            );
            assert!(!outcome.end_of_medium, "second object block {local}");
            assert_eq!(sink.active_object_blocks_written(), Some(u64::from(local)));
        }

        let second = sink
            .finish_object()
            .expect("second object closes and emits completed sidecar");
        assert_eq!(second.tape_file_number, 1);
        assert_eq!(second.first_parity_data_ordinal, 5);
        assert_eq!(second.data_block_count, 7);
        assert!(!second.filemark_outcome.early_warning);
        assert!(!second.filemark_outcome.end_of_medium);
        assert_eq!(second.sidecars_emitted.len(), 1);
        let sidecar = &second.sidecars_emitted[0];
        assert_eq!(sidecar.tape_file_number, 2);
        assert_eq!(sidecar.protected_ordinal_start, 0);
        assert_eq!(sidecar.protected_ordinal_end_exclusive, 12);
        assert!(!sidecar.filemark_outcome.early_warning);
        assert!(!sidecar.filemark_outcome.end_of_medium);
        assert_eq!(second.highest_protected_ordinal, 12);
        sidecar_block_count = sidecar.block_count;

        sink.finish()
            .expect("final bootstrap still writes after interleaved EW")
    };

    assert_eq!(raw.ew_blocks_seen, vec![6, 12]);
    assert_eq!(raw.ew_filemarks_seen, vec![1]);
    assert_eq!(raw.filemark_count, 4);
    assert_eq!(
        raw.block_count as u64,
        12 + sidecar_block_count + 1,
        "both objects, the completed sidecar, and final bootstrap must be written"
    );

    let final_bootstrap = crate::bootstrap::parse_bootstrap_block(
        raw.blocks
            .last()
            .expect("final bootstrap block was written"),
    )
    .expect("final bootstrap parses after interleaved EW");
    let digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(digest.is_final_map);
    assert_eq!(digest.tape_file_count, 4);
    assert_eq!(digest.map_total_data_ordinals, 12);
    assert_eq!(digest.highest_protected_ordinal, 12);
}

#[test]
fn bootstrap_early_warning_finishes_bootstrap_tape_files() {
    let block_size: u32 = 512;
    let mut raw = EarlyWarningRawTapeSink::new(vec![1, 2], vec![1, 2]);
    let _final_geometry = {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        assert_eq!(
            sink.write_bootstrap()
                .expect("non-final bootstrap EW does not abort"),
            0
        );
        sink.finish()
            .expect("final bootstrap EW does not abort the clean session")
    };

    assert_eq!(raw.ew_blocks_seen, vec![1, 2]);
    assert_eq!(raw.ew_filemarks_seen, vec![1, 2]);
    assert_eq!(raw.block_count, 2);
    assert_eq!(raw.filemark_count, 2);

    let final_bootstrap = crate::bootstrap::parse_bootstrap_block(
        raw.blocks
            .last()
            .expect("final bootstrap block was written"),
    )
    .expect("final bootstrap parses after EW-only writes");
    let digest = final_bootstrap
        .filemark_map_digest
        .expect("final bootstrap carries map digest");
    assert!(digest.is_final_map);
    assert_eq!(digest.tape_file_count, 2);
    assert_eq!(digest.map_total_data_ordinals, 0);
}

#[test]
fn sidecar_only_rejects_eom_on_object_data_before_filemark_or_map_commit() {
    #[derive(Debug, Default)]
    struct EomObjectDataRawTapeSink {
        cursor: u64,
        events: Vec<RawSinkEvent>,
    }

    impl RawTapeSink for EomObjectDataRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: true,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = EomObjectDataRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 1, block_size);

        let err = sink
            .write_block(&fixed_block(0xD0, block_size))
            .expect_err("object data EOM must hard-abort the write session");
        match err {
            TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                assert!(
                    message.contains("object data block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-data EOM invalid-input error, got {other:?}"),
        }

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not write an object filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![RawSinkEvent::WriteBlock(block_size as usize)],
        "object-data EOM must not be followed by an object filemark, sidecar, or bootstrap"
    );
}

#[test]
fn sidecar_only_prior_early_warning_does_not_mask_later_object_data_eom_abort() {
    #[derive(Debug, Default)]
    struct EwThenEomObjectDataRawTapeSink {
        cursor: u64,
        block_count: usize,
        events: Vec<RawSinkEvent>,
    }

    impl RawTapeSink for EwThenEomObjectDataRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: self.block_count == 1,
                end_of_medium: self.block_count == 2,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = EwThenEomObjectDataRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 2, block_size);

        let ew_outcome = sink
            .write_block(&fixed_block(0xE1, block_size))
            .expect("EW-only object data block remains committed");
        assert!(ew_outcome.early_warning);
        assert!(!ew_outcome.end_of_medium);
        assert_eq!(sink.active_object_blocks_written(), Some(1));

        let err = sink
            .write_block(&fixed_block(0xE2, block_size))
            .expect_err("later object data EOM must still hard-abort");
        match err {
            TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                assert!(
                    message.contains("object data block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object-data EOM invalid-input error, got {other:?}"),
        }

        let err = sink
            .finish_object()
            .expect_err("poisoned sink must not write an object filemark after EW+EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after EW+EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteBlock(block_size as usize),
        ],
        "EW before EOM must not allow an object filemark, sidecar, or bootstrap after EOM"
    );
}

#[test]
fn sidecar_only_prior_early_warning_does_not_mask_object_filemark_eom_abort() {
    #[derive(Debug)]
    struct EwTailThenFilemarkEomRawTapeSink {
        cursor: u64,
        block_count: usize,
        filemark_count: usize,
        ew_through_block: usize,
        eom_on_filemark: usize,
        ew_blocks_seen: Vec<usize>,
        events: Vec<RawSinkEvent>,
    }

    impl RawTapeSink for EwTailThenFilemarkEomRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            let early_warning = self.block_count <= self.ew_through_block;
            if early_warning {
                self.ew_blocks_seen.push(self.block_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: self.filemark_count == self.eom_on_filemark,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = EwTailThenFilemarkEomRawTapeSink {
        cursor: 0,
        block_count: 0,
        filemark_count: 0,
        ew_through_block: 10,
        eom_on_filemark: 1,
        ew_blocks_seen: Vec::new(),
        events: Vec::new(),
    };
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);

        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("EW-only object data blocks remain committed");
            assert_eq!(outcome.early_warning, seed <= 10, "block {seed}");
            assert!(!outcome.end_of_medium, "block {seed}");
            assert_eq!(sink.active_object_blocks_written(), Some(seed as u64));
        }

        let err = sink
            .finish_object()
            .expect_err("later object filemark EOM must still hard-abort");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("object trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after EW+filemark EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, (1..=10).collect::<Vec<_>>());
    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    assert_eq!(
        raw.events, expected_events,
        "EW before object-filemark EOM must not allow sidecar, map commit, or bootstrap writes"
    );
}

#[test]
fn finish_object_rejects_eom_on_object_filemark_before_map_commit() {
    #[derive(Debug, Default)]
    struct EomFilemarkRawTapeSink {
        cursor: u64,
        events: Vec<RawSinkEvent>,
    }

    impl RawTapeSink for EomFilemarkRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: true,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = EomFilemarkRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 1, block_size);
        sink.write_block(&fixed_block(0xA1, block_size))
            .expect("object block writes");

        let err = sink
            .finish_object()
            .expect_err("object filemark EOM must abort before map commit");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("object trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected object filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "object filemark EOM must not promote a map entry or write sidecars/bootstrap"
    );
}

#[test]
fn finish_object_rejects_eom_on_sidecar_filemark_before_map_commit() {
    #[derive(Debug)]
    struct EomOnNthFilemarkRawTapeSink {
        cursor: u64,
        events: Vec<RawSinkEvent>,
        filemark_count: usize,
        eom_on_filemark: usize,
    }

    impl RawTapeSink for EomOnNthFilemarkRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: self.filemark_count == self.eom_on_filemark,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let mut raw = EomOnNthFilemarkRawTapeSink {
        cursor: 0,
        events: Vec::new(),
        filemark_count: 0,
        eom_on_filemark: 2,
    };
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            sink.write_block(&fixed_block(seed, block_size))
                .expect("object block writes");
        }

        let err = sink
            .finish_object()
            .expect_err("sidecar filemark EOM must abort before sidecar map commit");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.filemark_count, 2);
    assert_eq!(
        raw.events.last(),
        Some(&RawSinkEvent::WriteFilemark),
        "no final bootstrap write may follow a sidecar filemark EOM"
    );
    assert!(
        raw.events
            .iter()
            .filter(|event| matches!(event, RawSinkEvent::WriteBlock(_)))
            .count()
            > 12,
        "the test must reach the sidecar body before the sidecar filemark reports EOM"
    );
}

#[test]
fn finish_object_rejects_sidecar_block_eom_even_when_early_warning_cofires() {
    #[derive(Debug)]
    struct EwEomOnNthBlockRawTapeSink {
        cursor: u64,
        events: Vec<RawSinkEvent>,
        block_count: usize,
        ew_eom_on_block: usize,
        ew_blocks_seen: Vec<usize>,
    }

    impl RawTapeSink for EwEomOnNthBlockRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.block_count += 1;
            let ew_eom = self.block_count == self.ew_eom_on_block;
            if ew_eom {
                self.ew_blocks_seen.push(self.block_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: ew_eom,
                end_of_medium: ew_eom,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let first_sidecar_block = 13;
    let mut raw = EwEomOnNthBlockRawTapeSink {
        cursor: 0,
        events: Vec::new(),
        block_count: 0,
        ew_eom_on_block: first_sidecar_block,
        ew_blocks_seen: Vec::new(),
    };
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("object block writes before sidecar EOM");
            assert!(!outcome.early_warning, "object block {seed}");
            assert!(!outcome.end_of_medium, "object block {seed}");
        }

        let err = sink
            .finish_object()
            .expect_err("sidecar block EW+EOM must abort before sidecar map commit");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar block EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, vec![first_sidecar_block]);
    let mut expected_events = vec![RawSinkEvent::WriteBlock(block_size as usize); 12];
    expected_events.push(RawSinkEvent::WriteFilemark);
    expected_events.push(RawSinkEvent::WriteBlock(block_size as usize));
    assert_eq!(
            raw.events, expected_events,
            "EW+EOM on a sidecar block must not be followed by a sidecar filemark, map commit, or final bootstrap"
        );
}

#[test]
fn finish_object_rejects_sidecar_filemark_eom_even_when_early_warning_cofires() {
    #[derive(Debug)]
    struct EwEomOnNthFilemarkRawTapeSink {
        cursor: u64,
        events: Vec<RawSinkEvent>,
        filemark_count: usize,
        ew_eom_on_filemark: usize,
        ew_filemarks_seen: Vec<usize>,
    }

    impl RawTapeSink for EwEomOnNthFilemarkRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteBlock(buf.len()));
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteBlock {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
            self.events.push(RawSinkEvent::WriteFilemark);
            self.filemark_count += 1;
            let ew_eom = self.filemark_count == self.ew_eom_on_filemark;
            if ew_eom {
                self.ew_filemarks_seen.push(self.filemark_count);
            }
            self.cursor += 1;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.cursor),
                early_warning: ew_eom,
                end_of_medium: ew_eom,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.events.push(RawSinkEvent::Position);
            Ok(PhysicalPositionHint::new(self.cursor))
        }
    }

    let block_size: u32 = 512;
    let sidecar_filemark = 2;
    let mut raw = EwEomOnNthFilemarkRawTapeSink {
        cursor: 0,
        events: Vec::new(),
        filemark_count: 0,
        ew_eom_on_filemark: sidecar_filemark,
        ew_filemarks_seen: Vec::new(),
    };
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");
        start_object(&mut sink, 12, block_size);
        for seed in 1..=12 {
            let outcome = sink
                .write_block(&fixed_block(seed, block_size))
                .expect("object block writes before sidecar filemark EOM");
            assert!(!outcome.early_warning, "object block {seed}");
            assert!(!outcome.end_of_medium, "object block {seed}");
        }

        let err = sink
            .finish_object()
            .expect_err("sidecar filemark EW+EOM must abort before sidecar map commit");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("sidecar trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected sidecar filemark EOM invariant, got {other:?}"),
        }

        let map = sink
            .filemark_map
            .clone()
            .build()
            .expect("object-only map remains valid after sidecar filemark EOM");
        assert_eq!(
            map.entries(),
            &[TapeFileMapEntry::object(0, 12, 0)],
            "EW+EOM on the sidecar filemark must not commit the sidecar map entry"
        );

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write final bootstrap after sidecar EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_filemarks_seen, vec![sidecar_filemark]);
    assert_eq!(raw.filemark_count, sidecar_filemark);
    assert_eq!(
        raw.events.last(),
        Some(&RawSinkEvent::WriteFilemark),
        "EW+EOM on the sidecar filemark must stop before final bootstrap"
    );
    assert!(
        raw.events
            .iter()
            .filter(|event| matches!(event, RawSinkEvent::WriteBlock(_)))
            .count()
            > 12,
        "the test must reach the sidecar body before the sidecar filemark reports EW+EOM"
    );
}

#[derive(Debug)]
struct BootstrapEomRawTapeSink {
    cursor: u64,
    events: Vec<RawSinkEvent>,
    block_count: usize,
    filemark_count: usize,
    ew_on_block: Option<usize>,
    ew_on_filemark: Option<usize>,
    eom_on_block: Option<usize>,
    eom_on_filemark: Option<usize>,
    ew_blocks_seen: Vec<usize>,
    ew_filemarks_seen: Vec<usize>,
}

impl BootstrapEomRawTapeSink {
    fn eom_on_block(block_number: usize) -> Self {
        Self {
            cursor: 0,
            events: Vec::new(),
            block_count: 0,
            filemark_count: 0,
            ew_on_block: None,
            ew_on_filemark: None,
            eom_on_block: Some(block_number),
            eom_on_filemark: None,
            ew_blocks_seen: Vec::new(),
            ew_filemarks_seen: Vec::new(),
        }
    }

    fn eom_on_filemark(filemark_number: usize) -> Self {
        Self {
            cursor: 0,
            events: Vec::new(),
            block_count: 0,
            filemark_count: 0,
            ew_on_block: None,
            ew_on_filemark: None,
            eom_on_block: None,
            eom_on_filemark: Some(filemark_number),
            ew_blocks_seen: Vec::new(),
            ew_filemarks_seen: Vec::new(),
        }
    }

    fn with_early_warning_on_block(mut self, block_number: usize) -> Self {
        self.ew_on_block = Some(block_number);
        self
    }

    fn with_early_warning_on_filemark(mut self, filemark_number: usize) -> Self {
        self.ew_on_filemark = Some(filemark_number);
        self
    }
}

impl RawTapeSink for BootstrapEomRawTapeSink {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteBlock(buf.len()));
        self.block_count += 1;
        let early_warning = self.ew_on_block == Some(self.block_count);
        if early_warning {
            self.ew_blocks_seen.push(self.block_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteBlock {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning,
            end_of_medium: self.eom_on_block == Some(self.block_count),
        })
    }

    fn write_filemark(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.events.push(RawSinkEvent::WriteFilemark);
        self.filemark_count += 1;
        let early_warning = self.ew_on_filemark == Some(self.filemark_count);
        if early_warning {
            self.ew_filemarks_seen.push(self.filemark_count);
        }
        self.cursor += 1;
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.cursor),
            early_warning,
            end_of_medium: self.eom_on_filemark == Some(self.filemark_count),
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.events.push(RawSinkEvent::Position);
        Ok(PhysicalPositionHint::new(self.cursor))
    }
}

#[test]
fn write_bootstrap_rejects_eom_on_bootstrap_block_before_filemark() {
    let block_size: u32 = 512;
    let mut raw = BootstrapEomRawTapeSink::eom_on_block(1);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        let err = sink
            .write_bootstrap()
            .expect_err("bootstrap block EOM must abort before filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap block EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write a final bootstrap");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![RawSinkEvent::WriteBlock(block_size as usize)],
        "bootstrap block EOM must not be followed by a filemark or final bootstrap"
    );
}

#[test]
fn write_bootstrap_rejects_eom_on_bootstrap_filemark_before_catalog_commit() {
    let block_size: u32 = 512;
    let mut raw = BootstrapEomRawTapeSink::eom_on_filemark(1);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        let err = sink
            .write_bootstrap()
            .expect_err("bootstrap filemark EOM must abort before catalog commit");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write a final bootstrap");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "bootstrap filemark EOM must not be followed by a final bootstrap"
    );
}

#[test]
fn write_bootstrap_prior_early_warning_does_not_mask_bootstrap_filemark_eom() {
    let block_size: u32 = 512;
    let mut raw = BootstrapEomRawTapeSink::eom_on_filemark(1)
        .with_early_warning_on_block(1)
        .with_early_warning_on_filemark(1);
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        let err = sink
            .write_bootstrap()
            .expect_err("bootstrap filemark EOM must win over prior EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap filemark EOM invariant, got {other:?}"),
        }

        let err = sink
            .finish()
            .expect_err("poisoned sink must not write a final bootstrap after EW+EOM");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("poisoned"), "{message}");
            }
            other => panic!("expected poisoned invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, vec![1]);
    assert_eq!(raw.ew_filemarks_seen, vec![1]);
    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "EW before bootstrap-filemark EOM must not allow a final bootstrap"
    );
}

#[test]
fn finish_rejects_eom_on_final_bootstrap_block_before_filemark() {
    let block_size: u32 = 512;
    let mut raw = BootstrapEomRawTapeSink::eom_on_block(1);
    {
        let sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        let err = sink
            .finish()
            .expect_err("final bootstrap block EOM must abort before filemark");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap block write reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected final bootstrap block EOM invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![RawSinkEvent::WriteBlock(block_size as usize)],
        "final bootstrap block EOM must not be followed by its filemark"
    );
}

#[test]
fn finish_prior_early_warning_does_not_mask_final_bootstrap_filemark_eom() {
    let block_size: u32 = 512;
    let mut raw = BootstrapEomRawTapeSink::eom_on_filemark(1)
        .with_early_warning_on_block(1)
        .with_early_warning_on_filemark(1);
    {
        let sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        let err = sink
            .finish()
            .expect_err("final bootstrap filemark EOM must win over prior EW");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap filemark EOM invariant, got {other:?}"),
        }
    }

    assert_eq!(raw.ew_blocks_seen, vec![1]);
    assert_eq!(raw.ew_filemarks_seen, vec![1]);
    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "EW before final-bootstrap filemark EOM must not be reported as a clean finish"
    );
}

#[test]
fn finish_rejects_eom_on_final_bootstrap_filemark() {
    let block_size: u32 = 512;
    let mut raw = BootstrapEomRawTapeSink::eom_on_filemark(1);
    {
        let sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sidecar-only raw sink constructs");

        let err = sink
            .finish()
            .expect_err("final bootstrap filemark EOM must abort the session");
        match err {
            ParityError::Invariant(message) => {
                assert!(
                    message.contains("bootstrap trailing filemark reached end of medium"),
                    "{message}"
                );
            }
            other => panic!("expected bootstrap filemark EOM invariant, got {other:?}"),
        }
    }

    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ],
        "final bootstrap filemark EOM must not be reported as a clean finish"
    );
}

#[test]
fn sidecar_only_from_resume_continues_rebuilt_live_epoch() {
    let block_size: u32 = 512;
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 14, 0),
        TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
    ])
    .expect("committed prefix validates");
    let live_block_a = fixed_block(0xA1, block_size);
    let live_block_b = fixed_block(0xA2, block_size);
    let live_epoch = ResumeLiveEpochState {
        epoch_id: 1,
        protected_ordinal_start: 12,
        next_data_ordinal: 14,
        data_blocks_in_epoch: 2,
        stripe_buffers: vec![
            vec![live_block_a.clone()],
            vec![live_block_b.clone()],
            Vec::new(),
        ],
        data_shard_crc64s: vec![
            data_shard_crc64(&live_block_a),
            data_shard_crc64(&live_block_b),
        ],
    };
    let resume_result = ResumeAppendResult {
        append_after_tape_file_number: 2,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 12,
        live_epoch_start: 12,
        next_data_ordinal: 14,
    };
    let append_lba: u64 = committed_prefix
        .entries()
        .iter()
        .map(|entry| entry.block_count + 1)
        .sum();
    let mut raw = RecordingRawTapeSink {
        cursor: append_lba,
        events: Vec::new(),
        blocks: Vec::new(),
    };

    let mut sink = ParitySink::new_sidecar_only_from_resume(
        &mut raw,
        small_scheme(),
        sample_uuid(),
        block_size,
        ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: Some(live_epoch),
            next_bootstrap_sequence: 7,
        },
    )
    .expect("resume sink constructs");
    assert_eq!(sink.neighborhood_idx(), 1);
    assert_eq!(sink.data_blocks_in_neighborhood(), 2);

    let (tape_file_number, _) = sink
        .begin_object_with_capacity_reserve(capacity_input_with_current_fill(
            10, 10_000, block_size, 2,
        ))
        .expect("resumed object reserve fits");
    assert_eq!(tape_file_number, 3);
    for seed in 0..10 {
        sink.write_block(&fixed_block(seed, block_size))
            .expect("append object block");
    }
    let summary = sink.finish_object().expect("object closes");

    assert_eq!(summary.tape_file_number, 3);
    assert_eq!(summary.first_parity_data_ordinal, 14);
    assert_eq!(summary.data_block_count, 10);
    assert_eq!(summary.sidecars_emitted.len(), 1);
    let sidecar = &summary.sidecars_emitted[0];
    assert_eq!(sidecar.tape_file_number, 4);
    assert_eq!(sidecar.epoch_id, 1);
    assert_eq!(sidecar.protected_ordinal_start, 12);
    assert_eq!(sidecar.protected_ordinal_end_exclusive, 24);
    assert_eq!(summary.highest_protected_ordinal, 24);
    assert_eq!(sink.neighborhood_idx(), 2);
    assert_eq!(sink.data_blocks_in_neighborhood(), 0);
    drop(sink);
    assert_eq!(
        raw.events
            .iter()
            .filter(|event| matches!(event, RawSinkEvent::WriteFilemark))
            .count(),
        2,
        "object close and the resumed sidecar each use a raw filemark barrier"
    );
}

#[test]
fn sidecar_only_from_resume_checkpoint_preserves_prefix_sidecar_directory() {
    let block_size: u32 = 512;
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 12, 0),
        TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
    ])
    .expect("committed prefix validates");
    let resume_result = ResumeAppendResult {
        append_after_tape_file_number: 2,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 12,
        live_epoch_start: 12,
        next_data_ordinal: 12,
    };
    let append_lba: u64 = committed_prefix
        .entries()
        .iter()
        .map(|entry| entry.block_count + 1)
        .sum();
    let mut raw = RecordingRawTapeSink {
        cursor: append_lba,
        events: Vec::new(),
        blocks: Vec::new(),
    };

    let checkpoint = {
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw,
            small_scheme(),
            sample_uuid(),
            block_size,
            ResumeWriterSeed {
                committed_prefix: &committed_prefix,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&committed_prefix),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: None,
                next_bootstrap_sequence: 1,
            },
        )
        .expect("resume sink constructs with prefix sidecar directory");
        sink.checkpoint()
            .expect("checkpoint after no-sidecar resume succeeds")
    };

    assert_eq!(checkpoint.bootstrap_tape_file_number, 3);
    assert_eq!(checkpoint.highest_protected_ordinal, 12);
    assert_eq!(checkpoint.total_committed_ordinals, 12);
    let payload = crate::bootstrap::parse_bootstrap_block(
        raw.blocks
            .last()
            .expect("checkpoint wrote a bootstrap block"),
    )
    .expect("checkpoint bootstrap parses");
    let directory = payload
        .sidecar_epoch_directory
        .expect("checkpoint bootstrap carries sidecar directory");
    assert_eq!(directory.directory_scope_tape_file_count, 4);
    assert_eq!(directory.directory_scope_highest_protected_ordinal, 12);
    assert_eq!(directory.entries.len(), 1);
    assert_eq!(directory.entries[0].tape_file_number, 2);
    assert_eq!(directory.entries[0].protected_ordinal_start, 0);
    assert_eq!(directory.entries[0].protected_ordinal_end_exclusive, 12);
}

#[test]
fn sidecar_only_from_resume_rejects_stale_bootstrap_sequence() {
    let block_size: u32 = 512;
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 12, 0),
        TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
        TapeFileMapEntry::bootstrap(3, 1),
    ])
    .expect("committed prefix validates");
    let resume_result = ResumeAppendResult {
        append_after_tape_file_number: 3,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 12,
        live_epoch_start: 12,
        next_data_ordinal: 12,
    };
    let append_lba: u64 = committed_prefix
        .entries()
        .iter()
        .map(|entry| entry.block_count + 1)
        .sum();
    let mut raw = RecordingRawTapeSink {
        cursor: append_lba,
        events: Vec::new(),
        blocks: Vec::new(),
    };

    let result = ParitySink::new_sidecar_only_from_resume(
        &mut raw,
        small_scheme(),
        sample_uuid(),
        block_size,
        ResumeWriterSeed {
            committed_prefix: &committed_prefix,
            committed_prefix_sidecar_directory_entries: committed_prefix_sidecar_directory_entries(
                &committed_prefix,
            ),
            committed_prefix_object_rows: Vec::new(),
            resume_result: &resume_result,
            live_epoch: None,
            next_bootstrap_sequence: 1,
        },
    );
    let err = match result {
        Ok(_) => panic!("stale bootstrap sequence must be rejected"),
        Err(err) => err,
    };
    match err {
        ParityError::Invariant(message) => {
            assert!(message.contains("bootstrap sequence"));
        }
        other => panic!("expected bootstrap-sequence invariant, got {other:?}"),
    }
    assert!(
        raw.events.is_empty(),
        "stale resume seed must be rejected before querying the raw sink"
    );
}

#[test]
fn sidecar_only_from_resume_requires_raw_cursor_at_catalog_append_point() {
    let block_size: u32 = 512;
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 12, 0),
        TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
    ])
    .expect("committed prefix validates");
    let resume_result = ResumeAppendResult {
        append_after_tape_file_number: 2,
        sidecars_emitted: Vec::new(),
        highest_protected_ordinal: 12,
        live_epoch_start: 12,
        next_data_ordinal: 12,
    };
    let expected_append = committed_prefix
        .append_position_after_prefix()
        .expect("append position computes");
    for (cursor, label) in [
        (0, "at BOT after a reset"),
        (expected_append.lba - 1, "before the catalog append point"),
        (
            expected_append.lba + 1,
            "one block past the catalog append point",
        ),
        (
            expected_append.lba + 4,
            "past a stale provisional physical tail",
        ),
    ] {
        let mut raw = RecordingRawTapeSink {
            cursor,
            events: Vec::new(),
            blocks: Vec::new(),
        };

        let result = ParitySink::new_sidecar_only_from_resume(
            &mut raw,
            small_scheme(),
            sample_uuid(),
            block_size,
            ResumeWriterSeed {
                committed_prefix: &committed_prefix,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&committed_prefix),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: None,
                next_bootstrap_sequence: 1,
            },
        );
        let err = match result {
            Ok(_) => panic!("resume sink must reject a raw cursor {label}"),
            Err(err) => err,
        };
        match err {
            ParityError::ResumeAppend(message) => {
                assert!(
                    message.contains("expected append position"),
                    "{label}: {message}"
                );
                assert!(
                    message.contains("catalog-committed prefix"),
                    "{label}: {message}"
                );
            }
            other => panic!("expected resume append cursor error for {label}, got {other:?}"),
        }
        assert_eq!(raw.events, vec![RawSinkEvent::Position], "{label}");
        assert!(
            raw.blocks.is_empty(),
            "{label}: wrong-cursor resume must reject before appending object data"
        );
    }
}

#[test]
fn sidecar_only_from_resume_cursor_guard_covers_prefix_tail_shapes() {
    let block_size: u32 = 512;
    let live_block_a = fixed_block(0xB1, block_size);
    let live_block_b = fixed_block(0xB2, block_size);
    let partial_object_live_epoch = ResumeLiveEpochState {
        epoch_id: 0,
        protected_ordinal_start: 0,
        next_data_ordinal: 2,
        data_blocks_in_epoch: 2,
        stripe_buffers: vec![
            vec![live_block_a.clone()],
            vec![live_block_b.clone()],
            Vec::new(),
        ],
        data_shard_crc64s: vec![
            data_shard_crc64(&live_block_a),
            data_shard_crc64(&live_block_b),
        ],
    };

    let cases = vec![
        (
            "partial object tail",
            FilemarkMap::new(vec![
                TapeFileMapEntry::bootstrap(0, 1),
                TapeFileMapEntry::object(1, 2, 0),
            ])
            .expect("partial-object prefix validates"),
            ResumeAppendResult {
                append_after_tape_file_number: 1,
                sidecars_emitted: Vec::new(),
                highest_protected_ordinal: 0,
                live_epoch_start: 0,
                next_data_ordinal: 2,
            },
            Some(partial_object_live_epoch),
            1,
        ),
        (
            "committed sidecar after intermediate bootstrap",
            FilemarkMap::new(vec![
                TapeFileMapEntry::bootstrap(0, 1),
                TapeFileMapEntry::object(1, 12, 0),
                TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
                TapeFileMapEntry::bootstrap(3, 1),
                TapeFileMapEntry::object(4, 12, 12),
                TapeFileMapEntry::parity_sidecar(5, 1, 1, 12, 24),
            ])
            .expect("sidecar-tail prefix validates"),
            ResumeAppendResult {
                append_after_tape_file_number: 5,
                sidecars_emitted: Vec::new(),
                highest_protected_ordinal: 24,
                live_epoch_start: 24,
                next_data_ordinal: 24,
            },
            None,
            2,
        ),
        (
            "final bootstrap tail",
            FilemarkMap::new(vec![
                TapeFileMapEntry::bootstrap(0, 1),
                TapeFileMapEntry::object(1, 12, 0),
                TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
                TapeFileMapEntry::bootstrap(3, 1),
                TapeFileMapEntry::object(4, 12, 12),
                TapeFileMapEntry::parity_sidecar(5, 1, 1, 12, 24),
                TapeFileMapEntry::bootstrap(6, 1),
            ])
            .expect("bootstrap-tail prefix validates"),
            ResumeAppendResult {
                append_after_tape_file_number: 6,
                sidecars_emitted: Vec::new(),
                highest_protected_ordinal: 24,
                live_epoch_start: 24,
                next_data_ordinal: 24,
            },
            None,
            3,
        ),
    ];

    for (label, committed_prefix, resume_result, live_epoch, next_bootstrap_sequence) in cases {
        let expected_append = committed_prefix
            .append_position_after_prefix()
            .expect("append position computes");

        for (cursor, cursor_label) in [
            (0, "at BOT after a reset"),
            (expected_append.lba - 1, "one block before the append point"),
            (expected_append.lba + 1, "one block past the append point"),
            (
                expected_append.lba + 4,
                "past a stale provisional sidecar tail",
            ),
            (
                expected_append.lba + 1_000_000,
                "far past the catalog prefix",
            ),
            (u64::MAX, "at the absolute maximum LBA"),
        ] {
            let case_label = format!("{label}; cursor {cursor_label}");
            let mut raw = RecordingRawTapeSink {
                cursor,
                events: Vec::new(),
                blocks: Vec::new(),
            };

            let result = ParitySink::new_sidecar_only_from_resume(
                &mut raw,
                small_scheme(),
                sample_uuid(),
                block_size,
                ResumeWriterSeed {
                    committed_prefix: &committed_prefix,
                    committed_prefix_sidecar_directory_entries:
                        committed_prefix_sidecar_directory_entries(&committed_prefix),
                    committed_prefix_object_rows: Vec::new(),
                    resume_result: &resume_result,
                    live_epoch: live_epoch.clone(),
                    next_bootstrap_sequence,
                },
            );
            let err = match result {
                Ok(_) => panic!("resume sink must reject {case_label}"),
                Err(err) => err,
            };
            match err {
                ParityError::ResumeAppend(message) => {
                    assert!(
                        message.contains("expected append position"),
                        "{case_label}: {message}"
                    );
                    assert!(
                        message.contains("catalog-committed prefix"),
                        "{case_label}: {message}"
                    );
                    assert!(
                        message.contains(&format!("lba: {}", expected_append.lba)),
                        "{case_label}: {message}"
                    );
                    assert!(
                        message.contains(&format!("lba: {cursor}")),
                        "{case_label}: {message}"
                    );
                }
                other => {
                    panic!("expected resume append cursor error for {case_label}, got {other:?}")
                }
            }
            assert_eq!(raw.events, vec![RawSinkEvent::Position], "{case_label}");
            assert!(
                raw.blocks.is_empty(),
                "{case_label}: wrong-cursor resume must reject before any raw block write"
            );
        }
    }
}

#[test]
fn sidecar_only_from_resume_appends_after_committed_resume_sidecars() {
    let block_size: u32 = 512;
    let committed_prefix = FilemarkMap::new(vec![
        TapeFileMapEntry::bootstrap(0, 1),
        TapeFileMapEntry::object(1, 36, 0),
        TapeFileMapEntry::parity_sidecar(2, 1, 0, 0, 12),
        TapeFileMapEntry::parity_sidecar(3, 1, 1, 12, 24),
        TapeFileMapEntry::parity_sidecar(4, 1, 2, 24, 36),
    ])
    .expect("committed prefix with resume sidecars validates");
    let original_append = committed_prefix
        .truncate_to_tape_files(3)
        .expect("original committed prefix validates")
        .append_position_after_prefix()
        .expect("original append position computes");
    let first_resume_sidecar_append = committed_prefix
        .truncate_to_tape_files(4)
        .expect("prefix after first resume sidecar validates")
        .append_position_after_prefix()
        .expect("first resume sidecar append position computes");
    let committed_append = committed_prefix
        .append_position_after_prefix()
        .expect("committed append position computes");
    assert_ne!(
        original_append, committed_append,
        "resume-generated sidecars must move the catalog append point"
    );
    let first_resume_sidecar = SidecarTapeFile {
        tape_file_number: 3,
        epoch_id: 1,
        block_count: 1,
        protected_ordinal_start: 12,
        protected_ordinal_end_exclusive: 24,
        sidecar_header_block_count: 1,
        parity_shard_block_count: 1,
        canonical_metadata_hash: [0xB1; 32],
        final_partial_epoch: false,
        filemark_outcome: WriteFilemarksOutcome::from_device_position(
            false,
            false,
            physical_to_tape_position(first_resume_sidecar_append),
        ),
    };
    let second_resume_sidecar = SidecarTapeFile {
        tape_file_number: 4,
        epoch_id: 2,
        block_count: 1,
        protected_ordinal_start: 24,
        protected_ordinal_end_exclusive: 36,
        sidecar_header_block_count: 1,
        parity_shard_block_count: 1,
        canonical_metadata_hash: [0xB2; 32],
        final_partial_epoch: false,
        filemark_outcome: WriteFilemarksOutcome::from_device_position(
            false,
            false,
            physical_to_tape_position(committed_append),
        ),
    };
    let resume_result = ResumeAppendResult {
        append_after_tape_file_number: 2,
        sidecars_emitted: vec![first_resume_sidecar, second_resume_sidecar],
        highest_protected_ordinal: 36,
        live_epoch_start: 36,
        next_data_ordinal: 36,
    };

    for (cursor, label) in [
        (original_append, "pre-rebuild append point"),
        (
            first_resume_sidecar_append,
            "append point after only the first resume sidecar",
        ),
    ] {
        let mut stale_raw = RecordingRawTapeSink {
            cursor: cursor.lba,
            events: Vec::new(),
            blocks: Vec::new(),
        };
        let result = ParitySink::new_sidecar_only_from_resume(
            &mut stale_raw,
            small_scheme(),
            sample_uuid(),
            block_size,
            ResumeWriterSeed {
                committed_prefix: &committed_prefix,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&committed_prefix),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: None,
                next_bootstrap_sequence: 1,
            },
        );
        let err = match result {
            Ok(_) => {
                panic!("resume sink must reject the {label} after multiple resume sidecars commit")
            }
            Err(err) => err,
        };
        match err {
            ParityError::ResumeAppend(message) => {
                assert!(message.contains("catalog-committed prefix"), "{message}");
                assert!(
                    message.contains(&format!("lba: {}", committed_append.lba)),
                    "{message}"
                );
                assert!(
                    message.contains(&format!("lba: {}", cursor.lba)),
                    "{message}"
                );
            }
            other => panic!("expected resume append cursor error, got {other:?}"),
        }
        assert_eq!(stale_raw.events, vec![RawSinkEvent::Position], "{label}");
        assert!(
            stale_raw.blocks.is_empty(),
            "{label}: stale cursor must reject before any raw block write"
        );
    }

    let mut raw = RecordingRawTapeSink {
        cursor: committed_append.lba,
        events: Vec::new(),
        blocks: Vec::new(),
    };
    {
        let mut sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw,
            small_scheme(),
            sample_uuid(),
            block_size,
            ResumeWriterSeed {
                committed_prefix: &committed_prefix,
                committed_prefix_sidecar_directory_entries:
                    committed_prefix_sidecar_directory_entries(&committed_prefix),
                committed_prefix_object_rows: Vec::new(),
                resume_result: &resume_result,
                live_epoch: None,
                next_bootstrap_sequence: 1,
            },
        )
        .expect("resume sink constructs at the post-sidecar append point");
        let (next_tape_file_number, _) = sink
            .begin_object_with_capacity_reserve(capacity_input_with_block_size(
                1, 10_000, block_size,
            ))
            .expect("resumed object reserve fits");
        assert_eq!(
            next_tape_file_number, 5,
            "new object must be assigned after the committed resume sidecars"
        );
        let next_object_block = fixed_block(0xE1, block_size);
        let write = sink
            .write_block(&next_object_block)
            .expect("resumed object block writes after committed sidecars");
        assert_eq!(
            write.position_after.lba,
            committed_append.lba + 1,
            "first resumed object block must append after all committed resume sidecars"
        );
        let summary = sink
            .finish_object()
            .expect("resumed object filemark writes after data");
        assert_eq!(summary.tape_file_number, 5);
        assert_eq!(summary.first_parity_data_ordinal, 36);
        assert_eq!(summary.projected_size_blocks, 1);
        assert_eq!(summary.data_block_count, 1);
        assert_eq!(summary.highest_protected_ordinal, 36);
        assert!(summary.sidecars_emitted.is_empty());
        assert_eq!(
            summary.filemark_outcome.position_after.lba,
            committed_append.lba + 2,
            "resumed object filemark must close the object after the written body block"
        );
    }
    assert_eq!(
        raw.events,
        vec![
            RawSinkEvent::Position,
            RawSinkEvent::WriteBlock(block_size as usize),
            RawSinkEvent::WriteFilemark,
        ]
    );
    assert_eq!(
        raw.blocks,
        vec![fixed_block(0xE1, block_size)],
        "resumed writer must write the next object's body only after the resume sidecars"
    );
}

#[cfg(any())]
#[test]
fn begin_object_with_capacity_reserve_starts_only_after_reserve_passes() {
    let mut inner = VecBlockSink::new();
    {
        let mut sink = ParitySink::new(&mut inner, small_scheme(), sample_uuid(), 8).unwrap();

        let err = sink
            .begin_object_with_capacity_reserve(capacity_input(2, 16))
            .expect_err("reserve should reject before object starts");
        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                ..
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::TapeCapacity);
                assert_eq!(projected_object_blocks, 2);
                assert_eq!(remaining_blocks, Some(16));
                assert_eq!(reserve_blocks, Some(18));
            }
            other => panic!("expected CapacityReserveExceeded, got {other:?}"),
        }
        assert_eq!(sink.active_object_tape_file_number(), None);

        let (tape_file_number, report) = sink
            .begin_object_with_capacity_reserve(capacity_input(2, 17))
            .expect("reserve fits");
        assert_eq!(tape_file_number, 0);
        assert_eq!(report.required_tape_blocks, 17);
        assert_eq!(sink.active_object_tape_file_number(), Some(0));

        sink.write_block(&[0xAA; 8]).unwrap();
        sink.write_block(&[0xBB; 8]).unwrap();
        assert_eq!(sink.active_object_blocks_written(), Some(2));

        let err = sink.write_block(&[0xCC; 8]).unwrap_err();
        assert!(matches!(err, TapeIoError::CheckCondition(_)));
        assert_eq!(
            sink.active_object_blocks_written(),
            Some(2),
            "projected-size overrun must be rejected before writing a third block"
        );

        let summary = sink.finish_object().expect("finish object");
        assert_eq!(summary.tape_file_number, 0);
        assert_eq!(summary.projected_size_blocks, 2);
        assert_eq!(summary.data_block_count, 2);
        assert_eq!(summary.filemark_outcome.position_after.lba, 3);
        assert_eq!(sink.active_object_tape_file_number(), None);
        let err = sink.write_block(&[0xDD; 8]).unwrap_err();
        match err {
            TapeIoError::CheckCondition(ScsiError::InvalidInput(message)) => {
                assert!(message.contains("outside active object"));
            }
            other => panic!("expected post-finish_object write rejection, got {other:?}"),
        }
    }
    assert_eq!(inner.blocks.len(), 2);
    assert_eq!(inner.filemarks, vec![1]);
}

#[test]
fn begin_object_with_capacity_reserve_rejects_tape_shortfall_before_raw_write() {
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), 8).unwrap();
        let err = sink
            .begin_object_with_capacity_reserve(capacity_input(2, 16))
            .expect_err("tape reserve should reject before object starts");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::TapeCapacity);
                assert_eq!(projected_object_blocks, 2);
                assert_eq!(remaining_blocks, Some(16));
                assert_eq!(reserve_blocks, Some(18));
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected CapacityReserveExceeded, got {other:?}"),
        }
        assert_eq!(sink.active_object_tape_file_number(), None);
    }

    assert!(
        raw.events.is_empty(),
        "tape-capacity rejection must happen before any raw tape operation"
    );
}

#[test]
fn bootstrap_object_row_fit_budget_includes_final_reference_overhead() {
    let block_size: u32 = 512;
    let mut raw = RecordingRawTapeSink::default();
    let sink = ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
        .expect("sink opens");
    let mut rows = Vec::new();
    let mut found_boundary = false;

    for tape_file_number in 0..512u32 {
        rows.push(
            BootstrapObjectRow::plaintext(
                tape_file_number,
                1,
                0,
                1,
                1,
                [tape_file_number as u8; 32],
            )
            .with_object_id([tape_file_number as u8; 16]),
        );
        if sink.validate_bootstrap_object_rows_fit(&rows).is_err()
            && legacy_object_rows_fit_without_final_overhead(&sink, &rows).is_ok()
        {
            found_boundary = true;
            break;
        }
    }

    assert!(
        found_boundary,
        "test geometry should expose a row count admitted by the old narrow model"
    );
}

#[test]
fn begin_object_with_bootstrap_row_admission_rejects_before_raw_write() {
    let block_size: u32 = 512;
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .expect("sink opens");
        let mut rows = Vec::new();
        for tape_file_number in 0..512u32 {
            let row = BootstrapObjectRow::plaintext(
                tape_file_number,
                1,
                0,
                1,
                1,
                [tape_file_number as u8; 32],
            )
            .with_object_id([tape_file_number as u8; 16]);
            let mut candidate = rows.clone();
            candidate.push(row);
            if sink.validate_bootstrap_object_rows_fit(&candidate).is_err() {
                break;
            }
            rows = candidate;
        }
        assert!(
            !rows.is_empty(),
            "test geometry should admit at least one row"
        );
        let committed_prefix = FilemarkMap::new(
            rows.iter()
                .enumerate()
                .map(|(index, row)| {
                    TapeFileMapEntry::object(
                        row.tape_file_number,
                        row.stored_block_count,
                        index as u64,
                    )
                })
                .collect(),
        )
        .expect("synthetic committed object map validates");
        sink.filemark_map = FilemarkMapBuilder::from_committed_prefix(&committed_prefix);
        sink.bootstrap_object_rows = rows;

        let err = sink
            .begin_object_with_capacity_reserve_and_bootstrap_object_row(
                capacity_input_with_block_size(1, 10_000, block_size),
                BootstrapObjectRowAdmission::PlaintextRao,
            )
            .expect_err("bootstrap row admission should reject before object start");

        assert!(
            matches!(err, ParityError::BootstrapPayloadTooLarge { .. }),
            "{err:?}"
        );
        assert_eq!(sink.active_object_tape_file_number(), None);
    }

    assert!(
        raw.events.is_empty(),
        "bootstrap-row admission rejection must happen before any raw tape operation"
    );
}

fn legacy_object_rows_fit_without_final_overhead(
    sink: &ParitySink<'_>,
    object_rows: &[BootstrapObjectRow],
) -> Result<(), ParityError> {
    let payload = BootstrapPayload {
        scheme: Some(ParitySchemeRecord {
            id: sink.scheme.id.as_str().to_string(),
            data_blocks_per_stripe: sink.scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: sink.scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: sink.scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(FilemarkMapDigest {
            map_sha256: [0; 32],
            tape_file_count: 0,
            map_total_data_ordinals: 0,
            highest_protected_ordinal: 0,
            is_final_map: false,
        }),
        tape_uuid: sink.tape_uuid,
        written_by_version: env!("CARGO_PKG_VERSION").to_string(),
        written_at: String::new(),
        sequence: 0,
        block_size_bytes: sink.block_size_bytes,
        drive_compression: false,
        sidecar_epoch_directory: None,
        parity_map_reference: None,
        object_rows: object_rows.to_vec(),
    };
    let mut block = vec![0u8; sink.block_size_bytes as usize];
    write_bootstrap_block(&payload, &mut block).map(|_| ())
}

#[test]
fn begin_object_rejects_object_larger_than_empty_tape_without_spanning() {
    let block_size: u32 = 512;
    let empty_tape_usable_blocks = 20;
    let projected_object_blocks = empty_tape_usable_blocks + 1;
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                .unwrap();
        let input = CapacityReserveInput {
            empty_tape_usable_blocks,
            ..capacity_input_with_block_size(
                projected_object_blocks,
                empty_tape_usable_blocks,
                block_size,
            )
        };
        let err = sink
            .begin_object_with_capacity_reserve(input)
            .expect_err("object larger than an empty tape must not start");

        match err {
            ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks: reported_projected,
                empty_tape_usable_blocks: reported_empty,
                required_reserve_blocks,
            } => {
                assert_eq!(reported_projected, projected_object_blocks);
                assert_eq!(reported_empty, empty_tape_usable_blocks);
                assert!(projected_object_blocks > empty_tape_usable_blocks);
                assert!(
                        required_reserve_blocks > 0,
                        "reserve accounting should still include sidecars, filemarks, bootstraps, and margin"
                    );
            }
            other => panic!("expected ObjectTooLargeForEmptyTape, got {other:?}"),
        }
        assert_eq!(sink.active_object_tape_file_number(), None);
    }

    assert!(
            raw.events.is_empty(),
            "oversized-object rejection must happen before any raw block, filemark, or spanning attempt"
        );
}

#[test]
fn begin_object_rejects_empty_tape_boundary_variants_without_spanning() {
    let block_size: u32 = 512;
    let empty_tape_usable_blocks = 20;
    let cases = [
        (
            "exact-body-fill",
            empty_tape_usable_blocks,
            true,
            "an object whose body exactly fills the usable tape still needs reserve",
        ),
        (
            "far-oversized",
            1_000_000,
            false,
            "a very large object must fail at the same pre-write gate",
        ),
    ];

    for (label, projected_object_blocks, exactly_fills_empty_tape, message) in cases {
        let mut raw = RecordingRawTapeSink::default();
        {
            let mut sink =
                ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), block_size)
                    .unwrap();
            let input = CapacityReserveInput {
                empty_tape_usable_blocks,
                ..capacity_input_with_block_size(
                    projected_object_blocks,
                    empty_tape_usable_blocks,
                    block_size,
                )
            };
            let err = sink
                .begin_object_with_capacity_reserve(input)
                .expect_err(message);

            match err {
                ParityError::ObjectTooLargeForEmptyTape {
                    projected_object_blocks: reported_projected,
                    empty_tape_usable_blocks: reported_empty,
                    required_reserve_blocks,
                } => {
                    assert_eq!(reported_projected, projected_object_blocks, "{label}");
                    assert_eq!(reported_empty, empty_tape_usable_blocks, "{label}");
                    if exactly_fills_empty_tape {
                        assert_eq!(
                            projected_object_blocks, empty_tape_usable_blocks,
                            "{label}: this case pins the exact empty-tape body boundary"
                        );
                    } else {
                        assert!(
                            projected_object_blocks > empty_tape_usable_blocks,
                            "{label}: this case pins the oversized-object boundary"
                        );
                    }
                    assert!(
                        required_reserve_blocks > 0,
                        "{label}: reserve must include sidecars, filemarks, bootstraps, and margin"
                    );
                    assert!(
                            projected_object_blocks
                                .checked_add(required_reserve_blocks)
                                .expect("test reserve arithmetic should not overflow")
                                > empty_tape_usable_blocks,
                            "{label}: projected body plus reserve must exceed the empty-tape usable capacity"
                        );
                }
                other => panic!("{label}: expected ObjectTooLargeForEmptyTape, got {other:?}"),
            }
            assert_eq!(sink.active_object_tape_file_number(), None, "{label}");
        }

        assert!(
            raw.events.is_empty(),
            "{label}: rejection must happen before any raw block, filemark, or spanning attempt"
        );
    }
}

#[test]
fn begin_object_with_capacity_reserve_rejects_spool_shortfall_before_raw_write() {
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), 8).unwrap();
        let err = sink
            .begin_object_with_capacity_reserve(CapacityReserveInput {
                remaining_spool_bytes: 95,
                ..capacity_input(12, 10_000)
            })
            .expect_err("spool reserve should reject before object starts");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::ParitySpoolCapacity);
                assert_eq!(projected_object_blocks, 12);
                assert_eq!(remaining_blocks, None);
                assert_eq!(reserve_blocks, None);
                assert_eq!(remaining_spool_bytes, Some(95));
                assert_eq!(required_spool_bytes, Some(96));
            }
            other => panic!("expected CapacityReserveExceeded, got {other:?}"),
        }
        assert_eq!(sink.active_object_tape_file_number(), None);
    }

    assert!(
        raw.events.is_empty(),
        "spool-capacity rejection must happen before any raw tape operation"
    );
}

#[test]
fn begin_object_rejects_pending_spool_shortfall_before_raw_write() {
    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), 8).unwrap();
        let input = CapacityReserveInput {
            projected_object_blocks: 1,
            pending_completed_epoch_parity_bytes: 128,
            remaining_spool_bytes: 127,
            remaining_tape_blocks: 10_000,
            ..capacity_input(1, 10_000)
        };
        let err = sink
            .begin_object_with_capacity_reserve(input)
            .expect_err("pending spool reserve should reject before object starts");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::ParitySpoolCapacity);
                assert_eq!(projected_object_blocks, 1);
                assert_eq!(remaining_blocks, None);
                assert_eq!(reserve_blocks, None);
                assert_eq!(remaining_spool_bytes, Some(127));
                assert_eq!(required_spool_bytes, Some(128));
            }
            other => panic!("expected pending spool CapacityReserveExceeded, got {other:?}"),
        }
        assert_eq!(sink.active_object_tape_file_number(), None);
    }

    assert!(
        raw.events.is_empty(),
        "pending-spool rejection must happen before any raw tape operation"
    );
}

#[test]
fn begin_object_spool_reserve_boundary_variants_do_not_touch_raw_tape() {
    let block_size: u32 = 8;
    let scheme = small_scheme();
    let data_shards_per_epoch =
        u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.data_blocks_per_stripe);
    let sidecar_tape_file_blocks = (2 * 2)
        + u64::from(scheme.stripes_per_neighborhood) * u64::from(scheme.parity_blocks_per_stripe)
        + 1
        + 1;
    let sidecar_tape_file_bytes = sidecar_tape_file_blocks * u64::from(block_size);
    let cases = [
        (
            "exact-spool-fit",
            data_shards_per_epoch,
            sidecar_tape_file_bytes,
            true,
        ),
        (
            "one-byte-short",
            data_shards_per_epoch,
            sidecar_tape_file_bytes - 1,
            false,
        ),
        (
            "far-oversized-spool-need",
            data_shards_per_epoch * 1_000,
            sidecar_tape_file_bytes,
            false,
        ),
    ];

    for (label, projected_object_blocks, remaining_spool_bytes, should_fit) in cases {
        let epochs_completed = projected_object_blocks / data_shards_per_epoch;
        let expected_required_spool_bytes = epochs_completed * sidecar_tape_file_bytes;
        let mut raw = RecordingRawTapeSink::default();
        {
            let mut sink =
                ParitySink::new_sidecar_only(&mut raw, scheme.clone(), sample_uuid(), block_size)
                    .unwrap();
            let input = CapacityReserveInput {
                projected_object_blocks,
                remaining_tape_blocks: 1_000_000,
                remaining_spool_bytes,
                ..capacity_input_with_block_size(projected_object_blocks, 1_000_000, block_size)
            };

            if should_fit {
                let (tape_file_number, report) = sink
                    .begin_object_with_capacity_reserve(input)
                    .expect("exact spool fit should admit the object");
                assert_eq!(tape_file_number, 0, "{label}");
                assert_eq!(
                    report.required_spool_bytes, expected_required_spool_bytes,
                    "{label}"
                );
                assert_eq!(
                    remaining_spool_bytes, expected_required_spool_bytes,
                    "{label}: equality at the spool boundary must be accepted"
                );
                assert_eq!(sink.active_object_tape_file_number(), Some(0), "{label}");
            } else {
                let err = sink
                    .begin_object_with_capacity_reserve(input)
                    .expect_err("spool reserve should reject before object starts");
                match err {
                    ParityError::CapacityReserveExceeded {
                        cause,
                        projected_object_blocks: reported_projected,
                        remaining_blocks,
                        reserve_blocks,
                        remaining_spool_bytes: reported_remaining_spool_bytes,
                        required_spool_bytes,
                    } => {
                        assert_eq!(
                            cause,
                            crate::CapacityReserveCause::ParitySpoolCapacity,
                            "{label}"
                        );
                        assert_eq!(reported_projected, projected_object_blocks, "{label}");
                        assert_eq!(remaining_blocks, None, "{label}");
                        assert_eq!(reserve_blocks, None, "{label}");
                        assert_eq!(
                            reported_remaining_spool_bytes,
                            Some(remaining_spool_bytes),
                            "{label}"
                        );
                        assert_eq!(
                            required_spool_bytes,
                            Some(expected_required_spool_bytes),
                            "{label}"
                        );
                        assert!(
                            remaining_spool_bytes < expected_required_spool_bytes,
                            "{label}: spool shortfall must be the failure predicate"
                        );
                    }
                    other => {
                        panic!("{label}: expected CapacityReserveExceeded, got {other:?}")
                    }
                }
                assert_eq!(sink.active_object_tape_file_number(), None, "{label}");
            }
        }

        assert!(
            raw.events.is_empty(),
            "{label}: reserve evaluation must not perform raw tape I/O"
        );
    }
}

#[test]
fn begin_object_combined_tape_and_spool_shortfall_reports_tape_capacity_first() {
    let projected_object_blocks = 12;
    let baseline = capacity_input(projected_object_blocks, u64::MAX)
        .evaluate()
        .expect("baseline reserve computes");
    assert!(baseline.required_tape_blocks > 0);
    assert!(baseline.required_spool_bytes > 0);

    let input = CapacityReserveInput {
        remaining_tape_blocks: baseline.required_tape_blocks - 1,
        remaining_spool_bytes: baseline.required_spool_bytes - 1,
        ..capacity_input(projected_object_blocks, baseline.required_tape_blocks - 1)
    };

    let spool_probe = CapacityReserveInput {
        remaining_tape_blocks: baseline.required_tape_blocks,
        ..input
    };
    match spool_probe
        .evaluate()
        .expect_err("the same input is also short on spool when tape fits")
    {
        ParityError::CapacityReserveExceeded { cause, .. } => {
            assert_eq!(cause, crate::CapacityReserveCause::ParitySpoolCapacity);
        }
        other => panic!("expected spool-capacity probe failure, got {other:?}"),
    }

    let mut raw = RecordingRawTapeSink::default();
    {
        let mut sink =
            ParitySink::new_sidecar_only(&mut raw, small_scheme(), sample_uuid(), 8).unwrap();
        let err = sink
            .begin_object_with_capacity_reserve(input)
            .expect_err("combined shortfall must reject before object starts");

        match err {
            ParityError::CapacityReserveExceeded {
                cause,
                projected_object_blocks: reported_projected,
                remaining_blocks,
                reserve_blocks,
                remaining_spool_bytes,
                required_spool_bytes,
            } => {
                assert_eq!(cause, crate::CapacityReserveCause::TapeCapacity);
                assert_eq!(reported_projected, projected_object_blocks);
                assert_eq!(remaining_blocks, Some(baseline.required_tape_blocks - 1));
                assert_eq!(reserve_blocks, Some(baseline.reserve_after_object_blocks));
                assert_eq!(remaining_spool_bytes, None);
                assert_eq!(required_spool_bytes, None);
            }
            other => panic!("expected tape-capacity failure, got {other:?}"),
        }
        assert_eq!(sink.active_object_tape_file_number(), None);
    }

    assert!(
        raw.events.is_empty(),
        "combined reserve rejection must happen before any raw tape operation"
    );
}
