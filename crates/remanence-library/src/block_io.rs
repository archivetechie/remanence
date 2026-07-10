//! `BlockSink` / `BlockSource` — the trait surface body formats and
//! the parity layer code against.
//!
//! These two traits abstract `DriveHandle`'s block-level I/O so
//! higher-layer crates (`remanence-format`, `remanence-parity`)
//! don't need to depend on each other or on the SCSI layer. Both
//! sit here in `remanence-library` so that Layer 3b (tape format)
//! and Layer 3c (tape parity) are **true siblings** — each depends
//! on `remanence-library` only.
//!
//! Two newtype wrappers — [`DriveHandleSink`] and
//! [`DriveHandleSource`] — adapt the existing [`DriveHandle`]
//! methods into the trait surface. Wrappers are explicit at every
//! call site (rather than a blanket impl) so the dep graph + the
//! orphan rule stay simple. The pattern is documented in
//! `docs/layer3b-design.md` §4.5.
//!
//! ### Errors
//!
//! Trait methods return [`crate::error::DriveOpError`]'s underlying
//! [`remanence_scsi::ScsiError`] via [`crate::handle::tape_io::TapeIoError`]
//! — the same error type that `DriveHandle` exposes today. Format-
//! and parity-layer error types wrap `TapeIoError` with
//! `#[from] TapeIoError`, so transport-error propagation stays
//! mechanical. Defining a new "BlockIoError" here would just
//! re-export `TapeIoError`, so we don't.
//!
//! ### Local file object adapters
//!
//! [`FileBlockSink`] / [`FileBlockSource`] adapt a local file as one
//! fixed-block object byte string. They are for portable RAO object files:
//! no tape filemarks, bootstrap, or REM-PARITY sidecars are represented in
//! the file. Those structures stay on tape; the file contains only the
//! object's stored bytes.
//!
//! ### In-memory test fixtures
//!
//! [`VecBlockSink`] / [`VecBlockSource`] back the traits with a
//! `Vec<Vec<u8>>` and expose the captured CDB / block sequence
//! for assertions. Used by `remanence-format` and
//! `remanence-parity` tests without those crates needing their
//! own private adapters.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use crate::handle::tape_io::{
    PipelinedWriteDiagnostics, ReadBatchOutcome, SpaceKind, SpaceResult, TapeIoError, TapePosition,
    WriteBatchOutcome, WriteFilemarksOutcome, WriteOutcome,
};
use crate::handle::DriveHandle;

// =====================================================================
//  BlockSink — write path
// =====================================================================

/// Block-level write surface that higher layers code against.
/// Implemented by [`DriveHandleSink`] for production and
/// [`VecBlockSink`] for tests. Body formats (Layer 3b) and the
/// parity layer (Layer 3c) both consume `&mut dyn BlockSink` and
/// are oblivious to which implementation they have.
pub trait BlockSink {
    /// Write one tape block. Mirrors
    /// [`DriveHandle::write_block`](crate::handle::DriveHandle::write_block).
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError>;

    /// Write one fixed-size batch. The default preserves single-block
    /// semantics by looping through [`Self::write_block`].
    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        if block_size_bytes == 0 {
            return Err(TapeIoError::OperationFailed(
                "write_block_batch block size must be nonzero".to_string(),
            ));
        }
        let block_size = block_size_bytes as usize;
        if buf.is_empty() || buf.len() % block_size != 0 {
            return Err(TapeIoError::OperationFailed(
                "write_block_batch buffer must contain whole records".to_string(),
            ));
        }
        let mut records = 0u32;
        let mut bytes = 0u32;
        let mut early_warning = false;
        let mut end_of_medium = false;
        let mut position_after = None;
        for block in buf.chunks_exact(block_size) {
            let outcome = self.write_block(block)?;
            records = records.checked_add(1).ok_or_else(|| {
                TapeIoError::OperationFailed("write batch record count overflow".to_string())
            })?;
            bytes = bytes.checked_add(outcome.bytes_written).ok_or_else(|| {
                TapeIoError::OperationFailed("write batch byte count overflow".to_string())
            })?;
            early_warning |= outcome.early_warning;
            end_of_medium |= outcome.end_of_medium;
            position_after = Some(outcome.position_after);
        }
        Ok(WriteBatchOutcome::from_computed_position(
            records,
            bytes,
            early_warning,
            end_of_medium,
            position_after.expect("non-empty batch has a final position"),
        ))
    }

    /// Submit a prebuilt fixed-mode WRITE(6) on the pipelined hot path.
    /// Non-drive sinks preserve semantics through the ordinary batch method.
    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        _cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        self.write_block_batch(buf, block_size_bytes)
    }

    /// Effective records per batch for this sink and block size. A return
    /// value of 1 means callers should preserve single-record behavior.
    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        1
    }

    /// Requested records per write batch before transport-level clamping.
    fn requested_write_batch_blocks(&self) -> u32 {
        self.write_batch_blocks(1)
    }

    /// Number of buffers in the fixed staging ring.
    fn staging_ring_buffers(&self) -> u32 {
        crate::DEFAULT_TAPE_IO_STAGING_RING_BUFFERS
    }

    /// Snapshot allocation-free hot-submitter timing and accounting.
    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        PipelinedWriteDiagnostics::default()
    }

    /// Emit a pre-ioctl intent marker and open the coalesced window span.
    fn begin_pipelined_write_window(
        &mut self,
        _command_count: u32,
        _bytes: u64,
        _first_records: u32,
        _last_records: u32,
    ) {
    }

    /// Close a successful coalesced window span.
    fn finish_pipelined_write_window_success(
        &mut self,
        _command_count: u32,
        _bytes: u64,
        _first_records: u32,
        _last_records: u32,
        _duration: Duration,
    ) {
    }

    /// Emit deferred per-command safety evidence, then close a failed span.
    fn finish_pipelined_write_window_error(
        &mut self,
        _command_count: u32,
        _bytes: u64,
        _first_records: u32,
        _last_records: u32,
        _error: &TapeIoError,
    ) {
    }

    /// Flush deferred safety evidence after the caller's durable fence.
    fn flush_pending_pipeline_audit(&mut self) {}

    /// Configured drift tripwire cadence in bytes, if known.
    fn position_check_bytes(&self) -> u64 {
        0
    }

    /// Write `count` file marks. IMMED is always 0; the call
    /// returns only after the marks are committed to media.
    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError>;

    /// Pipelined-session filemark path. Drive sinks defer safety-relevant
    /// completion evidence until the caller has persisted its fence.
    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.write_filemarks(count)
    }

    /// Position the sink at logical end-of-data before appending another
    /// tape file. Real drives use SSC SPACE(EOD); fixtures may model the
    /// same motion with their captured end position.
    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        Err(TapeIoError::OperationFailed(
            "block sink does not support space to end-of-data".to_string(),
        ))
    }

    /// Pipeline-ordered SPACE(EOD), with safety audit completion deferred.
    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.space_to_end_of_data()
    }

    /// Current tape position via READ POSITION long-form.
    fn position(&mut self) -> Result<TapePosition, TapeIoError>;

    /// Pipeline-ordered READ POSITION, with safety audit completion deferred.
    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.position()
    }
}

