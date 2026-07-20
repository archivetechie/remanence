//! Hash-chained append-only audit log.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};

use ciborium::value::Value as CborValue;
use remanence_crc::crc64_xz;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::StateError;

const AUDIT_MAGIC: &[u8; 7] = b"REMAUD\x01";
const AUDIT_SCHEMA_VERSION: u16 = 1;
const SEGMENT_DATE_LEN: usize = 10;
const HEADER_WITHOUT_CRC_LEN: usize = AUDIT_MAGIC.len() + 2 + SEGMENT_DATE_LEN + 32;
const HEADER_LEN: usize = HEADER_WITHOUT_CRC_LEN + 8;
const RECORD_HASH_LEN: usize = 32;
const RECORD_CRC_LEN: usize = 8;
const RECORD_TRAILER_LEN: usize = RECORD_HASH_LEN + RECORD_CRC_LEN;
const MAX_RECORD_LEN: u32 = 64 * 1024 * 1024;
const DEFAULT_CLOCK_FORWARD_TOLERANCE_SECONDS: i64 = 300;

/// Actor responsible for an audit event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditActor {
    /// Daemon-internal actor.
    System,
    /// Authenticated user actor.
    User(String),
    /// Authenticated service actor.
    Service(String),
}

impl AuditActor {
    /// Actor for operator-invoked local CLI mutations.
    ///
    /// Resolves the invoking login from the environment so locally audited
    /// operations (tape retire, tape init provisioning) record who ran them.
    /// Falls back to [`AuditActor::System`] when no login is available.
    pub fn local_user() -> Self {
        std::env::var("USER")
            .or_else(|_| std::env::var("LOGNAME"))
            .ok()
            .filter(|login| !login.trim().is_empty())
            .map(Self::User)
            .unwrap_or(Self::System)
    }
}

/// Source layer that emitted an audit event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceLayer {
    /// Layer 2 library/drive handling.
    Layer2,
    /// Layer 3b body-format handling.
    Layer3b,
    /// Layer 3c parity handling.
    Layer3c,
    /// Layer 4 local state handling.
    Layer4,
    /// Layer 5 API/session handling.
    Layer5,
}

/// Audit event kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuditEvent {
    /// Request was received.
    RequestReceived,
    /// Operation started.
    OperationStarted,
    /// Operation made progress.
    OperationProgress,
    /// Operation finished successfully.
    OperationFinished,
    /// Operation failed.
    OperationFailed,
    /// Cancellation was requested.
    CancelRequested,
    /// Cancellation happened before dispatch.
    CancelledBeforeDispatch,
    /// Operation completed after cancellation.
    CompletedAfterCancel,
    /// Cancellation could not be honored.
    CancellationRejected,
    /// Completion status is unknown after an error.
    CompletionUnknown,
    /// Session opened.
    SessionOpened,
    /// Session checkpointed.
    SessionCheckpointed,
    /// Session closed.
    SessionClosed,
    /// Session was observed without a matching local owner.
    SessionOrphaned,
    /// Session was lost across restart.
    SessionLostByRestart,
    /// Clock moved backward.
    ClockRegressionObserved,
    /// Clock jumped forward beyond tolerance.
    ClockForwardJumpObserved,
    /// Hardware or lower-layer state changed in a way operators should inspect.
    HardwareWarning,
    /// Lower-layer recovery event.
    RecoveryEvent,
    /// Configuration was accepted and loaded.
    ConfigLoaded,
    /// Configuration was rejected.
    ConfigRejected,
    /// Rebuildable index was rebuilt from authoritative sources.
    IndexRebuilt,
    /// Daemon entered read-only mode.
    ReadOnlyModeEntered,
    /// Daemon left read-only mode.
    ReadOnlyModeLeft,
    /// An audit write failed.
    AuditWriteFailed,
    /// A tape identity was permanently retired.
    TapeRetired,
    /// A tape was provisioned after a bootstrap write.
    TapeProvisioned,
    /// A tape was assigned to an operator-defined pool.
    TapePoolAssigned,
    /// A tape was sealed against future appends.
    TapeSealed,
    /// A drive was permanently removed from the managed fleet.
    DriveRetired,
    /// Operator metadata was attached to a drive.
    DriveAnnotated,
    /// A drive completed a verified cleaning cycle.
    DriveCleaned,
    /// A cleaning cartridge reached terminal expiry.
    CleaningCartridgeExpired,
    /// A cleaning cartridge was registered for use.
    CleaningCartridgeRegistered,
    /// A drive was fenced from session admission.
    DriveFenced,
    /// A drive fence was released.
    DriveUnfenced,
    /// A standing alarm was acknowledged.
    AlarmAcked,
    /// A standing alarm was raised or refreshed.
    AlarmRaised,
    /// A standing alarm was cleared.
    AlarmCleared,
    /// A tape-I/O quarantine fence was raised.
    TapeIoFenceRaised,
    /// A tape-I/O quarantine fence was released.
    TapeIoFenceReleased,
    /// A drive-health measurement was recorded.
    DriveHealthObserved,
}

/// Subject of an audit event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditSubject {
    /// Subject kind, such as `tape`, `operation`, or `library`.
    pub kind: String,
    /// Optional stable subject identifier.
    pub id: Option<String>,
}

/// Caller-supplied fields for a new audit append.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditEventRecord {
    /// Actor responsible for this event.
    pub actor: AuditActor,
    /// Source layer that emitted this event.
    pub source_layer: SourceLayer,
    /// Operation UUID, when applicable.
    pub operation_id: Option<Uuid>,
    /// Session UUID, when applicable.
    pub session_id: Option<Uuid>,
    /// Idempotency key, when applicable.
    pub idempotency_key: Option<Uuid>,
    /// Event kind.
    pub event: AuditEvent,
    /// Event subject.
    pub subject: AuditSubject,
    /// Structured event detail.
    pub detail: BTreeMap<String, CborValue>,
}

/// Durable audit record stored in the hash chain.
#[derive(Clone, Debug, PartialEq)]
pub struct AuditRecord {
    /// Audit schema version.
    pub schema_version: u16,
    /// Unique record UUID.
    pub record_uuid: Uuid,
    /// Monotonic sequence across all segments.
    pub sequence: u64,
    /// RFC3339 UTC timestamp.
    pub timestamp_utc: String,
    /// Host identity string.
    pub host_id: String,
    /// Process identifier.
    pub process_id: u32,
    /// Producing crate version plus build-time source-control description.
    ///
    /// `None` is accepted only when replaying records written before this
    /// field was introduced; every new append sets it.
    pub software_build: Option<String>,
    /// Actor responsible for this event.
    pub actor: AuditActor,
    /// Source layer that emitted this event.
    pub source_layer: SourceLayer,
    /// Operation UUID, when applicable.
    pub operation_id: Option<Uuid>,
    /// Session UUID, when applicable.
    pub session_id: Option<Uuid>,
    /// Idempotency key, when applicable.
    pub idempotency_key: Option<Uuid>,
    /// Event kind.
    pub event: AuditEvent,
    /// Event subject.
    pub subject: AuditSubject,
    /// Structured event detail.
    pub detail: BTreeMap<String, CborValue>,
}

/// Receipt returned after a durable audit append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditReceipt {
    /// Appended sequence number.
    pub sequence: u64,
    /// Appended record UUID.
    pub record_uuid: Uuid,
    /// Hash-chain value for the appended record.
    pub record_hash: [u8; 32],
    /// Whether `fsync` completed for this append.
    pub fsync_completed: bool,
}

/// Append surface for audit sinks.
pub trait AuditSink {
    /// Append one event and return its durable receipt.
    fn append(&mut self, event: AuditEventRecord) -> Result<AuditReceipt, StateError>;
}

