//! Hardware mount bridge shared by CLI and Layer 5 actor orchestration.

use remanence_library::{
    resolve_load_target, AccessPolicy, DriveHandle, Library, LibraryHandle, LoadError, LoadPlan,
};
use remanence_state::{CatalogIndex, StateError};
use std::collections::HashSet;
use std::time::Instant;
use tokio::sync::oneshot;
use tonic::Status;
use uuid::Uuid;

use crate::pool_write::select_tape_in_pool;
use crate::{bytes_to_hex, pb, status_from_state_error, ApiState, TapeUuid};

/// Error variants from `load_tape_by_uuid`.
#[derive(Debug)]
pub enum LoadByUuidError {
    /// The tape UUID is not known to the catalog.
    UnknownTape {
        /// Canonical lowercase hex tape UUID bytes.
        tape_uuid_hex: String,
    },
    /// The tape's cataloged voltag does not appear anywhere in the library inventory.
    NotInLibrary {
        /// Human-readable barcode/volume tag.
        voltag: String,
    },
    /// The tape is in a slot but there is no free drive bay to load it into.
    NoFreeDrive {
        /// Human-readable barcode/volume tag.
        voltag: String,
    },
    /// The changer MOVE or drive LOAD failed.
    LoadFailed {
        /// Human-readable barcode/volume tag.
        voltag: String,
        /// Underlying library load failure.
        cause: LoadError,
    },
    /// Opening the drive bay (identity revalidation) failed.
    OpenDriveFailed {
        /// SCSI drive bay element address.
        bay: u16,
        /// Underlying drive-open failure.
        cause: remanence_library::OpenError,
    },
    /// The catalog returned an error.
    State(StateError),
}

impl std::fmt::Display for LoadByUuidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownTape { tape_uuid_hex } => {
                write!(
                    f,
                    "tape UUID {tape_uuid_hex} is not registered in the catalog"
                )
            }
            Self::NotInLibrary { voltag } => {
                write!(
                    f,
                    "cartridge {voltag} not found in library inventory (slot or drive)"
                )
            }
            Self::NoFreeDrive { voltag } => {
                write!(f, "no free drive bay available to load cartridge {voltag}")
            }
            Self::LoadFailed { voltag, cause } => {
                write!(f, "load cartridge {voltag} failed: {cause}")
            }
            Self::OpenDriveFailed { bay, cause } => {
                write!(f, "open drive bay 0x{bay:04x} failed: {cause}")
            }
            Self::State(err) => write!(f, "catalog error: {err}"),
        }
    }
}

impl std::error::Error for LoadByUuidError {}

impl From<StateError> for LoadByUuidError {
    fn from(err: StateError) -> Self {
        Self::State(err)
    }
}

/// Locate the cartridge for `tape_uuid` in the library, load it if needed,
/// and return an open `DriveHandle`.
pub fn load_tape_by_uuid(
    state: &CatalogIndex,
    library: &mut LibraryHandle,
    policy: &dyn AccessPolicy,
    tape_uuid: &TapeUuid,
) -> Result<DriveHandle, LoadByUuidError> {
    let tape = state
        .get_tape(tape_uuid)
        .map_err(LoadByUuidError::State)?
        .ok_or_else(|| {
            let hex = bytes_to_hex(tape_uuid);
            LoadByUuidError::UnknownTape { tape_uuid_hex: hex }
        })?;

    let voltag = tape
        .voltag
        .clone()
        .unwrap_or_else(|| "<no-voltag>".to_string());

    // Clone the inventory snapshot so resolving the load plan does not hold a
    // borrow into `library` when the live load/open calls happen below.
    let lib_snapshot = library.library().clone();
    let plan = resolve_load_target(&lib_snapshot, voltag.as_str()).map_err(|err| match err {
        LoadError::NotInLibrary => LoadByUuidError::NotInLibrary {
            voltag: voltag.clone(),
        },
        LoadError::NoFreeDrive => LoadByUuidError::NoFreeDrive {
            voltag: voltag.clone(),
        },
        _ => LoadByUuidError::LoadFailed {
            voltag: voltag.clone(),
            cause: err,
        },
    })?;
    drop(lib_snapshot);

    let bay = match plan {
        LoadPlan::AlreadyLoaded { bay } => bay,
        LoadPlan::Load { slot, bay } => {
            library
                .load(slot, bay, policy)
                .map_err(|err| LoadByUuidError::LoadFailed {
                    voltag: voltag.clone(),
                    cause: err,
                })?;
            bay
        }
    };

    library
        .open_drive(bay, policy)
        .map_err(|err| LoadByUuidError::OpenDriveFailed { bay, cause: err })
}

