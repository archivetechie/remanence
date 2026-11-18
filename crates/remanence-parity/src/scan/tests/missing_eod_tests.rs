//! Missing-mark recovery and refusal through the public entries and a model drive.
use super::{bootstrap_payload, BLOCK_SIZE, TAPE_UUID};
use crate::{
    bootstrap::write_bootstrap_block, ControlledScanWalkOutcome, DriveHandleRawSource,
    EndOfDataMark, EndOfDataRule, FilemarkMap, ParityError, PhysicalPositionHint, RawReadOutcome,
    RawTapeSource, ScanDamageKind, ScanDamagedRegion, ScanMode, ScanTailTruncation,
    ScanTailTruncationKind, ScanWalkControl, ScanWalkResult, SpaceFilemarksOutcome, TapeFileKind,
    TapeFileMapEntry,
};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{outcome::CompletionEvidence, DriveHandle, StaticAllowlist};
use std::sync::{Arc, Mutex};

const BAY: u16 = 0x100;
const TAPE: &str = "TESTTAPE";
type Sense = (u8, u8, u8);
const MISSING: Sense = (3, 0x14, 0);
const DAMAGE: Sense = (3, 0x11, 0);

fn payload() -> crate::BootstrapPayload {
    let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).unwrap();
    bootstrap_payload(map.digest(false).unwrap(), 0)
}

fn records(size: u32, fragment: bool) -> Vec<Record> {
    let mut p = payload();
    p.block_size_bytes = size;
    let mut bot = vec![0; size as usize];
    write_bootstrap_block(&p, &mut bot).unwrap();
    let mut records = vec![Record::Block(bot), Record::Filemark];
    if fragment {
        records.push(Record::Block(vec![2; size as usize]));
    }
    records
}

struct Fixture {
    drive: DriveHandle,
    world: SharedVirtualWorld,
}

impl Fixture {
    fn new(records: Vec<Record>, size: u32, missing: Option<Sense>) -> Self {
        let mut world = VirtualWorld::single_drive("TESTLIB", BAY, "TESTDRIVE", 0x400, 1);
        let keep = records.len();
        let mut tape = VirtualTape::empty(1 << 30, size);
        tape.records = records;
        world.put_tape_in_drive(BAY, TAPE, Some(0x400), tape);
        if missing.is_some() {
            world.power_cut_during_write(BAY, keep).unwrap();
        } else {
            world.power_cut(BAY, keep).unwrap();
        }
        let world = Arc::new(Mutex::new(world));
        let mut drive = DriveHandle::open_standalone_with_transport(
            std::path::Path::new("/virtual/missing-eod"),
            Box::new(ModelTransport::new(
                world.clone(),
                DeviceRole::Drive { bay: BAY },
            )),
            &StaticAllowlist::new(["TESTDRIVE"]),
        )
        .unwrap();
        assert_sense(
            &drive.test_unit_ready().unwrap_err().completion_evidence(),
            (6, 0x29, 0),
        );
        assert_eq!(drive.locate(0).unwrap().lba, 0);
        if let Some(sense) = missing {
            world
                .lock()
                .unwrap()
                .tapes
                .get_mut(TAPE)
                .unwrap()
                .missing_eod_sense = Some(sense);
        }
        Self { drive, world }
    }

    fn source(&mut self) -> Source<'_> {
        Source {
            inner: DriveHandleRawSource::new(&mut self.drive),
            world: self.world.clone(),
            calls: Vec::new(),
            failed_space: None,
            locate_fault: None,
            fail_second_space_position: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Locate(u64),
    Eod,
    Space(i64),
    Position,
    Read,
}
use Call::*;

#[derive(Clone, Copy)]
enum LocateFault {
    Fail,
    WrongReport,
}

/// Keep the earliest measuring error and the entire subsequent command sequence.
struct Source<'a> {
    inner: DriveHandleRawSource<'a>,
    world: SharedVirtualWorld,
    calls: Vec<Call>,
    failed_space: Option<(usize, CompletionEvidence)>,
    locate_fault: Option<LocateFault>,
    fail_second_space_position: bool,
}

