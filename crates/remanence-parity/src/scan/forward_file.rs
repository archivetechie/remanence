//! Read one physical file through its first filemark and retain classification
//! windows. A suspect measurement uses the walk from the original file head.

use super::*;
use crate::raw::{device_position_after, RawPositionReport, RawReadEnd};
use remanence_library::TapeIoError;

/// Evidence that establishes the head before any read of this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadProvenance {
    Locate,
    DeviceReport,
}

/// Why the read-derived measurement cannot stand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpacingReason {
    NotFullyVerified,
    Unreadable,
    WrongLength,
    UncleanRead,
    EndOfData,
    PositionMismatch,
    PositionUnknown,
    FileIdentifierMismatch,
}

/// The first read's classification, independent of all subsequent reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeadOutcome {
    Block,
    WrongLength,
    Unreadable,
    Filemark,
    EndOfData,
}

/// Boundary or failed read that stops the forward observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileReadEnd {
    Filemark,
    EndOfData,
    Unreadable,
    WrongLength,
    UncleanRead,
}

/// Owned evidence for one file. Window storage is flat and reused for each block.
#[derive(Debug)]
pub(crate) struct FileObservation {
    pub(crate) tape_file_number: u64,
    pub(crate) head_position: PhysicalPositionHint,
    // Provenance remains available to the per-file diagnostic assertions.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) head_provenance: HeadProvenance,
    pub(crate) head_outcome: HeadOutcome,
    pub(crate) head_read: Option<Result<RawReadOutcome, ParityError>>,
    pub(crate) whole_blocks: u64,
    pub(crate) head_block: Vec<u8>,
    pub(crate) last_block: Vec<u8>,
    pub(crate) boundary: FileReadEnd,
    pub(crate) report_after_filemark: Option<RawPositionReport>,
    pub(crate) requires_spacing: Option<SpacingReason>,
    first: Vec<u8>,
    tail: Vec<u8>,
    block_size: usize,
}

impl FileObservation {
    /// Measure with the walk whenever the sequential read lacks proof.
    pub(super) fn measure(
        &self,
        source: &mut dyn RawTapeSource,
    ) -> Result<MeasureCurrentFileOutcome, ParityError> {
        match self.head_outcome {
            HeadOutcome::Filemark => {
                return Ok(MeasureCurrentFileOutcome::Truncated(
                    ScanTailTruncationKind::EmptyFile,
                ))
            }
            HeadOutcome::EndOfData => return Ok(MeasureCurrentFileOutcome::EndOfData),
            _ => {}
        }
        if self.requires_spacing.is_some() {
            #[cfg(any(test, feature = "test-support"))]
            test_support::record_fallback(self.head_position);
            source.locate_physical(self.head_position)?;
            return measure_current_file(source, self.head_position);
        }
        let report = self.report_after_filemark.ok_or(ParityError::Invariant(
            "file observation has no boundary report",
        ))?;
        Ok(MeasureCurrentFileOutcome::Complete(MeasuredTapeFile {
            block_count: self.whole_blocks,
            position_after: report.position,
        }))
    }

