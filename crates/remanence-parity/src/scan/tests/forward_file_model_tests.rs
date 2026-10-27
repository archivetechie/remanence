//! Exercise fixed READ, ILI, sense crediting and positioning over virtual media.
use super::*;
use crate::raw::DriveHandleRawSource;
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record as ModelRecord, VirtualTape, VirtualWorld,
};
use remanence_library::transport::TransferOutcome;
use remanence_library::{DriveHandle, ScsiError, SgTransport, StaticAllowlist, TimeoutClass};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default)]
enum Fault {
    #[default]
    None,
    Medium(u64),
    UnreadableFilemark(u64),
    Report {
        flags: u8,
        jump: i64,
        wrong_file: bool,
    },
    Disagree,
    UnconfirmedHead,
    UnconfirmedBody,
}

/// Leave record contents and motion to the model, adding wire-level fault evidence.
struct WireFault {
    inner: ModelTransport,
    records: Vec<ModelRecord>,
    fault: Fault,
    boundary: bool,
    fixed_counts: Arc<Mutex<Vec<u32>>>,
}
impl WireFault {
    fn position(&mut self) -> u64 {
        let mut response = [0; 32];
        self.inner
            .execute_in(&[0x34, 6, 0, 0, 0, 0, 0, 0, 0, 0], &mut response)
            .unwrap();
        u64::from_be_bytes(response[8..16].try_into().unwrap())
    }
    fn medium() -> ScsiError {
        ScsiError::CheckCondition {
            sense: vec![0x72, 3, 0x11, 0],
            bytes_transferred: 0,
        }
    }
}
impl SgTransport for WireFault {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if cdb[0] == 0x34 {
            let result = self.inner.execute_in(cdb, buf);
            if self.boundary && result.is_ok() {
                if let Fault::Report {
                    flags,
                    jump,
                    wrong_file,
                } = self.fault
                {
                    buf[0] |= flags;
                    let position = u64::from_be_bytes(buf[8..16].try_into().unwrap());
                    buf[8..16]
                        .copy_from_slice(&position.checked_add_signed(jump).unwrap().to_be_bytes());
                    if wrong_file {
                        buf[16..24].copy_from_slice(&99u64.to_be_bytes());
                    }
                }
            }
            self.boundary = false;
            return result;
        }
        if cdb[0] != 0x08 {
            return self.inner.execute_in(cdb, buf);
        }
        self.boundary = false;
        let before = self.position();
        let fixed = cdb[1] & 1 != 0;
        let count = if fixed {
            u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]])
        } else {
            1
        };
        if fixed {
            self.fixed_counts.lock().unwrap().push(count);
        }
        if fixed && before == 0 && matches!(self.fault, Fault::UnconfirmedHead) {
            let mut sense = vec![0; 18];
            sense[0] = 0x70; // no VALID residue: stream flags alone cannot identify the head
            sense[2] = 0x80;
            sense[7] = 10;
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        let stop = self.records[before as usize..]
            .iter()
            .position(|r| matches!(r, ModelRecord::Filemark))
            .map_or(before + u64::from(count), |i| before + i as u64 + 1);
        if let Fault::Medium(at) | Fault::UnreadableFilemark(at) = self.fault {
            if (before..(before + u64::from(count)).min(stop)).contains(&at) {
                return Err(Self::medium());
            }
        }
        // Variable reads must report oversized records as ILI, not truncate them.
        let mut scratch = vec![0; buf.len() + 1];
        let result = if fixed {
            self.inner.execute_in(cdb, buf)
        } else {
            let mut probe = cdb.to_vec();
            probe[2..5].copy_from_slice(&(scratch.len() as u32).to_be_bytes()[1..]);
            let result = self.inner.execute_in(&probe, &mut scratch);
            if let Ok(read) = &result {
                let actual = read.bytes_transferred as usize;
                let copied = actual.min(buf.len());
                buf[..copied].copy_from_slice(&scratch[..copied]);
                if actual > buf.len() {
                    let mut sense = vec![0; 18];
                    sense[0] = 0xf0;
                    sense[2] = 0x20;
                    sense[3..7].copy_from_slice(&(-1i32).to_be_bytes());
                    sense[7] = 10;
                    return Err(ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: buf.len() as u32,
                    });
                }
            }
            result
        };
        if fixed && before > 0 && matches!(self.fault, Fault::UnconfirmedBody) {
            if let Err(ScsiError::CheckCondition {
                mut sense,
                bytes_transferred,
            }) = result
            {
                self.boundary = true;
                sense[0] &= 0x7f;
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                });
            }
        }
        match result {
            Err(ScsiError::InvalidInput("fixed READ encountered non-fixed-size block"))
                if fixed =>
            {
                let mut sense = vec![0; 18];
                sense[0] = 0xf0;
                sense[2] = 0x20;
                sense[3..7].copy_from_slice(&count.to_be_bytes());
                sense[7] = 10;
                Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                })
            }
            Err(ScsiError::CheckCondition {
                mut sense,
                bytes_transferred,
            }) => {
                self.boundary = remanence_library::decode_scsi_sense(&sense)
                    .unwrap()
                    .filemark;
                // The model filled two slots, but sense falsely claims three.
                if fixed && self.boundary && matches!(self.fault, Fault::Disagree) && count == 4 {
                    assert_eq!(bytes_transferred, 2 * BLOCK_SIZE);
                    sense[3..7].copy_from_slice(&1u32.to_be_bytes());
                }
                Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred,
                })
            }
            result => result,
        }
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.boundary = false;
        if cdb[0] == 0x11 && cdb[1] & 7 == 1 {
            if let Fault::UnreadableFilemark(at) = self.fault {
                let before = self.position();
                let next = self.records[before as usize..]
                    .iter()
                    .position(|r| matches!(r, ModelRecord::Filemark))
                    .map(|i| before + i as u64);
                if next == Some(at) {
                    return Err(Self::medium());
                }
            }
        }
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        assert!(
            [0x15, 0x55].contains(&cdb[0]),
            "only MODE SELECT sends data"
        );
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