/// File-backed append-only audit log.
#[derive(Debug)]
pub struct FileAuditLog {
    dir: PathBuf,
    segment_date: String,
    file: File,
    previous_record_hash: [u8; 32],
    next_sequence: u64,
    last_timestamp_utc: Option<OffsetDateTime>,
    last_monotonic: Option<Instant>,
    clock_forward_tolerance: Option<Duration>,
    fsync: bool,
}

impl FileAuditLog {
    /// Open the audit log for today's UTC segment.
    pub fn open(dir: impl AsRef<Path>, fsync: bool) -> Result<Self, StateError> {
        let date = utc_segment_date();
        Self::open_for_date(dir, &date, fsync)
    }

    /// Open the audit log with an explicit forward-clock-jump tolerance.
    pub fn open_with_clock_forward_tolerance(
        dir: impl AsRef<Path>,
        fsync: bool,
        clock_forward_tolerance: Option<Duration>,
    ) -> Result<Self, StateError> {
        let date = utc_segment_date();
        Self::open_for_date_with_clock_forward_tolerance(dir, &date, fsync, clock_forward_tolerance)
    }

    /// Open the audit log for a specific UTC date segment.
    pub fn open_for_date(
        dir: impl AsRef<Path>,
        segment_date: &str,
        fsync: bool,
    ) -> Result<Self, StateError> {
        Self::open_for_date_with_clock_forward_tolerance(
            dir,
            segment_date,
            fsync,
            Some(Duration::seconds(DEFAULT_CLOCK_FORWARD_TOLERANCE_SECONDS)),
        )
    }

    /// Open the audit log for a specific UTC date segment and clock tolerance.
    pub fn open_for_date_with_clock_forward_tolerance(
        dir: impl AsRef<Path>,
        segment_date: &str,
        fsync: bool,
        clock_forward_tolerance: Option<Duration>,
    ) -> Result<Self, StateError> {
        validate_segment_date(segment_date)?;
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(|err| StateError::io_at("create audit dir", &dir, err))?;

        let replay = replay_segments_summary(&dir)?;
        let effective_segment_date = replay
            .latest_segment_date
            .as_ref()
            .filter(|latest| segment_date < latest.as_str())
            .map(String::as_str)
            .unwrap_or(segment_date);
        let last_timestamp_utc = replay
            .last_timestamp_utc
            .as_deref()
            .map(parse_timestamp_utc)
            .transpose()?;
        let path = segment_path(&dir, effective_segment_date);
        if !path.exists() {
            write_segment_header(&path, effective_segment_date, replay.terminal_hash)?;
        } else {
            let mut discard_record = |_| ControlFlow::Continue(());
            let segment = replay_segment(
                &path,
                Some(replay.previous_hash_for(effective_segment_date)?),
                replay.first_sequence_for(effective_segment_date)?,
                &mut discard_record,
            )?;
            let file = File::options()
                .write(true)
                .open(&path)
                .map_err(|err| StateError::io_at("open audit segment for truncate", &path, err))?;
            file.set_len(segment.valid_len)
                .map_err(|err| StateError::io_at("truncate torn audit tail", &path, err))?;
            file.sync_all()
                .map_err(|err| StateError::io_at("fsync truncated audit segment", &path, err))?;
        }

        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|err| StateError::io_at("open audit segment", &path, err))?;

