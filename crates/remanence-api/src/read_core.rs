//! Shared tape-object read core for CLI break-glass reads and Layer 5 read sessions.
//!
//! The CLI still owns the hardware orchestration for `rem-debug archive read`
//! and `verify`, while the daemon session owner owns the mounted drive for
//! `ReadSessionService`. Both paths use this module to position to a native
//! object tape file and stream the single RAO payload entry without
//! materializing the object in memory.

use std::collections::VecDeque;
use std::io::Write;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use remanence_format::{
    model::{BodyLba, MANIFEST_PATH},
    plan_plaintext_rao_file_range, stream_rem_tar_object_with_manifest_anchor, FormatError,
    RemTarEntrySink, RemTarStreamEntry,
};
#[cfg(test)]
use remanence_library::SpaceResult;
use remanence_library::{
    BlockRead, BlockSource, PositionAfter, ReadBuffer, ReadBufferHandoff, ReadDelivery,
    SequencedHandoff, SpaceKind, TapeIoError, TapePosition,
};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tonic::Status;

use crate::io_memory::{IoMemoryPermit, IoMemoryReservation};
use crate::pb;

pub(crate) const DEFAULT_READ_STREAM_CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const READ_STREAM_CHANNEL_BYTE_BUDGET: usize = 4 * 1024 * 1024;
const READ_STREAM_CHANNEL_MAX_MESSAGES: usize = 1024;
const READ_DELIVERY_PROOF_SLOTS: usize = 4;
const MAX_READ_RESERVOIR_SLABS: usize = 131_072;
const T_REPROOF_INCIDENTAL: Duration = Duration::from_millis(250);
const READ_DIAG_SCHEMA_VERSION: u64 = 1;
static NEXT_READ_DIAG_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Stable, log-scraped read-pipeline diagnostic schema (version 1).
///
/// The daemon's flattened JSON tracing formatter emits every field below on
/// exactly two single-line events per pipeline: `open` and `close`. Fields are
/// never added from the per-block submitter loop. Consumers must select target
/// `remanence_read_diag`, validate `schema_version`, correlate `session_id`,
/// and treat this fixed key set as the versioned acceptance interface.
#[derive(Clone, Copy)]
struct ReadDiagRecord {
    phase: &'static str,
    session_id: u64,
    block_size_bytes: u64,
    batch_records: u64,
    minimum_slabs: u64,
    maximum_slabs: u64,
    effective_reservoir_bytes: u64,
    reservoir_high_watermark: u64,
    occupancy_bytes: u64,
    park_cycles: u64,
    park_us: u64,
    free_wait_us: u64,
    feed_gap_total_us: u64,
    feed_gap_max_us: u64,
    feed_gap_samples: u64,
}

fn emit_read_diag(record: ReadDiagRecord) {
    tracing::info!(
        target: "remanence_read_diag",
        schema_version = READ_DIAG_SCHEMA_VERSION,
        event = "read_pipeline_diag",
        phase = record.phase,
        session_id = record.session_id,
        block_size_bytes = record.block_size_bytes,
        batch_records = record.batch_records,
        minimum_slabs = record.minimum_slabs,
        maximum_slabs = record.maximum_slabs,
        effective_reservoir_bytes = record.effective_reservoir_bytes,
        reservoir_high_watermark = record.reservoir_high_watermark,
        occupancy_bytes = record.occupancy_bytes,
        park_cycles = record.park_cycles,
        park_us = record.park_us,
        free_wait_us = record.free_wait_us,
        feed_gap_total_us = record.feed_gap_total_us,
        feed_gap_max_us = record.feed_gap_max_us,
        feed_gap_samples = record.feed_gap_samples,
    );
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatermarkSubmitterState {
    Filling,
    Gated,
    ResumingProofPending,
}

/// Decode-side fixed-block source fed only by typed read deliveries.
///
/// This type intentionally contains no drive handle and implements only
/// [`BlockRead`]. The submitter/reservoir stage wires these channels in R2b.
/// Tape motion is therefore absent by type:
///
/// ```compile_fail
/// use remanence_api::read_core::HandoffBlockSource;
/// use remanence_library::BlockSource;
///
/// fn decoder_cannot_query_drive(source: &mut HandoffBlockSource) {
///     source.position();
/// }
/// ```
pub struct HandoffBlockSource {
    delivery_receiver: Receiver<Result<ReadDelivery, TapeIoError>>,
    free_sender: SyncSender<ReadBuffer>,
    block_size: usize,
    remaining: u64,
    current: Option<ReadBufferHandoff>,
    next_record: u32,
    plan_total: u64,
    received_records: u64,
    next_seq: u64,
    proven_frontier: u64,
    ranged_frontier: bool,
    pending: VecDeque<SequencedHandoff>,
    last_position: Option<TapePosition>,
    reservoir: Arc<ReservoirState>,
}

impl HandoffBlockSource {
    /// Construct the no-drive decode source for one planned read window.
    fn new(
        delivery_receiver: Receiver<Result<ReadDelivery, TapeIoError>>,
        free_sender: SyncSender<ReadBuffer>,
        block_size: usize,
        remaining: u64,
        ranged_frontier: bool,
        reservoir: Arc<ReservoirState>,
    ) -> Result<Self, TapeIoError> {
        if block_size == 0 {
            return Err(TapeIoError::OperationFailed(
                "handoff block size must be nonzero".to_string(),
            ));
        }
        Ok(Self {
            delivery_receiver,
            free_sender,
            block_size,
            remaining,
            current: None,
            next_record: 0,
            plan_total: remaining,
            received_records: 0,
            next_seq: 1,
            proven_frontier: 0,
            ranged_frontier,
            pending: VecDeque::new(),
            last_position: None,
            reservoir,
        })
    }

    fn recycle(&self, handoff: ReadBufferHandoff) -> Result<(), TapeIoError> {
        self.reservoir.consume(handoff.valid_bytes as u64);
        self.free_sender
            .try_send(handoff.into_reusable_buffer())
            .map_err(|_| {
                TapeIoError::OperationFailed(
                    "read reservoir free-buffer channel unavailable".to_string(),
                )
            })
    }

    fn accept_handoff(&mut self, delivery: SequencedHandoff) -> Result<(), TapeIoError> {
        if delivery.seq != self.next_seq {
            return Err(TapeIoError::OperationFailed(format!(
                "read delivery sequence mismatch: expected {}, got {}",
                self.next_seq, delivery.seq
            )));
        }
        let expected_end = self
            .received_records
            .checked_add(u64::from(delivery.handoff.records_read))
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read handoff record frontier overflow".to_string())
            })?;
        if delivery.plan_records_end != expected_end {
            return Err(TapeIoError::OperationFailed(format!(
                "read handoff plan frontier mismatch: expected {expected_end}, got {}",
                delivery.plan_records_end
            )));
        }
        if let PositionAfter::Device(proof) = delivery.evidence {
            if proof.position() != delivery.position_after {
                return Err(TapeIoError::OperationFailed(
                    "read handoff device evidence position mismatch".to_string(),
                ));
            }
            self.proven_frontier = delivery.plan_records_end;
        }
        self.last_position = Some(delivery.position_after);
        self.received_records = expected_end;
        self.next_seq += 1;
        self.pending.push_back(delivery);
        Ok(())
    }

    fn accept_proof(
        &mut self,
        through_seq: u64,
        plan_records_end: u64,
        proof: remanence_library::DevicePositionProof,
    ) -> Result<(), TapeIoError> {
        let last_received_seq = self.next_seq - 1;
        let expected = match (through_seq, self.last_position) {
            (0, None) => (0, proof.position()),
            (seq, Some(position)) if seq == last_received_seq => (self.received_records, position),
            _ => {
                return Err(TapeIoError::OperationFailed(format!(
                    "proof frontier credits unreceived command {through_seq}; last received is {last_received_seq}"
                )))
            }
        };
        if plan_records_end != expected.0 || proof.position() != expected.1 {
            return Err(TapeIoError::OperationFailed(format!(
                "proof frontier attribution mismatch through_seq={through_seq} plan_records_end={plan_records_end}"
            )));
        }
        self.proven_frontier = self.proven_frontier.max(plan_records_end);
        Ok(())
    }

    fn install_next(&mut self, delivery: SequencedHandoff) -> Result<(), TapeIoError> {
        let handoff = delivery.handoff;
        let expected_bytes = (handoff.records_read as usize)
            .checked_mul(self.block_size)
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read handoff byte count overflow".to_string())
            })?;
        let validation_error = if handoff.terminal_flags.filemark || handoff.records_read == 0 {
            Some(TapeIoError::OperationFailed(format!(
                "fixed read batch stopped before object boundary: records_read={} filemark={}",
                handoff.records_read, handoff.terminal_flags.filemark
            )))
        } else if expected_bytes != handoff.valid_bytes {
            Some(TapeIoError::OperationFailed(format!(
                "read handoff byte/record mismatch: valid_bytes={} records_read={} block_size={}",
                handoff.valid_bytes, handoff.records_read, self.block_size
            )))
        } else {
            None
        };
        if let Some(error) = validation_error {
            self.reservoir.consume(handoff.valid_bytes as u64);
            let _ = self.free_sender.try_send(handoff.into_reusable_buffer());
            return Err(error);
        }
        self.remaining = self
            .remaining
            .checked_sub(u64::from(handoff.records_read))
            .ok_or_else(|| {
                TapeIoError::OperationFailed("read handoff remaining underflow".to_string())
            })?;
        self.current = Some(handoff);
        Ok(())
    }

    fn refill(&mut self) -> Result<(), TapeIoError> {
        if let Some(current) = self.current.take() {
            self.recycle(current)?;
        }
        self.next_record = 0;
        loop {
            let releasable = !self.ranged_frontier
                || self
                    .pending
                    .front()
                    .is_some_and(|handoff| handoff.plan_records_end <= self.proven_frontier);
            if releasable {
                if let Some(delivery) = self.pending.pop_front() {
                    return self.install_next(delivery);
                }
            }
            let delivery = self.delivery_receiver.recv().map_err(|_| {
                TapeIoError::OperationFailed("read delivery channel closed".to_string())
            })??;
            match delivery {
                ReadDelivery::Handoff(delivery) => self.accept_handoff(delivery)?,
                ReadDelivery::ProofFrontier {
                    through_seq,
                    plan_records_end,
                    proof,
                } => {
                    self.accept_proof(through_seq, plan_records_end, proof)?;
                }
            }
        }
    }

    fn finish(&mut self) -> Result<(), TapeIoError> {
        if let Some(current) = self.current.take() {
            self.recycle(current)?;
        }
        while let Some(pending) = self.pending.pop_front() {
            self.recycle(pending.handoff)?;
        }
        if self.received_records != self.plan_total || self.remaining != 0 {
            return Err(TapeIoError::OperationFailed(format!(
                "read delivery closed before issued plan was received: received={} plan={} remaining={}",
                self.received_records, self.plan_total, self.remaining
            )));
        }
        Ok(())
    }
}

