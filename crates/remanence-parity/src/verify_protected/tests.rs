//! Stable-address fault injection and a passive command counter for the forward
//! observation seam. This source never issues positioning commands to measure it.
use super::*;
use crate::filemark_map::FilemarkMap;
use crate::raw::{RawReadOutcome, SpaceFilemarksOutcome};
use crate::sidecar::{data_shard_crc64, encode_sidecar_tape_file, SidecarDescriptor};
use std::collections::BTreeMap;

const SIZE: u32 = 4096;
const UUID: [u8; 16] = [0x12; 16];

#[derive(Clone, Debug)]
enum Record {
    Block(Vec<u8>),
    Filemark,
    Eod,
    Medium,
    Fatal,
    Deferred,
    Hardware,
}

#[derive(Clone)]
struct Tape {
    records: Vec<Record>,
    cursor: usize,
    configured: bool,
    reads: Vec<u64>,
    locates: Vec<u64>,
    positions: usize,
    spaces: usize,
    batches: usize,
    configurations: usize,
}
impl RawTapeSource for Tape {
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        assert_eq!(size, SIZE);
        self.configured = true;
        self.configurations += 1;
        Ok(())
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.locates.push(hint.lba);
        self.cursor = hint.lba as usize;
        Ok(())
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        panic!("not used by protected pass")
    }
    fn space_filemarks(&mut self, _: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        self.spaces += 1;
        panic!("not used by protected pass")
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.positions += 1;
        Ok(PhysicalPositionHint {
            partition: 0,
            lba: self.cursor as u64,
        })
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        assert!(self.configured, "forward data must follow configuration");
        self.reads.push(self.cursor as u64);
        let record = self.records.get(self.cursor).unwrap_or(&Record::Eod);
        self.cursor += 1;
        let position_after = PhysicalPositionHint {
            partition: 0,
            lba: self.cursor as u64,
        };
        match record {
            Record::Block(bytes) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                Ok(RawReadOutcome::Block {
                    bytes: bytes.len(),
                    position_after,
                })
            }
            Record::Filemark => Ok(RawReadOutcome::Filemark { position_after }),
            Record::Eod => Ok(RawReadOutcome::EndOfData { position_after }),
            Record::Fatal => Err(ParityError::Invariant("injected non-medium read fault")),
            Record::Medium | Record::Deferred | Record::Hardware => {
                Err(remanence_library::TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![
                            if !matches!(record, Record::Deferred) {
                                0x72
                            } else {
                                0x73
                            },
                            if matches!(record, Record::Hardware) {
                                4
                            } else {
                                3
                            },
                            0x11,
                            0,
                        ],
                        bytes_transferred: 0,
                    },
                )
                .into())
            }
        }
    }
    fn read_records(
        &mut self,
        buf: &mut [u8],
        size: u32,
        max: u32,
        remaining: u32,
    ) -> crate::raw::RawReadBatch {
        self.batches += 1;
        crate::raw::read_records_default(self, buf, size, max, remaining)
    }
}

#[derive(Clone, Copy)]
enum File {
    Object(u64),
    Sidecar(u64),
    Map,
}

