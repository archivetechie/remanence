//! Shared tape-object read core for CLI break-glass reads and Layer 5 read sessions.
//!
//! The CLI still owns the hardware orchestration for `rem-debug archive read`
//! and `verify`, while the daemon session owner owns the mounted drive for
//! `ReadSessionService`. Both paths use this module to position to a native
//! object tape file and stream the single RAO payload entry without
//! materializing the object in memory.

use std::io::Write;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use remanence_format::{
    model::{BodyLba, MANIFEST_PATH},
    plan_plaintext_rao_file_range, stream_rem_tar_object_with_manifest_anchor, FormatError,
    RemTarEntrySink, RemTarStreamEntry,
};
use remanence_library::{BlockSource, SpaceKind, SpaceResult, TapeIoError, TapePosition};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tonic::Status;

use crate::pb;

const DEFAULT_READ_SEND_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_READ_STREAM_CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const READ_STREAM_CHANNEL_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const READ_STREAM_CHANNEL_MAX_MESSAGES: usize = 1024;

type ReadStreamItem = Result<pb::BytesChunk, Status>;

/// Return the effective protobuf chunk size for a client request.
pub(crate) fn effective_read_stream_chunk_bytes(requested: usize) -> usize {
    if requested == 0 {
        DEFAULT_READ_STREAM_CHUNK_BYTES
    } else {
        requested
    }
}

/// Size the delivery queue from a byte budget rather than a message count.
pub(crate) fn read_stream_channel_capacity(chunk_bytes: usize) -> usize {
    READ_STREAM_CHANNEL_BYTE_BUDGET
        .checked_div(effective_read_stream_chunk_bytes(chunk_bytes))
        .unwrap_or(0)
        .clamp(1, READ_STREAM_CHANNEL_MAX_MESSAGES)
}

#[derive(Clone)]
pub(crate) struct ReadStreamSender {
    inner: Arc<ReadStreamSenderInner>,
}

struct ReadStreamSenderInner {
    tx: mpsc::Sender<ReadStreamItem>,
    watchdog: SendWatchdog,
}

impl Drop for ReadStreamSenderInner {
    fn drop(&mut self) {
        self.watchdog.shutdown();
    }
}

impl ReadStreamSender {
    #[cfg(test)]
    pub(crate) async fn send(
        &self,
        item: ReadStreamItem,
    ) -> Result<(), mpsc::error::SendError<ReadStreamItem>> {
        self.inner.tx.send(item).await
    }

    pub(crate) fn blocking_send(
        &self,
        item: ReadStreamItem,
    ) -> Result<(), mpsc::error::SendError<ReadStreamItem>> {
        self.inner.tx.blocking_send(item)
    }

    fn send_with_timeout(
        &self,
        item: ReadStreamItem,
        timeout: Duration,
    ) -> Result<Duration, BlockingReadStreamSendError> {
        let was_full = self.inner.tx.capacity() == 0;
        let started = Instant::now();
        let generation = self.inner.watchdog.arm(timeout);
        let result = self.inner.tx.blocking_send(item);
        let timed_out = self.inner.watchdog.disarm(generation);
        let stalled = if was_full {
            started.elapsed()
        } else {
            Duration::ZERO
        };
        match result {
            _ if timed_out => Err(BlockingReadStreamSendError::TimedOut(started.elapsed())),
            Ok(()) => Ok(stalled),
            Err(_) => Err(BlockingReadStreamSendError::Closed),
        }
    }
}

pub(crate) struct ReadStreamReceiver {
    rx: Arc<Mutex<mpsc::Receiver<ReadStreamItem>>>,
}

impl Stream for ReadStreamReceiver {
    type Item = ReadStreamItem;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .poll_recv(cx)
    }
}

pub(crate) fn read_stream_channel(chunk_bytes: usize) -> (ReadStreamSender, ReadStreamReceiver) {
    read_stream_channel_with_capacity(read_stream_channel_capacity(chunk_bytes))
}

fn read_stream_channel_with_capacity(capacity: usize) -> (ReadStreamSender, ReadStreamReceiver) {
    let (tx, rx) = mpsc::channel(capacity);
    let rx = Arc::new(Mutex::new(rx));
    let watchdog = SendWatchdog::new(Arc::downgrade(&rx));
    (
        ReadStreamSender {
            inner: Arc::new(ReadStreamSenderInner { tx, watchdog }),
        },
        ReadStreamReceiver { rx },
    )
}

