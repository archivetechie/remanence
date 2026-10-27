//! Count the physical prefix sweep without adding commands to observe it.
use super::*;
use remanence_parity::raw::{RawForwardReadBatch, RawPositionReport, RawReadBatch};

struct Counted<S> {
    inner: S,
    cursor: Option<PhysicalPositionHint>,
    locates: Vec<u64>,
    spaces: Vec<Option<u64>>,
    positions: usize,
    reads: BTreeMap<u64, usize>,
    eod: usize,
}
impl<S: RawTapeSource> RawTapeSource for Counted<S> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.inner.verification_context()
    }
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.inner.configure_fixed_block_size(size)
    }
    fn locate_physical(&mut self, p: PhysicalPositionHint) -> Result<(), ParityError> {
        self.locates.push(p.lba);
        self.inner.locate_physical(p)?;
        self.cursor = Some(p);
        Ok(())
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.eod += 1;
        let p = self.inner.locate_end_of_data()?;
        self.cursor = Some(p);
        Ok(p)
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let p = self.inner.position()?;
        self.cursor = Some(p);
        Ok(p)
    }
    fn position_report(&mut self) -> Result<RawPositionReport, ParityError> {
        self.positions += 1;
        let report = self.inner.position_report()?;
        self.cursor = (!report.logical_object_number_unknown).then_some(report.position);
        Ok(report)
    }
    fn space_filemarks(
        &mut self,
        count: i64,
    ) -> Result<remanence_parity::SpaceFilemarksOutcome, ParityError> {
        self.spaces.push(self.cursor.map(|p| p.lba));
        let outcome = self.inner.space_filemarks(count)?;
        self.cursor = Some(outcome.position_after);
        Ok(outcome)
    }
    fn read_record(
        &mut self,
        bytes: &mut [u8],
    ) -> Result<remanence_parity::RawReadOutcome, ParityError> {
        let before = self.cursor;
        let result = self.inner.read_record(bytes);
        if let Some(before) = before {
            *self.reads.entry(before.lba).or_default() += 1;
        }
        self.cursor = result.as_ref().ok().map(|r| r.position_after());
        result
    }
    fn read_records(
        &mut self,
        bytes: &mut [u8],
        size: u32,
        count: u32,
        remaining: u32,
    ) -> RawReadBatch {
        let before = self.cursor;
        let result = self.inner.read_records(bytes, size, count, remaining);
        self.account(before, result.records_read);
        self.cursor = result.position_after;
        result
    }
    fn read_forward_records(
        &mut self,
        bytes: &mut [u8],
        size: u32,
        count: u32,
    ) -> RawForwardReadBatch {
        let before = self.cursor;
        let result = self.inner.read_forward_records(bytes, size, count);
        self.account(before, result.batch.records_read);
        self.cursor = result.batch.position_after;
        result
    }
}
impl<S> Counted<S> {
    fn account(&mut self, before: Option<PhysicalPositionHint>, count: u32) {
        if let Some(before) = before {
            for lba in before.lba..before.lba + u64::from(count) {
                *self.reads.entry(lba).or_default() += 1;
            }
        }
    }
}