// =====================================================================
//  BlockSource — read path
// =====================================================================

/// Block-level read surface that higher layers code against.
/// Implemented by [`DriveHandleSource`] for production and
/// [`VecBlockSource`] for tests.
pub trait BlockSource {
    /// Read one tape block, returning the number of bytes the
    /// drive delivered (the block size for that block in
    /// variable-block mode).
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError>;

    /// Read one fixed-size batch. The default loops through
    /// [`Self::read_block`] and never intentionally crosses `remaining`.
    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<ReadBatchOutcome, TapeIoError> {
        if requested_records == 0 || remaining_records_in_file == 0 {
            return Err(TapeIoError::OperationFailed(
                "read_block_batch record counts must be nonzero".to_string(),
            ));
        }
        if block_size_bytes == 0 {
            return Err(TapeIoError::OperationFailed(
                "read_block_batch block size must be nonzero".to_string(),
            ));
        }
        let block_size = block_size_bytes as usize;
        let records = requested_records
            .min(remaining_records_in_file)
            .min(self.read_batch_blocks(block_size_bytes));
        let needed = (records as usize).checked_mul(block_size).ok_or_else(|| {
            TapeIoError::OperationFailed("read batch byte count overflow".to_string())
        })?;
        if buf.len() < needed {
            return Err(TapeIoError::OperationFailed(
                "read_block_batch buffer too small".to_string(),
            ));
        }
        let mut records_read = 0u32;
        let mut bytes_read = 0u32;
        for slot in buf[..needed].chunks_exact_mut(block_size) {
            let read = self.read_block(slot)?;
            if read != block_size {
                return Err(TapeIoError::OperationFailed(format!(
                    "short fixed batch read: expected {block_size}, got {read}"
                )));
            }
            records_read += 1;
            bytes_read = bytes_read.checked_add(block_size_bytes).ok_or_else(|| {
                TapeIoError::OperationFailed("read batch byte count overflow".to_string())
            })?;
        }
        let position_after = self.position()?;
        Ok(ReadBatchOutcome::from_computed_position(
            records_read,
            bytes_read,
            false,
            position_after,
        ))
    }

    /// Effective records per batch for this source and block size.
    fn read_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        1
    }

    /// LOCATE to the given LBA.
    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError>;

    /// SPACE for relative motion.
    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError>;

    /// Current tape position via READ POSITION long-form.
    fn position(&mut self) -> Result<TapePosition, TapeIoError>;
}

// =====================================================================
//  DriveHandle adapters — production wiring
// =====================================================================

/// Newtype wrapper that lets a [`DriveHandle`] satisfy
/// [`BlockSink`]. Construct at every call site:
///
/// ```ignore
/// let mut sink = DriveHandleSink(&mut drive);
/// format.begin_write(&mut sink, params)?;
/// ```
///
/// Picking a newtype over a blanket impl avoids orphan-rule
/// surprises and makes the adapter explicit at the call site (the
/// rationale is in `docs/layer3b-design.md` §4.5).
pub struct DriveHandleSink<'a>(pub &'a mut DriveHandle);

impl BlockSink for DriveHandleSink<'_> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        self.0.write_block(buf)
    }
    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        self.0.write_block_batch(buf, block_size_bytes)
    }
    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        self.0
            .write_block_batch_pipelined(buf, block_size_bytes, cdb)
    }
    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.0.effective_write_batch_blocks().max(1).min(
            self.0
                .sg_reserved_size_bytes()
                .checked_div(block_size_bytes.max(1))
                .unwrap_or(1)
                .max(1),
        )
    }
    fn requested_write_batch_blocks(&self) -> u32 {
        self.0.requested_write_batch_blocks()
    }
    fn staging_ring_buffers(&self) -> u32 {
        self.0.staging_ring_buffers()
    }
    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        self.0.pipelined_write_diagnostics()
    }
    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.0
            .begin_pipelined_write_window(command_count, bytes, first_records, last_records);
    }
    fn finish_pipelined_write_window_success(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        duration: Duration,
    ) {
        self.0.finish_pipelined_write_window_success(
            command_count,
            bytes,
            first_records,
            last_records,
            duration,
        );
    }
    fn finish_pipelined_write_window_error(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
        error: &TapeIoError,
    ) {
        self.0.flush_pending_pipeline_audit();
        self.0.finish_pipelined_write_window_error(
            command_count,
            bytes,
            first_records,
            last_records,
            error,
        );
    }
    fn flush_pending_pipeline_audit(&mut self) {
        self.0.flush_pending_pipeline_audit();
    }
    fn position_check_bytes(&self) -> u64 {
        self.0.position_check_bytes()
    }
    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.0.write_filemarks(count)
    }
    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.0.write_filemarks_pipelined(count)
    }
    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(self.0.space(0, SpaceKind::EndOfData)?.position_after)
    }
    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(self
            .0
            .space_pipelined(0, SpaceKind::EndOfData)?
            .position_after)
    }
    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.0.position()
    }
    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.0.position_pipelined()
    }
}

/// Newtype wrapper that lets a [`DriveHandle`] satisfy
/// [`BlockSource`]. Same construction pattern as
/// [`DriveHandleSink`].
pub struct DriveHandleSource<'a>(pub &'a mut DriveHandle);

