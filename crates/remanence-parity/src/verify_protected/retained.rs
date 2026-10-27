//! One forward sidecar read supplies the Recoverer's acquisition.
//! Metadata uses single-record reads with one medium-error retry. Shards use
//! strict batches and retain only CRCs or failures. Replay never issues I/O for
//! a retained address; missing addresses always reach the positioned live source.

use super::*;
use crate::raw::{
    read_records_default, RawReadBatch, RawReadOutcome, ReadRun, SpaceFilemarksOutcome,
};
use remanence_library::{ScsiError, TapeIoError};
use std::collections::BTreeMap;

const METADATA_BUDGET: usize = 64 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_METADATA_BUDGET: std::cell::Cell<usize> = const { std::cell::Cell::new(METADATA_BUDGET) };
}

/// Scope a retention limit to one test thread and restore it on unwind.
#[cfg(test)]
pub(super) fn with_metadata_budget<T>(budget: usize, run: impl FnOnce() -> T) -> T {
    struct RestoreBudget(usize);
    impl Drop for RestoreBudget {
        fn drop(&mut self) {
            TEST_METADATA_BUDGET.set(self.0);
        }
    }
    let _restore = RestoreBudget(TEST_METADATA_BUDGET.replace(budget));
    run()
}

/// Raw classifications retain their reported positions and measured lengths.
/// Medium sense is copied because the enclosing I/O error is not cloneable.
enum Record {
    Read(RawReadOutcome, Box<[u8]>),
    Medium { sense: Vec<u8>, transferred: u32 },
    TooSmall { actual: u32, provided: u32 },
}

struct RetainedRecord {
    record: Record,
    after: Option<PhysicalPositionHint>,
}

impl RetainedRecord {
    fn replay(&self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        match &self.record {
            Record::Read(outcome, bytes) => {
                let copied = bytes.len().min(buf.len());
                buf[..copied].copy_from_slice(&bytes[..copied]);
                if bytes.len() > buf.len() {
                    return Err(TapeIoError::ReadBufferTooSmall {
                        actual: bytes.len() as u32,
                        provided: buf.len() as u32,
                    }
                    .into());
                }
                Ok(*outcome)
            }
            Record::Medium { sense, transferred } => {
                Err(TapeIoError::CheckCondition(ScsiError::CheckCondition {
                    sense: sense.clone(),
                    bytes_transferred: *transferred,
                })
                .into())
            }
            Record::TooSmall { actual, provided } => Err(TapeIoError::ReadBufferTooSmall {
                actual: *actual,
                provided: *provided,
            }
            .into()),
        }
    }
}

pub(super) struct RetainedSource<'a> {
    source: &'a mut dyn RawTapeSource,
    block_size: u32,
    cursor: Option<PhysicalPositionHint>,
    live_next: Option<PhysicalPositionHint>,
    metadata: BTreeMap<(u32, u64), RetainedRecord>,
    pub(super) shards: BTreeMap<u64, DataObservation>,
    recording: bool,
    enabled: bool,
    used: usize,
    budget: usize,
    pub(super) positioned: bool,
    observed_extent: Option<(u32, u64, u64)>,
    fallback_counted: bool,
}

impl<'a> RetainedSource<'a> {
    pub(super) fn new(source: &'a mut dyn RawTapeSource, block_size: u32) -> Self {
        Self {
            source,
            block_size,
            cursor: None,
            live_next: None,
            metadata: BTreeMap::new(),
            shards: BTreeMap::new(),
            recording: false,
            enabled: false,
            used: 0,
            positioned: false,
            observed_extent: None,
            fallback_counted: false,
            #[cfg(not(test))]
            budget: METADATA_BUDGET,
            #[cfg(test)]
            budget: TEST_METADATA_BUDGET.get(),
        }
    }

    /// Replay the file reader's bounded windows through the existing acquisition source.
    pub(super) fn from_file(
        source: &'a mut dyn RawTapeSource,
        block_size: u32,
        observation: &crate::scan::forward_file::FileObservation,
    ) -> Self {
        let mut retained = Self::new(source, block_size);
        retained.enabled = true;
        retained.observed_extent = Some((
            observation.head_position.partition,
            observation.head_position.lba,
            observation
                .head_position
                .lba
                .saturating_add(observation.whole_blocks),
        ));
        for offset in observation.retained_offsets() {
            if let Some(bytes) = observation.retained(offset) {
                let position = PhysicalPositionHint {
                    lba: observation.head_position.lba + offset,
                    ..observation.head_position
                };
                let after = PhysicalPositionHint {
                    lba: position.lba + 1,
                    ..position
                };
                retained.metadata.insert(
                    (position.partition, position.lba),
                    RetainedRecord {
                        record: Record::Read(
                            RawReadOutcome::Block {
                                bytes: bytes.len(),
                                position_after: after,
                            },
                            bytes.into(),
                        ),
                        after: Some(after),
                    },
                );
            }
        }
        retained
    }

