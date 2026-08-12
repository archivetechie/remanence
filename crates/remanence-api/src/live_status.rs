//! Live drive-status snapshots, byte counters, and rolling transfer rates.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio_stream::Stream;
use tonic::Status;

use crate::api_state::ApiState;
use crate::pb;
use crate::read_session_service::BytesChunkStream;

pub(crate) struct CountingBytesStream {
    pub(crate) inner: BytesChunkStream,
    pub(crate) state: ApiState,
    pub(crate) drive_uuid: Option<Vec<u8>>,
}

impl Stream for CountingBytesStream {
    type Item = Result<pb::BytesChunk, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                self.state
                    .record_drive_read_bytes(self.drive_uuid.as_deref(), chunk.data.len() as u64);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
    }
}

/// Inventory snapshot captured once at daemon startup (S6a). Static until
/// RefreshInventory (S6b); LibraryState.last_inventory_at surfaces capture time.
pub(crate) struct LibrarySnapshot {
    pub(crate) report: remanence_library::DiscoveryReport,
    pub(crate) captured_at: OffsetDateTime,
}

#[derive(Debug)]
pub(crate) struct DriveByteCounters {
    pub(crate) read_bytes: AtomicU64,
    pub(crate) write_bytes: AtomicU64,
    pub(crate) counter_epoch: u64,
    pub(crate) tape_io_staging_ring_buffers: AtomicU64,
    pub(crate) tape_io_effective_batch_blocks: AtomicU64,
    pub(crate) tape_io_gap_p50_us: AtomicU64,
    pub(crate) tape_io_gap_p95_us: AtomicU64,
    pub(crate) tape_io_gap_max_us: AtomicU64,
    pub(crate) tape_io_ioctl_p50_us: AtomicU64,
    pub(crate) tape_io_ioctl_p95_us: AtomicU64,
    pub(crate) tape_io_ioctl_max_us: AtomicU64,
    pub(crate) tape_io_cadence_us: AtomicU64,
    pub(crate) tape_io_effective_feed_bytes_per_second: AtomicU64,
    window: Mutex<RollingByteWindow>,
}

const RATE_WINDOW_MILLIS: u64 = 5_000;
const RATE_BUCKET_MILLIS: u64 = 500;
const RATE_BUCKET_COUNT: usize = 11;

#[derive(Clone, Copy, Debug)]
struct RateBucket {
    tick: u64,
    bytes: u64,
}

impl RateBucket {
    const EMPTY: Self = Self {
        tick: u64::MAX,
        bytes: 0,
    };
}

#[derive(Debug)]
pub(crate) struct RollingByteWindow {
    started_at: Instant,
    buckets: [RateBucket; RATE_BUCKET_COUNT],
}

impl RollingByteWindow {
    pub(crate) fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            buckets: [RateBucket::EMPTY; RATE_BUCKET_COUNT],
        }
    }

    pub(crate) fn record_at(&mut self, bytes: u64, now: Instant) {
        let tick = u64::try_from(now.duration_since(self.started_at).as_millis())
            .unwrap_or(u64::MAX)
            / RATE_BUCKET_MILLIS;
        let index =
            usize::try_from(tick % RATE_BUCKET_COUNT as u64).expect("rate bucket index fits usize");
        let bucket = &mut self.buckets[index];
        if bucket.tick != tick {
            *bucket = RateBucket { tick, bytes: 0 };
        }
        bucket.bytes = bucket.bytes.saturating_add(bytes);
    }

    pub(crate) fn bytes_per_second_at(&self, now: Instant) -> u64 {
        let elapsed_millis =
            u64::try_from(now.duration_since(self.started_at).as_millis()).unwrap_or(u64::MAX);
        let tick = elapsed_millis / RATE_BUCKET_MILLIS;
        let window_ticks = RATE_WINDOW_MILLIS / RATE_BUCKET_MILLIS;
        let bytes = self.buckets.iter().fold(0u64, |total, bucket| {
            if bucket.tick != u64::MAX && tick.saturating_sub(bucket.tick) < window_ticks {
                total.saturating_add(bucket.bytes)
            } else {
                total
            }
        });
        let denominator_millis = elapsed_millis.clamp(1, RATE_WINDOW_MILLIS);
        u64::try_from(
            u128::from(bytes)
                .saturating_mul(1_000)
                .checked_div(u128::from(denominator_millis))
                .unwrap_or(u128::from(u64::MAX)),
        )
        .unwrap_or(u64::MAX)
    }
}

impl DriveByteCounters {
    pub(crate) fn new(counter_epoch: u64) -> Self {
        Self {
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            counter_epoch,
            tape_io_staging_ring_buffers: AtomicU64::new(0),
            tape_io_effective_batch_blocks: AtomicU64::new(0),
            tape_io_gap_p50_us: AtomicU64::new(0),
            tape_io_gap_p95_us: AtomicU64::new(0),
            tape_io_gap_max_us: AtomicU64::new(0),
            tape_io_ioctl_p50_us: AtomicU64::new(0),
            tape_io_ioctl_p95_us: AtomicU64::new(0),
            tape_io_ioctl_max_us: AtomicU64::new(0),
            tape_io_cadence_us: AtomicU64::new(0),
            tape_io_effective_feed_bytes_per_second: AtomicU64::new(0),
            window: Mutex::new(RollingByteWindow::new(Instant::now())),
        }
    }