        Ok(Self {
            dir,
            segment_date: effective_segment_date.to_string(),
            file,
            previous_record_hash: replay.terminal_hash,
            next_sequence: replay.next_sequence,
            last_timestamp_utc,
            last_monotonic: None,
            clock_forward_tolerance,
            fsync,
        })
    }

    /// Replay all audit records in a directory into a collection.
    ///
    /// Call [`Self::replay_incremental`] when the caller does not inherently
    /// need the complete history resident in memory.
    pub fn replay(dir: impl AsRef<Path>) -> Result<Vec<AuditRecord>, StateError> {
        let mut records = Vec::new();
        Self::replay_incremental(dir, |record| {
            records.push(record);
            ControlFlow::Continue(())
        })?;
        Ok(records)
    }

    /// Replay and validate audit records one at a time in chain order.
    ///
    /// The visitor may stop replay early with [`ControlFlow::Break`]. Apart
    /// from the currently decoded record, this path does not retain history.
    pub fn replay_incremental(
        dir: impl AsRef<Path>,
        mut visitor: impl FnMut(AuditRecord) -> ControlFlow<()>,
    ) -> Result<(), StateError> {
        replay_segments(dir.as_ref(), &mut visitor)?;
        Ok(())
    }

    /// Current segment path.
    pub fn segment_path(&self) -> PathBuf {
        segment_path(&self.dir, &self.segment_date)
    }

    fn rotate_to_date(&mut self, segment_date: &str) -> Result<(), StateError> {
        if segment_date == self.segment_date {
            return Ok(());
        }

        let rotated = Self::open_for_date_with_clock_forward_tolerance(
            &self.dir,
            segment_date,
            self.fsync,
            self.clock_forward_tolerance,
        )?;
        self.segment_date = rotated.segment_date;
        self.file = rotated.file;
        self.previous_record_hash = rotated.previous_record_hash;
        self.next_sequence = rotated.next_sequence;
        self.last_timestamp_utc = rotated.last_timestamp_utc;
        self.last_monotonic = None;
        Ok(())
    }

    fn append_with_observed_clock(
        &mut self,
        event: AuditEventRecord,
        timestamp_utc: OffsetDateTime,
        observed_monotonic: Instant,
    ) -> Result<AuditReceipt, StateError> {
        let (receipt, _) =
            self.append_with_observed_clock_and_record(event, timestamp_utc, observed_monotonic)?;
        Ok(receipt)
    }

    fn append_with_observed_clock_and_record(
        &mut self,
        event: AuditEventRecord,
        timestamp_utc: OffsetDateTime,
        observed_monotonic: Instant,
    ) -> Result<(AuditReceipt, AuditRecord), StateError> {
        let segment_date = self.segment_date_for_append(timestamp_utc);
        self.rotate_to_date(&segment_date)?;
        self.emit_clock_warning_if_needed(timestamp_utc, observed_monotonic)?;
        self.append_record(event, timestamp_utc, Some(observed_monotonic))
    }

    fn segment_date_for_append(&self, timestamp_utc: OffsetDateTime) -> String {
        let candidate = segment_date_for_timestamp(timestamp_utc);
        if candidate < self.segment_date {
            self.segment_date.clone()
        } else {
            candidate
        }
    }

    fn emit_clock_warning_if_needed(
        &mut self,
        timestamp_utc: OffsetDateTime,
        observed_monotonic: Instant,
    ) -> Result<(), StateError> {
        let Some(previous_timestamp) = self.last_timestamp_utc else {
            return Ok(());
        };

        if timestamp_utc < previous_timestamp {
            let mut detail = BTreeMap::new();
            detail.insert(
                "previous_timestamp_utc".to_string(),
                CborValue::Text(format_timestamp_utc(previous_timestamp)?),
            );
            detail.insert(
                "observed_timestamp_utc".to_string(),
                CborValue::Text(format_timestamp_utc(timestamp_utc)?),
            );
            let _ = self.append_record(
                clock_warning_event(AuditEvent::ClockRegressionObserved, detail),
                timestamp_utc,
                Some(observed_monotonic),
            )?;
            return Ok(());
        }

        let (Some(previous_monotonic), Some(tolerance)) =
            (self.last_monotonic, self.clock_forward_tolerance)
        else {
            return Ok(());
        };
        let wall_delta = timestamp_utc - previous_timestamp;
        let monotonic_delta =
            duration_from_std(observed_monotonic.duration_since(previous_monotonic));
        if wall_delta > monotonic_delta && wall_delta - monotonic_delta > tolerance {
            let mut detail = BTreeMap::new();
            detail.insert(
                "previous_timestamp_utc".to_string(),
                CborValue::Text(format_timestamp_utc(previous_timestamp)?),
            );
            detail.insert(
                "observed_timestamp_utc".to_string(),
                CborValue::Text(format_timestamp_utc(timestamp_utc)?),
            );
            detail.insert(
                "wall_delta_seconds".to_string(),
                CborValue::Integer(wall_delta.whole_seconds().into()),
            );
            detail.insert(
                "monotonic_delta_seconds".to_string(),
                CborValue::Integer(monotonic_delta.whole_seconds().into()),
            );
            detail.insert(
                "tolerance_seconds".to_string(),
                CborValue::Integer(tolerance.whole_seconds().into()),
            );
            let _ = self.append_record(
                clock_warning_event(AuditEvent::ClockForwardJumpObserved, detail),
                timestamp_utc,
                Some(observed_monotonic),
            )?;
        }

        Ok(())
    }

    fn append_record(
        &mut self,
        event: AuditEventRecord,
        timestamp_utc: OffsetDateTime,
        observed_monotonic: Option<Instant>,
    ) -> Result<(AuditReceipt, AuditRecord), StateError> {
        validate_detail_keys(&event.detail)?;
        let record = AuditRecord {
            schema_version: AUDIT_SCHEMA_VERSION,
            record_uuid: Uuid::new_v4(),
            sequence: self.next_sequence,
            timestamp_utc: format_timestamp_utc(timestamp_utc)?,
            host_id: host_id(),
            process_id: std::process::id(),
            software_build: Some(software_build().to_string()),
            actor: event.actor,
            source_layer: event.source_layer,
            operation_id: event.operation_id,
            session_id: event.session_id,
            idempotency_key: event.idempotency_key,
            event: event.event,
            subject: event.subject,
            detail: event.detail,
        };
        let payload = canonical_record_cbor(&record)?;
        let record_hash = record_hash(self.previous_record_hash, &payload);
        let record_len = payload
            .len()
            .checked_add(RECORD_TRAILER_LEN)
            .and_then(|len| u32::try_from(len).ok())
            .ok_or_else(|| StateError::AuditWriteFailed("audit record too large".to_string()))?;
        if record_len > MAX_RECORD_LEN {
            return Err(StateError::AuditWriteFailed(format!(
                "audit record length {record_len} exceeds max {MAX_RECORD_LEN}"
            )));
        }

        let mut frame = Vec::with_capacity(4 + record_len as usize);
        frame.extend_from_slice(&record_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&record_hash);
        let crc = crc64_xz(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());

        let append_start = self
            .file
            .metadata()
            .map_err(|err| StateError::io("stat audit segment before append", err))?
            .len();
        if let Err(err) = self.file.write_all(&frame) {
            self.rollback_failed_append(append_start);
            return Err(StateError::io("append audit record", err));
        }
        if self.fsync {
            if let Err(err) = self.file.sync_all() {
                self.rollback_failed_append(append_start);
                return Err(StateError::io("fsync audit record", err));
            }
        }

        self.previous_record_hash = record_hash;
        self.next_sequence += 1;
        self.last_timestamp_utc = Some(timestamp_utc);
        self.last_monotonic = observed_monotonic;

        let receipt = AuditReceipt {
            sequence: record.sequence,
            record_uuid: record.record_uuid,
            record_hash,
            fsync_completed: self.fsync,
        };
        Ok((receipt, record))
    }

    fn rollback_failed_append(&mut self, append_start: u64) {
        let _ = self.file.set_len(append_start);
        let _ = self.file.sync_all();
    }

    /// Append one event and return both the durable receipt and projected record.
    pub fn append_and_return_record(
        &mut self,
        event: AuditEventRecord,
    ) -> Result<(AuditReceipt, AuditRecord), StateError> {
        self.append_with_observed_clock_and_record(event, OffsetDateTime::now_utc(), Instant::now())
    }

    #[cfg(test)]
    fn append_at_for_tests(
        &mut self,
        event: AuditEventRecord,
        timestamp_utc: OffsetDateTime,
    ) -> Result<AuditReceipt, StateError> {
        self.append_with_observed_clock(event, timestamp_utc, Instant::now())
    }

    #[cfg(test)]
    fn append_with_observed_clock_for_tests(
        &mut self,
        event: AuditEventRecord,
        timestamp_utc: OffsetDateTime,
        observed_monotonic: Instant,
    ) -> Result<AuditReceipt, StateError> {
        self.append_with_observed_clock(event, timestamp_utc, observed_monotonic)
    }
}

impl AuditSink for FileAuditLog {
    fn append(&mut self, event: AuditEventRecord) -> Result<AuditReceipt, StateError> {
        self.append_with_observed_clock(event, OffsetDateTime::now_utc(), Instant::now())
    }
}

#[derive(Debug)]
struct ReplaySummary {
    terminal_hash: [u8; 32],
    next_sequence: u64,
    segment_previous: BTreeMap<String, [u8; 32]>,
    segment_first_sequence: BTreeMap<String, u64>,
    latest_segment_date: Option<String>,
    last_timestamp_utc: Option<String>,
}

impl ReplaySummary {
    fn previous_hash_for(&self, segment_date: &str) -> Result<[u8; 32], StateError> {
        self.segment_previous
            .get(segment_date)
            .copied()
            .ok_or_else(|| StateError::AuditCorrupt(format!("missing segment {segment_date}")))
    }

    fn first_sequence_for(&self, segment_date: &str) -> Result<u64, StateError> {
        self.segment_first_sequence
            .get(segment_date)
            .copied()
            .ok_or_else(|| StateError::AuditCorrupt(format!("missing segment {segment_date}")))
    }
}

#[derive(Debug)]
struct SegmentReplay {
    record_count: u64,
    terminal_hash: [u8; 32],
    valid_len: u64,
    last_timestamp_utc: Option<String>,
    stopped: bool,
}

fn replay_segments(
    dir: &Path,
    visitor: &mut impl FnMut(AuditRecord) -> ControlFlow<()>,
) -> Result<ReplaySummary, StateError> {
    replay_segments_impl(dir, visitor)
}

fn replay_segments_summary(dir: &Path) -> Result<ReplaySummary, StateError> {
    replay_segments_impl(dir, &mut |_| ControlFlow::Continue(()))
}

// Memory bound: record bodies stream through the visitor one at a time, but
// the sorted path list and the two per-segment maps below are retained for
// the whole replay — O(segment count), not O(1). Accepted at the 2026-07-18
// re-gate: segments rotate by date, so the count grows by file, not by
// record, and streaming segment discovery is not worth its complexity here.
fn replay_segments_impl(
    dir: &Path,
    visitor: &mut impl FnMut(AuditRecord) -> ControlFlow<()>,
) -> Result<ReplaySummary, StateError> {
    let mut paths = audit_segment_paths(dir)?;
    paths.sort();

    let mut previous_hash = [0u8; 32];
    let mut next_sequence = 1u64;
    let mut segment_previous = BTreeMap::new();
    let mut segment_first_sequence = BTreeMap::new();
    let mut latest_segment_date = None;
    let mut last_timestamp_utc = None;

    for path in paths {
        let date = segment_date_from_path(&path)?;
        segment_previous.insert(date.clone(), previous_hash);
        segment_first_sequence.insert(date.clone(), next_sequence);
        let segment = replay_segment(&path, Some(previous_hash), next_sequence, visitor)?;
        next_sequence += segment.record_count;
        previous_hash = segment.terminal_hash;
        latest_segment_date = Some(date);
        if segment.last_timestamp_utc.is_some() {
            last_timestamp_utc = segment.last_timestamp_utc;
        }
        if segment.stopped {
            break;
        }
    }

    Ok(ReplaySummary {
        terminal_hash: previous_hash,
        next_sequence,
        segment_previous,
        segment_first_sequence,
        latest_segment_date,
        last_timestamp_utc,
    })
}