impl RawTapeSource for Source<'_> {
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.inner.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.calls.push(Locate(hint.lba));
        if self.failed_space.is_some() && matches!(self.locate_fault, Some(LocateFault::Fail)) {
            return Err(ParityError::Invariant("fixture LOCATE failed"));
        }
        self.inner.locate_physical(hint)
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.calls.push(Eod);
        self.inner.locate_end_of_data()
    }
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        if self.fail_second_space_position && count == 1 && self.calls.contains(&Space(1)) {
            self.fail_second_space_position = false;
            self.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .next_read_position_sense = Some(MISSING);
        }
        self.calls.push(Space(count));
        let result = self.inner.space_filemarks(count);
        if let Err(ParityError::TapeIo(error)) = &result {
            if count == 1 && self.failed_space.is_none() {
                self.failed_space = Some((self.calls.len(), error.completion_evidence()));
            }
        }
        result
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.calls.push(Position);
        let mut pos = RawTapeSource::position(&mut self.inner)?;
        if self.failed_space.is_some()
            && matches!(self.locate_fault, Some(LocateFault::WrongReport))
        {
            pos.lba += 1;
        }
        Ok(pos)
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.calls.push(Read);
        self.inner.read_record(buf)
    }
}

impl Source<'_> {
    fn walk(&mut self, rule: EndOfDataRule) -> Result<ScanWalkResult, ParityError> {
        match crate::scan_reconstruct_filemark_map_with_end_of_data_rule(
            self,
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Standard,
            rule,
            |_| ScanWalkControl::Continue,
        )? {
            ControlledScanWalkOutcome::Complete(walk) => Ok(walk),
            ControlledScanWalkOutcome::Aborted(_) => panic!("unconditional walk aborted"),
        }
    }
    fn after_failure(&self) -> &[Call] {
        let (index, _) = self
            .failed_space
            .as_ref()
            .expect("walk reached failed measuring SPACE");
        &self.calls[*index..]
    }
    fn original_error(&self, error: ParityError) -> CompletionEvidence {
        let ParityError::TapeIo(error) = error else {
            panic!("{error:?}")
        };
        let evidence = error.completion_evidence();
        assert_eq!(
            evidence,
            self.failed_space
                .as_ref()
                .expect("failed measuring SPACE")
                .1
        );
        assert_eq!(evidence.opcode, Some(0x11));
        evidence
    }
}

fn assert_sense(evidence: &CompletionEvidence, expected: Sense) {
    let sense = evidence.current_sense().expect("current sense");
    assert_eq!((sense.key, sense.asc, sense.ascq), expected);
}

/// T3. Compare every pre-existing field with the clean/torn model goldens at fb5e3d0c.
#[test]
fn readable_tail_and_marked_goldens() {
    for (fragment, missing, unreadable_filemark, mark) in [
        (false, None, false, EndOfDataMark::Present),
        (true, None, false, EndOfDataMark::NotReached),
        (true, Some(MISSING), false, EndOfDataMark::NotFound),
        (true, Some((3, 0x14, 3)), true, EndOfDataMark::NotFound),
    ] {
        let mut tape = records(BLOCK_SIZE, fragment);
        if unreadable_filemark {
            tape.push(Record::Filemark);
        }
        let mut fixture = Fixture::new(tape, BLOCK_SIZE, missing);
        if unreadable_filemark {
            // An intact final file whose filemark cannot be crossed is equally unresolved.
            fixture
                .world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .space_sense_at = Some((3, MISSING));
        }
        let mut source = fixture.source();
        let walk = source.walk(EndOfDataRule::StopWhenConfirmed).unwrap();
        let start = PhysicalPositionHint::new(2);
        assert!(walk.final_parity_map.is_none());
        assert_eq!(
            walk.map,
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).unwrap()
        );
        assert_eq!(
            walk.truncation,
            fragment.then_some(ScanTailTruncation {
                tape_file_number: 1,
                position: start,
                kind: ScanTailTruncationKind::MissingTrailingFilemark,
            })
        );
        assert_eq!(
            walk.truncation_candidate_kind,
            fragment.then_some(TapeFileKind::Object)
        );
        let golden_candidate = crate::ScanBootstrapCandidate {
            tape_file_number: 0,
            payload: crate::BootstrapPayload {
                tape_uuid: [0x42; 16],
                block_size_bytes: 512,
                sequence: 0,
                written_by_version: "scan-test".into(),
                written_at: String::new(),
                drive_compression: false,
                no_parity_flag: false,
                scheme: Some(crate::ParitySchemeRecord {
                    id: "test".into(),
                    data_blocks_per_stripe: 2,
                    parity_blocks_per_stripe: 1,
                    stripes_per_neighborhood: 1,
                    no_parity_flag: false,
                }),
                filemark_map_digest: Some(crate::FilemarkMapDigest {
                    map_sha256: [
                        72, 68, 202, 176, 80, 51, 117, 246, 236, 84, 110, 185, 68, 20, 79, 100, 25,
                        117, 207, 74, 40, 64, 222, 57, 232, 204, 226, 54, 83, 250, 182, 207,
                    ],
                    tape_file_count: 1,
                    map_total_data_ordinals: 0,
                    highest_protected_ordinal: 0,
                    covers_complete_map: false,
                }),
            },
            nonzero_fill: false,
        };
        assert_eq!(walk.bootstrap_candidates, [golden_candidate]);
        assert!(walk.damaged_regions.is_empty());
        assert!(walk.bootstrap_recovery_hints.is_none());
        assert_eq!(walk.end_of_data_mark, mark);
        assert_eq!(walk.unresolved_from, missing.map(|_| start));
        if missing.is_some() {
            assert_eq!(source.after_failure(), [Locate(2), Position, Eod]);
        }
    }
}

