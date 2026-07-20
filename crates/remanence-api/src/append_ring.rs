//! Bounded live append receive ring.
//!
//! The async gRPC receiver fills fixed-size slabs while the synchronous RAO
//! writer consumes them through [`Read`]. One daemon-wide memory permit is
//! held for the ring's configured lifetime; slab slots recycle without
//! accumulating one permit per incoming chunk.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};
use tonic::Status;

use crate::io_memory::{IoMemoryPermit, IoMemoryReservation};

pub(crate) const APPEND_RING_SLAB_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct RingState {
    occupancy_bytes: u64,
    peak_occupancy_bytes: u64,
    producer_finished: bool,
    failure: Option<String>,
}

/// Shared occupancy, liveness, and pause state for one overlap append.
#[derive(Debug)]
pub(crate) struct AppendRingControl {
    capacity_bytes: u64,
    high_bytes: u64,
    low_bytes: u64,
    state: Mutex<RingState>,
    changed: Condvar,
    async_changed: Notify,
    tape_started: AtomicBool,
    _reservation: IoMemoryPermit,
}

impl AppendRingControl {
    fn new(
        capacity_bytes: u64,
        high_pct: u8,
        low_pct: u8,
        declared_size_bytes: u64,
        reservation: IoMemoryPermit,
    ) -> Self {
        let percentage = |pct: u8| {
            u64::try_from(u128::from(capacity_bytes).saturating_mul(u128::from(pct)) / 100)
                .unwrap_or(u64::MAX)
        };
        let high_bytes = percentage(high_pct).min(declared_size_bytes).max(1);
        let low_bytes = percentage(low_pct).min(high_bytes.saturating_sub(1));
        Self {
            capacity_bytes,
            high_bytes,
            low_bytes,
            state: Mutex::new(RingState {
                occupancy_bytes: 0,
                peak_occupancy_bytes: 0,
                producer_finished: false,
                failure: None,
            }),
            changed: Condvar::new(),
            async_changed: Notify::new(),
            tape_started: AtomicBool::new(false),
            _reservation: reservation,
        }
    }

    fn notify_changed(&self) {
        self.changed.notify_all();
        self.async_changed.notify_waiters();
    }

    fn add_occupancy(&self, bytes: usize) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.occupancy_bytes = state.occupancy_bytes.saturating_add(bytes as u64);
        state.peak_occupancy_bytes = state.peak_occupancy_bytes.max(state.occupancy_bytes);
        debug_assert!(state.occupancy_bytes <= self.capacity_bytes);
        drop(state);
        self.notify_changed();
    }

    fn consume(&self, bytes: usize) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        debug_assert!(state.occupancy_bytes >= bytes as u64);
        state.occupancy_bytes = state.occupancy_bytes.saturating_sub(bytes as u64);
        drop(state);
        self.notify_changed();
    }

    fn finish_producer(&self) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.producer_finished = true;
        drop(state);
        self.notify_changed();
    }

    fn fail(&self, message: impl Into<String>) {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if state.failure.is_none() {
            state.failure = Some(message.into());
        }
        drop(state);
        self.notify_changed();
    }

    pub(crate) async fn wait_for_prefill(&self) -> Result<(), Status> {
        loop {
            let notified = self.async_changed.notified();
            {
                let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
                if let Some(message) = &state.failure {
                    return Err(Status::invalid_argument(message.clone()));
                }
                if state.occupancy_bytes >= self.high_bytes || state.producer_finished {
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    pub(crate) fn should_pause(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        !state.producer_finished
            && state.failure.is_none()
            && state.occupancy_bytes <= self.low_bytes
    }

    pub(crate) fn prefill_satisfied(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.failure.is_none()
            && (state.occupancy_bytes >= self.high_bytes || state.producer_finished)
    }

    pub(crate) fn wait_for_resume(&self) -> Result<(), io::Error> {
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        while state.occupancy_bytes < self.high_bytes
            && !state.producer_finished
            && state.failure.is_none()
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|err| err.into_inner());
        }
        if let Some(message) = &state.failure {
            Err(io::Error::other(message.clone()))
        } else {
            Ok(())
        }
    }

    pub(crate) fn mark_tape_started(&self) {
        self.tape_started.store(true, Ordering::Release);
    }

    pub(crate) fn tape_started(&self) -> bool {
        self.tape_started.load(Ordering::Acquire)
    }

    pub(crate) fn failure_message(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .failure
            .clone()
    }

    pub(crate) fn occupancy_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .occupancy_bytes
    }

    pub(crate) fn peak_occupancy_bytes(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .peak_occupancy_bytes
    }

    pub(crate) fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    pub(crate) fn high_bytes(&self) -> u64 {
        self.high_bytes
    }

    pub(crate) fn low_bytes(&self) -> u64 {
        self.low_bytes
    }
}