#[derive(Debug)]
enum BlockingReadStreamSendError {
    Closed,
    TimedOut(Duration),
}

#[derive(Default)]
struct SendWatchdogState {
    next_generation: u64,
    armed: Option<(u64, Instant)>,
    timed_out: Option<u64>,
    shutdown: bool,
}

struct SendWatchdog {
    state: Arc<(Mutex<SendWatchdogState>, Condvar)>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SendWatchdog {
    fn new(receiver: Weak<Mutex<mpsc::Receiver<ReadStreamItem>>>) -> Self {
        let state = Arc::new((Mutex::new(SendWatchdogState::default()), Condvar::new()));
        let thread_state = Arc::clone(&state);
        let thread = std::thread::Builder::new()
            .name("read-stream-send-watchdog".to_string())
            .spawn(move || run_send_watchdog(thread_state, receiver))
            .expect("spawn read stream send watchdog");
        Self {
            state,
            thread: Mutex::new(Some(thread)),
        }
    }

    fn arm(&self, timeout: Duration) -> u64 {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|err| err.into_inner());
        state.next_generation = state.next_generation.wrapping_add(1);
        let generation = state.next_generation;
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        state.armed = Some((generation, deadline));
        state.timed_out = None;
        wake.notify_one();
        generation
    }

    fn disarm(&self, generation: u64) -> bool {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|err| err.into_inner());
        if matches!(state.armed, Some((armed, _)) if armed == generation) {
            state.armed = None;
        }
        let timed_out = state.timed_out == Some(generation);
        wake.notify_one();
        timed_out
    }

    fn shutdown(&self) {
        let (lock, wake) = &*self.state;
        lock.lock().unwrap_or_else(|err| err.into_inner()).shutdown = true;
        wake.notify_one();
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
        {
            let _ = thread.join();
        }
    }
}

fn run_send_watchdog(
    state: Arc<(Mutex<SendWatchdogState>, Condvar)>,
    receiver: Weak<Mutex<mpsc::Receiver<ReadStreamItem>>>,
) {
    let (lock, wake) = &*state;
    let mut guard = lock.lock().unwrap_or_else(|err| err.into_inner());
    loop {
        if guard.shutdown {
            return;
        }
        let Some((generation, deadline)) = guard.armed else {
            guard = wake.wait(guard).unwrap_or_else(|err| err.into_inner());
            continue;
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if !remaining.is_zero() {
            let (next_guard, _) = wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|err| err.into_inner());
            guard = next_guard;
            continue;
        }
        if matches!(guard.armed, Some((armed, _)) if armed == generation) {
            guard.armed = None;
            guard.timed_out = Some(generation);
            drop(guard);
            if let Some(receiver) = receiver.upgrade() {
                receiver
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .close();
            }
            guard = lock.lock().unwrap_or_else(|err| err.into_inner());
        }
    }
}

/// Position to `tape_file_number` and stream the object's payload blocks into `sink`.
///
/// The caller is responsible for mounting the tape, setting the drive block
/// size, and positioning the source at the point from which tape-file spacing
/// is defined. Current hardware callers verify the BOT bootstrap immediately
/// before this helper, matching the established CLI archive-read path.
pub fn read_object_payload(
    source: &mut dyn BlockSource,
    block_size: usize,
    block_count: u64,
    tape_file_number: u32,
    manifest_sha256: Option<[u8; 32]>,
    sink: &mut dyn RemTarEntrySink,
) -> Result<(), FormatError> {
    source.space(i64::from(tape_file_number), SpaceKind::Filemarks)?;
    let mut batched_source = BatchingBlockSource::new(source, block_size, block_count)?;
    stream_rem_tar_object_with_manifest_anchor(
        &mut batched_source,
        block_size,
        block_count,
        sink,
        manifest_sha256,
    )?;
    Ok(())
}

