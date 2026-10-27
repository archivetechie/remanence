//! Raw physical-tape access traits for Layer 3c.
//!
//! The body-facing [`BlockSource`] and
//! [`BlockSink`] traits are intentionally
//! object-local. Bootstrap discovery, catalog-less scanning, and future
//! sidecar/filemark handling need physical tape operations instead: configure
//! fixed-block size, locate to a physical address, read records, and space
//! filemarks. This module is the v0.4.4 bridge for those operations.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use remanence_library::scsi::decode_sense;
use remanence_library::{
    BlockSink, BlockSize, BlockSource, DriveHandle, SpaceKind, TapeConfig, TapeIoError,
    TapePosition, WriteFilemarksOutcome, WriteOutcome, WriteUnpositionedOutcome,
};

use crate::error::ParityError;

/// Whether a failed raw READ positively establishes damaged media.
///
/// Only a current `MEDIUM ERROR` sense or a consumed over-size record is
/// evidence about the bytes on tape. Transport, reset, target-state, and
/// adapter failures leave the component unresolved: callers must not turn
/// those failures into overwrite authority.
pub(crate) fn raw_read_error_proves_damage(error: &ParityError) -> bool {
    matches!(
        error,
        ParityError::TapeIo(TapeIoError::ReadBufferTooSmall { .. })
    ) || matches!(
        error,
        ParityError::TapeIo(error) if tape_error_is_current_medium_damage(error)
    )
}

/// Whether current sense data positively identifies medium damage.
pub fn tape_error_is_current_medium_damage(error: &TapeIoError) -> bool {
    #[cfg(target_os = "linux")]
    {
        error.check_condition_transfer().is_some_and(|(sense, _)| {
            decode_sense(sense).is_some_and(|decoded| !decoded.is_deferred() && decoded.key == 0x03)
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = error;
        false
    }
}

/// A position that a device reports is the device's fact, not the tape's: when
/// it does not fit in u64, or goes backwards, the device is at fault and the
/// error is `TapeIo` (REM-PARITY 2.4). Every device-derived position and count
/// that Layer 3c computes reports through this one constructor.
pub(crate) fn device_position_error(detail: impl std::fmt::Display) -> ParityError {
    ParityError::TapeIo(TapeIoError::OperationFailed(format!(
        "device-reported position {detail}"
    )))
}

/// The position `records` records after a device-reported position. A sum that
/// does not fit in u64 is a device fault (REM-PARITY 2.4), never a saturated or
/// wrapped position.
pub(crate) fn device_position_after(
    position: PhysicalPositionHint,
    records: u64,
    context: &'static str,
) -> Result<PhysicalPositionHint, ParityError> {
    let lba = position.lba.checked_add(records).ok_or_else(|| {
        device_position_error(format_args!(
            "{} + {records} does not fit in u64 ({context})",
            position.lba
        ))
    })?;
    Ok(PhysicalPositionHint {
        lba,
        partition: position.partition,
    })
}

/// One fixed-block read as the Reader roles judge it (REM-PARITY 3.5).
///
/// A record shorter or longer than one block is a fact about the tape, not a
/// failure of the device. The raw layer reports it with its measured length,
/// as an outcome distinct from both boundary outcomes and from a transport or
/// medium failure. Its caller decides what the record means for its component:
/// an invalid candidate for a control component, or an erasure for the
/// Recoverer. It is never `TapeIo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedRecordRead {
    /// Exactly one block filled the caller's buffer.
    Block {
        /// Physical position immediately after the record.
        position_after: PhysicalPositionHint,
    },
    /// A record shorter or longer than one block.
    WrongLength {
        /// The record's measured length in bytes.
        measured_bytes: u64,
    },
    /// A filemark was encountered and consumed.
    Filemark {
        /// Physical position immediately after the filemark.
        position_after: PhysicalPositionHint,
    },
    /// End-of-data was encountered.
    EndOfData {
        /// Physical position at EOD.
        position_after: PhysicalPositionHint,
    },
}

/// Read one record and classify its length against the one-block buffer.
pub fn read_fixed_record(
    source: &mut dyn RawTapeSource,
    buf: &mut [u8],
) -> Result<FixedRecordRead, ParityError> {
    let block_len = buf.len();
    classify_fixed_record(source.read_record(buf), block_len)
}

/// The measured length of a record that is not one `block_len`-byte block,
/// when the read found one (REM-PARITY 3.5): a delivered length other than
/// `block_len`, or the drive's consumed over-length record
/// (`ReadBufferTooSmall` whose `actual` is not `block_len`). `None` for one
/// block, a boundary, or any other failure. A `ReadBufferTooSmall` whose
/// `actual` is `block_len` reports a caller buffer smaller than one block, not
/// a record of the wrong length. This is the one place the Reader roles judge
/// a record's length.
pub fn wrong_record_length(
    read: &Result<RawReadOutcome, ParityError>,
    block_len: usize,
) -> Option<u64> {
    match read {
        // usize is at most 64 bits on every supported target, so the delivered
        // length converts exactly.
        Ok(RawReadOutcome::Block { bytes, .. }) if *bytes != block_len => Some(*bytes as u64),
        Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall { actual, .. }))
            if *actual as usize != block_len =>
        {
            Some(u64::from(*actual))
        }
        _ => None,
    }
}

/// Classify a completed raw read against one `block_len`-byte block. A record
/// of the wrong length, judged by [`wrong_record_length`], is its own outcome;
/// the leading bytes of an over-length record are never used. Every other
/// error passes through unchanged.
pub fn classify_fixed_record(
    read: Result<RawReadOutcome, ParityError>,
    block_len: usize,
) -> Result<FixedRecordRead, ParityError> {
    if let Some(measured_bytes) = wrong_record_length(&read, block_len) {
        return Ok(FixedRecordRead::WrongLength { measured_bytes });
    }
    match read? {
        RawReadOutcome::Block { position_after, .. } => {
            Ok(FixedRecordRead::Block { position_after })
        }
        RawReadOutcome::Filemark { position_after } => {
            Ok(FixedRecordRead::Filemark { position_after })
        }
        RawReadOutcome::EndOfData { position_after } => {
            Ok(FixedRecordRead::EndOfData { position_after })
        }
    }
}

pub(crate) fn locate_physical_exact(
    source: &mut dyn RawTapeSource,
    hint: PhysicalPositionHint,
) -> Result<(), ParityError> {
    source.locate_physical(hint)?;
    let observed = source.position()?;
    validate_locate_position(hint, observed, "raw source")?;
    Ok(())
}

fn validate_locate_position(
    expected: PhysicalPositionHint,
    observed: PhysicalPositionHint,
    adapter: &'static str,
) -> Result<PhysicalPositionHint, ParityError> {
    // A position reported after LOCATE is a device report: one that differs
    // from the located position faults the device (REM-PARITY 2.4).
    if observed != expected {
        return Err(device_position_error(format_args!(
            "after {adapter} locate is partition {} lba {}, expected partition {} lba {}",
            observed.partition, observed.lba, expected.partition, expected.lba
        )));
    }
    Ok(observed)
}

/// Physical block-address hint used by raw tape operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysicalPositionHint {
    /// Physical logical block address from READ POSITION / LOCATE.
    pub lba: u64,
    /// Tape partition number. Remanence currently uses partition 0.
    pub partition: u32,
}

impl PhysicalPositionHint {
    /// Construct a partition-0 physical-position hint.
    pub const fn new(lba: u64) -> Self {
        Self { lba, partition: 0 }
    }
}

impl From<TapePosition> for PhysicalPositionHint {
    fn from(position: TapePosition) -> Self {
        Self {
            lba: position.lba,
            partition: position.partition,
        }
    }
}

/// Outcome of reading one physical tape record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawReadOutcome {
    /// A data record was read into the caller's buffer.
    Block {
        /// Number of bytes delivered by the drive.
        bytes: usize,
        /// Physical position immediately after the record.
        position_after: PhysicalPositionHint,
    },
    /// A filemark was encountered and consumed.
    Filemark {
        /// Physical position immediately after the filemark.
        position_after: PhysicalPositionHint,
    },
    /// End-of-data was encountered.
    EndOfData {
        /// Physical position at EOD.
        position_after: PhysicalPositionHint,
    },
}

impl RawReadOutcome {
    /// Physical position immediately after the raw read outcome.
    pub fn position_after(self) -> PhysicalPositionHint {
        match self {
            Self::Block { position_after, .. }
            | Self::Filemark { position_after }
            | Self::EndOfData { position_after } => position_after,
        }
    }
}

/// Outcome of writing one physical tape object through [`RawTapeSink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawWriteOutcome {
    /// A fixed-size data block was written.
    WroteBlock {
        /// Bytes physically credited by the underlying write completion.
        bytes_written: u32,
        /// Physical position immediately after the block.
        position_after: PhysicalPositionHint,
        /// True when the drive reported programmable early warning.
        early_warning: bool,
        /// True when the drive reported hard end-of-medium.
        end_of_medium: bool,
    },
    /// A synchronous filemark durability barrier was written.
    WroteFilemark {
        /// Physical position immediately after the filemark.
        position_after: PhysicalPositionHint,
        /// True when the drive reported programmable early warning.
        early_warning: bool,
        /// True when the drive reported hard end-of-medium.
        end_of_medium: bool,
    },
}

impl RawWriteOutcome {
    /// Physical position immediately after the raw write.
    pub fn position_after(self) -> PhysicalPositionHint {
        match self {
            Self::WroteBlock { position_after, .. }
            | Self::WroteFilemark { position_after, .. } => position_after,
        }
    }

    /// Whether the raw write crossed the drive's early-warning point.
    pub fn early_warning(self) -> bool {
        match self {
            Self::WroteBlock { early_warning, .. } | Self::WroteFilemark { early_warning, .. } => {
                early_warning
            }
        }
    }

    /// Whether the raw write reached hard end-of-medium.
    pub fn end_of_medium(self) -> bool {
        match self {
            Self::WroteBlock { end_of_medium, .. } | Self::WroteFilemark { end_of_medium, .. } => {
                end_of_medium
            }
        }
    }
}

/// Outcome of spacing over physical filemarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceFilemarksOutcome {
    /// Signed number of filemarks actually traversed.
    pub filemarks_spaced: i64,
    /// Physical position immediately after the SPACE command.
    pub position_after: PhysicalPositionHint,
    /// True when the operation stopped at end-of-data before the full count.
    pub hit_end_of_data: bool,
}

/// Geometry hints supplied to bootstrap discovery and scan reconstruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeGeometryHint {
    /// Known fixed block size from catalog or operator configuration.
    pub configured_block_size: Option<u32>,
    /// Candidate fixed block sizes to try when the configured size is unknown.
    pub candidate_block_sizes: Vec<u32>,
    /// Physical position hints for bootstrap discovery. REM-PARITY §8.3:
    /// "Exactly one bootstrap is mandatory at BOT, with sequence 0."
    pub probe_positions: Vec<PhysicalPositionHint>,
}

/// How a consecutive fixed-record read ended. Only `records_read` full records
/// are confirmed. A terminal observation is final for the next record; callers
/// must deliver it after the prefix without retrying that record.
#[derive(Debug)]
pub enum RawReadEnd {
    /// Every selected record was read.
    Complete,
    /// A filemark was consumed.
    Filemark {
        /// Position observed by the single reader after consuming the boundary.
        position_after: PhysicalPositionHint,
    },
    /// The drive reached end of recorded data.
    EndOfData {
        /// Position observed by the single reader at end of data.
        position_after: PhysicalPositionHint,
    },
    /// A record did not have the requested fixed length. Its bytes follow the
    /// confirmed prefix in the batch buffer.
    WrongLength {
        /// Length reported by the single reader.
        bytes: usize,
        /// Position observed after the short/long record.
        position_after: PhysicalPositionHint,
    },
    /// A read failed after the confirmed prefix.
    Error(ParityError),
}

/// Confirmed prefix of a bounded fixed-record read, in the caller's buffer.
#[derive(Debug)]
pub struct RawReadBatch {
    /// Number of complete, consecutive records in the buffer.
    pub records_read: u32,
    /// Position after the confirmed data, excluding any consumed boundary.
    pub position_after: Option<PhysicalPositionHint>,
    /// Completion or the reason the read stopped.
    pub end: RawReadEnd,
}

/// Raw physical-tape READ access used by Layer 3c.
pub trait RawTapeSource {
    /// Passive drive context; non-drive sources have no transport to observe.
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        Default::default()
    }

    /// Configure the drive for fixed-block reads at `block_size` bytes.
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError>;

    /// Locate to a physical block-address hint.
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError>;

    /// Position at physical end-of-data without walking tape files.
    ///
    /// Every adapter must implement the operation explicitly so a newly added
    /// source cannot silently make fast terminal inventory unavailable.
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError>;

    /// Space forward or backward over filemarks.
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError>;

    /// Read one physical record at the current position.
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError>;

    /// Read at most `max_records` fixed records without intentionally crossing
    /// the current tape file. A short/error result confirms only its prefix.
    /// Terminal outcomes, including fallback errors and failed fallback LOCATEs,
    /// are final for the next record and must be delivered after that prefix.
    /// Adapters must propagate state-invalidating errors without fallback.
    /// The default preserves compatibility with single-record sources.
    fn read_records(
        &mut self,
        buf: &mut [u8],
        block_size: u32,
        max_records: u32,
        records_remaining_in_file: u32,
    ) -> RawReadBatch {
        read_records_default(
            self,
            buf,
            block_size,
            max_records,
            records_remaining_in_file,
        )
    }

    /// Return the current physical tape position.
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError>;
}

/// Single-record implementation of the batch contract, also useful to adapters
/// that deliberately exercise the compatibility path.
pub fn read_records_default<S: RawTapeSource + ?Sized>(
    source: &mut S,
    buf: &mut [u8],
    block_size: u32,
    max_records: u32,
    records_remaining_in_file: u32,
) -> RawReadBatch {
    let mut batch = RawReadBatch {
        records_read: 0,
        position_after: None,
        end: RawReadEnd::Complete,
    };
    let count = max_records.min(records_remaining_in_file);
    if block_size == 0 || count == 0 || u64::from(count) * u64::from(block_size) > buf.len() as u64
    {
        batch.end = RawReadEnd::Error(ParityError::Invariant(
            "invalid raw read batch buffer/count",
        ));
        return batch;
    }
    for block in buf
        .chunks_exact_mut(block_size as usize)
        .take(count as usize)
    {
        match source.read_record(block) {
            Ok(RawReadOutcome::Block {
                bytes,
                position_after,
            }) if bytes == block_size as usize
                && batch
                    .position_after
                    .is_none_or(|previous: PhysicalPositionHint| {
                        previous.partition == position_after.partition
                            && previous.lba.checked_add(1) == Some(position_after.lba)
                    }) =>
            {
                batch.records_read += 1;
                batch.position_after = Some(position_after);
            }
            Ok(RawReadOutcome::Filemark { position_after }) => {
                batch.end = RawReadEnd::Filemark { position_after };
                break;
            }
            Ok(RawReadOutcome::EndOfData { position_after }) => {
                batch.end = RawReadEnd::EndOfData { position_after };
                break;
            }
            Ok(RawReadOutcome::Block {
                bytes,
                position_after,
            }) if bytes != block_size as usize => {
                batch.end = RawReadEnd::WrongLength {
                    bytes,
                    position_after,
                };
                break;
            }
            Ok(_) => {
                batch.end =
                    RawReadEnd::Error(ParityError::Invariant("unconfirmed fixed record in batch"));
                break;
            }
            Err(error) => {
                batch.end = RawReadEnd::Error(error);
                break;
            }
        }
    }
    batch
}

/// Bounded read-ahead for one consecutive run. Existing block readers retain
/// their parsing, error classification and address checks through this adapter.
/// Deliver the adapter's terminal observation after its confirmed prefix, then
/// finish any later records singly after explicit positioning.
pub(crate) struct ReadRun<'a> {
    source: &'a mut dyn RawTapeSource,
    block_size: u32,
    remaining: u64,
    next: Option<PhysicalPositionHint>,
    located: bool,
    single: bool,
    // Storage stays initialized across fills; only `consumed..valid` is readable.
    buffer: Vec<u8>,
    consumed: usize,
    valid: usize,
    pending_end: Option<RawReadEnd>,
    pending_data: Vec<u8>,
}