/// T4. The failed head remains damage. At BOT the original SPACE error wins.
#[test]
fn damaged_head_and_empty_bot() {
    let mut fixture = Fixture::new(records(BLOCK_SIZE, false), BLOCK_SIZE, Some(MISSING));
    let mut source = fixture.source();
    let walk = source.walk(EndOfDataRule::StopWhenConfirmed).unwrap();
    let start = PhysicalPositionHint::new(2);
    assert_eq!(
        walk.damaged_regions,
        [ScanDamagedRegion {
            start,
            block_count: 1,
            kind: ScanDamageKind::UnreadableTapeFileHead
        }],
        "damaged region"
    );
    assert_eq!(walk.map.entries(), &[TapeFileMapEntry::bootstrap(0, 1)]);
    assert_eq!(walk.end_of_data_mark, EndOfDataMark::NotFound);
    assert_eq!(walk.unresolved_from, Some(start));
    assert!(walk.truncation.is_none());
    assert!(walk.truncation_candidate_kind.is_none());
    assert_eq!(source.after_failure(), [Locate(2), Position, Eod]);

    let mut fixture = Fixture::new(vec![], BLOCK_SIZE, Some(MISSING));
    let mut source = fixture.source();
    let err = source.walk(EndOfDataRule::StopWhenConfirmed).unwrap_err();
    assert_sense(&source.original_error(err), MISSING);
    assert!(source.after_failure().is_empty());
}

/// T5. Each legacy public walk form refuses fresh tapes with and without a fragment.
#[test]
fn defaults_refuse() {
    assert_eq!(EndOfDataRule::default(), EndOfDataRule::Refuse);
    for fragment in [true, false] {
        for form in 0..7 {
            let mut fixture =
                Fixture::new(records(BLOCK_SIZE, fragment), BLOCK_SIZE, Some(MISSING));
            let mut source = fixture.source();
            let s = &mut source;
            let r = match form {
                0 => crate::acquire_filemark_map(s, &payload(), None).map(|_| ()),
                1 => crate::acquire_filemark_map_with_report(s, &payload(), None).map(|_| ()),
                2 => crate::scan_reconstruct_filemark_map(s, &TAPE_UUID, BLOCK_SIZE).map(|_| ()),
                3 => crate::scan_reconstruct_filemark_map_with_report(s, &TAPE_UUID, BLOCK_SIZE)
                    .map(|_| ()),
                4 => crate::scan_reconstruct_filemark_map_with_report_mode(
                    s,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    ScanMode::Standard,
                )
                .map(|_| ()),
                5 => crate::scan_reconstruct_filemark_map_with_control(
                    s,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    |_| ScanWalkControl::Continue,
                )
                .map(|_| ()),
                _ => crate::scan_reconstruct_filemark_map_with_control_mode(
                    s,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    ScanMode::Standard,
                    |_| ScanWalkControl::Continue,
                )
                .map(|_| ()),
            };
            assert_sense(
                &source.original_error(r.expect_err("missing mark is an error")),
                MISSING,
            );
            assert!(source.after_failure().is_empty());
        }
    }
}

/// T6. Successful EOD, failed LOCATE, different sense or wrong LOCATE position all refuse.
#[test]
fn confirmation_refuses() {
    for (mark, locate_fault, measuring_fault, expected) in [
        (None, None, true, &[Locate(2), Position, Eod][..]),
        (Some(MISSING), Some(LocateFault::Fail), false, &[Locate(2)]),
        (Some(DAMAGE), None, true, &[Locate(2), Position, Eod]),
        (
            Some(MISSING),
            Some(LocateFault::WrongReport),
            false,
            &[Locate(2), Position],
        ),
    ] {
        let mut tape = records(BLOCK_SIZE, true);
        if mark.is_none() {
            tape.push(Record::Filemark);
        }
        let mut fixture = Fixture::new(tape, BLOCK_SIZE, mark);
        if measuring_fault {
            fixture
                .world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .space_sense_at = Some((3, MISSING));
        }
        let mut source = fixture.source();
        source.locate_fault = locate_fault;
        let err = source
            .walk(EndOfDataRule::StopWhenConfirmed)
            .expect_err("unconfirmed report is an error");
        assert_sense(&source.original_error(err), MISSING);
        assert_eq!(source.after_failure(), expected);
    }
}