    /// A rejected first command cannot classify the head from its stream flags.
    /// Use the walk's single-record read, whose result is final, including ILI.
    fn read_head_after_locate(
        &mut self,
        source: &mut dyn RawTapeSource,
        on_block: &mut BlockObserver<'_>,
    ) -> Result<(), ParityError> {
        source.locate_physical(self.head_position)?;
        let read = source.read_record(&mut self.head_block);
        self.head_outcome = match &read {
            Ok(RawReadOutcome::Block { bytes, .. }) if *bytes == self.block_size => {
                HeadOutcome::Block
            }
            Ok(RawReadOutcome::Block { .. }) => HeadOutcome::WrongLength,
            Ok(RawReadOutcome::Filemark { .. }) => HeadOutcome::Filemark,
            Ok(RawReadOutcome::EndOfData { .. }) => HeadOutcome::EndOfData,
            Err(e) if scan_read_error_is_medium_damage(e) => HeadOutcome::Unreadable,
            Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall { .. })) => {
                HeadOutcome::WrongLength
            }
            Err(_) => return Err(read.unwrap_err()),
        };
        if self.head_outcome == HeadOutcome::Block {
            on_block(self.head_position, &self.head_block)?;
            self.whole_blocks = 1;
            self.last_block.copy_from_slice(&self.head_block);
            if !self.first.is_empty() {
                self.first[..self.block_size].copy_from_slice(&self.head_block);
            }
            if !self.tail.is_empty() {
                self.tail[..self.block_size].copy_from_slice(&self.head_block);
            }
        }
        self.head_read = Some(read);
        Ok(())
    }

    /// Iterate the bounded retained windows without visiting the file's interior.
    pub(crate) fn retained_offsets(&self) -> impl Iterator<Item = u64> {
        let first = (self.first.len() / self.block_size).max(1) as u64;
        let tail = (self.tail.len() / self.block_size).max(1) as u64;
        (0..first.min(self.whole_blocks))
            .chain(self.whole_blocks.saturating_sub(tail).max(first)..self.whole_blocks)
    }

    pub(crate) fn retained(&self, offset: u64) -> Option<&[u8]> {
        if self.requires_spacing.is_some() || offset >= self.whole_blocks {
            return None;
        }
        if offset == 0 {
            return Some(&self.head_block);
        }
        if offset == self.whole_blocks - 1 {
            return Some(&self.last_block);
        }
        let first_count = self.first.len() / self.block_size;
        if offset < first_count as u64 {
            let start = offset as usize * self.block_size;
            return Some(&self.first[start..start + self.block_size]);
        }
        let tail_count = self.tail.len() / self.block_size;
        if tail_count > 0 && offset >= self.whole_blocks.saturating_sub(tail_count as u64) {
            let start = (offset % tail_count as u64) as usize * self.block_size;
            return Some(&self.tail[start..start + self.block_size]);
        }
        None
    }
}

/// Borrow each whole block for one callback invocation.
pub(crate) type BlockObserver<'a> =
    dyn FnMut(PhysicalPositionHint, &[u8]) -> Result<(), ParityError> + 'a;

/// Reusable batch buffer; no per-block allocation occurs while streaming.
pub(crate) struct ForwardFileReader {
    pub(crate) read_through: bool,
    block_size: u32,
    batch_blocks: u32,
    buffer: Vec<u8>,
}

impl ForwardFileReader {
    pub(crate) fn new(block_size: u32, batch_blocks: u32) -> Result<Self, ParityError> {
        if block_size == 0 || batch_blocks == 0 {
            return Err(ParityError::Invariant("zero forward read geometry"));
        }
        let len = (block_size as usize)
            .checked_mul(batch_blocks as usize)
            .ok_or(ParityError::Invariant("forward batch size overflows"))?;
        Ok(Self {
            read_through: true,
            block_size,
            batch_blocks,
            buffer: vec![0; len],
        })
    }