fn fixture(files: &[File]) -> (ScopedFilemarkMap, ParityScheme, Tape) {
    let scheme = ParityScheme {
        id: crate::SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 2,
    };
    let data_count: u64 = files
        .iter()
        .map(|f| match f {
            File::Object(n) => *n,
            _ => 0,
        })
        .sum();
    let data: Vec<_> = (0..data_count)
        .map(|i| vec![i as u8; SIZE as usize])
        .collect();
    let mut entries = vec![TapeFileMapEntry::bootstrap(0, 1)];
    let mut records = vec![Record::Block(vec![0; SIZE as usize]), Record::Filemark];
    let mut ordinal = 0;
    for (i, file) in files.iter().enumerate() {
        let file_no = i as u64 + 1;
        match file {
            File::Object(n) => {
                entries.push(TapeFileMapEntry::object(file_no, *n, ordinal));
                records.extend(
                    data[ordinal as usize..(ordinal + n) as usize]
                        .iter()
                        .cloned()
                        .map(Record::Block),
                );
                ordinal += n;
            }
            File::Map => {
                entries.push(TapeFileMapEntry::parity_map(file_no, 3));
                records.extend((0..3).map(|_| Record::Block(vec![0; SIZE as usize])));
            }
            File::Sidecar(epoch) => {
                let start = epoch * 4;
                let blocks = &data[start as usize..start as usize + 4];
                let codec = crate::codec::ReedSolomonCodec::new(&scheme).unwrap();
                let mut parity = Vec::new();
                for stripe in 0..2 {
                    parity.extend(
                        codec
                            .encode(&[blocks[stripe].clone(), blocks[stripe + 2].clone()])
                            .unwrap(),
                    );
                }
                let encoded = encode_sidecar_tape_file(
                    &SidecarDescriptor {
                        tape_uuid: UUID,
                        epoch_id: *epoch,
                        k: 2,
                        m: 1,
                        stripes_per_epoch: 2,
                        block_size: SIZE,
                        protected_ordinal_start: start,
                        protected_ordinal_end_exclusive: start + 4,
                    },
                    &parity,
                    blocks.iter().map(|b| data_shard_crc64(b)).collect(),
                )
                .unwrap();
                entries.push(TapeFileMapEntry::parity_sidecar(
                    file_no,
                    encoded.blocks.len() as u64,
                    *epoch,
                    start,
                    start + 4,
                ));
                records.extend(encoded.blocks.into_iter().map(Record::Block));
            }
        }
        records.push(Record::Filemark);
    }
    (
        ScopedFilemarkMap::from_catalog(FilemarkMap::new(entries).unwrap(), data_count),
        scheme,
        Tape {
            records,
            cursor: 0,
            configured: false,
            reads: vec![],
            locates: vec![],
            positions: 0,
            spaces: 0,
            batches: 0,
            configurations: 0,
        },
    )
}

fn run_budget(
    tape: &mut Tape,
    map: &ScopedFilemarkMap,
    scheme: &ParityScheme,
    budget: usize,
) -> (ProtectedContentVerification, ForwardPass) {
    tape.configure_fixed_block_size(SIZE).unwrap();
    let mut cache = crate::recovery::DirectoryCache::default();
    cache.preload(tape, map, &UUID, SIZE).unwrap();
    let mut pending = ForwardPass::plan(map, budget).unwrap();
    let result = verify_pass(tape, map, scheme, &UUID, SIZE, &mut cache, &mut pending).unwrap();
    assert!(pending.high_water <= budget);
    (result, pending)
}
fn lba(map: &ScopedFilemarkMap, file: u64, block: u64) -> u64 {
    map.map
        .physical_position(TapeFilePosition {
            tape_file_number: file,
            block_within_file: block,
        })
        .unwrap()
        .lba
}
fn assert_data_once(tape: &Tape, map: &ScopedFilemarkMap) {
    let mut seen = BTreeMap::new();
    for &address in &tape.reads {
        *seen.entry(address).or_insert(0) += 1;
    }
    for object in map
        .map
        .entries()
        .iter()
        .filter(|e| e.kind == TapeFileKind::Object)
    {
        for b in 0..object.block_count {
            assert_eq!(seen.get(&lba(map, object.tape_file_number, b)), Some(&1));
        }
    }
}

