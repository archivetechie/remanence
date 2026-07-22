//! Drive/changer actor pool for Layer 5 read and write sessions.
//!
//! Phase 3b reserves individual drive bays for sessions while keeping
//! reconcile and robotics pool-exclusive.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, RwLock};
use std::time::{Duration as StdDuration, Instant};

use ciborium::value::Value as CborValue;
use remanence_format::error::FormatError;
use remanence_format::model::BodyLba;
use remanence_library::{
    classify_media_readiness_error_ref, BlockSize, BlockSource, ChangerHandle, DiscoveryReport,
    DriveHandle, DriveHandleSink, DriveHandleSource, DriveOpError, LoadError, MediaFamily,
    MediaReadiness, MediaReadinessPoll, MediaReadinessWaitEvent, MediaReadinessWaitOptions,
    ReadBatchOutcome, SpaceKind, SpaceResult, StaticAllowlist, TapeConfig, TapeIoError,
    TapePosition,
};
use remanence_parity::{
    committed_prefix_from_journal, plan_resume_append_from_committed_prefix,
    scan_reconstruct_filemark_map, CloseReason, DriveHandleRawSink, DriveHandleRawSource,
    FileTapeFileJournal, FilemarkMap, ParityError, ParitySink, ParitySinkSessionState,
    ResumeWriterSeed, TapeFileEntry, TapeFileJournal, TapeFileKind,
};
use remanence_state::{
    AlarmRecord, AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex,
    CleaningConfig, DriveHealthSnapshotInput, DriveHealthSnapshotRecord, FileAuditLog,
    NativeObjectFileRecord, SourceLayer, TapeIoConfig, TapeIoFenceRecord, TapePoolConfig,
};
use remanence_stream::StreamingError;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use crate::pool_write::{SelectedTape, WriteObjectToPoolRequest};
use crate::{
    load_tape_by_uuid, pb, status_from_state_error, timestamp_from_rfc3339, verify_tape_identity,
    PoolWriteError, SelectTapeError, TapeUuid,
};

pub(crate) const SPOOL_MAX_BYTES: u64 = crate::APPEND_SPOOL_MAX_BYTES;
const LOAD_READY_TIMEOUT: StdDuration = StdDuration::from_secs(9_000);
const LOAD_READY_POLL_INTERVAL: StdDuration = StdDuration::from_secs(30);

/// Session-independent coordinates used to position a newly minted read
/// session at a catalogued file boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReadResumeTarget {
    pub(crate) tape_uuid: [u8; 16],
    pub(crate) object_id: String,
    pub(crate) file_id: String,
    pub(crate) file_boundary_byte_offset: u64,
    pub(crate) expected_position_lba: Option<u64>,
    pub(crate) prior_daemon_epoch: Option<u64>,
}

/// Robotics work to perform after the owner opens and refreshes the library.
pub(crate) enum RoboticsAction {
    Refresh,
    Move {
        src: u16,
        dst: u16,
    },
    Load {
        slot: u16,
        bay: u16,
        wait_ready: bool,
    },
    Unload {
        bay: u16,
        destination: Option<u16>,
    },
    Clean {
        drive_uuid: Vec<u8>,
        trigger: String,
    },
}

pub(crate) enum ChangerCommand {
    Move {
        src: u16,
        dst: u16,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    #[expect(dead_code, reason = "Phase 3a command shape includes explicit refresh")]
    Refresh {
        reply: oneshot::Sender<Result<(), Status>>,
    },
    Reconcile {
        tape_uuid: [u8; 16],
        handle: crate::operations::OperationHandle,
    },
    Robotics {
        library_serial: String,
        action: RoboticsAction,
        handle: crate::operations::OperationHandle,
    },
}

pub(crate) enum DriveCommand {
    WaitReady {
        operation_id: Uuid,
        family: MediaFamily,
        options: MediaReadinessWaitOptions,
        handle: crate::operations::OperationHandle,
        reservation: DriveReservation,
    },
    OpenWrite {
        pool_cfg: TapePoolConfig,
        selected: SelectedTape,
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    OpenRead {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        barcode: Option<String>,
        source_slot: Option<u16>,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        resume_target: Option<ReadResumeTarget>,
        daemon_epoch: u64,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    Unload {
        reply: oneshot::Sender<Result<StdDuration, Status>>,
    },
    PollHealth {
        drive_uuid: Vec<u8>,
        trigger: &'static str,
        session_id: Option<Uuid>,
        tape_uuid: Option<[u8; 16]>,
        reply: oneshot::Sender<Result<DriveHealthSnapshotRecord, Status>>,
    },
    Heartbeat {
        drive_uuid: Vec<u8>,
        reply: oneshot::Sender<Result<(), Status>>,
    },
    AppendFinish {
        session_id: Uuid,
        source: crate::WriteObjectSource,
        archive_path: PathBuf,
        caller_object_id: String,
        expected_content_sha256: Option<[u8; 32]>,
        live_write_counter: Option<Arc<crate::DriveByteCounters>>,
        reply: oneshot::Sender<Result<AppendFinishOutcome, Status>>,
    },
    Checkpoint {
        session_id: Uuid,
        trigger: CheckpointTrigger,
        expected_batch_id: Option<Uuid>,
        reply: Option<oneshot::Sender<Result<CheckpointActorReply, Status>>>,
    },
    TimerIdleClose {
        session_id: Uuid,
        checkpoint_batch_id: Uuid,
    },
    Close {
        session_id: Uuid,
        reply: oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    },
    Abort {
        session_id: Uuid,
        reply: oneshot::Sender<Result<CloseWriteActorReply, Status>>,
    },
    Get {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    ReadFile {
        session_id: Uuid,
        object_id: String,
        file_id: Vec<u8>,
        stream_chunk_bytes: u32,
        chunk_tx: crate::read_core::ReadStreamSender,
    },
    ReadObjectRange {
        session_id: Uuid,
        object_id: String,
        file_id: String,
        start_byte: u64,
        end_byte: u64,
        stream_chunk_bytes: u32,
        chunk_tx: crate::read_core::ReadStreamSender,
    },
    CloseRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    GetRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
}

#[derive(Debug)]
pub(crate) struct AppendFinishOutcome {
    pub(crate) record: pb::ObjectRecord,
    pub(crate) replay: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckpointTrigger {
    Explicit,
    Timer,
    Shutdown,
}

#[derive(Debug)]
pub(crate) struct CheckpointActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) committed_objects: Vec<pb::ObjectRecord>,
}

fn send_checkpoint_actor_reply(
    reply: oneshot::Sender<Result<CheckpointActorReply, Status>>,
    session: pb::WriteSession,
    committed_receipts: &mut Vec<pb::ObjectRecord>,
) {
    let committed_objects = std::mem::take(committed_receipts);
    if let Err(Ok(unsent)) = reply.send(Ok(CheckpointActorReply {
        session,
        committed_objects,
    })) {
        *committed_receipts = unsent.committed_objects;
    }
}

/// Retain a catalog-replayed object until an explicit checkpoint can claim its durable copy.
///
/// A replay has no pending checkpoint batch because its copy was committed by an earlier
/// session. Append still reports that durable record to the current caller, whose batch contract
/// releases it only from `CheckpointSession`; coalescing by object id also prevents duplicate
/// receipts when the same replay arrives before that checkpoint.
fn retain_replayed_committed_receipt(
    committed_receipts: &mut Vec<pb::ObjectRecord>,
    record: &pb::ObjectRecord,
) {
    if !committed_receipts
        .iter()
        .any(|committed| committed.object_id == record.object_id)
    {
        committed_receipts.push(record.clone());
    }
}

#[derive(Debug)]
pub(crate) struct CloseWriteActorReply {
    pub(crate) session: pb::WriteSession,
    pub(crate) diagnostics: CloseWriteActorDiagnostics,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CloseWriteActorDiagnostics {
    /// Synchronous object-closing filemark time accumulated by append calls.
    pub(crate) filemark_write_drain: StdDuration,
    /// Catalog/journal commit time accumulated after those filemarks.
    pub(crate) catalog_journal_fsync: StdDuration,
    /// Close-time health snapshot and projection work.
    pub(crate) drive_snapshot: StdDuration,
    /// Always zero for lazy session close; later dismount diagnostics own rewind time.
    pub(crate) rewind: StdDuration,
    /// Always zero for lazy session close; later dismount diagnostics own SSC UNLOAD time.
    pub(crate) ssc_unload: StdDuration,
    /// SessionClosed audit append/fsync and SQLite projection time.
    pub(crate) session_audit_projection: StdDuration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MountedSession {
    pub bay: u16,
    pub library_serial: String,
    pub barcode: Option<String>,
    pub home_slot: Option<u16>,
    pub tape_uuid: TapeUuid,
    pub drive_uuid: Option<Vec<u8>>,
}

/// A library cartridge intentionally left seated after its session closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeatedCartridge {
    pub(crate) bay: u16,
    pub(crate) library_serial: String,
    pub(crate) barcode: Option<String>,
    pub(crate) home_slot: u16,
    pub(crate) tape_uuid: Option<TapeUuid>,
    pub(crate) prior_session_id: Option<Uuid>,
}

/// Generation-tagged idle record used to invalidate stale timeout tasks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParkedCartridge {
    pub(crate) seated: SeatedCartridge,
    generation: u64,
}

#[derive(Default)]
struct ParkedState {
    next_generation: u64,
    by_bay: HashMap<u16, ParkedCartridge>,
}

/// Shared actor/pool lifecycle maps used by timer-driven close-and-park.
#[derive(Clone, Default)]
pub(crate) struct DrivePoolLifecycle {
    sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    parked: Arc<Mutex<ParkedState>>,
    timer_park_tx: Option<mpsc::UnboundedSender<ParkedCartridge>>,
}

impl DrivePoolLifecycle {
    pub(crate) fn with_timer_park_sender(
        timer_park_tx: mpsc::UnboundedSender<ParkedCartridge>,
    ) -> Self {
        Self {
            timer_park_tx: Some(timer_park_tx),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct DrivePool {
    changer_tx: mpsc::Sender<ChangerCommand>,
    drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>,
    reservations: Arc<HashMap<u16, AtomicBool>>,
    sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    tape_reservations: Arc<Mutex<HashSet<TapeUuid>>>,
    parked: Arc<Mutex<ParkedState>>,
    shutting_down: Arc<AtomicBool>,
}

impl DrivePool {
    #[cfg(test)]
    pub(crate) fn new(
        changer_tx: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
    ) -> Self {
        Self::new_with_lifecycle(
            changer_tx,
            drives,
            reservations,
            DrivePoolLifecycle::default(),
        )
    }

    pub(crate) fn new_with_lifecycle(
        changer_tx: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
        lifecycle: DrivePoolLifecycle,
    ) -> Self {
        Self {
            changer_tx,
            drives: Arc::new(drives),
            reservations,
            sessions: lifecycle.sessions,
            tape_reservations: Arc::new(Mutex::new(HashSet::new())),
            parked: lifecycle.parked,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn changer_tx(&self) -> mpsc::Sender<ChangerCommand> {
        self.changer_tx.clone()
    }

    pub(crate) fn drive_tx(&self, bay: u16) -> Result<mpsc::Sender<DriveCommand>, Status> {
        self.drives
            .get(&bay)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("drive bay 0x{bay:04x} not available")))
    }

    #[cfg(test)]
    pub(crate) fn reserve_free_drive(&self) -> Result<u16, Status> {
        let mut bays = self.reservations.keys().copied().collect::<Vec<_>>();
        bays.sort_unstable();
        bays.into_iter()
            .find(|bay| {
                self.reservations.get(bay).is_some_and(|reservation| {
                    reservation
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                })
            })
            .ok_or_else(|| Status::failed_precondition("all drives are busy"))
    }

    pub(crate) fn reserve_drive(&self, bay: u16) -> Result<DriveReservation, Status> {
        if self.is_shutting_down() {
            return Err(Status::unavailable("drive pool is shutting down"));
        }
        self.reserve_drive_inner(bay)
    }

    pub(crate) fn reserve_drive_for_shutdown(&self, bay: u16) -> Result<DriveReservation, Status> {
        self.reserve_drive_inner(bay)
    }

    fn reserve_drive_inner(&self, bay: u16) -> Result<DriveReservation, Status> {
        let reservation = self
            .reservations
            .get(&bay)
            .ok_or_else(|| Status::not_found(format!("drive bay 0x{bay:04x} not available")))?;
        reservation
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| Status::failed_precondition(format!("drive bay 0x{bay:04x} is busy")))?;
        Ok(DriveReservation {
            bay,
            reservations: self.reservations.clone(),
            armed: true,
        })
    }

    pub(crate) fn release(&self, bay: u16) {
        if let Some(reservation) = self.reservations.get(&bay) {
            reservation.store(false, Ordering::SeqCst);
        }
    }

    pub(crate) fn reserve_all_exclusive(&self) -> Result<(), Status> {
        if self.is_shutting_down() {
            return Err(Status::unavailable("drive pool is shutting down"));
        }
        let mut acquired = Vec::new();
        let mut bays = self.reservations.keys().copied().collect::<Vec<_>>();
        bays.sort_unstable();
        for bay in bays {
            let Some(reservation) = self.reservations.get(&bay) else {
                continue;
            };
            if reservation
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                acquired.push(bay);
            } else {
                for acquired_bay in acquired {
                    self.release(acquired_bay);
                }
                return Err(Status::failed_precondition("drives are busy"));
            }
        }
        Ok(())
    }

    pub(crate) fn release_all(&self) {
        release_all_reservations(&self.reservations);
    }

    pub(crate) fn busy_bays(&self) -> HashSet<u16> {
        self.reservations
            .iter()
            .filter(|(_, reservation)| reservation.load(Ordering::SeqCst))
            .map(|(bay, _)| *bay)
            .collect()
    }

    /// Snapshot sessions by their enforcement key for advisory status only.
    ///
    /// The reservation atomics remain the sole authority for admission. This
    /// projection may race with an open/close and must never gate tape I/O.
    pub(crate) fn sessions_by_bay(&self) -> HashMap<u16, (Uuid, MountedSession)> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .map(|(session_id, mounted)| (mounted.bay, (*session_id, mounted.clone())))
            .collect()
    }

    pub(crate) fn mounted_tape_uuids(&self) -> HashSet<TapeUuid> {
        let mut in_use = self
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .map(|mounted| mounted.tape_uuid)
            .collect::<HashSet<_>>();
        in_use.extend(
            self.tape_reservations
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .iter()
                .copied(),
        );
        in_use
    }

    pub(crate) fn reserve_tape(&self, tape_uuid: TapeUuid) -> Result<TapeReservation, Status> {
        if self
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .any(|mounted| mounted.tape_uuid == tape_uuid)
        {
            return Err(Status::failed_precondition("tape is already mounted"));
        }
        let mut reservations = self
            .tape_reservations
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !reservations.insert(tape_uuid) {
            return Err(Status::failed_precondition("tape is already mounted"));
        }
        Ok(TapeReservation {
            tape_uuid,
            reservations: self.tape_reservations.clone(),
        })
    }

    pub(crate) fn record_session(&self, session_id: Uuid, mounted: MountedSession) {
        self.parked
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .by_bay
            .remove(&mounted.bay);
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(session_id, mounted);
    }

    pub(crate) fn session(&self, session_id: Uuid) -> Result<MountedSession, Status> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("session not found"))
    }

    pub(crate) fn forget_session(&self, session_id: Uuid) {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&session_id);
    }

    /// Convert a completed session reservation into an idle seated cartridge.
    /// The bay remains reserved until all bookkeeping is published.
    pub(crate) fn finish_session(
        &self,
        session_id: Uuid,
        mounted: MountedSession,
    ) -> Option<ParkedCartridge> {
        let bay = mounted.bay;
        self.forget_session(session_id);
        let parked = mounted.home_slot.map(|home_slot| {
            self.park_cartridge(SeatedCartridge {
                bay: mounted.bay,
                library_serial: mounted.library_serial,
                barcode: mounted.barcode,
                home_slot,
                tape_uuid: Some(mounted.tape_uuid),
                prior_session_id: Some(session_id),
            })
        });
        self.release(bay);
        parked
    }

    /// Register a cartridge found seated at startup or during reconciliation.
    pub(crate) fn park_cartridge(&self, seated: SeatedCartridge) -> ParkedCartridge {
        let mut state = self.parked.lock().unwrap_or_else(|err| err.into_inner());
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let parked = ParkedCartridge {
            seated,
            generation: state.next_generation,
        };
        state.by_bay.insert(parked.seated.bay, parked.clone());
        parked
    }

    pub(crate) fn parked_at(&self, bay: u16) -> Option<ParkedCartridge> {
        self.parked
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .by_bay
            .get(&bay)
            .cloned()
    }

    pub(crate) fn parked_is_current(&self, parked: &ParkedCartridge) -> bool {
        self.parked_at(parked.seated.bay)
            .is_some_and(|current| current.generation == parked.generation)
    }

    pub(crate) fn forget_parked(&self, parked: &ParkedCartridge) {
        let mut state = self.parked.lock().unwrap_or_else(|err| err.into_inner());
        if state
            .by_bay
            .get(&parked.seated.bay)
            .is_some_and(|current| current.generation == parked.generation)
        {
            state.by_bay.remove(&parked.seated.bay);
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub(crate) async fn poll_drive_health(
        &self,
        bay: u16,
        drive_uuid: Vec<u8>,
    ) -> Result<DriveHealthSnapshotRecord, Status> {
        let tx = self.drive_tx(bay)?;
        let (reply, rx) = oneshot::channel();
        tx.send(DriveCommand::PollHealth {
            drive_uuid,
            trigger: "manual",
            session_id: None,
            tape_uuid: None,
            reply,
        })
        .await
        .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
        rx.await
            .map_err(|_| Status::unavailable("drive actor stopped"))?
    }

    pub(crate) fn heartbeat_drive(&self, bay: u16, drive_uuid: Vec<u8>) -> Result<(), Status> {
        let tx = self.drive_tx(bay)?;
        let (reply, rx) = oneshot::channel();
        tx.blocking_send(DriveCommand::Heartbeat { drive_uuid, reply })
            .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
        rx.blocking_recv()
            .map_err(|_| Status::unavailable("drive actor stopped"))?
    }
}

#[derive(Debug)]
pub(crate) struct DriveReservation {
    bay: u16,
    reservations: Arc<HashMap<u16, AtomicBool>>,
    armed: bool,
}

impl DriveReservation {
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for DriveReservation {
    fn drop(&mut self) {
        if self.armed {
            if let Some(reservation) = self.reservations.get(&self.bay) {
                reservation.store(false, Ordering::SeqCst);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct TapeReservation {
    tape_uuid: TapeUuid,
    reservations: Arc<Mutex<HashSet<TapeUuid>>>,
}

impl Drop for TapeReservation {
    fn drop(&mut self) {
        self.reservations
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&self.tape_uuid);
    }
}

#[derive(Clone)]
pub(crate) struct WriteOwnerConfig {
    pub index_path: PathBuf,
    pub report: DiscoveryReport,
    pub policy: StaticAllowlist,
    pub audit_dir: PathBuf,
    pub audit_fsync: bool,
    pub audit_append_lock: Arc<std::sync::Mutex<()>>,
    pub reservations: Arc<HashMap<u16, AtomicBool>>,
    pub default_library_serial: Option<String>,
    pub library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
    pub snapshot_miss_alarm: u32,
    pub managed_library_serials: Arc<HashSet<String>>,
    pub cleaning: CleaningConfig,
    pub tape_io: TapeIoConfig,
    pub io_memory: Arc<crate::io_memory::IoMemoryReservation>,
    pub checkpoint_journal_dir: PathBuf,
    pub checkpoint_max_bytes: u64,
    pub checkpoint_max_objects: u64,
    pub checkpoint_max_age_seconds: u64,
    pub session_idle_seconds: u64,
    pub lifecycle: Option<DrivePoolLifecycle>,
}

pub(crate) struct ExclusiveGuard {
    reservations: Arc<HashMap<u16, AtomicBool>>,
}

impl ExclusiveGuard {
    pub(crate) fn from_reserved(reservations: Arc<HashMap<u16, AtomicBool>>) -> Self {
        Self { reservations }
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        release_all_reservations(&self.reservations);
    }
}

pub(crate) struct Spool {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
    cap: u64,
    keep: bool,
}

impl Spool {
    pub(crate) fn create(dir: &Path, cap: u64) -> std::io::Result<Self> {
        let path = dir.join(format!("spool-{}.bin", Uuid::new_v4()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|err| {
                std::io::Error::new(err.kind(), format!("open {}: {err}", path.display()))
            })?;
        Ok(Self {
            file,
            path,
            written: 0,
            cap,
            keep: false,
        })
    }

    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let next = self
            .written
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "spool size overflows u64")
            })?;
        if next > self.cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("spool size cap exceeded at {}", self.path.display()),
            ));
        }
        self.file.write_all(bytes).map_err(|err| {
            std::io::Error::new(err.kind(), format!("write {}: {err}", self.path.display()))
        })?;
        self.written = next;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush().map_err(|err| {
            std::io::Error::new(err.kind(), format!("flush {}: {err}", self.path.display()))
        })?;
        self.keep = true;
        Ok(self.path.clone())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn spawn_changer_actor(
    mut changer: ChangerHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<ChangerCommand> {
    let (tx, rx) = mpsc::channel::<ChangerCommand>(16);
    std::thread::Builder::new()
        .name("rem-changer-actor".to_string())
        .spawn(move || changer_loop(&mut changer, cfg, rx))
        .expect("spawn changer actor thread");
    tx
}

pub(crate) fn spawn_drive_actor(
    bay: u16,
    mut drive: DriveHandle,
    cfg: WriteOwnerConfig,
) -> mpsc::Sender<DriveCommand> {
    let (tx, rx) = mpsc::channel::<DriveCommand>(16);
    let actor_tx = tx.clone();
    std::thread::Builder::new()
        .name(format!("rem-drive-actor-{bay:04x}"))
        .spawn(move || drive_loop(bay, &mut drive, cfg, actor_tx, rx))
        .expect("spawn drive actor thread");
    tx
}

fn changer_loop(
    changer: &mut ChangerHandle,
    cfg: WriteOwnerConfig,
    mut rx: mpsc::Receiver<ChangerCommand>,
) {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_changer_commands(
                rx,
                format!("open catalog index: {err}"),
                cfg.reservations.clone(),
            );
            return;
        }
    };
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::Move { src, dst, reply } => {
                let result = changer
                    .move_medium(src, dst, &cfg.policy)
                    .map_err(|err| Status::internal(format!("move medium: {err}")));
                if result.is_ok() {
                    match observe_refreshed_library(&mut index, &cfg, changer.library()) {
                        Ok(()) => clear_library_snapshot_persist_alarm(
                            &mut index,
                            &cfg,
                            changer.library().serial.as_str(),
                        ),
                        Err(err) => record_library_observation_failure(
                            &mut index,
                            &cfg,
                            changer.library(),
                            err.message(),
                        ),
                    }
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Refresh { reply } => {
                let result = changer
                    .refresh()
                    .map_err(|err| Status::internal(format!("refresh inventory: {err}")))
                    .and_then(|()| observe_refreshed_library(&mut index, &cfg, changer.library()));
                if result.is_ok() {
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Reconcile { tape_uuid, handle } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved(cfg.reservations.clone());
                handle_reconcile(&mut index, &cfg, tape_uuid, handle);
                refresh_actor_changer(changer, &cfg);
            }
            ChangerCommand::Robotics {
                library_serial,
                action,
                handle,
            } => {
                let _exclusive_guard = ExclusiveGuard::from_reserved(cfg.reservations.clone());
                handle_robotics(&mut index, &cfg, library_serial, action, handle);
                refresh_actor_changer(changer, &cfg);
            }
        }
    }
}

fn refresh_actor_changer(changer: &mut ChangerHandle, cfg: &WriteOwnerConfig) {
    if changer.refresh().is_ok() {
        match CatalogIndex::open(cfg.index_path.as_path()) {
            Ok(mut index) => {
                if let Err(err) = observe_refreshed_library(&mut index, cfg, changer.library()) {
                    tracing::warn!("failed to observe refreshed drive catalog: {err}");
                }
            }
            Err(err) => tracing::warn!("failed to open catalog for refreshed drive catalog: {err}"),
        }
        publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
    }
}

fn drain_failed_changer_commands(
    mut rx: mpsc::Receiver<ChangerCommand>,
    message: String,
    reservations: Arc<HashMap<u16, AtomicBool>>,
) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            ChangerCommand::Move { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Refresh { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            ChangerCommand::Reconcile { handle, .. } | ChangerCommand::Robotics { handle, .. } => {
                handle.publish_failed(message.as_str(), &[("phase", "catalog")]);
                release_all_reservations(&reservations);
            }
        }
    }
}

fn release_all_reservations(reservations: &HashMap<u16, AtomicBool>) {
    for reservation in reservations.values() {
        reservation.store(false, Ordering::SeqCst);
    }
}

fn drive_loop(
    bay: u16,
    drive: &mut DriveHandle,
    cfg: WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    mut rx: mpsc::Receiver<DriveCommand>,
) {
    let mut index = match CatalogIndex::open(cfg.index_path.as_path()) {
        Ok(index) => index,
        Err(err) => {
            drain_failed_drive_commands(rx, format!("open catalog index: {err}"));
            return;
        }
    };
    let mut snapshot_misses = 0u32;
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::WaitReady {
                operation_id,
                family,
                options,
                handle,
                reservation: _reservation,
            } => handle_drive_wait_ready(
                bay,
                &mut index,
                drive,
                operation_id,
                family,
                options,
                &handle,
            ),
            DriveCommand::OpenWrite {
                pool_cfg,
                selected,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                reply,
            } => handle_drive_open_write(
                bay,
                &mut index,
                &cfg,
                actor_tx.clone(),
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenWriteActorRequest {
                    pool_cfg,
                    selected,
                    needs_drive_load,
                    library_serial,
                    barcode,
                    source_slot,
                    drive_uuid,
                    drive_serial,
                    reply,
                },
            ),
            DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load,
                library_serial,
                barcode,
                source_slot,
                drive_uuid,
                drive_serial,
                resume_target,
                daemon_epoch,
                reply,
            } => handle_drive_open_read(
                bay,
                &mut index,
                &cfg,
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenReadActorRequest {
                    tape_uuid,
                    needs_drive_load,
                    library_serial,
                    barcode,
                    source_slot,
                    drive_uuid,
                    drive_serial,
                    resume_target,
                    daemon_epoch,
                    reply,
                },
            ),
            DriveCommand::Unload { reply } => {
                let started = Instant::now();
                let result = drive
                    .unload()
                    .map(|()| started.elapsed())
                    .map_err(|err| Status::internal(format!("unload drive: {err}")));
                let _ = reply.send(result);
            }
            DriveCommand::PollHealth {
                drive_uuid,
                trigger,
                session_id,
                tape_uuid,
                reply,
            } => {
                let result = collect_drive_health_snapshot(
                    &mut index,
                    &cfg,
                    drive,
                    DriveSnapshotRequest {
                        drive_uuid,
                        trigger,
                        session_id,
                        tape_uuid,
                    },
                );
                let _ = reply.send(result);
            }
            DriveCommand::Heartbeat { drive_uuid, reply } => {
                let result = drive
                    .test_unit_ready()
                    .map_err(|err| Status::unavailable(format!("drive heartbeat: {err}")))
                    .and_then(|_| {
                        index
                            .touch_drive_last_seen(&drive_uuid)
                            .map(|_| ())
                            .map_err(status_from_state_error)
                    });
                let _ = reply.send(result);
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::failed_precondition("no active write session")));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::not_found("no active write session")));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active write session")));
            }
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active write session")));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::not_found("no active read session")));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::not_found("no active read session")));
            }
        }
    }
}

fn drain_failed_drive_commands(mut rx: mpsc::Receiver<DriveCommand>, message: String) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed(&message, &[("phase", "drive_actor")]);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::internal(message.clone())));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::internal(message.clone())));
            }
        }
    }
}

struct OpenWriteActorRequest {
    pool_cfg: TapePoolConfig,
    selected: SelectedTape,
    needs_drive_load: bool,
    library_serial: String,
    barcode: Option<String>,
    source_slot: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
}

struct OpenReadActorRequest {
    tape_uuid: [u8; 16],
    needs_drive_load: bool,
    library_serial: String,
    barcode: Option<String>,
    source_slot: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    resume_target: Option<ReadResumeTarget>,
    daemon_epoch: u64,
    reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
}

struct SessionAuditInput {
    session_id: Uuid,
    session_kind: &'static str,
    event: AuditEvent,
    tape_uuid: Option<[u8; 16]>,
    library_serial: Option<String>,
    drive_bay: Option<u16>,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
}

fn record_session_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    input: SessionAuditInput,
) -> Result<(), Status> {
    let _guard = cfg
        .audit_append_lock
        .lock()
        .map_err(|_| Status::internal("session audit append lock poisoned"))?;
    std::fs::create_dir_all(cfg.audit_dir.as_path()).map_err(|err| {
        Status::internal(format!(
            "create session audit directory {}: {err}",
            cfg.audit_dir.display()
        ))
    })?;
    let mut detail = BTreeMap::new();
    detail.insert(
        "session_kind".to_string(),
        CborValue::Text(input.session_kind.to_string()),
    );
    if let Some(tape_uuid) = input.tape_uuid {
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
    }
    if let Some(library_serial) = input.library_serial {
        detail.insert(
            "library_serial".to_string(),
            CborValue::Text(library_serial),
        );
    }
    if let Some(drive_bay) = input.drive_bay {
        detail.insert(
            "drive_bay".to_string(),
            CborValue::Integer(u64::from(drive_bay).into()),
        );
    }
    if let Some(drive_uuid) = input.drive_uuid {
        detail.insert("drive_uuid".to_string(), CborValue::Bytes(drive_uuid));
    }
    if let Some(drive_serial) = input.drive_serial {
        detail.insert("drive_serial".to_string(), CborValue::Text(drive_serial));
    }
    let mut audit = FileAuditLog::open(cfg.audit_dir.as_path(), cfg.audit_fsync)
        .map_err(crate::status_from_state_error)?;
    let (_, record) = audit
        .append_and_return_record(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(input.session_id),
            idempotency_key: None,
            event: input.event,
            subject: AuditSubject {
                kind: input.session_kind.to_string(),
                id: Some(input.session_id.to_string()),
            },
            detail,
        })
        .map_err(crate::status_from_state_error)?;
    index
        .project_audit_record(&record)
        .map_err(crate::status_from_state_error)
}

struct DriveSnapshotRequest {
    drive_uuid: Vec<u8>,
    trigger: &'static str,
    session_id: Option<Uuid>,
    tape_uuid: Option<[u8; 16]>,
}

fn collect_drive_health_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    request: DriveSnapshotRequest,
) -> Result<DriveHealthSnapshotRecord, Status> {
    let alerts = drive
        .read_tape_alerts()
        .map_err(|err| Status::unavailable(format!("read TapeAlert page: {err}")))?;
    let counters = drive
        .read_error_counters()
        .map_err(|err| Status::unavailable(format!("read error counter pages: {err}")))?;
    let tape_uuid_text = request
        .tape_uuid
        .map(|uuid| Uuid::from_bytes(uuid).to_string())
        .unwrap_or_default();
    let raw_pages = format!(
        "{{\"tape_uuid\":\"{}\",\"tape_alert\":true,\"write_error_counter\":true,\"read_error_counter\":true}}",
        tape_uuid_text
    );
    let snapshot = index
        .record_drive_health_snapshot(DriveHealthSnapshotInput {
            drive_uuid: request.drive_uuid.clone(),
            trigger: request.trigger.to_string(),
            session_id: request.session_id.map(|uuid| uuid.to_string()),
            tape_alert_flags: Some(tape_alert_flags_json(alerts.active())),
            write_errors_corrected: counters.write_errors_corrected.and_then(u64_to_i64),
            write_errors_uncorrected: counters.write_errors_uncorrected.and_then(u64_to_i64),
            read_errors_corrected: counters.read_errors_corrected.and_then(u64_to_i64),
            read_errors_uncorrected: counters.read_errors_uncorrected.and_then(u64_to_i64),
            raw_pages: Some(raw_pages),
            at_utc: None,
        })
        .map_err(crate::status_from_state_error)?;
    if alerts.is_set(20) || alerts.is_set(21) {
        let due = if alerts.is_set(20) { "now" } else { "periodic" };
        index
            .observe_managed_drive_cleaning_due(&request.drive_uuid, due)
            .map_err(crate::status_from_state_error)?;
    } else {
        index
            .touch_drive_last_seen(&request.drive_uuid)
            .map_err(crate::status_from_state_error)?;
    }
    append_drive_health_evidence(index, cfg, &snapshot)?;
    Ok(snapshot)
}

/// Append the durable evidence twin for a just-committed health snapshot and
/// project that exact record through the same replay funnel used at rebuild.
fn append_drive_health_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    snapshot: &DriveHealthSnapshotRecord,
) -> Result<(), Status> {
    let detail = crate::drive_health_audit_detail(index, snapshot)?;
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: snapshot
                .session_id
                .as_deref()
                .and_then(|value| Uuid::parse_str(value).ok()),
            idempotency_key: None,
            event: AuditEvent::DriveHealthObserved,
            subject_kind: "drive",
            subject_id: Some(crate::bytes_to_hex(snapshot.drive_uuid.as_slice())),
            detail,
        },
    )?;
    Ok(())
}

fn insert_optional_audit_text(
    detail: &mut BTreeMap<String, CborValue>,
    key: &str,
    value: Option<&String>,
) {
    if let Some(value) = value {
        detail.insert(key.to_string(), CborValue::Text(value.clone()));
    }
}

fn raise_alarm_with_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    condition_key: &str,
    kind: &str,
    severity: &str,
    alarm_detail: Option<&str>,
) -> Result<AlarmRecord, Status> {
    let alarm = index
        .raise_alarm(condition_key, kind, severity, alarm_detail)
        .map_err(crate::status_from_state_error)
        .inspect_err(
            |error| tracing::warn!(condition_key, %error, "failed to raise catalog alarm"),
        )?;
    append_alarm_evidence(index, cfg, &alarm, AuditEvent::AlarmRaised).inspect_err(
        |error| tracing::warn!(condition_key, %error, "failed to append raised-alarm evidence"),
    )?;
    Ok(alarm)
}

fn clear_alarm_with_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    condition_key: &str,
) -> Result<Option<AlarmRecord>, Status> {
    let alarm = index
        .clear_alarm(condition_key)
        .map_err(crate::status_from_state_error)
        .inspect_err(
            |error| tracing::warn!(condition_key, %error, "failed to clear catalog alarm"),
        )?;
    if let Some(alarm) = alarm.as_ref() {
        append_alarm_evidence(index, cfg, alarm, AuditEvent::AlarmCleared).inspect_err(
            |error| {
                tracing::warn!(condition_key, %error, "failed to append cleared-alarm evidence")
            },
        )?;
    }
    Ok(alarm)
}