impl<'a> ReadRun<'a> {
    pub(crate) fn new(source: &'a mut dyn RawTapeSource, block_size: u32, records: u64) -> Self {
        Self {
            source,
            block_size,
            remaining: records,
            next: None,
            located: false,
            single: false,
            buffer: Vec::new(),
            consumed: 0,
            valid: 0,
            pending_end: None,
            pending_data: Vec::new(),
        }
    }
    /// Set the suffix length after reading and validating a single header.
    pub(crate) fn after_header(&mut self, remaining: u64) {
        assert_eq!(
            self.consumed, self.valid,
            "header must not have unread prefetch"
        );
        self.remaining = remaining;
    }

    /// Continue from the preceding single read's observed position.
    pub(crate) fn continue_at(&mut self, next: Option<PhysicalPositionHint>) {
        self.next = next;
        self.located = next.is_some();
    }
}

impl RawTapeSource for ReadRun<'_> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.source.verification_context()
    }

    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        self.source.configure_fixed_block_size(block_size)
    }
    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        if self.next != Some(hint) {
            self.valid = 0;
            self.consumed = 0;
            self.pending_end = None;
            self.pending_data.clear();
            self.next = None;
            self.located = false;
            self.source.locate_physical(hint)?;
            self.next = Some(hint);
            self.located = true;
        }
        Ok(())
    }
    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.next = None;
        self.valid = 0;
        self.pending_end = None;
        self.pending_data.clear();
        self.source.locate_end_of_data()
    }
    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        self.next = None;
        self.valid = 0;
        self.pending_end = None;
        self.pending_data.clear();
        self.source.space_filemarks(count)
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        match self.next {
            Some(next) => Ok(next),
            None => self.source.position(),
        }
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let position = self
            .next
            .ok_or(ParityError::Invariant("read run requires a locate"))?;
        if self.consumed == self.valid && self.pending_end.is_none() {
            self.valid = 0;
            self.consumed = 0;
            if !self.located {
                self.source.locate_physical(position)?;
                self.located = true;
            }
            if !self.single && self.remaining > 1 && buf.len() == self.block_size as usize {
                let count = self
                    .remaining
                    .min(u64::from(remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS))
                    as u32;
                let fill_bytes = count as usize * self.block_size as usize;
                if self.buffer.len() < fill_bytes {
                    self.buffer.resize(fill_bytes, 0);
                }
                let batch = self.source.read_records(
                    &mut self.buffer[..fill_bytes],
                    self.block_size,
                    count,
                    count,
                );
                let confirmed = if batch.records_read <= count
                    && batch.position_after
                        == Some(device_position_after(
                            position,
                            u64::from(batch.records_read),
                            "read run batch",
                        )?) {
                    batch.records_read
                } else {
                    0
                };
                let confirmed_bytes = confirmed as usize * self.block_size as usize;
                if confirmed != count || !matches!(batch.end, RawReadEnd::Complete) {
                    self.single = true;
                    self.located = false;
                }
                if !matches!(batch.end, RawReadEnd::Complete) {
                    if confirmed != batch.records_read || confirmed == count {
                        self.valid = 0;
                        self.next = None;
                        return Err(ParityError::Invariant("invalid terminal batch prefix"));
                    }
                    if let RawReadEnd::WrongLength { bytes, .. } = &batch.end {
                        // Only the reported terminal bytes belong to this fill.
                        let copied = (*bytes).min(self.block_size as usize);
                        self.pending_data.extend_from_slice(
                            &self.buffer[confirmed_bytes..confirmed_bytes + copied],
                        );
                    }
                    self.pending_end = Some(batch.end);
                }
                self.valid = confirmed_bytes;
            }
        }
        if self.consumed < self.valid {
            let end = self.consumed + buf.len();
            buf.copy_from_slice(&self.buffer[..self.valid][self.consumed..end]);
            self.consumed = end;
            self.remaining -= 1;
            let position_after = device_position_after(position, 1, "read run delivery")?;
            self.next = Some(position_after);
            return Ok(RawReadOutcome::Block {
                bytes: buf.len(),
                position_after,
            });
        }
        if let Some(end) = self.pending_end.take() {
            // The adapter already made the decisive single read (or failed to
            // arrange it). Do not let a later successful retry erase its result.
            self.remaining = self.remaining.saturating_sub(1);
            self.next = None;
            self.located = false;
            return match end {
                RawReadEnd::Error(error) => Err(error),
                RawReadEnd::Filemark { position_after } => {
                    Ok(RawReadOutcome::Filemark { position_after })
                }
                RawReadEnd::EndOfData { position_after } => {
                    Ok(RawReadOutcome::EndOfData { position_after })
                }
                RawReadEnd::WrongLength {
                    bytes,
                    position_after,
                } => {
                    let copied = bytes.min(buf.len()).min(self.pending_data.len());
                    buf[..copied].copy_from_slice(&self.pending_data[..copied]);
                    self.pending_data.clear();
                    Ok(RawReadOutcome::Block {
                        bytes,
                        position_after,
                    })
                }
                RawReadEnd::Complete => {
                    unreachable!("complete batches have no terminal observation")
                }
            };
        }
        if !self.located {
            self.source.locate_physical(position)?;
        }
        self.remaining = self.remaining.saturating_sub(1);
        self.next = None;
        self.located = false;
        let result = self.source.read_record(buf);
        if let Ok(RawReadOutcome::Block {
            bytes,
            position_after,
        }) = &result
        {
            if *bytes == self.block_size as usize {
                self.next = Some(*position_after);
                self.located = true;
            }
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ImageRecord {
    Block {
        tape_file_index: usize,
        block_within_file: u64,
        byte_offset: usize,
    },
    Filemark,
}

/// Raw tape source backed by the publication-vector image-directory layout.
///
/// Each `tape-file-NNN-*.bin` file is one filemark-delimited tape file. The
/// adapter inserts a logical filemark after every image file and exposes the
/// same physical-position operations used by hardware scanning, so consumers
/// exercise the production scanner rather than a directory-specific parser.
#[derive(Debug)]
pub struct ImageDirectoryRawSource {
    directory: Option<PathBuf>,
    tape_files: Vec<Vec<u8>>,
    candidate_block_sizes: Vec<u32>,
    records: Vec<ImageRecord>,
    cursor: usize,
    configured_block_size: Option<u32>,
    unreadable_blocks: BTreeSet<(usize, u64)>,
}

impl ImageDirectoryRawSource {
    /// Open a directory containing dense `tape-file-NNN-*.bin` image files.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, ParityError> {
        let directory = directory.as_ref();
        let entries = fs::read_dir(directory).map_err(|error| {
            image_source_error(format!(
                "read image directory {}: {error}",
                directory.display()
            ))
        })?;
        let mut numbered = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|error| {
                image_source_error(format!(
                    "read image directory entry in {}: {error}",
                    directory.display()
                ))
            })?;
            let file_type = entry.file_type().map_err(|error| {
                image_source_error(format!(
                    "inspect image entry {}: {error}",
                    entry.path().display()
                ))
            })?;
            if !file_type.is_file() {
                continue;
            }
            let Some(tape_file_number) = image_tape_file_number(&entry.file_name()) else {
                continue;
            };
            if numbered.insert(tape_file_number, entry.path()).is_some() {
                return Err(image_source_error(format!(
                    "image directory {} contains duplicate tape file number {tape_file_number}",
                    directory.display()
                )));
            }
        }
        if numbered.is_empty() {
            return Err(image_source_error(format!(
                "image directory {} contains no tape-file-NNN-*.bin files",
                directory.display()
            )));
        }

        let mut tape_files = Vec::with_capacity(numbered.len());
        let mut candidate_block_sizes = Vec::new();
        for (expected, (number, path)) in numbered.into_iter().enumerate() {
            let expected = u64::try_from(expected)
                .map_err(|_| image_source_error("image tape-file count exceeds u64::MAX"))?;
            if number != expected {
                return Err(image_source_error(format!(
                    "image tape-file numbering is not dense: expected {expected}, found {number}"
                )));
            }
            let bytes = fs::read(&path).map_err(|error| {
                image_source_error(format!("read image tape file {}: {error}", path.display()))
            })?;
            if bytes.is_empty() {
                return Err(image_source_error(format!(
                    "image tape file {} is empty",
                    path.display()
                )));
            }
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains("bootstrap"))
            {
                if let Ok(size) = u32::try_from(bytes.len()) {
                    candidate_block_sizes.push(size);
                }
            }
            tape_files.push(bytes);
        }
        candidate_block_sizes.sort_unstable();
        candidate_block_sizes.dedup();
        if candidate_block_sizes.is_empty() {
            return Err(image_source_error(format!(
                "image directory {} has no bootstrap-named tape file from which to infer block size",
                directory.display()
            )));
        }

        Ok(Self {
            directory: Some(directory.to_path_buf()),
            tape_files,
            candidate_block_sizes,
            records: Vec::new(),
            cursor: 0,
            configured_block_size: None,
            unreadable_blocks: BTreeSet::new(),
        })
    }

    /// Construct the same adapter from already-separated tape-file bytes.
    ///
    /// Publication-vector generation uses this constructor to self-check
    /// staged bytes through the same record engine as directory consumers.
    pub fn from_tape_files(tape_files: Vec<Vec<u8>>, block_size: u32) -> Result<Self, ParityError> {
        if tape_files.is_empty() {
            return Err(image_source_error("image contains no tape files"));
        }
        if block_size == 0 {
            return Err(image_source_error("image block size is zero"));
        }
        if tape_files.iter().any(Vec::is_empty) {
            return Err(image_source_error("image contains an empty tape file"));
        }
        Ok(Self {
            directory: None,
            tape_files,
            candidate_block_sizes: vec![block_size],
            records: Vec::new(),
            cursor: 0,
            configured_block_size: None,
            unreadable_blocks: BTreeSet::new(),
        })
    }

    /// Candidate fixed-block sizes inferred from bootstrap-named image files.
    pub fn candidate_block_sizes(&self) -> &[u32] {
        &self.candidate_block_sizes
    }

    /// Number of physical records, including synthetic trailing filemarks.
    ///
    /// This becomes available after fixed-block configuration.
    pub fn physical_record_count(&self) -> Option<u64> {
        self.configured_block_size
            .and_then(|_| u64::try_from(self.records.len()).ok())
    }

    /// Mark one image block as unreadable for deterministic fault injection.
    ///
    /// This is used by immutable-vector tests to model SCSI medium errors
    /// without rewriting or regenerating the pinned artifact bytes.
    pub fn mark_unreadable(
        &mut self,
        tape_file_number: u64,
        block_within_file: u64,
    ) -> Result<(), ParityError> {
        let tape_file_index = usize::try_from(tape_file_number)
            .map_err(|_| image_source_error("image tape-file number does not fit usize"))?;
        if tape_file_index >= self.tape_files.len() {
            return Err(image_source_error(format!(
                "image tape file {tape_file_number} does not exist"
            )));
        }
        self.unreadable_blocks
            .insert((tape_file_index, block_within_file));
        Ok(())
    }

    fn rebuild_records(&mut self, block_size: u32) -> Result<(), ParityError> {
        let block_size = usize::try_from(block_size)
            .map_err(|_| image_source_error("image block size does not fit usize"))?;
        let mut records = Vec::new();
        for (tape_file_index, bytes) in self.tape_files.iter().enumerate() {
            if !bytes.len().is_multiple_of(block_size) {
                let location = self.directory.as_ref().map_or_else(
                    || "in-memory image".to_string(),
                    |path| path.display().to_string(),
                );
                return Err(image_source_error(format!(
                    "image tape file {tape_file_index} in {location} has {} bytes, not a multiple of block size {block_size}",
                    bytes.len()
                )));
            }
            for block_index in 0..(bytes.len() / block_size) {
                records.push(ImageRecord::Block {
                    tape_file_index,
                    block_within_file: u64::try_from(block_index)
                        .map_err(|_| image_source_error("image block index exceeds u64::MAX"))?,
                    byte_offset: block_index
                        .checked_mul(block_size)
                        .ok_or_else(|| image_source_error("image byte offset overflows"))?,
                });
            }
            records.push(ImageRecord::Filemark);
        }
        self.records = records;
        self.cursor = 0;
        Ok(())
    }
}

impl RawTapeSource for ImageDirectoryRawSource {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }
        if self.configured_block_size != Some(block_size) {
            self.rebuild_records(block_size)?;
            self.configured_block_size = Some(block_size);
        }
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        if hint.partition != 0 {
            return Err(image_source_error(format!(
                "image source does not contain partition {}",
                hint.partition
            )));
        }
        let requested = usize::try_from(hint.lba)
            .map_err(|_| image_source_error("image LBA does not fit usize"))?;
        if requested > self.records.len() {
            // A drive that is asked to LOCATE past its last record reports
            // end-of-data (BLANK CHECK, 08/00/05) and stays at end-of-data.
            self.cursor = self.records.len();
            return Err(TapeIoError::CheckCondition(
                remanence_library::scsi::ScsiError::CheckCondition {
                    sense: vec![0x72, 0x08, 0x00, 0x05],
                    bytes_transferred: 0,
                },
            )
            .into());
        }
        self.cursor = requested;
        validate_locate_position(
            hint,
            PhysicalPositionHint::new(
                u64::try_from(self.cursor)
                    .map_err(|_| image_source_error("image cursor exceeds u64::MAX"))?,
            ),
            "image raw source",
        )?;
        Ok(())
    }

    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.cursor = self.records.len();
        Ok(PhysicalPositionHint::new(
            u64::try_from(self.cursor)
                .map_err(|_| image_source_error("image EOD position exceeds u64::MAX"))?,
        ))
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        let mut spaced = 0i64;
        if count >= 0 {
            while self.cursor < self.records.len() && spaced < count {
                if self.records[self.cursor] == ImageRecord::Filemark {
                    spaced += 1;
                }
                self.cursor += 1;
            }
        } else {
            while self.cursor > 0 && spaced > count {
                self.cursor -= 1;
                if self.records[self.cursor] == ImageRecord::Filemark {
                    spaced -= 1;
                }
            }
        }
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: spaced,
            position_after: PhysicalPositionHint::new(
                u64::try_from(self.cursor)
                    .map_err(|_| image_source_error("image filemark position exceeds u64::MAX"))?,
            ),
            hit_end_of_data: spaced != count,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let Some(record) = self.records.get(self.cursor).copied() else {
            return Ok(RawReadOutcome::EndOfData {
                position_after: PhysicalPositionHint::new(self.cursor as u64),
            });
        };
        match record {
            ImageRecord::Filemark => {
                self.cursor += 1;
                Ok(RawReadOutcome::Filemark {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                })
            }
            ImageRecord::Block {
                tape_file_index,
                block_within_file,
                byte_offset,
            } => {
                let block_size = usize::try_from(
                    self.configured_block_size
                        .ok_or_else(|| image_source_error("image block size is not configured"))?,
                )
                .map_err(|_| image_source_error("image block size does not fit usize"))?;
                if self
                    .unreadable_blocks
                    .contains(&(tape_file_index, block_within_file))
                {
                    return Err(TapeIoError::CheckCondition(
                        remanence_library::scsi::ScsiError::CheckCondition {
                            sense: vec![0x72, 0x03, 0x11, 0x00],
                            bytes_transferred: 0,
                        },
                    )
                    .into());
                }
                if buf.len() < block_size {
                    return Err(TapeIoError::ReadBufferTooSmall {
                        actual: u32::try_from(block_size).unwrap_or(u32::MAX),
                        provided: u32::try_from(buf.len()).unwrap_or(u32::MAX),
                    }
                    .into());
                }
                let end = byte_offset
                    .checked_add(block_size)
                    .ok_or_else(|| image_source_error("image block end overflows"))?;
                let block = self.tape_files[tape_file_index]
                    .get(byte_offset..end)
                    .ok_or_else(|| image_source_error("image block lies outside tape file"))?;
                buf[..block_size].copy_from_slice(block);
                self.cursor += 1;
                Ok(RawReadOutcome::Block {
                    bytes: block_size,
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                })
            }
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.cursor as u64))
    }
}