    /// Borrow the source for this file only. Callback bytes live only for the call.
    /// First/tail counts select retention, never the file's length or boundary.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn read_file(
        &mut self,
        source: &mut dyn RawTapeSource,
        tape_file_number: u64,
        head_position: PhysicalPositionHint,
        head_provenance: HeadProvenance,
        first_blocks: usize,
        tail_blocks: usize,
        on_block: &mut BlockObserver<'_>,
    ) -> Result<FileObservation, ParityError> {
        #[cfg(any(test, feature = "test-support"))]
        test_support::record_head(head_position, head_provenance);
        let size = self.block_size as usize;
        let window_bytes = |count: usize| {
            count
                .checked_mul(size)
                .ok_or(ParityError::Invariant("retained window size overflows"))
        };
        let mut observation = FileObservation {
            tape_file_number,
            head_position,
            head_provenance,
            head_outcome: HeadOutcome::EndOfData,
            head_read: None,
            whole_blocks: 0,
            head_block: vec![0; size],
            last_block: vec![0; size],
            first: vec![0; window_bytes(first_blocks)?],
            tail: vec![0; window_bytes(tail_blocks)?],
            block_size: size,
            boundary: FileReadEnd::EndOfData,
            report_after_filemark: None,
            requires_spacing: None,
        };
        loop {
            // The head completes alone so a later damaged record cannot alter it.
            let requested = if observation.head_read.is_none() {
                1
            } else {
                self.batch_blocks
            };
            let forward = source.read_forward_records(&mut self.buffer, self.block_size, requested);
            let batch = forward.batch;
            if forward.unconfirmed_boundary && observation.head_read.is_none() {
                observation.requires_spacing = Some(SpacingReason::UncleanRead);
                observation.boundary = FileReadEnd::UncleanRead;
                observation.read_head_after_locate(source, on_block)?;
                return Ok(observation);
            }
            if batch.records_read > requested
                || (batch.records_read == 0 && matches!(batch.end, RawReadEnd::Complete))
            {
                return Err(ParityError::Invariant("invalid forward batch prefix"));
            }
            for bytes in self.buffer[..batch.records_read as usize * size].chunks_exact(size) {
                let position = device_position_after(
                    head_position,
                    observation.whole_blocks,
                    "forward block",
                )?;
                if observation.whole_blocks == 0 {
                    observation.head_block.copy_from_slice(bytes);
                    observation.head_outcome = HeadOutcome::Block;
                    observation.head_read = Some(Ok(RawReadOutcome::Block {
                        bytes: size,
                        position_after: device_position_after(position, 1, "forward head")?,
                    }));
                }
                observation.last_block.copy_from_slice(bytes);
                if observation.whole_blocks < first_blocks as u64 {
                    let start = observation.whole_blocks as usize * size;
                    observation.first[start..start + size].copy_from_slice(bytes);
                }
                if tail_blocks > 0 {
                    let start = (observation.whole_blocks % tail_blocks as u64) as usize * size;
                    observation.tail[start..start + size].copy_from_slice(bytes);
                }
                on_block(position, bytes)?;
                observation.whole_blocks = observation
                    .whole_blocks
                    .checked_add(1)
                    .ok_or_else(|| device_position_error("whole-block count overflows"))?;
            }
            if !self.read_through && matches!(batch.end, RawReadEnd::Complete) {
                observation.requires_spacing = Some(SpacingReason::NotFullyVerified);
                observation.boundary = FileReadEnd::UncleanRead;
                return Ok(observation);
            }
            match batch.end {
                RawReadEnd::Complete => continue,
                RawReadEnd::Filemark { position_after } => {
                    if observation.head_read.is_none() {
                        observation.head_outcome = HeadOutcome::Filemark;
                        observation.head_read =
                            Some(Ok(RawReadOutcome::Filemark { position_after }));
                    }
                    let report = source.position_report()?;
                    if !report.logical_object_number_unknown
                        && report.position.lba < head_position.lba
                    {
                        return Err(device_position_error("went backwards after a filemark"));
                    }
                    let expected = device_position_after(
                        head_position,
                        observation.whole_blocks,
                        "forward file",
                    )?;
                    let expected = device_position_after(expected, 1, "forward filemark")?;
                    observation.requires_spacing = if report.logical_object_number_unknown {
                        Some(SpacingReason::PositionUnknown)
                    } else if forward.unconfirmed_boundary {
                        Some(SpacingReason::UncleanRead)
                    } else if report.position != expected {
                        Some(SpacingReason::PositionMismatch)
                    } else if !report.mark_position_unknown
                        && report
                            .logical_file_identifier
                            .is_some_and(|file| tape_file_number.checked_add(1) != Some(file))
                    {
                        Some(SpacingReason::FileIdentifierMismatch)
                    } else {
                        None
                    };
                    observation.report_after_filemark = Some(report);
                    observation.boundary = FileReadEnd::Filemark;
                    return Ok(observation);
                }
                RawReadEnd::EndOfData { position_after } => {
                    if observation.head_read.is_none() {
                        observation.head_outcome = HeadOutcome::EndOfData;
                        observation.head_read =
                            Some(Ok(RawReadOutcome::EndOfData { position_after }));
                    }
                    observation.requires_spacing = Some(SpacingReason::EndOfData);
                    observation.boundary = FileReadEnd::EndOfData;
                }
                RawReadEnd::WrongLength {
                    bytes,
                    position_after,
                } => {
                    if observation.head_read.is_none() {
                        observation.head_outcome = HeadOutcome::WrongLength;
                        observation.head_read = Some(Ok(RawReadOutcome::Block {
                            bytes,
                            position_after,
                        }));
                    }
                    observation.requires_spacing = Some(SpacingReason::WrongLength);
                    observation.boundary = FileReadEnd::WrongLength;
                }
                RawReadEnd::Error(error) => {
                    let reason = if scan_read_error_is_medium_damage(&error) {
                        SpacingReason::Unreadable
                    } else if let ParityError::TapeIo(tape_error) = &error {
                        match remanence_library::verification_read_completion(
                            tape_error,
                            self.block_size,
                            requested,
                        )
                        .1
                        {
                            remanence_library::VerificationReadCompletion::WrongLength => {
                                SpacingReason::WrongLength
                            }
                            remanence_library::VerificationReadCompletion::UncleanRead
                            | remanence_library::VerificationReadCompletion::RecoveredError => {
                                SpacingReason::UncleanRead
                            }
                            _ => return Err(error),
                        }
                    } else {
                        return Err(error);
                    };
                    observation.requires_spacing = Some(reason);
                    observation.boundary = match reason {
                        SpacingReason::Unreadable => FileReadEnd::Unreadable,
                        SpacingReason::WrongLength => FileReadEnd::WrongLength,
                        _ => FileReadEnd::UncleanRead,
                    };
                    if observation.head_read.is_none()
                        && (reason == SpacingReason::Unreadable
                            || matches!(
                                &error, ParityError::TapeIo(TapeIoError::ReadBufferTooSmall { actual, .. }) if *actual as usize != size
                            ))
                    {
                        observation.head_outcome = if reason == SpacingReason::Unreadable {
                            HeadOutcome::Unreadable
                        } else {
                            HeadOutcome::WrongLength
                        };
                        observation.head_read = Some(Err(error));
                        return Ok(observation);
                    }
                    if observation.head_read.is_none() {
                        observation.read_head_after_locate(source, on_block)?;
                    }
                }
            }
            return Ok(observation);
        }
    }
}