fn replay_segment(
    path: &Path,
    expected_previous_hash: Option<[u8; 32]>,
    first_expected_sequence: u64,
    visitor: &mut impl FnMut(AuditRecord) -> ControlFlow<()>,
) -> Result<SegmentReplay, StateError> {
    let mut file =
        File::open(path).map_err(|err| StateError::io_at("open audit segment", path, err))?;
    let file_len = file
        .metadata()
        .map_err(|err| StateError::io_at("stat audit segment", path, err))?
        .len();
    let header = read_segment_header(&mut file, path)?;
    let expected_date = segment_date_from_path(path)?;
    if header.segment_date != expected_date {
        return Err(StateError::AuditCorrupt(format!(
            "audit segment date {} does not match filename {expected_date}",
            header.segment_date
        )));
    }
    if let Some(expected) = expected_previous_hash {
        if header.previous_segment_terminal_hash != expected {
            return Err(StateError::AuditCorrupt(format!(
                "segment {} previous hash does not match chain",
                path.display()
            )));
        }
    }

    let mut record_count = 0u64;
    let mut previous_hash = header.previous_segment_terminal_hash;
    let mut expected_sequence = first_expected_sequence;
    let mut valid_len = HEADER_LEN as u64;
    let mut last_timestamp_utc = None;

    loop {
        let record_start = file
            .stream_position()
            .map_err(|err| StateError::io_at("seek audit segment", path, err))?;
        let mut len_buf = [0u8; 4];
        match file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(SegmentReplay {
                    record_count,
                    terminal_hash: previous_hash,
                    valid_len,
                    last_timestamp_utc,
                    stopped: false,
                });
            }
            Err(err) => return Err(StateError::io_at("read audit record length", path, err)),
        }

        let record_len = u32::from_le_bytes(len_buf);
        if record_len < RECORD_TRAILER_LEN as u32 || record_len > MAX_RECORD_LEN {
            return Err(StateError::AuditCorrupt(format!(
                "invalid audit record length {record_len} at offset {record_start}"
            )));
        }
        let record_end = record_start + 4 + u64::from(record_len);
        if record_end > file_len {
            return Ok(SegmentReplay {
                record_count,
                terminal_hash: previous_hash,
                valid_len,
                last_timestamp_utc,
                stopped: false,
            });
        }

        let mut rest = vec![0u8; record_len as usize];
        file.read_exact(&mut rest)
            .map_err(|err| StateError::io_at("read audit record", path, err))?;
        let payload_len = rest.len() - RECORD_TRAILER_LEN;
        let (payload, trailer) = rest.split_at(payload_len);
        let stored_hash: [u8; 32] = trailer[..RECORD_HASH_LEN]
            .try_into()
            .expect("fixed record hash length");
        let stored_crc = u64::from_le_bytes(
            trailer[RECORD_HASH_LEN..]
                .try_into()
                .expect("fixed record crc length"),
        );

        let mut crc_input = Vec::with_capacity(4 + payload.len() + RECORD_HASH_LEN);
        crc_input.extend_from_slice(&len_buf);
        crc_input.extend_from_slice(payload);
        crc_input.extend_from_slice(&stored_hash);
        let computed_crc = crc64_xz(&crc_input);
        if stored_crc != computed_crc {
            return Err(StateError::AuditCorrupt(format!(
                "audit record crc mismatch at offset {record_start}"
            )));
        }

        let computed_hash = record_hash(previous_hash, payload);
        if stored_hash != computed_hash {
            return Err(StateError::AuditCorrupt(format!(
                "audit record hash mismatch at offset {record_start}"
            )));
        }

        let record = decode_record_cbor(payload)?;
        if record.sequence != expected_sequence {
            return Err(StateError::AuditCorrupt(format!(
                "audit sequence gap: expected {expected_sequence}, got {}",
                record.sequence
            )));
        }
        expected_sequence += 1;
        previous_hash = stored_hash;
        valid_len = record_end;
        record_count += 1;
        last_timestamp_utc = Some(record.timestamp_utc.clone());
        if visitor(record).is_break() {
            return Ok(SegmentReplay {
                record_count,
                terminal_hash: previous_hash,
                valid_len,
                last_timestamp_utc,
                stopped: true,
            });
        }
    }
}

#[derive(Debug)]
struct SegmentHeader {
    segment_date: String,
    previous_segment_terminal_hash: [u8; 32],
}

fn write_segment_header(
    path: &Path,
    segment_date: &str,
    previous_hash: [u8; 32],
) -> Result<(), StateError> {
    let mut bytes = Vec::with_capacity(HEADER_LEN);
    bytes.extend_from_slice(AUDIT_MAGIC);
    bytes.extend_from_slice(&AUDIT_SCHEMA_VERSION.to_le_bytes());
    bytes.extend_from_slice(segment_date.as_bytes());
    bytes.extend_from_slice(&previous_hash);
    let crc = crc64_xz(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| StateError::io_at("create audit segment", path, err))?;
    file.write_all(&bytes)
        .map_err(|err| StateError::io_at("write audit segment header", path, err))?;
    file.sync_all()
        .map_err(|err| StateError::io_at("fsync audit segment header", path, err))?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn read_segment_header(file: &mut File, path: &Path) -> Result<SegmentHeader, StateError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|err| StateError::io_at("seek audit segment header", path, err))?;
    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)
        .map_err(|err| StateError::io_at("read audit segment header", path, err))?;
    if &header[..AUDIT_MAGIC.len()] != AUDIT_MAGIC {
        return Err(StateError::AuditCorrupt(format!(
            "bad audit magic in {}",
            path.display()
        )));
    }
    let version_start = AUDIT_MAGIC.len();
    let version = u16::from_le_bytes(
        header[version_start..version_start + 2]
            .try_into()
            .expect("fixed version length"),
    );
    if version != AUDIT_SCHEMA_VERSION {
        return Err(StateError::AuditCorrupt(format!(
            "unsupported audit schema {version}"
        )));
    }
    let stored_crc = u64::from_le_bytes(
        header[HEADER_WITHOUT_CRC_LEN..HEADER_LEN]
            .try_into()
            .expect("fixed header crc length"),
    );
    let computed_crc = crc64_xz(&header[..HEADER_WITHOUT_CRC_LEN]);
    if stored_crc != computed_crc {
        return Err(StateError::AuditCorrupt(format!(
            "audit segment header crc mismatch in {}",
            path.display()
        )));
    }
    let date_start = AUDIT_MAGIC.len() + 2;
    let segment_date = std::str::from_utf8(&header[date_start..date_start + SEGMENT_DATE_LEN])
        .map_err(|err| StateError::AuditCorrupt(format!("bad audit segment date utf8: {err}")))?
        .to_string();
    validate_segment_date(&segment_date)?;
    let hash_start = AUDIT_MAGIC.len() + 2 + SEGMENT_DATE_LEN;
    let previous_segment_terminal_hash = header[hash_start..hash_start + 32]
        .try_into()
        .expect("fixed previous hash length");
    Ok(SegmentHeader {
        segment_date,
        previous_segment_terminal_hash,
    })
}

fn canonical_record_cbor(record: &AuditRecord) -> Result<Vec<u8>, StateError> {
    let value = record_to_cbor(record);
    let mut out = Vec::new();
    ciborium::ser::into_writer(&value, &mut out)
        .map_err(|err| StateError::AuditWriteFailed(err.to_string()))?;
    Ok(out)
}