fn image_tape_file_number(name: &std::ffi::OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let remainder = name.strip_prefix("tape-file-")?;
    let (number, suffix) = remainder.split_once('-')?;
    if number.is_empty() || !suffix.ends_with(".bin") {
        return None;
    }
    number.parse().ok()
}

fn image_source_error(message: impl Into<String>) -> ParityError {
    ParityError::FilemarkMapReconstruct(message.into())
}

/// Raw physical-tape WRITE access used by Layer 3c.
///
/// This trait is intentionally below the body-facing
/// [`BlockSink`] surface: Layer 3c owns
/// physical object, sidecar, bootstrap, and filemark emission. In particular,
/// [`Self::write_filemarks`] carries both advisory delimiters and synchronous
/// zero-count durability barriers.
///
/// Implementations must preserve completion-unknown failures from the tape
/// transport as `ParityError::TapeIo(TapeIoError::Transport(_))`. Do not wrap
/// SG_IO timeouts, disconnects, or equivalent driver-level failures inside
/// string-only `ParityError` variants; Layer 5 relies on the transport variant
/// to observe the dirty-bit / rescan-required signal.
pub trait RawTapeSink {
    /// Locate to a proved component start before overwriting a torn terminal
    /// control tail.
    ///
    /// The default rejects repositioning so append-only and legacy adapters
    /// cannot accidentally advertise rewritable-media repair. Production
    /// adapters for rewritable drives override this operation.
    fn locate_for_overwrite(&mut self, _hint: PhysicalPositionHint) -> Result<(), ParityError> {
        Err(ParityError::Invariant(
            "raw tape sink does not support proved-start overwrite",
        ))
    }

    /// Append one fixed-size tape block at the current physical position.
    ///
    /// The caller must pass exactly the fixed block size configured for the
    /// tape session. Raw adapters may rely on the drive's fixed-block mode for
    /// enforcement; a size mismatch is a caller invariant violation, not an
    /// alternate variable-block write path.
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError>;

    /// Issue WRITE FILEMARKS with the supplied count and IMMED bit.
    ///
    /// Object/control delimiters use `(1, true)`. A checkpoint barrier uses
    /// `(0, false)`, whose successful completion proves all preceding writes
    /// reached the medium without adding another tape-file boundary.
    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError>;

    /// Return the current physical tape position.
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError>;
}

/// Test/compatibility adapter from the legacy body-facing `BlockSink` to
/// [`RawTapeSink`].
///
/// Production write paths should prefer [`DriveHandleRawSink`]. This adapter
/// exists so in-memory fixtures can exercise the raw write API while the sink
/// migration from `BlockSink` to `RawTapeSink` is still underway.
pub struct BlockSinkRawTapeSink<'a> {
    inner: &'a mut dyn BlockSink,
}

impl<'a> BlockSinkRawTapeSink<'a> {
    /// Wrap a legacy block sink.
    pub fn new(inner: &'a mut dyn BlockSink) -> Self {
        Self { inner }
    }
}

impl RawTapeSink for BlockSinkRawTapeSink<'_> {
    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        Ok(raw_block_outcome(self.inner.write_block(buf)?))
    }

    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError> {
        if immed {
            self.inner.write_filemarks_immediate(count)?;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: self.inner.position()?.into(),
                early_warning: false,
                end_of_medium: false,
            })
        } else {
            Ok(raw_filemark_outcome(self.inner.write_filemarks(count)?))
        }
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(self.inner.position()?.into())
    }
}

/// Test/compatibility adapter from the legacy body-facing `BlockSource` to
/// [`RawTapeSource`].
///
/// This adapter is intended for in-memory tests and legacy callers only. It
/// cannot reconfigure a real drive's fixed block size because `BlockSource`
/// has no configuration hook; it records the requested block size only so
/// existing fixtures can exercise the raw API. Production tape access must use
/// [`DriveHandleRawSource`].
pub struct BlockSourceRawTapeSource<'a> {
    inner: &'a mut dyn BlockSource,
    cursor_hint: PhysicalPositionHint,
    configured_block_size: Option<u32>,
}

impl<'a> BlockSourceRawTapeSource<'a> {
    /// Wrap a legacy block source.
    pub fn new(inner: &'a mut dyn BlockSource) -> Self {
        Self {
            inner,
            cursor_hint: PhysicalPositionHint::new(0),
            configured_block_size: None,
        }
    }

    /// Last fixed block size requested through the raw trait.
    pub fn configured_block_size(&self) -> Option<u32> {
        self.configured_block_size
    }
}

impl RawTapeSource for BlockSourceRawTapeSource<'_> {
    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }
        self.configured_block_size = Some(block_size);
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let position = self.inner.locate(hint.lba)?;
        self.cursor_hint = position.into();
        validate_locate_position(hint, self.cursor_hint, "block-source raw adapter")?;
        Ok(())
    }

    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.cursor_hint = self
            .inner
            .space(0, SpaceKind::EndOfData)?
            .position_after
            .into();
        Ok(self.cursor_hint)
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        let outcome = self.inner.space(count, SpaceKind::Filemarks)?;
        self.cursor_hint = outcome.position_after.into();
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: outcome.units_traversed,
            position_after: self.cursor_hint,
            hit_end_of_data: outcome.stopped_at_boundary,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let bytes = self.inner.read_block(buf)?;
        // The cursor follows the device's reported position; one past it must
        // fit (REM-PARITY 2.4), never saturate.
        self.cursor_hint =
            device_position_after(self.cursor_hint, 1, "block-source adapter after a read")?;
        Ok(RawReadOutcome::Block {
            bytes,
            position_after: self.cursor_hint,
        })
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position = self.inner.position()?;
        self.cursor_hint = position.into();
        Ok(self.cursor_hint)
    }
}

/// Production raw-source adapter over Layer 3a's [`DriveHandle`].
pub struct DriveHandleRawSource<'a> {
    drive: &'a mut DriveHandle,
    cursor_hint: Option<PhysicalPositionHint>,
}

impl<'a> DriveHandleRawSource<'a> {
    /// Wrap a drive handle for raw Layer 3c reads.
    pub fn new(drive: &'a mut DriveHandle) -> Self {
        Self {
            drive,
            cursor_hint: None,
        }
    }

    fn current_or_seed_position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        if let Some(position) = self.cursor_hint {
            return Ok(position);
        }
        let position: PhysicalPositionHint = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn resync_position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position: PhysicalPositionHint = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }

    fn advance_after_block(
        &mut self,
        position_before: PhysicalPositionHint,
    ) -> Result<PhysicalPositionHint, ParityError> {
        let position_after =
            device_position_after(position_before, 1, "drive raw source after a block read")?;
        self.cursor_hint = Some(position_after);
        Ok(position_after)
    }
}

/// Production raw-sink adapter over Layer 3a's [`DriveHandle`].
pub struct DriveHandleRawSink<'a> {
    drive: &'a mut DriveHandle,
    cursor_hint: Option<PhysicalPositionHint>,
}

impl<'a> DriveHandleRawSink<'a> {
    /// Wrap a drive handle for raw Layer 3c writes.
    pub fn new(drive: &'a mut DriveHandle) -> Self {
        Self {
            drive,
            cursor_hint: None,
        }
    }

    /// Configure and verify the Layer 3c parity-write preconditions.
    ///
    /// v0.7.2 makes hardware compression a hard-false precondition for
    /// parity-protected writes because compression breaks the stable
    /// logical-block to physical-extent geometry that sidecar recovery relies
    /// on. This method must be called before the BOT bootstrap is written.
    pub fn configure_parity_write_session(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }
        let current = self
            .drive
            .read_config()
            .map_err(|_| ParityError::DriveCompressionModeUnknown)?;
        let desired_block_size = BlockSize::Fixed {
            size_bytes: block_size,
        };
        self.drive.write_config(TapeConfig {
            density_code: None,
            block_size: desired_block_size,
            compression: false,
            max_block_size_bytes: current.max_block_size_bytes,
            write_protected: current.write_protected,
            worm: current.worm,
        })?;
        let verified = self
            .drive
            .read_config()
            .map_err(|_| ParityError::DriveCompressionModeUnknown)?;
        if verified.compression {
            return Err(ParityError::DriveCompressionEnabled {
                context: crate::error::CompressionRefusalContext::Write,
            });
        }
        if verified.block_size != desired_block_size {
            return Err(ParityError::SessionOpen(format!(
                "drive read back block size {:?}, expected {:?}",
                verified.block_size, desired_block_size
            )));
        }
        Ok(())
    }

    fn current_or_seed_position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        if let Some(position) = self.cursor_hint {
            return Ok(position);
        }
        let position: PhysicalPositionHint = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }
}

impl RawTapeSink for DriveHandleRawSink<'_> {
    fn locate_for_overwrite(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let observed: PhysicalPositionHint = self.drive.locate(hint.lba)?.into();
        if observed.partition != hint.partition || observed.lba != hint.lba {
            self.cursor_hint = Some(observed);
            return Err(ParityError::SessionOpen(format!(
                "overwrite locate returned partition {} lba {}, expected partition {} lba {}",
                observed.partition, observed.lba, hint.partition, hint.lba
            )));
        }
        self.cursor_hint = Some(observed);
        Ok(())
    }

    fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.drive.check_append_admission()?;
        let position_before = self.current_or_seed_position()?;
        self.drive.check_append_admission()?;
        let outcome = match self.drive.write_block_unpositioned(buf) {
            Ok(outcome) => outcome,
            Err(err) => {
                self.cursor_hint = None;
                return Err(err.into());
            }
        };
        let requested_bytes = u32::try_from(buf.len())
            .map_err(|_| ParityError::Invariant("raw fixed-block write length exceeds u32"))?;
        if outcome.bytes_written != requested_bytes {
            self.cursor_hint = None;
            return Err(ParityError::TapeIo(
                TapeIoError::PartialBatchUncommittable {
                    requested_records: 1,
                    written_records: u32::from(outcome.bytes_written >= requested_bytes),
                    requested_bytes: u64::from(requested_bytes),
                    written_bytes: u64::from(outcome.bytes_written),
                    end_of_medium: outcome.end_of_medium,
                    sense: None,
                },
            ));
        }
        let position_after =
            device_position_after(position_before, 1, "drive raw sink after a block write")?;
        self.cursor_hint = Some(position_after);
        Ok(raw_unpositioned_block_outcome(outcome, position_after))
    }

    fn write_filemarks(&mut self, count: u32, immed: bool) -> Result<RawWriteOutcome, ParityError> {
        self.drive.check_append_admission()?;
        let outcome = if immed {
            let position_before = self.current_or_seed_position()?;
            self.drive.check_append_admission()?;
            self.drive.write_filemarks_immediate(count)?;
            RawWriteOutcome::WroteFilemark {
                position_after: device_position_after(
                    position_before,
                    u64::from(count),
                    "drive raw sink after immediate filemarks",
                )?,
                early_warning: false,
                end_of_medium: false,
            }
        } else {
            raw_filemark_outcome(self.drive.write_filemarks(count)?)
        };
        self.cursor_hint = Some(outcome.position_after());
        Ok(outcome)
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position = self.drive.position()?.into();
        self.cursor_hint = Some(position);
        Ok(position)
    }
}

impl RawTapeSource for DriveHandleRawSource<'_> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.drive.verification_context()
    }

    fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
        if block_size == 0 {
            return Err(ParityError::Invariant("fixed block size is zero"));
        }

        let current = self.drive.read_config()?;
        let desired = BlockSize::Fixed {
            size_bytes: block_size,
        };
        if current.block_size == desired {
            return Ok(());
        }

        self.drive.write_config(TapeConfig {
            density_code: None,
            block_size: desired,
            compression: current.compression,
            max_block_size_bytes: current.max_block_size_bytes,
            write_protected: current.write_protected,
            worm: current.worm,
        })?;
        Ok(())
    }

    fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
        let observed = self.drive.locate(hint.lba)?.into();
        self.cursor_hint = Some(observed);
        validate_locate_position(hint, observed, "drive raw source")?;
        Ok(())
    }

    fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        let position_after: PhysicalPositionHint = self
            .drive
            .space(0, SpaceKind::EndOfData)?
            .position_after
            .into();
        self.cursor_hint = Some(position_after);
        Ok(position_after)
    }

    fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
        let outcome = self.drive.space(count, SpaceKind::Filemarks)?;
        let position_after = outcome.position_after.into();
        self.cursor_hint = Some(position_after);
        Ok(SpaceFilemarksOutcome {
            filemarks_spaced: outcome.units_traversed,
            position_after,
            hit_end_of_data: outcome.stopped_at_boundary,
        })
    }

    fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
        let position_before = self.current_or_seed_position()?;
        match self.drive.read_block(buf) {
            Ok(bytes) => {
                let position_after = self.advance_after_block(position_before)?;
                Ok(RawReadOutcome::Block {
                    bytes,
                    position_after,
                })
            }
            Err(err) => match classify_read_boundary(&err) {
                Some(RawReadBoundary::Filemark) => Ok(RawReadOutcome::Filemark {
                    position_after: self.resync_position()?,
                }),
                Some(RawReadBoundary::EndOfData) => Ok(RawReadOutcome::EndOfData {
                    position_after: self.resync_position()?,
                }),
                None => {
                    self.cursor_hint = None;
                    Err(err.into())
                }
            },
        }
    }

    /// Use batches only as a clean fast path; redo an entire unclean command
    /// from its first record through the existing single-record reader. Earlier
    /// clean commands remain confirmed. Filemark/EOD completions are also redone.
    ///
    /// For responses stable per record, verification is equivalent to single
    /// reads. A time-dependent batch fault absent on the single re-read is not
    /// reported: that one re-read is final, including its error, and is delivered
    /// after the confirmed prefix without another retry by ReadRun. A failed
    /// fallback LOCATE is equally final. State-invalidating errors bypass the
    /// fallback and propagate unchanged; reset/mode recovery belongs to the
    /// caller, as on the single path. The drive handle still audits batch faults.
    fn read_records(
        &mut self,
        buf: &mut [u8],
        block_size: u32,
        max_records: u32,
        records_remaining_in_file: u32,
    ) -> RawReadBatch {
        let count = max_records.min(records_remaining_in_file);
        let mut batch = RawReadBatch {
            records_read: 0,
            position_after: None,
            end: RawReadEnd::Complete,
        };
        if block_size == 0
            || count == 0
            || u64::from(count) * u64::from(block_size) > buf.len() as u64
        {
            batch.end = RawReadEnd::Error(ParityError::Invariant(
                "invalid raw read batch buffer/count",
            ));
            return batch;
        }
        while batch.records_read < count {
            let before = match self.current_or_seed_position() {
                Ok(before) => before,
                Err(error) => {
                    batch.end = RawReadEnd::Error(error);
                    break;
                }
            };
            let offset = batch.records_read as usize * block_size as usize;
            let remaining = count - batch.records_read;
            // The strict entry point checks the actual transport byte count,
            // GOOD status and absence of sense against the selected CDB count.
            match self.drive.read_block_batch_for_verification(
                &mut buf[offset..],
                block_size,
                remaining,
                remaining,
            ) {
                Ok(read) => {
                    let after = match device_position_after(
                        before,
                        u64::from(read.records_read),
                        "drive raw batch",
                    ) {
                        Ok(after) => after,
                        Err(error) => {
                            self.cursor_hint = None;
                            batch.end = RawReadEnd::Error(error);
                            break;
                        }
                    };
                    self.cursor_hint = Some(after);
                    batch.records_read += read.records_read;
                    batch.position_after = Some(after);
                }
                Err(error) => {
                    self.cursor_hint = None;
                    if !self.drive.verification_single_fallback_allowed(&error) {
                        batch.end = RawReadEnd::Error(error.into());
                        break;
                    }
                    self.verification_context().hardware_fallback();
                    if let Err(error) = self.locate_physical(before) {
                        batch.end = RawReadEnd::Error(error);
                        break;
                    }
                    let single = read_records_default(
                        self,
                        &mut buf[offset..],
                        block_size,
                        remaining,
                        remaining,
                    );
                    batch.records_read += single.records_read;
                    if single.position_after.is_some() {
                        batch.position_after = single.position_after;
                    }
                    batch.end = single.end;
                    break;
                }
            }
        }
        batch
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.resync_position()
    }
}

