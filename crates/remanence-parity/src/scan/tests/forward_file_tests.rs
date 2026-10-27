//! Differential file measurements, retention, fault fallback and command bounds.
#[path = "forward_file_model_tests.rs"]
mod model_tests;

use super::*;
use crate::raw::RawPositionReport;
use crate::scan::forward_file::{HeadOutcome, SpacingReason};

const LONU_BIT: u8 = 1 << 2;
const MPU_BIT: u8 = 1 << 3;

/// Compare typed findings and errors, including bootstrap provenance and ordinals.
fn equivalent(mut walk: RecordingRawSource, mode: ScanMode<'_>) {
    compare_full_routes(&walk, &TAPE_UUID, BLOCK_SIZE, mode);
    walk.cursor = 0;
    walk.calls.clear();
    let mut forward = walk.clone();
    let walk = super::super::scan_reconstruct_filemark_map_with_report_mode(
        &mut walk, &TAPE_UUID, BLOCK_SIZE, mode,
    );
    crate::scan::forward_file::test_support::FILES.with_borrow_mut(Vec::clear);
    let records = forward.records.clone();
    let forward = forward_file_scan_for_test(&mut forward, &TAPE_UUID, BLOCK_SIZE, mode);
    assert_healthy_read_through(&records, BLOCK_SIZE);
    if let (Ok(walk), Ok(forward)) = (&walk, &forward) {
        assert_eq!(walk, forward);
    }
    assert_eq!(format!("{walk:?}"), format!("{forward:?}"));
}

fn damage_vectors() -> Vec<(&'static str, Vec<Record>)> {
    let base = vec![
        Record::Block(bootstrap_block(
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
                .unwrap()
                .digest(false)
                .unwrap(),
            0,
        )),
        Record::Filemark,
        Record::Block(block(1)),
        Record::Block(block(2)),
        Record::Block(block(3)),
        Record::Filemark,
        Record::Block(block(4)),
        Record::Filemark,
    ];
    let mut vectors = vec![("healthy", base.clone())];
    for (name, index) in [
        ("wrong_length_head", 2),
        ("wrong_length_middle", 3),
        ("wrong_length_last", 4),
    ] {
        for length in [BLOCK_SIZE as usize - 1, BLOCK_SIZE as usize + 1] {
            let mut records = base.clone();
            records[index] = Record::Block(vec![0; length]);
            vectors.push((name, records));
        }
    }
    let mut records = base.clone();
    records.insert(3, Record::Filemark);
    vectors.push(("extra_filemark_inside_object", records));
    let mut records = base.clone();
    records.remove(5);
    vectors.push(("lost_filemark_merges_files", records));
    let mut records = base.clone();
    records.insert(2, Record::Filemark);
    vectors.push(("zero_block_file_mid_prefix", records));
    let mut records = base.clone();
    records[4] = Record::Block(vec![0; BLOCK_SIZE as usize - 1]);
    records.insert(5, Record::Block(block(9)));
    // Head at 2 is read alone; the batch at 3 has a good block on either side.
    vectors.push(("short_middle_of_batch", records));
    vectors.push(("early_eod", base[..4].to_vec()));
    let mut records = base.clone();
    records[4] = Record::ReadFault(TestReadFault::Medium);
    vectors.push(("medium_error_before_filemark", records));
    let mut records = base;
    records[5] = Record::UnreadableFilemark;
    vectors.push(("unreadable_filemark", records));
    vectors
}

#[test]
fn forward_file_damage_vectors_match_walk() {
    let vectors = damage_vectors();
    for (name, records) in &vectors {
        equivalent(RecordingRawSource::new(records.clone()), ScanMode::Standard);
        println!("file vector: {name}");
    }
    let (source, _, _) = directory_scan_source();
    for index in 0..source.records.len() {
        let mut damaged = source.clone();
        if matches!(damaged.records[index], Record::Block(_)) {
            damaged.records[index] = Record::ReadFault(TestReadFault::Medium);
            equivalent(damaged, ScanMode::Standard);
        }
    }
    println!(
        "forward file reader: {} boundary/length vectors and every encoded metadata block",
        vectors.len()
    );
}