pub(crate) async fn open_write_session(
    state: &ApiState,
    pool_id: String,
    library_serial: String,
) -> Result<pb::WriteSession, Status> {
    let state = state.clone();
    await_critical_task(
        "open_write_session",
        tokio::spawn(
            async move { open_write_session_critical(state, pool_id, library_serial).await },
        ),
    )
    .await
}

async fn open_write_session_critical(
    state: ApiState,
    pool_id: String,
    library_serial: String,
) -> Result<pb::WriteSession, Status> {
    let pool = state.drive_pool()?.clone();
    open_write_session_reserved(&state, &pool, pool_id, library_serial).await
}

async fn open_write_session_reserved(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    pool_id: String,
    library_serial: String,
) -> Result<pb::WriteSession, Status> {
    let pool_cfg = state.pool_config(&pool_id)?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    let select_started = Instant::now();
    let mut select_attempts = 0u64;
    let (selected, _tape_reservation) = loop {
        select_attempts = select_attempts.saturating_add(1);
        let selected = select_tape_in_pool(&index, &pool_cfg, 0, &pool.mounted_tape_uuids())
            .map_err(crate::write_owner::status_from_select_tape_error)?;
        match pool.reserve_tape(selected.tape_uuid) {
            Ok(reservation) => break (selected, reservation),
            Err(err) if err.code() == tonic::Code::FailedPrecondition => continue,
            Err(err) => return Err(err),
        }
    };
    let select_elapsed = select_started.elapsed();
    let tape_uuid = selected.tape_uuid;
    let open_started = Instant::now();
    let resolve_started = Instant::now();
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(state, pool, &library_serial, &tape_uuid)?;
    let resolve_elapsed = resolve_started.elapsed();
    let drive = pool.drive_tx(mount.bay)?;
    let pool_id_for_diag = pool_cfg.id.clone();
    let mut changer_move_ms = 0.0;
    if let Some(slot) = mount.source_slot {
        let changer_started = Instant::now();
        changer_move(pool, slot, mount.bay).await?;
        changer_move_ms = crate::diagnostics::duration_ms(changer_started.elapsed());
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let actor_open_started = Instant::now();
    let open_result: Result<pb::WriteSession, Status> = async {
        drive
            .send(crate::write_owner::DriveCommand::OpenWrite {
                pool_cfg,
                selected,
                needs_drive_load: mount.needs_drive_load,
                library_serial: mount.library_serial.clone(),
                drive_uuid: mount.drive_uuid.clone(),
                drive_serial: mount.drive_serial.clone(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("drive actor unavailable"))?;
        reply_rx
            .await
            .map_err(|_| Status::internal("drive actor dropped reply"))?
    }
    .await;
    let actor_open_elapsed = actor_open_started.elapsed();
    let session = match open_result {
        Ok(session) => session,
        Err(err) => {
            compensate_open_mount(pool, &mount).await;
            return Err(err);
        }
    };
    let open_elapsed = open_started.elapsed();
    let session_id = uuid_from_proto(&session.session_id, "session_id")?;
    tracing::info!(
        target: "remanence_write_diag",
        phase = "mount_open",
        session_id = %session_id,
        pool_id = %pool_id_for_diag,
        tape_uuid = %Uuid::from_bytes(tape_uuid),
        bay = mount.bay,
        needs_drive_load = mount.needs_drive_load,
        selection_attempts = select_attempts,
        selection_ms = crate::diagnostics::duration_ms(select_elapsed),
        resolve_ms = crate::diagnostics::duration_ms(resolve_elapsed),
        changer_move_ms,
        actor_open_ms = crate::diagnostics::duration_ms(actor_open_elapsed),
        elapsed_ms = crate::diagnostics::duration_ms(open_elapsed),
        "remanence_write_diag",
    );
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: mount.bay,
            library_serial: mount.library_serial.clone(),
            barcode: mount.barcode.clone(),
            home_slot: mount.home_slot,
            tape_uuid,
            drive_uuid: mount.drive_uuid.clone(),
        },
    );
    drive_reservation.disarm();
    Ok(session)
}

pub(crate) async fn open_read_session(
    state: &ApiState,
    tape_uuid: TapeUuid,
) -> Result<pb::ReadSession, Status> {
    let state = state.clone();
    await_critical_task(
        "open_read_session",
        tokio::spawn(async move { open_read_session_critical(state, tape_uuid).await }),
    )
    .await
}

async fn open_read_session_critical(
    state: ApiState,
    tape_uuid: TapeUuid,
) -> Result<pb::ReadSession, Status> {
    let pool = state.drive_pool()?.clone();
    let library_serial = state
        .default_library_serial
        .as_deref()
        .map(|serial| serial.as_str().to_string())
        .ok_or_else(|| {
            Status::invalid_argument(
                "tape-target read sessions require exactly one configured library in this slice",
            )
        })?;
    if pool.is_tape_mounted(&tape_uuid) {
        return Err(Status::failed_precondition("tape is already mounted"));
    }
    let _tape_reservation = match pool.reserve_tape(tape_uuid) {
        Ok(reservation) => reservation,
        Err(err) => return Err(err),
    };
    open_read_session_reserved(&state, &pool, library_serial, tape_uuid).await
}

async fn open_read_session_reserved(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    library_serial: String,
    tape_uuid: TapeUuid,
) -> Result<pb::ReadSession, Status> {
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(state, pool, &library_serial, &tape_uuid)?;
    let drive = pool.drive_tx(mount.bay)?;
    if let Some(slot) = mount.source_slot {
        changer_move(pool, slot, mount.bay).await?;
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let open_result: Result<pb::ReadSession, Status> = async {
        drive
            .send(crate::write_owner::DriveCommand::OpenRead {
                tape_uuid,
                needs_drive_load: mount.needs_drive_load,
                library_serial: mount.library_serial.clone(),
                drive_uuid: mount.drive_uuid.clone(),
                drive_serial: mount.drive_serial.clone(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("drive actor unavailable"))?;
        reply_rx
            .await
            .map_err(|_| Status::internal("drive actor dropped reply"))?
    }
    .await;
    let session = match open_result {
        Ok(session) => session,
        Err(err) => {
            compensate_open_mount(pool, &mount).await;
            return Err(err);
        }
    };
    let session_id = uuid_from_proto(&session.session_id, "session_id")?;
    pool.record_session(
        session_id,
        crate::write_owner::MountedSession {
            bay: mount.bay,
            library_serial: mount.library_serial.clone(),
            barcode: mount.barcode.clone(),
            home_slot: mount.home_slot,
            tape_uuid,
            drive_uuid: mount.drive_uuid.clone(),
        },
    );
    drive_reservation.disarm();
    Ok(session)
}

pub(crate) async fn append_finish(
    state: &ApiState,
    session_id: Uuid,
    spool_path: std::path::PathBuf,
    archive_path: std::path::PathBuf,
    caller_object_id: String,
    expected_content_sha256: Option<[u8; 32]>,
) -> Result<pb::ObjectRecord, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let live_write_counter = mounted
        .drive_uuid
        .as_deref()
        .map(|drive_uuid| state.drive_counters(drive_uuid));
    let (reply_tx, reply_rx) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::AppendFinish {
            session_id,
            spool_path,
            archive_path,
            caller_object_id,
            expected_content_sha256,
            live_write_counter,
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))?
}

pub(crate) async fn get_write_session(
    state: &ApiState,
    session_id: Uuid,
) -> Result<pb::WriteSession, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::Get {
            session_id,
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))?
}