fn drive(records: &[Record], fault: Fault) -> (DriveHandle, Arc<Mutex<Vec<u32>>>) {
    let records: Vec<_> = records
        .iter()
        .map(|r| match r {
            Record::Block(b) => ModelRecord::Block(b.clone()),
            Record::Filemark | Record::UnreadableFilemark => ModelRecord::Filemark,
            Record::ReadFault(_) => ModelRecord::Block(block(0)),
        })
        .collect();
    let mut world = VirtualWorld::single_drive("TESTLIB", 0x100, "TESTDRIVE", 0x400, 1);
    let mut tape = VirtualTape::empty(1 << 30, BLOCK_SIZE);
    tape.records = records.clone();
    world.put_tape_in_drive(0x100, "TESTTAPE", Some(0x400), tape);
    let fixed_counts = Arc::new(Mutex::new(Vec::new()));
    let transport = WireFault {
        inner: ModelTransport::new(
            Arc::new(Mutex::new(world)),
            DeviceRole::Drive { bay: 0x100 },
        ),
        records,
        fault,
        boundary: false,
        fixed_counts: fixed_counts.clone(),
    };
    let drive = DriveHandle::open_standalone_with_transport(
        std::path::Path::new("/virtual/forward-reader"),
        Box::new(transport),
        &StaticAllowlist::new(["TESTDRIVE"]),
    )
    .unwrap();
    (drive, fixed_counts)
}