fn append_alarm_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    alarm: &AlarmRecord,
    event: AuditEvent,
) -> Result<(), Status> {
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event,
            subject_kind: "alarm",
            subject_id: Some(alarm.condition_key.clone()),
            detail: crate::alarm_audit_detail(alarm),
        },
    )?;
    Ok(())
}

fn record_session_close_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    drive_uuid: Option<Vec<u8>>,
    session_id: Uuid,
    tape_uuid: [u8; 16],
    consecutive_misses: &mut u32,
) {
    record_session_snapshot(
        index,
        cfg,
        drive,
        drive_uuid,
        session_id,
        tape_uuid,
        "session-close",
        consecutive_misses,
    );
}

#[allow(clippy::too_many_arguments)]
fn record_session_snapshot(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    drive_uuid: Option<Vec<u8>>,
    session_id: Uuid,
    tape_uuid: [u8; 16],
    trigger: &'static str,
    consecutive_misses: &mut u32,
) {
    let Some(drive_uuid) = drive_uuid else {
        return;
    };
    match collect_drive_health_snapshot(
        index,
        cfg,
        drive,
        DriveSnapshotRequest {
            drive_uuid: drive_uuid.clone(),
            trigger,
            session_id: Some(session_id),
            tape_uuid: Some(tape_uuid),
        },
    ) {
        Ok(_) => {
            clear_snapshot_persist_alarm(index, cfg, drive_uuid.as_slice());
            *consecutive_misses = 0;
        }
        Err(err) => {
            *consecutive_misses = consecutive_misses.saturating_add(1);
            tracing::warn!(
                "drive health snapshot missed session_id={} drive_uuid={} misses={} error={}",
                session_id,
                Uuid::from_slice(&drive_uuid)
                    .map(|uuid| uuid.to_string())
                    .unwrap_or_else(|_| crate::bytes_to_hex(&drive_uuid)),
                *consecutive_misses,
                err
            );
            if cfg.snapshot_miss_alarm > 0 && *consecutive_misses >= cfg.snapshot_miss_alarm {
                let condition_key = snapshot_persist_alarm_key(&drive_uuid);
                let detail = format!(
                    "{{\"session_id\":\"{}\",\"misses\":{},\"error\":\"{}\"}}",
                    session_id,
                    *consecutive_misses,
                    err.to_string().replace('"', "'")
                );
                if let Err(alarm_err) = raise_alarm_with_evidence(
                    index,
                    cfg,
                    condition_key.as_str(),
                    "snapshot-persist-failing",
                    "warning",
                    Some(detail.as_str()),
                ) {
                    tracing::warn!(
                        "failed to raise snapshot miss alarm condition_key={} error={}",
                        condition_key,
                        alarm_err
                    );
                }
            }
        }
    }
}

fn snapshot_persist_alarm_key(drive_uuid: &[u8]) -> String {
    format!(
        "snapshot-persist-failing:{}",
        crate::bytes_to_hex(drive_uuid)
    )
}

fn clear_snapshot_persist_alarm(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive_uuid: &[u8],
) {
    let condition_key = snapshot_persist_alarm_key(drive_uuid);
    if let Err(err) = clear_alarm_with_evidence(index, cfg, condition_key.as_str()) {
        tracing::warn!(
            "failed to clear snapshot miss alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

fn tape_alert_flags_json(flags: &std::collections::BTreeSet<u8>) -> String {
    let body = flags
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

fn u64_to_i64(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}

/// Append gate for one write session: the first failed append poisons
/// the session for all further appends.
///
/// Why: after a failed `AppendFinish` the drive head position and the
/// tape's committed state are unknown territory, and the parity-append
/// guard in `ensure_selected_tape_accepts_write` keys on
/// `total_committed_ordinals > 0` — which is still 0 after a *failed*
/// first append. Without this gate a client retry passes that guard
/// and writes a fresh BOT-relative bootstrap at the current mid-tape
/// position, committing locators that later `space(tape_file_number)`
/// reads mis-resolve. Close/Abort remain allowed (already-committed
/// objects are intact); writing again requires a new session, which
/// re-runs tape selection and the identity/position preparation.
#[derive(Debug, Default)]
struct SessionAppendGate {
    poisoned: bool,
    sealed: bool,
}

#[derive(Debug)]
struct PendingCheckpointBatch {
    batch_id: Uuid,
    opened_at: Instant,
    deadline: Instant,
    logical_bytes: u64,
    used_bytes: u64,
    early_warning: bool,
    objects: Vec<crate::pool_write::PoolWriteResult>,
}

struct ParityActorSession {
    scheme: remanence_parity::ParityScheme,
    sink_state: Option<ParitySinkSessionState>,
    journal: FileTapeFileJournal,
}

fn parity_journal_path(cfg: &WriteOwnerConfig, tape_uuid: TapeUuid) -> Result<PathBuf, Status> {
    let journal_dir = cfg.checkpoint_journal_dir.parent().ok_or_else(|| {
        Status::internal("checkpoint journal directory has no parent for the parity journal")
    })?;
    Ok(journal_dir.join(format!("{}.remjournal", crate::bytes_to_hex(&tape_uuid))))
}

fn open_parity_actor_session(
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
    checkpoints: &[remanence_state::CheckpointJournalRecord],
) -> Result<ParityActorSession, Status> {
    let scheme = match &selected.parity_config {
        remanence_parity::ParityConfig::Scheme(scheme) => scheme.clone(),
        remanence_parity::ParityConfig::None => {
            return Err(Status::internal(
                "parity actor session requested for a parity-off tape",
            ));
        }
    };
    let path = parity_journal_path(cfg, selected.tape_uuid)?;
    let mut journal = FileTapeFileJournal::open(
        path,
        selected.tape_uuid,
        selected.block_size,
        scheme.clone(),
    )
    .map_err(|err| Status::internal(format!("open parity tape journal: {err}")))?;
    if journal.orphaned_bundles_truncated_on_open() != 0 {
        tracing::warn!(
            tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
            orphaned_bundle_count = journal.orphaned_bundles_truncated_on_open(),
            "truncated sink-journal bundles beyond the last checkpoint watermark"
        );
    }
    let committed = journal
        .load_committed()
        .map_err(|err| Status::internal(format!("replay parity tape journal: {err}")))?;
    let sink_state = if committed.entries.is_empty() {
        drive
            .locate(0)
            .map_err(|err| Status::unavailable(format!("locate fresh parity BOT: {err}")))?;
        let mut raw = DriveHandleRawSink::new(drive);
        let mut sink = ParitySink::new_with_journal(
            &mut raw,
            &mut journal,
            scheme.clone(),
            selected.tape_uuid,
            selected.block_size,
        )
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        sink.write_bootstrap()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        sink.into_session_state()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?
    } else {
        let checkpoint = checkpoints.last().ok_or_else(|| {
            Status::failed_precondition(
                "non-fresh parity tape has no shared checkpoint journal watermark",
            )
        })?;
        drive
            .locate(checkpoint.eod_lba)
            .map_err(|err| Status::unavailable(format!("locate parity checkpoint EOD: {err}")))?;
        let (_, prefix) = committed_prefix_from_journal(&journal, &scheme)
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        let plan = plan_resume_append_from_committed_prefix(&prefix, &scheme)
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        if !plan.sidecars_to_emit.is_empty()
            || plan.highest_protected_ordinal_before_rebuild != plan.next_data_ordinal
        {
            return Err(Status::failed_precondition(
                "checkpointed parity tape unexpectedly retains an open epoch",
            ));
        }
        let resume_result = plan
            .complete(Vec::new())
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        let directory =
            remanence_parity::sidecar_directory_from_committed_state(&committed, &scheme)
                .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        let object_rows = committed
            .entries
            .iter()
            .filter_map(|entry| entry.bootstrap_object_row.clone())
            .collect();
        let next_bootstrap_sequence = u32::try_from(
            committed
                .entries
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::Bootstrap)
                .count(),
        )
        .map_err(|_| Status::internal("bootstrap count overflows u32"))?;
        let mut raw = DriveHandleRawSink::new(drive);
        let sink = ParitySink::new_sidecar_only_from_resume(
            &mut raw,
            &mut journal,
            scheme.clone(),
            selected.tape_uuid,
            selected.block_size,
            ResumeWriterSeed {
                committed_prefix: &prefix,
                committed_prefix_sidecar_directory_entries: directory,
                committed_prefix_object_rows: object_rows,
                resume_result: &resume_result,
                live_epoch: None,
                next_bootstrap_sequence,
            },
        )
        .map_err(|err| status_from_parity_error(&err, err.to_string()))?;
        sink.into_session_state()
            .map_err(|err| status_from_parity_error(&err, err.to_string()))?
    };
    Ok(ParityActorSession {
        scheme,
        sink_state: Some(sink_state),
        journal,
    })
}

impl PendingCheckpointBatch {
    fn new(max_age: StdDuration) -> Self {
        let opened_at = Instant::now();
        Self {
            batch_id: Uuid::new_v4(),
            opened_at,
            deadline: opened_at + max_age,
            logical_bytes: 0,
            used_bytes: 0,
            early_warning: false,
            objects: Vec::new(),
        }
    }

    fn push(&mut self, logical_bytes: u64, result: crate::pool_write::PoolWriteResult) {
        self.logical_bytes = self.logical_bytes.saturating_add(logical_bytes);
        self.used_bytes = self.used_bytes.max(result.post_write_used_bytes());
        self.early_warning |= result.hardware_early_warning();
        self.objects.push(result);
    }

    fn should_checkpoint(&self, cfg: &WriteOwnerConfig) -> bool {
        self.logical_bytes >= cfg.checkpoint_max_bytes
            || self.objects.len() as u64 >= cfg.checkpoint_max_objects
    }
}

struct BarrierOutcome {
    committed_objects: Vec<pb::ObjectRecord>,
    object_count: u64,
    logical_bytes: u64,
    filemark_drain: StdDuration,
    journal_projection: StdDuration,
    checkpoint_record: remanence_state::CheckpointJournalRecord,
    sealed_after_write: bool,
}

#[derive(Debug)]
struct CheckpointBarrierFailure {
    status: Status,
    journal_durable: bool,
}

impl CheckpointBarrierFailure {
    fn before_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: false,
        }
    }

    fn after_journal(status: Status) -> Self {
        Self {
            status,
            journal_durable: true,
        }
    }
}

/// Rebuild committed-object receipts from the catalog projection made durable by a barrier.
///
/// The caller's pre-barrier WRITTEN acknowledgement is deliberately locator-free, while the
/// pending batch contains pre-projection write results. Reading the durable record's projected
/// object ids back keeps the CHECKPOINTED response aligned with the catalog's canonical
/// object/copy protobuf conversion.
fn checkpointed_objects_from_catalog(
    index: &CatalogIndex,
    checkpoint_record: &remanence_state::CheckpointJournalRecord,
    sealed_after_write: bool,
) -> Result<Vec<pb::ObjectRecord>, CheckpointBarrierFailure> {
    checkpoint_record
        .objects
        .iter()
        .map(|projection| {
            let object_id = projection.object.object_id.as_str();
            let object = index
                .get_native_object(object_id)
                .map_err(|err| {
                    CheckpointBarrierFailure::after_journal(Status::internal(format!(
                        "checkpoint is durable but catalog lookup for committed object {object_id} failed: {err}"
                    )))
                })?
                .ok_or_else(|| {
                    CheckpointBarrierFailure::after_journal(Status::internal(format!(
                        "checkpoint is durable but committed object {object_id} is absent from the catalog projection"
                    )))
                })?;
            let mut record = crate::object_record_to_proto(object).map_err(|err| {
                CheckpointBarrierFailure::after_journal(Status::internal(format!(
                    "checkpoint is durable but committed object {object_id} could not be encoded from the catalog: {}",
                    err.message()
                )))
            })?;
            let append_info = record.append_commit_info.as_mut().ok_or_else(|| {
                CheckpointBarrierFailure::after_journal(Status::internal(format!(
                    "checkpoint is durable but committed object {object_id} has no projected copies"
                )))
            })?;
            append_info.sealed_after_write = Some(sealed_after_write);
            append_info.durability = pb::AppendDurability::Checkpointed as i32;
            Ok(record)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn perform_checkpoint_barrier(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    journal: &remanence_state::FileCheckpointJournal,
    prior_records: &[remanence_state::CheckpointJournalRecord],
    tape_uuid: TapeUuid,
    checkpoint_ordinal: &mut u64,
    tape_committed_object_count: &mut u64,
    batch: &PendingCheckpointBatch,
    mut parity_session: Option<&mut ParityActorSession>,
    selected: &SelectedTape,
    pool_cfg: &TapePoolConfig,
    cfg: &WriteOwnerConfig,
) -> Result<BarrierOutcome, CheckpointBarrierFailure> {
    let drain_started = Instant::now();
    let next_ordinal = checkpoint_ordinal.checked_add(1).ok_or_else(|| {
        CheckpointBarrierFailure::before_journal(Status::internal(
            "checkpoint ordinal overflows u64",
        ))
    })?;
    let object_count = u64::try_from(batch.objects.len()).map_err(|_| {
        CheckpointBarrierFailure::before_journal(Status::internal(
            "checkpoint object count exceeds u64",
        ))
    })?;
    let next_committed_count = tape_committed_object_count
        .checked_add(object_count)
        .ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "checkpoint committed object count overflows u64",
            ))
        })?;
    let objects = batch
        .objects
        .iter()
        .map(|result| {
            result.checkpoint_projection().cloned().ok_or_else(|| {
                CheckpointBarrierFailure::before_journal(Status::internal(
                    "batched object is missing its checkpoint projection",
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let object_tape_file_bundles = if parity_session.is_some() {
        batch
            .objects
            .iter()
            .map(|result| {
                result
                    .write_report()
                    .map(|report| report.catalog.tape_file_bundle.clone())
                    .ok_or_else(|| {
                        CheckpointBarrierFailure::before_journal(Status::internal(
                            "parity checkpoint object is missing its Layer 3c bundle",
                        ))
                    })
            })
            .collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };
    let (
        checkpoint_tape_file_number,
        checkpoint_block_size,
        sync,
        checkpoint_bundle,
        scheme,
        parity_early_warning,
    ) = if let Some(parity_session) = parity_session.as_deref_mut() {
        let state = parity_session.sink_state.take().ok_or_else(|| {
            CheckpointBarrierFailure::before_journal(Status::internal(
                "parity sink session state is unavailable",
            ))
        })?;
        let mut raw = DriveHandleRawSink::new(drive);
        let mut sink = ParitySink::from_session_state(&mut raw, &mut parity_session.journal, state)
            .map_err(|err| {
                CheckpointBarrierFailure::before_journal(status_from_parity_error(
                    &err,
                    err.to_string(),
                ))
            })?;
        let closed = sink.close_open_epoch(CloseReason::Barrier).map_err(|err| {
            CheckpointBarrierFailure::before_journal(status_from_parity_error(
                &err,
                err.to_string(),
            ))
        })?;
        let sink_state = sink.into_session_state().map_err(|err| {
            CheckpointBarrierFailure::before_journal(status_from_parity_error(
                &err,
                err.to_string(),
            ))
        })?;
        let parity_early_warning = sink_state.hardware_early_warning_seen();
        parity_session.sink_state = Some(sink_state);
        (
            closed.bootstrap_tape_file_number,
            objects[0].block_size,
            closed.barrier_outcome,
            Some(closed.committed_bundle),
            Some(parity_session.scheme.clone()),
            parity_early_warning,
        )
    } else {
        let checkpoint_bootstrap = crate::pool_write::build_no_parity_checkpoint_bootstrap(
                tape_uuid,
                next_ordinal,
                prior_records,
                &objects,
                now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string()),
            )
            .map_err(|err| {
                CheckpointBarrierFailure::before_journal(Status::internal(format!(
                    "checkpoint batch {} bootstrap preparation failed; re-send all {} WRITTEN objects: {err}",
                    batch.batch_id,
                    batch.objects.len()
                )))
            })?;
        drive.write_block(&checkpoint_bootstrap.block).map_err(|err| {
                CheckpointBarrierFailure::before_journal(Status::unavailable(format!(
                    "checkpoint batch {} on-tape bootstrap write failed; re-send all {} WRITTEN objects: {err}",
                    batch.batch_id,
                    batch.objects.len()
                )))
            })?;
        drive.write_filemarks_immediate(1).map_err(|err| {
                CheckpointBarrierFailure::before_journal(Status::unavailable(format!(
                    "checkpoint batch {} bootstrap delimiter failed; re-send all {} WRITTEN objects: {err}",
                    batch.batch_id,
                    batch.objects.len()
                )))
            })?;
        let sync = drive.write_filemarks(0).map_err(|err| {
            CheckpointBarrierFailure::before_journal(Status::unavailable(format!(
                "checkpoint batch {} barrier failed; re-send all {} WRITTEN objects: {err}",
                batch.batch_id,
                batch.objects.len()
            )))
        })?;
        (
            checkpoint_bootstrap.tape_file_number,
            checkpoint_bootstrap.block_size,
            sync,
            None,
            None,
            false,
        )
    };
    let captured = drive.position().map_err(|err| {
        CheckpointBarrierFailure::before_journal(Status::unavailable(format!(
            "checkpoint batch {} READ POSITION failed; re-send all {} WRITTEN objects: {err}",
            batch.batch_id,
            batch.objects.len()
        )))
    })?;
    if captured.partition != sync.position_after.partition
        || captured.lba != sync.position_after.lba
    {
        return Err(CheckpointBarrierFailure::before_journal(
            Status::unavailable(format!(
            "checkpoint batch {} position proof mismatch: synchronous barrier reported partition {} lba {}, daemon READ POSITION observed partition {} lba {}; re-send all {} WRITTEN objects",
            batch.batch_id,
            sync.position_after.partition,
            sync.position_after.lba,
            captured.partition,
            captured.lba,
            batch.objects.len()
        )),
        ));
    }
    let filemark_drain = drain_started.elapsed();
    let record = remanence_state::CheckpointJournalRecord {
        ordinal: next_ordinal,
        committed_object_count: next_committed_count,
        eod_partition: captured.partition,
        eod_lba: captured.lba,
        tape_uuid,
        batch_id: *batch.batch_id.as_bytes(),
        checkpoint_tape_file_number,
        block_size: checkpoint_block_size,
        objects,
        scheme,
        object_tape_file_bundles,
        checkpoint_bundle,
    };
    let projection_started = Instant::now();
    journal.append(&record).map_err(|err| {
        CheckpointBarrierFailure::before_journal(Status::internal(format!(
            "checkpoint batch {} journal fsync failed; re-send all {} WRITTEN objects: {err}",
            batch.batch_id,
            batch.objects.len(),
        )))
    })?;
    *checkpoint_ordinal = next_ordinal;
    *tape_committed_object_count = next_committed_count;
    index
        .project_checkpoint_record(&record)
        .map_err(|err| {
            CheckpointBarrierFailure::after_journal(Status::internal(format!(
                "checkpoint is durable in the journal but SQLite projection failed; close the session and retry after journal replay: {err}"
            )))
        })?;
    let barrier_used_bytes = captured
        .lba
        .checked_mul(u64::from(checkpoint_block_size))
        .ok_or_else(|| {
            CheckpointBarrierFailure::after_journal(Status::internal(
                "checkpoint post-barrier used-byte count overflows u64",
            ))
        })?;
    let used_bytes = batch.used_bytes.max(barrier_used_bytes);
    let sealed_after_write = crate::pool_write::seal_selected_tape_at_barrier(
        index,
        selected,
        pool_cfg,
        crate::pool_write::TapePositionAfterWrite {
            used_bytes,
            early_warning: batch.early_warning || sync.early_warning || parity_early_warning,
        },
    )
    .map_err(|err| CheckpointBarrierFailure::after_journal(status_from_pool_write_error(err)))?;
    if sealed_after_write {
        if let Err(err) = append_tape_sealed_evidence(index, cfg, selected.tape_uuid) {
            tracing::warn!(error = %err, "failed to append tape sealing evidence");
        }
        if let Some(parity_session) = parity_session {
            let terminal_result = (|| -> Result<(), ParityError> {
                let state = parity_session
                    .sink_state
                    .take()
                    .ok_or(ParityError::Invariant(
                        "parity sink session state is unavailable at seal",
                    ))?;
                let mut raw = DriveHandleRawSink::new(drive);
                let mut sink =
                    ParitySink::from_session_state(&mut raw, &mut parity_session.journal, state)?;
                sink.close_open_epoch(CloseReason::Finish)?;
                parity_session.sink_state = Some(sink.into_session_state()?);
                Ok(())
            })();
            if let Err(err) = terminal_result {
                tracing::warn!(
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    error = %err,
                    "terminal parity bootstrap append failed after the checkpoint journal became durable"
                );
            }
        } else {
            let mut records = prior_records.to_vec();
            records.push(record.clone());
            let terminal_result = (|| -> Result<(), PoolWriteError> {
                let terminal = crate::pool_write::build_no_parity_terminal_bootstrap(
                    tape_uuid,
                    &records,
                    now_rfc3339()?,
                )?;
                drive.write_block(&terminal.block)?;
                drive.write_filemarks_immediate(1)?;
                drive.write_filemarks(0)?;
                Ok(())
            })();
            if let Err(err) = terminal_result {
                tracing::warn!(
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    error = %err,
                    "terminal no-parity bootstrap append failed after the checkpoint journal became durable"
                );
            }
        }
    }
    let journal_projection = projection_started.elapsed();
    tracing::info!(
        target: "remanence_write_diag",
        phase = "checkpoint_barrier",
        batch_id = %batch.batch_id,
        tape_uuid = %Uuid::from_bytes(tape_uuid),
        batch_objects = object_count,
        batch_logical_bytes = batch.logical_bytes,
        position_partition = captured.partition,
        position_lba = captured.lba,
        position_proof_ok = true,
        filemark_drain_ms = crate::diagnostics::duration_ms(filemark_drain),
        journal_projection_ms = crate::diagnostics::duration_ms(journal_projection),
        "remanence_write_diag",
    );
    let committed_objects = checkpointed_objects_from_catalog(index, &record, sealed_after_write)?;
    Ok(BarrierOutcome {
        committed_objects,
        object_count,
        logical_bytes: batch.logical_bytes,
        filemark_drain,
        journal_projection,
        checkpoint_record: record,
        sealed_after_write,
    })
}

fn fence_failed_checkpoint_batch(
    index: &mut CatalogIndex,
    selected: &SelectedTape,
    batch: &PendingCheckpointBatch,
    status: Status,
) -> Status {
    let barcode = index
        .get_tape(&selected.tape_uuid)
        .ok()
        .flatten()
        .and_then(|tape| tape.voltag);
    let caller_objects = batch
        .objects
        .iter()
        .map(|result| result.object.caller_object_id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let evidence = serde_json::json!({
        "batch_id": batch.batch_id.to_string(),
        "caller_object_ids": caller_objects,
        "error": status.message(),
    })
    .to_string();
    match index.record_tape_io_fence(remanence_state::TapeIoFenceInput {
        tape_uuid: selected.tape_uuid,
        barcode,
        reason: "checkpoint_barrier_failed".to_string(),
        evidence_json: Some(evidence),
    }) {
        Ok(_) => status,
        Err(err) => {
            tracing::error!(
                tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
                batch_id = %batch.batch_id,
                error = %err,
                "failed to persist checkpoint batch tape-I/O fence"
            );
            Status::internal(format!(
                "{}; additionally failed to persist the required tape fence: {err}",
                status.message()
            ))
        }
    }
}

fn arm_checkpoint_timer(
    actor_tx: mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    batch_id: Uuid,
    max_age: StdDuration,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(format!("rem-checkpoint-timer-{session_id}"))
        .spawn(move || {
            std::thread::sleep(max_age);
            let _ = actor_tx.blocking_send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Timer,
                expected_batch_id: Some(batch_id),
                reply: None,
            });
        })
        .map(|_| ())
}

fn arm_checkpoint_idle_close(
    actor_tx: mpsc::Sender<DriveCommand>,
    session_id: Uuid,
    checkpoint_batch_id: Uuid,
    idle: StdDuration,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name(format!("rem-checkpoint-idle-{session_id}"))
        .spawn(move || {
            std::thread::sleep(idle);
            let _ = actor_tx.blocking_send(DriveCommand::TimerIdleClose {
                session_id,
                checkpoint_batch_id,
            });
        })
        .map(|_| ())
}

fn park_timer_closed_session(cfg: &WriteOwnerConfig, session_id: Uuid) -> Result<(), Status> {
    let Some(lifecycle) = cfg.lifecycle.as_ref() else {
        return Ok(());
    };
    let mounted = lifecycle
        .sessions
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .remove(&session_id);
    let Some(mounted) = mounted else {
        return Ok(());
    };
    let parked = mounted.home_slot.map(|home_slot| {
        let seated = SeatedCartridge {
            bay: mounted.bay,
            library_serial: mounted.library_serial,
            barcode: mounted.barcode,
            home_slot,
            tape_uuid: Some(mounted.tape_uuid),
            prior_session_id: Some(session_id),
        };
        let mut parked = lifecycle
            .parked
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        parked.next_generation = parked.next_generation.wrapping_add(1).max(1);
        let generation = parked.next_generation;
        let parked_cartridge = ParkedCartridge { seated, generation };
        parked.by_bay.insert(mounted.bay, parked_cartridge.clone());
        parked_cartridge
    });
    if let Some(reservation) = cfg.reservations.get(&mounted.bay) {
        reservation.store(false, Ordering::SeqCst);
    }
    if let (Some(parked), Some(timer_park_tx)) = (parked, lifecycle.timer_park_tx.as_ref()) {
        timer_park_tx.send(parked).map_err(|_| {
            Status::internal("timer-closed session could not arm lazy idle dismount")
        })?;
    }
    Ok(())
}

impl SessionAppendGate {
    fn check(&self) -> Result<(), Status> {
        if self.sealed {
            Err(Status::resource_exhausted(
                "selected tape sealed at the checkpoint boundary; reopen against the pool to roll placement",
            ))
        } else if self.poisoned {
            Err(Status::failed_precondition(
                "write session poisoned by a failed append; abort the session and open a new one",
            ))
        } else {
            Ok(())
        }
    }

    fn record_failure(&mut self) {
        self.poisoned = true;
    }

    fn record_sealed(&mut self) {
        self.sealed = true;
    }
}

struct SessionOpenReadinessContext<'a> {
    action: &'static str,
    bay: u16,
    library_serial: &'a str,
    barcode: Option<&'a str>,
    source_slot: Option<u16>,
    drive_serial: Option<&'a str>,
    needs_drive_load: bool,
}

#[cfg(test)]
const SESSION_OPEN_CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_millis(0);
#[cfg(not(test))]
const SESSION_OPEN_CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_secs(1);

fn session_open_short_probe_or_load(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
) -> Result<(), Status> {
    session_open_reject_admission_conflicts(index, &ctx)?;
    let family = session_open_media_family(ctx.barcode);
    let first = drive.probe_media_readiness(family);
    if first.is_ready() {
        return Ok(());
    }
    if session_open_readiness_requires_immediate_load(&ctx, &first) {
        return session_open_immediate_load_then_probe(
            index,
            drive,
            ctx,
            family,
            "drive LOAD IMMED",
        );
    }
    if session_open_readiness_should_retry_once(&first) {
        let second = drive.probe_media_readiness(family);
        if second.is_ready() {
            return Ok(());
        }
        if session_open_readiness_requires_immediate_load(&ctx, &second) {
            return session_open_immediate_load_then_probe(
                index,
                drive,
                ctx,
                family,
                "drive LOAD IMMED after retry",
            );
        }
        return Err(record_session_open_readiness_fence(
            index,
            &ctx,
            "session_open_short_probe",
            &second,
        ));
    }
    Err(record_session_open_readiness_fence(
        index,
        &ctx,
        "session_open_short_probe",
        &first,
    ))
}

fn handle_drive_wait_ready(
    bay: u16,
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    operation_id: Uuid,
    family: MediaFamily,
    options: MediaReadinessWaitOptions,
    handle: &crate::operations::OperationHandle,
) {
    handle.publish_state(
        pb::OperationState::Running,
        &[("phase", "readiness_poll"), ("state", "starting")],
    );
    let result = poll_drive_media_readiness(
        index,
        drive,
        operation_id,
        family,
        options,
        handle,
        "grpc_wait_ready",
    );

    match result {
        Ok(poll) if poll.readiness.is_ready() => handle.publish_state(
            pb::OperationState::Succeeded,
            &[("phase", "ready"), ("state", "ready")],
        ),
        Ok(poll) => {
            let state = if poll.timed_out {
                "timeout_unknown"
            } else {
                session_open_readiness_state(&poll.readiness)
            };
            let summary = if poll.timed_out {
                format!(
                    "timed out waiting for media readiness in drive bay 0x{bay:04x}: {}",
                    session_open_readiness_summary(&poll.readiness)
                )
            } else {
                format!(
                    "media readiness became non-retryable in drive bay 0x{bay:04x}: {}",
                    session_open_readiness_summary(&poll.readiness)
                )
            };
            handle.publish_failed(&summary, &[("phase", "readiness_poll"), ("state", state)]);
        }
        Err(error) if handle.is_cancelled() => handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", error.as_str())],
        ),
        Err(error) => handle.publish_failed(
            error.as_str(),
            &[("phase", "readiness_poll"), ("state", "recording_failed")],
        ),
    }
}

fn poll_drive_media_readiness(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    operation_id: Uuid,
    family: MediaFamily,
    options: MediaReadinessWaitOptions,
    handle: &crate::operations::OperationHandle,
    phase: &str,
) -> Result<MediaReadinessPoll, String> {
    drive.wait_for_media_readiness(
        family,
        None,
        options,
        || {
            handle
                .is_cancelled()
                .then(|| "daemon cancellation".to_string())
        },
        |event| match event {
            MediaReadinessWaitEvent::Poll(poll) => {
                record_session_open_readiness_poll_transition(
                    index,
                    operation_id,
                    phase,
                    &poll.readiness,
                    poll.timed_out,
                )
                .map_err(|error| format!("record media readiness transition: {error}"))?;
                let attempts = poll.attempts.to_string();
                let elapsed_seconds = poll.elapsed.as_secs().to_string();
                let state = if poll.timed_out {
                    "timeout_unknown"
                } else {
                    session_open_readiness_state(&poll.readiness)
                };
                handle.publish_state(
                    pb::OperationState::Running,
                    &[
                        ("phase", "readiness_poll"),
                        ("state", state),
                        ("attempts", attempts.as_str()),
                        ("elapsed_seconds", elapsed_seconds.as_str()),
                    ],
                );
                Ok(())
            }
            MediaReadinessWaitEvent::Cancelled(_) => Ok(()),
        },
    )
}

fn session_open_reject_admission_conflicts(
    index: &mut CatalogIndex,
    ctx: &SessionOpenReadinessContext<'_>,
) -> Result<(), Status> {
    let conflicts = index
        .media_readiness_admission_conflicts(ctx.library_serial, Some(ctx.bay), ctx.barcode, false)
        .map_err(status_from_state_error)?;
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(Status::failed_precondition(
        session_open_admission_error_message(ctx, &conflicts),
    ))
}

fn session_open_admission_error_message(
    ctx: &SessionOpenReadinessContext<'_>,
    conflicts: &[remanence_state::MediaReadinessOperationRecord],
) -> String {
    let conflict_summary = conflicts
        .iter()
        .map(|record| {
            format!(
                "operation={} state={} drive=0x{:04x} barcode={} quarantine={}",
                record.operation_id,
                record.state,
                record.drive_element,
                record.barcode.as_deref().unwrap_or("(unknown)"),
                record.quarantine_id.as_deref().unwrap_or("(none)")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let first_operation = conflicts
        .first()
        .map(|record| record.operation_id.as_str())
        .unwrap_or("(unknown)");
    format!(
        "{} blocked by active media-readiness fence library={} drive=0x{:04x} barcode={}: {}; run `rem tape wait-ready --library {} --resume {} --wait --json` or inspect quarantine before opening a session",
        ctx.action,
        ctx.library_serial,
        ctx.bay,
        ctx.barcode.unwrap_or("(unknown)"),
        conflict_summary,
        ctx.library_serial,
        first_operation,
    )
}

fn session_open_immediate_load_then_probe(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
    family: MediaFamily,
    detail_prefix: &str,
) -> Result<(), Status> {
    let operation_id = Uuid::new_v4();
    if let Err(err) = record_session_open_readiness_operation(index, operation_id, &ctx) {
        return Err(session_open_recording_failure_status(
            &ctx,
            None,
            "record_media_readiness_operation",
            &err,
        ));
    }
    if let Err(err) = record_session_open_mechanical_transition(
        index,
        operation_id,
        "session_open_immediate_load",
        "pre_ready_loading",
        Some(0x1b),
        None,
    ) {
        return Err(session_open_recording_failure_status(
            &ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        ));
    }
    std::thread::sleep(SESSION_OPEN_CONDITIONAL_LOAD_SETTLE);
    if let Err(err) = drive.load_immediate() {
        return Err(record_session_open_command_fence_on_operation(
            index,
            operation_id,
            &ctx,
            Some(0x1b),
            format!("{detail_prefix}: {err}"),
        ));
    }
    session_open_short_probe_after_load(index, drive, ctx, family, operation_id)
}

fn session_open_short_probe_after_load(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
    family: MediaFamily,
    operation_id: Uuid,
) -> Result<(), Status> {
    let first = drive.probe_media_readiness(family);
    if first.is_ready() {
        record_session_open_readiness_transition_on_operation(
            index,
            operation_id,
            &ctx,
            "session_open_after_immediate_load",
            &first,
        )?;
        return Ok(());
    }
    if session_open_readiness_should_retry_once(&first) {
        let second = drive.probe_media_readiness(family);
        if second.is_ready() {
            record_session_open_readiness_transition_on_operation(
                index,
                operation_id,
                &ctx,
                "session_open_after_immediate_load",
                &second,
            )?;
            return Ok(());
        }
        return Err(record_session_open_readiness_fence_on_operation(
            index,
            operation_id,
            &ctx,
            "session_open_after_immediate_load",
            &second,
        ));
    }
    Err(record_session_open_readiness_fence_on_operation(
        index,
        operation_id,
        &ctx,
        "session_open_after_immediate_load",
        &first,
    ))
}

fn session_open_media_family(barcode: Option<&str>) -> MediaFamily {
    if barcode
        .and_then(crate::lto_generation_from_voltag)
        .is_some_and(|generation| generation.generation_number() >= 9)
    {
        MediaFamily::Lto9OrLater
    } else {
        MediaFamily::Unknown
    }
}

fn session_open_readiness_requires_immediate_load(
    ctx: &SessionOpenReadinessContext<'_>,
    readiness: &MediaReadiness,
) -> bool {
    match readiness {
        MediaReadiness::BecomingReady { ascq: 0x02, .. } => true,
        MediaReadiness::NoMedium { .. } => ctx.needs_drive_load,
        _ => false,
    }
}

fn session_open_readiness_should_retry_once(readiness: &MediaReadiness) -> bool {
    matches!(
        readiness,
        MediaReadiness::UnitAttention { .. } | MediaReadiness::TargetBusy { .. }
    )
}

fn record_session_open_command_fence_on_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
    opcode: Option<u8>,
    detail: String,
) -> Status {
    if let Err(err) = record_session_open_mechanical_transition(
        index,
        operation_id,
        "session_open_immediate_load",
        "transport_unknown",
        opcode,
        Some(detail.clone()),
    ) {
        return session_open_recording_failure_status(
            ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        );
    }
    Status::failed_precondition(session_open_readiness_error_message(
        ctx,
        operation_id,
        "transport_unknown",
        detail.as_str(),
    ))
}

fn record_session_open_mechanical_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    state: &str,
    opcode: Option<u8>,
    detail: Option<String>,
) -> Result<(), remanence_state::StateError> {
    index
        .record_media_readiness_transition(remanence_state::MediaReadinessTransitionInput {
            operation_id,
            phase: Some(phase.to_string()),
            state: state.to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            last_cdb_opcode: opcode,
            last_sense_raw: None,
            last_sense_key: None,
            last_asc: None,
            last_ascq: None,
            last_host_status: None,
            last_driver_status: None,
            target_status: None,
            transport_class: (state == "transport_unknown").then(|| "unknown".to_string()),
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json: detail.map(|value| session_open_json_detail("detail", value.as_str())),
            quarantine_id: session_open_state_requires_release(state)
                .then(|| session_open_quarantine_id(operation_id)),
        })
        .map(|_| ())
}

fn record_session_open_readiness_fence(
    index: &mut CatalogIndex,
    ctx: &SessionOpenReadinessContext<'_>,
    phase: &str,
    readiness: &MediaReadiness,
) -> Status {
    let operation_id = Uuid::new_v4();
    if let Err(err) = record_session_open_readiness_operation(index, operation_id, ctx) {
        return session_open_recording_failure_status(
            ctx,
            None,
            "record_media_readiness_operation",
            &err,
        );
    }
    record_session_open_readiness_fence_on_operation(index, operation_id, ctx, phase, readiness)
}

fn record_session_open_readiness_fence_on_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
    phase: &str,
    readiness: &MediaReadiness,
) -> Status {
    if let Err(err) =
        record_session_open_readiness_transition(index, operation_id, phase, readiness)
    {
        return session_open_recording_failure_status(
            ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        );
    }
    let state = session_open_readiness_state(readiness);
    Status::failed_precondition(session_open_readiness_error_message(
        ctx,
        operation_id,
        state,
        session_open_readiness_summary(readiness).as_str(),
    ))
}

fn record_session_open_readiness_transition_on_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
    phase: &str,
    readiness: &MediaReadiness,
) -> Result<(), Status> {
    record_session_open_readiness_transition(index, operation_id, phase, readiness).map_err(|err| {
        session_open_recording_failure_status(
            ctx,
            Some(operation_id),
            "record_media_readiness_transition",
            &err,
        )
    })
}

fn record_session_open_readiness_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    readiness: &MediaReadiness,
) -> Result<(), remanence_state::StateError> {
    record_session_open_readiness_poll_transition(index, operation_id, phase, readiness, false)
}