/// Fault only the post-read report or SPACE, keeping fresh sources identically faulted.
struct ReportFault {
    source: RecordingRawSource,
    flag: u8,
    jump: i64,
    wrong_file: bool,
    fail_space: bool,
}
impl RawTapeSource for ReportFault {
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.source.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, p: PhysicalPositionHint) -> Result<(), ParityError> {
        self.source.locate_physical(p)
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.source.locate_end_of_data()
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.source.position()
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.source.read_record(buf)
    }
    fn space_filemarks(&mut self, count: i64) -> Result<crate::SpaceFilemarksOutcome, ParityError> {
        if self.fail_space {
            return Err(TestReadFault::Medium.error());
        }
        self.source.space_filemarks(count)
    }
    fn position_report(&mut self) -> Result<RawPositionReport, ParityError> {
        let mut position = self.source.position()?;
        position.lba = position.lba.checked_add_signed(self.jump).unwrap();
        Ok(RawPositionReport {
            position,
            logical_file_identifier: Some(if self.wrong_file { 99 } else { 1 }),
            logical_object_number_unknown: self.flag & LONU_BIT != 0,
            mark_position_unknown: self.flag & MPU_BIT != 0,
        })
    }
}

#[test]
fn report_faults_require_spacing_and_spacing_failure_stands() {
    for (name, flag, jump, wrong_file, reason) in [
        (
            "position_jump",
            0,
            2,
            false,
            SpacingReason::PositionMismatch,
        ),
        ("lonu", 4, 0, false, SpacingReason::PositionUnknown),
        (
            "logical_file_disagrees",
            0,
            0,
            true,
            SpacingReason::FileIdentifierMismatch,
        ),
    ] {
        let make = || ReportFault {
            source: RecordingRawSource::new(vec![
                Record::Block(bootstrap_block(
                    FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
                        .unwrap()
                        .digest(false)
                        .unwrap(),
                    0,
                )),
                Record::Filemark,
            ]),
            flag,
            jump,
            wrong_file,
            fail_space: false,
        };
        let walk = super::super::scan_reconstruct_filemark_map_with_report(
            &mut make(),
            &TAPE_UUID,
            BLOCK_SIZE,
        )
        .unwrap();
        let forward =
            forward_file_scan_for_test(&mut make(), &TAPE_UUID, BLOCK_SIZE, ScanMode::Standard)
                .unwrap();
        assert_eq!(walk, forward, "{name}");
        let mut source = crate::raw::CancellableRawSource(make());
        let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
            .unwrap()
            .read_file(
                &mut source,
                0,
                PhysicalPositionHint::new(0),
                HeadProvenance::Locate,
                0,
                0,
                &mut |_, _| Ok(()),
            )
            .unwrap();
        assert_eq!(observation.requires_spacing, Some(reason), "{name}");
        let measured = observation.measure(&mut source).unwrap();
        assert_eq!(
            measured,
            measure_current_file(&mut make(), PhysicalPositionHint::new(0)).unwrap()
        );
        assert!(source
            .0
            .source
            .calls
            .windows(2)
            .any(|calls| calls == [ScanCall::Locate(0), ScanCall::SpaceFilemarks(1)]));
        source.0.fail_space = true;
        assert!(
            matches!(
                observation.measure(&mut source),
                Err(ParityError::TapeIo(_))
            ),
            "SPACE failure at unreadable filemark"
        );
    }
}