/// Position to one plaintext member-file range and stream only covering blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaintextFileRangeReadRequest {
    /// Fixed tape block size in bytes.
    pub block_size: usize,
    /// Filemark-delimited tape-file number containing the object.
    pub tape_file_number: u32,
    /// First object-local body block containing the member-file data.
    pub first_chunk_lba: Option<BodyLba>,
    /// Exact size of the member file.
    pub file_size_bytes: u64,
    /// Requested byte offset within the member file.
    pub range_start: u64,
    /// Requested byte count.
    pub range_len: u64,
}

/// Position to one plaintext member-file range and stream only covering blocks.
pub fn read_plaintext_file_range(
    source: &mut dyn BlockSource,
    request: PlaintextFileRangeReadRequest,
    out: &mut dyn Write,
) -> Result<(), FormatError> {
    let chunk_size_bytes = u64::try_from(request.block_size)
        .map_err(|_| FormatError::invalid("block size does not fit u64"))?;
    let plan = plan_plaintext_rao_file_range(
        request.first_chunk_lba,
        request.file_size_bytes,
        chunk_size_bytes,
        request.range_start,
        request.range_len,
    )?;
    let Some(plan) = plan else {
        return Ok(());
    };
    source.space(i64::from(request.tape_file_number), SpaceKind::Filemarks)?;
    let skip_blocks = i64::try_from(plan.first_body_lba.0)
        .map_err(|_| FormatError::invalid("range first_body_lba exceeds SPACE range"))?;
    if skip_blocks != 0 {
        source.space(skip_blocks, SpaceKind::Blocks)?;
    }

    let mut batched_source =
        BatchingBlockSource::new(source, request.block_size, plan.block_count)?;
    let mut block = vec![0u8; request.block_size];
    let first_block_offset = usize::try_from(plan.first_block_offset)
        .map_err(|_| FormatError::invalid("range first block offset does not fit usize"))?;
    let mut remaining = plan.range_len;
    for block_index in 0..plan.block_count {
        let read = batched_source.read_block(&mut block)?;
        if read != request.block_size {
            return Err(FormatError::parse(format!(
                "short range object block: expected {}, got {read}",
                request.block_size
            )));
        }
        let start = if block_index == 0 {
            first_block_offset
        } else {
            0
        };
        let available = request
            .block_size
            .checked_sub(start)
            .ok_or_else(|| FormatError::invalid("range first block offset exceeds block size"))?;
        let available_u64 = u64::try_from(available)
            .map_err(|_| FormatError::invalid("range block length does not fit u64"))?;
        let to_write = usize::try_from(remaining.min(available_u64))
            .map_err(|_| FormatError::invalid("range chunk length does not fit usize"))?;
        out.write_all(&block[start..start + to_write])
            .map_err(|source| FormatError::SourceIo {
                context: "write range".to_string(),
                source,
            })?;
        remaining -= u64::try_from(to_write)
            .map_err(|_| FormatError::invalid("range chunk length does not fit u64"))?;
    }
    if remaining != 0 {
        return Err(FormatError::parse(
            "range read ended before requested bytes were produced",
        ));
    }
    out.flush().map_err(|source| FormatError::SourceIo {
        context: "flush range".to_string(),
        source,
    })?;
    Ok(())
}

struct BatchingBlockSource<'a> {
    inner: &'a mut dyn BlockSource,
    block_size: usize,
    remaining_records: u64,
    buffer: Vec<u8>,
    free_buffers: Vec<Vec<u8>>,
    buffered_records: u32,
    next_record: u32,
}

impl<'a> BatchingBlockSource<'a> {
    fn new(
        inner: &'a mut dyn BlockSource,
        block_size: usize,
        remaining_records: u64,
    ) -> Result<Self, FormatError> {
        if block_size == 0 {
            return Err(FormatError::invalid("block size must be nonzero"));
        }
        let ring_depth = usize::try_from(inner.read_ring_buffers())
            .map_err(|_| FormatError::invalid("read staging ring depth does not fit usize"))?;
        if ring_depth < remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS as usize
            || ring_depth > remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS as usize
        {
            return Err(FormatError::invalid(format!(
                "read staging ring depth {ring_depth} is outside {}..={}",
                remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS,
                remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS
            )));
        }
        let block_size_u32 = u32::try_from(block_size)
            .map_err(|_| FormatError::invalid("read block size exceeds u32"))?;
        let batch_records = inner.read_batch_blocks(block_size_u32).max(1);
        let batch_bytes = block_size
            .checked_mul(batch_records as usize)
            .ok_or_else(|| FormatError::invalid("read staging ring buffer size overflow"))?;
        let mut ring = Vec::new();
        ring.try_reserve_exact(ring_depth)
            .map_err(|err| FormatError::invalid(format!("allocate read staging ring: {err}")))?;
        for _ in 0..ring_depth {
            let mut buffer = Vec::new();
            buffer.try_reserve_exact(batch_bytes).map_err(|err| {
                FormatError::invalid(format!("allocate read staging ring buffer: {err}"))
            })?;
            ring.push(buffer);
        }
        let buffer = ring
            .pop()
            .ok_or_else(|| FormatError::invalid("read staging ring is empty"))?;
        Ok(Self {
            inner,
            block_size,
            remaining_records,
            buffer,
            free_buffers: ring,
            buffered_records: 0,
            next_record: 0,
        })
    }