pub(crate) async fn close_write_session(
    state: &ApiState,
    session_id: Uuid,
) -> Result<pb::WriteSession, Status> {
    close_write_like(state, session_id, false).await
}

pub(crate) async fn abort_write_session(
    state: &ApiState,
    session_id: Uuid,
) -> Result<pb::WriteSession, Status> {
    close_write_like(state, session_id, true).await
}

async fn close_write_like(
    state: &ApiState,
    session_id: Uuid,
    abort: bool,
) -> Result<pb::WriteSession, Status> {
    let state = state.clone();
    await_critical_task(
        "close_write_session",
        tokio::spawn(async move { close_write_like_critical(state, session_id, abort).await }),
    )
    .await
}

async fn close_write_like_critical(
    state: ApiState,
    session_id: Uuid,
    abort: bool,
) -> Result<pb::WriteSession, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    let close_started = Instant::now();
    let actor_close_started = Instant::now();
    let unload_before_close = mounted.home_slot.is_some();
    ensure_mounted_session_media_readiness_admitted(&state, "close session", &mounted)?;
    if abort {
        drive
            .send(crate::write_owner::DriveCommand::Abort {
                session_id,
                unload_before_close,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("drive actor unavailable"))?;
    } else {
        drive
            .send(crate::write_owner::DriveCommand::Close {
                session_id,
                unload_before_close,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("drive actor unavailable"))?;
    }
    let session = reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))??;
    let actor_close_elapsed = actor_close_started.elapsed();
    let finish_started = Instant::now();
    let move_home = mounted.home_slot.is_some();
    finish_mounted_session(&pool, session_id, mounted).await?;
    let finish_elapsed = finish_started.elapsed();
    let close_elapsed = close_started.elapsed();
    tracing::info!(
        target: "remanence_write_diag",
        phase = "close_unmount",
        session_id = %session_id,
        abort,
        unload_before_close,
        move_home,
        actor_close_ms = crate::diagnostics::duration_ms(actor_close_elapsed),
        finish_mount_ms = crate::diagnostics::duration_ms(finish_elapsed),
        elapsed_ms = crate::diagnostics::duration_ms(close_elapsed),
        "remanence_write_diag",
    );
    Ok(session)
}