#[test]
fn every_observation_tag_matches_live_comparison_and_keeps_reading_after_damage() {
    for record in [
        Record::Block(vec![0; SIZE as usize]),
        Record::Block(vec![99; SIZE as usize]),
        Record::Block(vec![0; 31]),
        Record::Block(vec![0; SIZE as usize + 1]),
        Record::Filemark,
        Record::Eod,
        Record::Medium,
    ] {
        let (map, scheme, mut tape) = fixture(&[File::Object(4), File::Sidecar(0)]);
        tape.records[2] = record.clone();
        let mut before = tape.clone();
        let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap();
        let (actual, pending) = run_budget(&mut tape, &map, &scheme, PENDING_BUDGET);
        assert_eq!(actual, expected, "{record:?}");
        let reason = match &record {
            Record::Block(bytes) if bytes.len() != SIZE as usize => {
                Some(BlockFailureReason::WrongLength {
                    measured_bytes: bytes.len() as u64,
                })
            }
            Record::Block(bytes) => (bytes[0] == 99).then_some(BlockFailureReason::CrcMismatch),
            Record::Filemark | Record::Eod => Some(BlockFailureReason::UnexpectedBoundary),
            Record::Medium => Some(BlockFailureReason::Unreadable),
            _ => unreachable!("this table contains only nonfatal observations"),
        };
        assert_eq!(
            actual
                .failed_data_blocks()
                .map(|block| (block.ordinal, block.reason))
                .collect::<Vec<_>>(),
            reason
                .into_iter()
                .map(|reason| (0, reason))
                .collect::<Vec<_>>()
        );
        assert_data_once(&tape, &map);
        assert_eq!(pending.payload_high_water, 36);
        // All parity remains on the physical reader after buffered replay.
        let sidecar_start = lba(&map, 2, 0);
        assert!(tape.reads.iter().any(|&read| read == sidecar_start + 2));
        assert_eq!(tape.positions, 0, "counter must not query position");
    }
    let tags: Vec<_> = [
        Probe::Block(&[]),
        Probe::WrongLength(u64::MAX),
        Probe::Boundary("filemark where a block belongs"),
        Probe::Boundary("end-of-data where a block belongs"),
        Probe::Unreadable,
    ]
    .into_iter()
    .map(DataObservation::from_probe)
    .collect();
    assert_ne!(tags[2], tags[3], "filemark and EOD retain distinct tags");
    assert_eq!(
        tags[1].failure(),
        Some(BlockFailureReason::WrongLength {
            measured_bytes: u64::MAX
        })
    );
}

#[test]
fn non_medium_and_deferred_read_faults_propagate_in_both_schedules() {
    for fault in [Record::Fatal, Record::Deferred] {
        let (map, scheme, mut tape) = fixture(&[File::Object(4), File::Sidecar(0)]);
        tape.records[2] = fault;
        let mut before = tape.clone();
        let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap_err();
        let actual = verify_protected_content(&mut tape, &map, &scheme, &UUID, SIZE).unwrap_err();
        assert_eq!(actual.to_string(), expected.to_string());
    }
}

#[test]
fn adjacent_epochs_stream_forward_and_shared_cursor_returns_to_parity() {
    let (map, scheme, mut tape) = fixture(&[
        File::Object(12),
        File::Sidecar(0),
        File::Sidecar(1),
        File::Sidecar(2),
        File::Object(2),
        File::Object(2),
        File::Sidecar(3),
    ]);
    let mut before = tape.clone();
    let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap();
    let (actual, pending) = run_budget(&mut tape, &map, &scheme, PENDING_BUDGET);
    assert!(actual.is_clean(), "{actual:?}");
    assert_eq!(actual, expected);
    assert_data_once(&tape, &map);
    assert_eq!(
        tape.locates
            .iter()
            .filter(|&&p| (2..14).contains(&p))
            .count(),
        1,
        "adjacent epochs share the forward cursor"
    );
    for sidecar in map
        .map
        .entries()
        .iter()
        .filter(|e| e.kind == TapeFileKind::ParitySidecar)
    {
        let begin = tape
            .reads
            .iter()
            .position(|&p| p >= lba(&map, sidecar.tape_file_number, 0))
            .unwrap();
        for object in map.map.entries() {
            if let Some((first, lo, hi)) = forward::overlap(
                object,
                sidecar.protected_ordinal_start.unwrap(),
                sidecar.protected_ordinal_end_exclusive.unwrap(),
            )
            .unwrap()
            {
                for ordinal in lo..hi {
                    assert!(!tape.reads[begin..].contains(&lba(
                        &map,
                        object.tape_file_number,
                        ordinal - first
                    )));
                }
            }
        }
    }
    println!("unit four-epoch counts: Step1 LOCATE={} SPACE={} READ_POSITION={}; forward LOCATE={} SPACE={} READ_POSITION={}; payload_high_water={} total_high_water={}", before.locates.len(), before.spaces, before.positions, tape.locates.len(), tape.spaces, tape.positions, pending.payload_high_water, pending.high_water);
}

