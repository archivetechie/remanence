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
    BlockSize, ChangerHandle, DiscoveryReport, DriveHandle, DriveHandleSink, DriveHandleSource,
    StaticAllowlist, TapeConfig,
};
use remanence_parity::{
    scan_reconstruct_filemark_map, DriveHandleRawSource, FilemarkMap, ParityError, TapeFileEntry,
    TapeFileKind,
};
use remanence_state::{
    AuditActor, AuditEvent, CatalogIndex, TapePoolConfig, OBJECT_COPY_REPRESENTATION_ENCRYPTED,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT,
};
use remanence_stream::StreamingError;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use crate::pool_write::{write_to_selected_tape, SelectedTape, WriteObjectToPoolRequest};
use crate::{
    load_tape_by_uuid, pb, status_from_state_error, timestamp_from_rfc3339, verify_tape_identity,
    PoolWriteError, SelectTapeError, TapeUuid,
};

pub(crate) const SPOOL_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Robotics work to perform after the owner opens and refreshes the library.
pub(crate) enum RoboticsAction {
    Refresh,
    Move { src: u16, dst: u16 },
    Load { slot: u16, bay: u16 },
    Unload { bay: u16, destination: Option<u16> },
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
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    OpenRead {
        tape_uuid: [u8; 16],
        needs_drive_load: bool,
        reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
    },
    Unload {
        reply: oneshot::Sender<Result<(), Status>>,
    },
    AppendFinish {
        session_id: Uuid,
        spool_path: PathBuf,
        archive_path: PathBuf,
        caller_object_id: String,
        expected_content_sha256: Option<[u8; 32]>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MountedSession {
    pub bay: u16,
    pub home_slot: Option<u16>,
    pub tape_uuid: TapeUuid,
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
            .copied()
            .ok_or_else(|| Status::not_found("session not found"))
    }

    pub(crate) fn forget_session(&self, session_id: Uuid) {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&session_id);
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
                    publish_library_snapshot(&cfg.library_snapshot, changer.library().clone());
                }
                let _ = reply.send(result);
            }
            ChangerCommand::Refresh { reply } => {
                let result = changer
                    .refresh()
                    .map_err(|err| Status::internal(format!("refresh inventory: {err}")));
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
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            DriveCommand::OpenWrite {
                pool_cfg,
                selected,
                needs_drive_load,
                reply,
            } => handle_drive_open_write(
                bay,
                &mut index,
                &mut rx,
                drive,
                OpenWriteActorRequest {
                    pool_cfg,
                    selected,
                    needs_drive_load,
                    reply,
                },
            ),
            DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load,
                reply,
            } => handle_drive_open_read(
                bay,
                &mut index,
                &mut rx,
                drive,
                tape_uuid,
                needs_drive_load,
                reply,
            ),
            DriveCommand::Unload { reply } => {
                let result = drive
                    .unload()
                    .map_err(|err| Status::internal(format!("unload drive: {err}")));
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
    reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
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
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    request: OpenWriteActorRequest,
) {
    let OpenWriteActorRequest {
        pool_cfg,
        selected,
        needs_drive_load,
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
            "remanence_write_diag phase=drive_load session_id={} tape_uuid={} block_size_bytes={} elapsed_ms={:.3}",
            session_id,
            Uuid::from_bytes(selected.tape_uuid),
            selected.block_size,
            crate::diagnostics::duration_ms(load_started.elapsed()),
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
        "remanence_write_diag phase=drive_open_actor session_id={} tape_uuid={} needs_drive_load={} block_size_bytes={} elapsed_ms={:.3}",
        session_id,
        Uuid::from_bytes(tape_uuid),
        needs_drive_load,
        selected.block_size,
        crate::diagnostics::duration_ms(actor_open_started.elapsed()),
    );

    let opened_at_utc = now_rfc3339().unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let mut objects_committed = 0u64;
    let mut bytes_committed = 0u64;
    let mut last_checkpoint_at_utc = None;
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
                let mut sink = DriveHandleSink(drive);
                let append_started = Instant::now();
                let result =
                    write_to_selected_tape(index, &mut sink, &pool_cfg, request, selected.clone());
                let append_elapsed = append_started.elapsed();
                let _ = std::fs::remove_file(&spool_path);
                match result {
                    Ok(result) => {
                        objects_committed = objects_committed.saturating_add(1);
                        bytes_committed = bytes_committed.saturating_add(logical_size);
                        last_checkpoint_at_utc =
                            Some(now_rfc3339().unwrap_or_else(|_| opened_at_utc.clone()));
                        tracing::info!(
                            target: "remanence_write_diag",
                            "remanence_write_diag phase=drive_append_total session_id={} caller_object_id={:?} tape_uuid={} payload_bytes={} block_size_bytes={} elapsed_ms={:.3} throughput_mib_s={:.3}",
                            session_id,
                            result.object.caller_object_id,
                            Uuid::from_bytes(tape_uuid),
                            logical_size,
                            selected.block_size,
                            crate::diagnostics::duration_ms(append_elapsed),
                            crate::diagnostics::mib_per_s(logical_size, append_elapsed),
                        );
                        let _ = reply.send(Ok(result.object.to_proto()));
                    }
                    Err(err) => {
                        append_gate.record_failure();
                        tracing::info!(
                            target: "remanence_write_diag",
                            "remanence_write_diag phase=drive_append_total session_id={} tape_uuid={} payload_bytes={} block_size_bytes={} status=error error={:?} elapsed_ms={:.3} throughput_mib_s={:.3}",
                            session_id,
                            Uuid::from_bytes(tape_uuid),
                            logical_size,
                            selected.block_size,
                            err.to_string(),
                            crate::diagnostics::duration_ms(append_elapsed),
                            crate::diagnostics::mib_per_s(logical_size, append_elapsed),
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
        "remanence_write_diag phase=drive_prepare session_id={} tape_uuid={} selected_block_size_bytes={} prior_block_size={:?} prior_compression={} target_block_size={:?} target_compression={} rewind_verify_ms={:.3} verify_bootstrap_ms={:.3} rewind_write_ms={:.3} read_config_ms={:.3} write_config_ms={:.3} elapsed_ms={:.3}",
        session_id,
        Uuid::from_bytes(*tape_uuid),
        block_size,
        current_cfg.block_size,
        current_cfg.compression,
        target_cfg.block_size,
        target_cfg.compression,
        crate::diagnostics::duration_ms(rewind_verify_elapsed),
        crate::diagnostics::duration_ms(verify_elapsed),
        crate::diagnostics::duration_ms(rewind_write_elapsed),
        crate::diagnostics::duration_ms(read_config_elapsed),
        crate::diagnostics::duration_ms(write_config_started.elapsed()),
        crate::diagnostics::duration_ms(prepare_started.elapsed()),
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
    rx: &mut mpsc::Receiver<DriveCommand>,
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    needs_drive_load: bool,
    reply: oneshot::Sender<Result<pb::ReadSession, Status>>,
) {
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
    };

    publish_library_snapshot(&cfg.library_snapshot, library.library().clone());

    match action_result {
        Ok(()) => {
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
        Err(message) => {
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
    }
    detail
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
    let object = index
        .get_native_object(object_id)
        .map_err(status_from_state_error)?;
    let object = object.ok_or_else(|| Status::not_found("object not found"))?;
    let file = index
        .get_native_object_file(object_id, file_id)
        .map_err(status_from_state_error)?;
    let file = file.ok_or_else(|| Status::not_found("object file not found"))?;
    let copy = object
        .copies
        .iter()
        .find(|copy| copy.tape_uuid.as_slice() == tape_uuid)
        .ok_or_else(|| {
            Status::failed_precondition("object is not on the tape pinned by this read session")
        })?;
    let (range_start, range_len) = requested_file_range(file.size_bytes, start_byte, end_byte)?;
    if copy.representation == OBJECT_COPY_REPRESENTATION_ENCRYPTED {
        return Err(Status::failed_precondition(
            "encrypted ranged reads require daemon key material; no key resolver is configured",
        ));
    }
    if copy.representation != OBJECT_COPY_REPRESENTATION_PLAINTEXT {
        return Err(Status::failed_precondition(format!(
            "unsupported object copy representation {}",
            copy.representation
        )));
    }

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

    verify_loaded_tape_identity(drive, tape_uuid)?;
    let current_cfg = drive
        .read_config()
        .map_err(|err| Status::internal(format!("read drive config: {err}")))?;
    let block_size_u32 = u32::try_from(block_size)
        .map_err(|_| Status::internal("tape block size does not fit u32"))?;
    drive
        .write_config(fixed_no_compression_config(current_cfg, block_size_u32))
        .map_err(|err| Status::internal(format!("set fixed-block config: {err}")))?;

    let mut source = DriveHandleSource(drive);
    let mut writer = if stream_chunk_bytes == 0 {
        crate::read_core::ChannelWriter::new(chunk_tx)
    } else {
        crate::read_core::ChannelWriter::with_chunk_size(chunk_tx, stream_chunk_bytes as usize)
    };
    crate::read_core::read_plaintext_file_range(
        &mut source,
        crate::read_core::PlaintextFileRangeReadRequest {
            block_size: block_size_usize,
            tape_file_number: tape_file.tape_file_number,
            first_chunk_lba: file.first_chunk_lba.map(BodyLba),
            file_size_bytes: file.size_bytes,
            range_start,
            range_len,
        },
        &mut writer,
    )
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
        PoolWriteError::NoParityAppendUnsupported { .. } => Status::failed_precondition(message),
        PoolWriteError::ParityAppendUnsupported { .. } => Status::failed_precondition(message),
        PoolWriteError::SelectedTapeInsufficientCapacity { .. } => {
            Status::failed_precondition(message)
        }
        PoolWriteError::ContentHashMismatch { .. } => Status::failed_precondition(message),
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
    use remanence_library::WormMediaState;

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
}