fn raw_block_outcome(outcome: WriteOutcome) -> RawWriteOutcome {
    RawWriteOutcome::WroteBlock {
        bytes_written: outcome.bytes_written,
        position_after: outcome.position_after.into(),
        early_warning: outcome.early_warning,
        end_of_medium: outcome.end_of_medium,
    }
}

fn raw_unpositioned_block_outcome(
    outcome: WriteUnpositionedOutcome,
    position_after: PhysicalPositionHint,
) -> RawWriteOutcome {
    RawWriteOutcome::WroteBlock {
        bytes_written: outcome.bytes_written,
        position_after,
        early_warning: outcome.early_warning,
        end_of_medium: outcome.end_of_medium,
    }
}

fn raw_filemark_outcome(outcome: WriteFilemarksOutcome) -> RawWriteOutcome {
    RawWriteOutcome::WroteFilemark {
        position_after: outcome.position_after.into(),
        early_warning: outcome.early_warning,
        end_of_medium: outcome.end_of_medium,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawReadBoundary {
    Filemark,
    EndOfData,
}

/// Whether the drive reports end-of-data (BLANK CHECK, 08/00/05): for a
/// LOCATE, the requested position lies past the recorded data.
pub(crate) fn tape_error_is_end_of_data(err: &TapeIoError) -> bool {
    classify_read_boundary(err) == Some(RawReadBoundary::EndOfData)
}

fn classify_read_boundary(err: &TapeIoError) -> Option<RawReadBoundary> {
    if matches!(err, TapeIoError::FilemarkEncountered) {
        return Some(RawReadBoundary::Filemark);
    }

    #[cfg(target_os = "linux")]
    {
        let (sense, _) = err.check_condition_transfer()?;
        let decoded = decode_sense(sense)?;

        // IBM LTO SCSI Reference GA32-0928-08 §4.12.1: a READ that
        // encounters a filemark returns CHECK CONDITION with FILEMARK +
        // VALID set and associated sense 0/0001. The position is after the
        // filemark, so the raw adapter reports a consumed boundary.
        if decoded.filemark
            && decoded.valid
            && decoded.key == 0x00
            && decoded.asc == 0x00
            && decoded.ascq == 0x01
        {
            return Some(RawReadBoundary::Filemark);
        }

        // IBM Annex B Table B.9 / Layer 3a fixture convention: READ at EOD
        // reports BLANK CHECK with ASC/ASCQ 00/05.
        if err.is_physical_eod() {
            return Some(RawReadBoundary::EndOfData);
        }

        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = err;
        None
    }
}

#[cfg(test)]
mod compat_tests {
    #[test]
    fn sense_residue_boundary_and_damage_classifiers_accept_corrected_error() {
        for (key, bytes, eod, damage) in [
            (8, 0, true, false),
            (8, 1, false, false),
            (3, 0, false, true),
        ] {
            let mut sense = vec![0; 18];
            sense[0] = 0xf0;
            sense[2] = key;
            sense[13] = 5;
            let error = TapeIoError::CheckConditionWithResidue {
                source: remanence_library::ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 262_144,
                },
                bytes_transferred: bytes,
            };
            assert_eq!(
                classify_read_boundary(&error) == Some(RawReadBoundary::EndOfData),
                eod
            );
            assert_eq!(tape_error_is_current_medium_damage(&error), damage);
        }
    }

    #[test]
    fn fixed_read_boundary_requires_exact_current_zero_transfer_eod() {
        for (response, key, ascq, bytes, accepted) in [
            (0x70, 8, 5, 0, true),
            (0x71, 8, 5, 0, false),
            (0x70, 8, 5, 1, false),
            (0x70, 8, 0, 0, false),
            (0x70, 3, 5, 0, false),
        ] {
            let mut sense = vec![0; 18];
            sense[0] = response;
            sense[2] = key;
            sense[7] = 10;
            sense[13] = ascq;
            let error = TapeIoError::CheckCondition(remanence_library::ScsiError::CheckCondition {
                sense,
                bytes_transferred: bytes,
            });
            assert_eq!(
                matches!(
                    classify_read_boundary(&error),
                    Some(RawReadBoundary::EndOfData)
                ),
                accepted
            );
        }
    }

    use super::*;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    use remanence_library::transport::{TimeoutClass, TransferOutcome};
    use remanence_library::{
        DriveBay, ElementLayout, FixtureTransport, IdentitySource, IePort, InstalledDrive,
        IoErrorKind, Library, RecordingLog, RecordingTransport, SgTransport, Slot, StaticAllowlist,
        VecBlockSink, VecBlockSource, VecBlockSourceCall,
    };

    type TestTransportFactory = Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>;

    struct MispositioningBlockSource {
        inner: VecBlockSource,
        partition: u32,
        lba_offset: u64,
    }

    impl remanence_library::BlockRead for MispositioningBlockSource {
        fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
            self.inner.read_block(buf)
        }
    }

    impl BlockSource for MispositioningBlockSource {
        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            let mut observed = self.inner.locate(lba.saturating_add(self.lba_offset))?;
            observed.partition = self.partition;
            Ok(observed)
        }

        fn space(
            &mut self,
            count: i64,
            kind: SpaceKind,
        ) -> Result<remanence_library::SpaceResult, TapeIoError> {
            self.inner.space(count, kind)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            let mut observed = self.inner.position()?;
            observed.partition = self.partition;
            Ok(observed)
        }
    }

    #[test]
    fn image_tape_file_number_parser_preserves_full_u64_domain() {
        for number in [u64::from(u32::MAX) + 1, (1_u64 << 53) + 1, u64::MAX] {
            let name = format!("tape-file-{number}-object.bin");
            assert_eq!(
                image_tape_file_number(std::ffi::OsStr::new(&name)),
                Some(number)
            );
        }
        assert_eq!(
            image_tape_file_number(std::ffi::OsStr::new(
                "tape-file-18446744073709551616-object.bin"
            )),
            None
        );
    }

    #[test]
    fn image_fault_injection_does_not_narrow_tape_file_number() {
        let mut source = ImageDirectoryRawSource::from_tape_files(vec![vec![0; 4]], 4)
            .expect("construct image source");
        let number = u64::from(u32::MAX) + 1;
        let error = source
            .mark_unreadable(number, 0)
            .expect_err("out-of-image u64 tape-file number must fail");
        assert!(error.to_string().contains(&number.to_string()), "{error}");
    }

    #[test]
    fn image_fault_injection_emits_descriptor_medium_error() {
        let mut source = ImageDirectoryRawSource::from_tape_files(vec![vec![0; 8]], 4)
            .expect("construct image source");
        source
            .mark_unreadable(0, 1)
            .expect("second image block exists");
        source
            .configure_fixed_block_size(4)
            .expect("configure image source");
        source
            .locate_physical(PhysicalPositionHint::new(1))
            .expect("locate unreadable block");

        let error = source
            .read_record(&mut [0; 4])
            .expect_err("marked block returns a medium error");
        assert!(matches!(
            error,
            ParityError::TapeIo(TapeIoError::CheckCondition(
                remanence_library::scsi::ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                }
            )) if sense == [0x72, 0x03, 0x11, 0x00]
        ));
    }

    #[test]
    fn image_source_reports_a_locate_past_the_last_record_as_end_of_data() {
        let mut source = ImageDirectoryRawSource::from_tape_files(vec![vec![0; 4]], 4)
            .expect("construct image source");
        source
            .configure_fixed_block_size(4)
            .expect("configure image source");
        // One block, then its filemark: records 0..2. A LOCATE to the position
        // after the last record is end-of-data itself, and succeeds.
        source
            .locate_physical(PhysicalPositionHint::new(2))
            .expect("LOCATE to end-of-data");
        assert_eq!(source.position().unwrap(), PhysicalPositionHint::new(2));
        // A LOCATE past the last record is reported as a drive reports it,
        // end-of-data (BLANK CHECK), never clamped and then failed as a wrong
        // position; the source stays at end-of-data.
        let error = source
            .locate_physical(PhysicalPositionHint::new(3))
            .expect_err("a LOCATE past the last record is end-of-data");
        assert!(
            matches!(&error, ParityError::TapeIo(e) if tape_error_is_end_of_data(e)),
            "{error:?}"
        );
        assert_eq!(source.position().unwrap(), PhysicalPositionHint::new(2));
    }

    #[test]
    fn block_source_raw_adapter_rejects_wrong_lba_and_partition() {
        for (partition, lba_offset) in [(0, 1), (1, 0)] {
            let mut source = MispositioningBlockSource {
                inner: VecBlockSource::new(vec![vec![0; 4]; 4]),
                partition,
                lba_offset,
            };
            let mut raw = BlockSourceRawTapeSource::new(&mut source);
            let error = raw
                .locate_physical(PhysicalPositionHint::new(1))
                .expect_err("compatibility adapter must reject a mismatched LOCATE result");
            assert!(error
                .to_string()
                .contains("block-source raw adapter locate"));
        }
    }

    #[test]
    fn image_source_models_eod_and_backward_filemark_positioning() {
        let mut source =
            ImageDirectoryRawSource::from_tape_files(vec![vec![0x11; 4], vec![0x22; 4]], 4)
                .expect("construct image source");
        source
            .configure_fixed_block_size(4)
            .expect("configure image source");
        assert_eq!(
            source.locate_end_of_data().expect("locate EOD"),
            PhysicalPositionHint::new(4)
        );
        let last = source
            .space_filemarks(-1)
            .expect("backspace final filemark");
        assert_eq!(last.filemarks_spaced, -1);
        assert_eq!(last.position_after, PhysicalPositionHint::new(3));
        let previous = source
            .space_filemarks(-1)
            .expect("backspace previous filemark");
        assert_eq!(previous.filemarks_spaced, -1);
        assert_eq!(previous.position_after, PhysicalPositionHint::new(1));
    }

    struct ShortFirstWriteTransport {
        inner: FixtureTransport,
        bytes_transferred: u32,
        write_returned_short: bool,
    }

    impl ShortFirstWriteTransport {
        fn new(inner: FixtureTransport, bytes_transferred: u32) -> Self {
            Self {
                inner,
                bytes_transferred,
                write_returned_short: false,
            }
        }
    }

    impl SgTransport for ShortFirstWriteTransport {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<TransferOutcome, remanence_library::ScsiError> {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            SgTransport::execute_none(&mut self.inner, cdb)
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<TransferOutcome, remanence_library::ScsiError> {
            if cdb.first() == Some(&0x0A) && !self.write_returned_short {
                self.write_returned_short = true;
                self.inner.cdb_log.push(cdb.to_vec());
                return Ok(TransferOutcome::clean(self.bytes_transferred));
            }
            SgTransport::execute_out(&mut self.inner, cdb, buf)
        }

        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }

        fn configure_reserved_buffer(
            &mut self,
            requested_bytes: u32,
        ) -> Result<u32, remanence_library::ScsiError> {
            self.inner.configure_reserved_buffer(requested_bytes)
        }
    }

    #[test]
    fn block_sink_adapter_writes_single_filemark_boundary() {
        let mut sink = VecBlockSink::new();

        let (block_outcome, filemark_outcome, position) = {
            let mut raw = BlockSinkRawTapeSink::new(&mut sink);
            let block_outcome = raw
                .write_fixed_block(&[0xAB; 4])
                .expect("block write succeeds");
            let filemark_outcome = raw.write_filemarks(1, false).expect("filemark succeeds");
            let position = raw.position().expect("position succeeds");
            (block_outcome, filemark_outcome, position)
        };

        assert_eq!(
            block_outcome,
            RawWriteOutcome::WroteBlock {
                bytes_written: 4,
                position_after: PhysicalPositionHint::new(1),
                early_warning: false,
                end_of_medium: false,
            }
        );
        assert_eq!(
            filemark_outcome,
            RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(2),
                early_warning: false,
                end_of_medium: false,
            }
        );
        assert_eq!(position, PhysicalPositionHint::new(2));
        assert_eq!(sink.blocks, vec![vec![0xAB; 4]]);
        assert_eq!(sink.filemarks, vec![1]);
        assert_eq!(sink.next_lba(), 2);
    }

    #[test]
    fn block_sink_adapter_preserves_nonexact_physical_byte_count() {
        struct NonExactBlockSink {
            inner: VecBlockSink,
            reported_bytes: u32,
        }

        impl BlockSink for NonExactBlockSink {
            fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
                let outcome = self.inner.write_block(buf)?;
                Ok(WriteOutcome::from_computed_position(
                    self.reported_bytes,
                    outcome.early_warning,
                    outcome.end_of_medium,
                    outcome.position_after,
                ))
            }

            fn write_filemarks(
                &mut self,
                count: u32,
            ) -> Result<WriteFilemarksOutcome, TapeIoError> {
                self.inner.write_filemarks(count)
            }

            fn position(&mut self) -> Result<TapePosition, TapeIoError> {
                self.inner.position()
            }
        }

        for reported_bytes in [3, 5] {
            let mut sink = NonExactBlockSink {
                inner: VecBlockSink::new(),
                reported_bytes,
            };
            let outcome = {
                let mut raw = BlockSinkRawTapeSink::new(&mut sink);
                raw.write_fixed_block(&[0xAB; 4])
                    .expect("compatibility adapter returns the physical outcome")
            };

            assert!(matches!(
                outcome,
                RawWriteOutcome::WroteBlock {
                    bytes_written,
                    position_after,
                    early_warning: false,
                    end_of_medium: false,
                } if bytes_written == reported_bytes
                    && position_after == PhysicalPositionHint::new(1)
            ));
        }
    }

    fn vpd80_response(serial: &str) -> Vec<u8> {
        let bytes = serial.as_bytes();
        let mut response = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
        response.extend_from_slice(bytes);
        response
    }

    fn changer_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
    }

    fn lto9_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
    }

    fn read_position_long_response(flags: u8, partition: u32, lba: u64) -> Vec<u8> {
        let mut response = vec![0u8; 32];
        response[0] = flags;
        response[4..8].copy_from_slice(&partition.to_be_bytes());
        response[8..16].copy_from_slice(&lba.to_be_bytes());
        response
    }

    fn rbl_response(max_block_length: u32, min_block_length: u16) -> Vec<u8> {
        let mut buf = vec![0u8; 6];
        let max = max_block_length.to_be_bytes();
        buf[1] = max[1];
        buf[2] = max[2];
        buf[3] = max[3];
        let min = min_block_length.to_be_bytes();
        buf[4] = min[0];
        buf[5] = min[1];
        buf
    }

    fn mode_sense_response(block_length: u32, dce: bool) -> Vec<u8> {
        let mut buf = vec![0u8; 28];
        buf[0] = 27;
        buf[3] = 8;
        let bl = block_length.to_be_bytes();
        buf[9] = bl[1];
        buf[10] = bl[2];
        buf[11] = bl[3];
        buf[12] = remanence_library::scsi::mode::PAGE_DATA_COMPRESSION;
        buf[13] = 14;
        buf[14] = if dce { 0x80 } else { 0x00 };
        buf
    }

    fn open_drive_test_lib(library_serial: &str) -> Library {
        Library {
            serial: library_serial.to_string(),
            changer_sg: PathBuf::from("/dev/sg-mock"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
            changer_inquiry: remanence_library::scsi::Inquiry::parse(include_bytes!(
                "../../../fixtures/inquiry/changer-msl-g3.bin"
            ))
            .unwrap(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x0100,
                drive_count: 1,
                slot_start: 0x0400,
                slot_count: 1,
                ie_start: 0,
                ie_count: 0,
            },
            drive_bays: vec![DriveBay {
                element_address: 0x0100,
                accessible: true,
                exception: None,
                installed: Some(InstalledDrive {
                    serial: "DRV_A".into(),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    vendor: None,
                    product: None,
                    revision: None,
                    sg_path: Some(PathBuf::from("/dev/sg-drive-mock")),
                    sysfs_path: None,
                }),
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            }],
            slots: vec![Slot {
                element_address: 0x0400,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("TAPE_A".into()),
            }],
            ie_ports: Vec::<IePort>::new(),
        }
    }

    #[allow(clippy::type_complexity)]
    fn multi_recording_factory(
        scripts: Vec<(PathBuf, Vec<Vec<u8>>)>,
    ) -> (
        Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>>,
        RecordingLog,
    ) {
        let log: RecordingLog = RecordingLog::new();
        let log_cl = log.clone();
        let mut bag: HashMap<PathBuf, FixtureTransport> = scripts
            .into_iter()
            .map(|(path, responses)| (path, FixtureTransport::new().with_responses(responses)))
            .collect();
        let factory: TestTransportFactory = Box::new(move |path: &Path| {
            let inner = bag.remove(path).ok_or_else(|| IoErrorKind {
                kind: "NotFound",
                message: format!("no fixture transport seeded for {path:?}"),
                raw_os_error: None,
            })?;
            let wrapped = RecordingTransport::with_log(inner, log_cl.clone());
            Ok(Box::new(wrapped) as Box<dyn SgTransport>)
        });
        (factory, log)
    }

    #[test]
    fn parity_write_session_configures_and_verifies_compression_off() {
        let lib = open_drive_test_lib("LIB_RAW_CFG01");
        let policy = StaticAllowlist::new(["LIB_RAW_CFG01"]);
        let block_size = 262_144;
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_CFG01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(0, true),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(block_size, false),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(block_size)
                .expect("parity session configures compression-off and fixed block size");
        }

        let config_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb[0], 0x05 | 0x15 | 0x1A))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            config_cdbs.iter().map(|cdb| cdb[0]).collect::<Vec<_>>(),
            vec![0x05, 0x1A, 0x15, 0x05, 0x1A],
            "configure_parity_write_session must read, select, then read back: {config_cdbs:?}"
        );
        assert_eq!(
            config_cdbs[1],
            remanence_library::scsi::mode::build_mode_sense6_cdb(
                remanence_library::scsi::mode::PageControl::Current,
                remanence_library::scsi::mode::PAGE_DATA_COMPRESSION,
                64,
            )
        );
        assert_eq!(
            config_cdbs[2].as_slice(),
            &[0x15, 0x10, 0x00, 0x00, 28, 0x00],
            "MODE SELECT(6) must use PF=1, SP=0, and the 28-byte compression parameter list"
        );
        assert_eq!(config_cdbs[4], config_cdbs[1]);
        assert!(!handle.is_dirty());
    }

    #[test]
    fn parity_write_session_rejects_compression_enabled_readback() {
        let lib = open_drive_test_lib("LIB_RAW_CFG02");
        let policy = StaticAllowlist::new(["LIB_RAW_CFG02"]);
        let block_size = 262_144;
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_CFG02")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(0, true),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(block_size, true),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let err = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(block_size)
                .expect_err("compression-enabled read-back must be refused")
        };

        assert!(matches!(err, ParityError::DriveCompressionEnabled { .. }));
        assert_eq!(
            err.to_string(),
            "LTO hardware compression is enabled; parity-protected writes require it disabled"
        );
        assert!(
            log.borrow().iter().any(|cdb| cdb[0] == 0x15),
            "the adapter should attempt MODE SELECT before rejecting the read-back"
        );
        assert!(!handle.is_dirty());
    }

    #[test]
    fn parity_write_session_maps_config_readback_failure_to_unknown() {
        let lib = open_drive_test_lib("LIB_RAW_CFG03");
        let policy = StaticAllowlist::new(["LIB_RAW_CFG03"]);
        let block_size = 262_144;
        let mut malformed_verified = mode_sense_response(block_size, false);
        malformed_verified[3] = 0;
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_CFG03")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    rbl_response(0x80_0000, 1),
                    mode_sense_response(0, false),
                    rbl_response(0x80_0000, 1),
                    malformed_verified,
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let err = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(block_size)
                .expect_err("malformed read-back must fail closed")
        };

        assert!(matches!(err, ParityError::DriveCompressionModeUnknown));
        assert!(
            log.borrow().iter().any(|cdb| cdb[0] == 0x15),
            "read-back failure happens after the configuration attempt"
        );
        assert!(!handle.is_dirty());
    }

    #[test]
    fn drive_handle_raw_sink_filemark_uses_count_one_immed_clear_cdb() {
        let lib = open_drive_test_lib("LIB_RAW_WF01");
        let policy = StaticAllowlist::new(["LIB_RAW_WF01"]);
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_WF01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    read_position_long_response(0, 0, 6_999),
                    read_position_long_response(0, 0, 7_000),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let outcome = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            // This low-level transport fixture supplies its admission proof.
            let position = drive.position().unwrap();
            let _admission = drive
                .install_append_admission(crate::raw::FixtureAppendProof(
                    [0x42; 16],
                    position.partition,
                    position.lba,
                    drive.identity_generation(),
                ))
                .unwrap();
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.write_filemarks(1, false)
                .expect("raw sync filemark succeeds")
        };

        assert_eq!(outcome.position_after(), PhysicalPositionHint::new(7_000));
        assert!(!outcome.early_warning());
        assert!(!outcome.end_of_medium());

        let write_filemarks_cdbs: Vec<Vec<u8>> = log
            .borrow()
            .iter()
            .filter(|cdb| cdb[0] == 0x10)
            .cloned()
            .collect();
        assert_eq!(
            write_filemarks_cdbs.len(),
            1,
            "expected exactly one WRITE FILEMARKS CDB: {write_filemarks_cdbs:?}"
        );
        assert_eq!(
            write_filemarks_cdbs[0].as_slice(),
            &[0x10, 0x00, 0x00, 0x00, 0x01, 0x00],
            "RawTapeSink filemark barrier must keep IMMED clear and write exactly one mark"
        );
    }

    #[test]
    fn drive_handle_raw_sink_seeds_position_once_for_sequential_block_writes() {
        let lib = open_drive_test_lib("LIB_RAW_WR01");
        let policy = StaticAllowlist::new(["LIB_RAW_WR01"]);
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_WR01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    read_position_long_response(0, 0, 10),
                    read_position_long_response(0, 0, 10),
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let (first, second) = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            // This low-level transport fixture supplies its admission proof.
            let position = drive.position().unwrap();
            let _admission = drive
                .install_append_admission(crate::raw::FixtureAppendProof(
                    [0x42; 16],
                    position.partition,
                    position.lba,
                    drive.identity_generation(),
                ))
                .unwrap();
            let mut raw = DriveHandleRawSink::new(&mut drive);
            let first = raw
                .write_fixed_block(&[0xAA; 1024])
                .expect("first raw block succeeds");
            let second = raw
                .write_fixed_block(&[0xBB; 1024])
                .expect("second raw block succeeds");
            (first, second)
        };

        assert_eq!(first.position_after(), PhysicalPositionHint::new(11));
        assert_eq!(second.position_after(), PhysicalPositionHint::new(12));

        let log = log.borrow();
        let read_position_count = log.iter().filter(|cdb| cdb[0] == 0x34).count();
        let write_count = log.iter().filter(|cdb| cdb[0] == 0x0A).count();
        assert_eq!(
            read_position_count, 2,
            "one position proves fixture admission and raw writes seed once, not after every block"
        );
        assert_eq!(write_count, 2);
        assert!(!handle.is_dirty());
    }

    #[test]
    fn drive_handle_raw_sink_rejects_short_write_and_discards_cursor_hint() {
        let lib = open_drive_test_lib("LIB_RAW_WR02");
        let policy = StaticAllowlist::new(["LIB_RAW_WR02"]);
        let log = RecordingLog::new();
        let log_cl = log.clone();
        let mut transports: HashMap<PathBuf, Box<dyn SgTransport>> =
            HashMap::from([
                (
                    PathBuf::from("/dev/sg-mock"),
                    Box::new(FixtureTransport::new().with_responses([
                        changer_inquiry_response(),
                        vpd80_response("LIB_RAW_WR02"),
                    ])) as Box<dyn SgTransport>,
                ),
                (
                    PathBuf::from("/dev/sg-drive-mock"),
                    Box::new(ShortFirstWriteTransport::new(
                        FixtureTransport::new().with_responses([
                            lto9_inquiry_response(),
                            vpd80_response("DRV_A"),
                            read_position_long_response(0, 0, 10),
                            read_position_long_response(0, 0, 10),
                            read_position_long_response(0, 0, 50),
                        ]),
                        1023,
                    )) as Box<dyn SgTransport>,
                ),
            ]);
        let factory: TestTransportFactory = Box::new(move |path: &Path| {
            let inner = transports.remove(path).ok_or_else(|| IoErrorKind {
                kind: "NotFound",
                message: format!("no fixture transport seeded for {path:?}"),
                raw_os_error: None,
            })?;
            Ok(Box::new(RecordingTransport::with_log(
                inner,
                log_cl.clone(),
            )))
        });
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let position_after_failure = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            // This low-level transport fixture supplies its admission proof.
            let position = drive.position().unwrap();
            let _admission = drive
                .install_append_admission(crate::raw::FixtureAppendProof(
                    [0x42; 16],
                    position.partition,
                    position.lba,
                    drive.identity_generation(),
                ))
                .unwrap();
            let mut raw = DriveHandleRawSink::new(&mut drive);
            let err = raw
                .write_fixed_block(&[0xAA; 1024])
                .expect_err("short physical completion must fail");
            assert!(matches!(
                err,
                ParityError::TapeIo(TapeIoError::PartialBatchUncommittable {
                    requested_records: 1,
                    written_records: 0,
                    requested_bytes: 1024,
                    written_bytes: 1023,
                    ..
                })
            ));
            raw.position()
                .expect("position after failure must query the drive again")
        };

        assert_eq!(position_after_failure, PhysicalPositionHint::new(50));
        let log = log.borrow();
        // Admission proof, initial raw cursor, then re-query after failure.
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x34).count(), 3);
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x0A).count(), 1);
        assert!(!handle.is_dirty());
    }

    #[test]
    fn raw_cursor_seed_cannot_move_the_admitted_first_write() {
        for filemark in [false, true] {
            let lib = open_drive_test_lib("LIB_RAW_ADMISSION");
            let policy = StaticAllowlist::new(["LIB_RAW_ADMISSION"]);
            let (factory, log) = multi_recording_factory(vec![
                (
                    PathBuf::from("/dev/sg-mock"),
                    vec![
                        changer_inquiry_response(),
                        vpd80_response("LIB_RAW_ADMISSION"),
                    ],
                ),
                (
                    PathBuf::from("/dev/sg-drive-mock"),
                    vec![
                        lto9_inquiry_response(),
                        vpd80_response("DRV_A"),
                        read_position_long_response(0, 0, 10),
                        read_position_long_response(0, 0, 11),
                    ],
                ),
            ]);
            let mut handle = lib.open_with(&policy, factory).unwrap();
            let mut drive = handle.open_drive(0x0100, &policy).unwrap();
            let position = drive.position().unwrap();
            let _admission = drive
                .install_append_admission(crate::raw::FixtureAppendProof(
                    [0x42; 16],
                    position.partition,
                    position.lba,
                    drive.identity_generation(),
                ))
                .unwrap();
            let mut raw = DriveHandleRawSink::new(&mut drive);
            let result = if filemark {
                raw.write_filemarks(1, true)
            } else {
                raw.write_fixed_block(&[0xAA; 1024])
            };
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("admission position mismatch"));
            assert_eq!(
                log.borrow()
                    .iter()
                    .filter(|cdb| matches!(cdb[0], 0x0A | 0x10))
                    .count(),
                0
            );
        }
    }

    #[test]
    fn drive_handle_raw_source_seeds_position_once_for_sequential_block_reads() {
        let lib = open_drive_test_lib("LIB_RAW_RD01");
        let policy = StaticAllowlist::new(["LIB_RAW_RD01"]);
        let (factory, log) = multi_recording_factory(vec![
            (
                PathBuf::from("/dev/sg-mock"),
                vec![changer_inquiry_response(), vpd80_response("LIB_RAW_RD01")],
            ),
            (
                PathBuf::from("/dev/sg-drive-mock"),
                vec![
                    lto9_inquiry_response(),
                    vpd80_response("DRV_A"),
                    read_position_long_response(0, 0, 20),
                    vec![0x11; 1024],
                    vec![0x22; 1024],
                ],
            ),
        ]);
        let mut handle = lib.open_with(&policy, factory).expect("library opens");

        let (first, second) = {
            let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let mut buf = vec![0u8; 1024];
            let first = raw.read_record(&mut buf).expect("first raw block succeeds");
            assert_eq!(&buf[..4], &[0x11; 4]);
            let second = raw
                .read_record(&mut buf)
                .expect("second raw block succeeds");
            assert_eq!(&buf[..4], &[0x22; 4]);
            (first, second)
        };

        assert_eq!(first.position_after(), PhysicalPositionHint::new(21));
        assert_eq!(second.position_after(), PhysicalPositionHint::new(22));

        let log = log.borrow();
        let read_position_count = log.iter().filter(|cdb| cdb[0] == 0x34).count();
        let read_count = log.iter().filter(|cdb| cdb[0] == 0x08).count();
        assert_eq!(
            read_position_count, 1,
            "raw fixed-block reads should seed position once, not after every block"
        );
        assert_eq!(read_count, 2);
        assert!(!handle.is_dirty());
    }

    /// Strict transport script: unexpected or omitted commands fail the test.
    struct ReadScript {
        steps: std::collections::VecDeque<(u8, Result<Vec<u8>, remanence_library::ScsiError>)>,
        fixed_good_sense: Option<remanence_library::transport::SenseInfo>,
    }

    impl SgTransport for ReadScript {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<TransferOutcome, remanence_library::ScsiError> {
            let (opcode, response) = self.steps.pop_front().expect("unexpected input command");
            assert_eq!(cdb[0], opcode);
            let bytes = response?;
            assert!(bytes.len() <= buf.len());
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok(TransferOutcome {
                bytes_transferred: bytes.len() as u32,
                sense: if cdb[0] == 0x08 && cdb[1] & 1 != 0 {
                    self.fixed_good_sense.take()
                } else {
                    None
                },
            })
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            assert_eq!(cdb[0], 0x92, "only LOCATE is expected");
            if self
                .steps
                .front()
                .is_some_and(|(opcode, _)| *opcode == 0x92)
            {
                return self.steps.pop_front().unwrap().1.map(|_| ());
            }
            Ok(())
        }
        fn execute_out(
            &mut self,
            _: &[u8],
            _: &[u8],
        ) -> Result<TransferOutcome, remanence_library::ScsiError> {
            panic!("no writes in a read test")
        }
    }

    impl Drop for ReadScript {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(self.steps.is_empty(), "unconsumed transport script");
                assert!(self.fixed_good_sense.is_none(), "unconsumed GOOD sense");
            }
        }
    }

    fn scripted_read_drive(
        steps: Vec<(u8, Result<Vec<u8>, remanence_library::ScsiError>)>,
        position_check_bytes: u64,
    ) -> (DriveHandle, RecordingLog) {
        scripted_read_drive_with_sense(steps, position_check_bytes, None)
    }

    fn scripted_read_drive_with_sense(
        steps: Vec<(u8, Result<Vec<u8>, remanence_library::ScsiError>)>,
        position_check_bytes: u64,
        fixed_good_sense: Option<remanence_library::transport::SenseInfo>,
    ) -> (DriveHandle, RecordingLog) {
        let serial = "LIB_READ_SCRIPT";
        let policy = StaticAllowlist::new([serial]);
        let mut script = vec![
            (0x12, Ok(lto9_inquiry_response())),
            (0x12, Ok(vpd80_response("DRV_A"))),
        ];
        script.extend(steps);
        let log = RecordingLog::new();
        let mut drive_transport = Some(RecordingTransport::with_log(
            ReadScript {
                steps: script.into(),
                fixed_good_sense,
            },
            log.clone(),
        ));
        let mut changer = Some(
            FixtureTransport::new()
                .with_responses([changer_inquiry_response(), vpd80_response(serial)]),
        );
        let factory: TestTransportFactory = Box::new(move |path| {
            if path == Path::new("/dev/sg-mock") {
                Ok(Box::new(changer.take().unwrap()) as Box<dyn SgTransport>)
            } else {
                Ok(Box::new(drive_transport.take().unwrap()) as Box<dyn SgTransport>)
            }
        });
        let mut handle = open_drive_test_lib(serial)
            .open_with(&policy, factory)
            .unwrap();
        let drive = handle
            .open_drive_with_tape_io(
                0x0100,
                &policy,
                remanence_library::TapeIoRuntimeConfig {
                    read_batch_blocks: 2,
                    position_check_bytes,
                    ..Default::default()
                },
            )
            .unwrap();
        (drive, log)
    }

    fn read_filemark_error(residual: u32, bytes_transferred: u32) -> remanence_library::ScsiError {
        let mut sense = vec![0; 18];
        sense[0] = 0xf0;
        sense[2] = 0x80;
        sense[3..7].copy_from_slice(&residual.to_be_bytes());
        sense[7] = 10;
        sense[13] = 1;
        remanence_library::ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        }
    }

    /// Counts below the bare adapter include tripwires, never decorator queries.
    #[test]
    fn bare_drive_single_batch_transitions_keep_position_through_boundaries() {
        let eod = || remanence_library::ScsiError::CheckCondition {
            sense: vec![0x72, 0x08, 0x00, 0x05],
            bytes_transferred: 0,
        };
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 20))),
                (0x08, Ok(vec![1; 4])), // single -> 21
                (0x08, Ok(vec![2; 8])), // clamped batch -> 23
                (0x34, Ok(read_position_long_response(0, 0, 23))),
                (0x08, Ok(vec![3; 4])), // remaining batch -> 24
                (0x08, Ok(vec![4; 4])), // single -> 25
                (0x08, Err(read_filemark_error(1, 4))), // partial batch: data -> 26, FM -> 27
                (0x34, Ok(read_position_long_response(0, 0, 25))), // redo LOCATE
                (0x08, Ok(vec![4; 4])), // redo data -> 26
                (0x08, Err(read_filemark_error(0, 0))), // single FM -> 27
                (0x34, Ok(read_position_long_response(0, 0, 27))), // boundary resync
                (0x08, Ok(vec![5; 4])), // single -> 28
                (0x08, Ok(vec![6; 8])), // prefix -> 30
                (0x34, Ok(read_position_long_response(0, 0, 30))),
                (0x08, Err(eod())), // EOD during the next partial command
                (0x34, Ok(read_position_long_response(0, 0, 30))), // redo LOCATE
                (0x08, Err(eod())),
                (0x34, Ok(read_position_long_response(0, 0, 30))), // redo boundary resync
                (0x08, Err(eod())),
                (0x34, Ok(read_position_long_response(0, 0, 30))), // single boundary resync
            ],
            8,
        );
        {
            let mut raw = DriveHandleRawSource::new(&mut drive);
            assert_eq!(
                raw.read_record(&mut [0; 4]).unwrap().position_after().lba,
                21
            );
            let batch = raw.read_records(&mut [0; 12], 4, 3, 3);
            assert!(matches!(batch.end, RawReadEnd::Complete), "{batch:?}");
            assert_eq!(
                (batch.records_read, batch.position_after.unwrap().lba),
                (3, 24)
            );
            assert_eq!(
                raw.read_record(&mut [0; 4]).unwrap().position_after().lba,
                25
            );
            let batch = raw.read_records(&mut [0; 12], 4, 3, 3);
            assert!(
                matches!(batch.end, RawReadEnd::Filemark { .. }),
                "{batch:?}"
            );
            assert_eq!(
                (batch.records_read, batch.position_after.unwrap().lba),
                (1, 26)
            );
            assert_eq!(
                raw.read_record(&mut [0; 4]).unwrap().position_after().lba,
                28
            );
            let batch = raw.read_records(&mut [0; 12], 4, 3, 3);
            assert!(
                matches!(batch.end, RawReadEnd::EndOfData { .. }),
                "{batch:?}"
            );
            assert_eq!(
                (batch.records_read, batch.position_after.unwrap().lba),
                (2, 30)
            );
            assert!(
                matches!(raw.read_record(&mut [0; 4]).unwrap(), RawReadOutcome::EndOfData { position_after } if position_after.lba == 30)
            );
        }
        let log = log.borrow();
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x92).count(), 2);
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x34).count(), 8);
        assert_eq!(
            log.iter()
                .filter(|cdb| cdb[0] == 0x08 && cdb[1] & 1 == 0)
                .count(),
            7
        );
        assert_eq!(
            log.iter()
                .filter(|cdb| cdb[0] == 0x08 && cdb[1] & 1 != 0)
                .count(),
            5
        );
        println!("bare transitions: LOCATE=2 READ_POSITION=8 single_READ=7 batch_READ=5");
    }

    /// A failed batch tripwire is discarded too; successful positioning and
    /// single re-reads supply the verification result.
    #[test]
    fn batch_read_position_failure_redoes_singly() {
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![1; 8])),
                (
                    0x34,
                    Err(remanence_library::ScsiError::InvalidInput(
                        "tripwire failure",
                    )),
                ),
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![2; 4])),
                (0x08, Ok(vec![3; 4])),
            ],
            4,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut buf = [0; 8];
        let batch = raw.read_records(&mut buf, 4, 2, 2);
        assert!(matches!(batch.end, RawReadEnd::Complete), "{batch:?}");
        assert_eq!(batch.records_read, 2);
        assert_eq!(buf, [2, 2, 2, 2, 3, 3, 3, 3]);
        assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(), 1);
    }

    /// The batch fault is time-dependent: the successful single re-read stands.
    #[test]
    fn one_shot_non_medium_batch_fault_uses_single_reread() {
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 8])), // earlier clean command stays confirmed
                (
                    0x08,
                    Err(remanence_library::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x04, 0x44, 0],
                        bytes_transferred: 0,
                    }),
                ),
                (0x34, Ok(read_position_long_response(0, 0, 2))),
                (0x08, Ok(vec![8; 4])),
            ],
            0,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut run = ReadRun::new(&mut raw, 4, 3);
        run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        for lba in 1..=3 {
            let mut buf = [0; 4];
            assert_eq!(run.read_record(&mut buf).unwrap().position_after().lba, lba);
            assert_eq!(buf, [if lba == 3 { 8 } else { 7 }; 4]);
        }
        let log = log.borrow();
        let locates: Vec<_> = log
            .iter()
            .filter(|cdb| cdb[0] == 0x92)
            .map(|cdb| u64::from_be_bytes(cdb[4..12].try_into().unwrap()))
            .collect();
        assert_eq!(locates, [0, 2]);
        assert_eq!(
            log.iter()
                .filter(|cdb| cdb[0] == 0x08 && cdb[1] & 1 == 0)
                .count(),
            1
        );
    }

    /// A read-ahead fault must not prevent delivery of an earlier clean prefix;
    /// the designated single fault is final even if a further attempt succeeds.
    #[test]
    fn read_run_delivers_single_fault_before_a_later_success() {
        let fault = || remanence_library::ScsiError::CheckCondition {
            sense: vec![0x72, 0x04, 0x44, 0],
            bytes_transferred: 0,
        };
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 8])),
                (0x08, Err(fault())), // speculative fixed command
                (0x34, Ok(read_position_long_response(0, 0, 2))),
                (0x08, Err(fault())), // adapter's single redo
                (0x34, Ok(read_position_long_response(0, 0, 2))),
                (0x08, Ok(vec![9; 4])), // a further attempt would succeed
            ],
            0,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut run = ReadRun::new(&mut raw, 4, 3);
        run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        for lba in 1..=2 {
            let mut buf = [0; 4];
            assert_eq!(run.read_record(&mut buf).unwrap().position_after().lba, lba);
            assert_eq!(buf, [7; 4]);
        }
        let error = run.read_record(&mut [0; 4]).unwrap_err();
        assert!(
            matches!(error, ParityError::TapeIo(TapeIoError::CheckCondition(
            remanence_library::ScsiError::CheckCondition { sense, .. }
        )) if sense == [0x72, 0x04, 0x44, 0])
        );
        let log = log.borrow();
        let locates: Vec<_> = log
            .iter()
            .filter(|cdb| cdb[0] == 0x92)
            .map(|cdb| u64::from_be_bytes(cdb[4..12].try_into().unwrap()))
            .collect();
        assert_eq!(locates, [0, 2]);
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x08).count(), 3);
        drop(log);
        drop(run);
        // Explicitly demonstrate the next attempt would succeed. It must not
        // be consumed by the verification run above.
        raw.locate_physical(PhysicalPositionHint::new(2)).unwrap();
        let mut buf = [0; 4];
        assert!(matches!(
            raw.read_record(&mut buf).unwrap(),
            RawReadOutcome::Block { bytes: 4, .. }
        ));
        assert_eq!(buf, [9; 4]);

        let (mut single_drive, _) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 2))),
                (0x08, Err(fault())),
            ],
            0,
        );
        let single_error = DriveHandleRawSource::new(&mut single_drive)
            .read_record(&mut [0; 4])
            .unwrap_err();
        assert!(
            matches!(single_error, ParityError::TapeIo(TapeIoError::CheckCondition(
            remanence_library::ScsiError::CheckCondition { sense, .. }
        )) if sense == [0x72, 0x04, 0x44, 0])
        );
    }

    /// A failed fallback LOCATE is delivered after the clean prefix, with no
    /// second positioning attempt that could erase the error.
    #[test]
    fn read_run_preserves_failed_fallback_locate() {
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 8])),
                (0x08, Ok(vec![8; 3])), // unclean short batch
                (
                    0x92,
                    Err(remanence_library::ScsiError::InvalidInput(
                        "fallback LOCATE failed",
                    )),
                ),
            ],
            0,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut run = ReadRun::new(&mut raw, 4, 3);
        run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        for lba in 1..=2 {
            let mut buf = [0; 4];
            assert_eq!(run.read_record(&mut buf).unwrap().position_after().lba, lba);
            assert_eq!(buf, [7; 4]);
        }
        assert!(matches!(
            run.read_record(&mut [0; 4]).unwrap_err(),
            ParityError::TapeIo(TapeIoError::InvalidRequest(
                remanence_library::ScsiError::InvalidInput("fallback LOCATE failed")
            ))
        ));
        let log = log.borrow();
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x92).count(), 2);
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x08).count(), 2);
    }

    /// Reset is returned unchanged, whether fixed mode was validated or not,
    /// and whether the failed batch has an earlier clean prefix or not.
    #[test]
    fn batch_reset_matches_single_without_fallback() {
        for validated in [false, true] {
            for prefix in [false, true] {
                for ascq in 0..=4 {
                    let sense = vec![0x72, 0x06, 0x29, ascq];
                    let observe = |batch| {
                        let mut steps = Vec::new();
                        if validated {
                            steps.extend([
                                (0x05, Ok(rbl_response(1024, 1))),
                                (0x1a, Ok(mode_sense_response(4, false))),
                            ]);
                        }
                        steps.push((0x34, Ok(read_position_long_response(0, 0, 0))));
                        if prefix {
                            if batch {
                                steps.push((0x08, Ok(vec![7; 8])));
                            } else {
                                steps.extend([(0x08, Ok(vec![7; 4])), (0x08, Ok(vec![7; 4]))]);
                            }
                        }
                        steps.push((
                            0x08,
                            Err(remanence_library::ScsiError::CheckCondition {
                                sense: sense.clone(),
                                bytes_transferred: 0,
                            }),
                        ));
                        let (mut drive, log) = scripted_read_drive(steps, 0);
                        if validated {
                            drive.read_config().unwrap();
                        }
                        let mut raw = DriveHandleRawSource::new(&mut drive);
                        let error = if batch {
                            let mut run = ReadRun::new(&mut raw, 4, if prefix { 3 } else { 2 });
                            run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
                            if prefix {
                                for lba in 1..=2 {
                                    assert_eq!(
                                        run.read_record(&mut [0; 4]).unwrap().position_after().lba,
                                        lba
                                    );
                                }
                            }
                            run.read_record(&mut [0; 4]).unwrap_err()
                        } else {
                            if prefix {
                                for _ in 0..2 {
                                    raw.read_record(&mut [0; 4]).unwrap();
                                }
                            }
                            raw.read_record(&mut [0; 4]).unwrap_err()
                        };
                        assert!(
                            matches!(&error, ParityError::TapeIo(TapeIoError::StateInvalidatingReset(
                            remanence_library::ScsiError::CheckCondition { sense: actual, bytes_transferred: 0 }
                        )) if actual == &sense),
                            "{error:?}"
                        );
                        assert_eq!(
                            log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(),
                            usize::from(batch)
                        );
                        format!("{error:?}")
                    };
                    assert_eq!(observe(true), observe(false));
                }
            }
        }
    }

    /// A pre-existing mode-reverification flag must preserve the guard's
    /// InvalidRequest too; selecting a batch cannot introduce another LOCATE.
    #[test]
    fn batch_preserves_mode_reverification_guard_error() {
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x05, Ok(rbl_response(1024, 1))),
                (0x1a, Ok(mode_sense_response(4, false))),
                (
                    0x08,
                    Err(remanence_library::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x06, 0x29, 2],
                        bytes_transferred: 0,
                    }),
                ),
                (0x34, Ok(read_position_long_response(0, 0, 0))),
            ],
            0,
        );
        drive.read_config().unwrap();
        assert!(matches!(
            drive.read_block(&mut [0; 4]),
            Err(TapeIoError::StateInvalidatingReset(_))
        ));
        let expected = drive.read_block(&mut [0; 4]).unwrap_err();
        assert!(matches!(expected, TapeIoError::InvalidRequest(_)));
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut run = ReadRun::new(&mut raw, 4, 2);
        run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        let error = run.read_record(&mut [0; 4]).unwrap_err();
        assert_eq!(
            format!("{error:?}"),
            format!("{:?}", ParityError::TapeIo(expected))
        );
        let log = log.borrow();
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x92).count(), 1);
        assert_eq!(log.iter().filter(|cdb| cdb[0] == 0x08).count(), 1);
    }

    /// ILI without complete INFORMATION remains the single reader's error.
    #[test]
    fn truncated_ili_batch_fault_matches_single_read_error() {
        for len in 3..7 {
            let mut sense = vec![0; len];
            sense[0] = 0xf0;
            sense[2] = 0x20;
            assert_stable_batch_fault_matches_single(sense, 0, None);
        }
    }

    /// Even recovered completions accepted by restore are re-read singly.
    #[test]
    fn recovered_batch_completion_matches_single_read_error() {
        for sense in [
            vec![0x70, 0, 0x01],
            vec![0xf0, 0, 0x01, 0, 0, 0, 0],
            vec![0x72, 0x01, 0, 0],
        ] {
            assert_stable_batch_fault_matches_single(sense, 4, None);
        }
    }

    /// Recovered completions rejected by restore also use the single error policy.
    #[test]
    fn rejected_recovered_batch_completion_matches_single_check_condition() {
        assert_stable_batch_fault_matches_single(vec![0x72, 1, 0, 0], 0, None);
        assert_stable_batch_fault_matches_single(vec![0xf0, 0, 1, 0, 0, 0, 1], 4, Some(3));
    }

    #[test]
    fn short_good_batch_redoes_singly_and_reports_wrong_length() {
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 3])), // GOOD, but one requested byte never arrived
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 3])),
            ],
            0,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let batch = raw.read_records(&mut [0xff; 4], 4, 1, 1);
        assert_eq!(batch.records_read, 0);
        assert!(
            matches!(batch.end, RawReadEnd::WrongLength { .. }),
            "{batch:?}"
        );
        assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(), 1);

        let (mut drive, _) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 3])),
            ],
            0,
        );
        assert!(matches!(
            read_fixed_record(&mut DriveHandleRawSource::new(&mut drive), &mut [0; 4]).unwrap(),
            FixedRecordRead::WrongLength {
                measured_bytes: 3,
                ..
            }
        ));
    }

    /// GOOD status alone is insufficient: informational recovered sense or a
    /// residual still forces a single re-read, whose bytes replace the batch.
    #[test]
    fn good_batch_with_sense_redoes_singly() {
        for (key, information) in [(1, None), (0, Some(1))] {
            let (mut drive, log) = scripted_read_drive_with_sense(
                vec![
                    (0x34, Ok(read_position_long_response(0, 0, 0))),
                    (0x08, Ok(vec![9; 4])),
                    (0x34, Ok(read_position_long_response(0, 0, 0))),
                    (0x08, Ok(vec![7; 4])),
                ],
                0,
                Some(remanence_library::transport::SenseInfo {
                    key,
                    asc: 0,
                    ascq: 0,
                    information,
                    ili: false,
                    eom: false,
                    filemark: false,
                }),
            );
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let mut buf = [0; 4];
            let batch = raw.read_records(&mut buf, 4, 1, 1);
            assert!(matches!(batch.end, RawReadEnd::Complete), "{batch:?}");
            assert_eq!(batch.records_read, 1);
            assert_eq!(buf, [7; 4]);
            assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(), 1);
        }
    }

    #[test]
    fn descriptor_filemark_batch_redoes_singly_and_reports_boundary() {
        let fault = || remanence_library::ScsiError::CheckCondition {
            sense: vec![
                0x72, 0, 0, 1, 0, 0, 0, 16, 0, 10, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 2, 0, 0x80,
            ],
            bytes_transferred: 0,
        };
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Err(fault())),
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Err(fault())),
                (0x34, Ok(read_position_long_response(0, 0, 1))),
            ],
            0,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let batch = raw.read_records(&mut [0xff; 4], 4, 1, 1);
        assert_eq!(batch.records_read, 0);
        assert!(
            matches!(batch.end, RawReadEnd::Filemark { .. }),
            "{batch:?}"
        );
        assert_eq!(raw.cursor_hint.unwrap().lba, 1);
        assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(), 1);
    }

    /// Verification accepts exact recovered singles; unrelated callers retain
    /// their existing CHECK CONDITION result. Inconsistent recovery is unchanged.
    #[test]
    fn verification_recovered_single_policy_is_scoped_and_exact() {
        for bytes in [3, 4, 5] {
            for residual in [0u32, 1] {
                let observe = |verify: bool| {
                    let mut sense = vec![0; 18];
                    sense[0] = 0xf0;
                    sense[2] = 1;
                    sense[7] = 10;
                    sense[3..7].copy_from_slice(&residual.to_be_bytes());
                    let (mut drive, _) = scripted_read_drive(
                        vec![
                            (0x34, Ok(read_position_long_response(0, 0, 12))),
                            (
                                0x08,
                                Err(remanence_library::ScsiError::CheckCondition {
                                    sense,
                                    bytes_transferred: bytes,
                                }),
                            ),
                        ],
                        0,
                    );
                    let io = drive.begin_verification_io();
                    let _scope = verify.then(|| drive.verification_context().verification());
                    let result = DriveHandleRawSource::new(&mut drive).read_record(&mut [0; 4]);
                    (result, io.finish())
                };
                let (plain, _) = observe(false);
                let (verified, io) = observe(true);
                assert!(plain.is_err());
                if bytes == 4 && residual == 0 {
                    assert!(matches!(
                        verified,
                        Ok(RawReadOutcome::Block { bytes: 4, .. })
                    ));
                    assert_eq!(io.total.records_confirmed, 1);
                    assert_eq!(io.recovered_lbas, vec![12]);
                } else {
                    assert_eq!(format!("{plain:?}"), format!("{verified:?}"));
                    assert_eq!(io.total.recovered_reads, 0);
                }
            }
        }
    }

    /// Restore's buffer handoff still accepts the fixed reader's recovered completion.
    #[test]
    fn recovered_batch_completion_remains_successful_for_restore_handoff() {
        let (mut drive, _) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (
                    0x08,
                    Err(remanence_library::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x01, 0, 0],
                        bytes_transferred: 8,
                    }),
                ),
            ],
            0,
        );
        let mut source = remanence_library::DriveHandleSource(&mut drive);
        let outcome = source
            .read_buffer_handoff(
                remanence_library::ReadBuffer::try_new_page_aligned(8).unwrap(),
                4,
                2,
                2,
            )
            .unwrap();
        assert_eq!(outcome.position_after.lba, 2);
        assert_eq!(outcome.handoff.records_read, 2);
        assert_eq!(outcome.handoff.valid_bytes, 8);
        assert!(!outcome.handoff.terminal_flags.error);
    }

    /// Stable responses yield the exact single-path error, retaining earlier
    /// clean batches and locating only the first record of the failed command.
    fn assert_stable_batch_fault_matches_single(
        sense: Vec<u8>,
        bytes_transferred: u32,
        corrected_bytes: Option<u32>,
    ) {
        let fault = || remanence_library::ScsiError::CheckCondition {
            sense: sense.clone(),
            bytes_transferred,
        };
        let observe = |batch| {
            let mut steps = vec![(0x34, Ok(read_position_long_response(0, 0, 0)))];
            if batch {
                steps.extend([
                    (0x08, Ok(vec![7; 8])),
                    (0x08, Err(fault())),
                    (0x34, Ok(read_position_long_response(0, 0, 2))),
                ]);
            } else {
                steps.extend([(0x08, Ok(vec![7; 4])), (0x08, Ok(vec![7; 4]))]);
            }
            steps.push((0x08, Err(fault())));
            let (mut drive, log) = scripted_read_drive(steps, 0);
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let error = if batch {
                let mut buf = [0xff; 12];
                let result = raw.read_records(&mut buf, 4, 3, 3);
                assert_eq!(result.records_read, 2);
                assert_eq!(result.position_after, Some(PhysicalPositionHint::new(2)));
                assert_eq!(&buf[..8], &[7; 8]);
                let RawReadEnd::Error(error) = result.end else {
                    panic!("{result:?}")
                };
                error
            } else {
                for lba in 1..=2 {
                    assert_eq!(
                        raw.read_record(&mut [0; 4]).unwrap().position_after().lba,
                        lba
                    );
                }
                raw.read_record(&mut [0; 4]).unwrap_err()
            };
            match (&error, corrected_bytes) {
                (
                    ParityError::TapeIo(TapeIoError::CheckConditionWithResidue {
                        source:
                            remanence_library::ScsiError::CheckCondition {
                                sense: actual,
                                bytes_transferred: kernel,
                            },
                        bytes_transferred: corrected,
                    }),
                    Some(expected),
                ) => {
                    assert_eq!(actual, &sense);
                    assert_eq!(*kernel, bytes_transferred);
                    assert_eq!(*corrected, expected);
                }
                (
                    ParityError::TapeIo(TapeIoError::CheckCondition(
                        remanence_library::ScsiError::CheckCondition {
                            sense: actual,
                            bytes_transferred: kernel,
                        },
                    )),
                    None,
                ) => {
                    assert_eq!(actual, &sense);
                    assert_eq!(*kernel, bytes_transferred);
                }
                _ => panic!("unexpected completion variant: {error:?}"),
            }
            assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0x08).count(), 3);
            assert_eq!(
                log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(),
                usize::from(batch)
            );
            format!("{error:?}")
        };
        assert_eq!(observe(true), observe(false));
    }

    /// READ residues use bytes in variable mode and blocks in fixed mode.
    #[test]
    fn read_residue_conversion_preserves_exact_byte_counts() {
        for (fixed, expected_bytes) in [(false, 7), (true, 4)] {
            let mut sense = vec![0; 18];
            sense[0] = 0xf0;
            sense[2] = 0x08;
            sense[6] = 1;
            sense[7] = 10;
            sense[13] = 5;
            let mut steps = Vec::new();
            if fixed {
                steps.push((0x34, Ok(read_position_long_response(0, 0, 0))));
            }
            steps.push((
                0x08,
                Err(remanence_library::ScsiError::CheckCondition {
                    sense: sense.clone(),
                    bytes_transferred: 8,
                }),
            ));
            let (mut drive, log) = scripted_read_drive(steps, 0);
            let error = if fixed {
                drive
                    .read_block_batch_for_verification(&mut [0; 8], 4, 2, 2)
                    .unwrap_err()
            } else {
                drive.read_block(&mut [0; 8]).unwrap_err()
            };
            match error {
                TapeIoError::CheckConditionWithResidue {
                    source:
                        remanence_library::ScsiError::CheckCondition {
                            sense: actual,
                            bytes_transferred: kernel,
                        },
                    bytes_transferred: corrected,
                } => {
                    assert_eq!(actual, sense);
                    assert_eq!(kernel, 8);
                    assert_eq!(corrected, expected_bytes);
                }
                other => panic!("expected corrected CHECK CONDITION, got {other:?}"),
            }
            let log = log.borrow();
            let read = log.iter().find(|cdb| cdb[0] == 0x08).unwrap();
            assert_eq!(read[1] & 1 != 0, fixed);
        }
    }

    #[test]
    fn fixed_batch_wrong_length_finishes_through_single_record_classification() {
        let mut sense = vec![0; 18];
        sense[0] = 0xf0;
        sense[2] = 0x20;
        sense[6] = 1;
        sense[7] = 10;
        let (mut drive, log) = scripted_read_drive(
            vec![
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (
                    0x08,
                    Err(remanence_library::ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: 0,
                    }),
                ),
                (0x34, Ok(read_position_long_response(0, 0, 0))),
                (0x08, Ok(vec![7; 3])),
            ],
            0,
        );
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut run = ReadRun::new(&mut raw, 4, 2);
        run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        assert!(matches!(
            read_fixed_record(&mut run, &mut [0; 4]).unwrap(),
            FixedRecordRead::WrongLength {
                measured_bytes: 3,
                ..
            }
        ));
        assert_eq!(log.borrow().iter().filter(|cdb| cdb[0] == 0x92).count(), 2);
    }

    #[test]
    fn drive_handle_raw_source_rejects_wrong_locate_lba_and_partition() {
        for (serial, observed) in [
            ("LIB_RAW_LC_LBA", PhysicalPositionHint::new(43)),
            (
                "LIB_RAW_LC_PART",
                PhysicalPositionHint {
                    partition: 1,
                    lba: 42,
                },
            ),
        ] {
            let lib = open_drive_test_lib(serial);
            let policy = StaticAllowlist::new([serial]);
            let (factory, log) = multi_recording_factory(vec![
                (
                    PathBuf::from("/dev/sg-mock"),
                    vec![changer_inquiry_response(), vpd80_response(serial)],
                ),
                (
                    PathBuf::from("/dev/sg-drive-mock"),
                    vec![
                        lto9_inquiry_response(),
                        vpd80_response("DRV_A"),
                        read_position_long_response(0, observed.partition, observed.lba),
                    ],
                ),
            ]);
            let mut handle = lib.open_with(&policy, factory).expect("library opens");

            let error = {
                let mut drive = handle.open_drive(0x0100, &policy).expect("drive opens");
                let mut raw = DriveHandleRawSource::new(&mut drive);
                raw.locate_physical(PhysicalPositionHint::new(42))
                    .expect_err("drive adapter must reject the observed LOCATE mismatch")
            };
            assert!(error.to_string().contains("drive raw source locate"));
            let opcodes: Vec<u8> = log.borrow().iter().map(|cdb| cdb[0]).collect();
            assert!(opcodes.windows(2).any(|window| window == [0x92, 0x34]));
            assert!(!handle.is_dirty());
        }
    }

    #[derive(Default)]
    struct DurabilityMockRawTapeSink {
        position: u64,
        pending_file_started: bool,
        barrier_ready: bool,
        committed_positions: Vec<PhysicalPositionHint>,
    }

    impl DurabilityMockRawTapeSink {
        fn commit_catalog_row(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            if !self.barrier_ready {
                return Err(ParityError::Invariant(
                    "catalog commit attempted before synchronous filemark barrier",
                ));
            }
            let position = PhysicalPositionHint::new(self.position);
            self.committed_positions.push(position);
            self.pending_file_started = false;
            self.barrier_ready = false;
            Ok(position)
        }
    }

    impl RawTapeSink for DurabilityMockRawTapeSink {
        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.position += 1;
            self.pending_file_started = true;
            self.barrier_ready = false;
            Ok(RawWriteOutcome::WroteBlock {
                bytes_written: u32::try_from(buf.len()).expect("test block length fits u32"),
                position_after: PhysicalPositionHint::new(self.position),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemarks(
            &mut self,
            _count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            if !self.pending_file_started {
                return Err(ParityError::Invariant(
                    "filemark barrier requested before a tape file was written",
                ));
            }
            self.position += 1;
            self.barrier_ready = true;
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.position),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(self.position))
        }
    }

    #[test]
    fn catalog_commit_gate_requires_synchronous_filemark_barrier() {
        let mut raw = DurabilityMockRawTapeSink::default();
        raw.write_fixed_block(&[0xCD; 4]).expect("block write");

        let err = raw
            .commit_catalog_row()
            .expect_err("catalog commit before filemark must fail");
        match err {
            ParityError::Invariant(message) => {
                assert!(message.contains("synchronous filemark barrier"));
            }
            other => panic!("expected invariant error, got {other:?}"),
        }

        let barrier = raw.write_filemarks(1, false).expect("sync filemark");
        assert_eq!(barrier.position_after(), PhysicalPositionHint::new(2));
        let committed = raw
            .commit_catalog_row()
            .expect("catalog commit after barrier succeeds");
        assert_eq!(committed, PhysicalPositionHint::new(2));
        assert_eq!(raw.committed_positions, vec![PhysicalPositionHint::new(2)]);
    }

    #[test]
    fn block_source_adapter_spaces_filemarks_through_legacy_source() {
        let mut source = VecBlockSource::new(vec![vec![0u8]; 3]);

        let outcome = {
            let mut raw = BlockSourceRawTapeSource::new(&mut source);
            raw.space_filemarks(5).expect("space filemarks")
        };

        assert_eq!(
            outcome,
            SpaceFilemarksOutcome {
                filemarks_spaced: 3,
                position_after: PhysicalPositionHint::new(3),
                hit_end_of_data: true,
            }
        );
        assert_eq!(source.cursor(), 3);
        assert_eq!(
            source.calls,
            vec![VecBlockSourceCall::Space {
                count: 5,
                kind: SpaceKind::Filemarks,
            }]
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A short terminal record after a full fill must not expose the retained suffix.
    #[test]
    fn reused_fill_delivers_only_confirmed_prefix_and_terminal_bytes() {
        let count = remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS as usize;
        let mut records: Vec<Vec<u8>> = (0..count + 1).map(|n| vec![n as u8; 4]).collect();
        records.push(vec![0xa1, 0xa2]);
        let mut blocks = remanence_library::VecBlockSource::new(records);
        let mut source = BlockSourceRawTapeSource::new(&mut blocks);
        source.configure_fixed_block_size(4).unwrap();
        let mut run = ReadRun::new(&mut source, 4, (count + 2) as u64);
        run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
        let mut block = [0; 4];
        let mut allocation = std::ptr::null();
        for n in 0..=count {
            assert!(matches!(
                run.read_record(&mut block).unwrap(),
                RawReadOutcome::Block { bytes: 4, .. }
            ));
            assert_eq!(block, [n as u8; 4]);
            if n == 0 {
                allocation = run.buffer.as_ptr();
            }
            assert_eq!(run.buffer.as_ptr(), allocation);
            assert_eq!(run.buffer.len(), count * 4);
        }
        assert_eq!(run.valid, 4);
        assert_eq!(run.pending_data, [0xa1, 0xa2]);
        block.fill(0x5a);
        assert!(matches!(
            run.read_record(&mut block).unwrap(),
            RawReadOutcome::Block { bytes: 2, .. }
        ));
        assert_eq!(block, [0xa1, 0xa2, 0x5a, 0x5a]);
    }

    /// A batch adapter that consumes a good prefix and loses its cursor at a
    /// boundary/fault, as a drive may. Singles still read the original image.
    struct StoppedBatchSource {
        inner: ImageDirectoryRawSource,
        end: Option<RawReadEnd>,
        locates: Vec<u64>,
        singles: Vec<u64>,
        batches: usize,
    }

    impl RawTapeSource for StoppedBatchSource {
        fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
            self.inner.configure_fixed_block_size(size)
        }
        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.locates.push(hint.lba);
            self.inner.locate_physical(hint)
        }
        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.inner.locate_end_of_data()
        }
        fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            self.inner.space_filemarks(count)
        }
        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.inner.position()
        }
        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            self.singles.push(self.inner.position()?.lba);
            self.inner.read_record(buf)
        }
        fn read_records(
            &mut self,
            buf: &mut [u8],
            size: u32,
            max: u32,
            remaining: u32,
        ) -> RawReadBatch {
            self.batches += 1;
            assert_eq!((max, remaining), (4, 4));
            let mut prefix = self.inner.read_records(buf, size, 1, remaining);
            assert_eq!(prefix.records_read, 1);
            buf[size as usize..].fill(0xff);
            self.inner.locate_end_of_data().unwrap();
            prefix.end = self.end.take().expect("only one batch before fallback");
            prefix
        }
    }

    /// A short clean batch leaves a suffix to read; a terminal observation is
    /// final and must be returned without consulting the underlying source.
    #[test]
    fn incomplete_batches_preserve_terminal_observations() {
        let position_after = PhysicalPositionHint::new(2);
        for end in [
            RawReadEnd::Complete,
            RawReadEnd::Filemark { position_after },
            RawReadEnd::EndOfData { position_after },
            RawReadEnd::WrongLength {
                bytes: 3,
                position_after,
            },
            RawReadEnd::Error(ParityError::Invariant("decisive single failure")),
        ] {
            let expected = match &end {
                RawReadEnd::Complete => Ok(RawReadOutcome::Block {
                    bytes: 4,
                    position_after,
                }),
                RawReadEnd::Filemark { position_after } => Ok(RawReadOutcome::Filemark {
                    position_after: *position_after,
                }),
                RawReadEnd::EndOfData { position_after } => Ok(RawReadOutcome::EndOfData {
                    position_after: *position_after,
                }),
                RawReadEnd::WrongLength {
                    bytes,
                    position_after,
                } => Ok(RawReadOutcome::Block {
                    bytes: *bytes,
                    position_after: *position_after,
                }),
                RawReadEnd::Error(_) => Err(()),
            };
            let complete = matches!(end, RawReadEnd::Complete);
            let mut source = StoppedBatchSource {
                inner: ImageDirectoryRawSource::from_tape_files(
                    vec![(0..4).flat_map(|n| [n; 4]).collect()],
                    4,
                )
                .unwrap(),
                end: Some(end),
                locates: Vec::new(),
                singles: Vec::new(),
                batches: 0,
            };
            source.configure_fixed_block_size(4).unwrap();
            let mut run = ReadRun::new(&mut source, 4, 4);
            run.locate_physical(PhysicalPositionHint::new(0)).unwrap();
            let mut block = [0; 4];
            assert_eq!(run.read_record(&mut block).unwrap().position_after().lba, 1);
            assert_eq!(block, [0; 4]);
            run.locate_physical(PhysicalPositionHint::new(1)).unwrap();
            let result = run.read_record(&mut block);
            match expected {
                Ok(outcome) => {
                    assert_eq!(result.unwrap(), outcome);
                    if matches!(outcome, RawReadOutcome::Block { bytes: 3, .. }) {
                        assert_eq!(&block[..3], &[0xff; 3]);
                    }
                }
                Err(()) => assert!(matches!(
                    result.unwrap_err(),
                    ParityError::Invariant("decisive single failure")
                )),
            }
            // The next explicit address can still be read after a final error.
            for lba in 2..4 {
                run.locate_physical(PhysicalPositionHint::new(lba)).unwrap();
                assert_eq!(
                    run.read_record(&mut block).unwrap().position_after().lba,
                    lba + 1
                );
                assert_eq!(block, [lba as u8; 4]);
            }
            assert_eq!(
                source.locates,
                if complete { vec![0, 1] } else { vec![0, 2] }
            );
            assert_eq!(
                source.singles,
                if complete { vec![1, 2, 3] } else { vec![2, 3] }
            );
            assert_eq!(source.batches, 1);
        }
    }

    #[test]
    fn a_failed_single_retry_forces_positioning_before_the_next_record() {
        let mut source = StoppedBatchSource {
            inner: ImageDirectoryRawSource::from_tape_files(vec![vec![0x35; 16]], 4).unwrap(),
            end: Some(RawReadEnd::Complete),
            locates: Vec::new(),
            singles: Vec::new(),
            batches: 0,
        };
        source.configure_fixed_block_size(4).unwrap();
        source.inner.mark_unreadable(0, 2).unwrap();
        let mut run = ReadRun::new(&mut source, 4, 4);
        for lba in 0..4 {
            run.locate_physical(PhysicalPositionHint::new(lba)).unwrap();
            let result = run.read_record(&mut [0; 4]);
            if lba == 2 {
                assert!(matches!(result, Err(ParityError::TapeIo(_))));
            } else {
                assert!(matches!(
                    result.unwrap(),
                    RawReadOutcome::Block { bytes: 4, .. }
                ));
            }
        }
        assert_eq!(source.locates, [0, 1, 3]);
        assert_eq!(source.singles, [1, 2, 3]);
        assert_eq!(source.batches, 1);
    }

    #[test]
    fn default_batches_report_good_prefix_and_respect_file_bound() {
        let mut source = ImageDirectoryRawSource::from_tape_files(vec![vec![0x35; 8]], 4).unwrap();
        source.configure_fixed_block_size(4).unwrap();
        let mut buf = [0xff; 16];
        let batch = source.read_records(&mut buf, 4, 4, 2);
        assert_eq!(batch.records_read, 2);
        assert!(matches!(batch.end, RawReadEnd::Complete));
        assert_eq!(source.position().unwrap().lba, 2);
        assert_eq!(buf[8..], [0xff; 8]);
        source
            .locate_physical(PhysicalPositionHint::new(0))
            .unwrap();
        let batch = source.read_records(&mut buf, 4, 4, 4);
        assert_eq!(batch.records_read, 2);
        assert!(matches!(batch.end, RawReadEnd::Filemark { .. }));
        assert_eq!(batch.position_after, Some(PhysicalPositionHint::new(2)));
        let batch = source.read_records(&mut buf, 4, 1, 1);
        assert_eq!(batch.records_read, 0);
        assert!(matches!(batch.end, RawReadEnd::EndOfData { .. }));
    }

    fn fixed_sense(byte2: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut sense = vec![0u8; 18];
        sense[0] = 0x80 | 0x70;
        sense[2] = byte2;
        sense[12] = asc;
        sense[13] = ascq;
        sense
    }

    fn check_condition(sense: Vec<u8>) -> TapeIoError {
        TapeIoError::CheckCondition(remanence_library::scsi::ScsiError::CheckCondition {
            sense,
            bytes_transferred: 0,
        })
    }

    fn read_error(error: TapeIoError) -> ParityError {
        ParityError::TapeIo(error)
    }

    #[test]
    fn raw_read_damage_classifier_is_strictly_fail_closed() {
        let mut deferred_medium = fixed_sense(0x03, 0x11, 0x00);
        deferred_medium[0] = 0x71;
        let current_descriptor_medium = vec![0x72, 0x03, 0x11, 0x00];
        let deferred_descriptor_medium = vec![0x73, 0x03, 0x11, 0x00];
        assert!(raw_read_error_proves_damage(&read_error(check_condition(
            fixed_sense(0x03, 0x11, 0x00),
        ))));
        assert!(raw_read_error_proves_damage(&read_error(check_condition(
            current_descriptor_medium,
        ))));
        assert!(raw_read_error_proves_damage(&read_error(
            TapeIoError::ReadBufferTooSmall {
                actual: 4097,
                provided: 4096,
            },
        )));

        for error in [
            TapeIoError::Transport(remanence_library::scsi::ScsiError::TransportError {
                status: 0,
                host_status: 0,
                driver_status: 0x06,
                info: 1,
                sense: Vec::new(),
            }),
            check_condition(fixed_sense(0x04, 0x44, 0x00)),
            check_condition(deferred_medium),
            check_condition(deferred_descriptor_medium),
            TapeIoError::OperationFailed("adapter failure".to_string()),
            TapeIoError::FilemarkEncountered,
        ] {
            assert!(
                !raw_read_error_proves_damage(&read_error(error)),
                "non-medium or completion-unknown read failure must remain unproved"
            );
        }
    }

    #[test]
    fn classify_read_filemark_boundary() {
        let err = check_condition(fixed_sense(0x80, 0x00, 0x01));
        assert_eq!(
            classify_read_boundary(&err),
            Some(RawReadBoundary::Filemark)
        );
    }

    #[test]
    fn classify_structured_filemark_boundary() {
        assert_eq!(
            classify_read_boundary(&TapeIoError::FilemarkEncountered),
            Some(RawReadBoundary::Filemark)
        );
    }

    #[test]
    fn classify_read_end_of_data_boundary() {
        let err = check_condition(fixed_sense(0x08, 0x00, 0x05));
        assert_eq!(
            classify_read_boundary(&err),
            Some(RawReadBoundary::EndOfData)
        );
    }

    #[test]
    fn classify_read_boundary_rejects_medium_error() {
        let err = check_condition(fixed_sense(0x03, 0x11, 0x00));
        assert_eq!(classify_read_boundary(&err), None);
    }
}