#[test]
fn fixed_batch_chaos_damage_vectors_match_single_record_walk() {
    use crate::scan::forward_file::test_support::FILES;
    for (name, records) in damage_vectors() {
        let fault = if name == "medium_error_before_filemark" {
            Fault::Medium(4)
        } else if name == "unreadable_filemark" {
            Fault::UnreadableFilemark(5)
        } else {
            Fault::None
        };
        let (mut walk, _) = drive(&records, fault);
        let (mut forward, counts) = drive(&records, fault);
        let walk = super::super::super::scan_reconstruct_filemark_map_with_report(
            &mut DriveHandleRawSource::new(&mut walk),
            &TAPE_UUID,
            BLOCK_SIZE,
        );
        FILES.with_borrow_mut(Vec::clear);
        let forward = forward_file_scan_for_test(
            &mut DriveHandleRawSource::new(&mut forward),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Standard,
        );
        assert_eq!(format!("{walk:?}"), format!("{forward:?}"), "{name}");
        if name == "unreadable_filemark" {
            assert!(matches!(walk, Err(ParityError::TapeIo(_))));
        }
        assert_healthy_read_through(&records, BLOCK_SIZE);
        assert!(
            counts.lock().unwrap().iter().any(|&count| count > 1),
            "{name}: fixed batches exercised"
        );
        println!("fixed batch vector: {name}");
    }
    for (name, flags, jump, wrong_file, fallback) in [
        ("position_jump", 0, 2, false, true),
        ("lonu", 4, 0, false, true),
        ("mpu", 8, 0, true, false),
        ("file_identifier_mismatch", 0, 0, true, true),
    ] {
        let records = damage_vectors().remove(0).1;
        let fault = Fault::Report {
            flags,
            jump,
            wrong_file,
        };
        let (mut walk, _) = drive(&records, fault);
        let (mut forward, _) = drive(&records, fault);
        let walk = super::super::super::scan_reconstruct_filemark_map_with_report(
            &mut DriveHandleRawSource::new(&mut walk),
            &TAPE_UUID,
            BLOCK_SIZE,
        );
        FILES.with_borrow_mut(Vec::clear);
        let forward = forward_file_scan_for_test(
            &mut DriveHandleRawSource::new(&mut forward),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Standard,
        );
        assert_eq!(format!("{walk:?}"), format!("{forward:?}"), "{name}");
        FILES.with_borrow(|files| {
            for file in files.iter().filter(|f| f.head.lba < records.len() as u64) {
                assert_eq!(file.fallbacks, usize::from(fallback), "{name}: {file:?}");
            }
        });
        println!("fixed batch vector: {name}");
    }
}