/// T7. A missing-mark READ in the middle of a marked tape follows damaged-head recovery.
#[test]
fn middle_read_preserves_later_files() {
    let mut tape = records(BLOCK_SIZE, false);
    for seed in 1..=3 {
        tape.extend([
            Record::Block(vec![seed; BLOCK_SIZE as usize]),
            Record::Filemark,
        ]);
    }
    let mut fixture = Fixture::new(tape, BLOCK_SIZE, None);
    fixture
        .world
        .lock()
        .unwrap()
        .drive_states
        .get_mut(&BAY)
        .unwrap()
        .read_sense_at = Some((2, MISSING));
    let mut source = fixture.source();
    let walk = source.walk(EndOfDataRule::StopWhenConfirmed).unwrap();
    assert_eq!(
        walk.map
            .entries()
            .iter()
            .map(|e| e.tape_file_number)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3],
        "later files in the map"
    );
    assert_eq!(
        walk.damaged_regions,
        [ScanDamagedRegion {
            start: PhysicalPositionHint::new(2),
            block_count: 1,
            kind: ScanDamageKind::UnreadableTapeFileHead
        }]
    );
    assert_eq!(walk.end_of_data_mark, EndOfDataMark::Present);
    assert!(walk.unresolved_from.is_none());
    assert!(!source.calls.contains(&Eod));
}

/// T8. Another key-3 measuring error propagates without confirmation under both rules.
#[test]
fn another_medium_error_refuses() {
    for rule in [EndOfDataRule::Refuse, EndOfDataRule::StopWhenConfirmed] {
        let mut fixture = Fixture::new(records(BLOCK_SIZE, true), BLOCK_SIZE, Some(DAMAGE));
        let mut source = fixture.source();
        let err = source.walk(rule).expect_err("03/11/00 is an error");
        assert_sense(&source.original_error(err), DAMAGE);
        assert!(source.after_failure().is_empty());
    }
}

/// T9. The READ POSITION child cannot turn a good SPACE into missing-mark evidence.
#[test]
fn post_transfer_sense_refuses() {
    let mut tape = records(BLOCK_SIZE, true);
    tape.push(Record::Filemark);
    let mut fixture = Fixture::new(tape, BLOCK_SIZE, None);
    let mut source = fixture.source();
    source.fail_second_space_position = true;
    let err = source.walk(EndOfDataRule::StopWhenConfirmed).unwrap_err();
    let primary = source.original_error(err);
    assert_eq!(primary.scsi_status, Some(0));
    assert!(primary.current_sense().is_none());
    let child = primary.post_transfer.unwrap();
    assert_eq!(child.opcode, Some(0x34));
    assert_sense(&child, MISSING);
    assert!(source.after_failure().is_empty());
}

/// T10. Both full verification routes reach the BOT measurement and refuse there.
#[test]
fn verification_keeps_refusing() {
    use crate::VerificationRoute::{OnePass, Walk};
    let size = 256 * 1024;
    for (route, normal_spaces) in [(Walk, 3), (OnePass, 4)] {
        let mut fixture = Fixture::new(records(size, true), size, Some(MISSING));
        fixture
            .world
            .lock()
            .unwrap()
            .drive_states
            .get_mut(&BAY)
            .unwrap()
            .normal_eod_spaces = normal_spaces;
        let mut source = fixture.source();
        let mut statuses = Vec::new();
        let result = crate::verify_terminal_index_full_with_route(
            &mut source,
            &TAPE_UUID,
            size,
            None,
            ScanMode::Standard,
            route,
            |s| statuses.push(s),
        );
        let (index, primary) = source
            .failed_space
            .as_ref()
            .expect("BOT walk reached its measuring SPACE");
        assert_sense(primary, MISSING);
        assert_eq!(primary.opcode, Some(0x11));
        assert_eq!(
            source.calls[..*index].iter().filter(|c| **c == Eod).count(),
            normal_spaces
        );
        assert!(source.calls[..*index].ends_with(&[Locate(2), Position, Read, Space(1)]));
        assert!(
            source.after_failure().is_empty(),
            "no confirming LOCATE + SPACE(EOD): {:?}",
            source.after_failure()
        );
        assert!(
            matches!(
                result,
                Err(crate::TerminalIndexVerificationError::PrefixWalk { .. })
            ),
            "BOT error: {result:?}"
        );
        if route == OnePass {
            assert!(statuses
                .iter()
                .any(|s| s.fell_back && s.fallback_reason == Some("no_complete_layout")));
        }
    }
}