#[cfg(test)]
mod record_length_tests {
    use super::*;

    #[test]
    fn a_position_that_differs_after_locate_is_a_device_fault() {
        let err = validate_locate_position(
            PhysicalPositionHint::new(18),
            PhysicalPositionHint::new(17),
            "drive raw source",
        )
        .unwrap_err();
        assert!(matches!(err, ParityError::TapeIo(_)), "{err:?}");
        assert!(validate_locate_position(
            PhysicalPositionHint::new(18),
            PhysicalPositionHint::new(18),
            "drive raw source",
        )
        .is_ok());
    }

    fn block(bytes: usize) -> Result<RawReadOutcome, ParityError> {
        Ok(RawReadOutcome::Block {
            bytes,
            position_after: PhysicalPositionHint::new(8),
        })
    }

    /// REM-PARITY 3.5: the raw layer reports a record of the wrong length, with
    /// its measured length, as an outcome of its own, distinct from both
    /// boundary outcomes and from a device failure.
    #[test]
    fn fixed_record_classification_reports_the_measured_length() {
        let position = PhysicalPositionHint::new(8);
        assert_eq!(
            classify_fixed_record(block(512), 512).unwrap(),
            FixedRecordRead::Block {
                position_after: position
            }
        );
        assert_eq!(
            classify_fixed_record(block(40), 512).unwrap(),
            FixedRecordRead::WrongLength { measured_bytes: 40 }
        );
        let longer = Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall {
            actual: 1024,
            provided: 512,
        }));
        assert_eq!(
            classify_fixed_record(longer, 512).unwrap(),
            FixedRecordRead::WrongLength {
                measured_bytes: 1024
            }
        );
        assert_eq!(
            classify_fixed_record(
                Ok(RawReadOutcome::Filemark {
                    position_after: position
                }),
                512
            )
            .unwrap(),
            FixedRecordRead::Filemark {
                position_after: position
            }
        );
        assert_eq!(
            classify_fixed_record(
                Ok(RawReadOutcome::EndOfData {
                    position_after: position
                }),
                512
            )
            .unwrap(),
            FixedRecordRead::EndOfData {
                position_after: position
            }
        );
        // A buffer smaller than the one-block record is the caller's error.
        let small_buffer = Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall {
            actual: 512,
            provided: 256,
        }));
        assert!(matches!(
            classify_fixed_record(small_buffer, 512),
            Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall { .. }))
        ));
        let transport = Err(ParityError::TapeIo(TapeIoError::OperationFailed(
            "transport".into(),
        )));
        assert!(matches!(
            classify_fixed_record(transport, 512),
            Err(ParityError::TapeIo(TapeIoError::OperationFailed(_)))
        ));
    }

    /// REM-PARITY 2.4: a device-reported position that does not fit is
    /// `TapeIo`, never `Invariant`, never saturated.
    #[test]
    fn device_positions_that_do_not_fit_fault_the_device() {
        let near_end = PhysicalPositionHint::new(u64::MAX);
        let error = device_position_after(near_end, 1, "test").unwrap_err();
        assert!(
            matches!(error, ParityError::TapeIo(TapeIoError::OperationFailed(ref m)) if m.starts_with("device-reported position"))
        );
        assert_eq!(
            device_position_after(PhysicalPositionHint::new(u64::MAX - 1), 1, "test").unwrap(),
            PhysicalPositionHint::new(u64::MAX)
        );
    }

    /// A block source whose device reports the last representable position.
    struct AtLastPosition;
    impl remanence_library::BlockRead for AtLastPosition {
        fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
            buf.fill(0);
            Ok(buf.len())
        }
    }
    impl BlockSource for AtLastPosition {
        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            Ok(TapePosition {
                lba,
                partition: 0,
                beginning_of_partition: lba == 0,
                end_of_partition: false,
                block_position_end_of_warning: false,
            })
        }
        fn space(
            &mut self,
            _count: i64,
            _kind: SpaceKind,
        ) -> Result<remanence_library::SpaceResult, TapeIoError> {
            Err(TapeIoError::OperationFailed("not used".into()))
        }
        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.locate(u64::MAX)
        }
    }

    /// The block-source adapter must not saturate its cursor (REM-PARITY 2.4).
    #[test]
    fn block_source_adapter_does_not_saturate_its_cursor() {
        let mut inner = AtLastPosition;
        let mut raw = BlockSourceRawTapeSource::new(&mut inner);
        raw.locate_physical(PhysicalPositionHint::new(u64::MAX))
            .expect("the device is at its last position");
        let error = raw
            .read_record(&mut [0; 4])
            .expect_err("a position after u64::MAX does not fit");
        assert!(matches!(error, ParityError::TapeIo(_)), "{error:?}");
    }
}

