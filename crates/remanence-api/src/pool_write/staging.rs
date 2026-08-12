//! Bounded staging rings, pipelined transfer execution, counters, and diagnostics.

use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc as std_mpsc, Arc, Mutex,
};
use std::time::{Duration, Instant};

use remanence_library::{
    BlockSink, PipelinedWriteDiagnostics, TapeIoError, TapePosition, WriteBatchOutcome,
    WriteFilemarksOutcome, WriteOutcome,
};
use remanence_state::CatalogIndex;
use sha2::{Digest, Sha256};

use super::model::{PoolWriteError, SelectedTape};
use super::no_parity::{
    record_tape_io_fence_for_transfer_error, tape_io_fence_reason_for_transfer_error,
};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockSinkStats {
    pub(crate) block_write_calls: u64,
    pub(crate) block_write_bytes: u64,
    pub(crate) min_block_bytes: Option<u64>,
    pub(crate) max_block_bytes: Option<u64>,
    pub(crate) filemark_calls: u64,
    pub(crate) filemarks: u64,
    pub(crate) filemark_write_drain: Duration,
    pub(crate) position_calls: u64,
    pub(crate) early_warning: bool,
    pub(crate) write_batch_blocks: u32,
    pub(crate) effective_batch_blocks: u32,
    pub(crate) position_check_bytes: u64,
    pub(crate) staging_ring_buffers: u32,
    pub(crate) staging_wait_samples: u64,
    pub(crate) staging_wait_p50_us: u64,
    pub(crate) staging_wait_p95_us: u64,
    pub(crate) staging_wait_max_us: u64,
    pub(crate) staging_wait_mean_us: u64,
    pub(crate) refill_samples: u64,
    pub(crate) refill_p50_us: u64,
    pub(crate) refill_p95_us: u64,
    pub(crate) refill_max_us: u64,
    pub(crate) refill_mean_us: u64,
    pub(crate) gap_samples: u64,
    pub(crate) gap_p50_us: u64,
    pub(crate) gap_p95_us: u64,
    pub(crate) gap_max_us: u64,
    pub(crate) gap_mean_us: u64,
    pub(crate) ioctl_samples: u64,
    pub(crate) ioctl_p50_us: u64,
    pub(crate) ioctl_p95_us: u64,
    pub(crate) ioctl_max_us: u64,
    pub(crate) ioctl_mean_us: u64,
    pub(crate) first_60s_ioctl_samples: u64,
    pub(crate) first_60s_ioctl_p50_us: u64,
    pub(crate) first_60s_ioctl_p95_us: u64,
    pub(crate) first_60s_ioctl_max_us: u64,
    pub(crate) first_60s_ioctl_mean_us: u64,
    pub(crate) accounting_samples: u64,
    pub(crate) accounting_p50_us: u64,
    pub(crate) accounting_p95_us: u64,
    pub(crate) accounting_max_us: u64,
    pub(crate) accounting_mean_us: u64,
    pub(crate) cadence_us: u64,
    pub(crate) effective_feed_bytes_per_second: u64,
    pub(crate) time_to_first_ioctl_ms: u64,
    pub(crate) steady_reached: bool,
    pub(crate) time_to_steady_ms: u64,
    pub(crate) steady_window_seconds: u32,
    pub(crate) steady_threshold_percent: u32,
    pub(crate) ramp_observation_seconds: u32,
}

impl BlockSinkStats {
    pub(crate) fn record_block(&mut self, bytes: u64, early_warning: bool) {
        self.block_write_calls = self.block_write_calls.saturating_add(1);
        self.block_write_bytes = self.block_write_bytes.saturating_add(bytes);
        self.early_warning |= early_warning;
        self.min_block_bytes = Some(
            self.min_block_bytes
                .map_or(bytes, |current| current.min(bytes)),
        );
        self.max_block_bytes = Some(
            self.max_block_bytes
                .map_or(bytes, |current| current.max(bytes)),
        );
    }

    pub(crate) fn record_filemarks(&mut self, count: u32, early_warning: bool, elapsed: Duration) {
        self.filemark_calls = self.filemark_calls.saturating_add(1);
        self.filemarks = self.filemarks.saturating_add(u64::from(count));
        self.filemark_write_drain = self.filemark_write_drain.saturating_add(elapsed);
        self.early_warning |= early_warning;
    }

    pub(crate) fn record_position(&mut self, position: TapePosition) {
        self.position_calls = self.position_calls.saturating_add(1);
        self.early_warning |= position.block_position_end_of_warning;
    }

    pub(crate) fn record_staging(&mut self, diagnostics: &StagingPhaseDiagnostics) {
        self.staging_wait_samples = diagnostics.wait_us.samples;
        self.staging_wait_p50_us = diagnostics.wait_us.percentile(50, 100);
        self.staging_wait_p95_us = diagnostics.wait_us.percentile(95, 100);
        self.staging_wait_max_us = diagnostics.wait_us.max_us;
        self.staging_wait_mean_us = diagnostics.wait_us.mean();
        self.refill_samples = diagnostics.refill_us.samples;
        self.refill_p50_us = diagnostics.refill_us.percentile(50, 100);
        self.refill_p95_us = diagnostics.refill_us.percentile(95, 100);
        self.refill_max_us = diagnostics.refill_us.max_us;
        self.refill_mean_us = diagnostics.refill_us.mean();
    }
}

pub(crate) struct LiveCounterBlockSink<'a> {
    pub(crate) inner: &'a mut dyn BlockSink,
    pub(crate) live_write_counter: Arc<crate::DriveByteCounters>,
}

pub(crate) struct CountingBlockSink<'a, S: BlockSink + ?Sized> {
    pub(crate) inner: &'a mut S,
    pub(crate) stats: BlockSinkStats,
}

pub(crate) struct ObjectDigestBlockSink<'a, S: BlockSink + ?Sized> {
    pub(crate) inner: &'a mut S,
    pub(crate) hasher: Sha256,
}

#[derive(Clone, Copy)]
pub(crate) struct StagedSinkCaps {
    pub(crate) block_size: usize,
    pub(crate) batch_blocks: u32,
    pub(crate) requested_write_batch_blocks: u32,
    pub(crate) position_check_bytes: u64,
}

impl StagedSinkCaps {
    pub(crate) fn from_inner<S: BlockSink + ?Sized>(inner: &S, block_size: usize) -> Self {
        let block_size_u32 = u32::try_from(block_size).unwrap_or(u32::MAX);
        let batch_blocks = inner.write_batch_blocks(block_size_u32).max(1);
        Self {
            block_size,
            batch_blocks,
            requested_write_batch_blocks: inner.requested_write_batch_blocks().max(1),
            position_check_bytes: inner.position_check_bytes(),
        }
    }
}