    /// Segment boundaries keep metadata out of hardware batches, so its second
    /// attempt is always explicit and never duplicates batch-to-single fallback.
    pub(super) fn read_sidecar(
        &mut self,
        map: &ScopedFilemarkMap,
        sidecar: &TapeFileMapEntry,
        scheme: &ParityScheme,
    ) -> Result<(), ParityError> {
        let Some((h, tail)) =
            sidecar_geometry_from_total(sidecar.block_count, scheme_parity_blocks(scheme))
        else {
            return Ok(());
        };
        self.enabled = true;
        let first = map
            .map
            .physical_run(sidecar.tape_file_number, 0, sidecar.block_count)?;
        let block_size = self.block_size;
        let mut next = None;
        for (start, end, metadata) in [
            (0, h, true),
            (h, tail, false),
            (tail, sidecar.block_count, true),
        ] {
            self.recording = metadata;
            let mut run = ReadRun::new(self, block_size, end - start);
            run.continue_at(next);
            let mut reader = SequentialReader::new(&mut run, block_size);
            let mut shards = BTreeMap::new();
            let mut boundary = false;
            for offset in start..end {
                let physical = PhysicalPositionHint {
                    lba: first.lba + offset,
                    ..first
                };
                let probe = reader.read(physical)?;
                boundary = matches!(probe, Probe::Boundary(_));
                if !metadata {
                    shards.insert(offset, DataObservation::from_parity_probe(probe));
                }
                if boundary {
                    break;
                }
            }
            next = reader.next;
            self.shards.extend(shards);
            if boundary {
                break;
            }
        }
        self.recording = false;
        self.cursor = None;
        Ok(())
    }

    /// Charge a live read within this observed file once; data rereads count at their epoch.
    fn count_missing(&mut self, position: PhysicalPositionHint) {
        if !self.fallback_counted
            && self.observed_extent.is_some_and(|(partition, start, end)| {
                position.partition == partition && (start..end).contains(&position.lba)
            })
        {
            self.source.verification_context().algorithmic_fallback();
            self.fallback_counted = true;
        }
    }

    fn position_live(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let cursor = self
            .cursor
            .ok_or(ParityError::Invariant("retained read requires a position"))?;
        if self.live_next != Some(cursor) {
            self.live_next = None;
            self.positioned = true;
            self.source.locate_physical(cursor)?;
            self.live_next = Some(cursor);
        }
        Ok(cursor)
    }

    fn accept_live(&mut self, result: &Result<RawReadOutcome, ParityError>) {
        self.cursor = result.as_ref().ok().map(|r| r.position_after());
        self.live_next = match result {
            Ok(RawReadOutcome::Block {
                bytes,
                position_after,
            }) if *bytes == self.block_size as usize => Some(*position_after),
            _ => None,
        };
    }
}