fn record_session_open_readiness_poll_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    readiness: &MediaReadiness,
    timed_out: bool,
) -> Result<(), remanence_state::StateError> {
    let state = if timed_out {
        "timeout_unknown"
    } else {
        session_open_readiness_state(readiness)
    };
    let (sense_key, asc, ascq, target_status, transport_class, last_error_json, sense_raw) =
        session_open_readiness_evidence(readiness);
    index
        .record_media_readiness_transition(remanence_state::MediaReadinessTransitionInput {
            operation_id,
            phase: Some(phase.to_string()),
            state: state.to_string(),
            dirty_scope: Some(if readiness.is_ready() {
                "none".to_string()
            } else {
                "drive+tape".to_string()
            }),
            last_cdb_opcode: Some(0x00),
            last_sense_raw: sense_raw,
            last_sense_key: sense_key,
            last_asc: asc,
            last_ascq: ascq,
            last_host_status: None,
            last_driver_status: None,
            target_status,
            transport_class,
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json,
            quarantine_id: session_open_state_requires_release(state)
                .then(|| session_open_quarantine_id(operation_id)),
        })
        .map(|_| ())
}

fn record_session_open_readiness_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
) -> Result<(), remanence_state::StateError> {
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: ctx.library_serial.to_string(),
            changer_sg: None,
            drive_element: ctx.bay,
            drive_sg: None,
            drive_serial: ctx.drive_serial.map(ToOwned::to_owned),
            barcode: ctx.barcode.map(ToOwned::to_owned),
            source_slot: ctx.source_slot,
            media_generation: ctx
                .barcode
                .and_then(crate::lto_generation_from_voltag)
                .map(|generation| generation.generation_number()),
            phase: "session_open_short_probe".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .map(|_| ())
}

fn session_open_readiness_state(readiness: &MediaReadiness) -> &'static str {
    match readiness {
        MediaReadiness::Ready => "ready",
        MediaReadiness::BecomingReady {
            media_initializing: true,
            ..
        } => "media_initializing",
        MediaReadiness::BecomingReady { .. } => "becoming_ready",
        MediaReadiness::UnitAttention { .. } => "unit_attention",
        MediaReadiness::TargetBusy { .. } => "target_busy",
        MediaReadiness::ReservationConflict => "reservation_conflict",
        MediaReadiness::TransportUnknown { .. } => "transport_unknown",
        MediaReadiness::NoMedium { .. }
        | MediaReadiness::RepeatedUnitAttention { .. }
        | MediaReadiness::TerminalNotReady { .. }
        | MediaReadiness::CheckCondition { .. }
        | MediaReadiness::UndecodedCheckCondition { .. }
        | MediaReadiness::TaskAborted
        | MediaReadiness::UnexpectedStatus { .. }
        | MediaReadiness::InvalidRequest { .. } => "terminal_error",
    }
}

fn session_open_state_requires_release(state: &str) -> bool {
    matches!(
        state,
        "aborted_unknown"
            | "timeout_unknown"
            | "transport_unknown"
            | "terminal_error"
            | "reservation_conflict"
    )
}

type SessionOpenReadinessEvidence = (
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn session_open_readiness_evidence(readiness: &MediaReadiness) -> SessionOpenReadinessEvidence {
    match readiness {
        MediaReadiness::Ready => (None, None, None, None, None, None, None),
        MediaReadiness::BecomingReady { ascq, .. } => {
            (Some(0x02), Some(0x04), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::NoMedium { ascq } => {
            (Some(0x02), Some(0x3a), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::UnitAttention { asc, ascq }
        | MediaReadiness::RepeatedUnitAttention { asc, ascq } => {
            (Some(0x06), Some(*asc), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::TerminalNotReady { ascq, action } => (
            Some(0x02),
            Some(0x04),
            Some(*ascq),
            None,
            None,
            Some(session_open_json_detail("action", action)),
            None,
        ),
        MediaReadiness::CheckCondition { key, asc, ascq } => {
            (Some(*key), Some(*asc), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::UndecodedCheckCondition { sense } => (
            None,
            None,
            None,
            None,
            None,
            Some(session_open_json_detail(
                "error",
                "undecoded_check_condition",
            )),
            Some(crate::bytes_to_hex(sense)),
        ),
        MediaReadiness::TargetBusy { status } | MediaReadiness::UnexpectedStatus { status } => {
            (None, None, None, Some(*status), None, None, None)
        }
        MediaReadiness::ReservationConflict => (None, None, None, Some(0x18), None, None, None),
        MediaReadiness::TaskAborted => (None, None, None, Some(0x40), None, None, None),
        MediaReadiness::TransportUnknown { detail } => (
            None,
            None,
            None,
            None,
            Some("unknown".to_string()),
            Some(session_open_json_detail("detail", detail)),
            None,
        ),
        MediaReadiness::InvalidRequest { detail } => (
            None,
            None,
            None,
            None,
            None,
            Some(session_open_json_detail("detail", detail)),
            None,
        ),
    }
}

fn session_open_readiness_summary(readiness: &MediaReadiness) -> String {
    match readiness {
        MediaReadiness::Ready => "ready".to_string(),
        MediaReadiness::BecomingReady {
            ascq,
            media_initializing,
        } => {
            if *media_initializing {
                format!("media initializing/calibrating on TEST UNIT READY sense 02/04/{ascq:02x}")
            } else {
                format!("logical unit becoming ready on TEST UNIT READY sense 02/04/{ascq:02x}")
            }
        }
        MediaReadiness::NoMedium { ascq } => {
            format!("drive reports no medium on TEST UNIT READY sense 02/3a/{ascq:02x}")
        }
        MediaReadiness::UnitAttention { asc, ascq } => {
            format!("unit attention during session-open readiness probe 06/{asc:02x}/{ascq:02x}")
        }
        MediaReadiness::RepeatedUnitAttention { asc, ascq } => {
            format!("repeated unit attention during session-open readiness probe 06/{asc:02x}/{ascq:02x}")
        }
        MediaReadiness::TerminalNotReady { ascq, action } => {
            format!("terminal not-ready state {action} on TEST UNIT READY sense 02/04/{ascq:02x}")
        }
        MediaReadiness::CheckCondition { key, asc, ascq } => {
            format!("readiness probe check condition {key:02x}/{asc:02x}/{ascq:02x}")
        }
        MediaReadiness::UndecodedCheckCondition { .. } => {
            "readiness probe returned undecoded check condition".to_string()
        }
        MediaReadiness::TargetBusy { status } => {
            format!("target busy during readiness probe status=0x{status:02x}")
        }
        MediaReadiness::ReservationConflict => {
            "reservation conflict during readiness probe".to_string()
        }
        MediaReadiness::TaskAborted => "task aborted during readiness probe".to_string(),
        MediaReadiness::UnexpectedStatus { status } => {
            format!("unexpected target status during readiness probe status=0x{status:02x}")
        }
        MediaReadiness::TransportUnknown { detail } => {
            format!("transport completion unknown during readiness probe: {detail}")
        }
        MediaReadiness::InvalidRequest { detail } => {
            format!("invalid readiness probe request: {detail}")
        }
    }
}

fn session_open_quarantine_id(operation_id: Uuid) -> String {
    format!("mrq-{operation_id}")
}

fn session_open_json_detail(field: &str, value: &str) -> String {
    format!(
        "{{\"{}\":\"{}\"}}",
        session_open_json_escape(field),
        session_open_json_escape(value)
    )
}

fn session_open_json_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\u{:04x}", ch as u32);
            }
            ch => escaped.push(ch),
        }
    }
    escaped
}

fn session_open_readiness_error_message(
    ctx: &SessionOpenReadinessContext<'_>,
    operation_id: Uuid,
    state: &str,
    summary: &str,
) -> String {
    format!(
        "{} blocked by media-readiness fence operation={} library={} drive=0x{:04x} barcode={} media_readiness_state={state}: {summary}; leave the cartridge in place and run `rem tape wait-ready --library {} --resume {} --wait --json`",
        ctx.action,
        operation_id,
        ctx.library_serial,
        ctx.bay,
        ctx.barcode.unwrap_or("(unknown)"),
        ctx.library_serial,
        operation_id,
    )
}

fn session_open_recording_failure_status(
    ctx: &SessionOpenReadinessContext<'_>,
    operation_id: Option<Uuid>,
    phase: &str,
    err: &dyn std::fmt::Display,
) -> Status {
    let operation = operation_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "(unrecorded)".to_string());
    Status::failed_precondition(format!(
        "{} blocked by media-readiness recording failure operation={} library={} drive=0x{:04x} barcode={} media_readiness_state=recording_failed: {phase}: {err}; leave the cartridge in place and inspect the catalog DB before retrying",
        ctx.action,
        operation,
        ctx.library_serial,
        ctx.bay,
        ctx.barcode.unwrap_or("(unknown)"),
    ))
}

struct CloseWriteActorInput<'a> {
    index: &'a mut CatalogIndex,
    cfg: &'a WriteOwnerConfig,
    drive: &'a mut DriveHandle,
    drive_uuid: &'a Option<Vec<u8>>,
    drive_serial: &'a Option<String>,
    snapshot_misses: &'a mut u32,
    session_id: Uuid,
    tape_uuid: TapeUuid,
    library_serial: &'a str,
    bay: u16,
    objects_committed: u64,
    bytes_committed: u64,
    opened_at_utc: &'a str,
    last_checkpoint_at_utc: Option<&'a str>,
    state: pb::write_session::State,
    append_commit_diagnostics: crate::pool_write::AppendCommitDiagnostics,
    checkpointed_objects: &'a [pb::ObjectRecord],
}

fn close_write_actor(input: CloseWriteActorInput<'_>) -> Result<CloseWriteActorReply, Status> {
    let mut diagnostics = CloseWriteActorDiagnostics {
        filemark_write_drain: input.append_commit_diagnostics.filemark_write_drain,
        catalog_journal_fsync: input.append_commit_diagnostics.catalog_journal_fsync,
        ..CloseWriteActorDiagnostics::default()
    };

    let snapshot_started = Instant::now();
    record_session_close_snapshot(
        input.index,
        input.cfg,
        input.drive,
        input.drive_uuid.clone(),
        input.session_id,
        input.tape_uuid,
        input.snapshot_misses,
    );
    diagnostics.drive_snapshot = snapshot_started.elapsed();

    let mut session = session_proto(WriteSessionProtoInput {
        session_id: input.session_id,
        tape_uuid: &input.tape_uuid,
        state: input.state,
        objects_committed: input.objects_committed,
        bytes_committed: input.bytes_committed,
        opened_at_utc: input.opened_at_utc,
        last_checkpoint_at_utc: input.last_checkpoint_at_utc,
        drive_element_address: input.bay,
        pending_batch: None,
    });
    session.checkpointed_objects = input.checkpointed_objects.to_vec();
    session.committed_copies = input
        .checkpointed_objects
        .iter()
        .flat_map(|object| object.copies.iter().cloned())
        .collect();
    let audit_started = Instant::now();
    record_session_event(
        input.index,
        input.cfg,
        SessionAuditInput {
            session_id: input.session_id,
            session_kind: "write",
            event: AuditEvent::SessionClosed,
            tape_uuid: Some(input.tape_uuid),
            library_serial: Some(input.library_serial.to_string()),
            drive_bay: Some(input.bay),
            drive_uuid: input.drive_uuid.clone(),
            drive_serial: input.drive_serial.clone(),
        },
    )?;
    diagnostics.session_audit_projection = audit_started.elapsed();

    Ok(CloseWriteActorReply {
        session,
        diagnostics,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_drive_open_write(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    actor_tx: mpsc::Sender<DriveCommand>,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    request: OpenWriteActorRequest,
) {
    let OpenWriteActorRequest {
        pool_cfg,
        selected,
        needs_drive_load,
        library_serial,
        barcode,
        source_slot,
        drive_uuid,
        drive_serial,
        reply,
    } = request;
    let actor_open_started = Instant::now();
    let session_id = Uuid::new_v4();
    if let Err(status) = session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "open write session",
            bay,
            library_serial: library_serial.as_str(),
            barcode: barcode.as_deref(),
            source_slot,
            drive_serial: drive_serial.as_deref(),
            needs_drive_load,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }

    let tape_uuid = selected.tape_uuid;
    if let Err(status) = session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode.as_deref(),
        "open write session",
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if let Err(status) = prepare_drive_for_write(drive, &tape_uuid, selected.block_size, session_id)
    {
        let _ = reply.send(Err(status));
        return;
    }
    tracing::info!(
        target: "remanence_write_diag",
        phase = "drive_open_actor",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(tape_uuid),
        needs_drive_load,
        block_size_bytes = selected.block_size,
        elapsed_ms = crate::diagnostics::duration_ms(actor_open_started.elapsed()),
        "remanence_write_diag",
    );

    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let mut objects_committed = 0u64;
    let mut bytes_committed = 0u64;
    let mut last_checkpoint_at_utc = None;
    let checkpoint_journal = match remanence_state::FileCheckpointJournal::open(
        cfg.checkpoint_journal_dir.as_path(),
        tape_uuid,
    ) {
        Ok(journal) => journal,
        Err(err) => {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    };
    let mut durable_checkpoint_records = match checkpoint_journal.replay() {
        Ok(records) => records,
        Err(err) => {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    };
    for record in &durable_checkpoint_records {
        if let Err(err) = index.project_checkpoint_record(record) {
            let _ = reply.send(Err(status_from_state_error(err)));
            return;
        }
    }
    let last_durable_checkpoint = durable_checkpoint_records.last().cloned();
    let mut parity_session = if matches!(
        selected.parity_config,
        remanence_parity::ParityConfig::Scheme(_)
    ) {
        match open_parity_actor_session(drive, cfg, &selected, &durable_checkpoint_records) {
            Ok(session) => Some(session),
            Err(status) => {
                let _ = reply.send(Err(status));
                return;
            }
        }
    } else {
        None
    };
    let mut next_batched_append = if parity_session.is_none() {
        match crate::pool_write::first_batched_append_context(
            index,
            &selected,
            &durable_checkpoint_records,
        ) {
            Ok(context) => Some(context),
            Err(err) => {
                let _ = reply.send(Err(status_from_pool_write_error(err)));
                return;
            }
        }
    } else {
        None
    };
    let mut checkpoint_ordinal = last_durable_checkpoint
        .as_ref()
        .map_or(0, |record| record.ordinal);
    let mut tape_committed_object_count = last_durable_checkpoint
        .as_ref()
        .map_or(0, |record| record.committed_object_count);
    let mut pending_batch: Option<PendingCheckpointBatch> = None;
    let mut committed_receipts = Vec::<pb::ObjectRecord>::new();
    let mut timer_checkpoint_waiting: Option<Uuid> = None;
    if let Err(status) = record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id,
            session_kind: "write",
            event: AuditEvent::SessionOpened,
            tape_uuid: Some(tape_uuid),
            library_serial: Some(library_serial.clone()),
            drive_bay: Some(bay),
            drive_uuid: drive_uuid.clone(),
            drive_serial: drive_serial.clone(),
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    let open_reply = session_proto(WriteSessionProtoInput {
        session_id,
        tape_uuid: &tape_uuid,
        state: pb::write_session::State::WriteSessionStateOpen,
        objects_committed,
        bytes_committed,
        opened_at_utc: opened_at_utc.as_str(),
        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
        drive_element_address: bay,
        pending_batch: None,
    });
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    let mut append_gate = SessionAppendGate::default();
    let mut append_commit_diagnostics = crate::pool_write::AppendCommitDiagnostics::default();
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::AppendFinish {
                session_id: requested,
                source,
                archive_path,
                caller_object_id,
                expected_content_sha256,
                live_write_counter,
                reply,
            } => {
                if requested != session_id {
                    source.remove_completed_path();
                    let _ = reply.send(Err(Status::not_found("write session not found")));
                    continue;
                }
                if let Err(status) = append_gate.check() {
                    source.remove_completed_path();
                    let _ = reply.send(Err(status));
                    continue;
                }
                // Any accepted append call is session activity, including a
                // catalog idempotency replay; invalidate a prior timer-close.
                timer_checkpoint_waiting = None;
                let logical_size = source.size_bytes().unwrap_or(0);
                let stream_control = source.stream_control();
                let cleanup_path = match &source {
                    crate::WriteObjectSource::Path(path) => Some(path.clone()),
                    crate::WriteObjectSource::Streamed(_) => None,
                };
                let current_caller_object_id = caller_object_id.clone();
                if let Some((provisional_index, pending)) =
                    pending_batch.as_ref().and_then(|batch| {
                        batch
                            .objects
                            .iter()
                            .enumerate()
                            .find(|(_, pending)| {
                                pending.object.caller_object_id == caller_object_id
                            })
                            .map(|(index, pending)| (index, (batch.batch_id, pending)))
                    })
                {
                    let (batch_id, pending) = pending;
                    let requested_hash = source.content_sha256();
                    source.remove_completed_path();
                    match requested_hash {
                        Ok(hash) if hash == pending.object.content_sha256 => {
                            let provisional_ordinal = provisional_index as u64 + 1;
                            let record = pending
                                .object
                                .to_written_proto(batch_id, provisional_ordinal);
                            let _ = reply.send(Ok(AppendFinishOutcome {
                                record,
                                replay: true,
                            }));
                        }
                        Ok(hash) => {
                            let _ = reply.send(Err(Status::already_exists(format!(
                                "caller_object_id replay conflict inside checkpoint batch: caller_object_id={caller_object_id:?}, existing content_sha256={}, requested content_sha256={}",
                                crate::bytes_to_hex(&pending.object.content_sha256),
                                crate::bytes_to_hex(&hash),
                            ))));
                        }
                        Err(err) => {
                            let _ = reply.send(Err(status_from_pool_write_error(err)));
                        }
                    }
                    continue;
                }
                let request = WriteObjectToPoolRequest {
                    pool_id: pool_cfg.id.clone(),
                    source,
                    archive_path,
                    caller_object_id,
                    expected_content_sha256,
                    representation: crate::PoolWriteRepresentation::Plaintext,
                };
                let append_started = Instant::now();
                let result = match crate::pool_write::maybe_replay_pool_write(
                    index, &pool_cfg, &request,
                ) {
                    Ok(Some(result)) => Ok(result),
                    Ok(None) => {
                        if let Some(parity_session) = parity_session.as_mut() {
                            let sink_state = parity_session.sink_state.take().ok_or_else(|| {
                                PoolWriteError::InvalidInput(
                                    "parity sink session state is unavailable".to_string(),
                                )
                            });
                            match sink_state {
                                Ok(mut sink_state) => {
                                    let reserve_result = if pending_batch.is_none() {
                                        sink_state.reserve_checkpoint_batch_object_rows(
                                            cfg.checkpoint_max_objects,
                                        )
                                    } else {
                                        Ok(())
                                    };
                                    if let Err(err) = reserve_result {
                                        parity_session.sink_state = Some(sink_state);
                                        Err(PoolWriteError::from(err))
                                    } else {
                                        let mut raw = DriveHandleRawSink::new(drive);
                                        crate::pool_write::write_batched_parity_to_selected_tape_after_replay_check(
                                            index,
                                            &mut raw,
                                            &mut parity_session.journal,
                                            sink_state,
                                            &pool_cfg,
                                            request,
                                            selected.clone(),
                                        )
                                        .map(|(state, result)| {
                                            parity_session.sink_state = Some(state);
                                            result
                                        })
                                    }
                                }
                                Err(err) => Err(err),
                            }
                        } else {
                            let mut sink = DriveHandleSink(drive);
                            if let Some(append) = next_batched_append.clone() {
                                let append = if pending_batch.is_none() {
                                    append.with_batch_headroom_objects(cfg.checkpoint_max_objects)
                                } else {
                                    append
                                };
                                next_batched_append = Some(append.clone());
                                crate::pool_write::write_batched_to_selected_tape_after_replay_check(
                                    index,
                                    &mut sink,
                                    &pool_cfg,
                                    request,
                                    selected.clone(),
                                    live_write_counter,
                                    append,
                                )
                            } else {
                                Err(PoolWriteError::InvalidInput(
                                    "checkpoint append context is unavailable".to_string(),
                                ))
                            }
                        }
                    }
                    Err(err) => Err(err),
                };
                let append_elapsed = append_started.elapsed();
                if let Some(path) = cleanup_path {
                    let _ = std::fs::remove_file(path);
                }
                match result {
                    Ok(result) => {
                        let replay = result.is_replay();
                        append_commit_diagnostics.accumulate(result.append_commit_diagnostics());
                        let response_record = if !replay {
                            if let Some(previous) = next_batched_append.as_ref() {
                                let next_context =
                                    match crate::pool_write::next_batched_append_context(
                                        previous, &result,
                                    ) {
                                        Ok(context) => context,
                                        Err(err) => {
                                            append_gate.record_failure();
                                            let _ =
                                                reply.send(Err(status_from_pool_write_error(err)));
                                            continue;
                                        }
                                    };
                                next_batched_append = Some(next_context);
                            }
                            let new_batch = pending_batch.is_none();
                            let batch = pending_batch.get_or_insert_with(|| {
                                PendingCheckpointBatch::new(StdDuration::from_secs(
                                    cfg.checkpoint_max_age_seconds,
                                ))
                            });
                            let provisional_ordinal = batch.objects.len() as u64 + 1;
                            let written = result
                                .object
                                .to_written_proto(batch.batch_id, provisional_ordinal);
                            let object_id = result.object.object_id;
                            let batch_id = batch.batch_id;
                            batch.push(logical_size, result);
                            let timer_arm_failed = if new_batch {
                                match arm_checkpoint_timer(
                                    actor_tx.clone(),
                                    session_id,
                                    batch_id,
                                    StdDuration::from_secs(cfg.checkpoint_max_age_seconds),
                                ) {
                                    Ok(()) => false,
                                    Err(err) => {
                                        tracing::error!(
                                            session_id = %session_id,
                                            batch_id = %batch_id,
                                            error = %err,
                                            "checkpoint timer could not start; forcing an immediate barrier"
                                        );
                                        true
                                    }
                                }
                            } else {
                                false
                            };
                            if timer_arm_failed || batch.should_checkpoint(cfg) {
                                let outcome = perform_checkpoint_barrier(
                                    index,
                                    drive,
                                    &checkpoint_journal,
                                    &durable_checkpoint_records,
                                    tape_uuid,
                                    &mut checkpoint_ordinal,
                                    &mut tape_committed_object_count,
                                    batch,
                                    parity_session.as_mut(),
                                    &selected,
                                    &pool_cfg,
                                    cfg,
                                );
                                match outcome {
                                    Ok(outcome) => {
                                        if let Some(previous) = next_batched_append.as_ref() {
                                            let checkpoint_context = match crate::pool_write::batched_append_context_after_checkpoint(
                                                previous,
                                                &outcome.checkpoint_record,
                                            ) {
                                                Ok(context) => context,
                                                Err(err) => {
                                                    durable_checkpoint_records.push(outcome.checkpoint_record);
                                                    append_gate.record_failure();
                                                    pending_batch = None;
                                                    let _ = reply.send(Err(status_from_pool_write_error(err)));
                                                    continue;
                                                }
                                            };
                                            next_batched_append = Some(checkpoint_context);
                                        }
                                        durable_checkpoint_records
                                            .push(outcome.checkpoint_record.clone());
                                        objects_committed =
                                            objects_committed.saturating_add(outcome.object_count);
                                        bytes_committed =
                                            bytes_committed.saturating_add(outcome.logical_bytes);
                                        last_checkpoint_at_utc = Some(
                                            now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()),
                                        );
                                        append_commit_diagnostics.accumulate(
                                            crate::pool_write::AppendCommitDiagnostics {
                                                filemark_write_drain: outcome.filemark_drain,
                                                catalog_journal_fsync: outcome.journal_projection,
                                            },
                                        );
                                        if outcome.sealed_after_write {
                                            append_gate.record_sealed();
                                        }
                                        let committed = outcome
                                            .committed_objects
                                            .iter()
                                            .find(|record| record.object_id == object_id)
                                            .cloned()
                                            .expect("threshold checkpoint returns current object");
                                        committed_receipts.extend(outcome.committed_objects);
                                        pending_batch = None;
                                        committed
                                    }
                                    Err(failure) => {
                                        let status = if failure.journal_durable {
                                            failure.status
                                        } else {
                                            fence_failed_checkpoint_batch(
                                                index,
                                                &selected,
                                                batch,
                                                failure.status,
                                            )
                                        };
                                        append_gate.record_failure();
                                        pending_batch = None;
                                        let _ = reply.send(Err(status));
                                        continue;
                                    }
                                }
                            } else {
                                written
                            }
                        } else {
                            if !replay {
                                objects_committed = objects_committed.saturating_add(1);
                                bytes_committed = bytes_committed.saturating_add(logical_size);
                                last_checkpoint_at_utc =
                                    Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                            }
                            result.object.to_proto()
                        };
                        tracing::info!(
                            target: "remanence_write_diag",
                            phase = "drive_append_total",
                            session_id = %session_id,
                            tape_uuid = %Uuid::from_bytes(tape_uuid),
                            payload_bytes = logical_size,
                            block_size_bytes = selected.block_size,
                            replay,
                            elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                            throughput_mib_s = if replay {
                                0.0
                            } else {
                                crate::diagnostics::mib_per_s(logical_size, append_elapsed)
                            },
                            "remanence_write_diag",
                        );
                        if replay {
                            retain_replayed_committed_receipt(
                                &mut committed_receipts,
                                &response_record,
                            );
                        }
                        let _ = reply.send(Ok(AppendFinishOutcome {
                            record: response_record,
                            replay,
                        }));
                    }
                    Err(err) => {
                        let directory_ceiling = matches!(
                            &err,
                            PoolWriteError::Parity(ParityError::BootstrapPayloadTooLarge { .. })
                        ) && pending_batch.is_none();
                        if directory_ceiling {
                            let original_error = err.to_string();
                            if let Err(seal_err) = index.seal_tape(selected.tape_uuid) {
                                append_gate.record_failure();
                                let _ = reply.send(Err(status_from_state_error(seal_err)));
                                continue;
                            }
                            if let Err(evidence_err) =
                                append_tape_sealed_evidence(index, cfg, selected.tape_uuid)
                            {
                                tracing::warn!(error = %evidence_err, "failed to append directory-ceiling seal evidence");
                            }
                            let terminal_result = if let Some(parity_session) =
                                parity_session.as_mut()
                            {
                                (|| -> Result<(), PoolWriteError> {
                                    let state =
                                        parity_session.sink_state.take().ok_or_else(|| {
                                            PoolWriteError::InvalidInput(
                                                "parity state unavailable at directory ceiling"
                                                    .to_string(),
                                            )
                                        })?;
                                    let mut raw = DriveHandleRawSink::new(drive);
                                    let mut sink = ParitySink::from_session_state(
                                        &mut raw,
                                        &mut parity_session.journal,
                                        state,
                                    )?;
                                    sink.close_open_epoch(CloseReason::Finish)?;
                                    parity_session.sink_state = Some(sink.into_session_state()?);
                                    Ok(())
                                })()
                            } else {
                                (|| -> Result<(), PoolWriteError> {
                                    let terminal =
                                        crate::pool_write::build_no_parity_terminal_bootstrap(
                                            selected.tape_uuid,
                                            &durable_checkpoint_records,
                                            now_rfc3339()?,
                                        )?;
                                    drive.write_block(&terminal.block)?;
                                    drive.write_filemarks_immediate(1)?;
                                    drive.write_filemarks(0)?;
                                    Ok(())
                                })()
                            };
                            if let Err(terminal_err) = terminal_result {
                                tracing::warn!(
                                    error = %terminal_err,
                                    "terminal bootstrap append failed after directory-ceiling seal"
                                );
                            }
                            append_gate.record_sealed();
                            let _ = reply.send(Err(Status::resource_exhausted(format!(
                                "checkpoint directory ceiling reached before tape motion; selected tape sealed at its last checkpoint, reopen against the pool to roll placement: {original_error}"
                            ))));
                            continue;
                        }
                        let tape_started = stream_control
                            .as_ref()
                            .map(|control| control.tape_started())
                            .unwrap_or(true);
                        if tape_started {
                            append_gate.record_failure();
                        }
                        let original_error = err.to_string();
                        let mut status = status_from_pool_write_error(err);
                        if tape_started {
                            if let Some(batch) = pending_batch.take() {
                                status = Status::unavailable(format!(
                                    "checkpoint batch {} failed while appending {}; re-send all {} prior WRITTEN objects and the current object: {original_error}",
                                    batch.batch_id,
                                    current_caller_object_id,
                                    batch.objects.len(),
                                ));
                                status =
                                    fence_failed_checkpoint_batch(index, &selected, &batch, status);
                            }
                        }
                        tracing::info!(
                            target: "remanence_write_diag",
                            phase = "drive_append_total",
                            session_id = %session_id,
                            tape_uuid = %Uuid::from_bytes(tape_uuid),
                            payload_bytes = logical_size,
                            block_size_bytes = selected.block_size,
                            status = "error",
                            error = %status,
                            elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                            throughput_mib_s = crate::diagnostics::mib_per_s(logical_size, append_elapsed),
                            "remanence_write_diag",
                        );
                        if let Err(audit_err) =
                            append_latest_tape_io_fence_evidence(index, cfg, selected.tape_uuid)
                        {
                            tracing::warn!(
                                "failed to append tape-I/O fence evidence after write error: {audit_err}"
                            );
                        }
                        record_session_snapshot(
                            index,
                            cfg,
                            drive,
                            drive_uuid.clone(),
                            session_id,
                            tape_uuid,
                            "append-failure",
                            snapshot_misses,
                        );
                        let _ = reply.send(Err(status));
                    }
                }
            }
            DriveCommand::Checkpoint {
                session_id: requested,
                trigger,
                expected_batch_id,
                reply,
            } => {
                if requested != session_id {
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(Status::not_found("write session not found")));
                    }
                    continue;
                }
                let Some(batch) = pending_batch.as_ref() else {
                    if let Some(reply) = reply {
                        let session = session_proto(WriteSessionProtoInput {
                            session_id,
                            tape_uuid: &tape_uuid,
                            state: pb::write_session::State::WriteSessionStateCheckpointed,
                            objects_committed,
                            bytes_committed,
                            opened_at_utc: opened_at_utc.as_str(),
                            last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                            drive_element_address: bay,
                            pending_batch: None,
                        });
                        send_checkpoint_actor_reply(reply, session, &mut committed_receipts);
                    }
                    continue;
                };
                if expected_batch_id.is_some_and(|expected| expected != batch.batch_id) {
                    continue;
                }
                let timer_batch_id = batch.batch_id;
                if trigger == CheckpointTrigger::Timer
                    && Instant::now() > batch.deadline + StdDuration::from_secs(1)
                {
                    let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
                    let detail = serde_json::json!({
                        "session_id": session_id.to_string(),
                        "batch_id": batch.batch_id.to_string(),
                        "deadline_overrun_seconds": Instant::now()
                            .saturating_duration_since(batch.deadline)
                            .as_secs(),
                    })
                    .to_string();
                    let _ = index.raise_alarm(
                        condition_key.as_str(),
                        "checkpoint-barrier-overdue",
                        "warning",
                        Some(detail.as_str()),
                    );
                }
                match perform_checkpoint_barrier(
                    index,
                    drive,
                    &checkpoint_journal,
                    &durable_checkpoint_records,
                    tape_uuid,
                    &mut checkpoint_ordinal,
                    &mut tape_committed_object_count,
                    batch,
                    parity_session.as_mut(),
                    &selected,
                    &pool_cfg,
                    cfg,
                ) {
                    Ok(outcome) => {
                        if let Some(previous) = next_batched_append.as_ref() {
                            let checkpoint_context =
                                match crate::pool_write::batched_append_context_after_checkpoint(
                                    previous,
                                    &outcome.checkpoint_record,
                                ) {
                                    Ok(context) => context,
                                    Err(err) => {
                                        durable_checkpoint_records.push(outcome.checkpoint_record);
                                        append_gate.record_failure();
                                        pending_batch = None;
                                        let status = status_from_pool_write_error(err);
                                        if let Some(reply) = reply {
                                            let _ = reply.send(Err(status));
                                        } else {
                                            tracing::error!(session_id = %session_id, error = %status, "checkpoint committed but next append context failed");
                                        }
                                        continue;
                                    }
                                };
                            next_batched_append = Some(checkpoint_context);
                        }
                        durable_checkpoint_records.push(outcome.checkpoint_record.clone());
                        objects_committed = objects_committed.saturating_add(outcome.object_count);
                        bytes_committed = bytes_committed.saturating_add(outcome.logical_bytes);
                        last_checkpoint_at_utc =
                            Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                        append_commit_diagnostics.accumulate(
                            crate::pool_write::AppendCommitDiagnostics {
                                filemark_write_drain: outcome.filemark_drain,
                                catalog_journal_fsync: outcome.journal_projection,
                            },
                        );
                        if outcome.sealed_after_write {
                            append_gate.record_sealed();
                        }
                        pending_batch = None;
                        let condition_key = format!("checkpoint-barrier-overdue:{session_id}");
                        let _ = index.clear_alarm(condition_key.as_str());
                        if trigger == CheckpointTrigger::Timer {
                            timer_checkpoint_waiting = Some(timer_batch_id);
                            if let Err(err) = arm_checkpoint_idle_close(
                                actor_tx.clone(),
                                session_id,
                                timer_batch_id,
                                StdDuration::from_secs(cfg.session_idle_seconds),
                            ) {
                                let condition_key =
                                    format!("checkpoint-barrier-overdue:{session_id}");
                                let detail = serde_json::json!({
                                    "session_id": session_id.to_string(),
                                    "batch_id": timer_batch_id.to_string(),
                                    "error": format!("idle close timer spawn failed: {err}"),
                                })
                                .to_string();
                                let _ = index.raise_alarm(
                                    condition_key.as_str(),
                                    "checkpoint-barrier-overdue",
                                    "error",
                                    Some(detail.as_str()),
                                );
                                tracing::error!(
                                    session_id = %session_id,
                                    batch_id = %timer_batch_id,
                                    error = %err,
                                    "checkpoint idle-close timer could not start"
                                );
                            }
                        }
                        committed_receipts.extend(outcome.committed_objects);
                        let session = session_proto(WriteSessionProtoInput {
                            session_id,
                            tape_uuid: &tape_uuid,
                            state: pb::write_session::State::WriteSessionStateCheckpointed,
                            objects_committed,
                            bytes_committed,
                            opened_at_utc: opened_at_utc.as_str(),
                            last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                            drive_element_address: bay,
                            pending_batch: None,
                        });
                        if let Some(reply) = reply {
                            send_checkpoint_actor_reply(reply, session, &mut committed_receipts);
                        }
                    }
                    Err(failure) => {
                        let status = if failure.journal_durable {
                            failure.status
                        } else {
                            fence_failed_checkpoint_batch(index, &selected, batch, failure.status)
                        };
                        append_gate.record_failure();
                        pending_batch = None;
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(status));
                        } else {
                            tracing::error!(
                                session_id = %session_id,
                                batch_id = %timer_batch_id,
                                error = %status,
                                "timer-fired checkpoint barrier failed"
                            );
                        }
                    }
                }
            }
            DriveCommand::TimerIdleClose {
                session_id: requested,
                checkpoint_batch_id,
            } => {
                if requested != session_id
                    || timer_checkpoint_waiting != Some(checkpoint_batch_id)
                    || pending_batch.is_some()
                {
                    continue;
                }
                let result = close_write_actor(CloseWriteActorInput {
                    index,
                    cfg,
                    drive,
                    drive_uuid: &drive_uuid,
                    drive_serial: &drive_serial,
                    snapshot_misses,
                    session_id,
                    tape_uuid,
                    library_serial: library_serial.as_str(),
                    bay,
                    objects_committed,
                    bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                    state: pb::write_session::State::WriteSessionStateClosed,
                    append_commit_diagnostics,
                    checkpointed_objects: &committed_receipts,
                });
                if let Err(err) = result {
                    tracing::error!(session_id = %session_id, error = %err, "idle checkpoint close failed");
                    continue;
                }
                if let Err(err) = park_timer_closed_session(cfg, session_id) {
                    tracing::error!(session_id = %session_id, error = %err, "timer-closed session could not enter idle eviction");
                }
                break;
            }
            DriveCommand::Close {
                session_id: requested,
                reply,
            } => {
                if requested != session_id {
                    let _ = reply.send(Err(Status::not_found("write session not found")));
                    continue;
                }
                if let Some(batch) = pending_batch.as_ref() {
                    match perform_checkpoint_barrier(
                        index,
                        drive,
                        &checkpoint_journal,
                        &durable_checkpoint_records,
                        tape_uuid,
                        &mut checkpoint_ordinal,
                        &mut tape_committed_object_count,
                        batch,
                        parity_session.as_mut(),
                        &selected,
                        &pool_cfg,
                        cfg,
                    ) {
                        Ok(outcome) => {
                            if let Some(previous) = next_batched_append.as_ref() {
                                let checkpoint_context =
                                    match crate::pool_write::batched_append_context_after_checkpoint(
                                        previous,
                                        &outcome.checkpoint_record,
                                    ) {
                                        Ok(context) => context,
                                        Err(err) => {
                                            durable_checkpoint_records
                                                .push(outcome.checkpoint_record);
                                            append_gate.record_failure();
                                            pending_batch = None;
                                            let _ =
                                                reply.send(Err(status_from_pool_write_error(err)));
                                            continue;
                                        }
                                    };
                                next_batched_append = Some(checkpoint_context);
                            }
                            durable_checkpoint_records.push(outcome.checkpoint_record.clone());
                            objects_committed =
                                objects_committed.saturating_add(outcome.object_count);
                            bytes_committed = bytes_committed.saturating_add(outcome.logical_bytes);
                            last_checkpoint_at_utc =
                                Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                            append_commit_diagnostics.accumulate(
                                crate::pool_write::AppendCommitDiagnostics {
                                    filemark_write_drain: outcome.filemark_drain,
                                    catalog_journal_fsync: outcome.journal_projection,
                                },
                            );
                            if outcome.sealed_after_write {
                                append_gate.record_sealed();
                            }
                            committed_receipts.extend(outcome.committed_objects);
                            pending_batch = None;
                        }
                        Err(failure) => {
                            let status = if failure.journal_durable {
                                failure.status
                            } else {
                                fence_failed_checkpoint_batch(
                                    index,
                                    &selected,
                                    batch,
                                    failure.status,
                                )
                            };
                            append_gate.record_failure();
                            pending_batch = None;
                            let _ = reply.send(Err(status));
                            continue;
                        }
                    }
                }
                let result = close_write_actor(CloseWriteActorInput {
                    index,
                    cfg,
                    drive,
                    drive_uuid: &drive_uuid,
                    drive_serial: &drive_serial,
                    snapshot_misses,
                    session_id,
                    tape_uuid,
                    library_serial: library_serial.as_str(),
                    bay,
                    objects_committed,
                    bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                    state: pb::write_session::State::WriteSessionStateClosed,
                    append_commit_diagnostics,
                    checkpointed_objects: &committed_receipts,
                });
                match result {
                    Ok(result) => {
                        let _ = reply.send(Ok(result));
                        break;
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                }
            }
            DriveCommand::Abort {
                session_id: requested,
                reply,
            } => {
                if requested != session_id {
                    let _ = reply.send(Err(Status::not_found("write session not found")));
                    continue;
                }
                let result = close_write_actor(CloseWriteActorInput {
                    index,
                    cfg,
                    drive,
                    drive_uuid: &drive_uuid,
                    drive_serial: &drive_serial,
                    snapshot_misses,
                    session_id,
                    tape_uuid,
                    library_serial: library_serial.as_str(),
                    bay,
                    objects_committed,
                    bytes_committed,
                    opened_at_utc: opened_at_utc.as_str(),
                    last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                    state: pb::write_session::State::WriteSessionStateAborted,
                    append_commit_diagnostics,
                    checkpointed_objects: &committed_receipts,
                });
                match result {
                    Ok(result) => {
                        let _ = reply.send(Ok(result));
                        break;
                    }
                    Err(err) => {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                }
            }
            DriveCommand::Get {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    Ok(session_proto(WriteSessionProtoInput {
                        session_id,
                        tape_uuid: &tape_uuid,
                        state: if pending_batch.is_none() && last_checkpoint_at_utc.is_some() {
                            pb::write_session::State::WriteSessionStateCheckpointed
                        } else {
                            pb::write_session::State::WriteSessionStateOpen
                        },
                        objects_committed,
                        bytes_committed,
                        opened_at_utc: opened_at_utc.as_str(),
                        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                        drive_element_address: bay,
                        pending_batch: pending_batch.as_ref(),
                    }))
                } else {
                    Err(Status::not_found("write session not found"))
                };
                let _ = reply.send(status);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "write session already active",
                )));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "write session already active",
                )));
            }
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed("write session already active", &[("phase", "admission")]);
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "write session already active",
                )));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "write session already active",
                )));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "write session already active",
                )));
            }
            DriveCommand::ReadFile { chunk_tx, .. }
            | DriveCommand::ReadObjectRange { chunk_tx, .. } => {
                let _ = chunk_tx.blocking_send(Err(Status::failed_precondition(
                    "active session is a write session",
                )));
            }
            DriveCommand::CloseRead { reply, .. } | DriveCommand::GetRead { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a write session",
                )));
            }
        }
    }
}