pub(crate) async fn get_read_session(
    state: &ApiState,
    session_id: Uuid,
) -> Result<pb::ReadSession, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::GetRead {
            session_id,
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))?
}

pub(crate) async fn close_read_session(
    state: &ApiState,
    session_id: Uuid,
) -> Result<pb::ReadSession, Status> {
    let state = state.clone();
    await_critical_task(
        "close_read_session",
        tokio::spawn(async move { close_read_session_critical(state, session_id).await }),
    )
    .await
}

async fn close_read_session_critical(
    state: ApiState,
    session_id: Uuid,
) -> Result<pb::ReadSession, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    ensure_mounted_session_media_readiness_admitted(&state, "close read session", &mounted)?;
    drive
        .send(crate::write_owner::DriveCommand::CloseRead {
            session_id,
            unload_before_close: mounted.home_slot.is_some(),
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    let session = reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))??;
    finish_mounted_session(&pool, session_id, mounted).await?;
    Ok(session)
}

async fn await_critical_task<T>(
    name: &'static str,
    task: tokio::task::JoinHandle<Result<T, Status>>,
) -> Result<T, Status>
where
    T: Send + 'static,
{
    // Tonic may drop the handler future on disconnect. The spawned task owns
    // the mount/session guards so open/close cleanup still runs to completion.
    task.await
        .map_err(|err| Status::internal(format!("{name} task failed: {err}")))?
}

pub(crate) async fn read_file(
    state: &ApiState,
    session_id: Uuid,
    object_id: String,
    file_id: Vec<u8>,
    stream_chunk_bytes: u32,
    chunk_tx: tokio::sync::mpsc::Sender<Result<pb::BytesChunk, Status>>,
) -> Result<(), Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    drive
        .send(crate::write_owner::DriveCommand::ReadFile {
            session_id,
            object_id,
            file_id,
            stream_chunk_bytes,
            chunk_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))
}

pub(crate) struct ReadObjectRangeDispatch {
    pub(crate) session_id: Uuid,
    pub(crate) object_id: String,
    pub(crate) file_id: String,
    pub(crate) start_byte: u64,
    pub(crate) end_byte: u64,
    pub(crate) stream_chunk_bytes: u32,
}