#[test]
fn healthy_reader_retains_windows_and_has_one_report_per_filemark() {
    let mut source = RecordingRawSource::new(
        (0..20)
            .map(|i| Record::Block(block(i)))
            .chain([Record::Filemark])
            .collect(),
    );
    let mut blocks = Vec::new();
    let observation = ForwardFileReader::new(BLOCK_SIZE, 7)
        .unwrap()
        .read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(0),
            HeadProvenance::Locate,
            3,
            4,
            &mut |position, bytes| {
                blocks.push((position.lba, bytes[0]));
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(observation.head_outcome, HeadOutcome::Block);
    assert_eq!(observation.head_provenance, HeadProvenance::Locate);
    assert_eq!(observation.tape_file_number, 0);
    assert_eq!(observation.whole_blocks, 20);
    assert_eq!(blocks, (0..20).map(|i| (i, i as u8)).collect::<Vec<_>>());
    assert_eq!(observation.last_block, block(19));
    let mut classifier = ObservedClassificationSource {
        source: &mut source,
        observation: &observation,
        positioned: false,
    };
    for i in [0, 1, 2, 16, 17, 18, 19] {
        assert_eq!(
            classifier
                .read_block(PhysicalPositionHint::new(0), i, BLOCK_SIZE)
                .unwrap(),
            ClassificationBlockRead::Block(block(i as u8))
        );
    }
    assert!(!classifier.positioned);
    assert_eq!(
        source
            .calls
            .iter()
            .filter(|c| matches!(c, ScanCall::Position(_)))
            .count(),
        1
    );
    assert!(!source
        .calls
        .iter()
        .any(|c| matches!(c, ScanCall::SpaceFilemarks(_) | ScanCall::Locate(_))));
    let mut classifier = ObservedClassificationSource {
        source: &mut source,
        observation: &observation,
        positioned: false,
    };
    assert_eq!(
        classifier
            .read_block(PhysicalPositionHint::new(0), 8, BLOCK_SIZE)
            .unwrap(),
        ClassificationBlockRead::Block(block(8))
    );
    assert!(classifier.positioned);
    assert!(source
        .calls
        .ends_with(&[ScanCall::Locate(8), ScanCall::ReadRecord(8)]));
}

/// All reporting scan fixtures also run through a fresh forward reader.
pub(super) fn checked_scan(
    source: &mut RecordingRawSource,
    uuid: &[u8; 16],
    size: u32,
) -> Result<ScanWalkResult, ParityError> {
    checked_scan_mode(source, uuid, size, ScanMode::Standard)
}

pub(super) fn checked_scan_mode(
    source: &mut RecordingRawSource,
    uuid: &[u8; 16],
    size: u32,
    mode: ScanMode<'_>,
) -> Result<ScanWalkResult, ParityError> {
    compare_control_and_bot(source, uuid, size, mode);
    compare_full_routes(source, uuid, size, mode);
    let mut forward = source.clone();
    let walk =
        super::super::scan_reconstruct_filemark_map_with_report_mode(source, uuid, size, mode);
    crate::scan::forward_file::test_support::FILES.with_borrow_mut(Vec::clear);
    let observed = forward_file_scan_for_test(&mut forward, uuid, size, mode);
    // Argument refusal can occur before any physical file is visited.
    if !crate::scan::forward_file::test_support::FILES.with_borrow(|files| files.is_empty()) {
        assert_healthy_read_through(&forward.records, size);
    }
    if let (Ok(walk), Ok(observed)) = (&walk, &observed) {
        assert_eq!(walk, observed);
    }
    assert_eq!(format!("{walk:?}"), format!("{observed:?}"));
    println!("scan fixture differential: matched");
    walk
}

#[test]
fn backward_device_report_faults_even_with_mpu_but_lonu_requires_spacing() {
    for flag in [0, 4, 8] {
        let mut source = ReportFault {
            source: RecordingRawSource::new(
                vec![Record::Block(block(1)); 11]
                    .into_iter()
                    .chain([Record::Filemark])
                    .collect(),
            ),
            flag,
            jump: -3,
            wrong_file: false,
            fail_space: false,
        };
        source
            .locate_physical(PhysicalPositionHint::new(10))
            .unwrap();
        let result = ForwardFileReader::new(BLOCK_SIZE, 4).unwrap().read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(10),
            HeadProvenance::Locate,
            0,
            0,
            &mut |_, _| Ok(()),
        );
        if flag != 4 {
            assert!(matches!(result, Err(ParityError::TapeIo(_))));
        } else {
            assert_eq!(
                result.unwrap().requires_spacing,
                Some(SpacingReason::PositionUnknown)
            );
        }
    }
}

#[test]
fn unreadable_filemark_spacing_error_stops_before_next_file() {
    let make = || {
        RecordingRawSource::new(vec![
            Record::Block(block(1)),
            Record::UnreadableFilemark,
            Record::Block(block(2)),
            Record::Filemark,
        ])
    };
    let walk_result = super::super::scan_reconstruct_filemark_map_with_report(
        &mut make(),
        &TAPE_UUID,
        BLOCK_SIZE,
    );
    let mut forward = make();
    let forward_result =
        forward_file_scan_for_test(&mut forward, &TAPE_UUID, BLOCK_SIZE, ScanMode::Standard);
    assert!(matches!(walk_result, Err(ParityError::TapeIo(_))));
    assert_eq!(format!("{walk_result:?}"), format!("{forward_result:?}"));
    assert!(!forward
        .calls
        .iter()
        .any(|c| matches!(c, ScanCall::ReadRecord(p) if *p >= 2)));
}