fn append_latest_tape_io_fence_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    let fence = index
        .list_active_tape_io_fences()
        .map_err(crate::status_from_state_error)?
        .into_iter()
        .find(|fence| fence.tape_uuid.as_slice() == tape_uuid);
    let Some(fence) = fence else {
        return Ok(());
    };
    append_tape_io_fence_evidence(index, cfg, &fence, AuditEvent::TapeIoFenceRaised)
}

fn append_tape_sealed_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
) -> Result<(), Status> {
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::TapeSealed,
            subject_kind: "tape",
            subject_id: Some(crate::bytes_to_hex(tape_uuid.as_slice())),
            detail: BTreeMap::new(),
        },
    )?;
    Ok(())
}

fn append_tape_io_fence_evidence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    fence: &TapeIoFenceRecord,
    event: AuditEvent,
) -> Result<(), Status> {
    let mut detail = BTreeMap::from([
        (
            "tape_uuid".to_string(),
            CborValue::Bytes(fence.tape_uuid.clone()),
        ),
        (
            "quarantine_id".to_string(),
            CborValue::Text(fence.quarantine_id.clone()),
        ),
        ("reason".to_string(), CborValue::Text(fence.reason.clone())),
    ]);
    insert_optional_audit_text(&mut detail, "barcode", fence.barcode.as_ref());
    insert_optional_audit_text(&mut detail, "evidence_json", fence.evidence_json.as_ref());
    insert_optional_audit_text(&mut detail, "release_ack", fence.release_ack.as_ref());
    crate::append_and_project_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::ProjectedAuditInput {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event,
            subject_kind: "tape_io_fence",
            subject_id: Some(fence.quarantine_id.clone()),
            detail,
        },
    )?;
    Ok(())
}

fn prepare_drive_for_write(
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    block_size: u32,
    session_id: Uuid,
) -> Result<(), Status> {
    let prepare_started = Instant::now();
    drive
        .reverify_invalidated_state()
        .map_err(|err| Status::failed_precondition(format!("reverify drive state: {err}")))?;
    let rewind_verify_started = Instant::now();
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before write verify: {err}")))?;
    let rewind_verify_elapsed = rewind_verify_started.elapsed();
    let verify_started = Instant::now();
    {
        let mut source = DriveHandleSource(drive);
        verify_tape_identity(&mut source, tape_uuid)
            .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    }
    let verify_elapsed = verify_started.elapsed();
    let rewind_write_started = Instant::now();
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before write: {err}")))?;
    let rewind_write_elapsed = rewind_write_started.elapsed();
    let read_config_started = Instant::now();
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config before write: {err}")))?;
    let read_config_elapsed = read_config_started.elapsed();
    let target_cfg = fixed_no_compression_config(current_cfg, block_size);
    let write_config_started = Instant::now();
    drive
        .write_config(target_cfg)
        .map_err(|err| Status::internal(format!("set fixed-block config: {err}")))?;
    let staging_ring_buffers = drive.staging_ring_buffers();
    let effective_batch_blocks = drive.requested_write_batch_blocks().min(
        drive
            .sg_reserved_size_bytes()
            .checked_div(block_size.max(1))
            .unwrap_or(1)
            .max(1),
    );
    let effective_ring_bytes = u64::from(staging_ring_buffers)
        .saturating_mul(u64::from(effective_batch_blocks))
        .saturating_mul(u64::from(block_size));
    tracing::info!(
        target: "remanence_write_diag",
        phase = "drive_prepare",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(*tape_uuid),
        selected_block_size_bytes = block_size,
        prior_block_size = ?current_cfg.block_size,
        prior_compression = current_cfg.compression,
        target_block_size = ?target_cfg.block_size,
        target_compression = target_cfg.compression,
        staging_ring_buffers,
        effective_batch_blocks,
        effective_ring_bytes,
        rewind_verify_ms = crate::diagnostics::duration_ms(rewind_verify_elapsed),
        verify_bootstrap_ms = crate::diagnostics::duration_ms(verify_elapsed),
        rewind_write_ms = crate::diagnostics::duration_ms(rewind_write_elapsed),
        read_config_ms = crate::diagnostics::duration_ms(read_config_elapsed),
        write_config_ms = crate::diagnostics::duration_ms(write_config_started.elapsed()),
        elapsed_ms = crate::diagnostics::duration_ms(prepare_started.elapsed()),
        "remanence_write_diag",
    );
    Ok(())
}

fn fixed_no_compression_config(current_cfg: TapeConfig, block_size: u32) -> TapeConfig {
    TapeConfig {
        block_size: BlockSize::Fixed {
            size_bytes: block_size,
        },
        compression: false,
        max_block_size_bytes: current_cfg.max_block_size_bytes,
        write_protected: current_cfg.write_protected,
        worm: current_cfg.worm,
    }
}

fn prepare_drive_for_read(
    index: &CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    session_id: Uuid,
) -> Result<(), Status> {
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::failed_precondition("tape catalog row is missing"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::failed_precondition("tape block_size is missing"))?;
    let block_size = u32::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;
    let started = Instant::now();
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config: {err}")))?;
    let target_cfg = fixed_no_compression_config(current_cfg, block_size);
    drive
        .write_config(target_cfg)
        .map_err(|err| Status::internal(format!("set fixed read config: {err}")))?;
    let verified = drive
        .read_config()
        .map_err(|err| Status::internal(format!("verify fixed read config: {err}")))?;
    if verified.block_size != target_cfg.block_size {
        return Err(Status::failed_precondition(format!(
            "fixed read mode verification mismatch: expected {:?}, got {:?}",
            target_cfg.block_size, verified.block_size
        )));
    }
    tracing::info!(
        target: "remanence_read_diag",
        phase = "drive_prepare_read",
        session_id = %session_id,
        tape_uuid = %Uuid::from_bytes(*tape_uuid),
        status = "ok",
        selected_block_size_bytes = block_size,
        prior_block_size = ?current_cfg.block_size,
        target_block_size = ?target_cfg.block_size,
        elapsed_ms = crate::diagnostics::duration_ms(started.elapsed()),
        "remanence_read_diag",
    );
    Ok(())
}

fn handle_drive_open_read(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    snapshot_misses: &mut u32,
    request: OpenReadActorRequest,
) {
    let OpenReadActorRequest {
        tape_uuid,
        needs_drive_load,
        library_serial,
        barcode,
        source_slot,
        drive_uuid,
        drive_serial,
        resume_target,
        daemon_epoch,
        reply,
    } = request;

    if let Err(status) = session_open_short_probe_or_load(
        index,
        drive,
        SessionOpenReadinessContext {
            action: "open read session",
            bay,
            library_serial: library_serial.as_str(),
            barcode: barcode.as_deref(),
            source_slot,
            drive_serial: drive_serial.as_deref(),
            needs_drive_load,
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if let Err(status) = session_open_reject_tape_io_fences(
        index,
        &tape_uuid,
        barcode.as_deref(),
        "open read session",
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    if resume_target
        .as_ref()
        .is_some_and(|target| target.tape_uuid != tape_uuid)
    {
        let _ = reply.send(Err(Status::invalid_argument(
            "resume target tape UUID does not match mounted read target",
        )));
        return;
    }
    let session_id = Uuid::new_v4();
    let position_proof = match resume_target.as_ref() {
        Some(target) => {
            if let Err(status) = prepare_drive_for_read(index, drive, &tape_uuid, session_id) {
                let _ = reply.send(Err(status));
                return;
            }
            match position_read_resume(index, drive, target) {
                Ok(proof) => Some(proof),
                Err(status) => {
                    let _ = reply.send(Err(status));
                    return;
                }
            }
        }
        None => {
            if let Err(status) = verify_loaded_tape_identity(drive, &tape_uuid) {
                let _ = reply.send(Err(status));
                return;
            }
            if let Err(status) = prepare_drive_for_read(index, drive, &tape_uuid, session_id) {
                let _ = reply.send(Err(status));
                return;
            }
            None
        }
    };
    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    if let Err(status) = record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id,
            session_kind: "read",
            event: AuditEvent::SessionOpened,
            tape_uuid: Some(tape_uuid),
            library_serial: Some(library_serial.clone()),
            drive_bay: Some(bay),
            drive_uuid: drive_uuid.clone(),
            drive_serial: drive_serial.clone(),
        },
    ) {
        let _ = reply.send(Err(status));
        return;
    }
    let open_reply = read_session_proto(
        session_id,
        &tape_uuid,
        pb::read_session::State::ReadSessionStateOpen,
        opened_at_utc.as_str(),
        bay,
        position_proof,
        daemon_epoch,
    );
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::ReadFile {
                session_id: requested,
                object_id,
                file_id,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                if requested != session_id {
                    let _ =
                        chunk_tx.blocking_send(Err(Status::not_found("read session not found")));
                    continue;
                }
                let result = if file_id.is_empty() {
                    stream_one_object(
                        index,
                        drive,
                        cfg,
                        session_id,
                        &tape_uuid,
                        object_id.as_str(),
                        stream_chunk_bytes,
                        chunk_tx.clone(),
                    )
                } else {
                    String::from_utf8(file_id)
                        .map_err(|err| {
                            Status::invalid_argument(format!("file_id is not utf-8: {err}"))
                        })
                        .and_then(|file_id| {
                            stream_one_file_range(
                                index,
                                drive,
                                cfg,
                                session_id,
                                &tape_uuid,
                                object_id.as_str(),
                                file_id.as_str(),
                                0,
                                0,
                                stream_chunk_bytes,
                                chunk_tx.clone(),
                            )
                        })
                };
                if let Err(status) = result {
                    record_session_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        "read-failure",
                        snapshot_misses,
                    );
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::ReadObjectRange {
                session_id: requested,
                object_id,
                file_id,
                start_byte,
                end_byte,
                stream_chunk_bytes,
                chunk_tx,
            } => {
                if requested != session_id {
                    let _ =
                        chunk_tx.blocking_send(Err(Status::not_found("read session not found")));
                    continue;
                }
                if let Err(status) = stream_one_file_range(
                    index,
                    drive,
                    cfg,
                    session_id,
                    &tape_uuid,
                    object_id.as_str(),
                    file_id.as_str(),
                    start_byte,
                    end_byte,
                    stream_chunk_bytes,
                    chunk_tx.clone(),
                ) {
                    record_session_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        "read-failure",
                        snapshot_misses,
                    );
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::CloseRead {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    record_session_close_snapshot(
                        index,
                        cfg,
                        drive,
                        drive_uuid.clone(),
                        session_id,
                        tape_uuid,
                        snapshot_misses,
                    );
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateClosed,
                        opened_at_utc.as_str(),
                        bay,
                        position_proof,
                        daemon_epoch,
                    ))
                } else {
                    Err(Status::not_found("read session not found"))
                };
                if status.is_ok() {
                    if let Err(err) = record_session_event(
                        index,
                        cfg,
                        SessionAuditInput {
                            session_id,
                            session_kind: "read",
                            event: AuditEvent::SessionClosed,
                            tape_uuid: Some(tape_uuid),
                            library_serial: Some(library_serial.clone()),
                            drive_bay: Some(bay),
                            drive_uuid: drive_uuid.clone(),
                            drive_serial: drive_serial.clone(),
                        },
                    ) {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                }
                let _ = reply.send(status);
                if requested == session_id {
                    break;
                }
            }
            DriveCommand::GetRead {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateOpen,
                        opened_at_utc.as_str(),
                        bay,
                        position_proof,
                        daemon_epoch,
                    ))
                } else {
                    Err(Status::not_found("read session not found"))
                };
                let _ = reply.send(status);
            }
            DriveCommand::OpenWrite { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::OpenRead { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::WaitReady { handle, .. } => {
                handle.publish_failed("read session already active", &[("phase", "admission")]);
            }
            DriveCommand::Unload { reply } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::PollHealth { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::Heartbeat { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "read session already active",
                )));
            }
            DriveCommand::AppendFinish { reply, source, .. } => {
                source.remove_completed_path();
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Checkpoint { reply, .. } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(Status::failed_precondition(
                        "active session is a read session",
                    )));
                }
            }
            DriveCommand::TimerIdleClose { .. } => {}
            DriveCommand::Close { reply, .. } | DriveCommand::Abort { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
        }
    }
}

fn session_open_reject_tape_io_fences(
    index: &CatalogIndex,
    tape_uuid: &TapeUuid,
    barcode: Option<&str>,
    action: &str,
) -> Result<(), Status> {
    let conflicts = index
        .tape_io_admission_conflicts(tape_uuid, barcode)
        .map_err(status_from_state_error)?;
    let Some(first) = conflicts.first() else {
        return Ok(());
    };
    Err(Status::failed_precondition(format!(
        "{action} blocked by active tape-I/O fence {} tape_uuid={} barcode={} reason={}; release via `rem tape quarantine release {}` before retrying",
        first.quarantine_id,
        Uuid::from_bytes(*tape_uuid),
        barcode.unwrap_or("(unknown)"),
        first.reason,
        first.quarantine_id
    )))
}

fn handle_robotics(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: String,
    action: RoboticsAction,
    handle: crate::operations::OperationHandle,
) {
    if handle.is_cancelled() {
        cancel_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            "cancelled before dispatch",
        );
        return;
    }
    publish_running(&handle, &[("phase", "open")]);
    if let Err(err) = record_library_event(
        index,
        cfg,
        &handle,
        &library_serial,
        AuditEvent::OperationStarted,
        robotics_detail(&action),
    ) {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let lib = match cfg.report.library(&library_serial) {
        Some(lib) => lib,
        None => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "open")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("open library: {err}"),
                &[("phase", "open")],
            );
            return;
        }
    };

    publish_running(&handle, &[("phase", "refresh")]);
    if let Err(err) = library.refresh() {
        fail_library_operation(
            index,
            cfg,
            &handle,
            &library_serial,
            &format!("refresh inventory: {err}"),
            &[("phase", "refresh")],
        );
        return;
    }

    publish_running(&handle, &[("phase", "execute")]);
    let action_result = match &action {
        RoboticsAction::Refresh => Ok(()),
        RoboticsAction::Move { src, dst } => library
            .move_medium(*src, *dst, &cfg.policy)
            .map_err(|err| err.to_string()),
        RoboticsAction::Load {
            slot,
            bay,
            wait_ready,
        } => run_load_sequence(index, cfg, &handle, &mut library, *slot, *bay, *wait_ready),
        RoboticsAction::Unload { bay, destination } => library
            .unload(*bay, *destination, &cfg.policy)
            .map_err(|err| err.to_string()),
        RoboticsAction::Clean {
            drive_uuid,
            trigger,
        } => run_cleaning_sequence(
            index,
            cfg,
            &handle,
            &mut library,
            drive_uuid.as_slice(),
            trigger.as_str(),
        )
        .map_err(|err| err.to_string()),
    };

    let observe_result = observe_refreshed_library(index, cfg, library.library())
        .map_err(|err| err.message().to_string());
    publish_library_snapshot(&cfg.library_snapshot, library.library().clone());

    match (action_result, observe_result) {
        (Ok(()), Ok(())) => {
            if let Err(err) = record_library_event(
                index,
                cfg,
                &handle,
                &library_serial,
                AuditEvent::OperationFinished,
                BTreeMap::new(),
            ) {
                fail_library_operation(
                    index,
                    cfg,
                    &handle,
                    &library_serial,
                    &format!("record operation finish audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(pb::OperationState::Succeeded, &[("phase", "done")]);
        }
        (Ok(()), Err(message)) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                &format!("observe refreshed drive catalog: {message}"),
                &[("phase", "catalog")],
            );
        }
        (Err(message), _) => {
            fail_library_operation(
                index,
                cfg,
                &handle,
                &library_serial,
                message.as_str(),
                &[("phase", "execute")],
            );
        }
    }
}

fn run_load_sequence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    slot: u16,
    bay: u16,
    wait_ready: bool,
) -> Result<(), String> {
    if !wait_ready {
        return library
            .load(slot, bay, &cfg.policy)
            .map_err(|error| error.to_string());
    }

    let barcode = library
        .library()
        .slots
        .iter()
        .find(|candidate| candidate.element_address == slot)
        .and_then(|candidate| candidate.cartridge.clone());
    let drive_bay = library
        .library()
        .drive_bays
        .iter()
        .find(|candidate| candidate.element_address == bay);
    let drive_serial = drive_bay
        .and_then(|candidate| candidate.installed.as_ref())
        .map(|drive| drive.serial.clone());
    let drive_sg = drive_bay
        .and_then(|candidate| candidate.installed.as_ref())
        .and_then(|drive| drive.sg_path.as_ref())
        .map(|path| path.display().to_string());
    let family = session_open_media_family(barcode.as_deref());
    let retryable_load_completion = match library.load(slot, bay, &cfg.policy) {
        Ok(()) => None,
        Err(error) => match retryable_readiness_from_load_error(&error, family) {
            Some(readiness) => Some(readiness),
            None => return Err(error.to_string()),
        },
    };

    let operation_id = handle.op_id_uuid();
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: library.library().serial.clone(),
            changer_sg: Some(library.library().changer_sg.display().to_string()),
            drive_element: bay,
            drive_sg,
            drive_serial,
            barcode,
            source_slot: Some(slot),
            media_generation: library
                .library()
                .drive_bays
                .iter()
                .find(|candidate| candidate.element_address == bay)
                .and_then(|candidate| candidate.loaded_tape.as_deref())
                .and_then(crate::lto_generation_from_voltag)
                .map(|generation| generation.generation_number()),
            phase: "load_drive_readiness".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: OffsetDateTime::now_utc()
                .checked_add(Duration::seconds(9_000))
                .and_then(|deadline| deadline.format(&Rfc3339).ok()),
            evidence_path: None,
        })
        .map_err(|error| format!("record load media-readiness operation: {error}"))?;
    if let Some(readiness) = retryable_load_completion.as_ref() {
        record_session_open_readiness_poll_transition(
            index,
            operation_id,
            "load_drive_completion",
            readiness,
            false,
        )
        .map_err(|error| format!("record LOAD readiness transition: {error}"))?;
    }

    handle.publish_state(
        pb::OperationState::Running,
        &[("phase", "readiness_poll"), ("state", "starting")],
    );
    let mut drive = library
        .open_drive(bay, &cfg.policy)
        .map_err(|error| format!("open drive 0x{bay:04x} for readiness wait: {error}"))?;
    let poll = poll_drive_media_readiness(
        index,
        &mut drive,
        operation_id,
        family,
        MediaReadinessWaitOptions {
            wait: true,
            timeout: LOAD_READY_TIMEOUT,
            poll_interval: LOAD_READY_POLL_INTERVAL,
        },
        handle,
        "load_drive_readiness",
    )?;
    if !poll.readiness.is_ready() {
        let state = if poll.timed_out {
            "timeout_unknown"
        } else {
            session_open_readiness_state(&poll.readiness)
        };
        return Err(format!(
            "load drive 0x{bay:04x} did not reach READY (state={state}): {}",
            session_open_readiness_summary(&poll.readiness)
        ));
    }
    library
        .refresh()
        .map_err(|error| format!("refresh inventory after READY: {error}"))
}

fn retryable_readiness_from_load_error(
    error: &LoadError,
    family: MediaFamily,
) -> Option<MediaReadiness> {
    let LoadError::DriveLoad(DriveOpError::ScsiError(error)) = error else {
        return None;
    };
    let readiness = classify_media_readiness_error_ref(error, family);
    readiness.is_retryable_wait().then_some(readiness)
}

fn observe_refreshed_library(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &remanence_library::Library,
) -> Result<(), Status> {
    crate::observe_drive_catalog_from_libraries(
        index,
        std::iter::once(library),
        &cfg.managed_library_serials,
    )
}

fn library_snapshot_persist_alarm_key(library_serial: &str) -> String {
    format!("snapshot-persist-failing:library:{library_serial}")
}

fn record_library_observation_failure(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &remanence_library::Library,
    error: &str,
) {
    tracing::warn!(
        "failed to observe refreshed drive catalog library_serial={} error={}",
        library.serial,
        error
    );
    let condition_key = library_snapshot_persist_alarm_key(library.serial.as_str());
    let detail = format!(
        "{{\"library_serial\":\"{}\",\"error\":\"{}\"}}",
        library.serial.replace('"', "'"),
        error.replace('"', "'")
    );
    if let Err(err) = raise_alarm_with_evidence(
        index,
        cfg,
        condition_key.as_str(),
        "snapshot-persist-failing",
        "warning",
        Some(detail.as_str()),
    ) {
        tracing::warn!(
            "failed to raise library snapshot alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

fn clear_library_snapshot_persist_alarm(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library_serial: &str,
) {
    let condition_key = library_snapshot_persist_alarm_key(library_serial);
    if let Err(err) = clear_alarm_with_evidence(index, cfg, condition_key.as_str()) {
        tracing::warn!(
            "failed to clear library snapshot alarm condition_key={} error={}",
            condition_key,
            err
        );
    }
}

fn publish_library_snapshot(
    cell: &RwLock<Arc<crate::LibrarySnapshot>>,
    updated: remanence_library::Library,
) {
    let mut report = cell
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .report
        .clone();
    match report
        .libraries
        .iter_mut()
        .find(|library| library.serial == updated.serial)
    {
        Some(slot) => *slot = updated,
        None => report.libraries.push(updated),
    }
    let snapshot = Arc::new(crate::LibrarySnapshot {
        report,
        captured_at: OffsetDateTime::now_utc(),
    });
    *cell.write().unwrap_or_else(|err| err.into_inner()) = snapshot;
}

fn record_library_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    detail.insert(
        "library_serial".to_string(),
        CborValue::Text(library_serial.to_string()),
    );
    crate::append_operation_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::OperationAuditInput {
            actor: AuditActor::System,
            operation_id: handle.op_id_uuid(),
            operation_kind: handle.operation_kind(),
            event,
            subject_kind: "library",
            subject_id: Some(library_serial.to_string()),
            idempotency_key: None,
            detail,
        },
    )
}

fn fail_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    tracing::error!(
        target: "remanence_library_operation",
        operation_id = %handle.op_id_uuid(),
        operation_kind = %handle.operation_kind(),
        library_serial,
        error_summary,
        "library operation failed"
    );
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::OperationFailed,
        detail,
    ) {
        let audit_error = format!("{error_summary}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), progress);
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

fn cancel_library_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library_serial: &str,
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial,
        AuditEvent::CancelledBeforeDispatch,
        detail,
    ) {
        let audit_error = format!("{detail_message}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), &[("phase", "audit")]);
    } else {
        handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", detail_message)],
        );
    }
}

fn robotics_detail(action: &RoboticsAction) -> BTreeMap<String, CborValue> {
    let mut detail = BTreeMap::new();
    match action {
        RoboticsAction::Refresh => {}
        RoboticsAction::Move { src, dst } => {
            detail.insert(
                "src".to_string(),
                CborValue::Integer(u64::from(*src).into()),
            );
            detail.insert(
                "dst".to_string(),
                CborValue::Integer(u64::from(*dst).into()),
            );
        }
        RoboticsAction::Load {
            slot,
            bay,
            wait_ready,
        } => {
            detail.insert(
                "slot".to_string(),
                CborValue::Integer(u64::from(*slot).into()),
            );
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
            detail.insert("wait_ready".to_string(), CborValue::Bool(*wait_ready));
        }
        RoboticsAction::Unload { bay, destination } => {
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
            if let Some(dst) = destination {
                detail.insert(
                    "destination".to_string(),
                    CborValue::Integer(u64::from(*dst).into()),
                );
            }
        }
        RoboticsAction::Clean {
            drive_uuid,
            trigger,
        } => {
            detail.insert(
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.clone()),
            );
            detail.insert("trigger".to_string(), CborValue::Text(trigger.clone()));
            detail.insert(
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            );
        }
    }
    detail
}