impl BlockSource for DriveHandleSource<'_> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        self.0.read_block(buf)
    }
    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<ReadBatchOutcome, TapeIoError> {
        self.0.read_block_batch(
            buf,
            block_size_bytes,
            requested_records,
            remaining_records_in_file,
        )
    }
    fn read_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.0.effective_read_batch_blocks().max(1).min(
            self.0
                .sg_reserved_size_bytes()
                .checked_div(block_size_bytes.max(1))
                .unwrap_or(1)
                .max(1),
        )
    }
    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.0.locate(lba)
    }
    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        self.0.space(count, kind)
    }
    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.0.position()
    }
}

// =====================================================================
//  FileBlockSink / FileBlockSource — portable local object files
// =====================================================================

/// File-backed [`BlockSink`] for writing one portable fixed-block object.
///
/// This adapter is intentionally narrower than tape I/O: every call to
/// [`BlockSink::write_block`] must supply exactly `block_size` bytes, and
/// nonzero filemark writes are rejected. The resulting file is just the
/// object's stored byte string; tape-only framing such as filemarks,
/// bootstrap rows, and REM-PARITY sidecars is not included.
#[derive(Debug)]
pub struct FileBlockSink {
    file: File,
    block_size: usize,
    next_lba: u64,
}

impl FileBlockSink {
    /// Create a new file sink, failing if `path` already exists.
    pub fn create(path: impl AsRef<Path>, block_size: usize) -> Result<Self, TapeIoError> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|err| file_io_error("create file block sink", path, err))?;
        Self::from_file(file, block_size)
    }

    /// Create or replace a file sink at `path`.
    pub fn create_truncate(path: impl AsRef<Path>, block_size: usize) -> Result<Self, TapeIoError> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|err| file_io_error("create file block sink", path, err))?;
        Self::from_file(file, block_size)
    }

    /// Construct a sink from an already-open file, truncating it to an empty
    /// object and positioning it at byte zero.
    pub fn from_file(mut file: File, block_size: usize) -> Result<Self, TapeIoError> {
        validate_file_block_size(block_size)?;
        file.set_len(0)
            .map_err(|err| file_operation_error("truncate file block sink", err))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|err| file_operation_error("seek file block sink", err))?;
        Ok(Self {
            file,
            block_size,
            next_lba: 0,
        })
    }

    /// Fixed block size this sink accepts.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Current next object-block LBA.
    pub fn next_lba(&self) -> u64 {
        self.next_lba
    }

    /// Flush buffered bytes to the operating system.
    pub fn flush(&mut self) -> Result<(), TapeIoError> {
        self.file
            .flush()
            .map_err(|err| file_operation_error("flush file block sink", err))
    }

    /// Flush buffered bytes and synchronize the file's data and metadata.
    pub fn sync_all(&mut self) -> Result<(), TapeIoError> {
        self.flush()?;
        self.file
            .sync_all()
            .map_err(|err| file_operation_error("sync file block sink", err))
    }

    /// Flush and return the underlying file handle.
    pub fn into_inner(mut self) -> Result<File, TapeIoError> {
        self.flush()?;
        Ok(self.file)
    }
}

impl BlockSink for FileBlockSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if buf.len() != self.block_size {
            return Err(TapeIoError::OperationFailed(format!(
                "file block sink expected {} bytes, got {}",
                self.block_size,
                buf.len()
            )));
        }
        self.file
            .write_all(buf)
            .map_err(|err| file_operation_error("write file block", err))?;
        self.next_lba = self
            .next_lba
            .checked_add(1)
            .ok_or_else(|| TapeIoError::OperationFailed("file block LBA overflow".to_string()))?;
        Ok(WriteOutcome::from_device_position(
            buf.len() as u32,
            false,
            false,
            file_tape_position(self.next_lba, false),
        ))
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        if count != 0 {
            return Err(TapeIoError::OperationFailed(
                "file block sink does not support filemarks".to_string(),
            ));
        }
        Ok(WriteFilemarksOutcome::from_device_position(
            false,
            false,
            file_tape_position(self.next_lba, false),
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(file_tape_position(self.next_lba, false))
    }
}

/// File-backed [`BlockSource`] for reading one portable fixed-block object.
///
/// The source validates at construction that the file length is an exact
/// multiple of `block_size`, then exposes dense object LBAs `0..block_count`.
/// Reads past the end surface the same synthetic BLANK CHECK / EOD condition
/// used by [`VecBlockSource`], so higher-layer readers see tape-like end
/// semantics while consuming local files.
#[derive(Debug)]
pub struct FileBlockSource {
    file: File,
    block_size: usize,
    block_count: u64,
    cursor: u64,
}

impl FileBlockSource {
    /// Open a file source from `path`.
    pub fn open(path: impl AsRef<Path>, block_size: usize) -> Result<Self, TapeIoError> {
        let path = path.as_ref();
        let file =
            File::open(path).map_err(|err| file_io_error("open file block source", path, err))?;
        Self::from_file(file, block_size)
    }

    /// Construct a source from an already-open file.
    pub fn from_file(file: File, block_size: usize) -> Result<Self, TapeIoError> {
        validate_file_block_size(block_size)?;
        let len = file
            .metadata()
            .map_err(|err| file_operation_error("stat file block source", err))?
            .len();
        let block_size_u64 = block_size as u64;
        if len % block_size_u64 != 0 {
            return Err(TapeIoError::OperationFailed(format!(
                "file block source length {len} is not a multiple of block size {block_size}"
            )));
        }
        let block_count = len / block_size_u64;
        if block_count > i64::MAX as u64 {
            return Err(TapeIoError::OperationFailed(format!(
                "file block source block count {block_count} exceeds i64::MAX"
            )));
        }
        Ok(Self {
            file,
            block_size,
            block_count,
            cursor: 0,
        })
    }

    /// Fixed block size this source reads.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Number of full object blocks in the file.
    pub fn block_count(&self) -> u64 {
        self.block_count
    }

    /// Total object bytes represented by this source.
    pub fn len_bytes(&self) -> u64 {
        self.block_count * self.block_size as u64
    }

    /// Return true if the source file has zero object bytes.
    pub fn is_empty(&self) -> bool {
        self.block_count == 0
    }

    /// Current object-block cursor.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// Return the underlying file handle.
    pub fn into_inner(self) -> File {
        self.file
    }
}