    fn refill(&mut self) -> Result<(), TapeIoError> {
        if self.next_record < self.buffered_records {
            return Ok(());
        }
        self.buffer.clear();
        self.buffered_records = 0;
        self.next_record = 0;
        if self.remaining_records == 0 {
            return Ok(());
        }
        let block_size_bytes = u32::try_from(self.block_size).map_err(|_| {
            TapeIoError::OperationFailed("read batch block size exceeds u32".to_string())
        })?;
        let requested = self
            .inner
            .read_batch_blocks(block_size_bytes)
            .max(1)
            .min(u32::try_from(self.remaining_records).unwrap_or(u32::MAX));
        let remaining_u32 = u32::try_from(self.remaining_records).unwrap_or(u32::MAX);
        let alloc_records = requested.min(remaining_u32).max(1);
        let alloc_bytes = self
            .block_size
            .checked_mul(alloc_records as usize)
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read batch buffer overflow".to_string())
            })?;
        let drained = std::mem::take(&mut self.buffer);
        self.free_buffers.push(drained);
        if self.free_buffers.is_empty() {
            return Err(TapeIoError::OperationFailed(
                "read staging ring exhausted".to_string(),
            ));
        }
        let mut ring_buffer = self.free_buffers.swap_remove(0);
        ring_buffer.resize(alloc_bytes, 0);
        let handoff = self.inner.read_buffer_handoff(
            ring_buffer,
            block_size_bytes,
            requested,
            remaining_u32,
        )?;
        if handoff.terminal_flags.filemark || handoff.records_read == 0 {
            return Err(TapeIoError::OperationFailed(format!(
                "fixed read batch stopped before object boundary: records_read={} filemark={}",
                handoff.records_read, handoff.terminal_flags.filemark
            )));
        }
        let bytes = (handoff.records_read as usize)
            .checked_mul(self.block_size)
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read batch byte count overflow".to_string())
            })?;
        if bytes != handoff.valid_bytes {
            return Err(TapeIoError::OperationFailed(format!(
                "read handoff byte/record mismatch: valid_bytes={} records_read={} block_size={}",
                handoff.valid_bytes, handoff.records_read, self.block_size
            )));
        }
        self.buffer = handoff.into_reusable_buffer();
        self.buffered_records = u32::try_from(bytes / self.block_size).map_err(|_| {
            TapeIoError::OperationFailed("read handoff record count exceeds u32".to_string())
        })?;
        self.remaining_records = self
            .remaining_records
            .checked_sub(u64::from(self.buffered_records))
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read batch remaining underflow".to_string())
            })?;
        Ok(())
    }
}

impl BlockSource for BatchingBlockSource<'_> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        self.refill()?;
        if self.next_record >= self.buffered_records {
            return self.inner.read_block(buf);
        }
        if buf.len() < self.block_size {
            return Err(TapeIoError::ReadBufferTooSmall {
                actual: self.block_size as u32,
                provided: buf.len() as u32,
            });
        }
        let start = self.next_record as usize * self.block_size;
        let end = start + self.block_size;
        buf[..self.block_size].copy_from_slice(&self.buffer[start..end]);
        self.next_record += 1;
        Ok(self.block_size)
    }

    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<remanence_library::ReadBatchOutcome, TapeIoError> {
        self.inner.read_block_batch(
            buf,
            block_size_bytes,
            requested_records,
            remaining_records_in_file,
        )
    }

    fn read_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.read_batch_blocks(block_size_bytes)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.buffer.clear();
        self.buffered_records = 0;
        self.next_record = 0;
        self.inner.locate(lba)
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        self.buffer.clear();
        self.buffered_records = 0;
        self.next_record = 0;
        self.inner.space(count, kind)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