fn run_cleaning_sequence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    drive_uuid: &[u8],
    trigger: &str,
) -> Result<(), Status> {
    let clean_cfg = &cfg.cleaning;
    if !clean_cfg.auto {
        return Err(Status::failed_precondition(
            "automatic cleaning is disabled",
        ));
    }
    let drive = index
        .get_drive_by_uuid(drive_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("drive not found"))?;
    if drive.managed != "rem" {
        return Err(Status::failed_precondition(
            "cleaning is only available for managed drives",
        ));
    }
    if drive.state != "active" {
        return Err(Status::failed_precondition("cannot clean a retired drive"));
    }
    if !drive.actionable {
        return Err(Status::failed_precondition(
            "drive is non-actionable because its serial identity is blank or collided",
        ));
    }
    let Some(library_serial) = drive.last_library_serial.clone() else {
        return Err(Status::failed_precondition(
            "drive has no current library assignment",
        ));
    };
    let drive_bay = drive
        .last_element_address
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| Status::failed_precondition("drive has no current bay"))?;
    if trigger == "periodic" && !cleaning_drive_is_idle(library, drive_bay)? {
        return Ok(());
    }
    // Join-check FIRST: a trigger while a run is already active is a join
    // (no-op), never a frequency refusal (diff-gate re-check finding).
    if let Some(active_run) = index
        .get_active_clean_run_by_drive(drive_uuid)
        .map_err(status_from_state_error)?
    {
        if active_run.phase != "done"
            && active_run.phase != "failed"
            && active_run.phase != "needs-operator"
        {
            return Ok(());
        }
    }
    let min_interval = parse_duration_or(&clean_cfg.min_interval, Duration::hours(12));
    let weekly_cap = clean_cfg.weekly_cap as usize;
    if cleaning_too_soon(index, drive_uuid, min_interval, weekly_cap)? {
        let detail = format!(
            "{{\"drive_uuid\":\"{}\",\"recovery_step\":\"frequency-cap\"}}",
            json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        );
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!(
                "drive-cleaning-abnormal-frequency:{}",
                crate::bytes_to_hex(drive_uuid)
            )
            .as_str(),
            "drive-cleaning-abnormal-frequency",
            "warning",
            Some(detail.as_str()),
        );
        return Err(Status::failed_precondition(
            "drive-cleaning-abnormal-frequency",
        ));
    }
    if drive.fenced {
        return Err(Status::failed_precondition("drive is already fenced"));
    }
    let run = index
        .begin_clean_run(drive_uuid, library_serial.as_str(), trigger, None)
        .map_err(status_from_state_error)?;
    let fence_detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"recovery_step\":\"fence\"}}",
        json_escape_text(&run.run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
    );
    if let Err(err) = raise_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run.run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(fence_detail.as_str()),
    ) {
        let _ =
            index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(fence_detail.as_str()));
        return Err(err);
    }
    index
        .set_drive_fenced(drive_uuid, true)
        .map_err(status_from_state_error)?;
    if let Err(err) = record_library_event(
        index,
        cfg,
        handle,
        library_serial.as_str(),
        AuditEvent::DriveFenced,
        BTreeMap::from([
            (
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.to_vec()),
            ),
            (
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            ),
        ]),
    ) {
        tracing::warn!("failed to append cleaning fence audit: {err}");
    }
    let tape_prefixes = clean_cfg
        .voltag_prefixes
        .iter()
        .map(|prefix| prefix.trim())
        .filter(|prefix| !prefix.is_empty())
        .collect::<Vec<_>>();
    let mut prefix_matches = 0_usize;
    let mut rejected_carts = Vec::new();
    let mut eligible_carts = Vec::new();
    for slot in &library.library().slots {
        let Some(voltag) = slot.cartridge.as_ref() else {
            continue;
        };
        if !tape_prefixes
            .iter()
            .any(|prefix| voltag.starts_with(prefix))
        {
            continue;
        }
        prefix_matches = prefix_matches.saturating_add(1);
        let tape = match index.ensure_cleaning_cartridge(voltag) {
            Ok(tape) => tape,
            Err(err) => {
                rejected_carts.push(format!(
                    "slot=0x{:04x} voltag={} registration={err}",
                    slot.element_address, voltag
                ));
                continue;
            }
        };
        let cleaning_state = match index.get_tape_cleaning_state(tape.tape_uuid.as_slice()) {
            Ok(state) => state.flatten(),
            Err(err) => {
                rejected_carts.push(format!(
                    "slot=0x{:04x} voltag={} state-query={err}",
                    slot.element_address, voltag
                ));
                continue;
            }
        };
        match cleaning_state.as_deref() {
            None | Some("unverified") | Some("ok") => {
                eligible_carts.push((slot.element_address, voltag.clone(), tape));
            }
            Some(state) => rejected_carts.push(format!(
                "slot=0x{:04x} voltag={} cleaning_state={state}",
                slot.element_address, voltag
            )),
        }
    }
    if eligible_carts.is_empty() {
        let rejection_summary = if rejected_carts.is_empty() {
            "none".to_string()
        } else {
            rejected_carts.join("; ")
        };
        let reason = format!(
            "no eligible cleaning cartridge in library {library_serial}: configured prefixes=[{}], inventory prefix matches={prefix_matches}, rejected=[{rejection_summary}]",
            tape_prefixes.join(",")
        );
        tracing::error!(
            target: "remanence_cleaning",
            library_serial,
            drive_uuid = %crate::bytes_to_hex(drive_uuid),
            reason,
            "cleaning cartridge selection failed"
        );
        let detail = format!(
            "{{\"reason\":\"{}\",\"recovery_step\":\"selecting\"}}",
            json_escape_text(&reason)
        );
        let _ = clear_alarm_with_evidence(
            index,
            cfg,
            format!("cleaning-needs-operator:{}", run.run_id).as_str(),
        );
        let _ = index.set_drive_fenced(drive_uuid, false);
        let _ = record_library_event(
            index,
            cfg,
            handle,
            library_serial.as_str(),
            AuditEvent::DriveUnfenced,
            BTreeMap::from([
                (
                    "drive_uuid".to_string(),
                    CborValue::Bytes(drive_uuid.to_vec()),
                ),
                (
                    "component".to_string(),
                    CborValue::Text("cleaning".to_string()),
                ),
            ]),
        );
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("no-cln-cart:{library_serial}").as_str(),
            "no-cln-cart",
            "critical",
            Some(detail.as_str()),
        );
        let _ = index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(detail.as_str()));
        return Err(Status::failed_precondition(reason));
    }
    eligible_carts.sort_by_key(|(slot, _, _)| *slot);
    let (slot_address, voltag, tape_row) = eligible_carts.remove(0);
    let selected = index
        .select_clean_run_cart(
            run.run_id.as_str(),
            tape_row.tape_uuid.as_slice(),
            i64::from(slot_address),
            Some("{\"phase\":\"selecting\"}"),
        )
        .map_err(status_from_state_error)?;
    let selected = selected.ok_or_else(|| Status::internal("selected clean run disappeared"))?;
    let run_id = selected.run_id.clone();
    let complete_timeout = parse_duration_or(&clean_cfg.complete_timeout, Duration::minutes(10));
    let drive_bay = drive
        .last_element_address
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| Status::failed_precondition("drive has no current bay"))?;
    retry_cleaning_move(index, cfg, run_id.as_str(), drive_uuid, "moving-in", || {
        library
            .load(slot_address, drive_bay, &cfg.policy)
            .map_err(|err| format!("load cleaning cartridge: {err}"))?;
        Ok(())
    })?;
    let load_completed = std::time::Instant::now();
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "moving-in",
            Some("{\"phase\":\"moving-in\"}"),
        )
        .map_err(status_from_state_error)?;
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "cleaning",
            Some("{\"phase\":\"cleaning\"}"),
        )
        .map_err(status_from_state_error)?;
    let min_cycle = parse_duration_or(&clean_cfg.min_cycle_duration, Duration::minutes(1));
    if load_completed.elapsed()
        > std::time::Duration::from_millis(complete_timeout.whole_milliseconds().max(0) as u64)
    {
        let detail = format!(
            "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"cart\":\"{}\",\"recovery_step\":\"timeout\"}}",
            json_escape_text(&run_id),
            json_escape_text(&crate::bytes_to_hex(drive_uuid)),
            json_escape_text(&voltag),
        );
        let _ = index.mark_clean_run_needs_operator(run_id.as_str(), Some(detail.as_str()));
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cleaning-needs-operator:{}", run_id).as_str(),
            "cleaning-needs-operator",
            "warning",
            Some(detail.as_str()),
        );
        return Err(Status::deadline_exceeded("cleaning timeout exceeded"));
    }
    if cleaning_drive_is_idle(library, drive_bay)? {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "expired")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(
                run_id.as_str(),
                "failed",
                Some("{\"reason\":\"fast-eject\"}"),
            )
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cln-cart-expired:{}", voltag).as_str(),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"fast-eject\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge fast-ejected during cleaning",
        ));
    }
    let elapsed = load_completed.elapsed();
    let min_cycle_millis = min_cycle.whole_milliseconds().max(0) as u64;
    if elapsed < std::time::Duration::from_millis(min_cycle_millis) {
        std::thread::sleep(std::time::Duration::from_millis(
            min_cycle_millis.saturating_sub(elapsed.as_millis() as u64),
        ));
    }
    let mut drive_handle = library
        .open_drive_with_tape_io(
            drive_bay,
            &cfg.policy,
            crate::tape_io_runtime_config(&cfg.tape_io),
        )
        .map_err(|err| Status::internal(format!("open drive for cleaning verify: {err}")))?;
    let alerts = drive_handle.read_tape_alerts().map_err(|err| {
        let _ = index.terminalize_clean_run(
            run_id.as_str(),
            "failed",
            Some("{\"reason\":\"verify-read-failed\"}"),
        );
        Status::unavailable(format!("read TapeAlert page: {err}"))
    })?;
    let active_alerts = alerts.active();
    if alerts.is_set(22) {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "expired")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(run_id.as_str(), "failed", Some("{\"reason\":\"flag-22\"}"))
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cln-cart-expired:{}", voltag).as_str(),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"flag-22\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge expired during cleaning",
        ));
    }
    if alerts.is_set(20) || alerts.is_set(21) {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "rejected")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(
                run_id.as_str(),
                "failed",
                Some("{\"reason\":\"corroboration\"}"),
            )
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cart-not-cleaning-behavior:{}", voltag).as_str(),
            "cart-not-cleaning-behavior",
            "warning",
            Some("{\"reason\":\"corroboration\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge behaved like data media",
        ));
    }
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "moving-back",
            Some("{\"phase\":\"moving-back\"}"),
        )
        .map_err(status_from_state_error)?;
    retry_cleaning_move(
        index,
        cfg,
        run_id.as_str(),
        drive_uuid,
        "moving-back",
        || {
            library
                .unload(drive_bay, Some(slot_address), &cfg.policy)
                .map_err(|err| format!("unload cleaning cartridge: {err}"))?;
            Ok(())
        },
    )?;
    let eject_observed = std::time::Instant::now();
    if eject_observed.duration_since(load_completed)
        < std::time::Duration::from_millis(min_cycle_millis)
    {
        let _ = index
            .set_tape_cleaning_state(tape_row.tape_uuid.as_slice(), "expired")
            .map_err(status_from_state_error)?;
        let _ = index
            .advance_clean_run(
                run_id.as_str(),
                "failed",
                Some("{\"reason\":\"fast-eject\"}"),
            )
            .map_err(status_from_state_error)?;
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("cln-cart-expired:{}", voltag).as_str(),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"fast-eject\"}"),
        );
        return Err(Status::failed_precondition(
            "cleaning cartridge fast-ejected during cleaning",
        ));
    }
    let detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"cart\":\"{}\",\"recovery_step\":\"verify\"}}",
        json_escape_text(&run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        json_escape_text(&voltag),
    );
    let _ = index
        .advance_clean_run(
            run_id.as_str(),
            "verifying",
            Some("{\"phase\":\"verifying\"}"),
        )
        .map_err(status_from_state_error)?;
    index
        .finalize_verified_clean_run(
            run_id.as_str(),
            drive_uuid,
            Some(tape_row.tape_uuid.as_slice()),
            Some(detail.as_str()),
        )
        .map_err(status_from_state_error)?;
    let _ = clear_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run_id).as_str(),
    );
    let _ = clear_alarm_with_evidence(
        index,
        cfg,
        format!(
            "drive-cleaning-abnormal-frequency:{}",
            crate::bytes_to_hex(drive_uuid)
        )
        .as_str(),
    );
    let _ = clear_alarm_with_evidence(index, cfg, format!("cln-cart-expired:{}", voltag).as_str());
    let _ = clear_alarm_with_evidence(
        index,
        cfg,
        format!("cart-not-cleaning-behavior:{}", voltag).as_str(),
    );
    let _ = active_alerts;
    let _ = record_library_event(
        index,
        cfg,
        handle,
        library_serial.as_str(),
        AuditEvent::DriveUnfenced,
        BTreeMap::from([
            (
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.to_vec()),
            ),
            (
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            ),
        ]),
    );
    let _ = record_library_event(
        index,
        cfg,
        handle,
        library_serial.as_str(),
        AuditEvent::DriveCleaned,
        BTreeMap::from([
            (
                "drive_uuid".to_string(),
                CborValue::Bytes(drive_uuid.to_vec()),
            ),
            (
                "cart_tape_uuid".to_string(),
                CborValue::Bytes(tape_row.tape_uuid.clone()),
            ),
            (
                "component".to_string(),
                CborValue::Text("cleaning".to_string()),
            ),
        ]),
    );
    Ok(())
}

fn cleaning_too_soon(
    index: &CatalogIndex,
    drive_uuid: &[u8],
    min_interval: Duration,
    weekly_cap: usize,
) -> Result<bool, Status> {
    let runs = index
        .list_clean_runs(true)
        .map_err(status_from_state_error)?;
    let mut completed = Vec::new();
    for run in runs {
        if run.drive_uuid.as_slice() != drive_uuid {
            continue;
        }
        if run.phase != "done" {
            continue;
        }
        if let Ok(parsed) = OffsetDateTime::parse(run.updated_at_utc.as_str(), &Rfc3339) {
            completed.push(parsed);
        }
    }
    completed.sort_unstable();
    if let Some(last) = completed.last().copied() {
        let since = OffsetDateTime::now_utc() - last;
        if since < min_interval {
            return Ok(true);
        }
    }
    if weekly_cap > 0 {
        let week_ago = OffsetDateTime::now_utc() - Duration::days(7);
        if completed.iter().filter(|value| **value >= week_ago).count() >= weekly_cap {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_duration_or(value: &str, default: Duration) -> Duration {
    parse_simple_duration(value).unwrap_or(default)
}

fn parse_simple_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let split = value.find(|ch: char| !ch.is_ascii_digit())?;
    let (digits, unit) = value.split_at(split);
    let count = digits.parse::<i64>().ok()?;
    match unit {
        "ms" => Some(Duration::milliseconds(count)),
        "s" => Some(Duration::seconds(count)),
        "m" => Some(Duration::minutes(count)),
        "h" => Some(Duration::hours(count)),
        _ => None,
    }
}

fn json_escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn cleaning_drive_is_idle(
    library: &mut remanence_library::LibraryHandle,
    drive_bay: u16,
) -> Result<bool, Status> {
    library
        .refresh()
        .map_err(|err| Status::unavailable(format!("refresh library during cleaning: {err}")))?;
    Ok(library
        .library()
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == drive_bay)
        .map(|bay| !bay.loaded)
        .unwrap_or(true))
}

fn retry_cleaning_move(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run_id: &str,
    drive_uuid: &[u8],
    label: &str,
    mut op: impl FnMut() -> Result<(), String>,
) -> Result<(), Status> {
    let mut last_err = None;
    for attempt in 0..2 {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt == 0 {
                    tracing::warn!("{label} failed once during cleaning; retrying");
                }
            }
        }
    }
    let err = last_err.unwrap_or_else(|| "move failed".to_string());
    let detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"recovery_step\":\"{}\",\"error\":\"{}\"}}",
        json_escape_text(run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        json_escape_text(label),
        json_escape_text(&err),
    );
    let _ = index.terminalize_clean_run(run_id, "failed", Some(detail.as_str()));
    let _ = raise_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(detail.as_str()),
    );
    Err(Status::internal(err))
}

fn handle_reconcile(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
    handle: crate::operations::OperationHandle,
) {
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled before dispatch");
        return;
    }
    publish_running(&handle, &[("phase", "mount")]);
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        &handle,
        &tape_uuid,
        AuditEvent::OperationStarted,
        BTreeMap::new(),
    ) {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            &format!("record operation start audit: {err}"),
            &[("phase", "audit")],
        );
        return;
    }

    let library_serial = match cfg.default_library_serial.as_deref() {
        Some(serial) => serial,
        None => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                "tape reconciliation requires exactly one configured library in this slice",
                &[("phase", "mount")],
            );
            return;
        }
    };
    let lib = match cfg.report.library(library_serial) {
        Some(lib) => lib,
        None => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("library {library_serial} not found in discovery report"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    let mut library = match lib.open(&cfg.policy) {
        Ok(handle) => handle,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("open library: {err}"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    let mut drive = match load_tape_by_uuid(index, &mut library, &cfg.policy, &tape_uuid) {
        Ok(drive) => drive,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("mount tape: {err}"),
                &[("phase", "mount")],
            );
            return;
        }
    };
    if let Err(status) = verify_loaded_tape_identity(&mut drive, &tape_uuid) {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            &format!("tape identity: {}", status.message()),
            &[("phase", "mount")],
        );
        return;
    }
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled after mount");
        return;
    }

    let tape = match index.get_tape(&tape_uuid) {
        Ok(Some(tape)) => tape,
        Ok(None) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                "tape not found in catalog",
                &[("phase", "scan")],
            );
            return;
        }
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("catalog lookup: {err}"),
                &[("phase", "scan")],
            );
            return;
        }
    };
    let Some(block_size) = tape
        .block_size
        .and_then(|block_size| u32::try_from(block_size).ok())
    else {
        fail_operation(
            index,
            cfg,
            &handle,
            &tape_uuid,
            "tape block size is unknown or outside u32 range",
            &[("phase", "scan")],
        );
        return;
    };

    publish_running(&handle, &[("phase", "scan")]);
    let scan = {
        let mut source = DriveHandleRawSource::new(&mut drive);
        scan_reconstruct_filemark_map(&mut source, &tape_uuid, block_size)
    };
    let scan = match scan {
        Ok(scan) => scan,
        Err(err) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                &format!("scan filemark map: {err}"),
                &[("phase", "scan")],
            );
            return;
        }
    };
    if handle.is_cancelled() {
        cancel_operation(index, cfg, &handle, &tape_uuid, "cancelled after scan");
        return;
    }

    match reconcile_tape_files(index, &tape_uuid, &scan, &handle) {
        Ok(report) => {
            let rebuilt = report.tape_files_rebuilt.to_string();
            let mut detail = BTreeMap::new();
            detail.insert("tape_files".to_string(), CborValue::Text(rebuilt.clone()));
            if let Err(err) = record_reconcile_event(
                index,
                cfg,
                &handle,
                &tape_uuid,
                AuditEvent::OperationFinished,
                detail,
            ) {
                fail_operation(
                    index,
                    cfg,
                    &handle,
                    &tape_uuid,
                    &format!("record operation finish audit: {err}"),
                    &[("phase", "audit")],
                );
                return;
            }
            handle.publish_state(
                pb::OperationState::Succeeded,
                &[("phase", "complete"), ("tape_files", rebuilt.as_str())],
            );
        }
        Err(ReconcileExit::Cancelled(message)) => {
            cancel_operation(index, cfg, &handle, &tape_uuid, message.as_str());
        }
        Err(ReconcileExit::Failed(message)) => {
            fail_operation(
                index,
                cfg,
                &handle,
                &tape_uuid,
                message.as_str(),
                &[("phase", "project")],
            );
        }
    }
}

enum ReconcileExit {
    Cancelled(String),
    Failed(String),
}

fn reconcile_tape_files(
    index: &mut CatalogIndex,
    tape_uuid: &[u8; 16],
    scan: &FilemarkMap,
    handle: &crate::operations::OperationHandle,
) -> Result<remanence_state::TapeJournalIndexReport, ReconcileExit> {
    let existing = index
        .list_tape_files(tape_uuid)
        .map_err(|err| ReconcileExit::Failed(format!("list existing tape files: {err}")))?;
    let existing_object_ids = existing
        .into_iter()
        .filter(|entry| entry.kind == "object")
        .filter_map(|entry| entry.object_id.map(|id| (entry.tape_file_number, id)))
        .collect::<HashMap<_, _>>();

    let mut entries = Vec::with_capacity(scan.entries().len());
    for (idx, map_entry) in scan.entries().iter().enumerate() {
        if handle.is_cancelled() {
            return Err(ReconcileExit::Cancelled(format!(
                "cancelled after {} tape files",
                entries.len()
            )));
        }
        let mut entry = TapeFileEntry::from_map_entry(map_entry.clone());
        if map_entry.kind == TapeFileKind::Object {
            entry.object_id = existing_object_ids
                .get(&map_entry.tape_file_number)
                .cloned();
        }
        entries.push(entry);
        let count = (idx + 1).to_string();
        publish_running(
            handle,
            &[("phase", "project"), ("tape_files", count.as_str())],
        );
    }

    index
        .reconcile_tape_files_projection(
            *tape_uuid,
            &entries,
            scan.max_sidecar_end_exclusive(),
            scan.total_data_ordinals(),
        )
        .map_err(|err| ReconcileExit::Failed(format!("project tape files: {err}")))
}

fn fail_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    error_summary: &str,
    progress: &[(&str, &str)],
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "error_summary".to_string(),
        CborValue::Text(error_summary.to_string()),
    );
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        handle,
        tape_uuid,
        AuditEvent::OperationFailed,
        detail,
    ) {
        let audit_error = format!("{error_summary}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), progress);
    } else {
        handle.publish_failed(error_summary, progress);
    }
}

fn cancel_operation(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    detail_message: &str,
) {
    let mut detail = BTreeMap::new();
    detail.insert(
        "cancel_detail".to_string(),
        CborValue::Text(detail_message.to_string()),
    );
    if let Err(err) = record_reconcile_event(
        index,
        cfg,
        handle,
        tape_uuid,
        AuditEvent::CancelledBeforeDispatch,
        detail,
    ) {
        let audit_error = format!("{detail_message}; audit record failed: {err}");
        handle.publish_failed(audit_error.as_str(), &[("phase", "audit")]);
    } else {
        handle.publish_state(
            pb::OperationState::Cancelled,
            &[("phase", "cancelled"), ("detail", detail_message)],
        );
    }
}

fn publish_running(handle: &crate::operations::OperationHandle, progress: &[(&str, &str)]) {
    handle.publish_state(pb::OperationState::Running, progress);
}

fn record_reconcile_event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    tape_uuid: &[u8; 16],
    event: AuditEvent,
    mut detail: BTreeMap<String, CborValue>,
) -> Result<(), Status> {
    if tape_uuid.iter().any(|byte| *byte != 0) {
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
    }
    let subject_id = if tape_uuid.iter().any(|byte| *byte != 0) {
        Some(Uuid::from_bytes(*tape_uuid).to_string())
    } else {
        Some(handle.op_id_uuid().to_string())
    };
    let subject_kind = if tape_uuid.iter().any(|byte| *byte != 0) {
        "tape"
    } else {
        "operation"
    };
    crate::append_operation_audit(
        index,
        cfg.audit_dir.as_path(),
        cfg.audit_fsync,
        &cfg.audit_append_lock,
        crate::OperationAuditInput {
            actor: AuditActor::System,
            operation_id: handle.op_id_uuid(),
            operation_kind: handle.operation_kind(),
            event,
            subject_kind,
            subject_id,
            idempotency_key: None,
            detail,
        },
    )
}

#[derive(Clone, Copy, Debug, Default)]
struct RestoreReadPhases {
    position: StdDuration,
    transfer: StdDuration,
    bytes: u64,
    records: u64,
    commands: u64,
}

#[derive(Clone, Copy, Debug)]
struct RestoreDiagnosticContext {
    session_id: Uuid,
    tape_uuid: [u8; 16],
    block_size_bytes: u32,
    success: bool,
}

/// Times the existing `BlockSource` safety funnel without reimplementing any
/// tape operation. Every call delegates exactly once to the wrapped source.
struct DiagnosticBlockSource<'a> {
    inner: &'a mut dyn BlockSource,
    phases: RestoreReadPhases,
}

impl<'a> DiagnosticBlockSource<'a> {
    fn new(inner: &'a mut dyn BlockSource) -> Self {
        Self {
            inner,
            phases: RestoreReadPhases::default(),
        }
    }

    fn phases(&self) -> RestoreReadPhases {
        self.phases
    }
}

impl remanence_library::BlockRead for DiagnosticBlockSource<'_> {
    fn read_block(&mut self, buf: &mut [u8]) -> Result<usize, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.read_block(buf);
        self.phases.transfer += started.elapsed();
        if let Ok(bytes) = result {
            self.phases.commands = self.phases.commands.saturating_add(1);
            self.phases.records = self.phases.records.saturating_add(1);
            self.phases.bytes = self.phases.bytes.saturating_add(bytes as u64);
        }
        result
    }
}

impl BlockSource for DiagnosticBlockSource<'_> {
    fn read_block_batch(
        &mut self,
        buf: &mut [u8],
        block_size_bytes: u32,
        requested_records: u32,
        remaining_records_in_file: u32,
    ) -> Result<ReadBatchOutcome, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.read_block_batch(
            buf,
            block_size_bytes,
            requested_records,
            remaining_records_in_file,
        );
        self.phases.transfer += started.elapsed();
        if let Ok(outcome) = result {
            self.phases.commands = self.phases.commands.saturating_add(1);
            self.phases.records = self
                .phases
                .records
                .saturating_add(u64::from(outcome.records_read));
            self.phases.bytes = self
                .phases
                .bytes
                .saturating_add(u64::from(outcome.bytes_read));
        }
        result
    }

    fn read_batch_blocks(&self, block_size_bytes: u32) -> u32 {
        self.inner.read_batch_blocks(block_size_bytes)
    }

    fn read_ring_buffers(&self) -> u32 {
        self.inner.read_ring_buffers()
    }

    fn prove_read_position(
        &mut self,
        expected: TapePosition,
    ) -> Result<remanence_library::DevicePositionProof, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.prove_read_position(expected);
        self.phases.position += started.elapsed();
        result
    }

    fn rewind(&mut self) -> Result<(), TapeIoError> {
        let started = Instant::now();
        let result = self.inner.rewind();
        self.phases.position += started.elapsed();
        result
    }

    fn locate(&mut self, lba: u64) -> Result<TapePosition, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.locate(lba);
        self.phases.position += started.elapsed();
        result
    }

    fn space(&mut self, count: i64, kind: SpaceKind) -> Result<SpaceResult, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.space(count, kind);
        self.phases.position += started.elapsed();
        result
    }

    fn position(&mut self) -> Result<TapePosition, TapeIoError> {
        let started = Instant::now();
        let result = self.inner.position();
        self.phases.position += started.elapsed();
        result
    }
}

fn log_restore_read_diagnostics(
    drive: &DriveHandle,
    context: RestoreDiagnosticContext,
    phases: RestoreReadPhases,
    relay_diagnostics: StagedReadRelayDiagnostics,
    wall: StdDuration,
) {
    let relay = relay_diagnostics.client_write;
    let phase_sum = wall;
    let bottleneck = if phases.transfer >= relay {
        "drive"
    } else {
        "sender"
    };
    let diagnostics = drive.pipelined_read_diagnostics();
    let effective_batch_blocks = drive.requested_read_batch_blocks().min(
        drive
            .sg_reserved_size_bytes()
            .checked_div(context.block_size_bytes.max(1))
            .unwrap_or(1)
            .max(1),
    );
    let batch_effectiveness = if phases.commands == 0 {
        0.0
    } else {
        phases.records as f64 / phases.commands as f64
    };
    tracing::info!(
        target: "remanence_read_diag",
        phase = "restore_total",
        session_id = %context.session_id,
        tape_uuid = %Uuid::from_bytes(context.tape_uuid),
        status = if context.success { "ok" } else { "error" },
        effective_mode = "fixed_pipelined",
        block_size_bytes = context.block_size_bytes,
        staging_ring_buffers = drive.staging_ring_buffers(),
        effective_batch_blocks,
        batch_effectiveness_records_per_command = batch_effectiveness,
        bytes = phases.bytes,
        records = phases.records,
        commands = phases.commands,
        locate_position_ms = crate::diagnostics::duration_ms(phases.position),
        transfer_ms = crate::diagnostics::duration_ms(phases.transfer),
        relay_ms = crate::diagnostics::duration_ms(relay),
        phase_sum_ms = crate::diagnostics::duration_ms(phase_sum),
        wall_ms = crate::diagnostics::duration_ms(wall),
        bottleneck,
        drive_rate_mib_s = crate::diagnostics::mib_per_s(phases.bytes, phases.transfer),
        relay_rate_mib_s = crate::diagnostics::mib_per_s(phases.bytes, relay),
        client_write_ms = crate::diagnostics::duration_ms(relay_diagnostics.client_write),
        sender_stall_ms = crate::diagnostics::duration_ms(relay_diagnostics.sender_stall),
        client_write_bytes = relay_diagnostics.bytes,
        client_write_rate_mib_s = crate::diagnostics::mib_per_s(
            relay_diagnostics.bytes,
            relay_diagnostics.client_write,
        ),
        gap_samples = diagnostics.gap_samples,
        ioctl_samples = diagnostics.ioctl_samples,
        gap_p50_us = diagnostics.gap_p50_us,
        gap_p95_us = diagnostics.gap_p95_us,
        gap_max_us = diagnostics.gap_max_us,
        ioctl_p50_us = diagnostics.ioctl_p50_us,
        ioctl_p95_us = diagnostics.ioctl_p95_us,
        ioctl_max_us = diagnostics.ioctl_max_us,
        ioctl_mean_us = diagnostics.ioctl_mean_us,
        first_60s_ioctl_samples = diagnostics.first_60s_ioctl_samples,
        first_60s_ioctl_p50_us = diagnostics.first_60s_ioctl_p50_us,
        first_60s_ioctl_p95_us = diagnostics.first_60s_ioctl_p95_us,
        first_60s_ioctl_max_us = diagnostics.first_60s_ioctl_max_us,
        first_60s_ioctl_mean_us = diagnostics.first_60s_ioctl_mean_us,
        accounting_samples = diagnostics.accounting_samples,
        accounting_p50_us = diagnostics.accounting_p50_us,
        accounting_p95_us = diagnostics.accounting_p95_us,
        accounting_max_us = diagnostics.accounting_max_us,
        accounting_mean_us = diagnostics.accounting_mean_us,
        cadence_us = diagnostics.cadence_us,
        effective_feed_bytes_per_second = diagnostics.effective_feed_bytes_per_second,
        time_to_first_ioctl_ms = diagnostics.time_to_first_ioctl_ms,
        steady_reached = diagnostics.steady_reached,
        time_to_steady_ms = diagnostics.time_to_steady_ms,
        steady_window_seconds = diagnostics.steady_window_seconds,
        steady_threshold_percent = diagnostics.steady_threshold_percent,
        ramp_observation_seconds = diagnostics.ramp_observation_seconds,
        "remanence_read_diag",
    );
}

#[cfg(test)]
fn exclusive_restore_relay_phase(
    wall: StdDuration,
    position: StdDuration,
    transfer: StdDuration,
) -> StdDuration {
    wall.saturating_sub(position).saturating_sub(transfer)
}