struct RingSlab {
    bytes: Vec<u8>,
    _slot: OwnedSemaphorePermit,
}

enum RingMessage {
    Data(RingSlab),
    Complete,
    Error(String),
}

/// Async producer half owned by the gRPC receive loop.
pub(crate) struct AppendRingProducer {
    tx: mpsc::Sender<RingMessage>,
    recycled_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    slots: Arc<Semaphore>,
    slab_bytes: usize,
    held: Option<RingSlab>,
    control: Arc<AppendRingControl>,
}

impl AppendRingProducer {
    async fn empty_slab(&mut self) -> Result<RingSlab, Status> {
        let slot = Arc::clone(&self.slots)
            .acquire_owned()
            .await
            .map_err(|_| Status::cancelled("append ring closed"))?;
        let mut bytes = self
            .recycled_rx
            .try_recv()
            .unwrap_or_else(|_| Vec::with_capacity(self.slab_bytes));
        bytes.clear();
        Ok(RingSlab { bytes, _slot: slot })
    }

    async fn send_held(&mut self) -> Result<(), Status> {
        let slab = self
            .held
            .take()
            .expect("send_held is called only with a held slab");
        self.tx
            .send(RingMessage::Data(slab))
            .await
            .map_err(|_| Status::cancelled("append ring consumer closed"))
    }

    pub(crate) async fn push(&mut self, mut data: &[u8]) -> Result<(), Status> {
        while !data.is_empty() {
            if self
                .held
                .as_ref()
                .is_some_and(|slab| slab.bytes.len() == self.slab_bytes)
            {
                self.send_held().await?;
            }
            if self.held.is_none() {
                self.held = Some(self.empty_slab().await?);
            }
            let slab = self.held.as_mut().expect("held slab was initialized");
            let available = self.slab_bytes - slab.bytes.len();
            let take = available.min(data.len());
            slab.bytes.extend_from_slice(&data[..take]);
            self.control.add_occupancy(take);
            data = &data[take..];
        }
        Ok(())
    }

    pub(crate) async fn finish(mut self) -> Result<(), Status> {
        if self.held.is_some() {
            self.send_held().await?;
        }
        self.tx
            .send(RingMessage::Complete)
            .await
            .map_err(|_| Status::cancelled("append ring consumer closed before Finish"))?;
        self.control.finish_producer();
        Ok(())
    }

    pub(crate) async fn abort(mut self, status: &Status) {
        self.control.fail(status.message().to_string());
        if let Some(slab) = self.held.take() {
            self.control.consume(slab.bytes.len());
        }
        let _ = self
            .tx
            .send(RingMessage::Error(status.message().to_string()))
            .await;
    }
}

/// Synchronous consumer half used as the RAO file reader.
pub(crate) struct AppendRingConsumer {
    rx: mpsc::Receiver<RingMessage>,
    recycled_tx: mpsc::UnboundedSender<Vec<u8>>,
    current: Option<RingSlab>,
    offset: usize,
    complete: bool,
    control: Arc<AppendRingControl>,
}

impl Read for AppendRingConsumer {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if let Some(slab) = self.current.as_mut() {
                let available = &slab.bytes[self.offset..];
                if !available.is_empty() {
                    let take = output.len().min(available.len());
                    output[..take].copy_from_slice(&available[..take]);
                    self.offset += take;
                    self.control.consume(take);
                    return Ok(take);
                }
                let mut slab = self.current.take().expect("current slab exists");
                slab.bytes.clear();
                let _ = self.recycled_tx.send(slab.bytes);
                self.offset = 0;
            }
            if self.complete {
                return Ok(0);
            }
            match self.rx.blocking_recv() {
                Some(RingMessage::Data(slab)) => self.current = Some(slab),
                Some(RingMessage::Complete) => self.complete = true,
                Some(RingMessage::Error(message)) => {
                    return Err(io::Error::other(message));
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "append ring producer closed before Finish",
                    ));
                }
            }
        }
    }
}