/// Streaming sink that captures the single non-manifest payload entry.
///
/// The RAO object contains a generated manifest plus one payload file for
/// the S5a restore surface. This sink skips the manifest, writes payload bytes
/// to `out`, and hashes the bytes as they pass through.
pub struct CapturePayloadSink<W: Write> {
    out: W,
    hasher: Sha256,
    bytes_written: u64,
    capturing: bool,
    payload_entries: u32,
}

impl<W: Write> CapturePayloadSink<W> {
    /// Create a payload-capturing sink around an arbitrary `Write`.
    pub fn new(out: W) -> Self {
        Self {
            out,
            hasher: Sha256::new(),
            bytes_written: 0,
            capturing: false,
            payload_entries: 0,
        }
    }

    /// Finalize, requiring exactly one payload entry.
    pub fn finish(self) -> Result<(u64, [u8; 32]), String> {
        let (_out, bytes_written, digest) = self.finish_with_writer()?;
        Ok((bytes_written, digest))
    }

    /// Finalize and return the inner writer after flushing it.
    pub fn finish_with_writer(mut self) -> Result<(W, u64, [u8; 32]), String> {
        if self.payload_entries == 0 {
            return Err("object contains no payload entry".to_string());
        }
        if self.payload_entries > 1 {
            return Err(format!(
                "object contains {} payload entries; single-file restore only (no --path in v1)",
                self.payload_entries
            ));
        }
        self.out.flush().map_err(|e| format!("flush --out: {e}"))?;
        let digest: [u8; 32] = self.hasher.finalize().into();
        Ok((self.out, self.bytes_written, digest))
    }
}

impl<W: Write> RemTarEntrySink for CapturePayloadSink<W> {
    fn begin_file(&mut self, entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        if entry.path == MANIFEST_PATH {
            self.capturing = false;
            return Ok(());
        }
        self.payload_entries += 1;
        self.capturing = true;
        Ok(())
    }

    fn write_file_data(&mut self, bytes: &[u8]) -> Result<(), FormatError> {
        if !self.capturing {
            return Ok(());
        }
        self.hasher.update(bytes);
        self.bytes_written += bytes.len() as u64;
        self.out
            .write_all(bytes)
            .map_err(|source| FormatError::SourceIo {
                context: "write payload".to_string(),
                source,
            })?;
        Ok(())
    }

    fn end_file(&mut self, _entry: &RemTarStreamEntry) -> Result<(), FormatError> {
        self.capturing = false;
        Ok(())
    }
}

/// Synchronous writer that frames payload bytes into `ReadSessionService` chunks.
pub(crate) struct ChannelWriter {
    tx: ReadStreamSender,
    max_chunk_bytes: usize,
    send_timeout: Duration,
    sender_stall: Duration,
}

impl ChannelWriter {
    pub(crate) fn new(tx: ReadStreamSender) -> Self {
        Self::with_chunk_size(tx, 0)
    }

    pub(crate) fn with_chunk_size(tx: ReadStreamSender, chunk_bytes: usize) -> Self {
        Self {
            tx,
            max_chunk_bytes: effective_read_stream_chunk_bytes(chunk_bytes),
            send_timeout: DEFAULT_READ_SEND_TIMEOUT,
            sender_stall: Duration::ZERO,
        }
    }

    #[cfg(test)]
    fn with_chunk_size_and_timeout(
        tx: ReadStreamSender,
        chunk_bytes: usize,
        send_timeout: Duration,
    ) -> Self {
        let mut writer = Self::with_chunk_size(tx, chunk_bytes);
        writer.send_timeout = send_timeout;
        writer
    }

    /// Send the terminal `is_last=true` frame.
    pub(crate) fn finish(&mut self) -> std::io::Result<()> {
        self.send_chunk(pb::BytesChunk {
            data: Vec::new(),
            is_last: true,
        })
    }

    pub(crate) fn sender_stall(&self) -> Duration {
        self.sender_stall
    }