#[test]
fn budget_exhaustion_retains_observed_failure_and_reads_only_unobserved_suffix() {
    let (map, scheme, mut original) =
        fixture(&[File::Object(2), File::Object(2), File::Sidecar(0)]);
    original.records[2] = Record::Medium;
    let mut before = original.clone();
    let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap();
    // Discover the exact one-run storage from the same charged allocations.
    let mut plan = ForwardPass::plan(&map, PENDING_BUDGET).unwrap();
    let table = plan.high_water;
    let mut probe = original.clone();
    probe.configure_fixed_block_size(SIZE).unwrap();
    plan.read_object(
        &mut SequentialReader::new(&mut probe, SIZE),
        &map,
        &map.map.entries()[1],
    )
    .unwrap();
    let one_run = plan.high_water;
    assert_eq!(plan.allocated_bytes(), one_run);
    assert_eq!(plan.payload_high_water, 18);
    for budget in [
        0,
        table - 1,
        table,
        one_run - 1,
        one_run,
        one_run + 1,
        PENDING_BUDGET,
    ] {
        let mut tape = original.clone();
        let (actual, pending) = run_budget(&mut tape, &map, &scheme, budget);
        assert_eq!(actual, expected, "budget {budget}");
        assert_data_once(&tape, &map);
        assert_eq!(
            actual.failed_data_blocks().next().unwrap().reason,
            BlockFailureReason::Unreadable
        );
        assert!(pending.used <= budget);
        assert_eq!(pending.used, pending.allocated_bytes());
        assert_eq!(pending.payload, 0, "observations released after sidecar");
        if budget == one_run {
            assert_eq!(pending.high_water, one_run);
            assert_eq!(pending.payload_high_water, 18);
            let sidecar_first_read = tape
                .reads
                .iter()
                .position(|&p| p >= lba(&map, 3, 0))
                .unwrap();
            assert!(!tape.reads[sidecar_first_read..].contains(&2));
            assert!(tape.reads[sidecar_first_read..].contains(&lba(&map, 2, 0)));
            println!(
                "partly buffered storage: payload={} total={}",
                pending.payload_high_water, pending.high_water
            );
        }
    }
}

#[test]
fn conflicting_ranges_including_unvalidated_claims_are_ineligible_but_adjacent_are_not() {
    let (map, _, _) = fixture(&[File::Object(8), File::Sidecar(0), File::Sidecar(1)]);
    let plan = ForwardPass::plan(&map, PENDING_BUDGET).unwrap();
    assert!(plan.epoch(2).is_some());
    for range in [(0, 4), (3, 7), (4, 8)] {
        let mut entries = map.map.entries().to_vec();
        entries[3].protected_ordinal_start = Some(range.0);
        entries[3].protected_ordinal_end_exclusive = Some(range.1);
        let mut changed = ScopedFilemarkMap::from_catalog(FilemarkMap::new(entries).unwrap(), 8);
        for prefix in [None, Some(3)] {
            changed.validated_prefix_tape_files = prefix;
            let mut pending = ForwardPass::plan(&changed, PENDING_BUDGET).unwrap();
            let (_, _, mut tape) = fixture(&[File::Object(8), File::Sidecar(0), File::Sidecar(1)]);
            tape.configure_fixed_block_size(SIZE).unwrap();
            pending
                .read_object(
                    &mut SequentialReader::new(&mut tape, SIZE),
                    &changed,
                    &changed.map.entries()[1],
                )
                .unwrap();
            assert_eq!(pending.epoch(2).unwrap().run(1).is_some(), range.0 == 4);
        }
    }
}