fn decode_record_cbor(bytes: &[u8]) -> Result<AuditRecord, StateError> {
    let value: CborValue = ciborium::de::from_reader(bytes)
        .map_err(|err| StateError::AuditCorrupt(format!("decode audit record cbor: {err}")))?;
    record_from_cbor(&value)
}

fn record_to_cbor(record: &AuditRecord) -> CborValue {
    let mut entries = vec![
        ("actor", actor_to_cbor(&record.actor)),
        ("detail", string_value_map(&record.detail)),
        ("event", CborValue::Text(record.event.as_str().to_string())),
        ("host_id", CborValue::Text(record.host_id.clone())),
        (
            "idempotency_key",
            opt_uuid_to_cbor(record.idempotency_key.as_ref()),
        ),
        (
            "operation_id",
            opt_uuid_to_cbor(record.operation_id.as_ref()),
        ),
        (
            "process_id",
            CborValue::Integer(u64::from(record.process_id).into()),
        ),
        (
            "record_uuid",
            CborValue::Text(record.record_uuid.to_string()),
        ),
        (
            "schema_version",
            CborValue::Integer(u64::from(record.schema_version).into()),
        ),
        ("sequence", CborValue::Integer(record.sequence.into())),
        ("session_id", opt_uuid_to_cbor(record.session_id.as_ref())),
        (
            "source_layer",
            CborValue::Text(record.source_layer.as_str().to_string()),
        ),
        ("subject", subject_to_cbor(&record.subject)),
        (
            "timestamp_utc",
            CborValue::Text(record.timestamp_utc.clone()),
        ),
    ];
    if let Some(software_build) = record.software_build.as_ref() {
        entries.push(("software_build", CborValue::Text(software_build.clone())));
    }
    sorted_map(entries)
}

fn record_from_cbor(value: &CborValue) -> Result<AuditRecord, StateError> {
    let fields = value_map(value, "audit record")?;
    let timestamp_utc = required_text(fields, "timestamp_utc")?;
    parse_timestamp_utc(&timestamp_utc)?;
    Ok(AuditRecord {
        schema_version: u16::try_from(required_u64(fields, "schema_version")?)
            .map_err(|_| StateError::AuditCorrupt("schema_version out of range".to_string()))?,
        record_uuid: required_uuid(fields, "record_uuid")?,
        sequence: required_u64(fields, "sequence")?,
        timestamp_utc,
        host_id: required_text(fields, "host_id")?,
        process_id: u32::try_from(required_u64(fields, "process_id")?)
            .map_err(|_| StateError::AuditCorrupt("process_id out of range".to_string()))?,
        software_build: optional_text_if_present(fields, "software_build")?,
        actor: actor_from_cbor(required_value(fields, "actor")?)?,
        source_layer: SourceLayer::parse(&required_text(fields, "source_layer")?)?,
        operation_id: optional_uuid(fields, "operation_id")?,
        session_id: optional_uuid(fields, "session_id")?,
        idempotency_key: optional_uuid(fields, "idempotency_key")?,
        event: AuditEvent::parse(&required_text(fields, "event")?)?,
        subject: subject_from_cbor(required_value(fields, "subject")?)?,
        detail: string_value_map_from_cbor(required_value(fields, "detail")?)?,
    })
}

fn actor_to_cbor(actor: &AuditActor) -> CborValue {
    match actor {
        AuditActor::System => sorted_map(vec![("kind", CborValue::Text("system".to_string()))]),
        AuditActor::User(id) => sorted_map(vec![
            ("id", CborValue::Text(id.clone())),
            ("kind", CborValue::Text("user".to_string())),
        ]),
        AuditActor::Service(id) => sorted_map(vec![
            ("id", CborValue::Text(id.clone())),
            ("kind", CborValue::Text("service".to_string())),
        ]),
    }
}

fn actor_from_cbor(value: &CborValue) -> Result<AuditActor, StateError> {
    let fields = value_map(value, "actor")?;
    match required_text(fields, "kind")?.as_str() {
        "system" => Ok(AuditActor::System),
        "user" => Ok(AuditActor::User(required_text(fields, "id")?)),
        "service" => Ok(AuditActor::Service(required_text(fields, "id")?)),
        kind => Err(StateError::AuditCorrupt(format!(
            "unknown audit actor kind {kind}"
        ))),
    }
}

fn subject_to_cbor(subject: &AuditSubject) -> CborValue {
    sorted_map(vec![
        ("id", optional_text_to_cbor(subject.id.as_ref())),
        ("kind", CborValue::Text(subject.kind.clone())),
    ])
}

fn subject_from_cbor(value: &CborValue) -> Result<AuditSubject, StateError> {
    let fields = value_map(value, "subject")?;
    Ok(AuditSubject {
        kind: required_text(fields, "kind")?,
        id: optional_text(fields, "id")?,
    })
}

fn sorted_map(entries: Vec<(&str, CborValue)>) -> CborValue {
    let mut entries: Vec<_> = entries
        .into_iter()
        .map(|(key, value)| (CborValue::Text(key.to_string()), value))
        .collect();
    entries
        .sort_by(|(left, _), (right, _)| text_key(left).as_bytes().cmp(text_key(right).as_bytes()));
    CborValue::Map(entries)
}

fn string_value_map(map: &BTreeMap<String, CborValue>) -> CborValue {
    let mut entries: Vec<_> = map
        .iter()
        .map(|(key, value)| (CborValue::Text(key.clone()), value.clone()))
        .collect();
    entries
        .sort_by(|(left, _), (right, _)| text_key(left).as_bytes().cmp(text_key(right).as_bytes()));
    CborValue::Map(entries)
}

fn string_value_map_from_cbor(
    value: &CborValue,
) -> Result<BTreeMap<String, CborValue>, StateError> {
    let mut out = BTreeMap::new();
    for (key, value) in value_map(value, "string map")? {
        let CborValue::Text(key) = key else {
            return Err(StateError::AuditCorrupt(
                "audit map contains non-text key".to_string(),
            ));
        };
        validate_detail_key(key)
            .map_err(|err| StateError::AuditCorrupt(format!("audit detail key: {err}")))?;
        out.insert(key.clone(), value.clone());
    }
    Ok(out)
}

fn value_map<'a>(
    value: &'a CborValue,
    context: &str,
) -> Result<&'a [(CborValue, CborValue)], StateError> {
    match value {
        CborValue::Map(entries) => Ok(entries),
        _ => Err(StateError::AuditCorrupt(format!(
            "{context} is not a cbor map"
        ))),
    }
}

fn required_value<'a>(
    fields: &'a [(CborValue, CborValue)],
    key: &str,
) -> Result<&'a CborValue, StateError> {
    fields
        .iter()
        .find_map(|(candidate, value)| {
            if matches!(candidate, CborValue::Text(text) if text == key) {
                Some(value)
            } else {
                None
            }
        })
        .ok_or_else(|| StateError::AuditCorrupt(format!("missing audit field {key}")))
}

fn required_text(fields: &[(CborValue, CborValue)], key: &str) -> Result<String, StateError> {
    match required_value(fields, key)? {
        CborValue::Text(text) => Ok(text.clone()),
        _ => Err(StateError::AuditCorrupt(format!(
            "audit field {key} is not text"
        ))),
    }
}

fn optional_text(
    fields: &[(CborValue, CborValue)],
    key: &str,
) -> Result<Option<String>, StateError> {
    match required_value(fields, key)? {
        CborValue::Null => Ok(None),
        CborValue::Text(text) => Ok(Some(text.clone())),
        _ => Err(StateError::AuditCorrupt(format!(
            "audit field {key} is not optional text"
        ))),
    }
}