#[test]
fn transport_failure_propagates_without_spacing() {
    for position in [0, 1] {
        let mut source = RecordingRawSource::new(vec![
            Record::Block(block(1)),
            Record::Block(block(2)),
            Record::Filemark,
        ]);
        source.records[position] = Record::ReadFault(TestReadFault::Transport);
        let error = ForwardFileReader::new(BLOCK_SIZE, 4)
            .unwrap()
            .read_file(
                &mut source,
                0,
                PhysicalPositionHint::new(0),
                HeadProvenance::Locate,
                0,
                0,
                &mut |_, _| Ok(()),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            TestReadFault::Transport.error().to_string()
        );
        assert!(!source
            .calls
            .iter()
            .any(|c| matches!(c, ScanCall::SpaceFilemarks(_) | ScanCall::Locate(_))));
    }
}

/// A failed head read is final even if a retry could return different bytes.
#[test]
fn unreadable_head_is_retained_without_a_retry() {
    let mut source = RecordingRawSource::new(vec![
        Record::ReadFault(TestReadFault::Medium),
        Record::Filemark,
    ]);
    let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
        .unwrap()
        .read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(0),
            HeadProvenance::Locate,
            0,
            0,
            &mut |_, _| panic!("no whole block"),
        )
        .unwrap();
    assert_eq!(observation.head_outcome, HeadOutcome::Unreadable);
    assert_eq!(
        observation.requires_spacing,
        Some(SpacingReason::Unreadable)
    );
    assert_eq!(source.calls, [ScanCall::ReadRecord(0)]);
}

/// Non-block heads terminate measurement without any extra physical command.
#[test]
fn measure_non_block_heads_matches_walk_without_positioning() {
    for (records, expected) in [
        (
            vec![Record::Filemark],
            MeasureCurrentFileOutcome::Truncated(ScanTailTruncationKind::EmptyFile),
        ),
        (vec![], MeasureCurrentFileOutcome::EndOfData),
    ] {
        let mut source = RecordingRawSource::new(records);
        let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
            .unwrap()
            .read_file(
                &mut source,
                0,
                PhysicalPositionHint::new(0),
                HeadProvenance::Locate,
                1,
                1,
                &mut |_, _| panic!("non-block head"),
            )
            .unwrap();
        source.calls.clear();
        assert_eq!(observation.measure(&mut source).unwrap(), expected);
        assert!(source.calls.is_empty());
    }
}

#[test]
fn mpu_ignores_file_identifier_but_keeps_object_position() {
    let mut source = ReportFault {
        source: RecordingRawSource::new(vec![Record::Block(block(1)), Record::Filemark]),
        flag: 8,
        jump: 0,
        wrong_file: true,
        fail_space: false,
    };
    let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
        .unwrap()
        .read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(0),
            HeadProvenance::Locate,
            1,
            1,
            &mut |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(observation.requires_spacing, None);
    assert_eq!(
        observation.measure(&mut source).unwrap(),
        MeasureCurrentFileOutcome::Complete(MeasuredTapeFile {
            block_count: 1,
            position_after: PhysicalPositionHint::new(2)
        })
    );
    assert!(!source
        .source
        .calls
        .iter()
        .any(|c| matches!(c, ScanCall::Locate(_) | ScanCall::SpaceFilemarks(_))));
}