#[allow(clippy::too_many_arguments)]
fn stream_one_object(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
    tape_uuid: &[u8; 16],
    object_id: &str,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
) -> Result<(), Status> {
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let manifest_sha256 = object
        .metadata_hash
        .as_deref()
        .map(|hash| {
            <[u8; 32]>::try_from(hash)
                .map_err(|_| Status::internal("catalog metadata_hash is not 32 bytes"))
        })
        .transpose()?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let tape_files = index
        .list_tape_files(tape_uuid)
        .map_err(status_from_state_error)?;
    let tape_file = tape_files
        .iter()
        .find(|file| {
            file.tape_file_number == copy.tape_file_number
                && file.kind == "object"
                && file.object_id.as_deref() == Some(object_id)
        })
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::internal("tape block size unknown"))?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit usize"))?;

    let block_size_u32 = u32::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before object read: {err}")))?;

    drive.reset_pipelined_diagnostics();
    let wall_started = Instant::now();
    let (result, phases) = {
        let mut source = DriveHandleSource(drive);
        let mut diagnostic_source = DiagnosticBlockSource::new(&mut source);
        let result = stream_with_staged_read_sender_diagnostics(
            chunk_tx,
            stream_chunk_bytes,
            |writer, terminal| {
                let mut sink = crate::read_core::CapturePayloadSink::new(writer);
                crate::read_core::read_object_payload_with_pipeline(
                    &mut diagnostic_source,
                    block_size_usize,
                    tape_file.block_count,
                    copy.tape_file_number,
                    manifest_sha256,
                    &mut sink,
                    crate::read_core::ReadPipelineConfig {
                        reservoir_bytes: cfg.tape_io.read_reservoir_bytes,
                        high_pct: cfg.tape_io.read_reservoir_high_pct,
                        low_pct: cfg.tape_io.read_reservoir_low_pct,
                        ranged_frontier: false,
                        proof_cadence_bytes: cfg
                            .tape_io
                            .position_check_bytes_ranged
                            .min(cfg.tape_io.read_reservoir_bytes / 2),
                        terminal: Some(terminal),
                    },
                    Arc::clone(&cfg.io_memory),
                )
                .map_err(|err| Status::internal(format!("read object: {err}")))?;
                let (_payload_bytes, _digest) = sink
                    .finish()
                    .map_err(|err| Status::internal(format!("finish payload stream: {err}")))?;
                Ok(())
            },
        );
        (result, diagnostic_source.phases())
    };
    let wall = wall_started.elapsed();
    log_restore_read_diagnostics(
        drive,
        RestoreDiagnosticContext {
            session_id,
            tape_uuid: *tape_uuid,
            block_size_bytes: block_size_u32,
            success: result.is_ok(),
        },
        phases,
        result.as_ref().copied().unwrap_or_default(),
        wall,
    );
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn stream_one_file_range(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    cfg: &WriteOwnerConfig,
    session_id: Uuid,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
) -> Result<(), Status> {
    let request =
        file_range_read_request(index, tape_uuid, object_id, file_id, start_byte, end_byte)?;
    let block_size_u32 = u32::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;

    drive.reset_pipelined_diagnostics();
    let wall_started = Instant::now();
    let (result, phases) = {
        let mut source = DriveHandleSource(drive);
        let mut diagnostic_source = DiagnosticBlockSource::new(&mut source);
        let result = stream_file_range_from_source(
            &mut diagnostic_source,
            request,
            stream_chunk_bytes,
            chunk_tx,
            &cfg.tape_io,
            Arc::clone(&cfg.io_memory),
        );
        (result, diagnostic_source.phases())
    };
    let wall = wall_started.elapsed();
    log_restore_read_diagnostics(
        drive,
        RestoreDiagnosticContext {
            session_id,
            tape_uuid: *tape_uuid,
            block_size_bytes: block_size_u32,
            success: result.is_ok(),
        },
        phases,
        result.as_ref().copied().unwrap_or_default(),
        wall,
    );
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn file_range_read_request(
    index: &CatalogIndex,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
) -> Result<crate::read_core::PlaintextFileRangeReadRequest, Status> {
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let file = resolve_object_file_for_range(index, object_id, file_id)?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let (range_start, range_len) = requested_file_range(file.size_bytes, start_byte, end_byte)?;

    let tape_files = index
        .list_tape_files(tape_uuid)
        .map_err(status_from_state_error)?;
    let tape_file = tape_files
        .iter()
        .find(|tape_file| {
            tape_file.tape_file_number == copy.tape_file_number
                && tape_file.kind == "object"
                && tape_file.object_id.as_deref() == Some(object_id)
        })
        .ok_or_else(|| Status::not_found("object tape file not in catalog"))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let block_size = tape
        .block_size
        .ok_or_else(|| Status::internal("tape block size unknown"))?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit usize"))?;
    let physical_file_start_lba =
        derive_physical_file_start_lba(tape_files.as_slice(), tape_file.tape_file_number);
    Ok(crate::read_core::PlaintextFileRangeReadRequest {
        block_size: block_size_usize,
        tape_file_number: tape_file.tape_file_number,
        physical_file_start_lba,
        first_chunk_lba: file.first_chunk_lba.map(BodyLba),
        file_size_bytes: file.size_bytes,
        range_start,
        range_len,
    })
}

/// Derive an absolute tape-file start from the dense committed catalog prefix.
/// Each trailing filemark consumes one physical LBA, matching the filemark-map
/// physical-position calculation. An incomplete or non-dense prefix returns
/// `None` so the range reader uses its logical REWIND/SPACE fallback.
fn derive_physical_file_start_lba(
    tape_files: &[remanence_state::TapeFileRecord],
    target_file_number: u32,
) -> Option<u64> {
    let mut expected_file_number = 0u32;
    let mut next_file_lba = 0u64;
    for tape_file in tape_files {
        if tape_file.tape_file_number != expected_file_number {
            return None;
        }
        if tape_file.tape_file_number == target_file_number {
            return Some(next_file_lba);
        }
        next_file_lba = next_file_lba
            .checked_add(tape_file.block_count)?
            .checked_add(1)?;
        expected_file_number = expected_file_number.checked_add(1)?;
    }
    None
}

fn resolve_object_file_for_range(
    index: &CatalogIndex,
    object_id: &str,
    file_id: &str,
) -> Result<NativeObjectFileRecord, Status> {
    if file_id.is_empty() {
        let files = index
            .list_native_object_files(object_id)
            .map_err(status_from_state_error)?;
        return match files.as_slice() {
            [file] => Ok(file.clone()),
            [] => Err(Status::failed_precondition(
                "empty file_id ranged reads require exactly one object file row; found 0",
            )),
            _ => Err(Status::failed_precondition(format!(
                "empty file_id ranged reads require exactly one object file row; found {}",
                files.len()
            ))),
        };
    }

    let file = index
        .get_native_object_file(object_id, file_id)
        .map_err(status_from_state_error)?;
    file.ok_or_else(|| Status::not_found("object file not found"))
}

fn stream_file_range_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    stream_chunk_bytes: u32,
    chunk_tx: crate::read_core::ReadStreamSender,
    tape_io: &TapeIoConfig,
    io_memory: Arc<crate::io_memory::IoMemoryReservation>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    // Ranged reads are opaque stored-payload reads. The daemon does not decrypt
    // or hold key material; clients interpret or decrypt the returned bytes.
    stream_with_staged_read_sender_diagnostics(chunk_tx, stream_chunk_bytes, |writer, terminal| {
        crate::read_core::read_plaintext_file_range_with_pipeline(
            source,
            request,
            writer,
            crate::read_core::ReadPipelineConfig {
                reservoir_bytes: tape_io.read_reservoir_bytes,
                high_pct: tape_io.read_reservoir_high_pct,
                low_pct: tape_io.read_reservoir_low_pct,
                ranged_frontier: true,
                proof_cadence_bytes: tape_io
                    .position_check_bytes_ranged
                    .min(tape_io.read_reservoir_bytes / 2),
                terminal: Some(terminal),
            },
            io_memory,
        )
        .map_err(status_from_file_range_error)
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct StagedReadRelayDiagnostics {
    client_write: StdDuration,
    sender_stall: StdDuration,
    bytes: u64,
}

fn stream_with_staged_read_sender_diagnostics(
    chunk_tx: crate::read_core::ReadStreamSender,
    stream_chunk_bytes: u32,
    produce: impl FnOnce(
        &mut (dyn std::io::Write + Send),
        Arc<crate::read_core::ReadTerminalAccumulator>,
    ) -> Result<(), Status>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    let staged_capacity = crate::read_core::read_stream_channel_capacity(
        usize::try_from(stream_chunk_bytes).unwrap_or(usize::MAX),
    );
    let (tx, rx) = std_mpsc::sync_channel(staged_capacity);
    let poison = Arc::new(Mutex::new(None::<String>));
    let terminal = Arc::new(crate::read_core::ReadTerminalAccumulator::default());
    std::thread::scope(|scope| {
        let sender_poison = Arc::clone(&poison);
        let sender_terminal = Arc::clone(&terminal);
        let sender = scope.spawn(move || {
            let result = drain_staged_read_sender(rx, chunk_tx, stream_chunk_bytes, sender_poison);
            if let Err(status) = &result {
                sender_terminal.record(
                    crate::read_core::ReadTerminalPriority::Sender,
                    status.clone(),
                );
            }
            result
        });
        let mut writer = StagedReadWriter::new(
            tx,
            Arc::clone(&poison),
            usize::try_from(stream_chunk_bytes).unwrap_or(usize::MAX),
        );
        let produce_result = produce(&mut writer, Arc::clone(&terminal)).and_then(|()| {
            writer
                .finish()
                .map_err(|err| Status::internal(format!("finish read stream: {err}")))
        });
        if let Err(status) = &produce_result {
            terminal.record(
                crate::read_core::ReadTerminalPriority::Decode,
                status.clone(),
            );
        }
        drop(writer);
        let sender_result = sender.join().unwrap_or_else(|_| {
            let status = Status::internal("staged read sender thread panicked");
            terminal.record(
                crate::read_core::ReadTerminalPriority::Sender,
                status.clone(),
            );
            Err(status)
        });
        match (produce_result, sender_result) {
            (Ok(()), Ok(diagnostics)) => Ok(diagnostics),
            _ => Err(terminal.finalize_after_join().unwrap_or_else(|| {
                Status::internal("read pipeline failed without terminal cause")
            })),
        }
    })
}

enum StagedReadItem {
    Data(Vec<u8>),
    Finish,
}

struct StagedReadWriter {
    tx: std_mpsc::SyncSender<StagedReadItem>,
    poison: Arc<Mutex<Option<String>>>,
    finished: bool,
    max_chunk_bytes: usize,
}

impl StagedReadWriter {
    fn new(
        tx: std_mpsc::SyncSender<StagedReadItem>,
        poison: Arc<Mutex<Option<String>>>,
        chunk_bytes: usize,
    ) -> Self {
        Self {
            tx,
            poison,
            finished: false,
            max_chunk_bytes: crate::read_core::effective_read_stream_chunk_bytes(chunk_bytes),
        }
    }

    fn check_poison(&self) -> std::io::Result<()> {
        if let Some(message) = self
            .poison
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                format!("staged read sender failed: {message}"),
            ))
        } else {
            Ok(())
        }
    }

    fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.check_poison()?;
        self.tx.send(StagedReadItem::Finish).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "staged read sender stopped")
        })?;
        self.finished = true;
        self.check_poison()
    }
}

impl std::io::Write for StagedReadWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.finished {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "staged read stream already finished",
            ));
        }
        self.check_poison()?;
        for chunk in buf.chunks(self.max_chunk_bytes) {
            self.tx
                .send(StagedReadItem::Data(chunk.to_vec()))
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "staged read sender stopped",
                    )
                })?;
        }
        self.check_poison()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.check_poison()
    }
}

fn drain_staged_read_sender(
    rx: std_mpsc::Receiver<StagedReadItem>,
    chunk_tx: crate::read_core::ReadStreamSender,
    stream_chunk_bytes: u32,
    poison: Arc<Mutex<Option<String>>>,
) -> Result<StagedReadRelayDiagnostics, Status> {
    let mut writer = Some(if stream_chunk_bytes == 0 {
        crate::read_core::ChannelWriter::new(chunk_tx)
    } else {
        crate::read_core::ChannelWriter::with_chunk_size(chunk_tx, stream_chunk_bytes as usize)
    });
    let mut first_error = None;
    let mut diagnostics = StagedReadRelayDiagnostics::default();
    while let Ok(item) = rx.recv() {
        if first_error.is_some() {
            continue;
        }
        let client_started = Instant::now();
        let result = match item {
            StagedReadItem::Data(bytes) => match writer.as_mut() {
                Some(writer) => {
                    let bytes_len = bytes.len() as u64;
                    let result = writer
                        .write_all(&bytes)
                        .map_err(|err| Status::internal(format!("send read stream: {err}")));
                    diagnostics.sender_stall = writer.sender_stall();
                    if result.is_ok() {
                        diagnostics.bytes = diagnostics.bytes.saturating_add(bytes_len);
                    }
                    result
                }
                None => Err(Status::internal("staged read data after finish")),
            },
            StagedReadItem::Finish => match writer.take() {
                Some(mut writer) => {
                    let result = writer
                        .finish()
                        .map_err(|err| Status::internal(format!("finish read stream: {err}")));
                    diagnostics.sender_stall = writer.sender_stall();
                    result
                }
                None => Ok(()),
            },
        };
        diagnostics.client_write += client_started.elapsed();
        if let Err(status) = result {
            set_staged_read_poison(&poison, status.message());
            first_error = Some(status);
        }
    }
    match first_error {
        Some(status) => Err(status),
        None => Ok(diagnostics),
    }
}

fn set_staged_read_poison(poison: &Arc<Mutex<Option<String>>>, message: &str) {
    let mut guard = poison.lock().unwrap_or_else(|err| err.into_inner());
    guard.get_or_insert_with(|| message.to_string());
}

fn requested_file_range(
    file_size_bytes: u64,
    start_byte: u64,
    end_byte: u64,
) -> Result<(u64, u64), Status> {
    if start_byte == 0 && end_byte == 0 {
        return Ok((0, file_size_bytes));
    }
    let range_len = end_byte.checked_sub(start_byte).ok_or_else(|| {
        Status::invalid_argument("end_byte must be greater than or equal to start_byte")
    })?;
    Ok((start_byte, range_len))
}

fn status_from_file_range_error(err: FormatError) -> Status {
    match err {
        FormatError::InvalidInput(message) => Status::invalid_argument(message),
        other => Status::internal(format!("read object range: {other}")),
    }
}

fn verify_loaded_tape_identity(
    drive: &mut DriveHandle,
    tape_uuid: &[u8; 16],
) -> Result<(), Status> {
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before read: {err}")))?;
    let mut source = DriveHandleSource(drive);
    verify_tape_identity(&mut source, tape_uuid)
        .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    Ok(())
}

fn position_read_resume(
    index: &CatalogIndex,
    drive: &mut DriveHandle,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    let request = file_range_read_request(
        index,
        &target.tape_uuid,
        target.object_id.as_str(),
        target.file_id.as_str(),
        0,
        0,
    )?;
    drive
        .rewind()
        .map_err(|err| Status::internal(format!("rewind before resume position: {err}")))?;
    let mut source = DriveHandleSource(drive);
    verify_and_position_read_resume_from_source(&mut source, request, target)
}

fn verify_and_position_read_resume_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    verify_tape_identity(source, &target.tape_uuid)
        .map_err(|err| Status::failed_precondition(format!("tape identity: {err}")))?;
    source
        .locate(0)
        .map_err(|err| Status::internal(format!("return to BOT after identity proof: {err}")))?;
    position_read_resume_from_source(source, request, target)
}

fn position_read_resume_from_source(
    source: &mut dyn BlockSource,
    request: crate::read_core::PlaintextFileRangeReadRequest,
    target: &ReadResumeTarget,
) -> Result<u64, Status> {
    let first_chunk_lba = request.first_chunk_lba.ok_or_else(|| {
        Status::failed_precondition("resume target file has no data-chunk boundary")
    })?;
    let block_size = u64::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u64"))?;
    let catalog_boundary = first_chunk_lba
        .0
        .checked_mul(block_size)
        .ok_or_else(|| Status::internal("catalogued file boundary byte offset overflow"))?;
    if target.file_boundary_byte_offset != catalog_boundary {
        return Err(Status::invalid_argument(format!(
            "resume offset is not the catalogued file boundary: expected {catalog_boundary}, got {}",
            target.file_boundary_byte_offset
        )));
    }

    let mut positioned = source
        .space(i64::from(request.tape_file_number), SpaceKind::Filemarks)
        .map_err(|err| Status::internal(format!("space to resume object: {err}")))?
        .position_after;
    let skip_blocks = i64::try_from(first_chunk_lba.0)
        .map_err(|_| Status::invalid_argument("resume file boundary exceeds SPACE range"))?;
    if skip_blocks != 0 {
        positioned = source
            .space(skip_blocks, SpaceKind::Blocks)
            .map_err(|err| Status::internal(format!("space to resume file boundary: {err}")))?
            .position_after;
    }
    let proof = source
        .prove_read_position(positioned)
        .map_err(|err| Status::failed_precondition(format!("resume position proof: {err}")))?;
    if let Some(expected) = target.expected_position_lba {
        if proof.lba() != expected {
            return Err(Status::failed_precondition(format!(
                "resume position proof mismatch: expected LBA {expected}, observed {}",
                proof.lba()
            )));
        }
    }
    Ok(proof.lba())
}

fn read_session_proto(
    session_id: Uuid,
    tape_uuid: &TapeUuid,
    state: pb::read_session::State,
    opened_at_utc: &str,
    drive_element_address: u16,
    position_after_lba: Option<u64>,
    daemon_epoch: u64,
) -> pb::ReadSession {
    pb::ReadSession {
        session_id: session_id.as_bytes().to_vec(),
        tape_uuid: tape_uuid.to_vec(),
        drive_element_address: u32::from(drive_element_address),
        state: state as i32,
        opened_at: timestamp_from_rfc3339(opened_at_utc),
        position_proof: position_after_lba
            .map(|position_after_lba| pb::DevicePositionProof { position_after_lba }),
        daemon_epoch,
    }
}

struct WriteSessionProtoInput<'a> {
    session_id: Uuid,
    tape_uuid: &'a TapeUuid,
    state: pb::write_session::State,
    objects_committed: u64,
    bytes_committed: u64,
    opened_at_utc: &'a str,
    last_checkpoint_at_utc: Option<&'a str>,
    drive_element_address: u16,
    pending_batch: Option<&'a PendingCheckpointBatch>,
}

fn session_proto(input: WriteSessionProtoInput<'_>) -> pb::WriteSession {
    let checkpoint_deadline = input.pending_batch.map(|batch| {
        let remaining = batch.deadline.saturating_duration_since(Instant::now());
        let seconds = OffsetDateTime::now_utc()
            .unix_timestamp()
            .saturating_add(i64::try_from(remaining.as_secs()).unwrap_or(i64::MAX));
        prost_types::Timestamp { seconds, nanos: 0 }
    });
    pb::WriteSession {
        session_id: input.session_id.as_bytes().to_vec(),
        tape_uuid: input.tape_uuid.to_vec(),
        drive_element_address: u32::from(input.drive_element_address),
        body_format: "rao-v1".to_string(),
        state: input.state as i32,
        objects_committed: input.objects_committed,
        bytes_committed: input.bytes_committed,
        opened_at: timestamp_from_rfc3339(input.opened_at_utc),
        last_checkpoint_at: input
            .last_checkpoint_at_utc
            .and_then(timestamp_from_rfc3339),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool as i32,
        tape_sequence: vec![input.tape_uuid.to_vec()],
        current_tape_index: 0,
        pending_checkpoint_objects: input
            .pending_batch
            .map_or(0, |batch| batch.objects.len() as u64),
        pending_checkpoint_bytes: input.pending_batch.map_or(0, |batch| batch.logical_bytes),
        oldest_pending_age_seconds: input
            .pending_batch
            .map_or(0, |batch| batch.opened_at.elapsed().as_secs()),
        checkpoint_deadline,
        checkpointed_objects: Vec::new(),
        committed_copies: Vec::new(),
    }
}

pub(crate) fn status_from_pool_write_error(err: PoolWriteError) -> Status {
    let message = err.to_string();
    match err {
        PoolWriteError::Select(select) => status_from_select_tape_error(select),
        PoolWriteError::State(state) => status_from_state_error(state),
        PoolWriteError::InvalidInput(_) => Status::invalid_argument(message),
        PoolWriteError::MissingTapeGeometry(_) => Status::failed_precondition(message),
        PoolWriteError::ParityAppendUnsupported { .. } => Status::failed_precondition(message),
        PoolWriteError::SelectedTapeInsufficientCapacity { .. } => {
            Status::failed_precondition(message)
        }
        PoolWriteError::CheckpointDirectoryCeiling { .. } => Status::resource_exhausted(message),
        PoolWriteError::ContentHashMismatch { .. } => Status::failed_precondition(message),
        PoolWriteError::CallerObjectIdConflict { .. } => Status::already_exists(message),
        PoolWriteError::ReplayObjectInvalid { .. } => Status::internal(message),
        PoolWriteError::Streaming(streaming) => status_from_streaming_error(&streaming, message),
        PoolWriteError::Parity(parity) => status_from_parity_error(&parity, message),
        PoolWriteError::Io { .. }
        | PoolWriteError::TapeIo(_)
        | PoolWriteError::TransferWithSecondary { .. }
        | PoolWriteError::TimeFormat(_) => Status::internal(message),
    }
}

fn status_from_streaming_error(err: &StreamingError, message: String) -> Status {
    match err {
        StreamingError::InvalidInput(_) | StreamingError::InvalidXattrNamespacePrefix { .. } => {
            Status::invalid_argument(message)
        }
        StreamingError::Format(format) => status_from_format_error(format, message),
        StreamingError::Parity(parity) => status_from_parity_error(parity, message),
        StreamingError::Io { .. } => Status::internal(message),
    }
}

fn status_from_format_error(err: &FormatError, message: String) -> Status {
    match err {
        FormatError::InvalidInput(_) => Status::invalid_argument(message),
        _ => Status::internal(message),
    }
}

fn status_from_parity_error(err: &ParityError, message: String) -> Status {
    match err {
        ParityError::CapacityReserveExceeded { .. }
        | ParityError::ObjectTooLargeForEmptyTape { .. }
        | ParityError::BootstrapPayloadTooLarge { .. } => Status::resource_exhausted(message),
        _ => Status::internal(message),
    }
}

pub(crate) fn status_from_select_tape_error(err: SelectTapeError) -> Status {
    let message = err.to_string();
    match err {
        SelectTapeError::UnknownPool { .. } => Status::invalid_argument(message),
        SelectTapeError::EmptyPool { .. }
        | SelectTapeError::NoWritableTapes { .. }
        | SelectTapeError::NoUnreservedWritableTapes { .. }
        | SelectTapeError::AmbiguousNeedsPolicy { .. } => Status::resource_exhausted(message),
        SelectTapeError::NoBatchedEligibleTapes { .. } => Status::failed_precondition(message),
        SelectTapeError::InvalidTapeGeometry { .. } => Status::failed_precondition(message),
        SelectTapeError::InvalidTapeUuid { .. } => Status::internal(message),
        SelectTapeError::State(state) => status_from_state_error(state),
    }
}