/// Construct one ring after atomically reserving its complete configured size.
pub(crate) fn create_append_ring(
    io_memory: &Arc<IoMemoryReservation>,
    capacity_bytes: u64,
    high_pct: u8,
    low_pct: u8,
    declared_size_bytes: u64,
) -> Result<
    (
        AppendRingProducer,
        AppendRingConsumer,
        Arc<AppendRingControl>,
    ),
    Status,
> {
    if capacity_bytes == 0 {
        return Err(Status::invalid_argument(
            "append ring capacity must be nonzero",
        ));
    }
    let reservation = io_memory.try_reserve(capacity_bytes).ok_or_else(|| {
        Status::resource_exhausted(format!(
            "append ring reservation of {capacity_bytes} bytes exceeds remaining daemon.io_memory_ceiling capacity"
        ))
    })?;
    let slab_bytes_u64 = capacity_bytes.min(APPEND_RING_SLAB_BYTES as u64);
    let slab_count_u64 = capacity_bytes / slab_bytes_u64;
    let slab_count = usize::try_from(slab_count_u64)
        .map_err(|_| Status::invalid_argument("append ring slab count exceeds usize"))?;
    let slab_bytes = usize::try_from(slab_bytes_u64)
        .map_err(|_| Status::invalid_argument("append ring slab size exceeds usize"))?;
    // A non-slab-aligned tail remains reserved but unused. This keeps every
    // allocated slab fixed-size without ever exceeding the configured bound.
    let effective_capacity = slab_count_u64.saturating_mul(slab_bytes_u64);
    let control = Arc::new(AppendRingControl::new(
        effective_capacity,
        high_pct,
        low_pct,
        declared_size_bytes,
        reservation,
    ));
    let (tx, rx) = mpsc::channel(slab_count.max(1));
    let (recycled_tx, recycled_rx) = mpsc::unbounded_channel();
    let slots = Arc::new(Semaphore::new(slab_count.max(1)));
    Ok((
        AppendRingProducer {
            tx,
            recycled_rx,
            slots,
            slab_bytes,
            held: None,
            control: Arc::clone(&control),
        },
        AppendRingConsumer {
            rx,
            recycled_tx,
            current: None,
            offset: 0,
            complete: false,
            control: Arc::clone(&control),
        },
        control,
    ))
}