impl BlockSource for FileBlockSource {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let lba = self.cursor;
        if lba >= self.block_count {
            return Err(TapeIoError::CheckCondition(
                remanence_scsi::ScsiError::CheckCondition {
                    sense: synth_blank_check_eod_sense(),
                    bytes_transferred: 0,
                },
            ));
        }

        let offset = block_offset(lba, self.block_size)?;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|err| file_operation_error("seek file block source", err))?;

        if buf.len() < self.block_size {
            self.cursor = self.cursor.checked_add(1).ok_or_else(|| {
                TapeIoError::OperationFailed("file block LBA overflow".to_string())
            })?;
            return Err(TapeIoError::ReadBufferTooSmall {
                actual: self.block_size as u32,
                provided: buf.len() as u32,
            });
        }

        self.file
            .read_exact(&mut buf[..self.block_size])
            .map_err(|err| file_operation_error("read file block", err))?;
        self.cursor = self
            .cursor
            .checked_add(1)
            .ok_or_else(|| TapeIoError::OperationFailed("file block LBA overflow".to_string()))?;
        Ok(self.block_size)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        if lba > self.block_count {
            self.cursor = self.block_count;
            return Err(TapeIoError::CheckCondition(
                remanence_scsi::ScsiError::CheckCondition {
                    sense: synth_blank_check_eod_sense(),
                    bytes_transferred: 0,
                },
            ));
        }
        self.cursor = lba;
        Ok(file_tape_position(lba, lba == self.block_count))
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        let block_count = self.block_count as i64;
        let cursor_signed = self.cursor as i64;
        let (new_cursor_signed, units_traversed, stopped_at_boundary) = match kind {
            SpaceKind::EndOfData => (block_count, 0, false),
            _ => {
                let requested_target = cursor_signed + count;
                let clamped_target = requested_target.max(0).min(block_count);
                let actual = clamped_target - cursor_signed;
                let stopped = clamped_target != requested_target;
                (clamped_target, actual, stopped)
            }
        };

        self.cursor = new_cursor_signed as u64;
        Ok(SpaceResult::from_device_position(
            units_traversed,
            stopped_at_boundary,
            file_tape_position(self.cursor, self.cursor == self.block_count),
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(file_tape_position(
            self.cursor,
            self.cursor == self.block_count,
        ))
    }
}

fn validate_file_block_size(block_size: usize) -> Result<(), TapeIoError> {
    if block_size == 0 {
        return Err(TapeIoError::OperationFailed(
            "file block size must be nonzero".to_string(),
        ));
    }
    if block_size > u32::MAX as usize {
        return Err(TapeIoError::OperationFailed(format!(
            "file block size {block_size} exceeds u32::MAX"
        )));
    }
    Ok(())
}

fn block_offset(lba: u64, block_size: usize) -> Result<u64, TapeIoError> {
    lba.checked_mul(block_size as u64)
        .ok_or_else(|| TapeIoError::OperationFailed("file block offset overflow".to_string()))
}

fn file_tape_position(lba: u64, end_of_partition: bool) -> TapePosition {
    TapePosition {
        lba,
        partition: 0,
        beginning_of_partition: lba == 0,
        end_of_partition,
        block_position_end_of_warning: false,
    }
}

fn file_io_error(operation: &str, path: &Path, err: std::io::Error) -> TapeIoError {
    TapeIoError::OperationFailed(format!("{operation} {}: {err}", path.display()))
}

fn file_operation_error(operation: &str, err: std::io::Error) -> TapeIoError {
    TapeIoError::OperationFailed(format!("{operation}: {err}"))
}

// =====================================================================
//  VecBlockSink / VecBlockSource — in-memory test fixtures
// =====================================================================

/// In-memory [`BlockSink`] that captures every block, filemark,
/// and position call in order. Drives format and parity unit
/// tests without touching SG_IO.
///
/// Position semantics: `next_lba` starts at 0; each successful
/// `write_block` increments it; each `write_filemarks(count)`
/// increments it by `count`. `position()` returns the current
/// `next_lba` as `TapePosition.lba`.
#[derive(Debug, Default)]
pub struct VecBlockSink {
    /// Every block payload, in order written. Tests inspect this
    /// to assert format/parity wire layout.
    pub blocks: Vec<Vec<u8>>,
    /// Recorded file mark counts, in order written.
    pub filemarks: Vec<u32>,
    /// Captures the LBA at which each block was written.
    pub block_lbas: Vec<u64>,
    /// Number of modeled SPACE(EOD) calls.
    pub space_to_eod_calls: u64,
    next_lba: u64,
    eod_lba: u64,
}

impl VecBlockSink {
    /// Construct an empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current next-LBA. Useful in tests after a series of writes
    /// without going through the `BlockSink::position()` method.
    pub fn next_lba(&self) -> u64 {
        self.next_lba
    }

    /// Test helper that models external positioning before the next write.
    pub fn set_next_lba_for_test(&mut self, next_lba: u64) {
        self.next_lba = next_lba;
    }

    /// Captured logical end-of-data LBA.
    pub fn eod_lba(&self) -> u64 {
        self.eod_lba
    }
}

impl BlockSink for VecBlockSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let lba = self.next_lba;
        let next_lba = self
            .next_lba
            .checked_add(1)
            .ok_or_else(|| TapeIoError::OperationFailed("VecBlockSink LBA overflow".to_string()))?;
        self.block_lbas.push(lba);
        self.blocks.push(buf.to_vec());
        self.next_lba = next_lba;
        self.eod_lba = self.eod_lba.max(self.next_lba);
        Ok(WriteOutcome::from_device_position(
            buf.len() as u32,
            false,
            false,
            TapePosition {
                lba: self.next_lba,
                partition: 0,
                beginning_of_partition: false,
                end_of_partition: false,
                block_position_end_of_warning: false,
            },
        ))
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.filemarks.push(count);
        self.next_lba = self
            .next_lba
            .checked_add(count as u64)
            .expect("VecBlockSink LBA overflow on filemark");
        self.eod_lba = self.eod_lba.max(self.next_lba);
        Ok(WriteFilemarksOutcome::from_device_position(
            false,
            false,
            TapePosition {
                lba: self.next_lba,
                partition: 0,
                beginning_of_partition: false,
                end_of_partition: false,
                block_position_end_of_warning: false,
            },
        ))
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.space_to_eod_calls = self.space_to_eod_calls.saturating_add(1);
        self.next_lba = self.eod_lba;
        Ok(TapePosition {
            lba: self.next_lba,
            partition: 0,
            beginning_of_partition: self.next_lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        })
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        Ok(TapePosition {
            lba: self.next_lba,
            partition: 0,
            beginning_of_partition: self.next_lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        })
    }
}