fn optional_text_if_present(
    fields: &[(CborValue, CborValue)],
    key: &str,
) -> Result<Option<String>, StateError> {
    let Some(value) = fields.iter().find_map(|(candidate, value)| {
        matches!(candidate, CborValue::Text(text) if text == key).then_some(value)
    }) else {
        return Ok(None);
    };
    match value {
        CborValue::Null => Ok(None),
        CborValue::Text(text) if !text.trim().is_empty() => Ok(Some(text.clone())),
        CborValue::Text(_) => Err(StateError::AuditCorrupt(
            "audit field software_build is empty".to_string(),
        )),
        _ => Err(StateError::AuditCorrupt(format!(
            "audit field {key} is not optional text"
        ))),
    }
}

fn required_u64(fields: &[(CborValue, CborValue)], key: &str) -> Result<u64, StateError> {
    match required_value(fields, key)? {
        CborValue::Integer(integer) => {
            let value: i128 = (*integer).into();
            u64::try_from(value).map_err(|_| {
                StateError::AuditCorrupt(format!("audit field {key} out of u64 range"))
            })
        }
        _ => Err(StateError::AuditCorrupt(format!(
            "audit field {key} is not integer"
        ))),
    }
}

fn required_uuid(fields: &[(CborValue, CborValue)], key: &str) -> Result<Uuid, StateError> {
    Uuid::parse_str(&required_text(fields, key)?)
        .map_err(|err| StateError::AuditCorrupt(format!("audit field {key} bad uuid: {err}")))
}

fn optional_uuid(fields: &[(CborValue, CborValue)], key: &str) -> Result<Option<Uuid>, StateError> {
    match required_value(fields, key)? {
        CborValue::Null => Ok(None),
        CborValue::Text(text) => Uuid::parse_str(text)
            .map(Some)
            .map_err(|err| StateError::AuditCorrupt(format!("audit field {key} bad uuid: {err}"))),
        _ => Err(StateError::AuditCorrupt(format!(
            "audit field {key} is not optional uuid"
        ))),
    }
}

fn opt_uuid_to_cbor(uuid: Option<&Uuid>) -> CborValue {
    uuid.map(|uuid| CborValue::Text(uuid.to_string()))
        .unwrap_or(CborValue::Null)
}

fn optional_text_to_cbor(text: Option<&String>) -> CborValue {
    text.map(|text| CborValue::Text(text.clone()))
        .unwrap_or(CborValue::Null)
}

fn text_key(value: &CborValue) -> &str {
    match value {
        CborValue::Text(text) => text,
        _ => "",
    }
}

impl SourceLayer {
    fn as_str(&self) -> &'static str {
        match self {
            SourceLayer::Layer2 => "layer2",
            SourceLayer::Layer3b => "layer3b",
            SourceLayer::Layer3c => "layer3c",
            SourceLayer::Layer4 => "layer4",
            SourceLayer::Layer5 => "layer5",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "layer2" => Ok(Self::Layer2),
            "layer3b" => Ok(Self::Layer3b),
            "layer3c" => Ok(Self::Layer3c),
            "layer4" => Ok(Self::Layer4),
            "layer5" => Ok(Self::Layer5),
            _ => Err(StateError::AuditCorrupt(format!(
                "unknown source layer {value}"
            ))),
        }
    }
}

impl AuditEvent {
    fn as_str(&self) -> &'static str {
        match self {
            AuditEvent::RequestReceived => "RequestReceived",
            AuditEvent::OperationStarted => "OperationStarted",
            AuditEvent::OperationProgress => "OperationProgress",
            AuditEvent::OperationFinished => "OperationFinished",
            AuditEvent::OperationFailed => "OperationFailed",
            AuditEvent::CancelRequested => "CancelRequested",
            AuditEvent::CancelledBeforeDispatch => "CancelledBeforeDispatch",
            AuditEvent::CompletedAfterCancel => "CompletedAfterCancel",
            AuditEvent::CancellationRejected => "CancellationRejected",
            AuditEvent::CompletionUnknown => "CompletionUnknown",
            AuditEvent::SessionOpened => "SessionOpened",
            AuditEvent::SessionCheckpointed => "SessionCheckpointed",
            AuditEvent::SessionClosed => "SessionClosed",
            AuditEvent::SessionOrphaned => "SessionOrphaned",
            AuditEvent::SessionLostByRestart => "SessionLostByRestart",
            AuditEvent::ClockRegressionObserved => "ClockRegressionObserved",
            AuditEvent::ClockForwardJumpObserved => "ClockForwardJumpObserved",
            AuditEvent::HardwareWarning => "HardwareWarning",
            AuditEvent::RecoveryEvent => "RecoveryEvent",
            AuditEvent::ConfigLoaded => "ConfigLoaded",
            AuditEvent::ConfigRejected => "ConfigRejected",
            AuditEvent::IndexRebuilt => "IndexRebuilt",
            AuditEvent::ReadOnlyModeEntered => "ReadOnlyModeEntered",
            AuditEvent::ReadOnlyModeLeft => "ReadOnlyModeLeft",
            AuditEvent::AuditWriteFailed => "AuditWriteFailed",
            AuditEvent::TapeRetired => "TapeRetired",
            AuditEvent::TapeProvisioned => "TapeProvisioned",
            AuditEvent::TapePoolAssigned => "TapePoolAssigned",
            AuditEvent::TapeSealed => "TapeSealed",
            AuditEvent::DriveRetired => "DriveRetired",
            AuditEvent::DriveAnnotated => "DriveAnnotated",
            AuditEvent::DriveCleaned => "DriveCleaned",
            AuditEvent::CleaningCartridgeExpired => "CleaningCartridgeExpired",
            AuditEvent::CleaningCartridgeRegistered => "CleaningCartridgeRegistered",
            AuditEvent::DriveFenced => "DriveFenced",
            AuditEvent::DriveUnfenced => "DriveUnfenced",
            AuditEvent::AlarmAcked => "AlarmAcked",
            AuditEvent::AlarmRaised => "AlarmRaised",
            AuditEvent::AlarmCleared => "AlarmCleared",
            AuditEvent::TapeIoFenceRaised => "TapeIoFenceRaised",
            AuditEvent::TapeIoFenceReleased => "TapeIoFenceReleased",
            AuditEvent::DriveHealthObserved => "DriveHealthObserved",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "RequestReceived" => Ok(Self::RequestReceived),
            "OperationStarted" => Ok(Self::OperationStarted),
            "OperationProgress" => Ok(Self::OperationProgress),
            "OperationFinished" => Ok(Self::OperationFinished),
            "OperationFailed" => Ok(Self::OperationFailed),
            "CancelRequested" => Ok(Self::CancelRequested),
            "CancelledBeforeDispatch" => Ok(Self::CancelledBeforeDispatch),
            "CompletedAfterCancel" => Ok(Self::CompletedAfterCancel),
            "CancellationRejected" => Ok(Self::CancellationRejected),
            "CompletionUnknown" => Ok(Self::CompletionUnknown),
            "SessionOpened" => Ok(Self::SessionOpened),
            "SessionCheckpointed" => Ok(Self::SessionCheckpointed),
            "SessionClosed" => Ok(Self::SessionClosed),
            "SessionOrphaned" => Ok(Self::SessionOrphaned),
            "SessionLostByRestart" => Ok(Self::SessionLostByRestart),
            "ClockRegressionObserved" => Ok(Self::ClockRegressionObserved),
            "ClockForwardJumpObserved" => Ok(Self::ClockForwardJumpObserved),
            "HardwareWarning" => Ok(Self::HardwareWarning),
            "RecoveryEvent" => Ok(Self::RecoveryEvent),
            "ConfigLoaded" => Ok(Self::ConfigLoaded),
            "ConfigRejected" => Ok(Self::ConfigRejected),
            "IndexRebuilt" => Ok(Self::IndexRebuilt),
            "ReadOnlyModeEntered" => Ok(Self::ReadOnlyModeEntered),
            "ReadOnlyModeLeft" => Ok(Self::ReadOnlyModeLeft),
            "AuditWriteFailed" => Ok(Self::AuditWriteFailed),
            "TapeRetired" => Ok(Self::TapeRetired),
            "TapeProvisioned" => Ok(Self::TapeProvisioned),
            "TapePoolAssigned" => Ok(Self::TapePoolAssigned),
            "TapeSealed" => Ok(Self::TapeSealed),
            "DriveRetired" => Ok(Self::DriveRetired),
            "DriveAnnotated" => Ok(Self::DriveAnnotated),
            "DriveCleaned" => Ok(Self::DriveCleaned),
            "CleaningCartridgeExpired" => Ok(Self::CleaningCartridgeExpired),
            "CleaningCartridgeRegistered" => Ok(Self::CleaningCartridgeRegistered),
            "DriveFenced" => Ok(Self::DriveFenced),
            "DriveUnfenced" => Ok(Self::DriveUnfenced),
            "AlarmAcked" => Ok(Self::AlarmAcked),
            "AlarmRaised" => Ok(Self::AlarmRaised),
            "AlarmCleared" => Ok(Self::AlarmCleared),
            "TapeIoFenceRaised" => Ok(Self::TapeIoFenceRaised),
            "TapeIoFenceReleased" => Ok(Self::TapeIoFenceReleased),
            "DriveHealthObserved" => Ok(Self::DriveHealthObserved),
            _ => Err(StateError::AuditCorrupt(format!(
                "unknown audit event {value}"
            ))),
        }
    }
}