/// Serve only ladder-named blocks; missing retained bytes require an addressed read.
pub(crate) struct ObservedClassificationSource<'a> {
    pub(crate) source: &'a mut dyn RawTapeSource,
    pub(crate) observation: &'a FileObservation,
    pub(crate) positioned: bool,
}

impl ClassificationBlockSource for ObservedClassificationSource<'_> {
    fn read_block(
        &mut self,
        file_start: PhysicalPositionHint,
        offset: u64,
        block_size: u32,
    ) -> Result<ClassificationBlockRead, ParityError> {
        if file_start != self.observation.head_position
            || block_size as usize != self.observation.block_size
        {
            return Err(ParityError::Invariant(
                "classification observation geometry differs",
            ));
        }
        if let Some(block) = self.observation.retained(offset) {
            return Ok(ClassificationBlockRead::Block(block.to_vec()));
        }
        self.positioned = true;
        PositionedClassificationSource {
            source: self.source,
        }
        .read_block(file_start, offset, block_size)
    }
}

/// Thread-local observations keep concurrent fixture comparisons independent.
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Debug)]
    pub(crate) struct FileTrace {
        pub head: PhysicalPositionHint,
        pub provenance: HeadProvenance,
        pub fallbacks: usize,
        pub positioned_reads: usize,
    }
    thread_local! {
        #[cfg(test)]
        pub(crate) static FORWARD: Cell<bool> = const { Cell::new(false) };
        static CLASSIFICATION: Cell<Option<PhysicalPositionHint>> = const { Cell::new(None) };
        pub(crate) static FILES: RefCell<Vec<FileTrace>> = const { RefCell::new(Vec::new()) };
    }
    pub(crate) fn record_head(head: PhysicalPositionHint, provenance: HeadProvenance) {
        FILES.with_borrow_mut(|files| {
            files.push(FileTrace {
                head,
                provenance,
                fallbacks: 0,
                positioned_reads: 0,
            })
        });
    }
    pub(crate) fn record_fallback(head: PhysicalPositionHint) {
        FILES.with_borrow_mut(|files| {
            let file = files.last_mut().expect("observed file");
            assert_eq!(file.head, head);
            file.fallbacks += 1;
        });
    }
    pub(crate) fn record_classification() {
        let Some(head) = CLASSIFICATION.get() else {
            return;
        };
        FILES.with_borrow_mut(|files| {
            let file = files.last_mut().expect("observed file");
            assert_eq!(file.head, head);
            file.positioned_reads += 1;
        });
    }
    pub(crate) struct ClassificationScope(Option<PhysicalPositionHint>);
    impl ClassificationScope {
        pub(crate) fn new(head: PhysicalPositionHint) -> Self {
            Self(CLASSIFICATION.replace(Some(head)))
        }
    }
    impl Drop for ClassificationScope {
        fn drop(&mut self) {
            CLASSIFICATION.set(self.0);
        }
    }
    #[cfg(test)]
    pub(crate) fn with_forward<T>(run: impl FnOnce() -> T) -> T {
        struct Reset(bool);
        impl Drop for Reset {
            fn drop(&mut self) {
                FORWARD.set(self.0);
            }
        }
        let _reset = Reset(FORWARD.replace(true));
        run()
    }
}