pub(super) fn compare(vector: &VectorImage) {
    compare_retracted(vector);
    let uuid = vector.written.inputs.tape_uuid;
    let size = vector.written.inputs.block_size;
    let mut outcomes = Vec::new();
    let mut totals = Vec::new();
    for wrapped in [false, true] {
        for route in [
            remanence_parity::VerificationRoute::Walk,
            remanence_parity::VerificationRoute::OnePass,
        ] {
            let (mut drive, _, log) =
                source_with_command_log(vector, &json!({"unreadable_records": []}));
            let io = drive.begin_verification_io();
            let mut source = Counted {
                inner: DriveHandleRawSource::new(&mut drive),
                cursor: None,
                locates: vec![],
                spaces: vec![],
                positions: 0,
                reads: BTreeMap::new(),
                eod: 0,
            };
            let mut fell_back = false;
            let mut wrapped_source = remanence_parity::raw::CancellableRawSource(source);
            let source_ref: &mut dyn RawTapeSource = if wrapped {
                &mut wrapped_source
            } else {
                &mut wrapped_source.0
            };
            let outcome = remanence_parity::verify_terminal_index_full_with_route(
                source_ref,
                &uuid,
                size,
                None,
                ScanMode::Standard,
                route,
                |status| fell_back |= status.fell_back,
            )
            .unwrap();
            source = wrapped_source.0;
            let io = io.finish();
            if route == remanence_parity::VerificationRoute::OnePass {
                use remanence_library::verification_io::VerificationPhase as Phase;
                assert!(io.phases[Phase::OnePassPrefix as usize].batch_read > 0);
                assert!(io.phases[Phase::Walk as usize].single_read > 0);
                assert!(io.phases[Phase::ParityMap as usize].bytes > 0);
            }
            assert!(!fell_back);
            assert!(outcome.is_complete_tape());
            let a = vector
                .written
                .map
                .entries()
                .iter()
                .find(|e| e.kind == TapeFileKind::TapeIndexReplica)
                .unwrap();
            let end = vector.image.files[a.tape_file_number as usize].start_record as u64;
            if route == remanence_parity::VerificationRoute::OnePass {
                assert_eq!(source.eod, 1, "one tail discovery");
                assert_eq!(
                    source.locates.iter().filter(|&&p| p == 0).count(),
                    1,
                    "one BOT locate"
                );
                let bot = source.locates.iter().position(|&p| p == 0).unwrap();
                assert!(
                    source.locates[bot + 1..].is_empty(),
                    "no prefix or tail LOCATE after BOT: {:?}",
                    source.locates
                );
                assert!(
                    source.spaces.iter().all(|p| p.is_some_and(|p| p >= end)),
                    "no SPACE in prefix"
                );
                assert_eq!(
                    source.positions, a.tape_file_number as usize,
                    "one explicit report per filemark"
                );
                for entry in vector
                    .written
                    .map
                    .entries()
                    .iter()
                    .take(a.tape_file_number as usize)
                {
                    if !matches!(
                        entry.kind,
                        TapeFileKind::Object | TapeFileKind::ParitySidecar
                    ) {
                        continue;
                    }
                    let first =
                        vector.image.files[entry.tape_file_number as usize].start_record as u64;
                    for lba in first..first + entry.block_count {
                        assert_eq!(source.reads[&lba], 1, "block {lba} read once");
                    }
                }
                // The final map is read once before BOT and once in the physical prefix.
                let final_map = vector
                    .written
                    .map
                    .entries()
                    .iter()
                    .rev()
                    .find(|entry| entry.kind == TapeFileKind::ParityMap)
                    .unwrap();
                let start =
                    vector.image.files[final_map.tape_file_number as usize].start_record as u64;
                for lba in start..start + final_map.block_count {
                    assert_eq!(source.reads[&lba], 2, "final map block {lba}");
                }
                // LOCATE itself proves position once; subsequent reports are exactly filemarks.
                let commands = log.borrow();
                let bot = commands
                    .iter()
                    .position(|c| c[0] == 0x92 && c[4..12] == [0; 8])
                    .unwrap();
                assert_eq!(
                    commands[bot + 1..].iter().filter(|c| c[0] == 0x34).count(),
                    a.tape_file_number as usize + 1
                );
            }
            let commands = log.borrow();
            let count = |opcode| commands.iter().filter(|c| c[0] == opcode).count();
            println!(
            "{} wrapped={wrapped} full verification: total={} LOCATE={} SPACE={} READ_POSITION={} READ={}",
            route.name(),
            commands.len(),
            count(0x92),
            count(0x11) + count(0x91),
            count(0x34),
            count(0x08)
        );
            totals.push((
                commands.len(),
                count(0x92),
                count(0x11) + count(0x91),
                count(0x34),
                count(0x08),
            ));
            outcomes.push(outcome);
        }
    }
    assert!(outcomes.iter().all(|outcome| *outcome == outcomes[0]));
    assert_eq!(
        totals[..2],
        totals[2..],
        "cancellation preserves command counts"
    );
    let (mut drive, _) = source(vector, &json!({"unreadable_records": []}));
    let mut partial = Counted {
        inner: DriveHandleRawSource::new(&mut drive),
        cursor: None,
        locates: vec![],
        spaces: vec![],
        positions: 0,
        reads: BTreeMap::new(),
        eod: 0,
    };
    let outcome =
        remanence_parity::terminal_inventory::verify_terminal_index_one_pass_budget_for_test(
            &mut partial,
            &uuid,
            size,
            500,
        )
        .unwrap();
    assert_eq!(outcome, outcomes[0]);
    let data_reads: Vec<_> = vector
        .written
        .map
        .entries()
        .iter()
        .filter(|entry| entry.kind == TapeFileKind::Object)
        .flat_map(|entry| {
            let start = vector.image.files[entry.tape_file_number as usize].start_record as u64;
            (start..start + entry.block_count)
                .map(|lba| partial.reads[&lba])
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(data_reads.contains(&1), "partial budget retains some runs");
    assert!(
        data_reads.iter().any(|&n| n > 1),
        "partial budget rereads missing runs"
    );
    println!(
        "partial budget: retained={} reread={}",
        data_reads.iter().filter(|&&n| n == 1).count(),
        data_reads.iter().filter(|&&n| n > 1).count()
    );
}

/// Protected findings remain complete outcomes; missing observations reread under a small budget.
#[test]
fn one_pass_protected_findings_and_budget() {
    let mut success = 0;
    let mut prefix = 0;
    let mut direct_walk = 0;
    let mut fallback = 0;
    let mut reasons = BTreeMap::new();
    for case in [
        "healthy",
        "crc",
        "object_read_failure",
        "sidecar_reidentified",
        "small_budget",
    ] {
        let mut vector = generate("a4-minimal").unwrap();
        let faults = match case {
            "object_read_failure" => json!({"unreadable_records": [{"lba": 4}]}),
            "sidecar_reidentified" => {
                json!({"unreadable_records": [{"lba": 7}, {"lba": 12}, {"lba": 13}]})
            }
            _ => json!({"unreadable_records": []}),
        };
        if case == "crc" {
            vector.image.files[1].bytes[2 * BLOCK as usize + 256] ^= 1;
        }
        let (mut before, _) = source(&vector, &faults);
        let (mut after, _) = source(&vector, &faults);
        let uuid = vector.written.inputs.tape_uuid;
        let expected =
            verify_terminal_index_full(&mut DriveHandleRawSource::new(&mut before), &uuid, BLOCK)
                .unwrap();
        remanence_parity::scan::take_forward_file_io_for_test();
        let io = after.begin_verification_io();
        let mut fell_back = false;
        let mut direct = false;
        let actual = if case == "small_budget" {
            remanence_parity::terminal_inventory::verify_terminal_index_one_pass_budget_for_test(
                &mut DriveHandleRawSource::new(&mut after),
                &uuid,
                BLOCK,
                0,
            )
            .unwrap()
        } else {
            remanence_parity::verify_terminal_index_full_with_route(
                &mut DriveHandleRawSource::new(&mut after),
                &uuid,
                BLOCK,
                None,
                ScanMode::Standard,
                remanence_parity::VerificationRoute::OnePass,
                |status| {
                    fell_back |= status.fell_back;
                    direct |= status.fallback_reason == Some("no_parity");
                    if let Some(reason) = status.fallback_reason {
                        *reasons.entry(reason).or_insert(0usize) += 1;
                    }
                },
            )
            .unwrap()
        };
        let files = remanence_parity::scan::take_forward_file_io_for_test();
        if case == "object_read_failure" {
            assert!(
                files
                    .iter()
                    .any(|file| file.head_position.lba == 2 && file.fallbacks == 1),
                "per-file spacing runs inside successful one-pass verification"
            );
        }
        assert_eq!(actual, expected, "{case}");
        assert_eq!(fell_back, case == "sidecar_reidentified", "{case}");
        if case == "crc" {
            assert_eq!(actual.protected().failed_data_blocks().count(), 1);
        }
        let io = io.finish();
        if case == "small_budget" {
            assert_eq!(io.total.algorithmic_fallback_epochs, 1);
            assert!(
                io.phases
                    [remanence_library::verification_io::VerificationPhase::FallbackData as usize]
                    .bytes
                    > 0
            );
        }
        prefix += usize::from(
            io.phases
                [remanence_library::verification_io::VerificationPhase::OnePassPrefix as usize]
                .batch_read
                > 0,
        );
        direct_walk += usize::from(direct);
        if fell_back {
            fallback += 1
        } else if !direct {
            success += 1
        }
    }
    assert_eq!((prefix, success, fallback, direct_walk), (5, 4, 1, 0));
    println!("protected fixtures: prefix={prefix}, success={success}, rejected={fallback}, direct_walk={direct_walk}, reasons={reasons:?}");
}

/// Every encoded tape image runs both routes, including images without a complete tail.
#[test]
fn one_pass_all_tape_images() {
    let mut success = 0;
    let mut prefix = 0;
    let mut direct_walk = 0;
    let mut fallback = 0;
    let mut reasons = BTreeMap::new();
    for name in crate::tape_image_vectors::IMAGE_NAMES {
        let vector = generate(name).unwrap();
        let (mut before, _) = source(&vector, &json!({"unreadable_records": []}));
        let (mut after, _) = source(&vector, &json!({"unreadable_records": []}));
        let uuid = vector.written.inputs.tape_uuid;
        let walk =
            verify_terminal_index_full(&mut DriveHandleRawSource::new(&mut before), &uuid, BLOCK);
        let io = after.begin_verification_io();
        let mut fell_back = false;
        let mut direct = false;
        let observed = remanence_parity::verify_terminal_index_full_with_route(
            &mut DriveHandleRawSource::new(&mut after),
            &uuid,
            BLOCK,
            None,
            ScanMode::Standard,
            remanence_parity::VerificationRoute::OnePass,
            |status| {
                fell_back |= status.fell_back;
                direct |= status.fallback_reason == Some("no_parity");
                if let Some(reason) = status.fallback_reason {
                    *reasons.entry(reason).or_insert(0usize) += 1;
                }
            },
        );
        assert_eq!(format!("{walk:?}"), format!("{observed:?}"), "{name}");
        if walk
            .as_ref()
            .is_ok_and(|outcome| outcome.is_complete_tape())
        {
            assert!(!fell_back, "{name}");
        }
        let io = io.finish();
        prefix += usize::from(
            io.phases
                [remanence_library::verification_io::VerificationPhase::OnePassPrefix as usize]
                .batch_read
                > 0,
        );
        direct_walk += usize::from(direct);
        if fell_back {
            fallback += 1
        } else if !direct {
            success += 1
        }
    }
    assert_eq!((prefix, success, fallback, direct_walk), (3, 3, 3, 0));
    println!("tape images: prefix={prefix}, success={success}, rejected={fallback}, direct_walk={direct_walk}, reasons={reasons:?}");
}

/// Count a large Object re-read after its prefix observations are retracted.
fn compare_retracted(vector: &VectorImage) {
    let uuid = vector.written.inputs.tape_uuid;
    let size = vector.written.inputs.block_size;
    let mut outcomes = Vec::new();
    for route in [
        remanence_parity::VerificationRoute::Walk,
        remanence_parity::VerificationRoute::OnePass,
    ] {
        let (mut drive, _, log) =
            source_with_command_log(vector, &json!({"unreadable_records": [{"lba": 4}]}));
        let io = drive.begin_verification_io();
        let mut status = None;
        let result = remanence_parity::verify_terminal_index_full_with_route(
            &mut DriveHandleRawSource::new(&mut drive),
            &uuid,
            size,
            None,
            ScanMode::Standard,
            route,
            |s| status = Some(s),
        )
        .unwrap();
        assert!(!status.unwrap().fell_back);
        let io = io.finish();
        let fallback = &io.phases
            [remanence_library::verification_io::VerificationPhase::FallbackData as usize];
        let commands = log.borrow();
        let count = |opcode| commands.iter().filter(|c| c[0] == opcode).count();
        println!("retracted {}: total={} LOCATE={} SPACE={} READ_POSITION={} READ={} fallback_single={} fallback_batch={} fallback_bytes={}",
            route.name(), commands.len(), count(0x92), count(0x11) + count(0x91), count(0x34), count(0x08),
            fallback.single_read, fallback.batch_read, fallback.bytes);
        if route == remanence_parity::VerificationRoute::OnePass {
            // The persistent fault uses the normal single-read recovery for its epoch;
            // the other retracted epoch resumes batched reads over the missing run.
            assert_eq!(fallback.single_read, 32);
            assert_eq!(fallback.batch_read, 3);
            assert_eq!(commands.len(), 206);
            assert_eq!(count(0x08), 85);
        }
        outcomes.push(result);
    }
    assert_eq!(outcomes[0], outcomes[1]);
}