impl Drop for HandoffBlockSource {
    fn drop(&mut self) {
        self.reservoir
            .consumer_alive
            .store(false, Ordering::Release);
        drop(
            self.reservoir
                .gate
                .lock()
                .unwrap_or_else(|err| err.into_inner()),
        );
        self.reservoir.wake.notify_all();
        if let Some(current) = self.current.take() {
            self.reservoir.consume(current.valid_bytes as u64);
            let _ = self.free_sender.try_send(current.into_reusable_buffer());
        }
        while let Some(pending) = self.pending.pop_front() {
            self.reservoir.consume(pending.handoff.valid_bytes as u64);
            let _ = self
                .free_sender
                .try_send(pending.handoff.into_reusable_buffer());
        }
    }
}

impl BlockRead for HandoffBlockSource {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let exhausted = self
            .current
            .as_ref()
            .is_none_or(|handoff| self.next_record >= handoff.records_read);
        if exhausted {
            if self.remaining == 0 {
                return Ok(0);
            }
            self.refill()?;
        }
        if buf.len() < self.block_size {
            return Err(TapeIoError::ReadBufferTooSmall {
                actual: u32::try_from(self.block_size).unwrap_or(u32::MAX),
                provided: u32::try_from(buf.len()).unwrap_or(u32::MAX),
            });
        }
        let handoff = self.current.as_ref().ok_or_else(|| {
            TapeIoError::OperationFailed("read handoff source is empty".to_string())
        })?;
        let start = self.next_record as usize * self.block_size;
        let end = start + self.block_size;
        buf[..self.block_size].copy_from_slice(&handoff.valid_data()[start..end]);
        self.next_record += 1;
        Ok(self.block_size)
    }
}

#[derive(Debug)]
struct ReservoirState {
    occupancy_bytes: AtomicU64,
    high_bytes: AtomicU64,
    low_bytes: AtomicU64,
    consumer_alive: AtomicBool,
    gate: Mutex<()>,
    wake: Condvar,
    park_cycles: AtomicU64,
    park_us: AtomicU64,
    free_wait_us: AtomicU64,
    feed_gap_total_us: AtomicU64,
    feed_gap_max_us: AtomicU64,
    feed_gap_samples: AtomicU64,
    #[cfg(test)]
    before_park_wait: Option<Arc<std::sync::Barrier>>,
}

impl ReservoirState {
    fn new(capacity_bytes: u64, high_pct: u8, low_pct: u8) -> Arc<Self> {
        Arc::new(Self {
            occupancy_bytes: AtomicU64::new(0),
            high_bytes: AtomicU64::new(percent_bytes(capacity_bytes, high_pct)),
            low_bytes: AtomicU64::new(percent_bytes(capacity_bytes, low_pct)),
            consumer_alive: AtomicBool::new(true),
            gate: Mutex::new(()),
            wake: Condvar::new(),
            park_cycles: AtomicU64::new(0),
            park_us: AtomicU64::new(0),
            free_wait_us: AtomicU64::new(0),
            feed_gap_total_us: AtomicU64::new(0),
            feed_gap_max_us: AtomicU64::new(0),
            feed_gap_samples: AtomicU64::new(0),
            #[cfg(test)]
            before_park_wait: None,
        })
    }

    #[cfg(test)]
    fn with_before_park_wait(
        capacity_bytes: u64,
        high_pct: u8,
        low_pct: u8,
        before_park_wait: Arc<std::sync::Barrier>,
    ) -> Arc<Self> {
        let mut reservoir = Self::new(capacity_bytes, high_pct, low_pct);
        Arc::get_mut(&mut reservoir)
            .expect("new reservoir has one owner")
            .before_park_wait = Some(before_park_wait);
        reservoir
    }

    fn set_effective_capacity(&self, bytes: u64, high_pct: u8, low_pct: u8) {
        self.high_bytes
            .store(percent_bytes(bytes, high_pct), Ordering::Release);
        self.low_bytes
            .store(percent_bytes(bytes, low_pct), Ordering::Release);
    }

    fn add(&self, bytes: u64) {
        self.occupancy_bytes.fetch_add(bytes, Ordering::AcqRel);
    }

    fn consume(&self, bytes: u64) {
        let previous = self.occupancy_bytes.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(previous >= bytes);
        drop(self.gate.lock().unwrap_or_else(|err| err.into_inner()));
        self.wake.notify_all();
    }

    fn wait_while_parked(&self) -> Result<(), TapeIoError> {
        let mut guard = self.gate.lock().unwrap_or_else(|err| err.into_inner());
        while self.occupancy_bytes.load(Ordering::Acquire) > self.low_bytes.load(Ordering::Acquire)
        {
            if !self.consumer_alive.load(Ordering::Acquire) {
                return Err(TapeIoError::OperationFailed(
                    "read consumer died while reservoir was parked".to_string(),
                ));
            }
            #[cfg(test)]
            if let Some(barrier) = &self.before_park_wait {
                barrier.wait();
                while self.occupancy_bytes.load(Ordering::Acquire)
                    > self.low_bytes.load(Ordering::Acquire)
                    && self.consumer_alive.load(Ordering::Acquire)
                {
                    std::hint::spin_loop();
                }
            }
            guard = self.wake.wait(guard).unwrap_or_else(|err| err.into_inner());
        }
        if !self.consumer_alive.load(Ordering::Acquire) {
            return Err(TapeIoError::OperationFailed(
                "read consumer died while reservoir was parked".to_string(),
            ));
        }
        Ok(())
    }
}

fn percent_bytes(bytes: u64, pct: u8) -> u64 {
    bytes.saturating_mul(u64::from(pct)).saturating_add(99) / 100
}

fn effective_proof_cadence(configured_bytes: u64, effective_capacity_bytes: u64) -> u64 {
    configured_bytes.min(effective_capacity_bytes / 2).max(1)
}

/// Runtime controls for one read-pipeline window.
#[derive(Clone)]
pub(crate) struct ReadPipelineConfig {
    pub(crate) reservoir_bytes: u64,
    pub(crate) high_pct: u8,
    pub(crate) low_pct: u8,
    pub(crate) ranged_frontier: bool,
    pub(crate) proof_cadence_bytes: u64,
    pub(crate) terminal: Option<Arc<ReadTerminalAccumulator>>,
}

impl ReadPipelineConfig {
    fn local_default(ranged_frontier: bool) -> Self {
        Self {
            reservoir_bytes: remanence_state::DEFAULT_READ_RESERVOIR_BYTES,
            high_pct: 90,
            low_pct: 25,
            ranged_frontier,
            proof_cadence_bytes: remanence_state::DEFAULT_RANGED_POSITION_CHECK_BYTES
                .min(remanence_state::DEFAULT_READ_RESERVOIR_BYTES / 2),
            terminal: None,
        }
    }
}

#[derive(Debug)]
struct LockedSlabPermit {
    _permit: IoMemoryPermit,
    address: usize,
    len: usize,
    unlock: unsafe extern "C" fn(*const libc::c_void, usize) -> libc::c_int,
}

impl Drop for LockedSlabPermit {
    fn drop(&mut self) {
        // SAFETY: the permit is retained alongside its reservoir slab and is
        // dropped before the channels that own the live buffer allocation.
        let _ = unsafe { (self.unlock)(self.address as *const libc::c_void, self.len) };
    }
}

fn allocate_locked_slab(
    manager: &Arc<IoMemoryReservation>,
    bytes: usize,
) -> Result<(ReadBuffer, LockedSlabPermit), String> {
    allocate_locked_slab_with(manager, bytes, |buffer| {
        // SAFETY: `buffer` is a live allocation for the duration of this call;
        // `mlock` neither takes ownership nor writes through the pointer.
        let result = unsafe { libc::mlock(buffer.as_slice().as_ptr().cast(), buffer.len()) };
        if result == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        }
    })
}