fn audit_segment_paths(dir: &Path) -> Result<Vec<PathBuf>, StateError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|err| StateError::io_at("read audit directory", dir, err))?
    {
        let entry =
            entry.map_err(|err| StateError::io_at("read audit directory entry", dir, err))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("remaudit") {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn segment_path(dir: &Path, segment_date: &str) -> PathBuf {
    dir.join(format!("{segment_date}.remaudit"))
}

fn segment_date_from_path(path: &Path) -> Result<String, StateError> {
    let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
        StateError::AuditCorrupt(format!("bad audit filename {}", path.display()))
    })?;
    validate_segment_date(stem)?;
    Ok(stem.to_string())
}

fn validate_segment_date(segment_date: &str) -> Result<(), StateError> {
    let bytes = segment_date.as_bytes();
    let valid = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(idx, byte)| matches!(idx, 4 | 7) || byte.is_ascii_digit());
    if !valid {
        return Err(StateError::ConfigInvalid(format!(
            "invalid audit segment date {segment_date}"
        )));
    }
    let year = segment_date[0..4].parse::<i32>().map_err(|_| {
        StateError::ConfigInvalid(format!("invalid audit segment date {segment_date}"))
    })?;
    let month = segment_date[5..7].parse::<u8>().map_err(|_| {
        StateError::ConfigInvalid(format!("invalid audit segment date {segment_date}"))
    })?;
    let day = segment_date[8..10].parse::<u8>().map_err(|_| {
        StateError::ConfigInvalid(format!("invalid audit segment date {segment_date}"))
    })?;
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(StateError::ConfigInvalid(format!(
            "invalid audit segment date {segment_date}"
        )));
    }
    Ok(())
}

fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn validate_detail_keys(detail: &BTreeMap<String, CborValue>) -> Result<(), StateError> {
    for key in detail.keys() {
        validate_detail_key(key).map_err(StateError::AuditWriteFailed)?;
    }
    Ok(())
}

fn validate_detail_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("empty key".to_string());
    }
    if !key.is_ascii() || key.bytes().any(|byte| byte < 0x20 || byte == 0x7F) {
        return Err(format!(
            "key {key:?} must be printable ASCII for canonical audit encoding"
        ));
    }
    Ok(())
}

fn utc_segment_date() -> String {
    let date = OffsetDateTime::now_utc().date();
    segment_date_for_timestamp(date.with_time(time::Time::MIDNIGHT).assume_utc())
}

fn segment_date_for_timestamp(timestamp: OffsetDateTime) -> String {
    let date = timestamp.to_offset(time::UtcOffset::UTC).date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

fn parse_timestamp_utc(timestamp: &str) -> Result<OffsetDateTime, StateError> {
    OffsetDateTime::parse(timestamp, &Rfc3339).map_err(|err| {
        StateError::AuditCorrupt(format!("audit timestamp is not RFC3339 UTC: {err}"))
    })
}

fn format_timestamp_utc(timestamp: OffsetDateTime) -> Result<String, StateError> {
    timestamp
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|err| StateError::AuditWriteFailed(err.to_string()))
}

fn duration_from_std(duration: StdDuration) -> Duration {
    let seconds = i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
    Duration::seconds(seconds) + Duration::nanoseconds(i64::from(duration.subsec_nanos()))
}

fn clock_warning_event(event: AuditEvent, detail: BTreeMap<String, CborValue>) -> AuditEventRecord {
    AuditEventRecord {
        actor: AuditActor::System,
        source_layer: SourceLayer::Layer4,
        operation_id: None,
        session_id: None,
        idempotency_key: None,
        event,
        subject: AuditSubject {
            kind: "clock".to_string(),
            id: None,
        },
        detail,
    }
}

fn record_hash(previous_hash: [u8; 32], payload: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(previous_hash);
    hasher.update(payload);
    hasher.finalize().into()
}