/// Check the fast path per physical file, including healthy neighbors of damage.
fn assert_healthy_read_through(records: &[Record], size: u32) {
    use crate::scan::forward_file::test_support::FILES;
    FILES.with_borrow(|files| {
        assert!(
            !files.is_empty(),
            "comparison must exercise the file reader"
        );
        for file in files {
            let start = file.head.lba as usize;
            let Some(rest) = records.get(start..) else {
                continue;
            };
            let Some(end) = rest.iter().position(|r| matches!(r, Record::Filemark)) else {
                continue;
            };
            if end == 0
                || !rest[..end]
                    .iter()
                    .all(|r| matches!(r, Record::Block(b) if b.len() == size as usize))
            {
                continue;
            }
            let (Record::Block(head), Record::Block(tail)) = (&rest[0], &rest[end - 1]) else {
                unreachable!()
            };
            // A damaged control head legitimately needs the footer/tail ladder.
            let damaged_control_head = (parse_parity_map_footer_block(tail, &TAPE_UUID).is_ok()
                && classify_parity_map_header_block(head, &TAPE_UUID)
                    .ok()
                    .flatten()
                    .is_none())
                || (parse_sidecar_footer_block(tail, &TAPE_UUID).is_ok()
                    && classify_sidecar_header_block(head, &TAPE_UUID)
                        .ok()
                        .flatten()
                        .is_none());
            if damaged_control_head {
                continue;
            }
            assert_eq!(file.fallbacks, 0, "healthy file at {start}: {file:?}");
            assert_eq!(
                file.positioned_reads, 0,
                "healthy file at {start}: {file:?}"
            );
        }
    });
}

/// Exercise public controlled scans and BOT helpers with the same reader switch.
fn compare_control_and_bot(
    fixture: &RecordingRawSource,
    uuid: &[u8; 16],
    size: u32,
    mode: ScanMode<'_>,
) {
    use crate::scan::forward_file::test_support::{with_forward, FILES};
    let controlled = |source: &mut RecordingRawSource| {
        let mut progress = Vec::new();
        let result = super::super::scan_reconstruct_filemark_map_with_control_mode(
            source,
            uuid,
            size,
            mode,
            |p| {
                progress.push((p.tape_file_number, p.position, p.structural_candidate_count));
                ScanWalkControl::Continue
            },
        );
        (format!("{result:?}"), progress)
    };
    let mut expected_source = fixture.clone();
    expected_source.calls.clear();
    let expected = controlled(&mut expected_source);
    FILES.with_borrow_mut(Vec::clear);
    let actual = with_forward(|| controlled(&mut fixture.clone()));
    assert_eq!(expected, actual, "controlled scan");
    if expected_source
        .calls
        .iter()
        .any(|call| matches!(call, ScanCall::ReadRecord(_)))
    {
        assert!(
            !FILES.with_borrow(|files| files.is_empty()),
            "controlled scan runs the forward reader"
        );
    }
    if !FILES.with_borrow(|files| files.is_empty()) {
        assert_healthy_read_through(&fixture.records, size);
    }

    let recover = |source: &mut RecordingRawSource| {
        let mut objects = Vec::new();
        let mut progress = Vec::new();
        let result = crate::bot_recovery::recover_terminal_inventory_from_bot_controlled_mode(
            source,
            uuid,
            size,
            mode,
            |event| {
                if let crate::BotStructuralRecoveryEvent::Progress(p) = event {
                    progress.push((p.tape_file_number, p.position, p.structural_candidate_count));
                }
                ScanWalkControl::Continue
            },
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        );
        (format!("{result:?}"), objects, progress)
    };
    let mut expected_source = fixture.clone();
    expected_source.calls.clear();
    let expected = recover(&mut expected_source);
    FILES.with_borrow_mut(Vec::clear);
    let actual = with_forward(|| recover(&mut fixture.clone()));
    assert_eq!(expected, actual, "BOT recovery");
    if expected_source
        .calls
        .iter()
        .any(|call| matches!(call, ScanCall::ReadRecord(_)))
    {
        assert!(
            !FILES.with_borrow(|files| files.is_empty()),
            "BOT recovery runs the forward reader"
        );
    }
    if !FILES.with_borrow(|files| files.is_empty()) {
        assert_healthy_read_through(&fixture.records, size);
    }
}

#[test]
fn controlled_scan_and_bot_recovery_damage_vectors_match() {
    for (_, records) in damage_vectors() {
        compare_control_and_bot(
            &RecordingRawSource::new(records),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Standard,
        );
    }
}