fn allocate_locked_slab_with(
    manager: &Arc<IoMemoryReservation>,
    bytes: usize,
    lock: impl FnOnce(&ReadBuffer) -> Result<(), String>,
) -> Result<(ReadBuffer, LockedSlabPermit), String> {
    let permit = manager
        .try_reserve(u64::try_from(bytes).map_err(|_| "reservoir slab size exceeds u64")?)
        .ok_or_else(|| "daemon.io_memory_ceiling has no reservoir capacity".to_string())?;
    let buffer = ReadBuffer::try_new_page_aligned(bytes)?;
    if !buffer.is_page_aligned() || buffer.capacity() != bytes {
        return Err("read reservoir slab construction lost page alignment".to_string());
    }
    lock(&buffer)
        .map_err(|err| format!("mlock read reservoir slab (check LimitMEMLOCK): {err}"))?;
    let slab = LockedSlabPermit {
        _permit: permit,
        address: buffer.as_slice().as_ptr() as usize,
        len: buffer.len(),
        unlock: libc::munlock,
    };
    Ok((buffer, slab))
}

fn send_handoff(
    tx: &SyncSender<Result<ReadDelivery, TapeIoError>>,
    delivery: ReadDelivery,
) -> Result<(), TapeIoError> {
    tx.try_send(Ok(delivery)).map_err(|error| match error {
        std::sync::mpsc::TrySendError::Full(_) => TapeIoError::OperationFailed(
            "read delivery channel construction invariant violated: handoff push would block"
                .to_string(),
        ),
        std::sync::mpsc::TrySendError::Disconnected(_) => {
            TapeIoError::OperationFailed("read delivery receiver dropped".to_string())
        }
    })
}

fn send_proof_frontier(
    source: &mut dyn BlockSource,
    tx: &SyncSender<Result<ReadDelivery, TapeIoError>>,
    expected: TapePosition,
    through_seq: u64,
    plan_records_end: u64,
    terminal: Option<&ReadTerminalAccumulator>,
) -> Result<(), TapeIoError> {
    let proof = source.prove_read_position(expected).map_err(|error| {
        if let Some(terminal) = terminal {
            terminal.record(
                ReadTerminalPriority::ScsiRoot,
                Status::internal(format!("read position proof: {error}")),
            );
        }
        error
    })?;
    tx.send(Ok(ReadDelivery::ProofFrontier {
        through_seq,
        plan_records_end,
        proof,
    }))
    .map_err(|_| TapeIoError::OperationFailed("read delivery receiver dropped".to_string()))
}

fn run_read_pipeline<T: Send>(
    source: &mut dyn BlockSource,
    block_size: usize,
    plan_records: u64,
    expected_start: TapePosition,
    config: ReadPipelineConfig,
    manager: Arc<IoMemoryReservation>,
    consume: impl FnOnce(&mut HandoffBlockSource) -> Result<T, FormatError> + Send,
) -> Result<T, FormatError> {
    if block_size == 0 {
        return Err(FormatError::invalid("block size must be nonzero"));
    }
    if plan_records == 0 {
        return Err(FormatError::invalid("read pipeline plan must be nonzero"));
    }
    let block_size_u32 = u32::try_from(block_size)
        .map_err(|_| FormatError::invalid("read block size exceeds u32"))?;
    let batch_records = source.read_batch_blocks(block_size_u32).max(1);
    let batch_bytes = block_size
        .checked_mul(batch_records as usize)
        .ok_or_else(|| FormatError::invalid("read reservoir slab size overflow"))?;
    let minimum_slabs = usize::try_from(source.read_ring_buffers())
        .map_err(|_| FormatError::invalid("read reservoir minimum does not fit usize"))?;
    let minimum_bytes = batch_bytes
        .checked_mul(minimum_slabs)
        .ok_or_else(|| FormatError::invalid("read reservoir minimum size overflow"))?;
    if config.reservoir_bytes < minimum_bytes as u64 {
        return Err(FormatError::invalid(format!(
            "read reservoir {} bytes is smaller than minimum pool {minimum_bytes} bytes",
            config.reservoir_bytes
        )));
    }
    let max_slabs_u64 = config.reservoir_bytes / batch_bytes as u64;
    let max_slabs = usize::try_from(max_slabs_u64)
        .map_err(|_| FormatError::invalid("read reservoir slab count exceeds usize"))?
        .max(minimum_slabs)
        .min(MAX_READ_RESERVOIR_SLABS);
    let effective_capacity_bytes = u64::try_from(max_slabs)
        .ok()
        .and_then(|slabs| slabs.checked_mul(batch_bytes as u64))
        .ok_or_else(|| FormatError::invalid("effective read reservoir size overflow"))?;
    let proof_cadence =
        effective_proof_cadence(config.proof_cadence_bytes, effective_capacity_bytes);
    let delivery_capacity = max_slabs
        .checked_add(READ_DELIVERY_PROOF_SLOTS)
        .ok_or_else(|| FormatError::invalid("read delivery channel capacity overflow"))?;
    let (free_tx, free_rx) = std::sync::mpsc::sync_channel(max_slabs);
    let (delivery_tx, delivery_rx) = std::sync::mpsc::sync_channel(delivery_capacity);
    let reservoir = ReservoirState::new(effective_capacity_bytes, config.high_pct, config.low_pct);
    let mut locked = Vec::with_capacity(max_slabs.min(minimum_slabs + 1));
    for _ in 0..minimum_slabs {
        let (buffer, permit) = allocate_locked_slab(&manager, batch_bytes)
            .map_err(|err| FormatError::invalid(format!("refuse read pipeline start: {err}")))?;
        free_tx
            .try_send(buffer)
            .map_err(|_| FormatError::invalid("failed to seed read reservoir free channel"))?;
        locked.push(permit);
    }
    assert!(
        locked.len() <= max_slabs,
        "allocated buffers exceed free capacity"
    );
    assert!(
        locked.len() <= delivery_capacity,
        "allocated buffers exceed delivery capacity"
    );

    let diag_session_id = NEXT_READ_DIAG_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let diag_base = ReadDiagRecord {
        phase: "open",
        session_id: diag_session_id,
        block_size_bytes: block_size as u64,
        batch_records: u64::from(batch_records),
        minimum_slabs: minimum_slabs as u64,
        maximum_slabs: max_slabs as u64,
        effective_reservoir_bytes: effective_capacity_bytes,
        reservoir_high_watermark: reservoir.high_bytes.load(Ordering::Acquire),
        occupancy_bytes: 0,
        park_cycles: 0,
        park_us: 0,
        free_wait_us: 0,
        feed_gap_total_us: 0,
        feed_gap_max_us: 0,
        feed_gap_samples: 0,
    };
    emit_read_diag(diag_base);

    let result = std::thread::scope(|scope| {
        let decode_reservoir = Arc::clone(&reservoir);
        let decode_free = free_tx.clone();
        let decode = scope.spawn(move || {
            let mut handoffs = HandoffBlockSource::new(
                delivery_rx,
                decode_free,
                block_size,
                plan_records,
                config.ranged_frontier,
                decode_reservoir,
            )?;
            let value = consume(&mut handoffs)?;
            handoffs.finish()?;
            Ok::<T, FormatError>(value)
        });
        drop(free_tx);

        let mut submitter_remaining = plan_records;
        let mut seq = 0u64;
        let mut plan_records_end = 0u64;
        let mut expected = expected_start;
        let mut allocated = minimum_slabs;
        let mut growth_warned = false;
        let mut growth_disabled = false;
        let mut root_sent = false;
        let mut bytes_since_ranged_proof = 0u64;
        let mut previous_completion = None::<Instant>;
        let mut submitter_state = WatermarkSubmitterState::Filling;

        let submitter_result = (|| -> Result<(), TapeIoError> {
            send_proof_frontier(
                source,
                &delivery_tx,
                expected,
                seq,
                plan_records_end,
                config.terminal.as_deref(),
            )?;
            while submitter_remaining > 0 {
                assert_eq!(submitter_state, WatermarkSubmitterState::Filling);
                let mut deliberate_wait = Duration::ZERO;
                if reservoir.occupancy_bytes.load(Ordering::Acquire)
                    >= reservoir.high_bytes.load(Ordering::Acquire)
                {
                    submitter_state = WatermarkSubmitterState::Gated;
                    let parked = Instant::now();
                    if config.ranged_frontier {
                        send_proof_frontier(
                            source,
                            &delivery_tx,
                            expected,
                            seq,
                            plan_records_end,
                            config.terminal.as_deref(),
                        )?;
                    }
                    reservoir.wait_while_parked()?;
                    assert_eq!(submitter_state, WatermarkSubmitterState::Gated);
                    submitter_state = WatermarkSubmitterState::ResumingProofPending;
                    // GATED -> RESUMING(rp-pending) -> FILLING. This proof is
                    // mandatory even when the low-water condition was already
                    // true and the deliberate park had zero duration.
                    send_proof_frontier(
                        source,
                        &delivery_tx,
                        expected,
                        seq,
                        plan_records_end,
                        config.terminal.as_deref(),
                    )?;
                    submitter_state = WatermarkSubmitterState::Filling;
                    let park_elapsed = parked.elapsed();
                    deliberate_wait = deliberate_wait.saturating_add(park_elapsed);
                    reservoir.park_cycles.fetch_add(1, Ordering::Relaxed);
                    reservoir.park_us.fetch_add(
                        park_elapsed.as_micros().try_into().unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                }

                let (buffer, waited) = match free_rx.try_recv() {
                    Ok(buffer) => (buffer, Duration::ZERO),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        return Err(TapeIoError::OperationFailed(
                            "read reservoir free-buffer channel disconnected".to_string(),
                        ));
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty)
                        if allocated < max_slabs && !growth_disabled =>
                    {
                        match allocate_locked_slab(&manager, batch_bytes) {
                            Ok((buffer, permit)) => {
                                locked.push(permit);
                                allocated += 1;
                                assert!(
                                    allocated <= max_slabs,
                                    "allocated buffers exceed channel capacity"
                                );
                                (buffer, Duration::ZERO)
                            }
                            Err(err) => {
                                if !growth_warned {
                                    tracing::warn!(
                                        target: "remanence_read_reservoir",
                                        reason = %err,
                                        allocated,
                                        "read reservoir growth stopped at effective cap",
                                    );
                                    growth_warned = true;
                                }
                                reservoir.set_effective_capacity(
                                    (allocated * batch_bytes) as u64,
                                    config.high_pct,
                                    config.low_pct,
                                );
                                growth_disabled = true;
                                continue;
                            }
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        let started = Instant::now();
                        let buffer = free_rx.recv().map_err(|_| {
                            TapeIoError::OperationFailed(
                                "read reservoir free-buffer channel disconnected".to_string(),
                            )
                        })?;
                        (buffer, started.elapsed())
                    }
                };
                if !waited.is_zero() {
                    deliberate_wait = deliberate_wait.saturating_add(waited);
                    reservoir.free_wait_us.fetch_add(
                        waited.as_micros().try_into().unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                }
                if waited >= T_REPROOF_INCIDENTAL {
                    send_proof_frontier(
                        source,
                        &delivery_tx,
                        expected,
                        seq,
                        plan_records_end,
                        config.terminal.as_deref(),
                    )?;
                }

                if let Some(completed) = previous_completion {
                    let feed_gap = completed.elapsed().saturating_sub(deliberate_wait);
                    let feed_gap_us = feed_gap.as_micros().try_into().unwrap_or(u64::MAX);
                    reservoir
                        .feed_gap_total_us
                        .fetch_add(feed_gap_us, Ordering::Relaxed);
                    reservoir
                        .feed_gap_max_us
                        .fetch_max(feed_gap_us, Ordering::Relaxed);
                    reservoir.feed_gap_samples.fetch_add(1, Ordering::Relaxed);
                }
                assert_eq!(submitter_state, WatermarkSubmitterState::Filling);

                let requested =
                    batch_records.min(u32::try_from(submitter_remaining).unwrap_or(u32::MAX));
                assert!(u64::from(requested) <= submitter_remaining);
                let outcome = match source.read_buffer_handoff(
                    buffer,
                    block_size_u32,
                    requested,
                    u32::try_from(submitter_remaining).unwrap_or(u32::MAX),
                ) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        if let Some(terminal) = &config.terminal {
                            terminal.record(
                                ReadTerminalPriority::ScsiRoot,
                                Status::internal(format!("tape read: {error}")),
                            );
                        }
                        match delivery_tx.send(Err(error)) {
                            Ok(()) => root_sent = true,
                            Err(send_error) => match send_error.0 {
                                Err(error) => return Err(error),
                                Ok(_) => unreachable!("submitter sent an error delivery"),
                            },
                        }
                        break;
                    }
                };
                previous_completion = Some(Instant::now());
                let records_read = outcome.handoff.records_read;
                submitter_remaining = submitter_remaining
                    .checked_sub(u64::from(records_read))
                    .ok_or_else(|| {
                        TapeIoError::OperationFailed(
                            "read submitter plan remaining underflow".to_string(),
                        )
                    })?;
                plan_records_end += u64::from(records_read);
                seq += 1;
                expected = outcome.position_after;
                let terminal = outcome.handoff.terminal_flags.filemark
                    || outcome.handoff.terminal_flags.end_of_data
                    || outcome.handoff.terminal_flags.error
                    || records_read == 0;
                let valid_bytes = outcome.handoff.valid_bytes as u64;
                bytes_since_ranged_proof = bytes_since_ranged_proof.saturating_add(valid_bytes);
                reservoir.add(valid_bytes);
                let delivery = ReadDelivery::Handoff(SequencedHandoff {
                    seq,
                    plan_records_end,
                    position_after: outcome.position_after,
                    evidence: outcome.evidence,
                    handoff: outcome.handoff,
                });
                if let Err(error) = send_handoff(&delivery_tx, delivery) {
                    reservoir.consume(valid_bytes);
                    return Err(error);
                }
                if config.ranged_frontier && bytes_since_ranged_proof >= proof_cadence {
                    send_proof_frontier(
                        source,
                        &delivery_tx,
                        expected,
                        seq,
                        plan_records_end,
                        config.terminal.as_deref(),
                    )?;
                    bytes_since_ranged_proof = 0;
                }
                if terminal || records_read != requested {
                    break;
                }
            }
            if config.ranged_frontier && seq != 0 && !root_sent {
                send_proof_frontier(
                    source,
                    &delivery_tx,
                    expected,
                    seq,
                    plan_records_end,
                    config.terminal.as_deref(),
                )?;
            }
            Ok(())
        })();
        drop(delivery_tx);
        let decode_result = decode
            .join()
            .unwrap_or_else(|_| Err(FormatError::parse("read decode thread panicked")));
        if let Err(error) = submitter_result {
            if !root_sent {
                return Err(FormatError::from(error));
            }
        }
        decode_result
    });
    emit_read_diag(ReadDiagRecord {
        phase: "close",
        reservoir_high_watermark: reservoir.high_bytes.load(Ordering::Acquire),
        occupancy_bytes: reservoir.occupancy_bytes.load(Ordering::Acquire),
        park_cycles: reservoir.park_cycles.load(Ordering::Relaxed),
        park_us: reservoir.park_us.load(Ordering::Relaxed),
        free_wait_us: reservoir.free_wait_us.load(Ordering::Relaxed),
        feed_gap_total_us: reservoir.feed_gap_total_us.load(Ordering::Relaxed),
        feed_gap_max_us: reservoir.feed_gap_max_us.load(Ordering::Relaxed),
        feed_gap_samples: reservoir.feed_gap_samples.load(Ordering::Relaxed),
        ..diag_base
    });
    result
}