fn host_id() -> String {
    fs::read_to_string("/etc/machine-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            std::env::var("HOSTNAME")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build identifier stamped into every newly appended audit record.
pub fn software_build() -> &'static str {
    env!("REMANENCE_SOFTWARE_BUILD")
}

fn sync_directory(path: &Path) -> Result<(), StateError> {
    let dir = File::open(path).map_err(|err| StateError::io_at("open audit dir", path, err))?;
    dir.sync_all()
        .map_err(|err| StateError::io_at("fsync audit dir", path, err))
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::time::Duration as StdDuration;

    use super::*;

    fn event(subject_id: &str) -> AuditEventRecord {
        AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::OperationStarted,
            subject: AuditSubject {
                kind: "test".to_string(),
                id: Some(subject_id.to_string()),
            },
            detail: BTreeMap::new(),
        }
    }

    fn ts(value: &str) -> OffsetDateTime {
        OffsetDateTime::parse(value, &Rfc3339).expect("test timestamp")
    }

    #[test]
    fn crc64_xz_matches_normative_check_value() {
        assert_eq!(crc64_xz(b"123456789"), 0x995D_C9BB_DF19_39FA);
    }

    #[test]
    fn validate_segment_date_checks_calendar_ranges() {
        validate_segment_date("2024-02-29").expect("leap day");
        for value in ["2026-00-10", "2026-99-10", "2026-02-30", "2025-02-29"] {
            let err = validate_segment_date(value).expect_err("invalid calendar date");
            assert!(err.to_string().contains("invalid audit segment date"));
        }
    }

    #[test]
    fn audit_append_replay_preserves_sequence_order() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");

        let first = log.append(event("first")).expect("first append");
        let second = log.append(event("second")).expect("second append");
        drop(log);

        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[1].sequence, 2);
        assert_eq!(records[1].subject.id.as_deref(), Some("second"));
        assert_eq!(records[0].software_build.as_deref(), Some(software_build()));
        assert!(!software_build().trim().is_empty());
        assert!(software_build().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn audit_decoder_tolerates_legacy_record_without_software_build() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit-legacy-build")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        let (_, mut current) = log
            .append_and_return_record(event("legacy"))
            .expect("append current record");
        current.software_build = None;

        let encoded = canonical_record_cbor(&current).expect("encode legacy-shaped record");
        let decoded = decode_record_cbor(&encoded).expect("decode legacy-shaped record");

        assert_eq!(decoded.software_build, None);
        let value: CborValue = ciborium::de::from_reader(encoded.as_slice()).expect("decode map");
        assert!(value_map(&value, "legacy record")
            .expect("map")
            .iter()
            .all(|(key, _)| key != &CborValue::Text("software_build".to_string())));
    }

    #[test]
    fn incremental_replay_visits_records_without_collecting_the_history() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit-incremental")
            .tempdir()
            .expect("temp dir");
        let mut log =
            FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open audit");
        for sequence in 1..=40 {
            log.append(event(sequence.to_string().as_str()))
                .expect("append audit record");
        }
        drop(log);

        let mut visited = Vec::new();
        FileAuditLog::replay_incremental(temp.path(), |record| {
            visited.push(record.sequence);
            if visited.len() == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .expect("incremental replay");

        assert_eq!(visited, vec![1, 2, 3]);
        assert_eq!(
            FileAuditLog::replay(temp.path())
                .expect("collecting replay remains available")
                .len(),
            40
        );
    }

    #[test]
    fn audit_replay_drops_torn_trailing_record() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        log.append(event("kept")).expect("append");
        let path = log.segment_path();
        drop(log);

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open append");
        file.write_all(&1234u32.to_le_bytes())
            .expect("write torn len");
        file.write_all(b"partial").expect("write torn body");
        file.sync_all().expect("sync torn body");

        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].subject.id.as_deref(), Some("kept"));

        let _reopened =
            FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("reopen truncates");
        assert_eq!(
            FileAuditLog::replay(temp.path())
                .expect("replay after truncate")
                .len(),
            1
        );
    }

    #[test]
    fn audit_replay_rejects_mid_log_corruption() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        log.append(event("first")).expect("append");
        let path = log.segment_path();
        drop(log);

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("open rw");
        file.seek(SeekFrom::Start((HEADER_LEN + 4 + 8) as u64))
            .expect("seek payload");
        file.write_all(&[0xFF]).expect("corrupt payload");
        file.sync_all().expect("sync corruption");

        let err = FileAuditLog::replay(temp.path()).expect_err("corruption must fail");
        assert!(err.to_string().contains("crc mismatch"), "{err}");
    }

    #[test]
    fn audit_rotation_chains_next_segment_to_previous_terminal_hash() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut first =
            FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open first");
        let first_receipt = first
            .append_at_for_tests(event("first"), ts("2026-05-27T12:00:00Z"))
            .expect("append first");
        drop(first);

        let mut second =
            FileAuditLog::open_for_date(temp.path(), "2026-05-28", true).expect("open second");
        let second_receipt = second
            .append_at_for_tests(event("second"), ts("2026-05-28T12:00:00Z"))
            .expect("append second");
        drop(second);

        assert_ne!(first_receipt.record_hash, second_receipt.record_hash);
        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[1].sequence, 2);

        let reopened =
            FileAuditLog::open_for_date(temp.path(), "2026-05-28", true).expect("reopen second");
        assert_eq!(reopened.next_sequence, 3);

        let mut second_file =
            File::open(temp.path().join("2026-05-28.remaudit")).expect("second segment");
        let header =
            read_segment_header(&mut second_file, &temp.path().join("2026-05-28.remaudit"))
                .expect("header");
        assert_eq!(
            header.previous_segment_terminal_hash,
            first_receipt.record_hash
        );
    }

    #[test]
    fn append_rotates_to_new_utc_day() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        log.append_at_for_tests(event("late"), ts("2026-05-27T23:59:59Z"))
            .expect("late append");
        log.append_at_for_tests(event("next-day"), ts("2026-05-28T00:00:00Z"))
            .expect("next-day append");
        drop(log);

        assert!(temp.path().join("2026-05-27.remaudit").exists());
        assert!(temp.path().join("2026-05-28.remaudit").exists());
        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].sequence, 1);
        assert_eq!(records[1].sequence, 2);
        assert_eq!(records[1].subject.id.as_deref(), Some("next-day"));
    }

    #[test]
    fn clock_regression_emits_warning_record() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        log.append_at_for_tests(event("first"), ts("2026-05-27T10:00:00Z"))
            .expect("first append");
        log.append_at_for_tests(event("regressed"), ts("2026-05-27T09:59:59Z"))
            .expect("regressed append");
        drop(log);

        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].event, AuditEvent::ClockRegressionObserved);
        assert_eq!(records[1].subject.kind, "clock");
        assert_eq!(records[2].subject.id.as_deref(), Some("regressed"));
        assert_eq!(records[2].sequence, 3);
    }

    #[test]
    fn clock_regression_does_not_rotate_back_to_prior_segment() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-28", true).expect("open");
        log.append_at_for_tests(event("first"), ts("2026-05-28T00:00:01Z"))
            .expect("first append");
        log.append_at_for_tests(event("regressed"), ts("2026-05-27T23:59:59Z"))
            .expect("regressed append");
        drop(log);

        assert!(!temp.path().join("2026-05-27.remaudit").exists());
        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].event, AuditEvent::ClockRegressionObserved);
        assert_eq!(records[2].subject.id.as_deref(), Some("regressed"));
    }

    #[test]
    fn reopen_after_forward_dated_segment_does_not_create_backdated_segment() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut future =
            FileAuditLog::open_for_date(temp.path(), "2026-05-29", true).expect("open future");
        future
            .append_at_for_tests(event("future"), ts("2026-05-29T00:00:01Z"))
            .expect("future append");
        drop(future);

        let mut reopened =
            FileAuditLog::open_for_date(temp.path(), "2026-05-28", true).expect("reopen corrected");
        assert_eq!(
            reopened.segment_path(),
            temp.path().join("2026-05-29.remaudit")
        );
        reopened
            .append_at_for_tests(event("corrected"), ts("2026-05-28T23:59:59Z"))
            .expect("corrected append");
        drop(reopened);

        assert!(!temp.path().join("2026-05-28.remaudit").exists());
        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].event, AuditEvent::ClockRegressionObserved);
        assert_eq!(records[2].subject.id.as_deref(), Some("corrected"));
    }

    #[test]
    fn forward_clock_jump_emits_warning_record() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        let start = Instant::now();
        log.append_with_observed_clock_for_tests(event("first"), ts("2026-05-27T10:00:00Z"), start)
            .expect("first append");
        log.append_with_observed_clock_for_tests(
            event("jumped"),
            ts("2026-05-27T10:10:00Z"),
            start + StdDuration::from_secs(60),
        )
        .expect("jumped append");
        drop(log);

        let records = FileAuditLog::replay(temp.path()).expect("replay");
        assert_eq!(records.len(), 3);
        assert_eq!(records[1].event, AuditEvent::ClockForwardJumpObserved);
        assert_eq!(records[2].subject.id.as_deref(), Some("jumped"));
        assert_eq!(records[2].sequence, 3);
    }

    #[test]
    fn audit_detail_keys_must_be_printable_ascii() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-audit")
            .tempdir()
            .expect("temp dir");
        let mut log = FileAuditLog::open_for_date(temp.path(), "2026-05-27", true).expect("open");
        let mut bad = event("bad-detail");
        bad.detail
            .insert("résumé".to_string(), CborValue::Text("bad".to_string()));

        let err = log
            .append_at_for_tests(bad, ts("2026-05-27T10:00:00Z"))
            .expect_err("non-ascii detail key must fail");
        assert!(err.to_string().contains("printable ASCII"), "{err}");
    }
}