pub(crate) const MAX_PIPELINE_WINDOW_BUFFERS: usize =
    remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS as usize;

#[derive(Default)]
pub(crate) struct RingAccounting {
    pub(crate) allocated: AtomicU32,
    pub(crate) dropped: AtomicU32,
}

pub(crate) const HOT_PHASE_HISTOGRAM_UPPER_US: [u64; 12] = [
    10,
    25,
    50,
    100,
    250,
    500,
    1_000,
    2_500,
    5_000,
    10_000,
    50_000,
    u64::MAX,
];

#[derive(Default)]
pub(crate) struct HotPhaseHistogram {
    pub(crate) buckets: [u64; HOT_PHASE_HISTOGRAM_UPPER_US.len()],
    pub(crate) samples: u64,
    pub(crate) sum_us: u64,
    pub(crate) max_us: u64,
}

impl HotPhaseHistogram {
    pub(crate) fn record(&mut self, duration: Duration) {
        let sample_us = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let bucket = HOT_PHASE_HISTOGRAM_UPPER_US
            .iter()
            .position(|upper| sample_us <= *upper)
            .unwrap_or(HOT_PHASE_HISTOGRAM_UPPER_US.len() - 1);
        self.buckets[bucket] = self.buckets[bucket].saturating_add(1);
        self.samples = self.samples.saturating_add(1);
        self.sum_us = self.sum_us.saturating_add(sample_us);
        self.max_us = self.max_us.max(sample_us);
    }

    pub(crate) fn percentile(&self, numerator: u64, denominator: u64) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let wanted = self
            .samples
            .saturating_mul(numerator)
            .saturating_add(denominator - 1)
            / denominator;
        let mut seen = 0u64;
        for (count, upper) in self.buckets.iter().zip(HOT_PHASE_HISTOGRAM_UPPER_US) {
            seen = seen.saturating_add(*count);
            if seen >= wanted {
                return if upper == u64::MAX {
                    self.max_us
                } else {
                    upper
                };
            }
        }
        self.max_us
    }

    pub(crate) fn mean(&self) -> u64 {
        self.sum_us.checked_div(self.samples.max(1)).unwrap_or(0)
    }
}

#[derive(Default)]
pub(crate) struct StagingPhaseDiagnostics {
    pub(crate) wait_us: HotPhaseHistogram,
    pub(crate) refill_us: HotPhaseHistogram,
}

pub(crate) struct PageAlignedBuffer {
    pub(crate) storage: Vec<u8>,
    pub(crate) start: usize,
    pub(crate) capacity: usize,
    pub(crate) used: usize,
    pub(crate) refill_elapsed: Duration,
    pub(crate) accounting: Arc<RingAccounting>,
}

impl PageAlignedBuffer {
    pub(crate) fn try_new(
        capacity: usize,
        accounting: Arc<RingAccounting>,
    ) -> Result<Self, TapeIoError> {
        let page_alignment = system_page_size();
        let allocation_bytes = capacity
            .checked_add(page_alignment - 1)
            .ok_or_else(|| TapeIoError::OperationFailed("staging buffer size overflow".into()))?;
        let mut storage = Vec::new();
        storage.try_reserve_exact(allocation_bytes).map_err(|err| {
            TapeIoError::OperationFailed(format!(
                "failed to allocate page-aligned staging buffer: {err}"
            ))
        })?;
        storage.resize(allocation_bytes, 0);
        let address = storage.as_ptr() as usize;
        let start = (page_alignment - (address % page_alignment)) % page_alignment;
        debug_assert_eq!((address + start) % page_alignment, 0);
        debug_assert!(start + capacity <= storage.len());
        accounting.allocated.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            storage,
            start,
            capacity,
            used: 0,
            refill_elapsed: Duration::ZERO,
            accounting,
        })
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) -> Result<(), TapeIoError> {
        let end = self
            .used
            .checked_add(bytes.len())
            .ok_or_else(|| TapeIoError::OperationFailed("staging buffer cursor overflow".into()))?;
        if end > self.capacity {
            return Err(TapeIoError::OperationFailed(
                "staging buffer exceeded fixed batch capacity".into(),
            ));
        }
        let destination = self.start + self.used..self.start + end;
        self.storage[destination].copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.storage[self.start..self.start + self.used]
    }

    pub(crate) fn is_full(&self) -> bool {
        self.used == self.capacity
    }

    pub(crate) fn reset(&mut self) {
        self.used = 0;
        self.refill_elapsed = Duration::ZERO;
    }
}

pub(crate) fn system_page_size() -> usize {
    // SAFETY: sysconf(_SC_PAGESIZE) takes no pointers and has no memory side
    // effects. A non-positive result falls back to a conservative 4 KiB.
    let reported = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(reported)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(4096)
}

impl Drop for PageAlignedBuffer {
    fn drop(&mut self) {
        self.accounting.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) struct PipelinedBatch {
    pub(crate) buffer: PageAlignedBuffer,
    pub(crate) cdb: [u8; 6],
    pub(crate) records: u32,
    pub(crate) block_size_bytes: u32,
}

pub(crate) struct PipelinedWindow {
    pub(crate) batches: [Option<PipelinedBatch>; MAX_PIPELINE_WINDOW_BUFFERS],
    pub(crate) len: usize,
    pub(crate) bytes: u64,
}

impl PipelinedWindow {
    pub(crate) fn new() -> Self {
        Self {
            batches: std::array::from_fn(|_| None),
            len: 0,
            bytes: 0,
        }
    }

    pub(crate) fn push(&mut self, batch: PipelinedBatch) -> Result<(), TapeIoError> {
        let slot = self.batches.get_mut(self.len).ok_or_else(|| {
            TapeIoError::OperationFailed("pipelined window exceeded ring depth".into())
        })?;
        self.bytes = self
            .bytes
            .checked_add(batch.buffer.used as u64)
            .ok_or_else(|| TapeIoError::OperationFailed("pipelined window byte overflow".into()))?;
        *slot = Some(batch);
        self.len += 1;
        Ok(())
    }

    pub(crate) fn first_records(&self) -> u32 {
        self.batches[0]
            .as_ref()
            .expect("non-empty window has first batch")
            .records
    }