#[test]
fn controlled_scan_and_bot_abort_at_same_file_boundary() {
    use crate::scan::forward_file::test_support::with_forward;
    let fixture = RecordingRawSource::new(damage_vectors().remove(0).1);
    for last in [0, 1, 2] {
        let scan = |source: &mut RecordingRawSource| {
            let result = super::super::scan_reconstruct_filemark_map_with_control(
                source,
                &TAPE_UUID,
                BLOCK_SIZE,
                |p| {
                    if p.tape_file_number == last {
                        ScanWalkControl::Abort
                    } else {
                        ScanWalkControl::Continue
                    }
                },
            )
            .unwrap();
            let ControlledScanWalkOutcome::Aborted(abort) = result else {
                panic!("expected abort")
            };
            (
                abort.last_tape_file_number,
                abort.position,
                abort.structural_candidate_count,
            )
        };
        let mut forward = fixture.clone();
        let expected = scan(&mut fixture.clone());
        let actual = with_forward(|| scan(&mut forward));
        assert_eq!(expected, actual);
        assert!(!forward
            .calls
            .iter()
            .any(|c| matches!(c, ScanCall::ReadRecord(at) if *at >= actual.1.lba)));
        let recover = |source: &mut RecordingRawSource| {
            let error = crate::bot_recovery::recover_terminal_inventory_from_bot_controlled_mode(
                source, &TAPE_UUID, BLOCK_SIZE, ScanMode::Standard,
                |event| if matches!(event, crate::BotStructuralRecoveryEvent::Progress(p) if p.tape_file_number == last) {
                    ScanWalkControl::Abort
                } else { ScanWalkControl::Continue },
                |_| panic!("aborted recovery emits no objects"),
            ).unwrap_err();
            let crate::BotStructuralRecoveryError::Aborted {
                last_tape_file_number,
                position,
                structural_candidate_count,
                ..
            } = error
            else {
                panic!("expected abort")
            };
            (last_tape_file_number, position, structural_candidate_count)
        };
        let mut forward = fixture.clone();
        let mut expected_source = fixture.clone();
        expected_source.calls.clear();
        let expected = recover(&mut expected_source);
        let actual = with_forward(|| recover(&mut forward));
        assert_eq!(expected, actual);
        assert!(!forward
            .calls
            .iter()
            .any(|c| matches!(c, ScanCall::ReadRecord(at) if *at >= actual.1.unwrap().lba)));
    }
}

#[test]
fn next_head_provenance_tracks_fallback_and_classification_locates() {
    use crate::scan::forward_file::test_support::FILES;
    for classification in [false, true] {
        let (mut fixture, _, _) = directory_scan_source();
        // The sidecar starts after BOT and its two-block Object.
        let sidecar_head = 5;
        if classification {
            let Record::Block(head) = &mut fixture.records[sidecar_head] else {
                panic!("sidecar head")
            };
            head.fill(0);
        } else {
            fixture.records[sidecar_head] = Record::ReadFault(TestReadFault::Medium);
        }
        FILES.with_borrow_mut(Vec::clear);
        forward_file_scan_for_test(&mut fixture, &TAPE_UUID, BLOCK_SIZE, ScanMode::Standard)
            .unwrap();
        FILES.with_borrow(|files| {
            let index = files
                .iter()
                .position(|f| f.head.lba == sidecar_head as u64)
                .unwrap();
            let file = &files[index];
            if classification {
                assert!(file.positioned_reads > 0);
            } else {
                assert_eq!(file.fallbacks, 1);
            }
            assert_eq!(files[index + 1].provenance, HeadProvenance::Locate);
            assert_eq!(files[1].provenance, HeadProvenance::DeviceReport);
        });
    }
}

/// Fresh sources preserve the same record faults for both full-verification routes.
fn compare_full_routes(
    fixture: &RecordingRawSource,
    uuid: &[u8; 16],
    size: u32,
    mode: ScanMode<'_>,
) {
    let walk = crate::verify_terminal_index_full_with_scan_mode(
        &mut fixture.clone(),
        uuid,
        size,
        None,
        mode,
    );
    let mut fallback = false;
    let observed = crate::verify_terminal_index_full_with_route(
        &mut fixture.clone(),
        uuid,
        size,
        None,
        mode,
        crate::VerificationRoute::OnePass,
        |status| fallback |= status.fell_back,
    );
    if let (Ok(walk), Ok(observed)) = (&walk, &observed) {
        assert_eq!(walk, observed);
        if walk.is_complete_tape() {
            assert!(!fallback);
        }
    }
    assert_eq!(format!("{walk:?}"), format!("{observed:?}"));
    println!(
        "scan full route: {}",
        if fallback {
            "fallback"
        } else if observed.is_ok() {
            "success"
        } else {
            "fault"
        }
    );
}
