//! PLR-1b probes preserve the model's primary completion evidence and command order.
use super::*;
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record as ModelRecord, SharedVirtualWorld, VirtualTape,
    VirtualWorld,
};
use remanence_library::{DriveHandle, StaticAllowlist};
use std::sync::{Arc, Mutex};
const BAY: u16 = 0x100;
const BARCODE: &str = "PLR-PARITY";
const MISSING: (u8, u8, u8) = (3, 0x14, 0);
struct Fixture {
    drive: DriveHandle,
    world: SharedVirtualWorld,
}
impl Fixture {
    fn new(records: Vec<Record>, missing: bool) -> Self {
        let mut tape = VirtualTape::empty(1 << 30, BLOCK_SIZE);
        tape.records = records
            .into_iter()
            .map(|r| match r {
                Record::Block(b) => ModelRecord::Block(b),
                Record::Filemark => ModelRecord::Filemark,
            })
            .collect();
        let keep = tape.records.len();
        let mut world = VirtualWorld::single_drive("PLR-LIB", BAY, "PLR-DRIVE", 0x400, 1);
        world.put_tape_in_drive(BAY, BARCODE, Some(0x400), tape);
        if missing {
            world.power_cut_during_write(BAY, keep).unwrap()
        } else {
            world.power_cut(BAY, keep).unwrap()
        }
        let world = Arc::new(Mutex::new(world));
        let mut drive = DriveHandle::open_standalone_with_transport(
            std::path::Path::new("/virtual/plr-parity"),
            Box::new(ModelTransport::new(
                world.clone(),
                DeviceRole::Drive { bay: BAY },
            )),
            &StaticAllowlist::new(["PLR-DRIVE"]),
        )
        .unwrap();
        assert!(drive.test_unit_ready().is_err());
        drive.test_unit_ready().unwrap();
        drive.locate(0).unwrap();
        world.lock().unwrap().command_log.clear();
        Self { drive, world }
    }
    fn source(&mut self, late: bool) -> Source<'_> {
        Source {
            inner: crate::DriveHandleRawSource::new(&mut self.drive),
            world: self.world.clone(),
            calls: Vec::new(),
            late,
            planned: false,
            active: false,
            eod_fault: None,
        }
    }
}
struct Source<'a> {
    inner: crate::DriveHandleRawSource<'a>,
    world: SharedVirtualWorld,
    calls: Vec<String>,
    late: bool,
    planned: bool,
    active: bool,
    eod_fault: Option<TestReadFault>,
}
impl RawTapeSource for Source<'_> {
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.inner.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.calls.push(format!("locate {}", hint.lba));
        if hint.lba == 0 && self.late && self.planned {
            self.late = false;
            self.active = true;
            let mut w = self.world.lock().unwrap();
            let state = w.drive_states.get_mut(&BAY).unwrap();
            state.read_sense_at = Some((3, MISSING));
            state.space_sense_at = Some((3, MISSING));
        }
        self.inner.locate_physical(hint)
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.calls.push("eod".into());
        if let Some(fault) = self.eod_fault {
            return Err(fault.error());
        }
        let r = self.inner.locate_end_of_data();
        if r.is_ok() {
            self.planned = true
        }
        r
    }
    fn space_filemarks(&mut self, count: i64) -> Result<crate::SpaceFilemarksOutcome, ParityError> {
        self.calls.push(format!("space {count}"));
        let result = self.inner.space_filemarks(count);
        // The model's one-shot interior SPACE fault retains its position; combine
        // it with the existing unknown-position fault to exercise the fallback.
        if self.active
            && matches!(&result,Err(ParityError::TapeIo(e)) if crate::raw::tape_error_is_end_of_data_not_found(e,0x11))
        {
            self.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .position_unknown = true;
        }
        result
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.calls.push("position".into());
        self.inner.position()
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        self.calls.push("read".into());
        if self.active {
            self.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .read_sense_at = Some((3, MISSING));
        }
        self.inner.read_record(buf)
    }
}
fn initial(route: VerificationRoute) {
    {
        let mut f = Fixture::new(vec![Record::Block(bot_bootstrap()), Record::Filemark], true);
        let mut source = f.source(false);
        let mut statuses = Vec::new();
        let error = verify_terminal_index_full_with_route(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            route,
            |s| statuses.push(s),
        )
        .unwrap_err();
        assert!(
            matches!(
                error,
                TerminalIndexVerificationError::Source {
                    operation: "SPACE(EOD)",
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(
            source.calls,
            if route == VerificationRoute::Walk {
                vec!["eod"]
            } else {
                vec!["eod", "eod"]
            }
        );
        if route == VerificationRoute::OnePass {
            assert_eq!(
                statuses.last(),
                Some(&VerificationRouteStatus {
                    route: VerificationRoute::Walk,
                    fell_back: true,
                    fallback_reason: Some("source"),
                }),
                "initial missing mark falls back for a source failure"
            );
        }
        println!(
            "{route:?} calls={:?} error={error} statuses={statuses:?}",
            source.calls
        );
        assert!(
            error
                .to_string()
                .contains("drive reports no end-of-data mark"),
            "diagnostic names missing mark ({route:?}): {error}"
        );
    }
}
fn late(route: VerificationRoute) {
    {
        let fixture = one_pass_tests::protected_fixture();
        let mut f = Fixture::new(fixture.records, false);
        let mut source = f.source(true);
        let mut statuses = Vec::new();
        let error = verify_terminal_index_full_with_route(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            None,
            ScanMode::Standard,
            route,
            |s| statuses.push(s),
        )
        .unwrap_err();
        assert!(!source.late, "prefix followed completed layout selection");
        match route {
            VerificationRoute::Walk => {
                assert!(matches!(
                    error,
                    TerminalIndexVerificationError::PrefixWalk { .. }
                ));
                assert_eq!(source.calls.last().map(String::as_str), Some("space 1"));
            }
            VerificationRoute::OnePass => {
                assert!(matches!(
                    error,
                    TerminalIndexVerificationError::Source {
                        operation: "SPACE(EOD)",
                        ..
                    }
                ));
                assert!(source.calls.ends_with(&[
                    "locate 2".into(),
                    "space 1".into(),
                    "eod".into()
                ]));
                assert_eq!(statuses.last().unwrap().fallback_reason, Some("tape_io"));
            }
        }
        println!(
            "late {route:?} calls={:?} error={error} statuses={statuses:?}",
            source.calls
        );
        assert!(
            error
                .to_string()
                .contains("drive reports no end-of-data mark"),
            "late prefix diagnostic names missing mark ({route:?}): {error}"
        );
    }
}

#[test]
fn walk_initial_names_missing_mark() {
    initial(VerificationRoute::Walk)
}
#[test]
fn one_pass_initial_names_missing_mark() {
    initial(VerificationRoute::OnePass)
}
#[test]
fn walk_late_names_missing_mark() {
    late(VerificationRoute::Walk)
}
#[test]
fn one_pass_late_names_missing_mark() {
    late(VerificationRoute::OnePass)
}
#[test]
fn primary_space_and_position_child_evidence() {
    for primary in [true, false] {
        let mut f = Fixture::new(
            vec![Record::Block(bot_bootstrap()), Record::Filemark],
            primary,
        );
        if !primary {
            f.world
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .next_read_position_sense = Some(MISSING);
        }
        let mut s = f.source(false);
        let error = s.locate_end_of_data().unwrap_err();
        let ParityError::TapeIo(ref tape_error) = error else {
            panic!("expected TapeIo: {error:?}")
        };
        assert_eq!(
            crate::raw::tape_error_is_end_of_data_not_found(tape_error, 0x11),
            primary
        );
        println!(
            "primary={primary} evidence={:?} source={:?}",
            tape_error.completion_evidence(),
            source_error("SPACE(EOD)", error)
        );
        s.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        assert_eq!(s.position().unwrap(), PhysicalPositionHint::new(0));
        assert!(matches!(
            s.read_record(&mut vec![0; BLOCK_SIZE as usize]).unwrap(),
            RawReadOutcome::Block { .. }
        ));
        assert_eq!(s.calls, ["eod", "locate 0", "position", "read"]);
    }
}

/// Rule A examines primary completion evidence before emitting or walking anything.
#[test]
fn rule_a_missing_mark_hands_off_without_visits() {
    for sense in [(3, 0x14, 0), (3, 0x14, 3)] {
        let mut f = Fixture::new(vec![Record::Block(bot_bootstrap()), Record::Filemark], true);
        f.world
            .lock()
            .unwrap()
            .tapes
            .get_mut(BARCODE)
            .unwrap()
            .missing_eod_sense = Some(sense);
        let mut source = f.source(false);
        let mut events = 0;
        let outcome =
            read_terminal_index_inventory_streamed(&mut source, &TAPE_UUID, BLOCK_SIZE, |_| {
                events += 1;
                Ok(())
            })
            .unwrap();
        let TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) = outcome else {
            panic!("missing mark requires BOT recovery")
        };
        assert_eq!(
            recovery.reason,
            BotStructuralRecoveryReason::EndOfDataNotFound
        );
        assert_eq!(recovery.replicas.len(), 3);
        assert!(recovery.replicas.iter().all(|r|matches!(r,TerminalReplicaEvidence::Invalid(f) if f.kind==TerminalReplicaFailureKind::NotInspected)));
        assert_eq!(events, 0);
        assert_eq!(source.calls, ["eod"]);
    }
}

/// Another medium error or a READ POSITION child retains the old Source conversion.
#[test]
fn rule_a_nonmatching_primary_and_position_child_remain_source() {
    for child in [false, true] {
        let make = || {
            let f = Fixture::new(
                vec![Record::Block(bot_bootstrap()), Record::Filemark],
                !child,
            );
            if child {
                f.world
                    .lock()
                    .unwrap()
                    .drive_states
                    .get_mut(&BAY)
                    .unwrap()
                    .next_read_position_sense = Some(MISSING)
            } else {
                f.world
                    .lock()
                    .unwrap()
                    .tapes
                    .get_mut(BARCODE)
                    .unwrap()
                    .missing_eod_sense = Some((3, 0x11, 0));
            }
            f
        };
        let mut expected = make();
        let expected = source_error(
            "SPACE(EOD)",
            expected.source(false).locate_end_of_data().unwrap_err(),
        );
        let mut f = make();
        let mut source = f.source(false);
        let mut events = 0;
        let result =
            read_terminal_index_inventory_streamed(&mut source, &TAPE_UUID, BLOCK_SIZE, |_| {
                events += 1;
                Ok(())
            });
        let error =
            result.expect_err("03/11/00 remains Source; READ POSITION child remains Source");
        assert!(matches!(
            error,
            TerminalInventoryReadError::Source {
                operation: "SPACE(EOD)",
                ..
            }
        ));
        assert_eq!(format!("{error:?}"), format!("{expected:?}"));
        assert_eq!(events, 0);
        assert_eq!(source.calls, ["eod"]);
    }
    for fault in [TestReadFault::Transport, TestReadFault::Cancelled] {
        let mut f = Fixture::new(vec![], false);
        let mut source = f.source(false);
        source.eod_fault = Some(fault);
        let expected = source_error("SPACE(EOD)", fault.error());
        let mut events = 0;
        let error =
            read_terminal_index_inventory_streamed(&mut source, &TAPE_UUID, BLOCK_SIZE, |_| {
                events += 1;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(format!("{error:?}"), format!("{expected:?}"));
        assert_eq!(events, 0);
        assert_eq!(source.calls, ["eod"]);
    }
}

fn recovery_records(fragment: bool) -> Vec<Record> {
    let mut records = vec![
        Record::Block(bot_bootstrap()),
        Record::Filemark,
        Record::Block(vec![2; BLOCK_SIZE as usize]),
        Record::Filemark,
    ];
    if fragment {
        records.push(Record::Block(vec![3; BLOCK_SIZE as usize]));
    }
    records
}
fn authority() -> TestBotAuthority {
    TestBotAuthority {
        scope: BotObjectRecoveryAuthorityScope {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            covered_prefix_tape_file_count: 2,
            object_row_count: 1,
        },
        second_scope: None,
        fail_after_rows_on_second_visit: None,
        rows: vec![BotObjectRecoveryAuthorityRow {
            tape_file_number: 1,
            stored_block_count: 1,
            object_id: b"committed-object".to_vec(),
        }],
        visits: 0,
    }
}

/// Every old BOT entry keeps refusal, while the explicit entries forward the measured stop.
#[test]
fn bot_entries_require_explicit_rule_and_preserve_evidence() {
    use crate::{EndOfDataMark, EndOfDataRule};
    for with_authority in [false, true] {
        for wrapper in 0..3 {
            let mut f = Fixture::new(recovery_records(true), true);
            let mut source = f.source(false);
            let mut auth = authority();
            let mut objects = 0;
            let mut visit = |_: &crate::BotRecoveredObject| {
                objects += 1;
                Ok(())
            };
            let result = match (with_authority, wrapper) {
                (false, 0) => crate::recover_terminal_inventory_from_bot(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    &mut visit,
                ),
                (false, 1) => crate::recover_terminal_inventory_from_bot_controlled(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    |_| ScanWalkControl::Continue,
                    &mut visit,
                ),
                (false, _) => crate::recover_terminal_inventory_from_bot_controlled_mode(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    ScanMode::Standard,
                    |_| ScanWalkControl::Continue,
                    &mut visit,
                ),
                (true, 0) => crate::recover_terminal_inventory_from_bot_with_authority(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    &mut auth,
                    &mut visit,
                ),
                (true, 1) => crate::recover_terminal_inventory_from_bot_with_authority_controlled(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    &mut auth,
                    |_| ScanWalkControl::Continue,
                    &mut visit,
                ),
                (true, _) => {
                    crate::recover_terminal_inventory_from_bot_with_authority_controlled_mode(
                        &mut source,
                        &TAPE_UUID,
                        BLOCK_SIZE,
                        ScanMode::Standard,
                        &mut auth,
                        |_| ScanWalkControl::Continue,
                        &mut visit,
                    )
                }
            };
            let error = result.expect_err("old entry refuses");
            assert!(error.to_string().contains(MISSING_END_OF_DATA_DIAGNOSTIC));
            assert_eq!(objects, 0);
        }
        for (fragment, missing) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut f = Fixture::new(recovery_records(fragment), missing);
            let mut source = f.source(false);
            let mut objects = Vec::new();
            let mut events = Vec::new();
            let mut auth = authority();
            let control = |e: &BotStructuralRecoveryEvent| {
                events.push(*e);
                ScanWalkControl::Continue
            };
            let visit = |o: &crate::BotRecoveredObject| {
                objects.push(o.clone());
                Ok(())
            };
            let summary=if with_authority {
                crate::recover_terminal_inventory_from_bot_with_authority_controlled_mode_with_end_of_data_rule(&mut source,&TAPE_UUID,BLOCK_SIZE,ScanMode::Standard,EndOfDataRule::StopWhenConfirmed,&mut auth,control,visit)
            }else{
                crate::recover_terminal_inventory_from_bot_controlled_mode_with_end_of_data_rule(&mut source,&TAPE_UUID,BLOCK_SIZE,ScanMode::Standard,EndOfDataRule::StopWhenConfirmed,control,visit)
            }.unwrap();
            assert_eq!(
                summary.end_of_data_mark,
                if missing {
                    EndOfDataMark::NotFound
                } else if fragment {
                    EndOfDataMark::NotReached
                } else {
                    EndOfDataMark::Present
                }
            );
            assert_eq!(
                summary.unresolved_from,
                missing.then_some(PhysicalPositionHint::new(4))
            );
            assert_eq!(summary.structural_entry_count, 2);
            assert_eq!(summary.complete_object_count, 1);
            assert_eq!(summary.recovered_object_count, u64::from(with_authority));
            assert_eq!(summary.unknown_object_count, u64::from(!with_authority));
            assert_eq!(summary.incomplete_object_count, u64::from(fragment));
            assert_eq!(
                summary.damaged_region_count,
                u64::from(missing && !fragment)
            );
            assert_eq!(objects.len(), 1 + usize::from(fragment));
            if fragment {
                assert_eq!(objects[1].state, BotRecoveredObjectState::Incomplete);
                assert_eq!(objects[1].stored_block_count, None);
            }
            assert!(matches!(
                events.first(),
                Some(BotStructuralRecoveryEvent::Started)
            ));
            assert!(
                !missing
                    || source
                        .calls
                        .windows(3)
                        .any(|c| c == ["locate 4", "position", "eod"]),
                "confirmed stop: {:?}",
                source.calls
            );
        }
    }
}

/// Recovery after damage refuses the hand-off before any second positioning attempt.
#[test]
fn post_damage_inventory_missing_mark_refuses_without_second_walk() {
    let mut f = Fixture::new(recovery_records(true), true);
    let mut source = f.source(false);
    let error = super::super::verify_terminal_index_after_damage::<Forward>(
        &mut source,
        &TAPE_UUID,
        BLOCK_SIZE,
        "prior physical damage".into(),
        &mut None,
        ScanMode::Standard,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TerminalIndexVerificationError::Source {
            operation: "SPACE(EOD)",
            ..
        }
    ));
    assert_eq!(source.calls, ["eod"]);
    assert!(error.to_string().contains(MISSING_END_OF_DATA_DIAGNOSTIC));
}