    pub(crate) fn last_records(&self) -> u32 {
        self.batches[self.len - 1]
            .as_ref()
            .expect("non-empty window has last batch")
            .records
    }
}

// The fixed-size window is intentionally inline: boxing it would add one heap
// allocation per staging window and violate the steady-state allocation rule.
#[allow(clippy::large_enum_variant)]
pub(crate) enum PipelinedSinkCommand {
    WriteWindow(PipelinedWindow),
    Barrier {
        reply: std_mpsc::Sender<Result<Option<WriteBatchOutcome>, String>>,
    },
    WriteFilemarks {
        count: u32,
        reply: std_mpsc::Sender<Result<WriteFilemarksOutcome, String>>,
    },
    WriteFilemarksImmediate {
        count: u32,
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    SpaceToEndOfData {
        reply: std_mpsc::Sender<Result<TapePosition, String>>,
    },
    Locate {
        lba: u64,
        reply: std_mpsc::Sender<Result<TapePosition, String>>,
    },
    Position {
        reply: std_mpsc::Sender<Result<TapePosition, String>>,
    },
}

pub(crate) struct StagedBlockSink {
    pub(crate) tx: std_mpsc::SyncSender<PipelinedSinkCommand>,
    pub(crate) free_rx: std_mpsc::Receiver<PageAlignedBuffer>,
    pub(crate) submitter_done_rx: std_mpsc::Receiver<()>,
    pub(crate) poison: Arc<Mutex<Option<String>>>,
    pub(crate) caps: StagedSinkCaps,
    pub(crate) ring_buffers: usize,
    pub(crate) current: Option<PageAlignedBuffer>,
    pub(crate) window: PipelinedWindow,
    pub(crate) cursor: Option<TapePosition>,
    pub(crate) diagnostics: StagingPhaseDiagnostics,
}

impl<'a, S: BlockSink + ?Sized> ObjectDigestBlockSink<'a, S> {
    pub(crate) fn new(inner: &'a mut S) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
        }
    }

    pub(crate) fn finish_digest(self) -> [u8; 32] {
        let digest = self.hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        out
    }
}

impl<S: BlockSink + ?Sized> BlockSink for ObjectDigestBlockSink<'_, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.hasher.update(buf);
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.hasher.update(buf);
        Ok(outcome)
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }
}

impl StagedBlockSink {
    pub(crate) fn new(
        tx: std_mpsc::SyncSender<PipelinedSinkCommand>,
        free_rx: std_mpsc::Receiver<PageAlignedBuffer>,
        submitter_done_rx: std_mpsc::Receiver<()>,
        poison: Arc<Mutex<Option<String>>>,
        caps: StagedSinkCaps,
        ring_buffers: usize,
    ) -> Self {
        Self {
            tx,
            free_rx,
            submitter_done_rx,
            poison,
            caps,
            ring_buffers,
            current: None,
            window: PipelinedWindow::new(),
            cursor: None,
            diagnostics: StagingPhaseDiagnostics::default(),
        }
    }

    pub(crate) fn check_poison(&self) -> Result<(), TapeIoError> {
        if let Some(message) = staged_poison_message(&self.poison) {
            Err(TapeIoError::OperationFailed(format!(
                "pipelined transfer poisoned after sink error: {message}"
            )))
        } else {
            Ok(())
        }
    }

    pub(crate) fn acquire_buffer(&mut self) -> Result<(), TapeIoError> {
        if self.current.is_some() {
            return Ok(());
        }
        self.check_poison()?;
        let wait_started = Instant::now();
        let received = self.free_rx.recv();
        self.diagnostics.wait_us.record(wait_started.elapsed());
        let buffer = received.map_err(|_| {
            TapeIoError::OperationFailed("pipelined staging ring was closed".into())
        })?;
        self.current = Some(buffer);
        Ok(())
    }

    pub(crate) fn finish_current_batch(&mut self) -> Result<(), TapeIoError> {
        let Some(buffer) = self.current.take() else {
            return Ok(());
        };
        if buffer.used == 0 {
            self.current = Some(buffer);
            return Ok(());
        }
        self.diagnostics.refill_us.record(buffer.refill_elapsed);
        let block_size_bytes = u32::try_from(self.caps.block_size)
            .map_err(|_| TapeIoError::OperationFailed("batch block size exceeds u32".into()))?;
        let records = records_in_staged_batch(buffer.bytes(), block_size_bytes)?;
        let cdb = remanence_scsi::read_write::build_write_fixed_cdb(records);
        self.window.push(PipelinedBatch {
            buffer,
            cdb,
            records,
            block_size_bytes,
        })?;
        if self.window.len == self.ring_buffers {
            self.send_window()?;
        }
        Ok(())
    }

    pub(crate) fn send_window(&mut self) -> Result<(), TapeIoError> {
        if self.window.len == 0 {
            return Ok(());
        }
        self.check_poison()?;
        let window = std::mem::replace(&mut self.window, PipelinedWindow::new());
        self.tx
            .send(PipelinedSinkCommand::WriteWindow(window))
            .map_err(|_| TapeIoError::OperationFailed("pipelined submitter stopped".into()))?;
        self.check_poison()
    }

    pub(crate) fn flush_pending(&mut self) -> Result<(), TapeIoError> {
        self.finish_current_batch()?;
        self.send_window()
    }

    pub(crate) fn request<T>(
        &self,
        build: impl FnOnce(std_mpsc::Sender<Result<T, String>>) -> PipelinedSinkCommand,
    ) -> Result<T, TapeIoError> {
        self.check_poison()?;
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| TapeIoError::OperationFailed("pipelined submitter stopped".into()))?;
        match reply_rx.recv() {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(TapeIoError::OperationFailed(format!(
                "pipelined submitter error: {message}"
            ))),
            Err(_) => Err(TapeIoError::OperationFailed(
                "pipelined submitter dropped reply".into(),
            )),
        }
    }

    pub(crate) fn seed_cursor(&mut self) -> Result<TapePosition, TapeIoError> {
        if let Some(position) = self.cursor {
            return Ok(position);
        }
        let position = self.request(|reply| PipelinedSinkCommand::Position { reply })?;
        self.cursor = Some(position);
        Ok(position)
    }

    pub(crate) fn advance_cursor(&mut self, records: u32) -> Result<TapePosition, TapeIoError> {
        let before = self.seed_cursor()?;
        let lba = before
            .lba
            .checked_add(u64::from(records))
            .ok_or_else(|| TapeIoError::OperationFailed("batch position overflow".into()))?;
        let position = TapePosition {
            lba,
            partition: before.partition,
            beginning_of_partition: lba == 0,
            end_of_partition: false,
            block_position_end_of_warning: before.block_position_end_of_warning,
        };
        self.cursor = Some(position);
        Ok(position)
    }

    pub(crate) fn finish(mut self) -> (Result<(), TapeIoError>, StagingPhaseDiagnostics) {
        let result = self.flush_pending().and_then(|()| {
            self.request(|reply| PipelinedSinkCommand::Barrier { reply })
                .map(|_| ())
        });
        (result, self.diagnostics)
    }

    pub(crate) fn abort(self, message: String) -> StagingPhaseDiagnostics {
        set_staged_poison(&self.poison, message);
        let StagedBlockSink {
            tx,
            free_rx,
            submitter_done_rx,
            diagnostics,
            ..
        } = self;
        drop(tx);
        let _free_rx = free_rx;
        let _ = submitter_done_rx.recv();
        diagnostics
    }
}