#[test]
fn data_after_sidecar_including_navigational_suffix_uses_live_reads() {
    let (mut map, scheme, original) =
        fixture(&[File::Object(2), File::Sidecar(0), File::Object(2)]);
    for prefix in [None, Some(3)] {
        map.validated_prefix_tape_files = prefix;
        let mut before = original.clone();
        let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap();
        let mut after = original.clone();
        let (actual, pending) = run_budget(&mut after, &map, &scheme, PENDING_BUDGET);
        assert_eq!(actual, expected);
        assert!(actual.is_clean());
        assert_eq!(pending.payload_high_water, 0);
        assert_data_once(&after, &map);
        assert_eq!(before.reads, after.reads);
    }
}

#[test]
fn damaged_file_count_mismatch_preserves_mapped_addresses_and_findings() {
    let (map, scheme, mut original) = fixture(&[File::Object(4), File::Sidecar(0)]);
    // An early boundary does not truncate or renumber the map's remaining reads.
    original.records[3] = Record::Filemark;
    original.records[4] = Record::Eod;
    let mut before = original.clone();
    let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap();
    let (actual, _) = run_budget(&mut original, &map, &scheme, PENDING_BUDGET);
    assert_eq!(actual, expected);
    assert_eq!(
        actual
            .failed_data_blocks()
            .map(|b| b.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_data_once(&original, &map);
}

/// A ParityMap excursion invalidates the outer forward cursor even when the
/// next Object continues the same epoch. Invalid map copies remain findings.
#[test]
fn parity_map_between_objects_invalidates_the_sweep_cursor() {
    let (map, scheme, mut tape) = fixture(&[
        File::Object(2),
        File::Map,
        File::Object(2),
        File::Sidecar(0),
    ]);
    let mut before = tape.clone();
    before.configure_fixed_block_size(SIZE).unwrap();
    let expected = Step1::verify(&mut before, &map, &scheme, &UUID, SIZE).unwrap();
    let (actual, _) = run_budget(&mut tape, &map, &scheme, PENDING_BUDGET);
    assert_eq!(actual, expected);
    assert!(!actual.parity_map_findings.is_empty());
    assert!(actual.sidecars[0].is_clean());
    assert_data_once(&tape, &map);
    assert!(tape.locates.contains(&lba(&map, 3, 0)));
}

/// Setup faults must not turn the recoverer's branch-dependent directory
/// policy into a cached absence or an unconditional acquisition failure.
#[test]
fn failed_preload_retains_branch_specific_directory_fault_handling() {
    let (map, scheme, original) = fixture(&[File::Object(4), File::Sidecar(0), File::Map]);
    let map_lba = lba(&map, 3, 0) as usize;
    for (footer_valid, fault) in [
        (true, Record::Fatal),
        (false, Record::Fatal),
        (true, Record::Hardware),
        (false, Record::Hardware),
    ] {
        let mut tape = original.clone();
        tape.records[map_lba] = fault.clone();
        if !footer_valid {
            let footer = lba(&map, 2, map.map.entries()[2].block_count - 1) as usize;
            tape.records[footer] = Record::Block(vec![0; SIZE as usize]);
        }
        tape.configure_fixed_block_size(SIZE).unwrap();
        let mut cache = crate::recovery::DirectoryCache::default();
        cache.preload(&mut tape, &map, &UUID, SIZE).unwrap();
        assert!(matches!(cache, crate::recovery::DirectoryCache::Failed));
        assert_eq!(tape.cursor, 0, "setup restores the physical cursor");
        let actual = verify_sidecar_cached(
            &mut tape,
            &map,
            &map.map.entries()[2],
            &scheme,
            &UUID,
            SIZE,
            &mut cache,
            None,
        );
        // Only TapeIo faults are tolerated by a valid footer. Invariant
        // faults propagate on both branches; failed attempts remain retryable.
        assert_eq!(
            actual.is_ok(),
            footer_valid && matches!(fault, Record::Hardware)
        );
        assert_eq!(
            tape.reads.iter().filter(|&&p| p == map_lba as u64).count(),
            2,
            "failed preload must retry at acquisition"
        );
    }
}