pub(crate) async fn read_object_range(
    state: &ApiState,
    request: ReadObjectRangeDispatch,
    chunk_tx: tokio::sync::mpsc::Sender<Result<pb::BytesChunk, Status>>,
) -> Result<(), Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(request.session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    drive
        .send(crate::write_owner::DriveCommand::ReadObjectRange {
            session_id: request.session_id,
            object_id: request.object_id,
            file_id: request.file_id,
            start_byte: request.start_byte,
            end_byte: request.end_byte,
            stream_chunk_bytes: request.stream_chunk_bytes,
            chunk_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))
}

async fn finish_mounted_session(
    pool: &crate::write_owner::DrivePool,
    session_id: Uuid,
    mounted: crate::write_owner::MountedSession,
) -> Result<(), Status> {
    let result = if let Some(home_slot) = mounted.home_slot {
        changer_move(pool, mounted.bay, home_slot).await
    } else {
        Ok(())
    };
    pool.forget_session(session_id);
    pool.release(mounted.bay);
    result
}

async fn changer_move(
    pool: &crate::write_owner::DrivePool,
    src: u16,
    dst: u16,
) -> Result<(), Status> {
    let changer = pool.changer_tx();
    let (reply_tx, reply_rx) = oneshot::channel();
    changer
        .send(crate::write_owner::ChangerCommand::Move {
            src,
            dst,
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("changer actor unavailable"))?;
    reply_rx
        .await
        .map_err(|_| Status::internal("changer actor dropped reply"))?
}

async fn compensate_open_mount(pool: &crate::write_owner::DrivePool, mount: &ActorMount) {
    if let Some(slot) = mount.source_slot {
        let _ = drive_unload(pool, mount.bay).await;
        let _ = changer_move(pool, mount.bay, slot).await;
    }
}

async fn drive_unload(pool: &crate::write_owner::DrivePool, bay: u16) -> Result<(), Status> {
    let drive = pool.drive_tx(bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::Unload { reply: reply_tx })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))?
}

#[derive(Debug)]
struct ActorMount {
    bay: u16,
    barcode: Option<String>,
    source_slot: Option<u16>,
    home_slot: Option<u16>,
    needs_drive_load: bool,
    library_serial: String,
    drive_uuid: Option<Vec<u8>>,
    drive_serial: Option<String>,
}

fn resolve_and_reserve_actor_mount(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    library_serial: &str,
    tape_uuid: &TapeUuid,
) -> Result<(ActorMount, crate::write_owner::DriveReservation), Status> {
    const ATTEMPTS: usize = 2;
    for attempt in 0..ATTEMPTS {
        let busy_bays = pool.busy_bays();
        let mount = resolve_actor_mount(state, library_serial, tape_uuid, &busy_bays)?;
        match pool.reserve_drive(mount.bay) {
            Ok(reservation) => {
                ensure_actor_mount_media_readiness_admitted(state, library_serial, &mount)?;
                return Ok((mount, reservation));
            }
            Err(err) if err.code() == tonic::Code::FailedPrecondition && attempt + 1 < ATTEMPTS => {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("drive reservation attempts are bounded and non-zero")
}

fn resolve_actor_mount(
    state: &ApiState,
    library_serial: &str,
    tape_uuid: &TapeUuid,
    busy_bays: &HashSet<u16>,
) -> Result<ActorMount, Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| {
            Status::not_found(format!(
                "tape UUID {} is not registered in the catalog",
                bytes_to_hex(tape_uuid)
            ))
        })?;
    let barcode = tape.voltag.clone();
    let voltag = barcode.clone().unwrap_or_else(|| "<no-voltag>".to_string());
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library not found"))?;
    let library = snapshot
        .report
        .libraries
        .iter()
        .find(|library| library.serial == library_serial)
        .ok_or_else(|| Status::not_found(format!("library {library_serial} not found")))?;
    let mut mount = resolve_actor_mount_from_library(library, voltag.as_str(), busy_bays)?;
    mount.library_serial = library_serial.to_string();
    mount.barcode = barcode;
    if index
        .list_drives(true, true)
        .map_err(status_from_state_error)?
        .into_iter()
        .any(|drive| {
            drive.last_library_serial.as_deref() == Some(library_serial)
                && drive.last_element_address == Some(i64::from(mount.bay))
                && drive.state == "retired"
        })
    {
        return Err(Status::failed_precondition(
            "resolved drive is retired and excluded from mount resolution",
        ));
    }
    if let Some(drive) = index
        .get_actionable_drive_at(library_serial, i64::from(mount.bay))
        .map_err(status_from_state_error)?
    {
        mount.drive_uuid = Some(drive.drive_uuid);
        mount.drive_serial = Some(drive.serial);
    }
    Ok(mount)
}

fn ensure_actor_mount_media_readiness_admitted(
    state: &ApiState,
    library_serial: &str,
    mount: &ActorMount,
) -> Result<(), Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    crate::ensure_media_readiness_admitted(
        &index,
        "open session",
        library_serial,
        Some(mount.bay),
        mount.barcode.as_deref(),
        actor_mount_may_use_library_robotics(mount),
    )
}