impl BlockSink for StagedBlockSink {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        if buf.len() != self.caps.block_size {
            return Err(TapeIoError::OperationFailed(format!(
                "pipelined fixed write requires {}-byte records, got {}",
                self.caps.block_size,
                buf.len()
            )));
        }
        self.acquire_buffer()?;
        let refill_started = Instant::now();
        let append_result = self.current.as_mut().expect("buffer acquired").append(buf);
        let refill_elapsed = refill_started.elapsed();
        self.current
            .as_mut()
            .expect("buffer remains acquired")
            .refill_elapsed += refill_elapsed;
        append_result?;
        let position = self.advance_cursor(1)?;
        if self
            .current
            .as_ref()
            .is_some_and(PageAlignedBuffer::is_full)
        {
            self.finish_current_batch()?;
        }
        Ok(WriteOutcome::from_computed_position(
            u32::try_from(buf.len()).unwrap_or(u32::MAX),
            false,
            false,
            position,
        ))
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        if block_size_bytes as usize != self.caps.block_size
            || buf.is_empty()
            || buf.len() % self.caps.block_size != 0
        {
            return Err(TapeIoError::OperationFailed(
                "pipelined batch must contain whole configured records".into(),
            ));
        }
        self.flush_pending()?;
        let _: Option<WriteBatchOutcome> =
            self.request(|reply| PipelinedSinkCommand::Barrier { reply })?;
        let records = u32::try_from(buf.len() / self.caps.block_size)
            .map_err(|_| TapeIoError::OperationFailed("batch record count overflow".into()))?;
        for block in buf.chunks_exact(self.caps.block_size) {
            self.write_block(block)?;
        }
        self.flush_pending()?;
        let outcome = self
            .request(|reply| PipelinedSinkCommand::Barrier { reply })?
            .ok_or_else(|| {
                TapeIoError::OperationFailed("pipelined batch barrier lost its outcome".into())
            })?;
        if outcome.records_written != records {
            return Err(TapeIoError::OperationFailed(format!(
                "pipelined batch outcome mismatch: requested={records} written={}",
                outcome.records_written
            )));
        }
        self.cursor = Some(outcome.position_after);
        Ok(outcome)
    }

    fn write_batch_blocks(&self, _block_size_bytes: u32) -> u32 {
        self.caps.batch_blocks
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.caps.requested_write_batch_blocks
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.ring_buffers as u32
    }

    fn position_check_bytes(&self) -> u64 {
        self.caps.position_check_bytes
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.flush_pending()?;
        let outcome =
            self.request(|reply| PipelinedSinkCommand::WriteFilemarks { count, reply })?;
        self.cursor = Some(outcome.position_after);
        Ok(outcome)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.flush_pending()?;
        self.request(|reply| PipelinedSinkCommand::WriteFilemarksImmediate { count, reply })
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::SpaceToEndOfData { reply })?;
        self.cursor = Some(position);
        Ok(position)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::Locate { lba, reply })?;
        self.cursor = Some(position);
        Ok(position)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.flush_pending()?;
        let position = self.request(|reply| PipelinedSinkCommand::Position { reply })?;
        self.cursor = Some(position);
        Ok(position)
    }
}

#[cfg(test)]
pub(crate) fn run_staged_transfer<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    run_staged_transfer_with_safety(inner, block_size, producer, |_| Ok(()))
}

#[cfg(test)]
pub(crate) fn run_staged_transfer_with_safety<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
    on_safety_error: impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    run_ring_staged_transfer(inner, block_size, producer, None, on_safety_error)?.result
}

#[cfg(test)]
pub(crate) fn run_fenced_staged_transfer<S, R>(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    run_ring_staged_transfer(inner, block_size, producer, None, |error| {
        let error = error.to_string();
        record_tape_io_fence_for_transfer_error(
            state,
            selected,
            tape_io_fence_reason_for_transfer_error(&error),
            &error,
        )
    })?
    .result
}

#[cfg(test)]
pub(crate) fn run_counted_fenced_staged_transfer<S, R>(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    inner: &mut CountingBlockSink<'_, S>,
    block_size: usize,
    overlap_control: Option<Arc<crate::append_ring::AppendRingControl>>,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    let tape_write_control = overlap_control.as_ref().map(Arc::clone);
    let outcome =
        run_ring_staged_transfer(inner, block_size, producer, tape_write_control, |error| {
            if overlap_control
                .as_ref()
                .is_some_and(|control| !control.tape_started())
            {
                return Ok(());
            }
            let error = error.to_string();
            record_tape_io_fence_for_transfer_error(
                state,
                selected,
                tape_io_fence_reason_for_transfer_error(&error),
                &error,
            )
        })?;
    inner.stats.record_staging(&outcome.staging_diagnostics);
    outcome.result
}

pub(crate) fn run_faulted_counted_fenced_staged_transfer<S, R>(
    state: &mut CatalogIndex,
    selected: &SelectedTape,
    inner: &mut CountingBlockSink<'_, S>,
    block_size: usize,
    overlap_control: Option<Arc<crate::append_ring::AppendRingControl>>,
    fault: Option<&crate::object_fault::ObjectFaultPlan>,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
) -> Result<R, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    let tape_write_control = overlap_control.as_ref().map(Arc::clone);
    let outcome = {
        let mut faulting = crate::object_fault::ObjectFaultSink::new(inner, fault);
        run_ring_staged_transfer(
            &mut faulting,
            block_size,
            producer,
            tape_write_control,
            |error| {
                if overlap_control
                    .as_ref()
                    .is_some_and(|control| !control.tape_started())
                {
                    return Ok(());
                }
                let error = error.to_string();
                record_tape_io_fence_for_transfer_error(
                    state,
                    selected,
                    tape_io_fence_reason_for_transfer_error(&error),
                    &error,
                )
            },
        )?
    };
    inner.stats.record_staging(&outcome.staging_diagnostics);
    outcome.result
}

pub(crate) struct RingStagedTransferOutcome<R> {
    pub(crate) result: Result<R, PoolWriteError>,
    pub(crate) staging_diagnostics: StagingPhaseDiagnostics,
}