/// In-memory [`BlockSource`] that serves blocks from an internal
/// `Vec<Vec<u8>>`. LBAs are dense (0..N); seeking past the end
/// returns a [`TapeIoError::CheckCondition`] modelled on the
/// drive's "no medium past EOD" behaviour.
#[derive(Debug)]
pub struct VecBlockSource {
    /// Block payloads indexed by LBA.
    pub blocks: Vec<Vec<u8>>,
    /// CDB-log analogue: every `locate`, `read_block`, `space`,
    /// and `position` call appended for test assertions.
    pub calls: Vec<VecBlockSourceCall>,
    /// Current LBA cursor — incremented by `read_block`, set by
    /// `locate`, adjusted by `space`.
    cursor: u64,
    read_batch_blocks: u32,
}

impl Default for VecBlockSource {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// One recorded call on a [`VecBlockSource`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VecBlockSourceCall {
    /// `read_block` called at this LBA, requested `requested`
    /// bytes; returned `returned` bytes.
    ReadBlock {
        /// LBA the cursor was at when the call landed.
        lba: u64,
        /// Caller's buffer length.
        requested: usize,
        /// Bytes actually copied (= min(block_size, requested)).
        returned: usize,
    },
    /// `read_block_batch` called at this LBA.
    ReadBlockBatch {
        /// LBA the cursor was at when the call landed.
        lba: u64,
        /// Fixed record size requested by the caller.
        block_size_bytes: u32,
        /// Record count selected by the caller.
        requested_records: u32,
        /// Data records actually copied.
        returned_records: u32,
    },
    /// `locate` called.
    Locate {
        /// Target LBA.
        target: u64,
    },
    /// `space` called.
    Space {
        /// Signed count.
        count: i64,
        /// Motion kind.
        kind: SpaceKind,
    },
    /// `position` called.
    Position,
}

impl VecBlockSource {
    /// Construct a source backed by the given blocks. Cursor
    /// starts at 0.
    pub fn new(blocks: Vec<Vec<u8>>) -> Self {
        Self {
            blocks,
            calls: Vec::new(),
            cursor: 0,
            read_batch_blocks: 1,
        }
    }

    /// Test helper that makes `read_batch_blocks` report a larger fixed
    /// record batch size.
    pub fn with_read_batch_blocks_for_test(mut self, read_batch_blocks: u32) -> Self {
        assert!(
            read_batch_blocks > 0,
            "VecBlockSource read batch size must be nonzero"
        );
        self.read_batch_blocks = read_batch_blocks;
        self
    }

    /// Current cursor LBA. Useful in tests to verify positioning
    /// without going through `BlockSource::position()`.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }
}