fn actor_mount_may_use_library_robotics(mount: &ActorMount) -> bool {
    mount.source_slot.is_some() || mount.home_slot.is_some()
}

fn ensure_mounted_session_media_readiness_admitted(
    state: &ApiState,
    action: &str,
    mounted: &crate::write_owner::MountedSession,
) -> Result<(), Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    crate::ensure_media_readiness_admitted(
        &index,
        action,
        mounted.library_serial.as_str(),
        Some(mounted.bay),
        mounted.barcode.as_deref(),
        mounted.home_slot.is_some(),
    )
}

fn resolve_actor_mount_from_library(
    library: &Library,
    voltag: &str,
    busy_bays: &HashSet<u16>,
) -> Result<ActorMount, Status> {
    if let Some(bay) = library
        .drive_bays
        .iter()
        .find(|bay| {
            bay.loaded
                && bay.loaded_tape.as_deref() == Some(voltag)
                && busy_bays.contains(&bay.element_address)
        })
        .map(|bay| bay.element_address)
    {
        return Err(Status::failed_precondition(format!(
            "drive bay 0x{bay:04x} is busy"
        )));
    }

    let masked_library;
    let resolution_library = if busy_bays.is_empty() {
        library
    } else {
        masked_library = library_with_pool_busy_bays_hidden(library, busy_bays);
        &masked_library
    };

    match resolve_load_target(resolution_library, voltag) {
        Ok(LoadPlan::AlreadyLoaded { bay }) => {
            let home_slot = library
                .drive_bays
                .iter()
                .find(|drive| drive.element_address == bay)
                .and_then(|drive| drive.source_slot);
            Ok(ActorMount {
                bay,
                barcode: Some(voltag.to_string()),
                source_slot: None,
                home_slot,
                needs_drive_load: false,
                library_serial: String::new(),
                drive_uuid: None,
                drive_serial: None,
            })
        }
        Ok(LoadPlan::Load { slot, bay }) => {
            ensure_target_bay_can_receive(library, bay)?;
            Ok(ActorMount {
                bay,
                barcode: Some(voltag.to_string()),
                source_slot: Some(slot),
                home_slot: Some(slot),
                needs_drive_load: true,
                library_serial: String::new(),
                drive_uuid: None,
                drive_serial: None,
            })
        }
        Err(LoadError::NotInLibrary) => Err(Status::not_found(format!(
            "cartridge {voltag} not found in library inventory (slot or drive)"
        ))),
        Err(LoadError::NoFreeDrive) => Err(Status::failed_precondition(format!(
            "no free drive bay available to load cartridge {voltag}"
        ))),
        Err(err) => Err(Status::internal(format!(
            "resolve load target for cartridge {voltag}: {err}"
        ))),
    }
}

fn library_with_pool_busy_bays_hidden(library: &Library, busy_bays: &HashSet<u16>) -> Library {
    let mut masked = library.clone();
    for bay in &mut masked.drive_bays {
        if busy_bays.contains(&bay.element_address) {
            bay.loaded = true;
            bay.loaded_tape = Some("<pool-reserved>".to_string());
        }
    }
    masked
}

fn ensure_target_bay_can_receive(
    library: &remanence_library::Library,
    target_bay: u16,
) -> Result<(), Status> {
    let bay = library
        .drive_bays
        .iter()
        .find(|drive| drive.element_address == target_bay)
        .ok_or_else(|| Status::not_found(format!("drive bay 0x{target_bay:04x} not found")))?;
    if bay.loaded {
        return Err(Status::failed_precondition(format!(
            "drive bay 0x{target_bay:04x} is not empty"
        )));
    }
    Ok(())
}