    fn send_chunk(&mut self, chunk: pb::BytesChunk) -> std::io::Result<()> {
        match self.tx.send_with_timeout(Ok(chunk), self.send_timeout) {
            Ok(stalled) => {
                self.sender_stall = self.sender_stall.saturating_add(stalled);
                Ok(())
            }
            Err(BlockingReadStreamSendError::Closed) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "read stream closed",
            )),
            Err(BlockingReadStreamSendError::TimedOut(stalled)) => {
                self.sender_stall = self.sender_stall.saturating_add(stalled);
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "read stream receiver stalled",
                ))
            }
        }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            self.send_chunk(pb::BytesChunk {
                data: Vec::new(),
                is_last: false,
            })?;
            return Ok(0);
        }
        for chunk in buf.chunks(self.max_chunk_bytes) {
            self.send_chunk(pb::BytesChunk {
                data: chunk.to_vec(),
                is_last: false,
            })?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use remanence_format::{
        write_rem_tar_object, RemTarEntrySink, RemTarEntryType, RemTarFile, RemTarObjectOptions,
        RemTarStreamEntry,
    };
    use remanence_library::{VecBlockSink, VecBlockSource, VecBlockSourceCall};
    use sha2::{Digest, Sha256};
    use tokio_stream::StreamExt;

    use super::*;

    fn stream_entry(path: &str) -> RemTarStreamEntry {
        RemTarStreamEntry {
            entry_type: RemTarEntryType::Regular,
            path: path.to_string(),
            size_bytes: 0,
            link_target: None,
            first_chunk_lba: None,
            chunk_count: 0,
            data_offset: 0,
            pax_records: std::collections::BTreeMap::new(),
            xattrs: std::collections::BTreeMap::new(),
        }
    }

    fn options(chunk_size: usize) -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            "55555555-5555-5555-5555-555555555555",
            "caller-reader",
            "2026-05-27T22:10:00+05:30",
            "66666666-6666-6666-6666-666666666666",
        );
        opts.chunk_size = chunk_size;
        opts
    }

    #[test]
    fn capture_payload_sink_extracts_single_entry_and_hashes() {
        let mut buf: Vec<u8> = Vec::new();
        let mut sink = CapturePayloadSink::new(&mut buf);

        let manifest = stream_entry(MANIFEST_PATH);
        sink.begin_file(&manifest).unwrap();
        sink.write_file_data(b"CBORCBOR").unwrap();
        sink.end_file(&manifest).unwrap();

        let file = stream_entry("hello.txt");
        sink.begin_file(&file).unwrap();
        sink.write_file_data(b"hel").unwrap();
        sink.write_file_data(b"lo").unwrap();
        sink.end_file(&file).unwrap();

        let (bytes_written, digest) = sink.finish().expect("finish");
        assert_eq!(bytes_written, 5);
        assert_eq!(buf, b"hello");
        let expected: [u8; 32] = Sha256::digest(b"hello").into();
        assert_eq!(digest, expected);
    }

    #[test]
    fn capture_payload_sink_rejects_zero_and_multiple_entries() {
        let mut buf0: Vec<u8> = Vec::new();
        let mut sink0 = CapturePayloadSink::new(&mut buf0);
        let manifest = stream_entry(MANIFEST_PATH);
        sink0.begin_file(&manifest).unwrap();
        sink0.end_file(&manifest).unwrap();
        assert!(sink0.finish().is_err());

        let mut buf2: Vec<u8> = Vec::new();
        let mut sink2 = CapturePayloadSink::new(&mut buf2);
        for name in ["a.txt", "b.txt"] {
            let e = stream_entry(name);
            sink2.begin_file(&e).unwrap();
            sink2.write_file_data(b"x").unwrap();
            sink2.end_file(&e).unwrap();
        }
        assert!(sink2.finish().is_err());
    }

    #[tokio::test]
    async fn channel_writer_frames_and_streams() {
        let (tx, mut rx) = read_stream_channel_with_capacity(8);
        let handle = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let mut writer = ChannelWriter::new(tx);
            writer.write_all(b"hello").unwrap();
            writer.finish().unwrap();
        });

        let mut got = Vec::new();
        let mut saw_last = false;
        while let Some(item) = rx.next().await {
            let chunk = item.unwrap();
            got.extend_from_slice(&chunk.data);
            saw_last |= chunk.is_last;
        }
        handle.await.unwrap();
        assert_eq!(got, b"hello");
        assert!(saw_last, "stream must end with an is_last chunk");
    }

    #[tokio::test]
    async fn channel_writer_honors_requested_chunk_size() {
        let (tx, mut rx) = read_stream_channel_with_capacity(8);
        let handle = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let mut writer = ChannelWriter::with_chunk_size(tx, 3);
            writer.write_all(b"abcdefg").unwrap();
            writer.finish().unwrap();
        });

        let mut chunk_lengths = Vec::new();
        while let Some(item) = rx.next().await {
            let chunk = item.unwrap();
            if !chunk.is_last {
                chunk_lengths.push(chunk.data.len());
            }
        }
        handle.await.unwrap();
        assert_eq!(chunk_lengths, [3, 3, 1]);
    }

    #[test]
    fn channel_writer_times_out_when_receiver_stalls() {
        use std::io::Write as _;

        let (tx, _rx) = read_stream_channel_with_capacity(1);
        tx.blocking_send(Ok(pb::BytesChunk {
            data: b"held".to_vec(),
            is_last: false,
        }))
        .expect("fill channel");
        let mut writer =
            ChannelWriter::with_chunk_size_and_timeout(tx, 3, Duration::from_millis(1));

        let err = writer
            .write_all(b"abc")
            .expect_err("stalled receiver times out");

        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            writer.sender_stall() >= Duration::from_millis(1),
            "timed-out full-channel wait must be observable"
        );
    }

    #[test]
    fn channel_writer_reports_broken_pipe_for_closed_receiver() {
        use std::io::Write as _;

        let (tx, rx) = read_stream_channel_with_capacity(1);
        drop(rx);
        let mut writer = ChannelWriter::with_chunk_size(tx, 3);
        let err = writer
            .write_all(b"abc")
            .expect_err("closed receiver must fail the write");

        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn default_chunk_and_channel_capacity_follow_byte_budget() {
        assert_eq!(
            effective_read_stream_chunk_bytes(0),
            DEFAULT_READ_STREAM_CHUNK_BYTES
        );
        assert_eq!(DEFAULT_READ_STREAM_CHUNK_BYTES, 256 * 1024);
        for chunk_bytes in [64 * 1024, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
            let capacity = read_stream_channel_capacity(chunk_bytes);
            assert!(capacity >= 1);
            assert!(
                capacity * chunk_bytes <= READ_STREAM_CHANNEL_BYTE_BUDGET,
                "chunk={chunk_bytes} capacity={capacity} must honor byte budget"
            );
        }
        assert_eq!(
            read_stream_channel_capacity(READ_STREAM_CHANNEL_BYTE_BUDGET + 1),
            1,
            "an oversized requested chunk still gets exactly one queue slot"
        );
        assert_eq!(
            read_stream_channel_capacity(1),
            READ_STREAM_CHANNEL_MAX_MESSAGES,
            "tiny chunks must not turn the byte budget into millions of queue slots"
        );
    }

    #[test]
    fn blocking_send_tracks_slow_drain_without_ten_millisecond_quantization() {
        use std::io::Write as _;

        const CHUNKS: usize = 40;
        let (tx, mut rx) = read_stream_channel_with_capacity(1);
        let drain = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build slow-drain runtime");
            runtime.block_on(async move {
                for _ in 0..CHUNKS {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    rx.next().await.expect("writer keeps channel open").unwrap();
                }
            });
        });
        let mut writer = ChannelWriter::with_chunk_size(tx, 1);
        let started = Instant::now();
        writer
            .write_all(&[0xA5; CHUNKS])
            .expect("slow drain remains live");
        let elapsed = started.elapsed();
        drain.join().expect("slow-drain thread joins");

        assert!(writer.sender_stall() >= Duration::from_millis(20));
        assert!(
            elapsed < Duration::from_millis(250),
            "{CHUNKS} chunks took {elapsed:?}; a 10 ms retry quantum would take about 400 ms"
        );
    }

    #[test]
    fn sender_stall_is_zero_when_channel_never_fills() {
        use std::io::Write as _;

        let (tx, _rx) = read_stream_channel_with_capacity(8);
        let mut writer = ChannelWriter::with_chunk_size(tx, 1);
        writer.write_all(b"fast").expect("queue has spare capacity");
        assert_eq!(writer.sender_stall(), Duration::ZERO);
    }

    #[test]
    fn read_object_payload_streams_rem_tar_payload_from_block_source() {
        let opts = options(4096);
        let files = [RemTarFile {
            path: "hello.txt",
            file_id: "file-a",
            data: b"hello from tape",
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut block_sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut block_sink, &opts, &files).unwrap();
        let mut source = VecBlockSource::new(block_sink.blocks);
        let mut payload = Vec::new();
        let mut sink = CapturePayloadSink::new(&mut payload);

        read_object_payload(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            0,
            None,
            &mut sink,
        )
        .unwrap();

        let (bytes_written, digest) = sink.finish().unwrap();
        assert_eq!(bytes_written, b"hello from tape".len() as u64);
        assert_eq!(payload, b"hello from tape");
        let expected: [u8; 32] = Sha256::digest(b"hello from tape").into();
        assert_eq!(digest, expected);
    }

    #[test]
    fn read_object_payload_refills_with_batched_reads() {
        let opts = options(512);
        let payload = (0..7000)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect::<Vec<_>>();
        let files = [RemTarFile {
            path: "payload.bin",
            file_id: "file-payload",
            data: payload.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut block_sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut block_sink, &opts, &files).unwrap();
        let mut source = VecBlockSource::new(block_sink.blocks).with_read_batch_blocks_for_test(4);
        let mut restored = Vec::new();
        let mut sink = CapturePayloadSink::new(&mut restored);

        read_object_payload(
            &mut source,
            opts.chunk_size,
            layout.projected_size_blocks,
            0,
            None,
            &mut sink,
        )
        .unwrap();

        let (bytes_written, digest) = sink.finish().unwrap();
        assert_eq!(bytes_written, payload.len() as u64);
        assert_eq!(restored, payload);
        let expected: [u8; 32] = Sha256::digest(&payload).into();
        assert_eq!(digest, expected);
        assert!(
            source.calls.iter().any(|call| matches!(
                call,
                VecBlockSourceCall::ReadBlockBatch {
                    requested_records,
                    ..
                } if *requested_records > 1
            )),
            "read core must use the batched BlockSource primitive: {:?}",
            source.calls
        );
    }

    #[test]
    fn chaos_process_loss_discards_unconsumed_read_ring_without_extra_data_command() {
        let blocks = (0u8..8).map(|value| vec![value; 4]).collect::<Vec<_>>();
        let mut source = VecBlockSource::new(blocks).with_read_batch_blocks_for_test(4);
        {
            let mut batched = BatchingBlockSource::new(&mut source, 4, 8).expect("open read ring");
            let mut record = [0xa5; 4];
            batched
                .read_block(&mut record)
                .expect("consume one record before injected process loss");
            assert_eq!(record, [0; 4]);
            // Dropping here models every read-side crash-table point after the
            // completed CDB: typed handoffs and unused ring slots are
            // process-local and cannot trigger a destructor-side READ.
        }

        let reads = source
            .calls
            .iter()
            .filter(|call| matches!(call, VecBlockSourceCall::ReadBlockBatch { .. }))
            .count();
        assert_eq!(
            reads, 1,
            "process loss must discard staged buffers without issuing another READ"
        );
    }

    #[test]
    fn read_plaintext_file_range_streams_only_requested_bytes() {
        let opts = options(512);
        let payload = (0..1600)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect::<Vec<_>>();
        let files = [RemTarFile {
            path: "camera.raw",
            file_id: "file-camera",
            data: payload.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut block_sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut block_sink, &opts, &files).unwrap();
        let mut source = VecBlockSource::new(block_sink.blocks);
        let mut range = Vec::new();

        read_plaintext_file_range(
            &mut source,
            PlaintextFileRangeReadRequest {
                block_size: opts.chunk_size,
                tape_file_number: 0,
                first_chunk_lba: layout.files[0].first_chunk_lba,
                file_size_bytes: u64::try_from(payload.len()).unwrap(),
                range_start: 400,
                range_len: 700,
            },
            &mut range,
        )
        .unwrap();

        assert_eq!(range, payload[400..1100]);
    }
}