pub(crate) fn run_ring_staged_transfer<S, R>(
    inner: &mut S,
    block_size: usize,
    producer: impl FnOnce(&mut dyn BlockSink) -> Result<R, PoolWriteError> + Send,
    tape_write_control: Option<Arc<crate::append_ring::AppendRingControl>>,
    mut on_safety_error: impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<RingStagedTransferOutcome<R>, PoolWriteError>
where
    S: BlockSink + ?Sized,
    R: Send,
{
    let ring_buffers = usize::try_from(inner.staging_ring_buffers()).map_err(|_| {
        PoolWriteError::InvalidInput("staging ring depth does not fit usize".into())
    })?;
    if !(remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS as usize..=MAX_PIPELINE_WINDOW_BUFFERS)
        .contains(&ring_buffers)
    {
        return Err(PoolWriteError::InvalidInput(format!(
            "staging ring depth must be {}..={}, got {ring_buffers}",
            remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS,
            remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS,
        )));
    }
    let caps = StagedSinkCaps::from_inner(inner, block_size);
    let batch_bytes = block_size
        .checked_mul(caps.batch_blocks as usize)
        .ok_or_else(|| PoolWriteError::InvalidInput("staging batch bytes overflow".into()))?;
    let ring_bytes = batch_bytes
        .checked_mul(ring_buffers)
        .ok_or_else(|| PoolWriteError::InvalidInput("staging ring bytes overflow".into()))?;
    tracing::info!(
        target: "remanence_write_diag",
        phase = "staging_ring_open",
        effective_mode = "fixed_pipelined",
        staging_ring_buffers = ring_buffers,
        effective_batch_blocks = caps.batch_blocks,
        block_size_bytes = block_size,
        effective_ring_bytes = ring_bytes,
        "remanence_write_diag",
    );

    let accounting = Arc::new(RingAccounting::default());
    let (free_tx, free_rx) = std_mpsc::sync_channel(ring_buffers);
    for _ in 0..ring_buffers {
        let buffer = PageAlignedBuffer::try_new(batch_bytes, Arc::clone(&accounting))?;
        free_tx
            .try_send(buffer)
            .map_err(|_| PoolWriteError::InvalidInput("failed to seed staging free ring".into()))?;
    }
    let (submit_tx, submit_rx) = std_mpsc::sync_channel(1);
    let (submitter_done_tx, submitter_done_rx) = std_mpsc::channel();
    let poison = Arc::new(Mutex::new(None::<String>));
    inner.reset_pipelined_write_diagnostics();
    let result = std::thread::scope(|scope| {
        let producer_poison = Arc::clone(&poison);
        let producer_handle = scope.spawn(move || {
            let mut staged = StagedBlockSink::new(
                submit_tx,
                free_rx,
                submitter_done_rx,
                producer_poison,
                caps,
                ring_buffers,
            );
            let result = producer(&mut staged);
            match result {
                Ok(value) => {
                    let (finish_result, diagnostics) = staged.finish();
                    (
                        finish_result.map(|()| value).map_err(PoolWriteError::from),
                        diagnostics,
                    )
                }
                Err(err) => {
                    let diagnostics = staged.abort(err.to_string());
                    (Err(err), diagnostics)
                }
            }
        });

        let submitter_result = drain_pipelined_transfer(
            inner,
            submit_rx,
            free_tx,
            &poison,
            tape_write_control.as_deref(),
            &mut on_safety_error,
        );
        let _ = submitter_done_tx.send(());
        let (producer_result, staging_diagnostics) = match producer_handle.join() {
            Ok(result) => result,
            Err(_) => (
                Err(PoolWriteError::InvalidInput(
                    "pipelined staging producer thread panicked".into(),
                )),
                StagingPhaseDiagnostics::default(),
            ),
        };
        log_staging_phase_diagnostics(&staging_diagnostics);
        let result = match submitter_result {
            Ok(()) => match producer_result {
                Ok(value) => Ok(value),
                Err(primary) => {
                    let safety_error = TapeIoError::OperationFailed(primary.to_string());
                    match on_safety_error(&safety_error) {
                        Ok(()) => Err(primary),
                        Err(secondary) => Err(attach_secondary(
                            primary,
                            "tape-I/O fence persistence",
                            secondary,
                        )),
                    }
                }
            },
            Err(err) => Err(err),
        };
        (result, staging_diagnostics)
    });
    let (mut result, staging_diagnostics) = result;
    inner.publish_pipelined_write_diagnostics();
    let allocated = accounting.allocated.load(Ordering::Relaxed);
    let dropped = accounting.dropped.load(Ordering::Relaxed);
    if allocated != dropped {
        let imbalance = PoolWriteError::InvalidInput(format!(
            "staging ring accounting imbalance: allocated={allocated} dropped={dropped}"
        ));
        result = match result {
            Ok(_) => {
                let safety_error = TapeIoError::OperationFailed(imbalance.to_string());
                let fence_result = on_safety_error(&safety_error);
                inner.flush_pending_pipeline_audit();
                match fence_result {
                    Ok(()) => Err(imbalance),
                    Err(secondary) => Err(attach_secondary(
                        imbalance,
                        "tape-I/O fence persistence",
                        secondary,
                    )),
                }
            }
            Err(primary) => Err(attach_secondary(
                primary,
                "staging ring accounting",
                imbalance,
            )),
        };
    }
    Ok(RingStagedTransferOutcome {
        result,
        staging_diagnostics,
    })
}

pub(crate) fn log_staging_phase_diagnostics(diagnostics: &StagingPhaseDiagnostics) {
    tracing::info!(
        target: "remanence_write_diag",
        phase = "staging_subphases",
        staging_wait_samples = diagnostics.wait_us.samples,
        staging_wait_p50_us = diagnostics.wait_us.percentile(50, 100),
        staging_wait_p95_us = diagnostics.wait_us.percentile(95, 100),
        staging_wait_max_us = diagnostics.wait_us.max_us,
        staging_wait_mean_us = diagnostics.wait_us.mean(),
        refill_samples = diagnostics.refill_us.samples,
        refill_p50_us = diagnostics.refill_us.percentile(50, 100),
        refill_p95_us = diagnostics.refill_us.percentile(95, 100),
        refill_max_us = diagnostics.refill_us.max_us,
        refill_mean_us = diagnostics.refill_us.mean(),
        "remanence_write_diag",
    );
}

pub(crate) fn drain_pipelined_transfer<S: BlockSink + ?Sized>(
    inner: &mut S,
    rx: std_mpsc::Receiver<PipelinedSinkCommand>,
    free_tx: std_mpsc::SyncSender<PageAlignedBuffer>,
    poison: &Arc<Mutex<Option<String>>>,
    tape_write_control: Option<&crate::append_ring::AppendRingControl>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<(), PoolWriteError> {
    let mut completed_since_barrier: Option<WriteBatchOutcome> = None;
    while let Ok(command) = rx.recv() {
        if let Some(message) = staged_poison_message(poison) {
            discard_pipelined_command(command, &free_tx, message);
            continue;
        }
        let result = match command {
            PipelinedSinkCommand::WriteWindow(window) => execute_pipelined_window(
                inner,
                window,
                &free_tx,
                tape_write_control,
                on_safety_error,
            )
            .map(|window_outcome| {
                completed_since_barrier = Some(match completed_since_barrier {
                    Some(accumulated) => merge_batch_outcomes(accumulated, window_outcome),
                    None => window_outcome,
                });
            }),
            PipelinedSinkCommand::Barrier { reply } => {
                let _ = reply.send(Ok(completed_since_barrier.take()));
                Ok(())
            }
            PipelinedSinkCommand::WriteFilemarks { count, reply } => {
                if let Some(control) = tape_write_control {
                    control.mark_tape_started();
                }
                match inner.write_filemarks_pipelined(count) {
                    Ok(outcome) => {
                        let _ = reply.send(Ok(outcome));
                        Ok(())
                    }
                    Err(err) => {
                        let failure = finish_transfer_failure(inner, err, on_safety_error);
                        let _ = reply.send(Err(failure.to_string()));
                        Err(failure)
                    }
                }
            }
            PipelinedSinkCommand::WriteFilemarksImmediate { count, reply } => {
                if let Some(control) = tape_write_control {
                    control.mark_tape_started();
                }
                match inner.write_filemarks_immediate(count) {
                    Ok(()) => {
                        let _ = reply.send(Ok(()));
                        Ok(())
                    }
                    Err(err) => {
                        let failure = finish_transfer_failure(inner, err, on_safety_error);
                        let _ = reply.send(Err(failure.to_string()));
                        Err(failure)
                    }
                }
            }
            PipelinedSinkCommand::SpaceToEndOfData { reply } => {
                match inner.space_to_end_of_data_pipelined() {
                    Ok(position) => {
                        let _ = reply.send(Ok(position));
                        Ok(())
                    }
                    Err(err) => {
                        let failure = finish_transfer_failure(inner, err, on_safety_error);
                        let _ = reply.send(Err(failure.to_string()));
                        Err(failure)
                    }
                }
            }
            PipelinedSinkCommand::Locate { lba, reply } => match inner.locate(lba) {
                Ok(position) => {
                    let _ = reply.send(Ok(position));
                    Ok(())
                }
                Err(err) => {
                    let failure = finish_transfer_failure(inner, err, on_safety_error);
                    let _ = reply.send(Err(failure.to_string()));
                    Err(failure)
                }
            },
            PipelinedSinkCommand::Position { reply } => match inner.position_pipelined() {
                Ok(position) => {
                    let _ = reply.send(Ok(position));
                    Ok(())
                }
                Err(err) => {
                    let failure = finish_transfer_failure(inner, err, on_safety_error);
                    let _ = reply.send(Err(failure.to_string()));
                    Err(failure)
                }
            },
        };
        if let Err(err) = result {
            set_staged_poison(poison, err.to_string());
            while let Ok(queued) = rx.try_recv() {
                discard_pipelined_command(queued, &free_tx, err.to_string());
            }
            drop(rx);
            drop(free_tx);
            return Err(err);
        }
    }
    Ok(())
}

pub(crate) fn attach_secondary(
    primary: PoolWriteError,
    context: &'static str,
    secondary: PoolWriteError,
) -> PoolWriteError {
    PoolWriteError::TransferWithSecondary {
        primary: primary.to_string(),
        context,
        secondary: secondary.to_string(),
    }
}

pub(crate) fn finish_transfer_failure<S: BlockSink + ?Sized>(
    inner: &mut S,
    error: TapeIoError,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> PoolWriteError {
    let fence_result = on_safety_error(&error);
    inner.flush_pending_pipeline_audit();
    let primary = PoolWriteError::from(error);
    match fence_result {
        Ok(()) => primary,
        Err(secondary) => attach_secondary(primary, "tape-I/O fence persistence", secondary),
    }
}

pub(crate) fn execute_pipelined_window<S: BlockSink + ?Sized>(
    inner: &mut S,
    mut window: PipelinedWindow,
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    tape_write_control: Option<&crate::append_ring::AppendRingControl>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
) -> Result<WriteBatchOutcome, PoolWriteError> {
    let command_count = window.len as u32;
    let bytes = window.bytes;
    let first_records = window.first_records();
    let last_records = window.last_records();
    inner.begin_pipelined_write_window(command_count, bytes, first_records, last_records);
    let started = Instant::now();
    let mut completed: Option<WriteBatchOutcome> = None;
    for index in 0..window.len {
        let batch = window.batches[index]
            .take()
            .expect("window slot below len is occupied");
        let requested = batch.records;
        let requested_bytes = u64::from(requested) * u64::from(batch.block_size_bytes);
        if let Some(control) = tape_write_control {
            control.mark_tape_started();
        }
        let result = inner.write_block_batch_pipelined(
            batch.buffer.bytes(),
            batch.block_size_bytes,
            &batch.cdb,
        );
        let buffer_return_error = return_ring_buffer(free_tx, batch.buffer).err();
        let outcome = match result {
            Ok(outcome)
                if outcome.records_written == requested
                    && u64::from(outcome.bytes_written) == requested_bytes
                    && !outcome.end_of_medium =>
            {
                if let Some(error) = buffer_return_error {
                    return finish_pipelined_window_failure(
                        inner,
                        &mut window,
                        free_tx,
                        on_safety_error,
                        command_count,
                        bytes,
                        first_records,
                        last_records,
                        TapeIoError::OperationFailed(error.to_string()),
                        None,
                    );
                }
                outcome
            }
            Ok(outcome) => {
                let err = TapeIoError::PartialBatchUncommittable {
                    requested_records: requested,
                    written_records: outcome.records_written,
                    requested_bytes,
                    written_bytes: u64::from(outcome.bytes_written),
                    end_of_medium: outcome.end_of_medium,
                    sense: None,
                };
                return finish_pipelined_window_failure(
                    inner,
                    &mut window,
                    free_tx,
                    on_safety_error,
                    command_count,
                    bytes,
                    first_records,
                    last_records,
                    err,
                    buffer_return_error,
                );
            }
            Err(err) => {
                return finish_pipelined_window_failure(
                    inner,
                    &mut window,
                    free_tx,
                    on_safety_error,
                    command_count,
                    bytes,
                    first_records,
                    last_records,
                    err,
                    buffer_return_error,
                );
            }
        };
        completed = Some(match completed {
            Some(accumulated) => merge_batch_outcomes(accumulated, outcome),
            None => outcome,
        });
    }
    inner.finish_pipelined_write_window_success(
        command_count,
        bytes,
        first_records,
        last_records,
        started.elapsed(),
    );
    completed.ok_or_else(|| PoolWriteError::InvalidInput("empty pipelined window".into()))
}

pub(crate) fn merge_batch_outcomes(
    accumulated: WriteBatchOutcome,
    next: WriteBatchOutcome,
) -> WriteBatchOutcome {
    WriteBatchOutcome::from_computed_position(
        accumulated
            .records_written
            .saturating_add(next.records_written),
        accumulated.bytes_written.saturating_add(next.bytes_written),
        accumulated.early_warning || next.early_warning,
        accumulated.end_of_medium || next.end_of_medium,
        next.position_after,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_pipelined_window_failure<S: BlockSink + ?Sized, T>(
    inner: &mut S,
    window: &mut PipelinedWindow,
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    on_safety_error: &mut impl FnMut(&TapeIoError) -> Result<(), PoolWriteError>,
    command_count: u32,
    bytes: u64,
    first_records: u32,
    last_records: u32,
    error: TapeIoError,
    mut secondary: Option<PoolWriteError>,
) -> Result<T, PoolWriteError> {
    for batch in window.batches.iter_mut().filter_map(Option::take) {
        if let Err(error) = return_ring_buffer(free_tx, batch.buffer) {
            secondary.get_or_insert(error);
        }
    }
    let fence_result = on_safety_error(&error);
    inner.finish_pipelined_write_window_error(
        command_count,
        bytes,
        first_records,
        last_records,
        &error,
    );
    let mut primary = PoolWriteError::from(error);
    if let Err(error) = fence_result {
        primary = attach_secondary(primary, "tape-I/O fence persistence", error);
    }
    if let Some(error) = secondary {
        primary = attach_secondary(primary, "staging buffer return", error);
    }
    Err(primary)
}

pub(crate) fn return_ring_buffer(
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    mut buffer: PageAlignedBuffer,
) -> Result<(), PoolWriteError> {
    buffer.reset();
    free_tx.try_send(buffer).map_err(|err| match err {
        std_mpsc::TrySendError::Full(_) => PoolWriteError::InvalidInput(
            "staging buffer return path filled despite ring-sized capacity".into(),
        ),
        std_mpsc::TrySendError::Disconnected(_) => {
            PoolWriteError::InvalidInput("staging buffer return path disconnected".into())
        }
    })
}

pub(crate) fn discard_pipelined_command(
    command: PipelinedSinkCommand,
    free_tx: &std_mpsc::SyncSender<PageAlignedBuffer>,
    message: String,
) {
    match command {
        PipelinedSinkCommand::WriteWindow(mut window) => {
            for batch in window.batches.iter_mut().filter_map(Option::take) {
                let _ = return_ring_buffer(free_tx, batch.buffer);
            }
        }
        PipelinedSinkCommand::Barrier { reply } => {
            let _ = reply.send(Err(message));
        }
        PipelinedSinkCommand::WriteFilemarks { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        PipelinedSinkCommand::WriteFilemarksImmediate { reply, .. } => {
            let _ = reply.send(Err(message));
        }
        PipelinedSinkCommand::SpaceToEndOfData { reply }
        | PipelinedSinkCommand::Locate { reply, .. }
        | PipelinedSinkCommand::Position { reply } => {
            let _ = reply.send(Err(message));
        }
    }
}

pub(crate) fn records_in_staged_batch(
    data: &[u8],
    block_size_bytes: u32,
) -> Result<u32, TapeIoError> {
    if block_size_bytes == 0 {
        return Err(TapeIoError::OperationFailed(
            "staged write batch block size must be nonzero".to_string(),
        ));
    }
    let block_size = block_size_bytes as usize;
    if data.is_empty() || data.len() % block_size != 0 {
        return Err(TapeIoError::OperationFailed(
            "staged write batch must contain whole records".to_string(),
        ));
    }
    u32::try_from(data.len() / block_size).map_err(|_| {
        TapeIoError::OperationFailed("staged write batch record count overflow".to_string())
    })
}

pub(crate) fn staged_poison_message(poison: &Arc<Mutex<Option<String>>>) -> Option<String> {
    poison.lock().unwrap_or_else(|err| err.into_inner()).clone()
}

pub(crate) fn set_staged_poison(poison: &Arc<Mutex<Option<String>>>, message: String) {
    let mut guard = poison.lock().unwrap_or_else(|err| err.into_inner());
    guard.get_or_insert(message);
}

impl<'a> LiveCounterBlockSink<'a> {
    pub(crate) fn new(
        inner: &'a mut dyn BlockSink,
        live_write_counter: Arc<crate::DriveByteCounters>,
        block_size_bytes: u32,
    ) -> Self {
        live_write_counter.configure_tape_io(
            inner.staging_ring_buffers(),
            inner.write_batch_blocks(block_size_bytes),
        );
        Self {
            inner,
            live_write_counter,
        }
    }
}

impl<'a, S: BlockSink + ?Sized> CountingBlockSink<'a, S> {
    pub(crate) fn new(inner: &'a mut S, block_size: u32) -> Self {
        let write_batch_blocks = inner.requested_write_batch_blocks().max(1);
        let effective_batch_blocks = inner.write_batch_blocks(block_size).max(1);
        let position_check_bytes = inner.position_check_bytes();
        let staging_ring_buffers = inner.staging_ring_buffers();
        Self {
            inner,
            stats: BlockSinkStats {
                write_batch_blocks,
                effective_batch_blocks,
                position_check_bytes,
                staging_ring_buffers,
                ..BlockSinkStats::default()
            },
        }
    }

    pub(crate) fn stats(&self) -> BlockSinkStats {
        let mut stats = self.stats;
        let diagnostics = self.inner.pipelined_write_diagnostics();
        stats.gap_samples = diagnostics.gap_samples;
        stats.gap_p50_us = diagnostics.gap_p50_us;
        stats.gap_p95_us = diagnostics.gap_p95_us;
        stats.gap_max_us = diagnostics.gap_max_us;
        stats.gap_mean_us = diagnostics.gap_mean_us;
        stats.ioctl_samples = diagnostics.ioctl_samples;
        stats.ioctl_p50_us = diagnostics.ioctl_p50_us;
        stats.ioctl_p95_us = diagnostics.ioctl_p95_us;
        stats.ioctl_max_us = diagnostics.ioctl_max_us;
        stats.ioctl_mean_us = diagnostics.ioctl_mean_us;
        stats.first_60s_ioctl_samples = diagnostics.first_60s_ioctl_samples;
        stats.first_60s_ioctl_p50_us = diagnostics.first_60s_ioctl_p50_us;
        stats.first_60s_ioctl_p95_us = diagnostics.first_60s_ioctl_p95_us;
        stats.first_60s_ioctl_max_us = diagnostics.first_60s_ioctl_max_us;
        stats.first_60s_ioctl_mean_us = diagnostics.first_60s_ioctl_mean_us;
        stats.accounting_samples = diagnostics.accounting_samples;
        stats.accounting_p50_us = diagnostics.accounting_p50_us;
        stats.accounting_p95_us = diagnostics.accounting_p95_us;
        stats.accounting_max_us = diagnostics.accounting_max_us;
        stats.accounting_mean_us = diagnostics.accounting_mean_us;
        stats.cadence_us = diagnostics.cadence_us;
        stats.effective_feed_bytes_per_second = diagnostics.effective_feed_bytes_per_second;
        stats.time_to_first_ioctl_ms = diagnostics.time_to_first_ioctl_ms;
        stats.steady_reached = diagnostics.steady_reached;
        stats.time_to_steady_ms = diagnostics.time_to_steady_ms;
        stats.steady_window_seconds = diagnostics.steady_window_seconds;
        stats.steady_threshold_percent = diagnostics.steady_threshold_percent;
        stats.ramp_observation_seconds = diagnostics.ramp_observation_seconds;
        stats
    }
}

impl<'a> BlockSink for LiveCounterBlockSink<'a> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.live_write_counter
            .record_write_bytes(u64::from(outcome.bytes_written));
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.live_write_counter
            .record_write_bytes(u64::from(outcome.bytes_written));
        Ok(outcome)
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let result = self
            .inner
            .write_block_batch_pipelined(buf, block_size_bytes, cdb);
        match &result {
            Ok(outcome) => self
                .live_write_counter
                .record_write_bytes(u64::from(outcome.bytes_written)),
            Err(TapeIoError::GoodWriteTripwire { outcome, .. }) => self
                .live_write_counter
                .record_write_bytes(u64::from(outcome.bytes_written)),
            Err(_) => {}
        }
        result
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.inner.staging_ring_buffers()
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        self.inner.pipelined_write_diagnostics()
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.inner.reset_pipelined_write_diagnostics();
        self.live_write_counter
            .record_tape_io_diagnostics(PipelinedWriteDiagnostics::default());
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.live_write_counter
            .record_tape_io_diagnostics(self.inner.pipelined_write_diagnostics());
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.inner
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
        self.inner.finish_pipelined_write_window_success(
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
        self.inner.finish_pipelined_write_window_error(
            command_count,
            bytes,
            first_records,
            last_records,
            error,
        );
    }

    fn flush_pending_pipeline_audit(&mut self) {
        self.inner.flush_pending_pipeline_audit();
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks(count)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        self.inner.write_filemarks_immediate(count)
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        self.inner.write_filemarks_pipelined(count)
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data()
    }

    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.space_to_end_of_data_pipelined()
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        self.inner.locate(lba)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position()
    }

    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        self.inner.position_pipelined()
    }
}

impl<'a, S: BlockSink + ?Sized> BlockSink for CountingBlockSink<'a, S> {
    fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
        let outcome = self.inner.write_block(buf)?;
        self.stats
            .record_block(u64::from(outcome.bytes_written), outcome.early_warning);
        Ok(outcome)
    }

    fn write_block_batch(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let outcome = self.inner.write_block_batch(buf, block_size_bytes)?;
        self.stats
            .record_block(u64::from(outcome.bytes_written), outcome.early_warning);
        Ok(outcome)
    }

    fn write_block_batch_pipelined(
        &mut self,
        buf: &[u8],
        block_size_bytes: u32,
        cdb: &[u8],
    ) -> Result<WriteBatchOutcome, TapeIoError> {
        let result = self
            .inner
            .write_block_batch_pipelined(buf, block_size_bytes, cdb);
        match &result {
            Ok(outcome) => self
                .stats
                .record_block(u64::from(outcome.bytes_written), outcome.early_warning),
            Err(TapeIoError::GoodWriteTripwire { outcome, .. }) => self
                .stats
                .record_block(u64::from(outcome.bytes_written), outcome.early_warning),
            Err(_) => {}
        }
        result
    }

    fn write_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.write_batch_blocks(block_size_bytes)
    }

    fn requested_write_batch_blocks(&self) -> u32 {
        self.inner.requested_write_batch_blocks()
    }

    fn staging_ring_buffers(&self) -> u32 {
        self.inner.staging_ring_buffers()
    }

    fn pipelined_write_diagnostics(&self) -> PipelinedWriteDiagnostics {
        self.inner.pipelined_write_diagnostics()
    }

    fn reset_pipelined_write_diagnostics(&mut self) {
        self.inner.reset_pipelined_write_diagnostics();
    }

    fn publish_pipelined_write_diagnostics(&mut self) {
        self.inner.publish_pipelined_write_diagnostics();
    }

    fn begin_pipelined_write_window(
        &mut self,
        command_count: u32,
        bytes: u64,
        first_records: u32,
        last_records: u32,
    ) {
        self.inner
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
        self.inner.finish_pipelined_write_window_success(
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
        self.inner.finish_pipelined_write_window_error(
            command_count,
            bytes,
            first_records,
            last_records,
            error,
        );
    }

    fn flush_pending_pipeline_audit(&mut self) {
        self.inner.flush_pending_pipeline_audit();
    }

    fn position_check_bytes(&self) -> u64 {
        self.inner.position_check_bytes()
    }

    fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let started = Instant::now();
        let outcome = self.inner.write_filemarks(count)?;
        self.stats
            .record_filemarks(count, outcome.early_warning, started.elapsed());
        Ok(outcome)
    }

    fn write_filemarks_pipelined(
        &mut self,
        count: u32,
    ) -> Result<WriteFilemarksOutcome, TapeIoError> {
        let started = Instant::now();
        let outcome = self.inner.write_filemarks_pipelined(count)?;
        self.stats
            .record_filemarks(count, outcome.early_warning, started.elapsed());
        Ok(outcome)
    }

    fn write_filemarks_immediate(&mut self, count: u32) -> Result<(), TapeIoError> {
        let started = Instant::now();
        self.inner.write_filemarks_immediate(count)?;
        self.stats.record_filemarks(count, false, started.elapsed());
        Ok(())
    }

    fn space_to_end_of_data(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.space_to_end_of_data()?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn space_to_end_of_data_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.space_to_end_of_data_pipelined()?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.locate(lba)?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.position()?;
        self.stats.record_position(position);
        Ok(position)
    }

    fn position_pipelined(&mut self) -> Result<TapePosition, TapeIoError> {
        let position = self.inner.position_pipelined()?;
        self.stats.record_position(position);
        Ok(position)
    }
}