    pub(crate) fn record_read_bytes(&self, bytes: u64) {
        self.read_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.window
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .record_at(bytes, Instant::now());
    }

    pub(crate) fn record_write_bytes(&self, bytes: u64) {
        self.write_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.window
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .record_at(bytes, Instant::now());
    }

    pub(crate) fn configure_tape_io(&self, staging_ring_buffers: u32, effective_batch_blocks: u32) {
        self.tape_io_staging_ring_buffers
            .store(u64::from(staging_ring_buffers), Ordering::Relaxed);
        self.tape_io_effective_batch_blocks
            .store(u64::from(effective_batch_blocks), Ordering::Relaxed);
    }

    pub(crate) fn record_tape_io_diagnostics(
        &self,
        diagnostics: remanence_library::PipelinedWriteDiagnostics,
    ) {
        self.tape_io_gap_p50_us
            .store(diagnostics.gap_p50_us, Ordering::Relaxed);
        self.tape_io_gap_p95_us
            .store(diagnostics.gap_p95_us, Ordering::Relaxed);
        self.tape_io_gap_max_us
            .store(diagnostics.gap_max_us, Ordering::Relaxed);
        self.tape_io_ioctl_p50_us
            .store(diagnostics.ioctl_p50_us, Ordering::Relaxed);
        self.tape_io_ioctl_p95_us
            .store(diagnostics.ioctl_p95_us, Ordering::Relaxed);
        self.tape_io_ioctl_max_us
            .store(diagnostics.ioctl_max_us, Ordering::Relaxed);
        self.tape_io_cadence_us
            .store(diagnostics.cadence_us, Ordering::Relaxed);
        self.tape_io_effective_feed_bytes_per_second.store(
            diagnostics.effective_feed_bytes_per_second,
            Ordering::Relaxed,
        );
    }

    pub(crate) fn window_feed_bytes_per_second(&self) -> u64 {
        self.window
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .bytes_per_second_at(Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn write_bytes(&self) -> u64 {
        self.write_bytes.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct LiveStatusState {
    pub(crate) min_poll_interval: Duration,
    pub(crate) cache: RwLock<Option<(OffsetDateTime, pb::GetLiveStatusResponse)>>,
    pub(crate) drive_counters: RwLock<HashMap<Vec<u8>, Arc<DriveByteCounters>>>,
    pub(crate) mount_observations: Mutex<HashMap<(String, u32), MountObservation>>,
}

#[derive(Debug)]
pub(crate) struct MountObservation {
    barcode: String,
    seated_at: Instant,
}

impl LiveStatusState {
    pub(crate) fn new(min_poll_interval: Duration) -> Self {
        Self {
            min_poll_interval,
            cache: RwLock::new(None),
            drive_counters: RwLock::new(HashMap::new()),
            mount_observations: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn observe_mount(&self, library_serial: &str, drive: &mut pb::Drive) {
        self.observe_mount_at(library_serial, drive, Instant::now());
    }

    pub(crate) fn observe_mount_at(
        &self,
        library_serial: &str,
        drive: &mut pb::Drive,
        now: Instant,
    ) {
        // A mount observation is keyed by bay. Without one there is nothing to
        // key, and nothing to say about how long anything has been seated.
        let Some(element_address) = drive.element_address else {
            drive.mount_age_seconds = None;
            return;
        };
        let key = (library_serial.to_string(), element_address);
        let mut observations = self
            .mount_observations
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        // No barcode means nothing seated, or a label that could not be read.
        // Either way there is no mount to age, so the age is absent rather than
        // zero -- zero would mean "seated just now", which is a real state this
        // used to be indistinguishable from.
        let Some(barcode) = drive.loaded_tape_barcode.clone() else {
            observations.remove(&key);
            drive.mount_age_seconds = None;
            return;
        };
        let observation = observations.entry(key).or_insert_with(|| MountObservation {
            barcode: barcode.clone(),
            seated_at: now,
        });
        if observation.barcode != barcode {
            *observation = MountObservation {
                barcode,
                seated_at: now,
            };
        }
        drive.mount_age_seconds = Some(now.duration_since(observation.seated_at).as_secs());
    }

    pub(crate) fn counter_epoch(daemon_epoch: u64, drive_uuid: &[u8]) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(daemon_epoch.to_le_bytes());
        hasher.update(drive_uuid);
        let digest = hasher.finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("sha256 prefix is 8 bytes"))
    }

    pub(crate) fn get_or_create_counters(
        &self,
        daemon_epoch: u64,
        drive_uuid: &[u8],
    ) -> Arc<DriveByteCounters> {
        if let Some(existing) = self
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(drive_uuid)
            .cloned()
        {
            return existing;
        }
        let mut counters = self
            .drive_counters
            .write()
            .unwrap_or_else(|err| err.into_inner());
        counters
            .entry(drive_uuid.to_vec())
            .or_insert_with(|| {
                Arc::new(DriveByteCounters::new(Self::counter_epoch(
                    daemon_epoch,
                    drive_uuid,
                )))
            })
            .clone()
    }
}