/// A prior full batch leaves recognizable bytes in the untouched buffer slots.
#[test]
fn stale_batch_bytes_never_reach_callback_or_retained_blocks() {
    let records: Vec<_> = (0..7)
        .map(|i| Record::Block(block(i)))
        .chain([Record::Filemark])
        .collect();
    let (mut drive, _) = drive(&records, Fault::Disagree);
    let mut source = DriveHandleRawSource::new(&mut drive);
    source.configure_fixed_block_size(BLOCK_SIZE).unwrap();
    source
        .locate_physical(PhysicalPositionHint::new(0))
        .unwrap();
    let mut seen = Vec::new();
    let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
        .unwrap()
        .read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(0),
            HeadProvenance::Locate,
            8,
            8,
            &mut |_, bytes| {
                seen.push(bytes[0]);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(seen, [0, 1, 2, 3, 4]);
    assert_eq!(observation.whole_blocks, 5);
    assert_eq!(observation.last_block, block(4));
    assert!(observation.requires_spacing.is_some());
    let mut classifier = ObservedClassificationSource {
        source: &mut source,
        observation: &observation,
        positioned: false,
    };
    assert_eq!(
        classifier
            .read_block(PhysicalPositionHint::new(0), 6, BLOCK_SIZE)
            .unwrap(),
        ClassificationBlockRead::Block(block(6))
    );
    assert!(
        classifier.positioned,
        "unconfirmed retained windows are unavailable"
    );
}

#[test]
fn fixed_batch_control_and_bot_recovery_vectors_match() {
    use crate::scan::forward_file::test_support::with_forward;
    let mut vectors: Vec<_> = damage_vectors()
        .into_iter()
        .map(|(name, records)| {
            let fault = match name {
                "medium_error_before_filemark" => Fault::Medium(4),
                "unreadable_filemark" => Fault::UnreadableFilemark(5),
                _ => Fault::None,
            };
            (name, records, fault)
        })
        .collect();
    for (name, flags, jump, wrong_file) in [
        ("position_jump", 0, 2, false),
        ("lonu", 4, 0, false),
        ("mpu", 8, 0, true),
        ("file_identifier_mismatch", 0, 0, true),
    ] {
        vectors.push((
            name,
            damage_vectors().remove(0).1,
            Fault::Report {
                flags,
                jump,
                wrong_file,
            },
        ));
    }
    for (name, records, fault) in vectors {
        let run = |forward| {
            let (mut drive, _) = drive(&records, fault);
            let mut source = DriveHandleRawSource::new(&mut drive);
            let mut progress = Vec::new();
            let mut scan = || {
                super::super::super::scan_reconstruct_filemark_map_with_control(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    |p| {
                        progress.push((
                            p.tape_file_number,
                            p.position,
                            p.structural_candidate_count,
                        ));
                        ScanWalkControl::Continue
                    },
                )
            };
            let result = if forward { with_forward(scan) } else { scan() };
            (format!("{result:?}"), progress)
        };
        assert_eq!(run(false), run(true), "controlled: {name}");
        let run = |forward| {
            let (mut drive, _) = drive(&records, fault);
            let mut source = DriveHandleRawSource::new(&mut drive);
            let mut objects = Vec::new();
            let mut recover = || {
                crate::recover_terminal_inventory_from_bot(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    |object| {
                        objects.push(object.clone());
                        Ok(())
                    },
                )
            };
            let result = if forward {
                with_forward(recover)
            } else {
                recover()
            };
            (format!("{result:?}"), objects)
        };
        assert_eq!(run(false), run(true), "BOT: {name}");
    }
}

#[test]
fn unconfirmed_head_is_classified_by_single_read_before_spacing() {
    let records = vec![Record::Block(block(7)), Record::Filemark];
    let (mut drive, _) = drive(&records, Fault::UnconfirmedHead);
    let mut source = DriveHandleRawSource::new(&mut drive);
    source.configure_fixed_block_size(BLOCK_SIZE).unwrap();
    source
        .locate_physical(PhysicalPositionHint::new(0))
        .unwrap();
    let mut seen = Vec::new();
    let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
        .unwrap()
        .read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(0),
            HeadProvenance::Locate,
            1,
            1,
            &mut |_, bytes| {
                seen.push(bytes[0]);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(seen, [7]);
    assert_eq!(observation.head_outcome, HeadOutcome::Block);
    assert_eq!(
        observation.requires_spacing,
        Some(SpacingReason::UncleanRead)
    );
    assert_eq!(
        observation.measure(&mut source).unwrap(),
        MeasureCurrentFileOutcome::Complete(MeasuredTapeFile {
            block_count: 1,
            position_after: PhysicalPositionHint::new(2)
        })
    );
}

/// An unconfirmed boundary after a good head uses spacing and keeps the head.
#[test]
fn non_head_unconfirmed_boundary_requires_unclean_read_spacing() {
    let records = vec![
        Record::Block(block(7)),
        Record::Block(block(8)),
        Record::Filemark,
    ];
    let (mut drive, _) = drive(&records, Fault::UnconfirmedBody);
    let mut source = DriveHandleRawSource::new(&mut drive);
    source.configure_fixed_block_size(BLOCK_SIZE).unwrap();
    source
        .locate_physical(PhysicalPositionHint::new(0))
        .unwrap();
    let mut seen = Vec::new();
    let observation = ForwardFileReader::new(BLOCK_SIZE, 4)
        .unwrap()
        .read_file(
            &mut source,
            0,
            PhysicalPositionHint::new(0),
            HeadProvenance::Locate,
            1,
            1,
            &mut |_, bytes| {
                seen.push(bytes[0]);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(seen, [7]);
    assert_eq!(observation.head_outcome, HeadOutcome::Block);
    assert_eq!(
        observation.requires_spacing,
        Some(SpacingReason::UncleanRead)
    );
    assert_eq!(
        observation.measure(&mut source).unwrap(),
        MeasureCurrentFileOutcome::Complete(MeasuredTapeFile {
            block_count: 2,
            position_after: PhysicalPositionHint::new(3)
        })
    );
}
