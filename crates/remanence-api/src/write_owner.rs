//! Drive/changer actor pool for Layer 5 read and write sessions.
//!
//! Phase 3b reserves individual drive bays for sessions while keeping
//! reconcile and robotics pool-exclusive.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use ciborium::value::Value as CborValue;
use remanence_format::error::FormatError;
use remanence_format::model::BodyLba;
use remanence_library::{
    BlockSize, BlockSource, ChangerHandle, DiscoveryReport, DriveHandle, DriveHandleSink,
    DriveHandleSource, StaticAllowlist, TapeConfig,
};
use remanence_parity::{
    scan_reconstruct_filemark_map, DriveHandleRawSource, FilemarkMap, ParityError, TapeFileEntry,
    TapeFileKind,
};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex, CleaningConfig,
    DriveHealthSnapshotInput, DriveHealthSnapshotRecord, FileAuditLog, NativeObjectFileRecord,
    SourceLayer, TapePoolConfig,
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

pub(crate) const SPOOL_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

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
    OpenWrite {
        pool_cfg: TapePoolConfig,
        selected: SelectedTape,
        needs_drive_load: bool,
        library_serial: String,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    OpenRead {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        library_serial: String,
        drive_uuid: Option<Vec<u8>>,
        drive_serial: Option<String>,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    Unload {
        reply: oneshot::Sender<Result<(), Status>>,
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
        spool_path: PathBuf,
        archive_path: PathBuf,
        caller_object_id: String,
        expected_content_sha256: Option<[u8; 32]>,
        live_write_counter: Option<Arc<crate::DriveByteCounters>>,
        reply: oneshot::Sender<Result<pb::ObjectRecord, Status>>,
    },
    Close {
        session_id: Uuid,
        unload_before_close: bool,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    Abort {
        session_id: Uuid,
        unload_before_close: bool,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
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
        chunk_tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
    },
    ReadObjectRange {
        session_id: Uuid,
        object_id: String,
        file_id: String,
        start_byte: u64,
        end_byte: u64,
        stream_chunk_bytes: u32,
        chunk_tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
    },
    CloseRead {
        session_id: Uuid,
        unload_before_close: bool,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    GetRead {
        session_id: Uuid,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
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

#[derive(Clone)]
pub(crate) struct DrivePool {
    changer_tx: mpsc::Sender<ChangerCommand>,
    drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>,
    reservations: Arc<HashMap<u16, AtomicBool>>,
    sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    tape_reservations: Arc<Mutex<HashSet<TapeUuid>>>,
}

impl DrivePool {
    pub(crate) fn new(
        changer_tx: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
    ) -> Self {
        Self {
            changer_tx,
            drives: Arc::new(drives),
            reservations,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            tape_reservations: Arc::new(Mutex::new(HashSet::new())),
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

    pub(crate) fn is_tape_mounted(&self, tape_uuid: &TapeUuid) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .any(|mounted| &mounted.tape_uuid == tape_uuid)
            || self
                .tape_reservations
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .contains(tape_uuid)
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
            .open(&path)?;
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
                "spool size cap exceeded",
            ));
        }
        self.file.write_all(bytes)?;
        self.written = next;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> std::io::Result<PathBuf> {
        self.file.flush()?;
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
    std::thread::Builder::new()
        .name(format!("rem-drive-actor-{bay:04x}"))
        .spawn(move || drive_loop(bay, &mut drive, cfg, rx))
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
                            changer.library().serial.as_str(),
                        ),
                        Err(err) => record_library_observation_failure(
                            &mut index,
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
            DriveCommand::OpenWrite {
                pool_cfg,
                selected,
                needs_drive_load,
                library_serial,
                drive_uuid,
                drive_serial,
                reply,
            } => handle_drive_open_write(
                bay,
                &mut index,
                &cfg,
                &mut rx,
                drive,
                &mut snapshot_misses,
                OpenWriteActorRequest {
                    pool_cfg,
                    selected,
                    needs_drive_load,
                    library_serial,
                    drive_uuid,
                    drive_serial,
                    reply,
                },
            ),
            DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load,
                library_serial,
                drive_uuid,
                drive_serial,
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
                    drive_uuid,
                    drive_serial,
                    reply,
                },
            ),
            DriveCommand::Unload { reply } => {
                let result = drive
                    .unload()
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
            DriveCommand::AppendFinish {
                reply, spool_path, ..
            } => {
                let _ = std::fs::remove_file(spool_path);
                let _ = reply.send(Err(Status::failed_precondition("no active write session")));
            }
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
            DriveCommand::AppendFinish {
                reply, spool_path, ..
            } => {
                let _ = std::fs::remove_file(spool_path);
                let _ = reply.send(Err(Status::internal(message.clone())));
            }
            DriveCommand::Close { reply, .. }
            | DriveCommand::Abort { reply, .. }
            | DriveCommand::Get { reply, .. } => {
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
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
    reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
}

struct OpenReadActorRequest {
    tape_uuid: [u8; 16],
    needs_drive_load: bool,
    library_serial: String,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
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
    _cfg: &WriteOwnerConfig,
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
    Ok(snapshot)
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
    let Some(drive_uuid) = drive_uuid else {
        return;
    };
    match collect_drive_health_snapshot(
        index,
        cfg,
        drive,
        DriveSnapshotRequest {
            drive_uuid: drive_uuid.clone(),
            trigger: "session-close",
            session_id: Some(session_id),
            tape_uuid: Some(tape_uuid),
        },
    ) {
        Ok(_) => {
            clear_snapshot_persist_alarm(index, drive_uuid.as_slice());
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
                if let Err(alarm_err) = index.raise_alarm(
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

fn clear_snapshot_persist_alarm(index: &mut CatalogIndex, drive_uuid: &[u8]) {
    let condition_key = snapshot_persist_alarm_key(drive_uuid);
    if let Err(err) = index.clear_alarm(condition_key.as_str()) {
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
}

impl SessionAppendGate {
    fn check(&self) -> Result<(), Status> {
        if self.poisoned {
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
}

fn handle_drive_open_write(
    bay: u16,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
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
        drive_uuid,
        drive_serial,
        reply,
    } = request;
    let actor_open_started = Instant::now();
    let session_id = Uuid::new_v4();
    if needs_drive_load {
        let load_started = Instant::now();
        if let Err(err) = drive.load() {
            let _ = reply.send(Err(Status::internal(format!("load drive: {err}"))));
            return;
        }
        tracing::info!(
            target: "remanence_write_diag",
            phase = "drive_load",
            session_id = %session_id,
            tape_uuid = %Uuid::from_bytes(selected.tape_uuid),
            block_size_bytes = selected.block_size,
            elapsed_ms = crate::diagnostics::duration_ms(load_started.elapsed()),
            "remanence_write_diag",
        );
    }

    let tape_uuid = selected.tape_uuid;
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
    });
    if reply.send(Ok(open_reply)).is_err() {
        if needs_drive_load {
            let _ = drive.unload();
        }
        return;
    }

    let mut append_gate = SessionAppendGate::default();
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::AppendFinish {
                session_id: requested,
                spool_path,
                archive_path,
                caller_object_id,
                expected_content_sha256,
                live_write_counter,
                reply,
            } => {
                if requested != session_id {
                    let _ = std::fs::remove_file(spool_path);
                    let _ = reply.send(Err(Status::not_found("write session not found")));
                    continue;
                }
                if let Err(status) = append_gate.check() {
                    let _ = std::fs::remove_file(spool_path);
                    let _ = reply.send(Err(status));
                    continue;
                }
                let logical_size = std::fs::metadata(&spool_path)
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                let request = WriteObjectToPoolRequest {
                    pool_id: pool_cfg.id.clone(),
                    source_path: spool_path.clone(),
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
                        let mut sink = DriveHandleSink(drive);
                        crate::pool_write::write_to_selected_tape_with_live_counter_after_replay_check(
                                index,
                                &mut sink,
                                &pool_cfg,
                                request,
                                selected.clone(),
                                live_write_counter,
                            )
                    }
                    Err(err) => Err(err),
                };
                let append_elapsed = append_started.elapsed();
                let _ = std::fs::remove_file(&spool_path);
                match result {
                    Ok(result) => {
                        let replay = result.is_replay();
                        if !replay {
                            objects_committed = objects_committed.saturating_add(1);
                            bytes_committed = bytes_committed.saturating_add(logical_size);
                            last_checkpoint_at_utc =
                                Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                        }
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
                        let _ = reply.send(Ok(result.object.to_proto()));
                    }
                    Err(err) => {
                        append_gate.record_failure();
                        tracing::info!(
                            target: "remanence_write_diag",
                            phase = "drive_append_total",
                            session_id = %session_id,
                            tape_uuid = %Uuid::from_bytes(tape_uuid),
                            payload_bytes = logical_size,
                            block_size_bytes = selected.block_size,
                            status = "error",
                            error = %err,
                            elapsed_ms = crate::diagnostics::duration_ms(append_elapsed),
                            throughput_mib_s = crate::diagnostics::mib_per_s(logical_size, append_elapsed),
                            "remanence_write_diag",
                        );
                        let _ = reply.send(Err(status_from_pool_write_error(err)));
                    }
                }
            }
            DriveCommand::Close {
                session_id: requested,
                unload_before_close,
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
                    if unload_before_close {
                        if let Err(err) = drive.unload() {
                            let _ =
                                reply.send(Err(Status::internal(format!("unload drive: {err}"))));
                            continue;
                        }
                    }
                    Ok(session_proto(WriteSessionProtoInput {
                        session_id,
                        tape_uuid: &tape_uuid,
                        state: pb::write_session::State::WriteSessionStateClosed,
                        objects_committed,
                        bytes_committed,
                        opened_at_utc: opened_at_utc.as_str(),
                        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                        drive_element_address: bay,
                    }))
                } else {
                    Err(Status::not_found("write session not found"))
                };
                if status.is_ok() {
                    if let Err(err) = record_session_event(
                        index,
                        cfg,
                        SessionAuditInput {
                            session_id,
                            session_kind: "write",
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
            DriveCommand::Abort {
                session_id: requested,
                unload_before_close,
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
                    if unload_before_close {
                        if let Err(err) = drive.unload() {
                            let _ =
                                reply.send(Err(Status::internal(format!("unload drive: {err}"))));
                            continue;
                        }
                    }
                    Ok(session_proto(WriteSessionProtoInput {
                        session_id,
                        tape_uuid: &tape_uuid,
                        state: pb::write_session::State::WriteSessionStateAborted,
                        objects_committed,
                        bytes_committed,
                        opened_at_utc: opened_at_utc.as_str(),
                        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                        drive_element_address: bay,
                    }))
                } else {
                    Err(Status::not_found("write session not found"))
                };
                if status.is_ok() {
                    if let Err(err) = record_session_event(
                        index,
                        cfg,
                        SessionAuditInput {
                            session_id,
                            session_kind: "write",
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
            DriveCommand::Get {
                session_id: requested,
                reply,
            } => {
                let status = if requested == session_id {
                    Ok(session_proto(WriteSessionProtoInput {
                        session_id,
                        tape_uuid: &tape_uuid,
                        state: pb::write_session::State::WriteSessionStateOpen,
                        objects_committed,
                        bytes_committed,
                        opened_at_utc: opened_at_utc.as_str(),
                        last_checkpoint_at_utc: last_checkpoint_at_utc.as_deref(),
                        drive_element_address: bay,
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

fn prepare_drive_for_write(
    drive: &mut DriveHandle,
    tape_uuid: &TapeUuid,
    block_size: u32,
    session_id: Uuid,
) -> Result<(), Status> {
    let prepare_started = Instant::now();
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
        drive_uuid,
        drive_serial,
        reply,
    } = request;

    if needs_drive_load {
        if let Err(err) = drive.load() {
            let _ = reply.send(Err(Status::internal(format!("load drive: {err}"))));
            return;
        }
    }
    if let Err(status) = verify_loaded_tape_identity(drive, &tape_uuid) {
        let _ = reply.send(Err(status));
        return;
    }

    let session_id = Uuid::new_v4();
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
                    &tape_uuid,
                    object_id.as_str(),
                    file_id.as_str(),
                    start_byte,
                    end_byte,
                    stream_chunk_bytes,
                    chunk_tx.clone(),
                ) {
                    let _ = chunk_tx.blocking_send(Err(status));
                }
            }
            DriveCommand::CloseRead {
                session_id: requested,
                unload_before_close,
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
                    if unload_before_close {
                        if let Err(err) = drive.unload() {
                            let _ =
                                reply.send(Err(Status::internal(format!("unload drive: {err}"))));
                            continue;
                        }
                    }
                    Ok(read_session_proto(
                        session_id,
                        &tape_uuid,
                        pb::read_session::State::ReadSessionStateClosed,
                        opened_at_utc.as_str(),
                        bay,
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
            DriveCommand::AppendFinish {
                reply, spool_path, ..
            } => {
                let _ = std::fs::remove_file(spool_path);
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
            DriveCommand::Close { reply, .. }
            | DriveCommand::Abort { reply, .. }
            | DriveCommand::Get { reply, .. } => {
                let _ = reply.send(Err(Status::failed_precondition(
                    "active session is a read session",
                )));
            }
        }
    }
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
        RoboticsAction::Load { slot, bay } => library
            .load(*slot, *bay, &cfg.policy)
            .map_err(|err| err.to_string()),
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
    if let Err(err) = index.raise_alarm(
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

fn clear_library_snapshot_persist_alarm(index: &mut CatalogIndex, library_serial: &str) {
    let condition_key = library_snapshot_persist_alarm_key(library_serial);
    if let Err(err) = index.clear_alarm(condition_key.as_str()) {
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
        RoboticsAction::Load { slot, bay } => {
            detail.insert(
                "slot".to_string(),
                CborValue::Integer(u64::from(*slot).into()),
            );
            detail.insert(
                "bay".to_string(),
                CborValue::Integer(u64::from(*bay).into()),
            );
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
        let _ = index.raise_alarm(
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
    if let Err(err) = index.raise_alarm(
        format!("cleaning-needs-operator:{}", run.run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(fence_detail.as_str()),
    ) {
        let _ =
            index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(fence_detail.as_str()));
        return Err(status_from_state_error(err));
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
    let tape_prefixes = &clean_cfg.voltag_prefixes;
    let mut eligible_carts = library
        .library()
        .slots
        .iter()
        .filter_map(|slot| {
            let voltag = slot.cartridge.as_ref()?;
            if !tape_prefixes
                .iter()
                .any(|prefix| voltag.starts_with(prefix))
            {
                return None;
            }
            let tape = index.get_tape_by_voltag(voltag).ok().flatten()?;
            let cleaning_state = index
                .get_tape_cleaning_state(tape.tape_uuid.as_slice())
                .ok()
                .flatten()
                .flatten();
            if tape.kind != "cleaning" {
                return None;
            }
            match cleaning_state.as_deref() {
                None | Some("unverified") | Some("ok") => {
                    Some((slot.element_address, voltag.clone(), tape))
                }
                Some("expired") | Some("rejected") => None,
                _ => None,
            }
        })
        .collect::<Vec<_>>();
    if eligible_carts.is_empty() {
        let _ = index.clear_alarm(format!("cleaning-needs-operator:{}", run.run_id).as_str());
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
        let _ = index.raise_alarm(
            format!("no-cln-cart:{library_serial}").as_str(),
            "no-cln-cart",
            "critical",
            Some("{\"recovery_step\":\"selecting\"}"),
        );
        let _ = index.terminalize_clean_run(
            run.run_id.as_str(),
            "failed",
            Some("{\"reason\":\"no-cln-cart\"}"),
        );
        return Err(Status::failed_precondition(
            "no eligible cleaning cartridge is available",
        ));
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
    retry_cleaning_move(index, run_id.as_str(), drive_uuid, "moving-in", || {
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
        let _ = index.raise_alarm(
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
        let _ = index.raise_alarm(
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
        .open_drive(drive_bay, &cfg.policy)
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
        let _ = index.raise_alarm(
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
        let _ = index.raise_alarm(
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
    retry_cleaning_move(index, run_id.as_str(), drive_uuid, "moving-back", || {
        library
            .unload(drive_bay, Some(slot_address), &cfg.policy)
            .map_err(|err| format!("unload cleaning cartridge: {err}"))?;
        Ok(())
    })?;
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
        let _ = index.raise_alarm(
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
    let _ = index.clear_alarm(format!("cleaning-needs-operator:{}", run_id).as_str());
    let _ = index.clear_alarm(
        format!(
            "drive-cleaning-abnormal-frequency:{}",
            crate::bytes_to_hex(drive_uuid)
        )
        .as_str(),
    );
    let _ = index.clear_alarm(format!("cln-cart-expired:{}", voltag).as_str());
    let _ = index.clear_alarm(format!("cart-not-cleaning-behavior:{}", voltag).as_str());
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
    let _ = index.raise_alarm(
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

fn stream_one_object(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: &[u8; 16],
    object_id: &str,
    stream_chunk_bytes: u32,
    chunk_tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
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

    verify_loaded_tape_identity(drive, tape_uuid)?;
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config: {err}")))?;
    let block_size_u32 = u32::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;
    drive
        .write_config(TapeConfig {
            block_size: BlockSize::Fixed {
                size_bytes: block_size_u32,
            },
            compression: false,
            max_block_size_bytes: current_cfg.max_block_size_bytes,
            write_protected: current_cfg.write_protected,
            worm: current_cfg.worm,
        })
        .map_err(|err| Status::internal(format!("set fixed-block config: {err}")))?;

    let mut source = DriveHandleSource(drive);
    let writer = if stream_chunk_bytes == 0 {
        crate::read_core::ChannelWriter::new(chunk_tx)
    } else {
        crate::read_core::ChannelWriter::with_chunk_size(chunk_tx, stream_chunk_bytes as usize)
    };
    let mut sink = crate::read_core::CapturePayloadSink::new(writer);
    crate::read_core::read_object_payload(
        &mut source,
        block_size_usize,
        tape_file.block_count,
        copy.tape_file_number,
        manifest_sha256,
        &mut sink,
    )
    .map_err(|err| Status::internal(format!("read object: {err}")))?;
    let (writer, _payload_bytes, _digest) = sink
        .finish_with_writer()
        .map_err(|err| Status::internal(format!("finish payload stream: {err}")))?;
    writer
        .finish()
        .map_err(|err| Status::internal(format!("finish read stream: {err}")))?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stream_one_file_range(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    tape_uuid: &[u8; 16],
    object_id: &str,
    file_id: &str,
    start_byte: u64,
    end_byte: u64,
    stream_chunk_bytes: u32,
    chunk_tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
) -> Result<(), Status> {
    let request =
        file_range_read_request(index, tape_uuid, object_id, file_id, start_byte, end_byte)?;
    let block_size_u32 = u32::try_from(request.block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;

    verify_loaded_tape_identity(drive, tape_uuid)?;
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config: {err}")))?;
    drive
        .write_config(fixed_no_compression_config(current_cfg, block_size_u32))
        .map_err(|err| Status::internal(format!("set fixed-block config: {err}")))?;

    let mut source = DriveHandleSource(drive);
    stream_file_range_from_source(&mut source, request, stream_chunk_bytes, chunk_tx)?;
    Ok(())
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
    Ok(crate::read_core::PlaintextFileRangeReadRequest {
        block_size: block_size_usize,
        tape_file_number: tape_file.tape_file_number,
        first_chunk_lba: file.first_chunk_lba.map(BodyLba),
        file_size_bytes: file.size_bytes,
        range_start,
        range_len,
    })
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
    chunk_tx: mpsc::Sender<Result<pb::BytesChunk, Status>>,
) -> Result<(), Status> {
    // Ranged reads are opaque stored-payload reads. The daemon does not decrypt
    // or hold key material; clients interpret or decrypt the returned bytes.
    let mut writer = if stream_chunk_bytes == 0 {
        crate::read_core::ChannelWriter::new(chunk_tx)
    } else {
        crate::read_core::ChannelWriter::with_chunk_size(chunk_tx, stream_chunk_bytes as usize)
    };
    crate::read_core::read_plaintext_file_range(source, request, &mut writer)
        .map_err(status_from_file_range_error)?;
    writer
        .finish()
        .map_err(|err| Status::internal(format!("finish read stream: {err}")))?;
    Ok(())
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

fn read_session_proto(
    session_id: Uuid,
    tape_uuid: &TapeUuid,
    state: pb::read_session::State,
    opened_at_utc: &str,
    drive_element_address: u16,
) -> pb::ReadSession {
    pb::ReadSession {
        session_id: session_id.as_bytes().to_vec(),
        tape_uuid: tape_uuid.to_vec(),
        drive_element_address: u32::from(drive_element_address),
        state: state as i32,
        opened_at: timestamp_from_rfc3339(opened_at_utc),
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
}

fn session_proto(input: WriteSessionProtoInput<'_>) -> pb::WriteSession {
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
        PoolWriteError::ContentHashMismatch { .. } => Status::failed_precondition(message),
        PoolWriteError::CallerObjectIdConflict { .. } => Status::already_exists(message),
        PoolWriteError::ReplayObjectInvalid { .. } => Status::internal(message),
        PoolWriteError::Streaming(streaming) => status_from_streaming_error(&streaming, message),
        PoolWriteError::Parity(parity) => status_from_parity_error(&parity, message),
        PoolWriteError::Io { .. } | PoolWriteError::TapeIo(_) | PoolWriteError::TimeFormat(_) => {
            Status::internal(message)
        }
    }
}

fn status_from_streaming_error(err: &StreamingError, message: String) -> Status {
    match err {
        StreamingError::InvalidInput(_) => Status::invalid_argument(message),
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
        | ParityError::ObjectTooLargeForEmptyTape { .. } => Status::resource_exhausted(message),
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
    use remanence_aead::RootKey;
    use remanence_chaos::model::{ModelTransport, VirtualTape, VirtualWorld};
    use remanence_format::{
        read_encrypted_rao_file_range_to_vec, write_encrypted_rao_object, write_rem_tar_object,
        RemTarFile, RemTarObjectLayout, RemTarObjectOptions,
    };
    use remanence_library::{
        DriveBay, ElementLayout, FixtureTransport, IdentitySource, InstalledDrive, Library, Slot,
        VecBlockSink, VecBlockSource, WormMediaState,
    };
    use remanence_parity::{
        CommittedBundle, CommittedBundleKind, ParityConfig, TapeFileEntry, TapeFileKind,
    };
    use remanence_state::{
        CatalogIndex, DriveObservationInput, NativeObjectCopyProjectionInput,
        NativeObjectFileProjectionInput, NativeObjectProjectionInput, ProvisionTapeInput,
        TapeJournalIndexInput, OBJECT_COPY_REPRESENTATION_PLAINTEXT,
    };

    const RANGE_OBJECT_ID: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
    const RANGE_TAPE_UUID: [u8; 16] = [0xAB; 16];

    fn changer_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
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
        }
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
                    key_id: None,
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

    async fn collect_stream_chunks(
        mut rx: mpsc::Receiver<Result<pb::BytesChunk, Status>>,
    ) -> Result<Vec<u8>, Status> {
        let mut bytes = Vec::new();
        let mut saw_last = false;
        while let Some(item) = rx.recv().await {
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
        let (tx, rx) = mpsc::channel(256);
        stream_file_range_from_source(&mut source, request, 0, tx)?;
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
        });
        let read = read_session_proto(
            session_id,
            &tape_uuid,
            pb::read_session::State::ReadSessionStateOpen,
            opened_at,
            0x0101,
        );

        assert_eq!(write.drive_element_address, 0x0100);
        assert_eq!(read.drive_element_address, 0x0101);
    }

    #[test]
    fn pool_write_status_maps_nested_input_and_capacity_errors() {
        let invalid = status_from_pool_write_error(PoolWriteError::Streaming(
            StreamingError::InvalidInput("bad archive path".to_string()),
        ));
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);

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

        clear_snapshot_persist_alarm(&mut index, &drive_uuid);

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
    fn fast_eject_cleaning_cart_is_not_credited() {
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB-FAST", 0x0100, "DRV-FAST", 0x0400, 1,
        )));
        {
            let mut world = world.lock().expect("world lock");
            world.put_tape_in_slot(
                0x0400,
                "CLN-FAST",
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
        let cln_uuid = [0x93; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: cln_uuid,
                voltag: "CLN-FAST".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision cleaning tape");
        index
            .set_tape_kind(&cln_uuid, "cleaning")
            .expect("mark cleaning cart")
            .expect("cleaning tape row");
        index
            .set_tape_cleaning_state(&cln_uuid, "ok")
            .expect("mark cleaning cart state")
            .expect("cleaning tape row");
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
        assert_eq!(
            index
                .get_tape_cleaning_state(cln_uuid.as_slice())
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
        let root_key = RootKey::new([0x42; 32]).expect("test key");
        let mut encrypted_sink = VecBlockSink::new();
        let encrypted_report = write_encrypted_rao_object(
            &mut encrypted_sink,
            &encrypted_opts,
            &encrypted_files,
            &root_key,
            [0x24; 16],
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
            &root_key,
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
        let (tx, _rx) = mpsc::channel(8);
        let overflow = stream_file_range_from_source(&mut source, overflow_request, 0, tx)
            .expect_err("overflow must fail");
        assert_eq!(overflow.code(), tonic::Code::InvalidArgument);

        let reversed =
            file_range_read_request(&fixture.index, &RANGE_TAPE_UUID, RANGE_OBJECT_ID, "", 5, 4)
                .expect_err("end before start must fail");
        assert_eq!(reversed.code(), tonic::Code::InvalidArgument);
    }
}