impl BlockSource for VecBlockSource {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let lba = self.cursor;
        let block = self.blocks.get(lba as usize).cloned();
        match block {
            Some(b) if b.len() > buf.len() => {
                // On-tape block is larger than the host buffer.
                // Per IBM §4.12.1 / Table 17 the drive consumes
                // the block (cursor advances) and returns ILI
                // CHECK CONDITION. DriveHandle::read_block
                // surfaces this as TapeIoError::ReadBufferTooSmall;
                // the fixture must match so 3b/3c tests behave
                // like production. Codex 23:26 catch.
                let actual = b.len() as u32;
                let provided = buf.len() as u32;
                self.cursor += 1;
                self.calls.push(VecBlockSourceCall::ReadBlock {
                    lba,
                    requested: buf.len(),
                    returned: 0,
                });
                Err(TapeIoError::ReadBufferTooSmall { actual, provided })
            }
            Some(b) => {
                let n = b.len();
                buf[..n].copy_from_slice(&b);
                self.cursor += 1;
                self.calls.push(VecBlockSourceCall::ReadBlock {
                    lba,
                    requested: buf.len(),
                    returned: n,
                });
                Ok(n)
            }
            None => {
                self.calls.push(VecBlockSourceCall::ReadBlock {
                    lba,
                    requested: buf.len(),
                    returned: 0,
                });
                // EOD per IBM §5.2.15 / Annex B Table B.9:
                // sense key 0x08 BLANK CHECK + ASC/ASCQ 00/05.
                Err(TapeIoError::CheckCondition(
                    remanence_scsi::ScsiError::CheckCondition {
                        sense: synth_blank_check_eod_sense(),
                        bytes_transferred: 0,
                    },
                ))
            }
        }
    }

    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<ReadBatchOutcome, TapeIoError> {
        if block_size_bytes == 0 || requested_records == 0 || remaining_records_in_file == 0 {
            return Err(TapeIoError::OperationFailed(
                "VecBlockSource read_block_batch requires nonzero block and record counts"
                    .to_string(),
            ));
        }
        let records = requested_records.min(remaining_records_in_file);
        let block_size = usize::try_from(block_size_bytes).map_err(|_| {
            TapeIoError::OperationFailed(
                "VecBlockSource read_block_batch block size exceeds usize".to_string(),
            )
        })?;
        let transfer_len = block_size.checked_mul(records as usize).ok_or_else(|| {
            TapeIoError::OperationFailed(
                "VecBlockSource read_block_batch transfer length overflow".to_string(),
            )
        })?;
        if buf.len() < transfer_len {
            return Err(TapeIoError::OperationFailed(
                "VecBlockSource read_block_batch buffer is too small".to_string(),
            ));
        }

        let start_lba = self.cursor;
        let mut bytes_read = 0usize;
        for record_index in 0..records {
            let lba = self.cursor + u64::from(record_index);
            let block = self.blocks.get(lba as usize).ok_or_else(|| {
                TapeIoError::CheckCondition(remanence_scsi::ScsiError::CheckCondition {
                    sense: synth_blank_check_eod_sense(),
                    bytes_transferred: bytes_read as u32,
                })
            })?;
            if block.len() != block_size {
                return Err(TapeIoError::OperationFailed(format!(
                    "VecBlockSource read_block_batch fixed block mismatch: expected {block_size} got {}",
                    block.len()
                )));
            }
            let end = bytes_read + block_size;
            buf[bytes_read..end].copy_from_slice(block);
            bytes_read = end;
        }

        self.cursor += u64::from(records);
        self.calls.push(VecBlockSourceCall::ReadBlockBatch {
            lba: start_lba,
            block_size_bytes,
            requested_records: records,
            returned_records: records,
        });
        Ok(ReadBatchOutcome::from_computed_position(
            records,
            bytes_read as u32,
            false,
            TapePosition {
                lba: self.cursor,
                partition: 0,
                beginning_of_partition: self.cursor == 0,
                end_of_partition: self.cursor as usize >= self.blocks.len(),
                block_position_end_of_warning: false,
            },
        ))
    }

    fn read_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.read_batch_blocks
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        // Per IBM LTO SCSI Reference §4.12.1 / Table 17 LOCATE
        // (target after EOD) returns CHECK CONDITION with sense
        // key 0x08 BLANK CHECK + ASC/ASCQ 00/05 and leaves the
        // tape at EOD. The fixture clamps the cursor to
        // blocks.len() on past-EOD so a follow-up call doesn't
        // start from an impossible LBA. Codex 23:41 idref=1343a11b
        // Medium catch on the prior commit. Three cases:
        //   - lba <  blocks.len(): normal LOCATE, cursor = lba.
        //   - lba == blocks.len(): LOCATE to EOD boundary,
        //     cursor = lba, position.end_of_partition = true.
        //   - lba >  blocks.len(): LOCATE past EOD, clamp cursor
        //     to blocks.len(), return BLANK CHECK EOD.
        self.calls.push(VecBlockSourceCall::Locate { target: lba });
        let blocks_len = self.blocks.len() as u64;
        if lba > blocks_len {
            self.cursor = blocks_len;
            return Err(TapeIoError::CheckCondition(
                remanence_scsi::ScsiError::CheckCondition {
                    sense: synth_blank_check_eod_sense(),
                    bytes_transferred: 0,
                },
            ));
        }
        self.cursor = lba;
        Ok(TapePosition {
            lba,
            partition: 0,
            beginning_of_partition: lba == 0,
            end_of_partition: lba == blocks_len,
            block_position_end_of_warning: false,
        })
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        // Per IBM §5.2.39: SPACE that hits EOD/BOP/filemark stops
        // short and reports residual via stopped_at_boundary +
        // units_traversed. The fixture clamps the cursor and
        // reports the actual move so 3b/3c tests can distinguish
        // a real boundary stop from a successful full-count move.
        // Codex 23:26 idref=25cf017f Medium catch.
        self.calls.push(VecBlockSourceCall::Space { count, kind });
        let blocks_len = self.blocks.len() as i64;
        let cursor_signed = self.cursor as i64;

        let (new_cursor_signed, units_traversed, stopped_at_boundary) = match kind {
            SpaceKind::EndOfData => {
                // EndOfData ignores count; jump straight to EOD.
                // Not a boundary stop — the operation succeeded
                // exactly as requested.
                (blocks_len, 0, false)
            }
            _ => {
                let requested_target = cursor_signed + count;
                let clamped_target = requested_target.max(0).min(blocks_len);
                let actual = clamped_target - cursor_signed;
                let stopped = clamped_target != requested_target;
                (clamped_target, actual, stopped)
            }
        };

        self.cursor = new_cursor_signed as u64;
        Ok(SpaceResult::from_device_position(
            units_traversed,
            stopped_at_boundary,
            TapePosition {
                lba: self.cursor,
                partition: 0,
                beginning_of_partition: self.cursor == 0,
                end_of_partition: self.cursor as usize >= self.blocks.len(),
                block_position_end_of_warning: false,
            },
        ))
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.calls.push(VecBlockSourceCall::Position);
        Ok(TapePosition {
            lba: self.cursor,
            partition: 0,
            beginning_of_partition: self.cursor == 0,
            end_of_partition: false,
            block_position_end_of_warning: false,
        })
    }
}

/// Synthetic fixed-format sense buffer for END-OF-DATA on the
/// data path. Per IBM LTO SCSI Reference §5.2.15 (READ(6)) and
/// Annex B Table B.9: a READ that hits EOD returns CHECK
/// CONDITION with sense key **0x08 BLANK CHECK** + ASC 0x00 /
/// ASCQ 0x05. Codex 23:26 idref=25cf017f Medium catch on the
/// earlier draft, which incorrectly used key 0 NO SENSE.
fn synth_blank_check_eod_sense() -> Vec<u8> {
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70; // fixed-format current
    sense[2] = 0x08; // sense key 0x08 BLANK CHECK (Annex B Table B.9)
    sense[7] = 24; // additional sense length
    sense[12] = 0x00; // ASC
    sense[13] = 0x05; // ASCQ — END-OF-DATA DETECTED
    sense
}