fn now_rfc3339() -> Result<String, time::error::Format> {
    OffsetDateTime::now_utc().format(&Rfc3339)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use remanence_aead::RecipientPrivateKey;
    use remanence_chaos::model::{ModelTransport, Record, VirtualTape, VirtualWorld};
    use remanence_format::{
        read_encrypted_rao_file_range_to_vec, write_encrypted_rao_object, write_rem_tar_object,
        RemTarFile, RemTarObjectLayout, RemTarObjectOptions,
    };
    use remanence_library::{
        DriveBay, ElementLayout, FixtureTransport, IdentitySource, InstalledDrive, Library,
        RecordingLog, RecordingTransport, SgTransport, Slot, VecBlockSink, VecBlockSource,
        VecBlockSourceCall, WormMediaState,
    };
    use remanence_parity::bootstrap::{parse_bootstrap_block, write_bootstrap_block};
    use remanence_parity::{
        BootstrapPayload, CommittedBundle, CommittedBundleKind, ParityConfig, TapeFileEntry,
        TapeFileKind,
    };
    use remanence_state::{
        CatalogIndex, DriveObservationInput, NativeObjectCopyProjectionInput,
        NativeObjectFileProjectionInput, NativeObjectProjectionInput, ProvisionTapeInput,
        TapeFileRecord, TapeJournalIndexInput, TapePoolProjectionInput,
        OBJECT_COPY_REPRESENTATION_PLAINTEXT,
    };
    use tokio_stream::StreamExt;

    const RANGE_OBJECT_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const RANGE_TAPE_UUID: [u8; 16] = [0xAB; 16];

    #[test]
    fn restore_phase_decomposition_sums_to_wall_including_saturation() {
        let wall = StdDuration::from_millis(100);
        let position = StdDuration::from_millis(20);
        let transfer = StdDuration::from_millis(65);
        let relay = exclusive_restore_relay_phase(wall, position, transfer);
        assert_eq!(relay, StdDuration::from_millis(15));
        assert_eq!(position + transfer + relay, wall);

        let saturated = exclusive_restore_relay_phase(
            StdDuration::from_millis(5),
            StdDuration::from_millis(4),
            StdDuration::from_millis(4),
        );
        assert_eq!(saturated, StdDuration::ZERO);
    }

    #[test]
    fn session_open_media_family_uses_lto9_barcode_suffix() {
        assert!(matches!(
            session_open_media_family(Some("AOX030L9")),
            MediaFamily::Lto9OrLater
        ));
        assert!(matches!(
            session_open_media_family(Some("AOX030LZ")),
            MediaFamily::Lto9OrLater
        ));
        assert!(matches!(
            session_open_media_family(Some("AOX030L8")),
            MediaFamily::Unknown
        ));
        assert!(matches!(
            session_open_media_family(None),
            MediaFamily::Unknown
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn load_wait_absorbs_only_retryable_drive_completions() {
        fn load_check_condition(key: u8, asc: u8, ascq: u8) -> LoadError {
            let mut sense = vec![0_u8; 32];
            sense[0] = 0x70;
            sense[2] = key;
            sense[7] = 24;
            sense[12] = asc;
            sense[13] = ascq;
            LoadError::DriveLoad(DriveOpError::ScsiError(
                remanence_library::ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                },
            ))
        }

        let first_mount_attention = load_check_condition(0x06, 0x28, 0x00);
        assert!(matches!(
            retryable_readiness_from_load_error(&first_mount_attention, MediaFamily::Lto9OrLater),
            Some(MediaReadiness::UnitAttention {
                asc: 0x28,
                ascq: 0x00
            })
        ));

        let medium_error = load_check_condition(0x03, 0x11, 0x00);
        assert!(
            retryable_readiness_from_load_error(&medium_error, MediaFamily::Lto9OrLater).is_none()
        );
    }

    #[test]
    fn session_open_readiness_fence_records_operation_and_guidance() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-readiness-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let ctx = SessionOpenReadinessContext {
            action: "open write session",
            bay: 0x0001,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: Some(0x03eb),
            drive_serial: Some("8031BDC7D1"),
            needs_drive_load: true,
        };

        let status = record_session_open_readiness_fence(
            &mut index,
            &ctx,
            "session_open_short_probe",
            &MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true,
            },
        );

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status
            .message()
            .contains("media_readiness_state=media_initializing"));
        assert!(status
            .message()
            .contains("rem tape wait-ready --library DEC418146K_LL02"));
        let active = index
            .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
            .expect("active fences");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].phase, "session_open_short_probe");
        assert_eq!(active[0].state, "media_initializing");
        assert_eq!(active[0].dirty_scope.as_deref(), Some("drive+tape"));
        assert_eq!(active[0].drive_element, 1);
        assert_eq!(active[0].drive_serial.as_deref(), Some("8031BDC7D1"));
        assert_eq!(active[0].barcode.as_deref(), Some("AOX030L9"));
        assert_eq!(active[0].source_slot, Some(0x03eb));
        assert_eq!(active[0].media_generation, Some(9));
        assert_eq!(active[0].last_cdb_opcode, Some(0));
        assert_eq!(active[0].last_sense_key, Some(0x02));
        assert_eq!(active[0].last_asc, Some(0x04));
        assert_eq!(active[0].last_ascq, Some(0x01));
        assert!(active[0].quarantine_id.is_none());
    }

    #[test]
    fn session_open_refuses_active_tape_io_fence_until_operator_release() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-tape-io-fence-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let tape_uuid = [0x44; 16];
        let fence = index
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: Some("AOX044L9".to_string()),
                reason: "partial_batch".to_string(),
                evidence_json: Some("{\"records_written\":2}".to_string()),
            })
            .expect("record tape-I/O fence");

        let status = session_open_reject_tape_io_fences(
            &index,
            &tape_uuid,
            Some("AOX044L9"),
            "open write session",
        )
        .expect_err("active tape-I/O fence must block session open");

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains(&fence.quarantine_id));
        assert!(status.message().contains("partial_batch"));

        index
            .release_tape_io_fence(&fence.quarantine_id, "operator released")
            .expect("release tape-I/O fence")
            .expect("released fence");
        session_open_reject_tape_io_fences(
            &index,
            &tape_uuid,
            Some("AOX044L9"),
            "open write session",
        )
        .expect("released tape-I/O fence no longer blocks session open");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_open_refuses_active_media_readiness_fence_before_drive_probe() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-admission-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let active_operation_id = Uuid::from_u128(0x9100);
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: active_operation_id,
                run_id: None,
                library_serial: "DEC418146K_LL02".to_string(),
                changer_sg: Some("/dev/sg8".to_string()),
                drive_element: 0x0100,
                drive_sg: Some("/dev/sg7".to_string()),
                drive_serial: Some("DRV_MOVE_OBS".to_string()),
                barcode: Some("AOX030L9".to_string()),
                source_slot: Some(0x03eb),
                media_generation: Some(9),
                phase: "readiness_poll".to_string(),
                state: "media_initializing".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record active readiness operation");
        let (mut drive, log) = open_test_drive_with_tur_script("DEC418146K_LL02", vec![None]);
        let before = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .count();
        let ctx = SessionOpenReadinessContext {
            action: "open write session",
            bay: 0x0100,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: Some(0x03eb),
            drive_serial: Some("DRV_MOVE_OBS"),
            needs_drive_load: true,
        };

        let status = session_open_short_probe_or_load(&mut index, &mut drive, ctx)
            .expect_err("active readiness fence must block session-open admission");

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("active media-readiness fence"));
        assert!(status.message().contains(&active_operation_id.to_string()));
        let after = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .count();
        assert_eq!(
            after, before,
            "session-open admission must refuse before TUR or LOAD"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_open_loads_immediate_after_unit_attention_then_load_required() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-load-after-ua-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let (mut drive, log) = open_test_drive_with_tur_script(
            "DEC418146K_LL02",
            vec![
                Some(readiness_fixed_sense(0x06, 0x29, 0x00)),
                Some(readiness_fixed_sense(0x02, 0x04, 0x02)),
                None,
            ],
        );
        let ctx = SessionOpenReadinessContext {
            action: "open write session",
            bay: 0x0100,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: Some(0x03eb),
            drive_serial: Some("DRV_MOVE_OBS"),
            needs_drive_load: true,
        };

        session_open_short_probe_or_load(&mut index, &mut drive, ctx)
            .expect("session-open readiness should issue LOAD IMMED then reach ready");

        let control_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .map(|cdb| (cdb[0], cdb[1], cdb[4]))
            .collect::<Vec<_>>();
        assert_eq!(
            control_cdbs,
            vec![
                (0x00, 0x00, 0x00),
                (0x00, 0x00, 0x00),
                (0x1b, 0x01, 0x01),
                (0x00, 0x00, 0x00)
            ]
        );
        assert!(
            index
                .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
                .expect("active fences")
                .is_empty(),
            "ready session-open probe must not leave an active fence"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_open_loads_immediate_for_already_loaded_initialization_required() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-session-open-already-loaded-load-required-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let (mut drive, log) = open_test_drive_with_tur_script(
            "DEC418146K_LL02",
            vec![Some(readiness_fixed_sense(0x02, 0x04, 0x02)), None],
        );
        let ctx = SessionOpenReadinessContext {
            action: "open read session",
            bay: 0x0100,
            library_serial: "DEC418146K_LL02",
            barcode: Some("AOX030L9"),
            source_slot: None,
            drive_serial: Some("DRV_MOVE_OBS"),
            needs_drive_load: false,
        };

        session_open_short_probe_or_load(&mut index, &mut drive, ctx)
            .expect("already-loaded 04/02 should issue LOAD IMMED then reach ready");

        let control_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .map(|cdb| (cdb[0], cdb[1], cdb[4]))
            .collect::<Vec<_>>();
        assert_eq!(
            control_cdbs,
            vec![(0x00, 0x00, 0x00), (0x1b, 0x01, 0x01), (0x00, 0x00, 0x00)]
        );
        assert!(
            index
                .list_active_media_readiness_operations(Some("DEC418146K_LL02"))
                .expect("active fences")
                .is_empty(),
            "ready session-open probe must not leave an active fence"
        );
    }

    fn changer_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
    }

    fn drive_lto9_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
    }

    fn vpd80_response(serial: &str) -> Vec<u8> {
        let bytes = serial.as_bytes();
        let mut response = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
        response.extend_from_slice(bytes);
        response
    }

    fn test_changer_library(serial: &str) -> Library {
        Library {
            serial: serial.to_string(),
            changer_sg: PathBuf::from("/dev/sg-mock"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
            changer_inquiry: remanence_library::scsi::Inquiry::parse(include_bytes!(
                "../../../fixtures/inquiry/changer-msl-g3.bin"
            ))
            .expect("parse changer inquiry fixture"),
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
                    serial: "DRV_MOVE_OBS".to_string(),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    vendor: Some("IBM".to_string()),
                    product: Some("ULT3580".to_string()),
                    revision: Some("A1".to_string()),
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
                cartridge: Some("TAPE_MOVE".to_string()),
            }],
            ie_ports: Vec::new(),
        }
    }

    fn open_test_changer(library: &Library) -> ChangerHandle {
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        let serial = library.serial.clone();
        let mut responses = Some(vec![changer_inquiry_response(), vpd80_response(&serial)]);
        library
            .open_with(&policy, move |_| {
                let responses = responses
                    .take()
                    .expect("test changer transport opened once");
                Ok::<_, remanence_library::IoErrorKind>(Box::new(
                    FixtureTransport::new().with_responses(responses),
                )
                    as Box<dyn remanence_library::SgTransport>)
            })
            .expect("open test changer")
            .into_changer()
    }

    #[cfg(target_os = "linux")]
    fn open_test_drive_with_tur_script(
        library_serial: &str,
        tur_senses: Vec<Option<Vec<u8>>>,
    ) -> (DriveHandle, RecordingLog) {
        let library = test_changer_library(library_serial);
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        let log = RecordingLog::new();
        let log_for_factory = log.clone();
        let changer_serial = library.serial.clone();
        let mut changer_responses = Some(vec![
            changer_inquiry_response(),
            vpd80_response(&changer_serial),
        ]);
        let mut drive_responses = Some(vec![
            drive_lto9_inquiry_response(),
            vpd80_response("DRV_MOVE_OBS"),
        ]);
        let mut tur_senses = Some(tur_senses);
        let mut handle = library
            .open_with(&policy, move |path| {
                if path == Path::new("/dev/sg-mock") {
                    let responses = changer_responses
                        .take()
                        .expect("changer opened once in test");
                    Ok::<_, remanence_library::IoErrorKind>(Box::new(RecordingTransport::with_log(
                        FixtureTransport::new().with_responses(responses),
                        log_for_factory.clone(),
                    ))
                        as Box<dyn SgTransport>)
                } else if path == Path::new("/dev/sg-drive-mock") {
                    let responses = drive_responses.take().expect("drive opened once in test");
                    let inner = FixtureTransport::new().with_responses(responses);
                    Ok::<_, remanence_library::IoErrorKind>(Box::new(RecordingTransport::with_log(
                        TurScriptTransport::new(
                            inner,
                            tur_senses.take().expect("TUR script consumed once"),
                        ),
                        log_for_factory.clone(),
                    ))
                        as Box<dyn SgTransport>)
                } else {
                    Err(remanence_library::IoErrorKind {
                        kind: "NotFound",
                        message: format!("unknown test path {path:?}"),
                        raw_os_error: None,
                    })
                }
            })
            .expect("library opens");
        (
            handle.open_drive(0x0100, &policy).expect("drive opens"),
            log,
        )
    }

    #[cfg(target_os = "linux")]
    struct TurScriptTransport<T> {
        inner: T,
        tur_senses: std::collections::VecDeque<Option<Vec<u8>>>,
    }

    #[cfg(target_os = "linux")]
    impl<T> TurScriptTransport<T> {
        fn new(inner: T, tur_senses: Vec<Option<Vec<u8>>>) -> Self {
            Self {
                inner,
                tur_senses: tur_senses.into(),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl<T: SgTransport> SgTransport for TurScriptTransport<T> {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            self.inner.execute_none(cdb)?;
            if cdb == [0, 0, 0, 0, 0, 0] {
                if let Some(Some(sense)) = self.tur_senses.pop_front() {
                    return Err(remanence_library::ScsiError::CheckCondition {
                        sense,
                        bytes_transferred: 0,
                    });
                }
            }
            Ok(())
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_out(cdb, buf)
        }

        fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
            self.inner.set_timeout_for(class)
        }
    }

    #[cfg(target_os = "linux")]
    fn readiness_fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut sense = vec![0u8; 32];
        sense[0] = 0x70;
        sense[2] = key & 0x0f;
        sense[7] = 24;
        sense[12] = asc;
        sense[13] = ascq;
        sense
    }

    fn open_model_library(
        world: std::sync::Arc<std::sync::Mutex<VirtualWorld>>,
    ) -> remanence_library::LibraryHandle {
        let library = world.lock().expect("world lock").library_snapshot();
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        library
            .open_with(&policy, move |path| {
                let role = world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .expect("known model path");
                Ok(Box::new(ModelTransport::new(
                    std::sync::Arc::clone(&world),
                    role,
                )))
            })
            .expect("open model library")
    }

    fn test_write_owner_config(
        index_path: PathBuf,
        audit_dir: PathBuf,
        library: &remanence_library::LibraryHandle,
        library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
    ) -> WriteOwnerConfig {
        let serial = library.library().serial.clone();
        WriteOwnerConfig {
            index_path,
            report: DiscoveryReport {
                libraries: vec![library.library().clone()],
                warnings: Vec::new(),
            },
            policy: remanence_library::StaticAllowlist::new([serial.as_str()]),
            audit_dir,
            audit_fsync: false,
            audit_append_lock: Arc::new(std::sync::Mutex::new(())),
            reservations: Arc::new(HashMap::new()),
            default_library_serial: Some(serial.clone()),
            library_snapshot,
            snapshot_miss_alarm: 1,
            managed_library_serials: Arc::new(HashSet::from([serial])),
            cleaning: remanence_state::CleaningConfig::default(),
            tape_io: remanence_state::TapeIoConfig::default(),
            io_memory: crate::io_memory::IoMemoryReservation::new(
                remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
            )
            .expect("test I/O memory manager"),
            checkpoint_journal_dir: std::env::temp_dir().join("rem-checkpoint-tests"),
            checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
            checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
            checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
            session_idle_seconds: 1800,
            lifecycle: None,
        }
    }

    fn test_io_memory() -> Arc<crate::io_memory::IoMemoryReservation> {
        crate::io_memory::IoMemoryReservation::new(remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES)
            .expect("test I/O memory manager")
    }

    fn library_snapshot_cell(library: Library) -> Arc<RwLock<Arc<crate::LibrarySnapshot>>> {
        Arc::new(RwLock::new(Arc::new(crate::LibrarySnapshot {
            report: DiscoveryReport {
                libraries: vec![library],
                warnings: Vec::new(),
            },
            captured_at: OffsetDateTime::UNIX_EPOCH,
        })))
    }

    async fn append_actor_test_file(
        drive_tx: &mpsc::Sender<DriveCommand>,
        session_id: Uuid,
        source_path: PathBuf,
        archive_path: &str,
        caller_object_id: &str,
        payload: &[u8],
    ) -> AppendFinishOutcome {
        append_actor_test_file_result(
            drive_tx,
            session_id,
            source_path,
            archive_path,
            caller_object_id,
            payload,
        )
        .await
        .expect("actor test append succeeds")
    }

    async fn append_actor_test_file_result(
        drive_tx: &mpsc::Sender<DriveCommand>,
        session_id: Uuid,
        source_path: PathBuf,
        archive_path: &str,
        caller_object_id: &str,
        payload: &[u8],
    ) -> Result<AppendFinishOutcome, Status> {
        std::fs::write(&source_path, payload).expect("write actor test source");
        let (append_tx, append_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::AppendFinish {
                session_id,
                source: crate::WriteObjectSource::Path(source_path),
                archive_path: PathBuf::from(archive_path),
                caller_object_id: caller_object_id.to_string(),
                expected_content_sha256: None,
                live_write_counter: None,
                reply: append_tx,
            })
            .await
            .expect("send actor test append");
        append_rx.await.expect("actor test append reply")
    }

    /// Open one parity-off actor session against an already seated test tape.
    async fn open_actor_test_write_session(
        drive_tx: &mpsc::Sender<DriveCommand>,
        pool_cfg: &TapePoolConfig,
        tape_uuid: TapeUuid,
        library_serial: &str,
        barcode: &str,
        drive_uuid: &[u8],
        drive_serial: &str,
    ) -> Uuid {
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    pool_id: pool_cfg.id.clone(),
                    tape_uuid,
                    block_size: u32::try_from(pool_cfg.block_size_bytes)
                        .expect("actor test pool block size fits u32"),
                    parity_config: ParityConfig::None,
                },
                needs_drive_load: false,
                library_serial: library_serial.to_string(),
                barcode: Some(barcode.to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.to_vec()),
                drive_serial: Some(drive_serial.to_string()),
                reply: open_tx,
            })
            .await
            .expect("send actor test write open");
        let session = open_rx
            .await
            .expect("actor test write open reply")
            .expect("open actor test write session");
        Uuid::from_slice(&session.session_id).expect("actor test write session UUID")
    }

    #[test]
    fn checkpoint_timer_request_queues_behind_existing_drive_actor_work() {
        let (tx, mut rx) = mpsc::channel(4);
        let session_id = Uuid::from_bytes([0x71; 16]);
        let batch_id = Uuid::from_bytes([0x72; 16]);
        let (reply, _reply_rx) = oneshot::channel();
        tx.blocking_send(DriveCommand::Get { session_id, reply })
            .expect("queue in-flight actor command");

        arm_checkpoint_timer(tx, session_id, batch_id, StdDuration::from_millis(0))
            .expect("spawn checkpoint timer");

        assert!(matches!(
            rx.blocking_recv().expect("first queued command"),
            DriveCommand::Get { .. }
        ));
        assert!(matches!(
            rx.blocking_recv().expect("timer checkpoint request"),
            DriveCommand::Checkpoint {
                session_id: queued_session,
                trigger: CheckpointTrigger::Timer,
                expected_batch_id: Some(queued_batch),
                reply: None,
            } if queued_session == session_id && queued_batch == batch_id
        ));
    }

    #[test]
    fn canceled_checkpoint_reply_restores_unclaimed_committed_receipts() {
        let mut receipts = vec![pb::ObjectRecord {
            object_id: vec![0x70; 16],
            ..Default::default()
        }];
        let (reply, receiver) = oneshot::channel();
        drop(receiver);

        send_checkpoint_actor_reply(reply, pb::WriteSession::default(), &mut receipts);

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].object_id, vec![0x70; 16]);
    }

    #[test]
    fn timer_close_parks_session_and_releases_drive_bay() {
        let temp = tempfile::tempdir().expect("temp dir");
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-CHECKPOINT-IDLE",
            0x0100,
            "DRV-CHECKPOINT-IDLE",
            0x0400,
            1,
        )));
        let library = open_model_library(world);
        let snapshot = library_snapshot_cell(library.library().clone());
        let (timer_park_tx, mut timer_park_rx) = mpsc::unbounded_channel();
        let lifecycle = DrivePoolLifecycle::with_timer_park_sender(timer_park_tx);
        let reservations = Arc::new(HashMap::from([(0x0100, AtomicBool::new(true))]));
        let session_id = Uuid::from_bytes([0x73; 16]);
        lifecycle
            .sessions
            .lock()
            .expect("session lifecycle")
            .insert(
                session_id,
                MountedSession {
                    bay: 0x0100,
                    library_serial: "LIB-CHECKPOINT-IDLE".to_string(),
                    barcode: Some("CHK001L9".to_string()),
                    home_slot: Some(0x0400),
                    tape_uuid: [0x74; 16],
                    drive_uuid: Some(vec![0x75; 16]),
                },
            );
        let mut cfg = test_write_owner_config(
            temp.path().join("index.sqlite"),
            temp.path().join("audit"),
            &library,
            snapshot,
        );
        cfg.reservations = Arc::clone(&reservations);
        cfg.lifecycle = Some(lifecycle.clone());

        park_timer_closed_session(&cfg, session_id).expect("close and park session");

        assert!(!lifecycle
            .sessions
            .lock()
            .expect("session lifecycle")
            .contains_key(&session_id));
        let parked = lifecycle
            .parked
            .lock()
            .expect("parked lifecycle")
            .by_bay
            .get(&0x0100)
            .cloned()
            .expect("parked cartridge");
        assert_eq!(parked.seated.prior_session_id, Some(session_id));
        assert!(!reservations[&0x0100].load(Ordering::SeqCst));
        assert_eq!(
            timer_park_rx
                .try_recv()
                .expect("timer close arms idle-dismount scheduling"),
            parked
        );
    }

    #[tokio::test]
    async fn checkpoint_actor_deduplicates_in_batch_and_holds_until_checkpoint() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-batched-actor")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = [0x76; 16];
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "checkpoint-test".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "CHK002L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, "checkpoint-test")
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-CHECKPOINT".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-CHECKPOINT".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-21T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        };
        let mut bootstrap_block = vec![0u8; 4096];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = 4096;
        let mut world =
            VirtualWorld::single_drive("LIB-CHECKPOINT", 0x0100, "DRV-CHECKPOINT", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "CHK002L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
        cfg.checkpoint_max_objects = 2;
        cfg.checkpoint_max_age_seconds = 3600;
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

        let pool_cfg = TapePoolConfig {
            id: "checkpoint-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let (open_tx, open_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg,
                selected: SelectedTape {
                    pool_id: "checkpoint-test".to_string(),
                    tape_uuid,
                    block_size: 4096,
                    parity_config: ParityConfig::None,
                },
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some("CHK002L9".to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid),
                drive_serial: Some("DRV-CHECKPOINT".to_string()),
                reply: open_tx,
            })
            .await
            .expect("send write open");
        let session = open_rx
            .await
            .expect("open reply")
            .expect("open batched session");
        let session_id = Uuid::from_slice(&session.session_id).expect("session UUID");

        let written = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-1.bin"),
            "payload-1.bin",
            "checkpoint-caller-object-1",
            b"checkpoint payload one",
        )
        .await
        .record;
        let written_info = written
            .append_commit_info
            .as_ref()
            .expect("WRITTEN append info");
        assert_eq!(
            written_info.durability,
            pb::AppendDurability::Written as i32
        );
        assert!(written.copies.is_empty());
        assert_eq!(written_info.tape_file_number, None);
        let replay = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-1-replay.bin"),
            "payload-1.bin",
            "checkpoint-caller-object-1",
            b"checkpoint payload one",
        )
        .await;
        assert!(replay.replay, "same in-batch content must be a replay");
        assert_eq!(replay.record.object_id, written.object_id);
        let conflict = append_actor_test_file_result(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-1-conflict.bin"),
            "payload-1.bin",
            "checkpoint-caller-object-1",
            b"different checkpoint payload",
        )
        .await
        .expect_err("different in-batch content under the same caller id must conflict");
        assert_eq!(conflict.code(), tonic::Code::AlreadyExists);
        let object_id = Uuid::from_slice(&written.object_id)
            .expect("object UUID")
            .to_string();
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
        assert!(read_only
            .get_native_object(&object_id)
            .expect("query WRITTEN object")
            .is_none());
        drop(read_only);

        let (get_tx, get_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Get {
                session_id,
                reply: get_tx,
            })
            .await
            .expect("send session get");
        let pending = get_rx
            .await
            .expect("get reply")
            .expect("get batched session");
        assert_eq!(pending.pending_checkpoint_objects, 1);
        assert!(pending.pending_checkpoint_bytes > 0);
        assert!(pending.checkpoint_deadline.is_some());

        let (checkpoint_tx, checkpoint_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Explicit,
                expected_batch_id: None,
                reply: Some(checkpoint_tx),
            })
            .await
            .expect("send explicit checkpoint");
        let checkpoint = checkpoint_rx
            .await
            .expect("checkpoint reply")
            .expect("checkpoint batch");
        assert_eq!(checkpoint.committed_objects.len(), 1);
        let checkpointed_object = &checkpoint.committed_objects[0];
        assert_eq!(
            checkpointed_object
                .append_commit_info
                .as_ref()
                .expect("checkpointed append info")
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(
            checkpointed_object
                .append_commit_info
                .as_ref()
                .expect("checkpointed append info")
                .sealed_after_write,
            Some(false)
        );
        assert_eq!(checkpointed_object.copies.len(), 1);
        assert_eq!(checkpointed_object.copies[0].tape_uuid, tape_uuid);
        assert_eq!(checkpointed_object.copies[0].tape_file_number, 1);
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
        assert!(read_only
            .get_native_object(&object_id)
            .expect("query checkpointed object")
            .is_some());
        drop(read_only);

        let second = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-2.bin"),
            "payload-2.bin",
            "checkpoint-caller-object-2",
            b"checkpoint payload two",
        )
        .await
        .record;
        assert_eq!(
            second
                .append_commit_info
                .as_ref()
                .expect("second WRITTEN info")
                .durability,
            pb::AppendDurability::Written as i32
        );
        let third = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-3.bin"),
            "payload-3.bin",
            "checkpoint-caller-object-3",
            b"checkpoint payload three",
        )
        .await
        .record;
        assert_eq!(
            third
                .append_commit_info
                .as_ref()
                .expect("threshold CHECKPOINTED info")
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(
            third
                .append_commit_info
                .as_ref()
                .expect("threshold CHECKPOINTED info")
                .sealed_after_write,
            Some(false)
        );
        assert_eq!(third.copies.len(), 1);
        assert_eq!(third.copies[0].tape_uuid, tape_uuid);
        assert_eq!(third.copies[0].tape_file_number, 4);

        let (receipt_tx, receipt_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Checkpoint {
                session_id,
                trigger: CheckpointTrigger::Explicit,
                expected_batch_id: None,
                reply: Some(receipt_tx),
            })
            .await
            .expect("request automatic-checkpoint receipts");
        let receipts = receipt_rx
            .await
            .expect("receipt reply")
            .expect("retrieve automatic checkpoint receipts");
        assert_eq!(
            receipts.committed_objects.len(),
            2,
            "automatic threshold checkpoints retain their full copy set"
        );
        let threshold_copy_placements = receipts
            .committed_objects
            .iter()
            .map(|object| {
                assert_eq!(object.copies.len(), 1);
                (
                    object.copies[0].tape_uuid.clone(),
                    object.copies[0].tape_file_number,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            threshold_copy_placements,
            vec![(tape_uuid.to_vec(), 3), (tape_uuid.to_vec(), 4)]
        );
        let journal = remanence_state::FileCheckpointJournal::open(
            temp.path().join("checkpoints"),
            tape_uuid,
        )
        .expect("open checkpoint journal");
        assert_eq!(
            journal
                .last()
                .expect("replay checkpoint journal")
                .expect("checkpoint record")
                .committed_object_count,
            3
        );

        let fourth = append_actor_test_file(
            &drive_tx,
            session_id,
            temp.path().join("checkpoint-source-4.bin"),
            "payload-4.bin",
            "checkpoint-caller-object-4",
            b"checkpoint payload four",
        )
        .await
        .record;
        assert_eq!(
            fourth
                .append_commit_info
                .as_ref()
                .expect("close-trigger WRITTEN info")
                .durability,
            pb::AppendDurability::Written as i32
        );
        let fourth_object_id = Uuid::from_slice(&fourth.object_id)
            .expect("fourth object UUID")
            .to_string();

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id,
                reply: close_tx,
            })
            .await
            .expect("send close");
        let closed = close_rx
            .await
            .expect("close reply")
            .expect("close checkpointed session");
        assert_eq!(closed.session.checkpointed_objects.len(), 1);
        assert_eq!(
            closed.session.checkpointed_objects[0].object_id,
            fourth.object_id
        );
        assert_eq!(closed.session.committed_copies.len(), 1);
        assert_eq!(closed.session.committed_copies[0].tape_uuid, tape_uuid);
        assert_eq!(closed.session.committed_copies[0].tape_file_number, 6);
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open projection");
        assert!(read_only
            .get_native_object(&fourth_object_id)
            .expect("query close-checkpointed object")
            .is_some());
        assert_eq!(
            journal
                .last()
                .expect("replay close checkpoint")
                .expect("close checkpoint record")
                .committed_object_count,
            4
        );
        let records = journal.replay().expect("replay all checkpoints");
        assert_eq!(
            records
                .iter()
                .map(|record| record.checkpoint_tape_file_number)
                .collect::<Vec<_>>(),
            vec![2, 5, 7],
            "every barrier consumes one checkpoint-bootstrap tape file"
        );
        let read_only = CatalogIndex::open_read_only(&index_path).expect("open final projection");
        let tape_files = read_only
            .list_tape_files(&tape_uuid)
            .expect("list tape files");
        assert_eq!(tape_files.len(), 8);
        assert_eq!(tape_files.last().expect("last tape file").kind, "bootstrap");
        drop(read_only);

        let world = world.lock().expect("world lock");
        let tape = world.tapes.get("CHK002L9").expect("checkpoint tape");
        let checkpoint_bootstraps = tape
            .records
            .iter()
            .filter_map(|record| match record {
                Record::Block(block) => parse_bootstrap_block(block).ok(),
                Record::Filemark => None,
            })
            .filter(|payload| payload.sequence > 0)
            .collect::<Vec<_>>();
        assert_eq!(checkpoint_bootstraps.len(), 3);
        for (payload, record) in checkpoint_bootstraps.iter().zip(&records) {
            assert_eq!(payload.sequence, record.ordinal as u32);
            assert_eq!(
                payload.object_rows.len() as u64,
                record.committed_object_count,
                "checkpoint bootstrap carries every committed-prefix RAO row"
            );
            assert!(payload
                .object_rows
                .iter()
                .all(|row| row.object_id.is_some()));
            let digest = payload
                .filemark_map_digest
                .as_ref()
                .expect("checkpoint bootstrap map digest");
            assert!(!digest.is_final_map);
            assert_eq!(
                digest.tape_file_count,
                record.checkpoint_tape_file_number + 1
            );
        }
        assert_eq!(
            records.last().expect("last record").eod_lba as usize,
            tape.records.len(),
            "journal EOD names the physical boundary after the checkpoint bootstrap"
        );
    }

    /// Mount-dispatched explicit checkpoints must return catalog-projected copies after reopening
    /// a session, including a catalog replay whose append acknowledgement is already durable.
    #[tokio::test]
    async fn sequential_sessions_and_replay_return_catalog_copies_through_mount() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-sequential-batch-one")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let tape_uuid = [0x77; 16];
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "batch-one-test".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "CHK003L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, "batch-one-test")
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-BATCH-ONE".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-BATCH-ONE".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        drop(index);

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-21T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        };
        let mut bootstrap_block = vec![0u8; 4096];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
        tape.records = vec![Record::Block(bootstrap_block), Record::Filemark];
        tape.written_bytes = 4096;
        let mut world =
            VirtualWorld::single_drive("LIB-BATCH-ONE", 0x0100, "DRV-BATCH-ONE", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "CHK003L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let mut cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        cfg.checkpoint_journal_dir = temp.path().join("checkpoints");
        cfg.checkpoint_max_objects = 2;
        cfg.checkpoint_max_age_seconds = 3600;
        let library_serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([library_serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);
        let pool_cfg = TapePoolConfig {
            id: "batch-one-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let (changer_tx, _changer_rx) = mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0100, AtomicBool::new(false))]));
        let pool = DrivePool::new(
            changer_tx,
            HashMap::from([(0x0100, drive_tx.clone())]),
            reservations,
        );
        let state_index = CatalogIndex::open(&index_path).expect("open mount test catalog");
        let mut state = crate::ApiState::new_with_pool_configs(state_index, [pool_cfg.clone()]);
        state.drive_pool = Some(pool.clone());

        let mut previous_session_id = None;
        for (session_ordinal, expected_tape_file_number) in [(1u8, 1u64), (2, 3)] {
            let session_id = open_actor_test_write_session(
                &drive_tx,
                &pool_cfg,
                tape_uuid,
                library_serial.as_str(),
                "CHK003L9",
                &drive_uuid,
                "DRV-BATCH-ONE",
            )
            .await;
            assert_ne!(Some(session_id), previous_session_id);
            previous_session_id = Some(session_id);
            pool.record_session(
                session_id,
                MountedSession {
                    bay: 0x0100,
                    library_serial: library_serial.clone(),
                    barcode: Some("CHK003L9".to_string()),
                    home_slot: Some(0x0400),
                    tape_uuid,
                    drive_uuid: Some(drive_uuid.clone()),
                },
            );
            let source_path = temp
                .path()
                .join(format!("batch-one-source-{session_ordinal}.bin"));
            std::fs::write(&source_path, format!("batch-one payload {session_ordinal}"))
                .expect("write mount append source");
            let append = crate::mount::append_finish(
                &state,
                session_id,
                source_path,
                PathBuf::from(format!("payload-{session_ordinal}.bin")),
                format!("batch-one-caller-{session_ordinal}"),
                None,
            )
            .await
            .expect("append through mount dispatcher");
            let written_info = append
                .append_commit_info
                .as_ref()
                .expect("batch-of-one WRITTEN append info");
            assert_eq!(
                written_info.durability,
                pb::AppendDurability::Written as i32
            );
            assert!(append.copies.is_empty());

            let checkpoint = crate::mount::checkpoint_write_session(
                &state,
                session_id,
                CheckpointTrigger::Explicit,
            )
            .await
            .expect("explicit checkpoint through mount dispatcher");
            assert_eq!(checkpoint.committed_objects.len(), 1);
            let committed = &checkpoint.committed_objects[0];
            assert_eq!(committed.object_id, append.object_id);
            let committed_info = committed
                .append_commit_info
                .as_ref()
                .expect("batch-of-one CHECKPOINTED append info");
            assert_eq!(
                committed_info.durability,
                pb::AppendDurability::Checkpointed as i32
            );
            assert_eq!(committed.copies.len(), 1);
            assert_eq!(committed.copies[0].tape_uuid, tape_uuid);
            assert_eq!(
                committed.copies[0].tape_file_number,
                expected_tape_file_number
            );

            let (close_tx, close_rx) = oneshot::channel();
            drive_tx
                .send(DriveCommand::Close {
                    session_id,
                    reply: close_tx,
                })
                .await
                .expect("send actor test write close");
            let closed = close_rx
                .await
                .expect("actor test write close reply")
                .expect("close actor test write session");
            assert!(closed.session.checkpointed_objects.is_empty());
            assert!(closed.session.committed_copies.is_empty());
            pool.forget_session(session_id);
        }

        let replay_session_id = open_actor_test_write_session(
            &drive_tx,
            &pool_cfg,
            tape_uuid,
            library_serial.as_str(),
            "CHK003L9",
            &drive_uuid,
            "DRV-BATCH-ONE",
        )
        .await;
        pool.record_session(
            replay_session_id,
            MountedSession {
                bay: 0x0100,
                library_serial,
                barcode: Some("CHK003L9".to_string()),
                home_slot: Some(0x0400),
                tape_uuid,
                drive_uuid: Some(drive_uuid),
            },
        );
        let replay_source = temp.path().join("batch-one-replay-source.bin");
        std::fs::write(&replay_source, "batch-one payload 1").expect("write replay source");
        let replay = crate::mount::append_finish(
            &state,
            replay_session_id,
            replay_source,
            PathBuf::from("payload-1.bin"),
            "batch-one-caller-1".to_string(),
            None,
        )
        .await
        .expect("replay append through mount dispatcher");
        assert_eq!(
            replay
                .append_commit_info
                .as_ref()
                .expect("catalog replay append info")
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
        assert_eq!(replay.copies.len(), 1);
        assert_eq!(replay.copies[0].tape_file_number, 1);

        let replay_checkpoint = crate::mount::checkpoint_write_session(
            &state,
            replay_session_id,
            CheckpointTrigger::Explicit,
        )
        .await
        .expect("explicit replay checkpoint through mount dispatcher");
        assert_eq!(
            replay_checkpoint.committed_objects.len(),
            1,
            "catalog replay must remain claimable by the explicit checkpoint"
        );
        assert_eq!(
            replay_checkpoint.committed_objects[0].object_id,
            replay.object_id
        );
        assert_eq!(replay_checkpoint.committed_objects[0].copies.len(), 1);
        assert_eq!(
            replay_checkpoint.committed_objects[0].copies[0].tape_file_number,
            1
        );

        let claimed_again = crate::mount::checkpoint_write_session(
            &state,
            replay_session_id,
            CheckpointTrigger::Explicit,
        )
        .await
        .expect("repeat explicit replay checkpoint through mount dispatcher");
        assert!(
            claimed_again.committed_objects.is_empty(),
            "a replay receipt must be returned by exactly one explicit checkpoint"
        );

        let (close_tx, close_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id: replay_session_id,
                reply: close_tx,
            })
            .await
            .expect("send replay session close");
        close_rx
            .await
            .expect("replay session close reply")
            .expect("close replay session");
        pool.forget_session(replay_session_id);
    }

    struct RangeCatalogFixture {
        index: CatalogIndex,
        _temp: tempfile::TempDir,
        blocks: Vec<Vec<u8>>,
        layout: RemTarObjectLayout,
    }

    fn range_options(block_size: usize) -> RemTarObjectOptions {
        let mut opts = RemTarObjectOptions::new(
            RANGE_OBJECT_ID,
            "caller-range",
            "2026-06-16T12:00:00Z",
            "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
        );
        opts.chunk_size = block_size;
        opts
    }

    fn cataloged_payload_fixture(payload: &[u8]) -> RangeCatalogFixture {
        let opts = range_options(512);
        let files = [RemTarFile {
            path: "payload.rao",
            file_id: "payload-file",
            data: payload,
            mtime: Some("0"),
            executable: Some(false),
        }];
        let mut sink = VecBlockSink::new();
        let layout = write_rem_tar_object(&mut sink, &opts, &files).expect("write wrapped payload");
        let payload_layout = &layout.files[0];
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-range-test-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: RANGE_TAPE_UUID,
                voltag: "RANGE01".to_string(),
                block_size: opts.chunk_size as u32,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_native_object_and_committed_tape_file_bundle(
                NativeObjectProjectionInput {
                    object_id: RANGE_OBJECT_ID.to_string(),
                    caller_object_id: Some("caller-range".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(payload.len() as u64),
                    content_hash: payload_layout.file_sha256.map(|hash| hash.to_vec()),
                    metadata_hash: None,
                    created_at_utc: Some("2026-06-16T12:00:00Z".to_string()),
                },
                &[NativeObjectFileProjectionInput {
                    object_id: RANGE_OBJECT_ID.to_string(),
                    file_id: "payload-file".to_string(),
                    path: "payload.rao".to_string(),
                    size_bytes: payload.len() as u64,
                    file_sha256: payload_layout
                        .file_sha256
                        .expect("regular payload hash")
                        .to_vec(),
                    first_chunk_lba: payload_layout.first_chunk_lba.map(|lba| lba.0),
                    chunk_count: payload_layout.chunk_count,
                    mtime: Some("0".to_string()),
                    executable: Some(false),
                }],
                &[NativeObjectCopyProjectionInput {
                    object_id: RANGE_OBJECT_ID.to_string(),
                    tape_uuid: RANGE_TAPE_UUID,
                    tape_file_number: 0,
                    first_body_lba: 0,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: None,
                    stored_digest: None,
                }],
                TapeJournalIndexInput {
                    tape_uuid: RANGE_TAPE_UUID,
                    block_size: opts.chunk_size as u32,
                    scheme: None,
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 0,
                        kind: TapeFileKind::Object,
                        block_count: layout.projected_size_blocks,
                        physical_start_hint: Some(0),
                        object_id: Some(RANGE_OBJECT_ID.to_string()),
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 0,
                },
            )
            .expect("project range fixture");
        RangeCatalogFixture {
            index,
            _temp: temp,
            blocks: sink.blocks,
            layout,
        }
    }

    #[test]
    fn ranged_absolute_lba_derives_from_dense_filemark_prefix() {
        let tape_uuid = RANGE_TAPE_UUID.to_vec();
        let files = [
            TapeFileRecord {
                tape_uuid: tape_uuid.clone(),
                tape_file_number: 0,
                kind: "bootstrap".to_string(),
                block_count: 1,
                object_id: None,
                canonical_metadata_hash: None,
                canonical_metadata_hash_algorithm: None,
            },
            TapeFileRecord {
                tape_uuid: tape_uuid.clone(),
                tape_file_number: 1,
                kind: "object".to_string(),
                block_count: 10,
                object_id: Some("first".to_string()),
                canonical_metadata_hash: None,
                canonical_metadata_hash_algorithm: None,
            },
            TapeFileRecord {
                tape_uuid,
                tape_file_number: 2,
                kind: "object".to_string(),
                block_count: 3,
                object_id: Some("target".to_string()),
                canonical_metadata_hash: None,
                canonical_metadata_hash_algorithm: None,
            },
        ];
        assert_eq!(derive_physical_file_start_lba(&files, 2), Some(13));

        let mut incomplete = files.to_vec();
        incomplete.remove(1);
        assert_eq!(
            derive_physical_file_start_lba(&incomplete, 2),
            None,
            "a non-dense prefix must use the logical fallback"
        );
    }

    async fn collect_stream_chunks(
        mut rx: crate::read_core::ReadStreamReceiver,
    ) -> Result<Vec<u8>, Status> {
        let mut bytes = Vec::new();
        let mut saw_last = false;
        while let Some(item) = rx.next().await {
            let chunk = item?;
            bytes.extend_from_slice(&chunk.data);
            saw_last |= chunk.is_last;
            if chunk.is_last {
                break;
            }
        }
        assert!(saw_last, "range stream must send terminal frame");
        Ok(bytes)
    }

    #[tokio::test]
    async fn l3_read_actor_batches_are_consumed_by_staged_sender() {
        let (tx, rx) = crate::read_core::read_stream_channel(4);

        let diagnostics = stream_with_staged_read_sender_diagnostics(tx, 4, |writer, _| {
            std::io::Write::write_all(writer, b"abcdef")
                .map_err(|err| Status::internal(format!("write staged bytes: {err}")))?;
            std::io::Write::write_all(writer, b"gh")
                .map_err(|err| Status::internal(format!("write staged bytes: {err}")))?;
            Ok(())
        })
        .expect("staged read sender succeeds");
        assert_eq!(diagnostics.bytes, 8);

        let bytes = collect_stream_chunks(rx)
            .await
            .expect("collect staged read stream");
        assert_eq!(bytes, b"abcdefgh");
    }

    #[tokio::test]
    async fn staged_sender_surfaces_full_channel_stall_time() {
        let requested_chunk =
            u32::try_from(crate::read_core::READ_STREAM_CHANNEL_BYTE_BUDGET + 1).unwrap();
        let (tx, rx) = crate::read_core::read_stream_channel(requested_chunk as usize);
        let sender = tokio::task::spawn_blocking(move || {
            stream_with_staged_read_sender_diagnostics(tx, requested_chunk, |writer, _| {
                std::io::Write::write_all(writer, b"a")
                    .map_err(|err| Status::internal(format!("write first byte: {err}")))?;
                std::io::Write::write_all(writer, b"b")
                    .map_err(|err| Status::internal(format!("write second byte: {err}")))?;
                Ok(())
            })
        });
        tokio::time::sleep(StdDuration::from_millis(10)).await;
        let bytes = collect_stream_chunks(rx)
            .await
            .expect("drain staged stream");
        let diagnostics = sender
            .await
            .expect("sender task joins")
            .expect("staged sender succeeds");

        assert_eq!(bytes, b"ab");
        assert!(
            diagnostics.sender_stall >= StdDuration::from_millis(5),
            "full-channel wait must surface in restore diagnostics: {:?}",
            diagnostics.sender_stall
        );
    }

    #[tokio::test]
    async fn l3_read_sink_error_drains_without_hanging_actor_writer() {
        let (tx, rx) = crate::read_core::read_stream_channel(1);
        drop(rx);

        let err = stream_with_staged_read_sender_diagnostics(tx, 1, |writer, _| {
            for _ in 0..8 {
                std::io::Write::write_all(writer, b"x").map_err(|err| {
                    Status::internal(format!("actor observed staged sender failure: {err}"))
                })?;
            }
            Ok(())
        })
        .expect_err("closed gRPC receiver must fail staged sender");

        assert!(
            err.message().contains("read stream closed")
                || err.message().contains("staged read sender failed"),
            "sink error should be surfaced, got {err}"
        );
    }

    async fn stream_fixture_range(
        fixture: &RangeCatalogFixture,
        file_id: &str,
        start_byte: u64,
        end_byte: u64,
    ) -> Result<Vec<u8>, Status> {
        let request = file_range_read_request(
            &fixture.index,
            &RANGE_TAPE_UUID,
            RANGE_OBJECT_ID,
            file_id,
            start_byte,
            end_byte,
        )?;
        let mut source = VecBlockSource::new(fixture.blocks.clone());
        let (tx, rx) = crate::read_core::read_stream_channel(256);
        stream_file_range_from_source(
            &mut source,
            request,
            0,
            tx,
            &TapeIoConfig::default(),
            test_io_memory(),
        )?;
        collect_stream_chunks(rx).await
    }

    #[test]
    fn append_gate_poisons_session_after_failed_append() {
        let mut gate = SessionAppendGate::default();
        assert!(gate.check().is_ok(), "fresh session must accept appends");

        gate.record_failure();

        let status = gate.check().expect_err("poisoned gate must refuse");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("poisoned"));
        // Poisoning is permanent for the session's lifetime.
        assert!(gate.check().is_err());
    }

    #[test]
    fn channel_and_command_bounds_hold() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send<T: Send>() {}
        assert_send_sync::<mpsc::Sender<ChangerCommand>>();
        assert_send::<ChangerCommand>();
        assert_send_sync::<mpsc::Sender<DriveCommand>>();
        assert_send::<DriveCommand>();
        assert_send_sync::<mpsc::Sender<Result<pb::BytesChunk, Status>>>();
    }

    #[tokio::test]
    async fn changer_move_succeeds_and_publishes_snapshot_when_catalog_observation_fails() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-move-observe-failure")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        CatalogIndex::open(&index_path).expect("create catalog");
        let sqlite = rusqlite::Connection::open(&index_path).expect("open raw sqlite");
        sqlite
            .execute_batch(
                "create trigger fail_drive_observation
                 before insert on drives
                 begin
                   select raise(fail, 'injected drive catalog observation failure');
                 end;",
            )
            .expect("install observation failure trigger");
        drop(sqlite);

        let library = test_changer_library("LIB_MOVE_OBS_FAIL");
        let snapshot_cell = library_snapshot_cell(library.clone());
        let changer = open_test_changer(&library);
        let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
        let cfg = WriteOwnerConfig {
            index_path: index_path.clone(),
            report: DiscoveryReport {
                libraries: vec![library.clone()],
                warnings: Vec::new(),
            },
            policy,
            audit_dir: temp.path().join("audit"),
            audit_fsync: false,
            audit_append_lock: Arc::new(std::sync::Mutex::new(())),
            reservations: Arc::new(HashMap::new()),
            default_library_serial: Some(library.serial.clone()),
            library_snapshot: Arc::clone(&snapshot_cell),
            snapshot_miss_alarm: 1,
            managed_library_serials: Arc::new(HashSet::from([library.serial.clone()])),
            cleaning: remanence_state::CleaningConfig::default(),
            tape_io: remanence_state::TapeIoConfig::default(),
            io_memory: test_io_memory(),
            checkpoint_journal_dir: temp.path().join("checkpoints"),
            checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
            checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
            checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
            session_idle_seconds: 1800,
            lifecycle: None,
        };
        let actor = spawn_changer_actor(changer, cfg);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        actor
            .send(ChangerCommand::Move {
                src: 0x0400,
                dst: 0x0100,
                reply: reply_tx,
            })
            .await
            .expect("send move command");
        let result = reply_rx.await.expect("move reply");

        assert!(
            result.is_ok(),
            "physical move success must not be converted to failure by catalog observation: {result:?}"
        );
        let published = snapshot_cell
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let published_library = published
            .report
            .libraries
            .iter()
            .find(|candidate| candidate.serial == library.serial)
            .expect("published library");
        let bay = &published_library.drive_bays[0];
        assert!(bay.loaded, "published snapshot must include the moved tape");
        assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_MOVE"));
        assert_eq!(bay.source_slot, Some(0x0400));
        assert!(!published_library.slots[0].full);

        let alarm_key = library_snapshot_persist_alarm_key(library.serial.as_str());
        let alarm = CatalogIndex::open(&index_path)
            .expect("reopen catalog")
            .get_alarm(alarm_key.as_str())
            .expect("lookup alarm")
            .expect("observation failure alarm");
        assert_eq!(alarm.kind, "snapshot-persist-failing");
        assert_eq!(alarm.state, "open");
        assert!(
            alarm
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("injected drive catalog observation failure")),
            "alarm detail must surface the observation failure: {alarm:?}"
        );
    }

    #[test]
    fn spool_enforces_size_cap() {
        let dir = std::env::temp_dir().join(format!("remanence-spool-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create spool test dir");
        let mut spool = Spool::create(&dir, 4).expect("create spool");
        assert!(spool.path().exists());
        assert!(spool.write_chunk(b"ab").is_ok());
        assert!(spool.write_chunk(b"cde").is_err());
        let path = spool.path().to_path_buf();
        drop(spool);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn spool_removes_unfinished_file_on_drop() {
        let dir =
            std::env::temp_dir().join(format!("remanence-spool-drop-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create spool test dir");
        let path = {
            let mut spool = Spool::create(&dir, 4).expect("create spool");
            spool.write_chunk(b"ab").expect("write chunk");
            spool.path().to_path_buf()
        };
        assert!(!path.exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn process_loss_without_drop_leaves_owned_spool_for_startup_reconciliation() {
        let dir = std::env::temp_dir().join(format!(
            "remanence-spool-process-loss-test-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create spool test dir");
        let mut spool = Spool::create(&dir, 16).expect("create spool");
        spool.write_chunk(b"orphan").expect("write orphan bytes");
        let path = spool.path().to_path_buf();

        std::mem::forget(spool);

        assert!(path.exists(), "process loss bypasses Spool::drop");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 spool name");
        assert!(name.starts_with("spool-") && name.ends_with(".bin"));
        std::fs::remove_file(path).expect("remove simulated orphan");
        std::fs::remove_dir(dir).expect("remove spool test dir");
    }

    #[test]
    fn session_protos_include_drive_element_address() {
        let session_id = Uuid::from_u128(0x5E5510);
        let tape_uuid = [0xAB; 16];
        let opened_at = "2026-06-10T12:00:00Z";

        let write = session_proto(WriteSessionProtoInput {
            session_id,
            tape_uuid: &tape_uuid,
            state: pb::write_session::State::WriteSessionStateOpen,
            objects_committed: 0,
            bytes_committed: 0,
            opened_at_utc: opened_at,
            last_checkpoint_at_utc: None,
            drive_element_address: 0x0100,
            pending_batch: None,
        });
        let read = read_session_proto(
            session_id,
            &tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            opened_at,
            0x0101,
            None,
            7,
        );

        assert_eq!(write.drive_element_address, 0x0100);
        assert_eq!(read.drive_element_address, 0x0101);
        assert_eq!(read.daemon_epoch, 7);
    }

    fn resume_target_for_fixture(fixture: &RangeCatalogFixture) -> ReadResumeTarget {
        let first_chunk_lba = fixture.layout.files[0]
            .first_chunk_lba
            .expect("fixture file has a body-chunk boundary")
            .0;
        ReadResumeTarget {
            tape_uuid: RANGE_TAPE_UUID,
            object_id: RANGE_OBJECT_ID.to_string(),
            file_id: "payload-file".to_string(),
            file_boundary_byte_offset: first_chunk_lba * 512,
            expected_position_lba: Some(first_chunk_lba),
            prior_daemon_epoch: Some(11),
        }
    }

    #[test]
    fn cold_resume_relocates_returns_proof_and_mints_fresh_session() {
        let fixture = cataloged_payload_fixture(b"cold resume payload");
        let target = resume_target_for_fixture(&fixture);
        let request = file_range_read_request(
            &fixture.index,
            &target.tape_uuid,
            target.object_id.as_str(),
            target.file_id.as_str(),
            0,
            0,
        )
        .expect("resolve durable resume coordinates");

        let first_session_id = Uuid::new_v4();
        let first = read_session_proto(
            first_session_id,
            &target.tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            "2026-07-12T00:00:00Z",
            0x0101,
            None,
            target.prior_daemon_epoch.expect("prior epoch"),
        );
        drop(first);

        let mut cold_source = VecBlockSource::new(fixture.blocks.clone());
        let proof_lba = position_read_resume_from_source(&mut cold_source, request, &target)
            .expect("cold resume position proof");
        let resumed_session_id = Uuid::new_v4();
        let resumed = read_session_proto(
            resumed_session_id,
            &target.tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            "2026-07-12T00:01:00Z",
            0x0101,
            Some(proof_lba),
            12,
        );

        assert_ne!(resumed_session_id, first_session_id);
        assert_eq!(resumed.session_id, resumed_session_id.as_bytes());
        assert_eq!(resumed.daemon_epoch, 12);
        assert_eq!(
            resumed
                .position_proof
                .expect("resume open returns proof")
                .position_after_lba,
            target.expected_position_lba.expect("expected LBA")
        );
        assert!(cold_source.calls.iter().any(|call| matches!(
            call,
            VecBlockSourceCall::Space {
                kind: SpaceKind::Blocks,
                ..
            }
        )));
    }

    #[test]
    fn wrong_tape_is_rejected_before_position_even_at_matching_lba() {
        let fixture = cataloged_payload_fixture(b"wrong tape position collision");
        let actual_tape_uuid = RANGE_TAPE_UUID;
        let requested_tape_uuid = [0xCD; 16];
        let payload = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid: actual_tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-12T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        };
        let mut block = vec![0u8; 4096];
        write_bootstrap_block(&payload, &mut block).expect("write wrong-tape bootstrap");
        let mut target = resume_target_for_fixture(&fixture);
        target.tape_uuid = requested_tape_uuid;
        let request = file_range_read_request(
            &fixture.index,
            &actual_tape_uuid,
            target.object_id.as_str(),
            target.file_id.as_str(),
            0,
            0,
        )
        .expect("resolve colliding physical position");
        let expected_lba = target.expected_position_lba.expect("expected LBA");
        let mut blocks = vec![block];
        blocks.resize_with(expected_lba as usize + 1, || vec![0u8; 512]);
        let mut source = VecBlockSource::new(blocks);

        let error = verify_and_position_read_resume_from_source(&mut source, request, &target)
            .expect_err("wrong tape must fail before trusting its matching LBA");

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("tape identity mismatch"));
        assert_eq!(
            source.cursor(),
            1,
            "identity read stops at an LBA from which the expected proof was reachable"
        );
        assert!(source.calls.iter().all(|call| !matches!(
            call,
            VecBlockSourceCall::Space { .. } | VecBlockSourceCall::Position
        )));
    }

    #[test]
    fn resume_rejects_mid_file_offset_without_positioning() {
        let fixture = cataloged_payload_fixture(b"file-boundary payload");
        let mut target = resume_target_for_fixture(&fixture);
        target.file_boundary_byte_offset += 1;
        let request = file_range_read_request(
            &fixture.index,
            &target.tape_uuid,
            target.object_id.as_str(),
            target.file_id.as_str(),
            0,
            0,
        )
        .expect("resolve durable resume coordinates");
        let mut source = VecBlockSource::new(fixture.blocks.clone());

        let error = position_read_resume_from_source(&mut source, request, &target)
            .expect_err("mid-file resume must fail");

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
        assert!(error.message().contains("file boundary"));
        assert!(
            source.calls.is_empty(),
            "invalid offset must not move the tape"
        );
    }

    #[test]
    fn serialized_resume_token_contains_no_session_id() {
        let persisted_session_id = [0xEE; 16];
        let token = pb::ReadResumeTarget {
            tape_uuid: RANGE_TAPE_UUID.to_vec(),
            object_id: Uuid::parse_str(RANGE_OBJECT_ID)
                .expect("object UUID")
                .as_bytes()
                .to_vec(),
            file_id: b"payload-file".to_vec(),
            file_boundary_byte_offset: 1024,
            expected_position_lba: Some(17),
            daemon_epoch: Some(41),
        };

        let encoded = token.encode_to_vec();

        assert!(
            !encoded
                .windows(persisted_session_id.len())
                .any(|window| window == persisted_session_id),
            "the durable resume token must not serialize a session id"
        );
    }

    #[test]
    fn pool_write_status_maps_nested_input_and_capacity_errors() {
        let invalid = status_from_pool_write_error(PoolWriteError::Streaming(
            StreamingError::InvalidInput("bad archive path".to_string()),
        ));
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        let invalid_prefix = status_from_pool_write_error(PoolWriteError::Streaming(
            StreamingError::InvalidXattrNamespacePrefix {
                prefix: "s".to_string(),
            },
        ));
        assert_eq!(invalid_prefix.code(), tonic::Code::InvalidArgument);

        let exhausted = status_from_pool_write_error(PoolWriteError::Parity(
            ParityError::ObjectTooLargeForEmptyTape {
                projected_object_blocks: 10,
                empty_tape_usable_blocks: 9,
                required_reserve_blocks: 1,
            },
        ));
        assert_eq!(exhausted.code(), tonic::Code::ResourceExhausted);
    }

    #[test]
    fn session_close_snapshot_success_clears_snapshot_persist_alarm() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-snapshot-alarm")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let drive_uuid = Uuid::new_v4().as_bytes().to_vec();
        let condition_key = snapshot_persist_alarm_key(&drive_uuid);
        index
            .raise_alarm(
                condition_key.as_str(),
                "snapshot-persist-failing",
                "warning",
                Some("{\"misses\":3}"),
            )
            .expect("raise snapshot alarm");

        index
            .clear_alarm(condition_key.as_str())
            .expect("clear snapshot alarm");

        assert_eq!(
            index
                .get_alarm(condition_key.as_str())
                .expect("alarm lookup")
                .expect("alarm row")
                .state,
            "cleared"
        );
    }

    #[test]
    fn failure_snapshots_are_keyed_by_failing_session() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-failure-snapshots")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-FAIL-SNAPSHOT".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-FAIL-SNAPSHOT".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-18T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let mut world =
            VirtualWorld::single_drive("LIB-FAIL-SNAPSHOT", 0x0100, "DRV-FAIL-SNAPSHOT", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "FAIL001L9", Some(0x0400), VirtualTape::default());
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot);
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let mut drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let append_session = Uuid::new_v4();
        let read_session = Uuid::new_v4();
        let tape_uuid = [0x77; 16];
        let mut misses = 0;

        record_session_snapshot(
            &mut index,
            &cfg,
            &mut drive,
            Some(drive_uuid.clone()),
            append_session,
            tape_uuid,
            "append-failure",
            &mut misses,
        );
        record_session_snapshot(
            &mut index,
            &cfg,
            &mut drive,
            Some(drive_uuid.clone()),
            read_session,
            tape_uuid,
            "read-failure",
            &mut misses,
        );

        let rows = index
            .list_drive_health_snapshots(&drive_uuid)
            .expect("list failure snapshots");
        let append_session = append_session.to_string();
        let read_session = read_session.to_string();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].trigger, "append-failure");
        assert_eq!(rows[0].session_id.as_deref(), Some(append_session.as_str()));
        assert_eq!(rows[1].trigger, "read-failure");
        assert_eq!(rows[1].session_id.as_deref(), Some(read_session.as_str()));
    }

    #[tokio::test]
    async fn induced_append_and_read_failures_persist_session_snapshots() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-induced-failure-snapshots")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        let tape_uuid = [0x78; 16];
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "failure-test".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "FAIL002L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, "failure-test")
            .expect("assign pool");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-INDUCED-FAIL".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-INDUCED-FAIL".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-18T00:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;

        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-07-18T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: 4096,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        };
        let mut bootstrap_block = vec![0u8; 4096];
        write_bootstrap_block(&bootstrap, &mut bootstrap_block).expect("encode bootstrap");
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, 4096);
        tape.records = vec![
            Record::Block(bootstrap_block),
            Record::Filemark,
            Record::Filemark,
        ];
        tape.written_bytes = 4096;
        let mut world =
            VirtualWorld::single_drive("LIB-INDUCED-FAIL", 0x0100, "DRV-INDUCED-FAIL", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "FAIL002L9", Some(0x0400), tape);
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path.clone(), audit_dir, &library, snapshot);
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");
        let drive_tx = spawn_drive_actor(0x0100, drive, cfg);

        let (open_read_tx, open_read_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load: false,
                library_serial: serial.clone(),
                barcode: Some("FAIL002L9".to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-INDUCED-FAIL".to_string()),
                resume_target: None,
                daemon_epoch: 1,
                reply: open_read_tx,
            })
            .await
            .expect("send read open");
        let read_session = open_read_rx
            .await
            .expect("read open reply")
            .expect("open read session");
        let read_session_id =
            Uuid::from_slice(&read_session.session_id).expect("read session UUID");
        let (chunk_tx, mut chunk_rx) = crate::read_core::read_stream_channel(4096);
        drive_tx
            .send(DriveCommand::ReadFile {
                session_id: read_session_id,
                object_id: Uuid::new_v4().to_string(),
                file_id: Vec::new(),
                stream_chunk_bytes: 4096,
                chunk_tx,
            })
            .await
            .expect("send failing read");
        chunk_rx
            .next()
            .await
            .expect("read failure item")
            .expect_err("missing object induces read failure");
        let (close_read_tx, close_read_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::CloseRead {
                session_id: read_session_id,
                reply: close_read_tx,
            })
            .await
            .expect("send read close");
        close_read_rx
            .await
            .expect("read close reply")
            .expect("close read session");

        let pool_cfg = TapePoolConfig {
            id: "failure-test".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            block_size_bytes: 4096,
            min_object_size_bytes: 0,
        };
        let (open_write_tx, open_write_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::OpenWrite {
                pool_cfg: pool_cfg.clone(),
                selected: SelectedTape {
                    pool_id: "failure-test".to_string(),
                    tape_uuid,
                    block_size: 4096,
                    parity_config: ParityConfig::None,
                },
                needs_drive_load: false,
                library_serial: serial,
                barcode: Some("FAIL002L9".to_string()),
                source_slot: None,
                drive_uuid: Some(drive_uuid.clone()),
                drive_serial: Some("DRV-INDUCED-FAIL".to_string()),
                reply: open_write_tx,
            })
            .await
            .expect("send write open");
        let write_session = open_write_rx
            .await
            .expect("write open reply")
            .expect("open write session");
        let write_session_id =
            Uuid::from_slice(&write_session.session_id).expect("write session UUID");
        let spool = temp.path().join("invalid-archive-path.spool");
        std::fs::write(&spool, b"induced append failure").expect("write spool");
        let (append_tx, append_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::AppendFinish {
                session_id: write_session_id,
                source: crate::WriteObjectSource::Path(spool),
                archive_path: PathBuf::from("../invalid"),
                caller_object_id: "failure-test-object".to_string(),
                expected_content_sha256: None,
                live_write_counter: None,
                reply: append_tx,
            })
            .await
            .expect("send failing append");
        append_rx
            .await
            .expect("append reply")
            .expect_err("invalid archive path induces append failure");

        let close_command_start = world.lock().expect("world lock").command_log.len();
        let (close_write_tx, close_write_rx) = oneshot::channel();
        drive_tx
            .send(DriveCommand::Close {
                session_id: write_session_id,
                reply: close_write_tx,
            })
            .await
            .expect("send write close");
        let close_reply = close_write_rx
            .await
            .expect("write close reply")
            .expect("close write session");
        assert_eq!(
            close_reply.session.state,
            pb::write_session::State::WriteSessionStateClosed as i32
        );
        assert_eq!(
            close_reply.diagnostics.filemark_write_drain,
            StdDuration::ZERO
        );
        assert_eq!(
            close_reply.diagnostics.catalog_journal_fsync,
            StdDuration::ZERO
        );
        assert_eq!(close_reply.diagnostics.rewind, StdDuration::ZERO);
        assert_eq!(close_reply.diagnostics.ssc_unload, StdDuration::ZERO);
        let close_opcodes = world
            .lock()
            .expect("world lock")
            .command_log
            .iter()
            .skip(close_command_start)
            .map(|command| command.opcode)
            .collect::<Vec<_>>();
        assert!(
            !close_opcodes.contains(&0x1b),
            "session close must leave the cartridge seated: {close_opcodes:?}"
        );
        assert!(
            !close_opcodes.contains(&0x01),
            "diagnostics must not add a separate REWIND command: {close_opcodes:?}"
        );

        let check = CatalogIndex::open(&index_path).expect("reopen catalog");
        let rows = check
            .list_drive_health_snapshots(&drive_uuid)
            .expect("list snapshots");
        let read_session_text = read_session_id.to_string();
        let write_session_text = write_session_id.to_string();
        assert!(
            rows.iter().any(|row| {
                row.trigger == "read-failure"
                    && row.session_id.as_deref() == Some(read_session_text.as_str())
            }),
            "missing read-failure snapshot: {rows:#?}"
        );
        assert!(
            rows.iter().any(|row| {
                row.trigger == "append-failure"
                    && row.session_id.as_deref() == Some(write_session_text.as_str())
            }),
            "missing append-failure snapshot: {rows:#?}"
        );
    }

    #[test]
    fn fixed_no_compression_config_preserves_drive_reported_fields() {
        let current = TapeConfig {
            block_size: BlockSize::Variable,
            compression: true,
            max_block_size_bytes: 8 * 1024 * 1024,
            write_protected: true,
            worm: WormMediaState::Unknown,
        };

        let prepared = fixed_no_compression_config(current, 4096);

        assert_eq!(prepared.block_size, BlockSize::Fixed { size_bytes: 4096 });
        assert!(!prepared.compression);
        assert_eq!(prepared.max_block_size_bytes, current.max_block_size_bytes);
        assert_eq!(prepared.write_protected, current.write_protected);
        assert_eq!(prepared.worm, current.worm);
    }

    #[test]
    fn prepare_drive_for_read_sets_catalog_fixed_block_size_and_rejects_missing_geometry() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-read-mode-prepare-")
            .tempdir()
            .expect("tempdir");
        let mut index =
            CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open test index");
        let tape_uuid = [0x44; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "DATA044L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");

        let mut world = VirtualWorld::single_drive("LIB-READ-PREP", 0x0100, "DRV-READ", 0x0400, 1);
        world.put_tape_in_drive(
            0x0100,
            "DATA044L9",
            Some(0x0400),
            VirtualTape::empty(1024 * 1024, 1024),
        );
        let world = Arc::new(Mutex::new(world));
        let mut library = open_model_library(Arc::clone(&world));
        let serial = library.library().serial.clone();
        let policy = remanence_library::StaticAllowlist::new([serial.as_str()]);
        let mut drive = library
            .open_drive(0x0100, &policy)
            .expect("open model drive");

        prepare_drive_for_read(&index, &mut drive, &tape_uuid, Uuid::new_v4())
            .expect("prepare fixed read mode");

        assert_eq!(
            world
                .lock()
                .expect("world lock")
                .tapes
                .get("DATA044L9")
                .expect("model tape")
                .block_size,
            4096
        );

        let missing_tape_uuid = [0x45; 16];
        let error = prepare_drive_for_read(&index, &mut drive, &missing_tape_uuid, Uuid::new_v4())
            .expect_err("missing catalog geometry must fail closed");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("catalog row is missing"));
    }

    #[tokio::test]
    async fn empty_file_id_ranges_are_payload_relative_real_bytes() {
        let payload: Vec<u8> = (0..1600)
            .map(|value| u8::try_from(value % 251).unwrap())
            .collect();
        let fixture = cataloged_payload_fixture(&payload);
        assert!(fixture.layout.files[0].first_chunk_lba.is_some());

        let mid = stream_fixture_range(&fixture, "", 400, 900)
            .await
            .expect("mid range");
        assert_eq!(mid, payload[400..900]);

        let to_eof = stream_fixture_range(&fixture, "", 1200, payload.len() as u64)
            .await
            .expect("range to eof");
        assert_eq!(to_eof, payload[1200..]);

        let empty = stream_fixture_range(&fixture, "", 777, 777)
            .await
            .expect("empty range");
        assert!(empty.is_empty());

        let whole = stream_fixture_range(&fixture, "", 0, 0)
            .await
            .expect("whole payload range");
        assert_eq!(whole, payload);
    }

    #[tokio::test]
    async fn member_scoped_ranges_still_resolve_file_id() {
        let payload = b"member scoped range bytes".to_vec();
        let fixture = cataloged_payload_fixture(&payload);

        let got = stream_fixture_range(&fixture, "payload-file", 7, 13)
            .await
            .expect("member range");

        assert_eq!(got, b"scoped");
    }

    #[test]
    fn frequency_cap_alarm_triggers_on_recent_run() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-cap")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-CAP".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("mainlib".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T04:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;

        let run = index
            .begin_clean_run(&drive_uuid, "mainlib", "periodic", None)
            .expect("begin run");
        index
            .terminalize_clean_run(run.run_id.as_str(), "done", Some("{\"stage\":\"done\"}"))
            .expect("finish run");

        assert!(
            cleaning_too_soon(&index, &drive_uuid, Duration::seconds(0), 1)
                .expect("frequency check"),
            "one completed run in the current week must hit the weekly cap"
        );
    }

    #[test]
    fn cleaning_alarm_failure_rolls_back_fence_before_error() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-alarm-fail")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let db = rusqlite::Connection::open(&index_path).expect("open sqlite");
        db.execute_batch(
            "create trigger fail_alarm_insert
             before insert on alarms
             begin
               select raise(fail, 'injected alarm failure');
             end;",
        )
        .expect("install alarm failure trigger");
        drop(db);

        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-ALARM".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-ALARM".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:00:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;

        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-ALARM",
            0x0100,
            "DRV-ALARM",
            0x0400,
            1,
        )));
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot_cell = library_snapshot_cell(library.library().clone());
        let audit_dir = temp.path().join("audit");
        std::fs::create_dir_all(&audit_dir).expect("create audit dir");
        let cfg = test_write_owner_config(index_path, audit_dir, &library, snapshot_cell);
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("alarm failure must fail cleaning");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(
            !index
                .get_drive_by_uuid(&drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "fence must be rolled back when alarm insertion fails"
        );
    }

    #[test]
    fn periodic_cleaning_defers_on_busy_drive_and_now_fences_after_session_end() {
        let busy_world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-POLICY",
            0x0100,
            "DRV-POLICY",
            0x0400,
            1,
        )));
        {
            let mut world = busy_world.lock().expect("world lock");
            world.put_tape_in_drive(0x0100, "DATA-BUSY", None, VirtualTape::default());
            world.put_tape_in_slot(
                0x0400,
                "CLN-POLICY",
                VirtualTape {
                    cleaning_cart: true,
                    ..VirtualTape::default()
                },
            );
        }
        let busy_library = open_model_library(std::sync::Arc::clone(&busy_world));
        let busy_snapshot = library_snapshot_cell(busy_library.library().clone());
        let busy_temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-periodic")
            .tempdir()
            .expect("temp dir");
        let busy_index_path = busy_temp.path().join("rem-state.sqlite");
        let mut busy_index = CatalogIndex::open(&busy_index_path).expect("open");
        let busy_drive_uuid = busy_index
            .observe_drive(DriveObservationInput {
                serial: "DRV-POLICY".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-POLICY".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:10:00Z".to_string()),
            })
            .expect("observe busy drive")
            .drive_uuid;
        let cln_uuid = [0x91; 16];
        busy_index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: cln_uuid,
                voltag: "CLN-POLICY".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision cleaning tape");
        busy_index
            .set_tape_kind(&cln_uuid, "cleaning")
            .expect("mark cleaning cart")
            .expect("cleaning tape row");
        busy_index
            .set_tape_cleaning_state(&cln_uuid, "ok")
            .expect("mark cleaning cart state")
            .expect("cleaning tape row");
        let busy_cfg = test_write_owner_config(
            busy_index_path.clone(),
            busy_temp.path().join("audit"),
            &busy_library,
            busy_snapshot,
        );
        std::fs::create_dir_all(&busy_cfg.audit_dir).expect("create audit dir");

        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = busy_library;
        assert!(
            run_cleaning_sequence(
                &mut busy_index,
                &busy_cfg,
                &handle,
                &mut library,
                &busy_drive_uuid,
                "periodic",
            )
            .is_ok(),
            "periodic cleaning must defer while the drive is busy"
        );
        assert!(
            !busy_index
                .get_drive_by_uuid(&busy_drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "periodic defer must not fence the drive"
        );
        assert!(
            busy_index
                .get_active_clean_run_by_drive(&busy_drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "periodic defer must not create a clean run"
        );

        let now_world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-NOW", 0x0100, "DRV-NOW", 0x0400, 1,
        )));
        {
            let mut world = now_world.lock().expect("world lock");
            world.put_tape_in_drive(0x0100, "DATA-NOW", None, VirtualTape::default());
            world.put_tape_in_slot(
                0x0400,
                "CLN-NOW",
                VirtualTape {
                    cleaning_cart: true,
                    ..VirtualTape::default()
                },
            );
        }
        let now_library = open_model_library(std::sync::Arc::clone(&now_world));
        let now_snapshot = library_snapshot_cell(now_library.library().clone());
        let now_temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-now")
            .tempdir()
            .expect("temp dir");
        let now_index_path = now_temp.path().join("rem-state.sqlite");
        let mut now_index = CatalogIndex::open(&now_index_path).expect("open");
        let now_drive_uuid = now_index
            .observe_drive(DriveObservationInput {
                serial: "DRV-NOW".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-NOW".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:11:00Z".to_string()),
            })
            .expect("observe now drive")
            .drive_uuid;
        let now_uuid = [0x92; 16];
        now_index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: now_uuid,
                voltag: "CLN-NOW".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision cleaning tape");
        now_index
            .set_tape_kind(&now_uuid, "cleaning")
            .expect("mark cleaning cart")
            .expect("cleaning tape row");
        now_index
            .set_tape_cleaning_state(&now_uuid, "ok")
            .expect("mark cleaning cart state")
            .expect("cleaning tape row");
        let now_cfg = test_write_owner_config(
            now_index_path.clone(),
            now_temp.path().join("audit"),
            &now_library,
            now_snapshot,
        );
        std::fs::create_dir_all(&now_cfg.audit_dir).expect("create audit dir");

        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = now_library;
        let err = run_cleaning_sequence(
            &mut now_index,
            &now_cfg,
            &handle,
            &mut library,
            &now_drive_uuid,
            "now",
        )
        .expect_err("now cleaning should fence and then hit the busy-drive path");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            now_index
                .get_drive_by_uuid(&now_drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "now cleaning must fence the drive"
        );
    }

    #[test]
    fn no_cln_cart_branch_unfences_drive_and_raises_alarm() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-NOCART",
            0x0100,
            "DRV-NOCART",
            0x0400,
            1,
        )));
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-no-cart")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-NOCART".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-NOCART".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:20:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let cfg = test_write_owner_config(
            index_path.clone(),
            temp.path().join("audit"),
            &library,
            snapshot,
        );
        std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("no-cart branch must stop cleaning");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            !index
                .get_drive_by_uuid(&drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "no-cart branch must leave the drive unfenced"
        );
        assert!(
            index
                .get_alarm(format!("no-cln-cart:{}", library.library().serial).as_str())
                .expect("alarm lookup")
                .is_some_and(|alarm| alarm.state == "open"),
            "no-cart branch must raise the standing alarm"
        );
        assert!(
            index
                .get_active_clean_run_by_drive(&drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "no-cart branch must not leave an active clean run"
        );
    }

    #[test]
    fn cleaning_frequency_cap_refuses_before_fence_or_run() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-CAP", 0x0100, "DRV-CAP", 0x0400, 1,
        )));
        {
            let mut world = world.lock().expect("world lock");
            world.put_tape_in_slot(
                0x0400,
                "CLN-CAP",
                VirtualTape {
                    cleaning_cart: true,
                    ..VirtualTape::default()
                },
            );
        }
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-frequency-cap")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-CAP".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-CAP".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:30:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let completed = index
            .begin_clean_run(&drive_uuid, "LIB-CAP", "now", None)
            .expect("begin prior run");
        index
            .terminalize_clean_run(
                completed.run_id.as_str(),
                "done",
                Some("{\"stage\":\"done\"}"),
            )
            .expect("finish prior run");
        let cfg = WriteOwnerConfig {
            cleaning: remanence_state::CleaningConfig {
                weekly_cap: 1,
                min_interval: "0s".to_string(),
                ..remanence_state::CleaningConfig::default()
            },
            ..test_write_owner_config(
                index_path.clone(),
                temp.path().join("audit"),
                &library,
                snapshot,
            )
        };
        std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("frequency cap must reject");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            !index
                .get_drive_by_uuid(&drive_uuid)
                .expect("drive lookup")
                .expect("drive row")
                .fenced,
            "frequency cap must not fence the drive"
        );
        assert!(
            index
                .get_active_clean_run_by_drive(&drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "frequency cap must not leave an active clean run"
        );
        assert!(
            index
                .get_alarm(
                    format!(
                        "drive-cleaning-abnormal-frequency:{}",
                        crate::bytes_to_hex(&drive_uuid)
                    )
                    .as_str()
                )
                .expect("alarm lookup")
                .is_some_and(|alarm| alarm.state == "open"),
            "frequency cap must raise the abnormal-frequency alarm"
        );
    }

    #[test]
    fn inventory_only_cleaning_cart_is_recognized_before_fast_eject() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-FAST", 0x0100, "DRV-FAST", 0x0400, 1,
        )));
        {
            let mut world = world.lock().expect("world lock");
            world.put_tape_in_slot(
                0x0400,
                "CLNU01L9",
                VirtualTape {
                    cleaning_cart: true,
                    cleaning_cart_expired: true,
                    ..VirtualTape::default()
                },
            );
        }
        let library = open_model_library(std::sync::Arc::clone(&world));
        let snapshot = library_snapshot_cell(library.library().clone());
        let temp = tempfile::Builder::new()
            .prefix("remanence-cleaning-fast-eject")
            .tempdir()
            .expect("temp dir");
        let index_path = temp.path().join("rem-state.sqlite");
        let mut index = CatalogIndex::open(&index_path).expect("open");
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: "DRV-FAST".to_string(),
                identity_source: "DvcidAndInquiry".to_string(),
                vendor: Some("IBM".to_string()),
                product: Some("ULT3580".to_string()),
                firmware_rev: Some("A1".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("LIB-FAST".to_string()),
                element_address: Some(0x0100),
                observed_at_utc: Some("2026-07-04T05:40:00Z".to_string()),
            })
            .expect("observe drive")
            .drive_uuid;
        let cfg = test_write_owner_config(
            index_path.clone(),
            temp.path().join("audit"),
            &library,
            snapshot,
        );
        std::fs::create_dir_all(&cfg.audit_dir).expect("create audit dir");
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let mut library = library;
        let err =
            run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
                .expect_err("fast-eject cart must be rejected");
        assert_ne!(err.code(), tonic::Code::Ok);
        assert!(
            !err.message().contains("no eligible cleaning cartridge"),
            "inventory-only cart must reach the physical cleaning path: {err}"
        );
        let cart = index
            .get_tape_by_voltag("CLNU01L9")
            .expect("cleaning cart lookup")
            .expect("inventory cart registered");
        assert_eq!(cart.kind, "cleaning");
        assert_eq!(
            index
                .get_tape_cleaning_state(cart.tape_uuid.as_slice())
                .expect("cleaning state lookup")
                .flatten()
                .as_deref(),
            Some("expired")
        );
        assert!(
            index
                .get_active_clean_run_by_drive(&drive_uuid)
                .expect("active run lookup")
                .is_none(),
            "fast-eject path should not leave the selected clean run active"
        );
    }

    #[tokio::test]
    async fn encrypted_payload_is_served_opaque_and_decrypted_client_side() {
        let mut encrypted_opts = RemTarObjectOptions::new(
            "cccccccc-cccc-cccc-cccc-cccccccccccc",
            "caller-encrypted",
            "2026-06-16T12:00:00Z",
            "dddddddd-dddd-dddd-dddd-dddddddddddd",
        );
        encrypted_opts.chunk_size = 512;
        let secret: Vec<u8> = (0..1800)
            .map(|value| u8::try_from((value * 7) % 251).unwrap())
            .collect();
        let encrypted_files = [RemTarFile {
            path: "secret.bin",
            file_id: "secret-file",
            data: secret.as_slice(),
            mtime: Some("0"),
            executable: Some(false),
        }];
        let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
        let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
        let recipients = vec![
            primary.public_key(0).unwrap(),
            recovery.public_key(1).unwrap(),
        ];
        let mut encrypted_sink = VecBlockSink::new();
        let encrypted_report = write_encrypted_rao_object(
            &mut encrypted_sink,
            &encrypted_opts,
            &encrypted_files,
            &recipients,
        )
        .expect("write encrypted payload");
        let encrypted_payload: Vec<u8> = encrypted_sink.blocks.iter().flatten().copied().collect();
        assert_eq!(&encrypted_payload[0..4], b"RAO1");

        let fixture = cataloged_payload_fixture(&encrypted_payload);
        let header = stream_fixture_range(&fixture, "", 0, 64)
            .await
            .expect("opaque header range");
        assert_eq!(header, encrypted_payload[0..64]);

        let opaque = stream_fixture_range(&fixture, "", 0, encrypted_payload.len() as u64)
            .await
            .expect("opaque encrypted payload");
        assert_eq!(opaque, encrypted_payload);

        let opened = read_encrypted_rao_file_range_to_vec(
            &opaque,
            &primary,
            encrypted_report.plaintext_layout.files[0].first_chunk_lba,
            secret.len() as u64,
            300,
            333,
        )
        .expect("client-side decrypt range");
        assert_eq!(opened.bytes, secret[300..633]);
    }

    #[tokio::test]
    async fn invalid_payload_ranges_return_typed_status() {
        let payload = b"short payload".to_vec();
        let fixture = cataloged_payload_fixture(&payload);

        let past_eof = stream_fixture_range(&fixture, "", 99, 100)
            .await
            .expect_err("past EOF must fail");
        assert_eq!(past_eof.code(), tonic::Code::InvalidArgument);

        let overflow_request = file_range_read_request(
            &fixture.index,
            &RANGE_TAPE_UUID,
            RANGE_OBJECT_ID,
            "",
            u64::MAX - 1,
            u64::MAX,
        )
        .expect("request builder allows planner to catch arithmetic overflow");
        let mut source = VecBlockSource::new(fixture.blocks.clone());
        let (tx, _rx) = crate::read_core::read_stream_channel(8);
        let overflow = stream_file_range_from_source(
            &mut source,
            overflow_request,
            0,
            tx,
            &TapeIoConfig::default(),
            test_io_memory(),
        )
        .expect_err("overflow must fail");
        assert_eq!(overflow.code(), tonic::Code::InvalidArgument);

        let reversed =
            file_range_read_request(&fixture.index, &RANGE_TAPE_UUID, RANGE_OBJECT_ID, "", 5, 4)
                .expect_err("end before start must fail");
        assert_eq!(reversed.code(), tonic::Code::InvalidArgument);
    }
}