/// Emit one rate-limited occupancy sample on the established write target.
pub(crate) fn log_ring_sample(
    session_id: uuid::Uuid,
    control: &AppendRingControl,
    received_bytes: u64,
    started: Instant,
    sample_elapsed: Duration,
) {
    tracing::info!(
        target: "remanence_write_diag",
        phase = "overlap_ring",
        session_id = %session_id,
        ring_occupancy_bytes = control.occupancy_bytes(),
        ring_peak_occupancy_bytes = control.peak_occupancy_bytes(),
        ring_capacity_bytes = control.capacity_bytes(),
        ingress_bytes = received_bytes,
        ingress_rate_mib_s = crate::diagnostics::mib_per_s(received_bytes, started.elapsed()),
        sample_elapsed_ms = crate::diagnostics::duration_ms(sample_elapsed),
        "remanence_write_diag",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_producer_stalled_consumer_stays_bounded_and_recycles() {
        let capacity = 3 * APPEND_RING_SLAB_BYTES as u64;
        let manager = IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, mut consumer, control) =
            create_append_ring(&manager, capacity, 90, 25, 4 * capacity).expect("ring");
        let payload = vec![0x5a; 4 * APPEND_RING_SLAB_BYTES];
        let producer_task = tokio::spawn(async move {
            producer.push(&payload).await.expect("push payload");
            producer.finish().await.expect("finish producer");
        });

        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !producer_task.is_finished(),
            "producer must backpressure while the consumer is stalled"
        );
        assert_eq!(control.occupancy_bytes(), capacity);
        assert!(control.peak_occupancy_bytes() <= capacity);
        assert_eq!(manager.granted(), capacity);

        let consumed = tokio::task::spawn_blocking(move || {
            let mut output = Vec::new();
            consumer.read_to_end(&mut output).expect("consume ring");
            output
        })
        .await
        .expect("consumer task");
        producer_task.await.expect("producer task");
        assert_eq!(consumed, vec![0x5a; 4 * APPEND_RING_SLAB_BYTES]);
        assert_eq!(control.occupancy_bytes(), 0);
        drop(control);
        assert_eq!(manager.granted(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_producer_and_slow_consumer_preserve_bytes_within_bound() {
        let capacity = 2 * APPEND_RING_SLAB_BYTES as u64;
        let manager = IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, mut consumer, control) =
            create_append_ring(&manager, capacity, 75, 25, capacity).expect("ring");
        let producer_task = tokio::spawn(async move {
            for value in 0u8..8 {
                producer
                    .push(&vec![value; APPEND_RING_SLAB_BYTES / 4])
                    .await
                    .expect("slow push");
                tokio::task::yield_now().await;
            }
            producer.finish().await.expect("finish producer");
        });
        let consumer_task = tokio::task::spawn_blocking(move || {
            let mut output = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let read = consumer.read(&mut chunk).expect("slow read");
                if read == 0 {
                    break;
                }
                output.extend_from_slice(&chunk[..read]);
                std::thread::yield_now();
            }
            output
        });
        producer_task.await.expect("producer task");
        let output = consumer_task.await.expect("consumer task");
        assert_eq!(output.len(), 2 * APPEND_RING_SLAB_BYTES);
        for value in 0u8..8 {
            let start = value as usize * APPEND_RING_SLAB_BYTES / 4;
            assert!(output[start..start + APPEND_RING_SLAB_BYTES / 4]
                .iter()
                .all(|byte| *byte == value));
        }
        assert!(control.peak_occupancy_bytes() <= capacity);
        drop(control);
        assert_eq!(manager.granted(), 0);
    }

    #[tokio::test]
    async fn cancellation_and_error_release_the_single_ring_reservation() {
        let capacity = APPEND_RING_SLAB_BYTES as u64;
        let manager = IoMemoryReservation::new(capacity).expect("manager");
        let (mut producer, mut consumer, control) =
            create_append_ring(&manager, capacity, 90, 25, capacity).expect("ring");
        producer.push(b"partial").await.expect("partial push");
        let status = Status::cancelled("client cancelled overlap append");
        producer.abort(&status).await;
        assert_eq!(control.occupancy_bytes(), 0);
        let error = tokio::task::spawn_blocking(move || {
            let mut output = [0u8; 16];
            consumer
                .read(&mut output)
                .expect_err("abort reaches consumer")
        })
        .await
        .expect("consumer task");
        assert!(error.to_string().contains("client cancelled"), "{error}");
        drop(control);
        assert_eq!(manager.granted(), 0);
    }

    #[test]
    fn non_aligned_capacity_never_allocates_past_the_reserved_bound() {
        let capacity = APPEND_RING_SLAB_BYTES as u64 + 17;
        let manager = IoMemoryReservation::new(capacity).expect("manager");
        let (_producer, _consumer, control) =
            create_append_ring(&manager, capacity, 90, 25, capacity).expect("ring");
        assert_eq!(control.capacity_bytes(), APPEND_RING_SLAB_BYTES as u64);
        assert!(control.capacity_bytes() <= capacity);
        assert_eq!(manager.granted(), capacity);
    }

    #[test]
    fn ring_reservation_above_shared_ceiling_is_resource_exhausted() {
        let manager = IoMemoryReservation::new(APPEND_RING_SLAB_BYTES as u64).expect("manager");
        let status = match create_append_ring(
            &manager,
            2 * APPEND_RING_SLAB_BYTES as u64,
            90,
            25,
            4 * APPEND_RING_SLAB_BYTES as u64,
        ) {
            Ok(_) => panic!("ring must fit the shared ceiling before admission"),
            Err(status) => status,
        };
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert!(status.message().contains("io_memory_ceiling"), "{status}");
        assert_eq!(manager.granted(), 0);
    }
}