// =====================================================================
//  Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn file_sink_and_source_round_trip_fixed_blocks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("object.rao");
        let mut sink = FileBlockSink::create(&path, 4).expect("create sink");

        let first = sink.write_block(&[1, 2, 3, 4]).expect("write first");
        assert_eq!(first.bytes_written, 4);
        assert_eq!(first.position_after.lba, 1);
        let second = sink.write_block(&[5, 6, 7, 8]).expect("write second");
        assert_eq!(second.position_after.lba, 2);
        assert_eq!(sink.next_lba(), 2);
        sink.flush().expect("flush");

        let mut source = FileBlockSource::open(&path, 4).expect("open source");
        assert_eq!(source.block_size(), 4);
        assert_eq!(source.block_count(), 2);
        assert_eq!(source.len_bytes(), 8);
        assert!(!source.is_empty());

        let mut buf = [0u8; 4];
        assert_eq!(source.read_block(&mut buf).expect("read 0"), 4);
        assert_eq!(buf, [1, 2, 3, 4]);
        assert_eq!(source.read_block(&mut buf).expect("read 1"), 4);
        assert_eq!(buf, [5, 6, 7, 8]);
        let pos = source.position().expect("position");
        assert_eq!(pos.lba, 2);
        assert!(pos.end_of_partition);
    }

    #[test]
    fn file_sink_rejects_partial_blocks_without_writing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("object.rao");
        let mut sink = FileBlockSink::create(&path, 4).expect("create sink");

        let err = sink.write_block(&[1, 2, 3]).expect_err("short write");
        assert!(matches!(err, TapeIoError::OperationFailed(_)), "{err}");
        sink.flush().expect("flush");
        assert_eq!(fs::metadata(path).unwrap().len(), 0);
        assert_eq!(sink.next_lba(), 0);
    }

    #[test]
    fn file_source_rejects_unaligned_file_length() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.rao");
        fs::write(&path, [1, 2, 3]).unwrap();

        let err = FileBlockSource::open(&path, 4).expect_err("unaligned length");
        assert!(matches!(err, TapeIoError::OperationFailed(_)), "{err}");
    }

    #[test]
    fn file_source_read_buffer_too_small_consumes_block() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("object.rao");
        fs::write(&path, [1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let mut source = FileBlockSource::open(&path, 4).expect("open source");
        let mut small = [0u8; 2];

        let err = source
            .read_block(&mut small)
            .expect_err("small read buffer");
        match err {
            TapeIoError::ReadBufferTooSmall { actual, provided } => {
                assert_eq!(actual, 4);
                assert_eq!(provided, 2);
            }
            other => panic!("expected ReadBufferTooSmall, got {other:?}"),
        }
        assert_eq!(source.cursor(), 1);

        let mut buf = [0u8; 4];
        source.read_block(&mut buf).expect("read second block");
        assert_eq!(buf, [5, 6, 7, 8]);
    }

    #[test]
    fn file_source_locate_and_space_match_dense_object_lbas() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("object.rao");
        fs::write(&path, [0u8; 12]).unwrap();
        let mut source = FileBlockSource::open(&path, 4).expect("open source");

        source.locate(2).expect("locate");
        assert_eq!(source.cursor(), 2);
        let result = source.space(5, SpaceKind::Blocks).expect("space");
        assert_eq!(result.units_traversed, 1);
        assert!(result.stopped_at_boundary);
        assert_eq!(result.position_after.lba, 3);
        assert!(result.position_after.end_of_partition);

        let err = source.locate(4).expect_err("past EOD");
        assert!(matches!(err, TapeIoError::CheckCondition(_)), "{err}");
        assert_eq!(source.cursor(), 3);
    }

    #[test]
    fn file_sink_rejects_nonzero_filemarks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("object.rao");
        let mut sink = FileBlockSink::create(&path, 4).expect("create sink");

        let zero = sink.write_filemarks(0).expect("zero filemarks");
        assert_eq!(zero.position_after.lba, 0);
        let err = sink.write_filemarks(1).expect_err("filemarks");
        assert!(matches!(err, TapeIoError::OperationFailed(_)), "{err}");
    }

    #[test]
    fn vec_sink_records_blocks_in_order() {
        let mut sink = VecBlockSink::new();
        sink.write_block(&[0xAAu8; 64]).expect("write 0 ok");
        sink.write_block(&[0xBBu8; 32]).expect("write 1 ok");
        assert_eq!(sink.blocks.len(), 2);
        assert_eq!(sink.blocks[0].len(), 64);
        assert_eq!(sink.blocks[1].len(), 32);
        assert_eq!(sink.block_lbas, vec![0, 1]);
        assert_eq!(sink.next_lba(), 2);
    }

    #[test]
    fn vec_sink_filemarks_advance_position() {
        let mut sink = VecBlockSink::new();
        sink.write_block(&[0u8; 1]).unwrap();
        sink.write_filemarks(2).unwrap();
        // FM count of 2 → next_lba bumps by 2 over the prior 1
        // block write → 3.
        assert_eq!(sink.next_lba(), 3);
        assert_eq!(sink.filemarks, vec![2]);
    }

    #[test]
    fn vec_sink_write_outcome_position_after_matches_next_lba() {
        let mut sink = VecBlockSink::new();
        let outcome = sink.write_block(&[1, 2, 3]).expect("ok");
        assert_eq!(outcome.bytes_written, 3);
        assert_eq!(outcome.position_after.lba, 1);
        assert!(!outcome.early_warning);
        assert!(!outcome.end_of_medium);
    }

    #[test]
    fn vec_source_reads_blocks_at_cursor_advance() {
        let mut src = VecBlockSource::new(vec![vec![0xAA; 8], vec![0xBB; 8]]);
        let mut buf = [0u8; 8];
        let n = src.read_block(&mut buf).expect("read 0");
        assert_eq!(n, 8);
        assert_eq!(&buf, &[0xAA; 8]);
        assert_eq!(src.cursor(), 1);
        let n = src.read_block(&mut buf).expect("read 1");
        assert_eq!(n, 8);
        assert_eq!(&buf, &[0xBB; 8]);
        assert_eq!(src.cursor(), 2);
    }

    #[test]
    fn vec_source_locate_jumps_cursor() {
        let mut src = VecBlockSource::new(vec![vec![1], vec![2], vec![3]]);
        let pos = src.locate(2).expect("locate ok");
        assert_eq!(pos.lba, 2);
        assert_eq!(src.cursor(), 2);
        assert!(!pos.end_of_partition);
        let mut buf = [0u8; 1];
        src.read_block(&mut buf).expect("read 2");
        assert_eq!(buf[0], 3);
    }

    #[test]
    fn vec_source_locate_at_eod_boundary_sets_end_of_partition_flag() {
        // LOCATE to exactly blocks.len() is the EOD boundary —
        // legal, but position.end_of_partition = true.
        let mut src = VecBlockSource::new(vec![vec![1], vec![2], vec![3]]);
        let pos = src.locate(3).expect("locate to EOD ok");
        assert_eq!(pos.lba, 3);
        assert_eq!(src.cursor(), 3);
        assert!(pos.end_of_partition);
    }

    #[test]
    fn vec_source_locate_past_eod_returns_blank_check_and_clamps() {
        // Per IBM Table 17: LOCATE (target after EOD) → 8/0005,
        // position at EOD. Codex 23:41 idref=1343a11b catch.
        let mut src = VecBlockSource::new(vec![vec![1], vec![2], vec![3]]);
        let err = src.locate(100).expect_err("past-EOD locate");
        match err {
            TapeIoError::CheckCondition(remanence_scsi::ScsiError::CheckCondition {
                sense,
                ..
            }) => {
                assert_eq!(sense[2] & 0x0F, 0x08, "BLANK CHECK key");
                assert_eq!(sense[12], 0x00, "ASC");
                assert_eq!(sense[13], 0x05, "ASCQ EOD");
            }
            other => panic!("expected BLANK CHECK CheckCondition, got {other:?}"),
        }
        // Cursor clamped to EOD, NOT left at the impossible LBA
        // (would break downstream space/read accounting).
        assert_eq!(src.cursor(), 3);
    }

    #[test]
    fn vec_source_locate_past_eod_then_space_accounts_from_clamped_cursor() {
        // Regression for codex's example: after locate(100) on a
        // 5-block fixture, space(0, Blocks) should not produce
        // nonzero negative units_traversed. With the clamp,
        // cursor is at 5 (blocks.len()) and space(0) does nothing.
        let mut src = VecBlockSource::new(vec![vec![0u8; 1]; 5]);
        let _ = src.locate(100); // err, but cursor clamps
        let r = src.space(0, SpaceKind::Blocks).expect("space 0 ok");
        assert_eq!(r.units_traversed, 0);
        assert!(!r.stopped_at_boundary);
        assert_eq!(src.cursor(), 5);
    }

    #[test]
    fn vec_source_read_past_end_returns_blank_check_eod() {
        let mut src = VecBlockSource::new(vec![vec![0u8; 1]]);
        let mut buf = [0u8; 1];
        src.read_block(&mut buf).expect("read 0");
        // Next read is past the end → CHECK CONDITION with sense
        // key 0x08 BLANK CHECK + ASC/ASCQ 00/05 per IBM Annex B
        // Table B.9.
        let err = src.read_block(&mut buf).expect_err("past-EOD");
        match err {
            TapeIoError::CheckCondition(remanence_scsi::ScsiError::CheckCondition {
                sense,
                ..
            }) => {
                assert_eq!(sense[2] & 0x0F, 0x08, "sense key BLANK CHECK");
                assert_eq!(sense[12], 0x00, "ASC");
                assert_eq!(sense[13], 0x05, "ASCQ — END-OF-DATA DETECTED");
            }
            other => panic!("expected CheckCondition, got {other:?}"),
        }
    }

    #[test]
    fn vec_source_read_oversized_block_returns_read_buffer_too_small() {
        // 64-byte block on tape; caller passes 8-byte buffer.
        // Real DriveHandle::read_block returns
        // TapeIoError::ReadBufferTooSmall after consuming the
        // block. The fixture must match — codex 23:26 catch.
        let mut src = VecBlockSource::new(vec![vec![0xAA; 64]]);
        let mut buf = [0u8; 8];
        let err = src.read_block(&mut buf).expect_err("ReadBufferTooSmall");
        match err {
            TapeIoError::ReadBufferTooSmall { actual, provided } => {
                assert_eq!(actual, 64);
                assert_eq!(provided, 8);
            }
            other => panic!("expected ReadBufferTooSmall, got {other:?}"),
        }
        // Cursor MUST advance — drive consumed the block. A naive
        // retry without space(-1) would skip the block.
        assert_eq!(src.cursor(), 1);
    }

    #[test]
    fn vec_source_records_call_log_in_order() {
        let mut src = VecBlockSource::new(vec![vec![0u8; 4]]);
        let mut buf = [0u8; 4];
        src.locate(0).unwrap();
        src.read_block(&mut buf).unwrap();
        src.position().unwrap();
        assert_eq!(src.calls.len(), 3);
        assert!(matches!(
            src.calls[0],
            VecBlockSourceCall::Locate { target: 0 }
        ));
        assert!(matches!(
            src.calls[1],
            VecBlockSourceCall::ReadBlock {
                lba: 0,
                requested: 4,
                returned: 4
            }
        ));
        assert!(matches!(src.calls[2], VecBlockSourceCall::Position));
    }

    #[test]
    fn vec_source_space_end_of_data_jumps_to_end_without_boundary_stop() {
        let mut src = VecBlockSource::new(vec![vec![0u8; 1]; 5]);
        let result = src.space(0, SpaceKind::EndOfData).expect("EOD ok");
        assert_eq!(result.position_after.lba, 5);
        assert_eq!(src.cursor(), 5);
        // EndOfData isn't a "stopped short" event — it's an
        // explicit jump-to-EOD that succeeded as requested.
        assert!(!result.stopped_at_boundary);
    }

    #[test]
    fn vec_source_space_backward_past_bop_clamps_and_flags_boundary() {
        // Real drives stop at BOP with stopped_at_boundary = true
        // and report actual move via residual. Codex 23:26 catch.
        let mut src = VecBlockSource::new(vec![vec![0u8; 1]; 5]);
        src.locate(2).unwrap();
        let result = src.space(-10, SpaceKind::Blocks).expect("space ok");
        assert_eq!(result.position_after.lba, 0);
        assert_eq!(src.cursor(), 0);
        // Actual move: -2 (from LBA 2 to LBA 0), NOT the
        // requested -10.
        assert_eq!(result.units_traversed, -2);
        assert!(result.stopped_at_boundary);
    }

    #[test]
    fn vec_source_space_forward_past_eod_clamps_and_flags_boundary() {
        let mut src = VecBlockSource::new(vec![vec![0u8; 1]; 5]);
        src.locate(2).unwrap();
        let result = src.space(10, SpaceKind::Blocks).expect("space ok");
        assert_eq!(result.position_after.lba, 5);
        assert_eq!(result.units_traversed, 3, "moved 3 of requested 10");
        assert!(result.stopped_at_boundary);
    }

    #[test]
    fn vec_source_space_within_range_full_count_no_boundary() {
        let mut src = VecBlockSource::new(vec![vec![0u8; 1]; 10]);
        src.locate(2).unwrap();
        let result = src.space(3, SpaceKind::Blocks).expect("space ok");
        assert_eq!(result.position_after.lba, 5);
        assert_eq!(result.units_traversed, 3);
        assert!(!result.stopped_at_boundary);
    }
}