fn uuid_from_proto(value: &[u8], field: &str) -> Result<Uuid, Status> {
    Uuid::from_slice(value).map_err(|_| Status::invalid_argument(format!("{field} must be a UUID")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use remanence_library::scsi::{DeviceType, Inquiry};
    use remanence_library::{
        DriveBay, ElementLayout, IdentitySource, IePort, InstalledDrive, Slot,
    };
    use std::path::PathBuf;

    #[test]
    fn actor_mount_resolution_skips_pool_busy_bay_for_slot_load() {
        let library = test_library(
            vec![
                drive_bay(0x0101, false, None),
                drive_bay(0x0102, false, None),
            ],
            vec![slot(0x0401, "RMN001L9")],
        );
        let mount =
            resolve_actor_mount_from_library(&library, "RMN001L9", &HashSet::from([0x0101]))
                .expect("resolve mount");

        assert_eq!(mount.bay, 0x0102);
        assert_eq!(mount.source_slot, Some(0x0401));
        assert_eq!(mount.home_slot, Some(0x0401));
        assert!(actor_mount_may_use_library_robotics(&mount));
        assert!(mount.needs_drive_load);
    }

    #[test]
    fn already_loaded_actor_mount_with_home_slot_may_use_robotics_on_close() {
        let library = test_library(
            vec![drive_bay(0x0101, true, Some("RMN001L9"))],
            vec![slot(0x0401, "OTHERL9")],
        );
        let mount = resolve_actor_mount_from_library(&library, "RMN001L9", &HashSet::new())
            .expect("resolve loaded mount");

        assert_eq!(mount.bay, 0x0101);
        assert_eq!(mount.source_slot, None);
        assert_eq!(mount.home_slot, Some(0x0401));
        assert!(actor_mount_may_use_library_robotics(&mount));
        assert!(!mount.needs_drive_load);
    }

    #[test]
    fn actor_mount_resolution_rejects_already_loaded_busy_bay() {
        let library = test_library(
            vec![
                drive_bay(0x0101, true, Some("RMN001L9")),
                drive_bay(0x0102, false, None),
            ],
            vec![],
        );

        let err = resolve_actor_mount_from_library(&library, "RMN001L9", &HashSet::from([0x0101]))
            .expect_err("busy loaded bay must not be reused");

        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("busy"), "{err}");
    }

    fn test_library(drive_bays: Vec<DriveBay>, slots: Vec<Slot>) -> Library {
        Library {
            serial: "LIB001".to_string(),
            changer_sg: PathBuf::from("/dev/sg0"),
            changer_sysfs: PathBuf::from("/sys/test"),
            changer_inquiry: Inquiry {
                device_type: DeviceType::MediumChanger,
                peripheral_qualifier: 0,
                removable: true,
                version: 7,
                response_data_format: 2,
                additional_length: 31,
                vendor: *b"HPE     ",
                product: *b"MSL3040         ",
                revision: *b"6.40",
            },
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x0101,
                drive_count: drive_bays.len() as u16,
                slot_start: 0x0401,
                slot_count: slots.len() as u16,
                ie_start: 0x0010,
                ie_count: 0,
            },
            drive_bays,
            slots,
            ie_ports: Vec::<IePort>::new(),
        }
    }

    fn drive_bay(element_address: u16, loaded: bool, loaded_tape: Option<&str>) -> DriveBay {
        DriveBay {
            element_address,
            accessible: true,
            installed: Some(InstalledDrive {
                serial: format!("DRV{element_address:04x}"),
                identity_source: IdentitySource::DvcidInline,
                vendor: Some("HPE".to_string()),
                product: Some("Ultrium 9-SCSI".to_string()),
                revision: Some("R1.0".to_string()),
                sg_path: Some(PathBuf::from(format!("/dev/sg{element_address}"))),
                sysfs_path: None,
            }),
            loaded,
            loaded_tape: loaded_tape.map(str::to_string),
            source_slot: Some(0x0401),
        }
    }

    fn slot(element_address: u16, cartridge: &str) -> Slot {
        Slot {
            element_address,
            accessible: true,
            full: true,
            cartridge: Some(cartridge.to_string()),
        }
    }
}