// Foundation mechanism wired into the live three-thread path by TIO-6 R2b.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReadTerminalPriority {
    // declaration order defines rank — ScsiRoot first
    ScsiRoot,
    Decode,
    Sender,
}

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadTerminalDisposition {
    Emitted,
    Disconnected,
    NoCause,
    AlreadyFinalized,
}

#[allow(dead_code)]
#[derive(Default)]
struct ReadTerminalState {
    cause: Option<(ReadTerminalPriority, Status)>,
    disconnected: bool,
    finalized: bool,
}

/// Replaceable ranked terminal cause plus the post-join emission barrier.
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct ReadTerminalAccumulator {
    state: Mutex<ReadTerminalState>,
}

#[allow(dead_code)]
impl ReadTerminalAccumulator {
    pub(crate) fn record(&self, priority: ReadTerminalPriority, status: Status) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.finalized {
            return;
        }
        let should_replace = state
            .cause
            .as_ref()
            .is_none_or(|(held, _)| priority < *held);
        if should_replace {
            state.cause = Some((priority, status));
        }
    }

    pub(crate) fn record_then_close(
        &self,
        priority: ReadTerminalPriority,
        status: Status,
        close: impl FnOnce(),
    ) {
        self.record(priority, status);
        close();
    }

    pub(crate) fn mark_disconnected(&self) {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .disconnected = true;
    }

    pub(crate) fn join_and_emit(
        &self,
        joins: Vec<(
            ReadTerminalPriority,
            &'static str,
            std::thread::JoinHandle<()>,
        )>,
        mut emit: impl FnMut(Status),
    ) -> ReadTerminalDisposition {
        for (priority, stage, join) in joins {
            if join.join().is_err() {
                self.record(
                    priority,
                    Status::internal(format!("{stage} thread panicked")),
                );
            }
        }

        let status = {
            let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
            if state.finalized {
                return ReadTerminalDisposition::AlreadyFinalized;
            }
            state.finalized = true;
            if state.disconnected {
                return ReadTerminalDisposition::Disconnected;
            }
            state.cause.take().map(|(_, status)| status)
        };
        match status {
            Some(status) => {
                emit(status);
                ReadTerminalDisposition::Emitted
            }
            None => ReadTerminalDisposition::NoCause,
        }
    }

    pub(crate) fn finalize_after_join(&self) -> Option<Status> {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.finalized || state.disconnected {
            state.finalized = true;
            return None;
        }
        state.finalized = true;
        state.cause.take().map(|(_, status)| status)
    }
}

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

    fn blocking_send_observed(
        &self,
        item: ReadStreamItem,
    ) -> Result<Duration, BlockingReadStreamSendError> {
        let was_full = self.inner.tx.capacity() == 0;
        let started = Instant::now();
        let result = self.inner.tx.blocking_send(item);
        let stalled = if was_full {
            started.elapsed()
        } else {
            Duration::ZERO
        };
        match result {
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
    (
        ReadStreamSender {
            inner: Arc::new(ReadStreamSenderInner { tx }),
        },
        ReadStreamReceiver { rx },
    )
}

#[derive(Debug)]
enum BlockingReadStreamSendError {
    Closed,
}

/// Position to `tape_file_number` and stream the object's payload blocks into `sink`.
///
/// The caller is responsible for mounting the tape, setting the drive block
/// size, and positioning the source at the point from which tape-file spacing
/// is defined. Current hardware callers verify the BOT bootstrap immediately
/// before this helper, matching the established CLI archive-read path.
pub fn read_object_payload<S: RemTarEntrySink + Send + ?Sized>(
    source: &mut dyn BlockSource,
    block_size: usize,
    block_count: u64,
    tape_file_number: u32,
    manifest_sha256: Option<[u8; 32]>,
    sink: &mut S,
) -> Result<(), FormatError> {
    let config = ReadPipelineConfig::local_default(false);
    let manager = IoMemoryReservation::new(config.reservoir_bytes).map_err(FormatError::invalid)?;
    read_object_payload_with_pipeline(
        source,
        block_size,
        block_count,
        tape_file_number,
        manifest_sha256,
        sink,
        config,
        manager,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn read_object_payload_with_pipeline<S: RemTarEntrySink + Send + ?Sized>(
    source: &mut dyn BlockSource,
    block_size: usize,
    block_count: u64,
    tape_file_number: u32,
    manifest_sha256: Option<[u8; 32]>,
    sink: &mut S,
    config: ReadPipelineConfig,
    manager: Arc<IoMemoryReservation>,
) -> Result<(), FormatError> {
    let positioned = source.space(i64::from(tape_file_number), SpaceKind::Filemarks)?;
    run_read_pipeline(
        source,
        block_size,
        block_count,
        positioned.position_after,
        config,
        manager,
        move |handoffs| {
            stream_rem_tar_object_with_manifest_anchor(
                handoffs,
                block_size,
                block_count,
                sink,
                manifest_sha256,
            )
        },
    )
    .map(|_| ())
}

/// Position to one plaintext member-file range and stream only covering blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaintextFileRangeReadRequest {
    /// Fixed tape block size in bytes.
    pub block_size: usize,
    /// Filemark-delimited tape-file number containing the object.
    pub tape_file_number: u32,
    /// Absolute physical LBA of the first block in the containing tape file,
    /// when the committed catalog prefix can derive it.
    pub physical_file_start_lba: Option<u64>,
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
pub fn read_plaintext_file_range<W: Write + Send + ?Sized>(
    source: &mut dyn BlockSource,
    request: PlaintextFileRangeReadRequest,
    out: &mut W,
) -> Result<(), FormatError> {
    let config = ReadPipelineConfig::local_default(true);
    let manager = IoMemoryReservation::new(config.reservoir_bytes).map_err(FormatError::invalid)?;
    read_plaintext_file_range_with_pipeline(source, request, out, config, manager)
}

pub(crate) fn read_plaintext_file_range_with_pipeline<W: Write + Send + ?Sized>(
    source: &mut dyn BlockSource,
    request: PlaintextFileRangeReadRequest,
    out: &mut W,
    config: ReadPipelineConfig,
    manager: Arc<IoMemoryReservation>,
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
    let positioned = position_plaintext_file_range(source, request, plan.first_body_lba)?;
    run_read_pipeline(
        source,
        request.block_size,
        plan.block_count,
        positioned,
        config,
        manager,
        move |handoffs| {
            let mut block = vec![0u8; request.block_size];
            let first_block_offset = usize::try_from(plan.first_block_offset)
                .map_err(|_| FormatError::invalid("range first block offset does not fit usize"))?;
            let mut remaining = plan.range_len;
            for block_index in 0..plan.block_count {
                let read = handoffs.read_block(&mut block)?;
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
                let available = request.block_size.checked_sub(start).ok_or_else(|| {
                    FormatError::invalid("range first block offset exceeds block size")
                })?;
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
            })
        },
    )
}

/// Position a ranged read from the live cursor, preferring same-file forward
/// SPACE and absolute LOCATE before falling back to REWIND plus logical motion.
/// The caller retains the existing mandatory proof immediately before the
/// first READ by passing the returned position into `run_read_pipeline`.
fn position_plaintext_file_range(
    source: &mut dyn BlockSource,
    request: PlaintextFileRangeReadRequest,
    first_body_lba: BodyLba,
) -> Result<TapePosition, FormatError> {
    let current = source.position()?;
    if let Some(file_start_lba) = request.physical_file_start_lba {
        let target_lba = file_start_lba
            .checked_add(first_body_lba.0)
            .ok_or_else(|| FormatError::invalid("range physical target LBA overflow"))?;
        // SPACE(Blocks) stops at a filemark, so it is safe as the fast path
        // only when both cursors are inside the target tape file.
        let positioned =
            if current.partition == 0 && current.lba >= file_start_lba && current.lba <= target_lba
            {
                let delta = target_lba - current.lba;
                match i64::try_from(delta) {
                    Ok(0) => current,
                    Ok(delta) => {
                        let outcome = source.space(delta, SpaceKind::Blocks)?;
                        if outcome.stopped_at_boundary {
                            return Err(FormatError::parse(
                                "range forward SPACE stopped at an unexpected tape-file boundary",
                            ));
                        }
                        outcome.position_after
                    }
                    Err(_) => source.locate(target_lba)?,
                }
            } else {
                source.locate(target_lba)?
            };
        if positioned.partition != 0 || positioned.lba != target_lba {
            return Err(FormatError::parse(format!(
                "range positioning mismatch: expected partition 0 lba {target_lba}, observed partition {} lba {}",
                positioned.partition, positioned.lba
            )));
        }
        return Ok(positioned);
    }

    if current.partition != 0 || current.lba != 0 {
        source.rewind()?;
    }
    let mut positioned = source
        .space(i64::from(request.tape_file_number), SpaceKind::Filemarks)?
        .position_after;
    let skip_blocks = i64::try_from(first_body_lba.0)
        .map_err(|_| FormatError::invalid("range first_body_lba exceeds SPACE range"))?;
    if skip_blocks != 0 {
        positioned = source.space(skip_blocks, SpaceKind::Blocks)?.position_after;
    }
    Ok(positioned)
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
            sender_stall: Duration::ZERO,
        }
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
        match self.tx.blocking_send_observed(Ok(chunk)) {
            Ok(stalled) => {
                self.sender_stall = self.sender_stall.saturating_add(stalled);
                Ok(())
            }
            Err(BlockingReadStreamSendError::Closed) => Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "read stream closed",
            )),
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
    use std::collections::BTreeSet;
    use std::io;

    use remanence_format::{
        write_rem_tar_object, RemTarEntrySink, RemTarEntryType, RemTarFile, RemTarObjectOptions,
        RemTarStreamEntry,
    };
    use remanence_library::{VecBlockSink, VecBlockSource, VecBlockSourceCall};
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use tokio_stream::StreamExt;
    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    #[derive(Clone, Default)]
    struct SharedLogWriter(Arc<Mutex<Vec<u8>>>);

    struct SharedLogGuard(Arc<Mutex<Vec<u8>>>);

    impl io::Write for SharedLogGuard {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedLogWriter {
        type Writer = SharedLogGuard;

        fn make_writer(&'a self) -> Self::Writer {
            SharedLogGuard(Arc::clone(&self.0))
        }
    }

    fn capture_read_diag<T>(run: impl FnOnce() -> T) -> (T, Vec<Value>) {
        let writer = SharedLogWriter::default();
        let bytes = Arc::clone(&writer.0);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .without_time()
            .with_writer(writer)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, run);
        let output = String::from_utf8(
            bytes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        )
        .expect("diagnostic log is UTF-8");
        let events = output
            .lines()
            .map(|line| serde_json::from_str(line).expect("diagnostic line is JSON"))
            .filter(|event: &Value| {
                event.get("target").and_then(Value::as_str) == Some("remanence_read_diag")
            })
            .collect();
        (result, events)
    }

    fn diag_u64(event: &Value, key: &str) -> u64 {
        event
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| panic!("diagnostic field {key} is missing or not u64: {event}"))
    }

    fn assert_read_diag_schema(event: &Value) {
        let expected = [
            "batch_records",
            "block_size_bytes",
            "effective_reservoir_bytes",
            "event",
            "feed_gap_max_us",
            "feed_gap_samples",
            "feed_gap_total_us",
            "free_wait_us",
            "maximum_slabs",
            "minimum_slabs",
            "occupancy_bytes",
            "park_cycles",
            "park_us",
            "phase",
            "reservoir_high_watermark",
            "schema_version",
            "session_id",
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        let actual = event
            .as_object()
            .expect("diagnostic event is an object")
            .keys()
            .map(String::as_str)
            .filter(|key| !matches!(*key, "timestamp" | "level" | "target"))
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected, "version 1 diagnostic key set changed");
        assert_eq!(
            event.get("target").and_then(Value::as_str),
            Some("remanence_read_diag")
        );
        assert_eq!(event.get("level").and_then(Value::as_str), Some("INFO"));
        assert_eq!(diag_u64(event, "schema_version"), 1);
        assert_eq!(
            event.get("event").and_then(Value::as_str),
            Some("read_pipeline_diag")
        );
        for key in [
            "batch_records",
            "block_size_bytes",
            "effective_reservoir_bytes",
            "feed_gap_max_us",
            "feed_gap_samples",
            "feed_gap_total_us",
            "free_wait_us",
            "maximum_slabs",
            "minimum_slabs",
            "occupancy_bytes",
            "park_cycles",
            "park_us",
            "reservoir_high_watermark",
            "schema_version",
            "session_id",
        ] {
            let _ = diag_u64(event, key);
        }
    }

    struct InstrumentedReadSource {
        inner: VecBlockSource,
        in_flight: Arc<std::sync::atomic::AtomicUsize>,
        max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
        proofs: Arc<std::sync::atomic::AtomicUsize>,
        fail_proof_at: Option<usize>,
    }

    impl BlockRead for InstrumentedReadSource {
        fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
            self.inner.read_block(buf)
        }
    }

    impl BlockSource for InstrumentedReadSource {
        fn read_block_batch(
            &mut self,
            buf: &mut [u8],
            block_size_bytes: u32,
            requested_records: u32,
            remaining_records_in_file: u32,
        ) -> Result<remanence_library::ReadBatchOutcome, TapeIoError> {
            let active = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_in_flight.fetch_max(active, Ordering::AcqRel);
            let result = self.inner.read_block_batch(
                buf,
                block_size_bytes,
                requested_records,
                remaining_records_in_file,
            );
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            result
        }

        fn read_batch_blocks(&self, block_size_bytes: u32) -> u32 {
            self.inner.read_batch_blocks(block_size_bytes)
        }

        fn read_ring_buffers(&self) -> u32 {
            2
        }

        fn prove_read_position(
            &mut self,
            expected: TapePosition,
        ) -> Result<remanence_library::DevicePositionProof, TapeIoError> {
            let proof_number = self.proofs.fetch_add(1, Ordering::AcqRel) + 1;
            if self.fail_proof_at == Some(proof_number) {
                return Err(TapeIoError::OperationFailed(
                    "injected position proof failure".to_string(),
                ));
            }
            self.inner.prove_read_position(expected)
        }

        fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
            self.inner.locate(lba)
        }

        fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
            self.inner.space(count, kind)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.position()
        }
    }

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
            extensions: std::collections::BTreeMap::new(),
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
    fn channel_writer_waits_for_slow_but_alive_receiver() {
        use std::io::Write as _;

        let (tx, mut rx) = read_stream_channel_with_capacity(1);
        tx.blocking_send(Ok(pb::BytesChunk {
            data: b"held".to_vec(),
            is_last: false,
        }))
        .expect("fill channel");
        let sender = std::thread::spawn(move || {
            let mut writer = ChannelWriter::with_chunk_size(tx, 3);
            writer.write_all(b"abc").expect("alive receiver resumes");
            writer
        });
        std::thread::sleep(Duration::from_millis(5));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            assert_eq!(rx.next().await.unwrap().unwrap().data, b"held");
            assert_eq!(rx.next().await.unwrap().unwrap().data, b"abc");
        });
        let writer = sender.join().expect("sender thread");
        assert!(
            writer.sender_stall() >= Duration::from_millis(1),
            "full-channel wait must remain observable"
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
            let buffer = ReadBuffer::try_new_page_aligned(16).expect("read slab");
            let handoff = source
                .read_buffer_handoff(buffer, 4, 4, 8)
                .expect("one classified read command")
                .handoff;
            assert_eq!(&handoff.valid_data()[..4], [0; 4]);
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
    fn ranged_read_matches_full_read_slice() {
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
        let mut full_source = VecBlockSource::new(block_sink.blocks.clone());
        let mut full = Vec::new();
        let mut full_sink = CapturePayloadSink::new(&mut full);
        read_object_payload(
            &mut full_source,
            opts.chunk_size,
            layout.projected_size_blocks,
            0,
            None,
            &mut full_sink,
        )
        .unwrap();
        full_sink.finish().unwrap();

        let mut source = VecBlockSource::new(block_sink.blocks);
        let mut range = Vec::new();

        read_plaintext_file_range(
            &mut source,
            PlaintextFileRangeReadRequest {
                block_size: opts.chunk_size,
                tape_file_number: 0,
                physical_file_start_lba: Some(0),
                first_chunk_lba: layout.files[0].first_chunk_lba,
                file_size_bytes: u64::try_from(payload.len()).unwrap(),
                range_start: 400,
                range_len: 700,
            },
            &mut range,
        )
        .unwrap();

        assert_eq!(full, payload);
        assert_eq!(range, full[400..1100]);
    }

    #[test]
    fn ranged_read_issues_batched_read_commands() {
        let opts = options(512);
        let payload = (0..7000)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect::<Vec<_>>();
        let files = [RemTarFile {
            path: "funnel.bin",
            file_id: "file-funnel",
            data: payload.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut block_sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut block_sink, &opts, &files).unwrap();
        let mut source = VecBlockSource::new(block_sink.blocks).with_read_batch_blocks_for_test(4);
        let mut restored = Vec::new();

        read_plaintext_file_range(
            &mut source,
            PlaintextFileRangeReadRequest {
                block_size: opts.chunk_size,
                tape_file_number: 0,
                physical_file_start_lba: Some(0),
                first_chunk_lba: layout.files[0].first_chunk_lba,
                file_size_bytes: payload.len() as u64,
                range_start: 0,
                range_len: payload.len() as u64,
            },
            &mut restored,
        )
        .unwrap();

        assert_eq!(restored, payload);
        assert!(
            source.calls.iter().any(|call| matches!(
                call,
                VecBlockSourceCall::ReadBlockBatch {
                    requested_records,
                    ..
                } if *requested_records > 1
            )),
            "ranged reads must ride the batched READ funnel: {:?}",
            source.calls
        );
        assert!(
            source
                .calls
                .iter()
                .all(|call| !matches!(call, VecBlockSourceCall::ReadBlock { .. })),
            "ranged reads must not fall back to one synchronous READ per block: {:?}",
            source.calls
        );
    }

    #[test]
    fn ranged_read_enforces_proof_cadence() {
        let proofs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut source = InstrumentedReadSource {
            inner: VecBlockSource::new((0u8..5).map(|value| vec![value; 4]).collect())
                .with_read_batch_blocks_for_test(2),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            proofs: Arc::clone(&proofs),
            fail_proof_at: None,
        };
        let mut restored = Vec::new();

        read_plaintext_file_range_with_pipeline(
            &mut source,
            PlaintextFileRangeReadRequest {
                block_size: 4,
                tape_file_number: 0,
                physical_file_start_lba: Some(0),
                first_chunk_lba: Some(BodyLba(0)),
                file_size_bytes: 16,
                range_start: 0,
                range_len: 16,
            },
            &mut restored,
            ReadPipelineConfig {
                reservoir_bytes: 32,
                high_pct: 90,
                low_pct: 25,
                ranged_frontier: true,
                proof_cadence_bytes: 8,
                terminal: None,
            },
            IoMemoryReservation::new(64).expect("manager"),
        )
        .expect("ranged pipeline");

        assert_eq!(restored, [0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
        assert_eq!(
            proofs.load(Ordering::Acquire),
            4,
            "initial, each 8-byte frontier, and final frontier proofs are mandatory"
        );
    }

    #[test]
    fn ranged_positioning_prefers_forward_space_then_locate_then_rewind() {
        let blocks = (0u8..16).map(|value| vec![value; 4]).collect::<Vec<_>>();
        let request = PlaintextFileRangeReadRequest {
            block_size: 4,
            tape_file_number: 2,
            physical_file_start_lba: Some(2),
            first_chunk_lba: Some(BodyLba(0)),
            file_size_bytes: 16,
            range_start: 0,
            range_len: 4,
        };

        let mut forward = VecBlockSource::new(blocks.clone());
        forward.locate(3).expect("seed forward cursor");
        forward.calls.clear();
        let positioned = position_plaintext_file_range(&mut forward, request, BodyLba(5))
            .expect("forward positioning");
        assert_eq!(positioned.lba, 7);
        assert_eq!(
            forward.calls,
            [
                VecBlockSourceCall::Position,
                VecBlockSourceCall::Space {
                    count: 4,
                    kind: SpaceKind::Blocks,
                },
            ]
        );

        let mut backward = VecBlockSource::new(blocks.clone());
        backward.locate(9).expect("seed backward cursor");
        backward.calls.clear();
        let positioned = position_plaintext_file_range(&mut backward, request, BodyLba(5))
            .expect("absolute positioning");
        assert_eq!(positioned.lba, 7);
        assert_eq!(
            backward.calls,
            [
                VecBlockSourceCall::Position,
                VecBlockSourceCall::Locate { target: 7 },
            ]
        );

        let mut fallback = VecBlockSource::new(blocks);
        fallback.locate(9).expect("seed fallback cursor");
        fallback.calls.clear();
        let positioned = position_plaintext_file_range(
            &mut fallback,
            PlaintextFileRangeReadRequest {
                physical_file_start_lba: None,
                ..request
            },
            BodyLba(5),
        )
        .expect("logical fallback positioning");
        assert_eq!(positioned.lba, 7);
        assert_eq!(
            fallback.calls,
            [
                VecBlockSourceCall::Position,
                VecBlockSourceCall::Rewind,
                VecBlockSourceCall::Space {
                    count: 2,
                    kind: SpaceKind::Filemarks,
                },
                VecBlockSourceCall::Space {
                    count: 5,
                    kind: SpaceKind::Blocks,
                },
            ]
        );
    }

    #[test]
    fn terminal_accumulator_scsi_root_recorded_last_still_wins() {
        let accumulator = Arc::new(ReadTerminalAccumulator::default());
        let (sender_recorded, wait_for_sender) = std::sync::mpsc::channel();
        let sender_accumulator = Arc::clone(&accumulator);
        let sender = std::thread::spawn(move || {
            sender_accumulator.record(
                ReadTerminalPriority::Sender,
                Status::unavailable("sender stalled"),
            );
            sender_recorded.send(()).expect("signal sender cause");
        });
        let decode = std::thread::spawn(|| panic!("decode panic"));
        wait_for_sender.recv().expect("sender cause recorded");
        accumulator.record(
            ReadTerminalPriority::ScsiRoot,
            Status::data_loss("SCSI completion unknown"),
        );
        let submitter = std::thread::spawn(|| {});
        let mut emitted = Vec::new();

        let disposition = accumulator.join_and_emit(
            vec![
                (ReadTerminalPriority::Sender, "sender", sender),
                (ReadTerminalPriority::Decode, "decode", decode),
                (ReadTerminalPriority::ScsiRoot, "submitter", submitter),
            ],
            |status| emitted.push(status),
        );

        assert_eq!(disposition, ReadTerminalDisposition::Emitted);
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].message(), "SCSI completion unknown");
    }

    #[test]
    fn terminal_priority_declaration_order_defines_rank() {
        assert!(
            ReadTerminalPriority::ScsiRoot < ReadTerminalPriority::Decode
                && ReadTerminalPriority::Decode < ReadTerminalPriority::Sender
        );
    }

    #[test]
    fn proof_frontier_cannot_credit_the_next_command() {
        let reservoir = ReservoirState::new(16, 90, 25);
        let (free_tx, _free_rx) = std::sync::mpsc::sync_channel(2);
        let (delivery_tx, delivery_rx) = std::sync::mpsc::sync_channel(4);
        let mut source = VecBlockSource::new(vec![vec![0; 4]]);
        let expected = source.position().expect("position fixture");
        let proof = source
            .prove_read_position(expected)
            .expect("device proof fixture");
        delivery_tx
            .send(Ok(ReadDelivery::ProofFrontier {
                through_seq: 1,
                plan_records_end: 1,
                proof,
            }))
            .expect("send off-by-one fixture");
        let mut handoffs = HandoffBlockSource::new(delivery_rx, free_tx, 4, 1, true, reservoir)
            .expect("handoff source");
        let err = handoffs
            .read_block(&mut [0; 4])
            .expect_err("proof for next command must fail");
        assert!(
            err.to_string().contains("credits unreceived command 1"),
            "{err}"
        );
    }

    fn handoff_validation_error(mutate: impl FnOnce(&mut ReadBufferHandoff)) -> String {
        let mut tape = VecBlockSource::new(vec![vec![1, 2, 3, 4]]);
        let outcome = tape
            .read_buffer_handoff(ReadBuffer::try_new_page_aligned(4).expect("slab"), 4, 1, 1)
            .expect("handoff fixture");
        let mut handoff = outcome.handoff;
        mutate(&mut handoff);
        let occupied = handoff.valid_bytes as u64;
        let reservoir = ReservoirState::new(8, 90, 25);
        reservoir.add(occupied);
        let (free_tx, _free_rx) = std::sync::mpsc::sync_channel(2);
        let (delivery_tx, delivery_rx) = std::sync::mpsc::sync_channel(4);
        delivery_tx
            .send(Ok(ReadDelivery::Handoff(SequencedHandoff {
                seq: 1,
                plan_records_end: u64::from(handoff.records_read),
                position_after: outcome.position_after,
                evidence: outcome.evidence,
                handoff,
            })))
            .expect("delivery fixture");
        let mut source = HandoffBlockSource::new(delivery_rx, free_tx, 4, 1, false, reservoir)
            .expect("handoff source");
        source
            .read_block(&mut [0; 4])
            .expect_err("invalid handoff must fail")
            .to_string()
    }

    #[test]
    fn handoff_block_source_validation_errors_preserve_refill_wording() {
        assert_eq!(
            handoff_validation_error(|handoff| handoff.valid_bytes = 3),
            "tape operation failed: read handoff byte/record mismatch: valid_bytes=3 records_read=1 block_size=4"
        );
        assert_eq!(
            handoff_validation_error(|handoff| handoff.terminal_flags.filemark = true),
            "tape operation failed: fixed read batch stopped before object boundary: records_read=1 filemark=true"
        );
        assert_eq!(
            handoff_validation_error(|handoff| {
                handoff.records_read = 0;
                handoff.valid_bytes = 0;
            }),
            "tape operation failed: fixed read batch stopped before object boundary: records_read=0 filemark=false"
        );
        assert_eq!(
            handoff_validation_error(|handoff| handoff.records_read = 0),
            "tape operation failed: fixed read batch stopped before object boundary: records_read=0 filemark=false"
        );
    }

    #[test]
    fn reservoir_permit_rolls_back_when_mlock_fails() {
        let manager = IoMemoryReservation::new(4096).expect("manager");
        let err =
            allocate_locked_slab_with(
                &manager,
                4096,
                |_| Err("injected mlock failure".to_string()),
            )
            .expect_err("minimum slab must refuse swappable fallback");
        assert!(err.contains("LimitMEMLOCK"), "{err}");
        assert_eq!(manager.granted(), 0, "failed mlock must roll permit back");
    }

    #[test]
    fn locked_slab_permit_munlocks_before_releasing_reservation() {
        use std::sync::atomic::AtomicUsize;

        static UNLOCK_ADDRESS: AtomicUsize = AtomicUsize::new(0);
        static UNLOCK_LEN: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn record_unlock(
            address: *const libc::c_void,
            len: usize,
        ) -> libc::c_int {
            UNLOCK_ADDRESS.store(address as usize, Ordering::Release);
            UNLOCK_LEN.store(len, Ordering::Release);
            0
        }

        let manager = IoMemoryReservation::new(4096).expect("manager");
        let buffer = ReadBuffer::try_new_page_aligned(4096).expect("slab");
        let address = buffer.as_slice().as_ptr() as usize;
        let permit = manager.try_reserve(4096).expect("reservation");
        let slab = LockedSlabPermit {
            _permit: permit,
            address,
            len: buffer.len(),
            unlock: record_unlock,
        };

        drop(slab);

        assert_eq!(UNLOCK_ADDRESS.load(Ordering::Acquire), address);
        assert_eq!(UNLOCK_LEN.load(Ordering::Acquire), buffer.len());
        assert_eq!(manager.granted(), 0, "permit rolls back after munlock");
    }

    #[test]
    fn reservoir_consume_wakeup_is_not_lost_before_waiter_suspends() {
        let before_wait = Arc::new(std::sync::Barrier::new(2));
        let reservoir = ReservoirState::with_before_park_wait(2, 100, 50, before_wait.clone());
        reservoir.add(2);
        let waiter_reservoir = Arc::clone(&reservoir);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let result = waiter_reservoir.wait_while_parked();
            done_tx
                .send((result, started.elapsed()))
                .expect("report parked waiter result");
        });
        let notifier_reservoir = Arc::clone(&reservoir);
        let notifier = std::thread::spawn(move || {
            before_wait.wait();
            notifier_reservoir.consume(1);
            // A delayed rescue notification bounds the intentionally failing
            // pre-fix run instead of leaving a deadlocked test process.
            std::thread::sleep(Duration::from_millis(200));
            notifier_reservoir.wake.notify_all();
        });

        let (result, elapsed) = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("parked waiter must eventually be rescued");
        result.expect("consumer remains alive");
        assert!(
            elapsed < Duration::from_millis(100),
            "predicate transition wakeup was lost; waiter resumed after {elapsed:?}"
        );
        waiter.join().expect("waiter joins");
        notifier.join().expect("notifier joins");
    }

    #[test]
    fn ranged_consumer_death_while_parked_tears_down_without_drive_motion() {
        let before_wait = Arc::new(std::sync::Barrier::new(2));
        let reservoir = ReservoirState::with_before_park_wait(4, 100, 25, before_wait.clone());
        let mut tape = VecBlockSource::new(vec![vec![1; 4]]);
        let outcome = tape
            .read_buffer_handoff(ReadBuffer::try_new_page_aligned(4).expect("slab"), 4, 1, 1)
            .expect("completed read fixture");
        reservoir.add(outcome.handoff.valid_bytes as u64);
        let (free_tx, _free_rx) = std::sync::mpsc::sync_channel(1);
        let (_delivery_tx, delivery_rx) = std::sync::mpsc::sync_channel(1);
        let mut handoffs =
            HandoffBlockSource::new(delivery_rx, free_tx, 4, 1, true, Arc::clone(&reservoir))
                .expect("ranged handoff source");
        handoffs.current = Some(outcome.handoff);
        let drive_moves = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let waiter_reservoir = Arc::clone(&reservoir);
        let waiter_moves = Arc::clone(&drive_moves);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let started = Instant::now();
            let result = waiter_reservoir.wait_while_parked();
            if result.is_ok() {
                waiter_moves.fetch_add(1, Ordering::AcqRel);
            }
            done_tx
                .send((result, started.elapsed()))
                .expect("report submitter result");
        });

        before_wait.wait();
        drop(handoffs);
        let rescue_reservoir = Arc::clone(&reservoir);
        let rescue = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            rescue_reservoir.wake.notify_all();
        });
        let (result, elapsed) = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("dead ranged consumer must tear down");
        let error = result.expect_err("consumer death must fail closed");
        assert!(
            error
                .to_string()
                .contains("read consumer died while reservoir was parked"),
            "{error}"
        );
        assert!(
            elapsed < Duration::from_millis(100),
            "death wakeup was lost; teardown took {elapsed:?}"
        );
        assert_eq!(
            drive_moves.load(Ordering::Acquire),
            1,
            "parked submitter must issue no further drive command"
        );
        waiter.join().expect("submitter joins");
        rescue.join().expect("rescue joins");
    }

    #[test]
    fn slow_alive_consumer_remains_parked_indefinitely_without_abort() {
        let reservoir = ReservoirState::new(4, 100, 25);
        reservoir.add(4);
        let waiter_reservoir = Arc::clone(&reservoir);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            done_tx
                .send(waiter_reservoir.wait_while_parked())
                .expect("report parked result");
        });

        assert!(
            matches!(
                done_rx.recv_timeout(Duration::from_millis(150)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "an alive consumer must not be aborted by a park timeout"
        );
        reservoir.consume(3);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("low-water transition wakes submitter")
            .expect("consumer remains alive");
        waiter.join().expect("submitter joins");
    }

    #[test]
    fn ranged_proof_cadence_clamps_to_half_effective_reservoir() {
        assert_eq!(effective_proof_cadence(100, 8), 4);
        assert_eq!(effective_proof_cadence(3, 8), 3);
        assert_eq!(effective_proof_cadence(0, 1), 1);
    }

    #[test]
    fn read_pipeline_diag_open_and_close_have_stable_json_schema() {
        let mut source = InstrumentedReadSource {
            inner: VecBlockSource::new((0u8..4).map(|value| vec![value; 4]).collect())
                .with_read_batch_blocks_for_test(2),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            proofs: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_proof_at: None,
        };
        let expected = source.position().expect("window cursor");
        let (result, events) = capture_read_diag(|| {
            run_read_pipeline(
                &mut source,
                4,
                4,
                expected,
                ReadPipelineConfig {
                    reservoir_bytes: 32,
                    high_pct: 50,
                    low_pct: 25,
                    ranged_frontier: false,
                    proof_cadence_bytes: 8,
                    terminal: None,
                },
                IoMemoryReservation::new(64).expect("manager"),
                |handoffs| {
                    for _ in 0..4 {
                        let mut block = [0; 4];
                        assert_eq!(handoffs.read_block(&mut block)?, 4);
                    }
                    Ok(())
                },
            )
        });
        result.expect("pipeline");

        assert_eq!(events.len(), 2, "one OPEN and one CLOSE event are required");
        for event in &events {
            assert_read_diag_schema(event);
        }
        assert_eq!(events[0].get("phase").and_then(Value::as_str), Some("open"));
        assert_eq!(
            events[1].get("phase").and_then(Value::as_str),
            Some("close")
        );
        assert_eq!(
            diag_u64(&events[0], "session_id"),
            diag_u64(&events[1], "session_id"),
            "OPEN and CLOSE must correlate"
        );
        assert_eq!(diag_u64(&events[0], "park_cycles"), 0);
        assert_eq!(diag_u64(&events[0], "occupancy_bytes"), 0);
    }

    #[test]
    fn slow_consumer_parks_reproves_and_keeps_one_command_in_flight() {
        let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proofs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blocks = (0u8..10).map(|value| vec![value; 4]).collect();
        let mut source = InstrumentedReadSource {
            inner: VecBlockSource::new(blocks).with_read_batch_blocks_for_test(2),
            in_flight,
            max_in_flight: Arc::clone(&max_in_flight),
            proofs: Arc::clone(&proofs),
            fail_proof_at: None,
        };
        let expected = source.position().expect("window cursor");
        let manager = IoMemoryReservation::new(64).expect("manager");
        let (restored, events) = capture_read_diag(|| {
            run_read_pipeline(
                &mut source,
                4,
                10,
                expected,
                ReadPipelineConfig {
                    reservoir_bytes: 32,
                    high_pct: 50,
                    low_pct: 25,
                    ranged_frontier: false,
                    proof_cadence_bytes: 8,
                    terminal: None,
                },
                manager,
                |handoffs| {
                    let mut restored = Vec::new();
                    for _ in 0..10 {
                        let mut block = [0; 4];
                        assert_eq!(handoffs.read_block(&mut block)?, 4);
                        restored.push(block[0]);
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Ok(restored)
                },
            )
        });
        let restored = restored.expect("pipeline");

        assert_eq!(restored, (0u8..10).collect::<Vec<_>>());
        assert_eq!(events.len(), 2, "slow harness must scrape OPEN and CLOSE");
        let close = &events[1];
        assert_read_diag_schema(close);
        assert_eq!(close.get("phase").and_then(Value::as_str), Some("close"));
        assert!(
            diag_u64(close, "park_cycles") >= 2,
            "slow consumer must park the drive repeatedly: {close}"
        );
        assert!(
            diag_u64(close, "occupancy_bytes") <= diag_u64(close, "reservoir_high_watermark"),
            "reported reservoir occupancy must remain bounded: {close}"
        );
        assert_eq!(max_in_flight.load(Ordering::Acquire), 1);
        assert!(
            proofs.load(Ordering::Acquire) >= 2,
            "window-open plus every park resume must re-prove"
        );
        let requested = source
            .inner
            .calls
            .iter()
            .filter_map(|call| match call {
                VecBlockSourceCall::ReadBlockBatch {
                    requested_records, ..
                } => Some(*requested_records),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(requested, [2, 2, 2, 2, 2]);
        assert_eq!(
            requested.iter().map(|count| u64::from(*count)).sum::<u64>(),
            10
        );
    }

    #[test]
    fn ranged_proof_failure_discards_every_withheld_handoff() {
        let proofs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut source = InstrumentedReadSource {
            inner: VecBlockSource::new((0u8..4).map(|value| vec![value; 4]).collect())
                .with_read_batch_blocks_for_test(2),
            in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            proofs,
            fail_proof_at: Some(2),
        };
        let expected = source.position().expect("window cursor");
        let released = Arc::new(Mutex::new(Vec::new()));
        let released_by_decode = Arc::clone(&released);
        let err = run_read_pipeline(
            &mut source,
            4,
            4,
            expected,
            ReadPipelineConfig {
                reservoir_bytes: 32,
                high_pct: 90,
                low_pct: 25,
                ranged_frontier: true,
                proof_cadence_bytes: 32,
                terminal: None,
            },
            IoMemoryReservation::new(64).expect("manager"),
            move |handoffs| {
                for _ in 0..4 {
                    let mut block = [0; 4];
                    handoffs.read_block(&mut block)?;
                    released_by_decode
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .extend_from_slice(&block);
                }
                Ok(())
            },
        )
        .expect_err("final proof failure must poison ranged delivery");
        assert!(err.to_string().contains("position proof failure"), "{err}");
        assert!(
            released
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .is_empty(),
            "unproven ranged bytes must never be released"
        );
    }

    #[test]
    fn terminal_accumulator_translates_post_join_panic_at_stage_rank() {
        let accumulator = ReadTerminalAccumulator::default();
        accumulator.record(
            ReadTerminalPriority::Sender,
            Status::unavailable("sender failed"),
        );
        let decode = std::thread::spawn(|| panic!("decode panic"));
        let sender = std::thread::spawn(|| panic!("sender panic"));
        let mut emitted = Vec::new();

        accumulator.join_and_emit(
            vec![
                (ReadTerminalPriority::Sender, "sender", sender),
                (ReadTerminalPriority::Decode, "decode", decode),
            ],
            |status| emitted.push(status),
        );

        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].message(), "decode thread panicked");
    }

    #[test]
    fn terminal_status_emits_once_only_after_all_joins_and_record_then_close() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let accumulator = Arc::new(ReadTerminalAccumulator::default());
        let joined = Arc::new(AtomicUsize::new(0));
        let close_observed = Arc::new(AtomicUsize::new(0));
        let close_accumulator = Arc::clone(&accumulator);
        let close_inspector = Arc::clone(&accumulator);
        let close_observed_thread = Arc::clone(&close_observed);
        let sender_joined = Arc::clone(&joined);
        let sender = std::thread::spawn(move || {
            close_accumulator.record_then_close(
                ReadTerminalPriority::Sender,
                Status::unavailable("sender failed"),
                || {
                    assert!(close_inspector
                        .state
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .cause
                        .is_some());
                    close_observed_thread.store(1, Ordering::SeqCst);
                },
            );
            sender_joined.fetch_add(1, Ordering::SeqCst);
        });
        let decode_joined = Arc::clone(&joined);
        let decode = std::thread::spawn(move || {
            decode_joined.fetch_add(1, Ordering::SeqCst);
        });
        let submitter_joined = Arc::clone(&joined);
        let submitter = std::thread::spawn(move || {
            submitter_joined.fetch_add(1, Ordering::SeqCst);
        });
        let joined_at_emit = Arc::clone(&joined);
        let mut emissions = 0;

        let disposition = accumulator.join_and_emit(
            vec![
                (ReadTerminalPriority::Sender, "sender", sender),
                (ReadTerminalPriority::Decode, "decode", decode),
                (ReadTerminalPriority::ScsiRoot, "submitter", submitter),
            ],
            |_| {
                assert_eq!(joined_at_emit.load(Ordering::SeqCst), 3);
                emissions += 1;
            },
        );
        assert_eq!(close_observed.load(Ordering::SeqCst), 1);
        assert_eq!(disposition, ReadTerminalDisposition::Emitted);
        assert_eq!(emissions, 1);
        assert_eq!(
            accumulator.join_and_emit(Vec::new(), |_| emissions += 1),
            ReadTerminalDisposition::AlreadyFinalized
        );
        assert_eq!(emissions, 1);
    }

    #[test]
    fn terminal_disconnect_runs_teardown_and_skips_emission() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let accumulator = ReadTerminalAccumulator::default();
        accumulator.record(
            ReadTerminalPriority::ScsiRoot,
            Status::data_loss("would otherwise emit"),
        );
        accumulator.mark_disconnected();
        let teardown = Arc::new(AtomicUsize::new(0));
        let joins = (0..3)
            .map(|_| {
                let teardown = Arc::clone(&teardown);
                std::thread::spawn(move || {
                    teardown.fetch_add(1, Ordering::SeqCst);
                })
            })
            .collect::<Vec<_>>();
        let ranked = [
            ReadTerminalPriority::ScsiRoot,
            ReadTerminalPriority::Decode,
            ReadTerminalPriority::Sender,
        ];
        let joins = joins
            .into_iter()
            .zip(ranked)
            .map(|(join, priority)| (priority, "stage", join))
            .collect();
        let mut emissions = 0;

        let disposition = accumulator.join_and_emit(joins, |_| emissions += 1);

        assert_eq!(teardown.load(Ordering::SeqCst), 3);
        assert_eq!(disposition, ReadTerminalDisposition::Disconnected);
        assert_eq!(emissions, 0);
    }
}
