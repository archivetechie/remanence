//! Shared tape-object read core for CLI break-glass reads and Layer 5 read sessions.
//!
//! The CLI still owns the hardware orchestration for `rem-debug archive read`
//! and `verify`, while the daemon session owner owns the mounted drive for
//! `ReadSessionService`. Both paths use this module to position to a native
//! object tape file and stream the single RAO payload entry without
//! materializing the object in memory.

use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use remanence_format::{
    model::{BodyLba, MANIFEST_PATH},
    plan_plaintext_rao_file_range, stream_rem_tar_object_with_manifest_anchor, FormatError,
    RemTarEntrySink, RemTarStreamEntry,
};
use remanence_library::{BlockSource, SpaceKind, SpaceResult, TapeIoError, TapePosition};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tonic::Status;

use crate::pb;

const DEFAULT_READ_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const READ_SEND_RETRY_DELAY: Duration = Duration::from_millis(10);

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
        Ok(Self {
            inner,
            block_size,
            remaining_records,
            buffer: Vec::new(),
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
        self.buffer.resize(alloc_bytes, 0);
        let outcome = self.inner.read_block_batch(
            &mut self.buffer,
            block_size_bytes,
            requested,
            remaining_u32,
        )?;
        if outcome.filemark || outcome.records_read == 0 {
            return Err(TapeIoError::OperationFailed(format!(
                "fixed read batch stopped before object boundary: records_read={} filemark={}",
                outcome.records_read, outcome.filemark
            )));
        }
        let bytes = (outcome.records_read as usize)
            .checked_mul(self.block_size)
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read batch byte count overflow".to_string())
            })?;
        self.buffer.truncate(bytes);
        self.buffered_records = outcome.records_read;
        self.remaining_records = self
            .remaining_records
            .checked_sub(u64::from(outcome.records_read))
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
    tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
    max_chunk_bytes: usize,
    send_timeout: Duration,
}

impl ChannelWriter {
    pub(crate) fn new(tx: mpsc::Sender<Result<pb::BytesChunk, Status>>) -> Self {
        Self::with_chunk_size(tx, 0)
    }

    pub(crate) fn with_chunk_size(
        tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
        chunk_bytes: usize,
    ) -> Self {
        Self {
            tx,
            max_chunk_bytes: if chunk_bytes == 0 {
                64 * 1024
            } else {
                chunk_bytes
            },
            send_timeout: DEFAULT_READ_SEND_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_chunk_size_and_timeout(
        tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
        chunk_bytes: usize,
        send_timeout: Duration,
    ) -> Self {
        let mut writer = Self::with_chunk_size(tx, chunk_bytes);
        writer.send_timeout = send_timeout;
        writer
    }

    /// Send the terminal `is_last=true` frame.
    pub(crate) fn finish(self) -> std::io::Result<()> {
        self.send_chunk(pb::BytesChunk {
            data: Vec::new(),
            is_last: true,
        })
    }

    fn send_chunk(&self, chunk: pb::BytesChunk) -> std::io::Result<()> {
        let mut item = Ok(chunk);
        let deadline = Instant::now()
            .checked_add(self.send_timeout)
            .unwrap_or_else(Instant::now);
        loop {
            match self.tx.try_send(item) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "read stream closed",
                    ));
                }
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    item = returned;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "read stream receiver stalled",
                        ));
                    }
                    thread::sleep(remaining.min(READ_SEND_RETRY_DELAY));
                }
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
        let (tx, mut rx) = mpsc::channel::<Result<pb::BytesChunk, Status>>(8);
        let handle = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let mut writer = ChannelWriter::new(tx);
            writer.write_all(b"hello").unwrap();
            writer.finish().unwrap();
        });

        let mut got = Vec::new();
        let mut saw_last = false;
        while let Some(item) = rx.recv().await {
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
        let (tx, mut rx) = mpsc::channel::<Result<pb::BytesChunk, Status>>(8);
        let handle = tokio::task::spawn_blocking(move || {
            use std::io::Write as _;
            let mut writer = ChannelWriter::with_chunk_size(tx, 3);
            writer.write_all(b"abcdefg").unwrap();
            writer.finish().unwrap();
        });

        let mut chunk_lengths = Vec::new();
        while let Some(item) = rx.recv().await {
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

        let (tx, _rx) = mpsc::channel::<Result<pb::BytesChunk, Status>>(1);
        tx.try_send(Ok(pb::BytesChunk {
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