/// Test-only authority for synthetic transport fixtures that own all media bytes.
#[cfg(test)]
struct FixtureAppendProof([u8; 16], u32, u64, u64);
// SAFETY: fixture callers exclusively own synthetic transports and their entire
// contents. This capability is never compiled into production code.
#[cfg(test)]
unsafe impl remanence_library::AppendBoundaryProof for FixtureAppendProof {
    fn into_boundary(self) -> ([u8; 16], u32, u64, u64) {
        (self.0, self.1, self.2, self.3)
    }
}

/// Raw boundary cancellation complements the per-CDB guard for composite calls.
/// Neither guard interprets cancellation as tape damage or adds verifier findings.
pub struct CancellableRawSource<S>(pub S);
impl<S: crate::RawTapeSource> crate::RawTapeSource for CancellableRawSource<S> {
    fn verification_context(&self) -> remanence_library::verification_io::VerificationContext {
        self.0.verification_context()
    }
    fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), crate::ParityError> {
        self.check()?;
        self.0.configure_fixed_block_size(size)
    }
    fn locate_physical(
        &mut self,
        hint: crate::PhysicalPositionHint,
    ) -> Result<(), crate::ParityError> {
        self.check()?;
        self.0.locate_physical(hint)
    }
    fn locate_end_of_data(&mut self) -> Result<crate::PhysicalPositionHint, crate::ParityError> {
        self.check()?;
        self.0.locate_end_of_data()
    }
    fn space_filemarks(
        &mut self,
        count: i64,
    ) -> Result<crate::SpaceFilemarksOutcome, crate::ParityError> {
        self.check()?;
        self.0.space_filemarks(count)
    }
    fn read_record(&mut self, buf: &mut [u8]) -> Result<crate::RawReadOutcome, crate::ParityError> {
        self.check()?;
        self.0.read_record(buf)
    }
    fn position(&mut self) -> Result<crate::PhysicalPositionHint, crate::ParityError> {
        self.check()?;
        self.0.position()
    }
    fn read_records(
        &mut self,
        buf: &mut [u8],
        size: u32,
        max: u32,
        remaining: u32,
    ) -> crate::raw::RawReadBatch {
        if let Err(error) = self.check() {
            return crate::raw::RawReadBatch {
                records_read: 0,
                position_after: None,
                end: crate::raw::RawReadEnd::Error(error),
            };
        }
        self.0.read_records(buf, size, max, remaining)
    }
}
impl<S: crate::RawTapeSource> CancellableRawSource<S> {
    fn check(&self) -> Result<(), crate::ParityError> {
        self.0
            .verification_context()
            .check_cancelled()
            .map_err(|_| remanence_library::TapeIoError::Cancelled.into())
    }
}