impl RawTapeSource for RetainedSource<'_> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.source.verification_context()
    }

    fn position_report(&mut self) -> Result<crate::raw::RawPositionReport, ParityError> {
        if !self.enabled {
            return self.source.position_report();
        }
        Err(ParityError::Invariant(
            "retained source cannot report physical measurement",
        ))
    }

    fn read_forward_records(
        &mut self,
        buf: &mut [u8],
        size: u32,
        count: u32,
    ) -> crate::raw::RawForwardReadBatch {
        if !self.enabled {
            return self.source.read_forward_records(buf, size, count);
        }
        crate::raw::RawForwardReadBatch {
            batch: RawReadBatch {
                records_read: 0,
                position_after: None,
                end: crate::raw::RawReadEnd::Error(ParityError::Invariant(
                    "retained source cannot measure physical files",
                )),
            },
            unconfirmed_boundary: false,
        }
    }

    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
        self.source.configure_fixed_block_size(size)
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        if !self.enabled {
            return self.source.locate_physical(hint);
        }
        if !self.metadata.contains_key(&(hint.partition, hint.lba)) {
            self.cursor = None;
            self.live_next = None;
            self.count_missing(hint);
            self.positioned = true;
            self.source.locate_physical(hint)?;
            self.live_next = Some(hint);
        }
        self.cursor = Some(hint);
        Ok(())
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        if !self.enabled {
            return self.source.position();
        }
        match self.cursor {
            Some(cursor) => Ok(cursor),
            None => self.source.position(),
        }
    }

    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.cursor = None;
        self.live_next = None;
        let after = self.source.locate_end_of_data()?;
        self.cursor = Some(after);
        Ok(after)
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        if !self.enabled {
            return self.source.space_filemarks(count);
        }
        self.position_live()?;
        self.cursor = None;
        self.live_next = None;
        let outcome = self.source.space_filemarks(count)?;
        self.cursor = Some(outcome.position_after);
        Ok(outcome)
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        if !self.enabled {
            return self.source.read_record(buf);
        }
        let cursor = self
            .cursor
            .ok_or(ParityError::Invariant("retained read requires a position"))?;
        if let Some(record) = self.metadata.get(&(cursor.partition, cursor.lba)) {
            let result = record.replay(buf);
            self.cursor = if result.is_ok() { record.after } else { None };
            return result;
        }
        self.position_live()?;
        let mut result = self.source.read_record(buf);
        self.accept_live(&result);
        if self.recording
            && matches!(&result, Err(ParityError::TapeIo(error)) if tape_error_is_current_medium_damage(error))
        {
            self.positioned = true;
            self.source.locate_physical(cursor)?;
            result = self.source.read_record(buf);
            self.accept_live(&result);
        }
        if self.recording && self.block_size as usize <= self.budget - self.used {
            let record = match &result {
                Ok(outcome) => {
                    let bytes = match outcome {
                        RawReadOutcome::Block { bytes, .. } => &buf[..(*bytes).min(buf.len())],
                        _ => &[],
                    };
                    Some(Record::Read(*outcome, bytes.into()))
                }
                Err(ParityError::TapeIo(error)) if tape_error_is_current_medium_damage(error) => {
                    let (sense, transferred) =
                        error.check_condition_transfer().expect("medium sense");
                    Some(Record::Medium {
                        sense: sense.to_vec(),
                        transferred,
                    })
                }
                Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall { actual, provided })) => {
                    Some(Record::TooSmall {
                        actual: *actual,
                        provided: *provided,
                    })
                }
                _ => None,
            };
            if let Some(record) = record {
                let after = self.cursor;
                self.used += self.block_size as usize;
                self.metadata.insert(
                    (cursor.partition, cursor.lba),
                    RetainedRecord { record, after },
                );
            }
        }
        result
    }

    fn read_records(
        &mut self,
        buf: &mut [u8],
        size: u32,
        max: u32,
        remaining: u32,
    ) -> RawReadBatch {
        if !self.enabled {
            return self.source.read_records(buf, size, max, remaining);
        }
        let cached = self.cursor.is_some_and(|p| {
            self.metadata
                .range(
                    (p.partition, p.lba)
                        ..=(
                            p.partition,
                            p.lba
                                .saturating_add(u64::from(max.min(remaining)).saturating_sub(1)),
                        ),
                )
                .next()
                .is_some()
        });
        if self.recording || cached {
            return read_records_default(self, buf, size, max, remaining);
        }
        if let Err(error) = self.position_live() {
            return RawReadBatch {
                records_read: 0,
                position_after: None,
                end: crate::raw::RawReadEnd::Error(error),
            };
        }
        let batch = self.source.read_records(buf, size, max, remaining);
        self.live_next = if matches!(batch.end, crate::raw::RawReadEnd::Complete) {
            batch.position_after
        } else {
            None
        };
        self.cursor = self.live_next;
        batch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filemark_map::FilemarkMap;

    #[derive(Clone, Copy)]
    enum ReadFault {
        Medium,
        TooSmall,
    }

    /// A lazy fixed-record source measures retained storage without allocating
    /// a second copy of a large synthetic tape.
    struct ZeroSource {
        cursor: PhysicalPositionHint,
        reads: Vec<u64>,
        locates: Vec<u64>,
        positions: usize,
        fault: Option<ReadFault>,
    }

    impl RawTapeSource for ZeroSource {
        fn configure_fixed_block_size(&mut self, _: u32) -> Result<(), ParityError> {
            Ok(())
        }
        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.locates.push(hint.lba);
            self.cursor = hint;
            Ok(())
        }
        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            panic!("no EOD command")
        }
        fn space_filemarks(&mut self, _: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            panic!("no SPACE command")
        }
        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.positions += 1;
            Ok(self.cursor)
        }
        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            self.reads.push(self.cursor.lba);
            buf.fill(0);
            self.cursor.lba += 1;
            if let Some(fault) = self.fault {
                return Err(match fault {
                    ReadFault::Medium => TapeIoError::CheckCondition(ScsiError::CheckCondition {
                        sense: vec![0x72, 3, 0x11, 0],
                        bytes_transferred: 0,
                    }),
                    ReadFault::TooSmall => TapeIoError::ReadBufferTooSmall {
                        actual: buf.len() as u32 + 1,
                        provided: buf.len() as u32,
                    },
                }
                .into());
            }
            Ok(RawReadOutcome::Block {
                bytes: buf.len(),
                position_after: self.cursor,
            })
        }
    }

    /// Recording and replay preserve an error's unknown position without I/O.
    #[test]
    fn recording_and_replaying_read_errors_issue_no_read_position() {
        for fault in [ReadFault::Medium, ReadFault::TooSmall] {
            let first = PhysicalPositionHint {
                partition: 0,
                lba: 7,
            };
            let mut source = ZeroSource {
                cursor: first,
                reads: vec![],
                locates: vec![],
                positions: 0,
                fault: Some(fault),
            };
            let mut retained = RetainedSource::new(&mut source, 4096);
            retained.enabled = true;
            retained.recording = true;
            retained.locate_physical(first).unwrap();
            let mut buf = vec![0; 4096];
            let live = retained.read_record(&mut buf).unwrap_err();
            assert_eq!(retained.cursor, None);
            assert_eq!(retained.metadata[&(first.partition, first.lba)].after, None);
            retained.recording = false;
            retained.locate_physical(first).unwrap();
            let replay = retained.read_record(&mut buf).unwrap_err();
            assert_eq!(replay.to_string(), live.to_string());
            assert_eq!(retained.cursor, None);
            drop(retained);
            assert_eq!(source.positions, 0);
            let attempts = if matches!(fault, ReadFault::Medium) {
                2
            } else {
                1
            };
            assert_eq!(source.reads, vec![first.lba; attempts]);
            assert_eq!(source.locates, vec![first.lba; attempts]);
        }
    }

    #[test]
    fn metadata_bound_replay_and_missing_position_reads() {
        let size = 256 * 1024;
        let scheme = ParityScheme {
            id: crate::SchemeId::new_static("rs-cauchy-gf256-v1"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 2,
        };
        for h in [3, 129] {
            let total = 2 * h + 3;
            let sidecar = TapeFileMapEntry::parity_sidecar(1, total, 0, 0, 4);
            let map = ScopedFilemarkMap::from_catalog(
                FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1), sidecar.clone()]).unwrap(),
                4,
            );
            let first = map.map.physical_run(1, 0, total).unwrap();
            let mut source = ZeroSource {
                cursor: first,
                reads: vec![],
                locates: vec![],
                positions: 0,
                fault: None,
            };
            let mut retained = RetainedSource::new(&mut source, size);
            retained.read_sidecar(&map, &sidecar, &scheme).unwrap();
            let bytes: usize = retained
                .metadata
                .values()
                .map(|r| match &r.record {
                    Record::Read(_, bytes) => bytes.len(),
                    _ => 0,
                })
                .sum();
            assert_eq!(bytes, retained.used);
            assert_eq!(
                bytes,
                ((2 * h + 1) as usize * size as usize).min(METADATA_BUDGET)
            );
            assert_eq!(retained.shards.len(), 2);
            let mut buf = vec![0; size as usize];
            for _ in 0..2 {
                retained.locate_physical(first).unwrap();
                assert!(matches!(
                    read_fixed_record(&mut retained, &mut buf).unwrap(),
                    FixedRecordRead::Block { .. }
                ));
                assert_eq!(retained.position().unwrap().lba, first.lba + 1);
            }
            let footer = PhysicalPositionHint {
                lba: first.lba + total - 1,
                ..first
            };
            retained.locate_physical(footer).unwrap();
            read_fixed_record(&mut retained, &mut buf).unwrap();
            drop(retained);
            assert_eq!(
                source.reads.iter().filter(|&&p| p == first.lba).count(),
                1,
                "replay has no device reads"
            );
            assert_eq!(
                source.reads.iter().filter(|&&p| p == footer.lba).count(),
                if h == 3 { 1 } else { 2 }
            );
            assert_eq!(
                source.locates,
                if h == 3 {
                    vec![first.lba]
                } else {
                    vec![first.lba, footer.lba]
                }
            );
        }
    }
}
