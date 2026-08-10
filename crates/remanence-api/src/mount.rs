//! Hardware mount bridge shared by CLI and Layer 5 actor orchestration.

use remanence_library::{
    resolve_load_target, AccessPolicy, DriveHandle, Library, LibraryHandle, LoadError, LoadPlan,
};
use remanence_state::{CatalogIndex, StateError};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use uuid::Uuid;

use crate::pool_write::select_tape_in_pool_for_write_session;
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

/// Write-session target: pool-mode selection or an operator-pinned tape.
///
/// Both variants converge on the same reserve → mount → drive-actor open
/// path below; pinning changes only tape *selection* and never bypasses
/// *admission* (writability, fences, batch eligibility), media readiness,
/// BOT identity proof, or session audit recording. The pinned variant's
/// pool guard is validated in [`admit_pinned_tape_for_write_session`]:
/// pools carry copy-class segregation, so a guard mismatch is a refusal.
#[derive(Clone, Debug)]
pub(crate) enum WriteSessionTarget {
    Pool {
        pool_id: String,
    },
    PinnedTape {
        tape_uuid: TapeUuid,
        required_pool_id: String,
    },
}

impl WriteSessionTarget {
    fn pool_id(&self) -> &str {
        match self {
            Self::Pool { pool_id } => pool_id,
            Self::PinnedTape {
                required_pool_id, ..
            } => required_pool_id,
        }
    }

    fn kind(&self) -> pb::write_session::TargetKind {
        match self {
            Self::Pool { .. } => pb::write_session::TargetKind::WriteSessionTargetKindPool,
            Self::PinnedTape { .. } => {
                pb::write_session::TargetKind::WriteSessionTargetKindPinnedTape
            }
        }
    }
}

pub(crate) async fn open_write_session(
    state: &ApiState,
    target: WriteSessionTarget,
    library_serial: String,
) -> Result<pb::WriteSession, Status> {
    let state = state.clone();
    await_critical_task(
        "open_write_session",
        tokio::spawn(
            async move { open_write_session_critical(state, target, library_serial).await },
        ),
    )
    .await
}

async fn open_write_session_critical(
    state: ApiState,
    target: WriteSessionTarget,
    library_serial: String,
) -> Result<pb::WriteSession, Status> {
    let pool = state.drive_pool()?.clone();
    open_write_session_reserved(&state, &pool, target, library_serial).await
}

async fn open_write_session_reserved(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    target: WriteSessionTarget,
    library_serial: String,
) -> Result<pb::WriteSession, Status> {
    enum ReservedWriteDisposition {
        Writable {
            selected: crate::SelectedTape,
            reservation: crate::write_owner::TapeReservation,
        },
        HostOnlyTerminalRecovery {
            selected: crate::SelectedTape,
            reservation: crate::write_owner::TapeReservation,
        },
    }

    let pool_cfg = state.pool_config(target.pool_id())?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    let select_started = Instant::now();
    let mut select_attempts = 0u64;
    let disposition = match &target {
        WriteSessionTarget::Pool { .. } => loop {
            select_attempts = select_attempts.saturating_add(1);
            let selected = select_tape_in_pool_for_write_session(
                &index,
                &pool_cfg,
                0,
                &pool.mounted_tape_uuids(),
                state.checkpoint_journal_dir.as_path(),
            )
            .map_err(crate::write_owner::status_from_select_tape_error)?;
            match pool.reserve_tape(selected.tape_uuid) {
                Ok(reservation) => {
                    break ReservedWriteDisposition::Writable {
                        selected,
                        reservation,
                    };
                }
                Err(err) if err.code() == tonic::Code::FailedPrecondition => continue,
                Err(err) => return Err(err),
            }
        },
        WriteSessionTarget::PinnedTape {
            tape_uuid,
            required_pool_id,
        } => {
            select_attempts = 1;
            // Own the exact tape before consulting the exceptional post-C
            // admission path. Recovery in this runtime uses the same owner,
            // so it cannot finalize the tape between admission and preflight.
            let reservation = pool.reserve_tape(*tape_uuid)?;
            let disposition = crate::pool_write::admit_pinned_tape_for_write_session(
                &index,
                *tape_uuid,
                required_pool_id,
                &pool_cfg,
                state.checkpoint_journal_dir.as_path(),
            )
            .map_err(crate::write_owner::status_from_pinned_tape_error)?;
            match disposition {
                crate::pool_write::PinnedWriteDisposition::Writable(selected) => {
                    ReservedWriteDisposition::Writable {
                        selected,
                        reservation,
                    }
                }
                crate::pool_write::PinnedWriteDisposition::HostOnlyTerminalRecovery(selected) => {
                    ReservedWriteDisposition::HostOnlyTerminalRecovery {
                        selected,
                        reservation,
                    }
                }
            }
        }
    };
    let select_elapsed = select_started.elapsed();
    let selected = match &disposition {
        ReservedWriteDisposition::Writable { selected, .. }
        | ReservedWriteDisposition::HostOnlyTerminalRecovery { selected, .. } => selected,
    };
    let tape_uuid = selected.tape_uuid;
    drop(index);
    let mut host_index = CatalogIndex::open(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let host_completion = crate::write_owner::preflight_automatic_terminal_completion(
        &mut host_index,
        crate::write_owner::ManualFinalizePreflightConfig {
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
            audit_dir: state.audit_dir.as_path(),
            audit_fsync: state.audit_fsync,
            audit_append_lock: &state.audit_append_lock,
        },
        selected,
        &pool_cfg,
    )?;
    drop(host_index);
    let (selected, _tape_reservation) = match (disposition, host_completion) {
        (
            ReservedWriteDisposition::Writable {
                selected,
                reservation,
            },
            false,
        ) => (selected, reservation),
        (ReservedWriteDisposition::Writable { .. }, true)
        | (ReservedWriteDisposition::HostOnlyTerminalRecovery { .. }, true) => {
            return Err(Status::failed_precondition(format!(
                "tape {} completed terminal finalization during host-only open recovery and cannot accept Objects",
                Uuid::from_bytes(tape_uuid)
            )));
        }
        (ReservedWriteDisposition::HostOnlyTerminalRecovery { reservation, .. }, false) => {
            drop(reservation);
            return Err(Status::failed_precondition(format!(
                "tape {} terminal recovery authority changed during host-only preflight; refusing Object admission without media access",
                Uuid::from_bytes(tape_uuid)
            )));
        }
    };
    let open_started = Instant::now();
    let resolve_started = Instant::now();
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(state, pool, &library_serial, &tape_uuid).await?;
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
                target_kind: target.kind(),
                needs_drive_load: mount.needs_drive_load,
                library_serial: mount.library_serial.clone(),
                barcode: mount.barcode.clone(),
                source_slot: mount.source_slot,
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
            if should_compensate_open_mount(&err) {
                compensate_open_mount(pool, &mount).await;
            }
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

/// Inventory constraint used to select the mount for a read-session open.
///
/// Both variants converge on the same drive-actor open path below; the pinned
/// variant changes only mount selection and never bypasses readiness, media
/// fencing, BOT identity proof, or session audit recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadSessionTarget {
    Tape {
        tape_uuid: TapeUuid,
    },
    LoadedDrive {
        tape_uuid: TapeUuid,
        library_serial: String,
        bay: u16,
    },
}

/// Authenticated manual close-out request before catalog assignment is reread
/// under the shared per-tape owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManualFinalizeTapeAdmission {
    pub(crate) candidate_operation_id: Uuid,
    pub(crate) actor: remanence_state::AuditActor,
    pub(crate) actor_fingerprint: String,
    pub(crate) idempotency_key: Uuid,
    pub(crate) request_fingerprint: [u8; 32],
    pub(crate) tape_uuid: TapeUuid,
    pub(crate) expected_pool_id: Option<String>,
    pub(crate) reason: String,
}

/// Read the terminal index of one exact tape under the same tape/drive owner
/// used by ordinary read sessions. The actor performs only readiness, identity,
/// fixed-read configuration, and read/position commands.
pub(crate) async fn tape_inventory(
    state: &ApiState,
    tape_uuid: TapeUuid,
) -> Result<ReceiverStream<Result<pb::TapeInventoryStreamItem, Status>>, Status> {
    state.drive_pool()?;
    let (stream_tx, stream_rx) = mpsc::channel(16);
    let state = state.clone();
    tokio::spawn(async move {
        let error_tx = stream_tx.clone();
        if let Err(error) = tape_inventory_critical(state, tape_uuid, stream_tx).await {
            let _ = error_tx.send(Err(error)).await;
        }
    });
    Ok(ReceiverStream::new(stream_rx))
}

async fn tape_inventory_critical(
    state: ApiState,
    tape_uuid: TapeUuid,
    stream_tx: mpsc::Sender<Result<pb::TapeInventoryStreamItem, Status>>,
) -> Result<(), Status> {
    let pool = state.drive_pool()?.clone();
    let _tape_reservation = pool.reserve_tape(tape_uuid)?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if tape.kind != "data" {
        return Err(Status::failed_precondition(format!(
            "tape is a {} cartridge, not a data tape",
            tape.kind
        )));
    }
    let library_serial = state
        .default_library_serial
        .as_deref()
        .map(|serial| serial.as_str().to_string())
        .ok_or_else(|| {
            Status::invalid_argument(
                "tape inventory requires exactly one configured library in this slice",
            )
        })?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?;
    let drive = pool.drive_tx(mount.bay)?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, slot, mount.bay).await {
            drop(drive_reservation);
            return Err(error);
        }
    }
    let (reply, response) = oneshot::channel();
    if drive
        .send(crate::write_owner::DriveCommand::TapeInventory {
            tape_uuid,
            needs_drive_load: mount.needs_drive_load,
            library_serial: mount.library_serial.clone(),
            barcode: mount.barcode.clone(),
            source_slot: mount.source_slot,
            drive_serial: mount.drive_serial.clone(),
            stream_tx,
            reply,
        })
        .await
        .is_err()
    {
        compensate_open_mount(&pool, &mount).await;
        drop(drive_reservation);
        return Err(Status::unavailable("drive actor is unavailable"));
    }
    let result = response
        .await
        .map_err(|_| Status::unavailable("drive actor stopped during tape inventory"))?;
    if let Some(home_slot) = mount.home_slot {
        let parked = pool.park_cartridge(crate::write_owner::SeatedCartridge {
            bay: mount.bay,
            library_serial: mount.library_serial,
            barcode: mount.barcode,
            home_slot,
            tape_uuid: Some(tape_uuid),
            prior_session_id: None,
        });
        schedule_idle_dismount(state, parked);
    }
    drop(drive_reservation);
    result
}

/// Run the slow measured-prefix/full-terminal verification under the exact
/// tape owner. Unlike [`tape_inventory`], this walks from BOT and reads every
/// record in all three replicas and both separation extents.
pub(crate) async fn verify_tape_index(
    state: &ApiState,
    tape_uuid: TapeUuid,
) -> Result<pb::TapeIndexVerification, Status> {
    let state = state.clone();
    await_critical_task(
        "verify_tape_index",
        tokio::spawn(async move { verify_tape_index_critical(state, tape_uuid).await }),
    )
    .await
}

async fn verify_tape_index_critical(
    state: ApiState,
    tape_uuid: TapeUuid,
) -> Result<pb::TapeIndexVerification, Status> {
    let pool = state.drive_pool()?.clone();
    let _tape_reservation = pool.reserve_tape(tape_uuid)?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if tape.kind != "data" {
        return Err(Status::failed_precondition(format!(
            "tape is a {} cartridge, not a data tape",
            tape.kind
        )));
    }
    let library_serial = state
        .default_library_serial
        .as_deref()
        .map(|serial| serial.as_str().to_string())
        .ok_or_else(|| {
            Status::invalid_argument(
                "tape index verification requires exactly one configured library in this slice",
            )
        })?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?;
    let drive = pool.drive_tx(mount.bay)?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, slot, mount.bay).await {
            drop(drive_reservation);
            return Err(error);
        }
    }
    let (reply, response) = oneshot::channel();
    if drive
        .send(crate::write_owner::DriveCommand::VerifyTapeIndex {
            tape_uuid,
            needs_drive_load: mount.needs_drive_load,
            library_serial: mount.library_serial.clone(),
            barcode: mount.barcode.clone(),
            source_slot: mount.source_slot,
            drive_serial: mount.drive_serial.clone(),
            reply,
        })
        .await
        .is_err()
    {
        compensate_open_mount(&pool, &mount).await;
        drop(drive_reservation);
        return Err(Status::unavailable("drive actor is unavailable"));
    }
    let result = response
        .await
        .map_err(|_| Status::unavailable("drive actor stopped during tape index verification"))?;
    if let Some(home_slot) = mount.home_slot {
        let parked = pool.park_cartridge(crate::write_owner::SeatedCartridge {
            bay: mount.bay,
            library_serial: mount.library_serial,
            barcode: mount.barcode,
            home_slot,
            tape_uuid: Some(tape_uuid),
            prior_session_id: None,
        });
        schedule_idle_dismount(state, parked);
    }
    drop(drive_reservation);
    result
}

/// Acquire the same exact-tape owner used by append and dispatch terminal
/// close-out only after the pool assignment and generation have been captured.
/// A conflicting Object/read session therefore returns busy before audit,
/// intent publication, changer motion, or tape motion.
pub(crate) async fn manual_finalize_tape(
    state: &ApiState,
    mut admission: ManualFinalizeTapeAdmission,
) -> Result<crate::write_owner::ManualFinalizeTapeResult, Status> {
    let pool = state.drive_pool()?.clone();
    let tape_reservation = match pool.reserve_tape(admission.tape_uuid) {
        Ok(reservation) => reservation,
        Err(error)
            if error.code() == tonic::Code::FailedPrecondition
                && error.message() == "tape is already mounted" =>
        {
            if let Some(reply) = rejoin_accepted_manual_finalization(state, &admission)? {
                return Ok(crate::write_owner::ManualFinalizeTapeResult::Accepted(
                    reply,
                ));
            }
            return Ok(crate::write_owner::ManualFinalizeTapeResult::Busy);
        }
        Err(error) => return Err(error),
    };
    let mut index = CatalogIndex::open(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(&admission.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if tape.kind != "data" {
        return Err(Status::failed_precondition(format!(
            "tape is a {} cartridge, not a data tape",
            tape.kind
        )));
    }
    let assignment = index
        .get_tape_assignment_snapshot(&admission.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape assignment vanished"))?;
    match (&assignment.pool_id, &admission.expected_pool_id) {
        (Some(actual), Some(expected)) if actual == expected => {}
        (None, None) => {}
        (Some(actual), None) => {
            return Err(Status::invalid_argument(format!(
                "expected_pool_id is required for pooled tape assigned to {actual:?}"
            )));
        }
        (actual, expected) => {
            return Err(Status::failed_precondition(format!(
                "expected_pool_id {expected:?} does not match current assignment {actual:?}"
            )));
        }
    }
    if let Some(scope) = index
        .idempotency_scope_record(
            admission.actor_fingerprint.as_str(),
            "finalize_tape",
            admission.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
    {
        if scope.request_fingerprint.as_slice() != admission.request_fingerprint {
            return Err(Status::already_exists(
                "FinalizeTape idempotency key is already bound to a different request",
            ));
        }
        admission.candidate_operation_id = scope.operation_id;
    }
    if !matches!(
        tape.state.as_str(),
        "ready"
            | "ingested"
            | "finalizing"
            | "finalized"
            | "sealed"
            | "finalized_degraded"
            | "recovery_required"
    ) {
        return Err(Status::failed_precondition(format!(
            "tape state {:?} cannot be terminally finalized",
            tape.state
        )));
    }
    let geometry_label = assignment.pool_id.as_deref().unwrap_or("(unpooled)");
    let (block_size, parity_config) =
        crate::pool_write::selected_tape_geometry(&tape, geometry_label)
            .map_err(crate::write_owner::status_from_select_tape_error)?;
    let pool_config = match assignment.pool_id.as_deref() {
        Some(pool_id) => Some(state.pool_config(pool_id)?),
        None => None,
    };
    let mut actor_request = crate::write_owner::ManualFinalizeTapeActorRequest {
        candidate_operation_id: admission.candidate_operation_id,
        actor: admission.actor,
        actor_fingerprint: admission.actor_fingerprint,
        idempotency_key: admission.idempotency_key,
        request_fingerprint: admission.request_fingerprint,
        tape_uuid: admission.tape_uuid,
        expected_pool_id: admission.expected_pool_id,
        assignment_generation: assignment.assignment_generation,
        reason: admission.reason,
        block_size,
        parity_config,
        pool_config,
    };
    if let Some(reply) = crate::write_owner::preflight_manual_finalize_tape(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
            audit_dir: state.audit_dir.as_path(),
            audit_fsync: state.audit_fsync,
            audit_append_lock: &state.audit_append_lock,
        },
        tape.voltag.as_deref(),
        &mut actor_request,
    )? {
        return Ok(crate::write_owner::ManualFinalizeTapeResult::Accepted(
            reply,
        ));
    }
    let projection = index
        .terminal_finalization(&admission.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| {
            Status::internal("durably accepted FinalizeTape has no finalization projection")
        })?;
    let accepted = crate::write_owner::ManualFinalizeTapeActorReply {
        operation_id: actor_request.candidate_operation_id,
        projection,
    };
    drop(index);

    let worker_state = state.clone();
    tokio::spawn(async move {
        let tape_uuid = actor_request.tape_uuid;
        let operation_id = actor_request.candidate_operation_id;
        if let Err(error) =
            run_manual_finalize_worker(worker_state, pool, tape_reservation, actor_request).await
        {
            tracing::error!(
                tape_uuid = %Uuid::from_bytes(tape_uuid),
                %operation_id,
                %error,
                "accepted manual terminal finalization worker stopped before completion"
            );
        }
    });
    Ok(crate::write_owner::ManualFinalizeTapeResult::Accepted(
        accepted,
    ))
}

/// Rejoin only an exact, durably accepted manual operation while its worker
/// owns the tape. Unrelated owners retain the typed BUSY/no-motion result.
fn rejoin_accepted_manual_finalization(
    state: &ApiState,
    admission: &ManualFinalizeTapeAdmission,
) -> Result<Option<crate::write_owner::ManualFinalizeTapeActorReply>, Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let Some(scope) = index
        .idempotency_scope_record(
            admission.actor_fingerprint.as_str(),
            "finalize_tape",
            admission.idempotency_key,
        )
        .map_err(crate::status_from_state_error)?
    else {
        return Ok(None);
    };
    if scope.request_fingerprint.as_slice() != admission.request_fingerprint {
        return Err(Status::already_exists(
            "FinalizeTape idempotency key is already bound to a different request",
        ));
    }
    let Some(projection) = index
        .terminal_finalization(&admission.tape_uuid)
        .map_err(crate::status_from_state_error)?
    else {
        return Ok(None);
    };
    if projection.operation_id != Some(scope.operation_id) {
        return Ok(None);
    }
    Ok(Some(crate::write_owner::ManualFinalizeTapeActorReply {
        operation_id: scope.operation_id,
        projection,
    }))
}

/// Execute the physical manual-finalization tail while retaining exact-tape
/// ownership transferred from durable admission or startup recovery.
async fn run_manual_finalize_worker(
    state: ApiState,
    pool: crate::write_owner::DrivePool,
    _tape_reservation: crate::write_owner::TapeReservation,
    mut actor_request: crate::write_owner::ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let tape_uuid = actor_request.tape_uuid;
    let mut index = CatalogIndex::open(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let barcode = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .and_then(|tape| tape.voltag);
    if crate::write_owner::preflight_manual_finalize_tape(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
            audit_dir: state.audit_dir.as_path(),
            audit_fsync: state.audit_fsync,
            audit_append_lock: &state.audit_append_lock,
        },
        barcode.as_deref(),
        &mut actor_request,
    )?
    .is_some()
    {
        return Ok(());
    }
    drop(index);
    let library_serial = state
        .default_library_serial
        .as_deref()
        .map(|serial| serial.as_str().to_string())
        .ok_or_else(|| {
            Status::invalid_argument(
                "manual tape finalization requires exactly one configured library in this slice",
            )
        })?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(&state, &pool, library_serial.as_str(), &tape_uuid).await?;
    let drive = pool.drive_tx(mount.bay)?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, slot, mount.bay).await {
            drop(drive_reservation);
            return Err(error);
        }
    }
    let (reply, response) = oneshot::channel();
    let command = crate::write_owner::DriveCommand::FinalizeTape {
        request: actor_request,
        needs_drive_load: mount.needs_drive_load,
        library_serial: mount.library_serial.clone(),
        barcode: mount.barcode.clone(),
        source_slot: mount.source_slot,
        drive_uuid: mount.drive_uuid.clone(),
        drive_serial: mount.drive_serial.clone(),
        reply,
    };
    if drive.send(command).await.is_err() {
        compensate_open_mount(&pool, &mount).await;
        drop(drive_reservation);
        return Err(Status::unavailable("drive actor is unavailable"));
    }
    let result = response
        .await
        .map_err(|_| Status::unavailable("drive actor stopped during tape finalization"))?;
    if let Some(home_slot) = mount.home_slot {
        let parked = pool.park_cartridge(crate::write_owner::SeatedCartridge {
            bay: mount.bay,
            library_serial: mount.library_serial,
            barcode: mount.barcode,
            home_slot,
            tape_uuid: Some(tape_uuid),
            prior_session_id: None,
        });
        schedule_idle_dismount(state.clone(), parked);
    }
    drop(drive_reservation);
    result.map(|_| ())
}

/// Resume every terminal finalization that was durable when the process
/// stopped, including accepted manual operations whose callers disconnected.
pub(crate) fn spawn_startup_terminal_recoveries(state: ApiState) {
    tokio::spawn(async move {
        let paths =
            match remanence_state::list_checkpoint_journals(state.checkpoint_journal_dir.as_path())
            {
                Ok(paths) => paths,
                Err(error) => {
                    tracing::error!(%error, "cannot enumerate startup terminal recoveries");
                    return;
                }
            };
        for path in paths {
            let tape_uuid = match remanence_state::tape_uuid_from_checkpoint_path(&path) {
                Ok(tape_uuid) => tape_uuid,
                Err(error) => {
                    tracing::error!(path = %path.display(), %error, "invalid checkpoint journal path during startup recovery");
                    continue;
                }
            };
            let journal = match remanence_state::FileCheckpointJournal::open(
                state.checkpoint_journal_dir.as_path(),
                tape_uuid,
            ) {
                Ok(journal) => journal,
                Err(error) => {
                    tracing::error!(tape_uuid = %Uuid::from_bytes(tape_uuid), %error, "cannot open checkpoint journal during startup recovery");
                    continue;
                }
            };
            let intent = match journal.terminal_finalization_intent() {
                Ok(intent) => intent,
                Err(error) => {
                    tracing::error!(tape_uuid = %Uuid::from_bytes(tape_uuid), %error, "cannot inspect terminal intent during startup recovery");
                    continue;
                }
            };
            let Some(intent) = intent else {
                continue;
            };
            let recovery = if intent.manual.is_some() {
                recover_manual_terminal_tape(&state, &intent).await
            } else {
                recover_automatic_terminal_tape(&state, tape_uuid).await
            };
            if let Err(error) = recovery {
                tracing::error!(
                    tape_uuid = %Uuid::from_bytes(tape_uuid),
                    progress = ?intent.progress,
                    %error,
                    "terminal finalization did not complete during startup recovery"
                );
            }
        }
    });
}

/// Reconstruct an accepted manual operation exclusively from durable intent
/// and current guarded catalog/config data, then resume its physical tail.
async fn recover_manual_terminal_tape(
    state: &ApiState,
    intent: &remanence_state::TerminalFinalizationIntent,
) -> Result<(), Status> {
    let pool = state.drive_pool()?.clone();
    let tape_reservation = pool.reserve_tape(intent.tape_uuid)?;
    let request = manual_finalize_request_from_intent(state, intent)?;
    run_manual_finalize_worker(state.clone(), pool, tape_reservation, request).await
}

/// Rehydrate the original exact request without trusting transient process
/// memory or synthesizing a new operation identity.
fn manual_finalize_request_from_intent(
    state: &ApiState,
    intent: &remanence_state::TerminalFinalizationIntent,
) -> Result<crate::write_owner::ManualFinalizeTapeActorRequest, Status> {
    let manual = intent.manual.as_ref().ok_or_else(|| {
        Status::failed_precondition("startup manual recovery intent has no operation identity")
    })?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(&intent.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("manual terminal-recovery tape not found"))?;
    let assignment = index
        .get_tape_assignment_snapshot(&intent.tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("manual terminal-recovery assignment vanished"))?;
    if assignment.pool_id != manual.assigned_pool_id
        || assignment.assignment_generation != manual.assignment_generation
    {
        return Err(Status::failed_precondition(
            "manual terminal-recovery assignment no longer matches durable admission",
        ));
    }
    let geometry_label = assignment.pool_id.as_deref().unwrap_or("(unpooled)");
    let (block_size, parity_config) =
        crate::pool_write::selected_tape_geometry(&tape, geometry_label)
            .map_err(crate::write_owner::status_from_select_tape_error)?;
    let pool_config = match manual.expected_pool_id.as_deref() {
        Some(pool_id) => Some(state.pool_config(pool_id)?),
        None => None,
    };
    Ok(crate::write_owner::ManualFinalizeTapeActorRequest {
        candidate_operation_id: Uuid::from_bytes(manual.operation_id),
        actor: remanence_state::AuditActor::System,
        actor_fingerprint: manual.actor_fingerprint.clone(),
        idempotency_key: Uuid::from_bytes(manual.idempotency_key),
        request_fingerprint: manual.request_fingerprint,
        tape_uuid: intent.tape_uuid,
        expected_pool_id: manual.expected_pool_id.clone(),
        assignment_generation: manual.assignment_generation,
        reason: manual.reason.clone(),
        block_size,
        parity_config,
        pool_config,
    })
}

async fn recover_automatic_terminal_tape(
    state: &ApiState,
    tape_uuid: TapeUuid,
) -> Result<(), Status> {
    let pool = state.drive_pool()?.clone();
    let _tape_reservation = pool.reserve_tape(tape_uuid)?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("terminal-recovery tape not found"))?;
    let pool_id = tape.pool_id.as_deref().ok_or_else(|| {
        Status::failed_precondition(
            "automatic terminal recovery requires the original pool assignment",
        )
    })?;
    let pool_cfg = state.pool_config(pool_id)?;
    let (block_size, parity_config) = crate::pool_write::selected_tape_geometry(&tape, pool_id)
        .map_err(crate::write_owner::status_from_select_tape_error)?;
    let selected = crate::SelectedTape {
        pool_id: pool_id.to_string(),
        tape_uuid,
        block_size,
        parity_config,
    };
    drop(index);
    let mut host_index = CatalogIndex::open(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    if crate::write_owner::preflight_automatic_terminal_completion(
        &mut host_index,
        crate::write_owner::ManualFinalizePreflightConfig {
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
            audit_dir: state.audit_dir.as_path(),
            audit_fsync: state.audit_fsync,
            audit_append_lock: &state.audit_append_lock,
        },
        &selected,
        &pool_cfg,
    )? {
        return Ok(());
    }
    drop(host_index);
    let library_serial = state
        .default_library_serial
        .as_deref()
        .map(|serial| serial.as_str().to_string())
        .ok_or_else(|| {
            Status::invalid_argument(
                "automatic terminal recovery requires exactly one configured library",
            )
        })?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(state, &pool, &library_serial, &tape_uuid).await?;
    let drive = pool.drive_tx(mount.bay)?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, slot, mount.bay).await {
            drop(drive_reservation);
            return Err(error);
        }
    }
    let (reply, response) = oneshot::channel();
    if drive
        .send(crate::write_owner::DriveCommand::OpenWrite {
            pool_cfg,
            selected,
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPinnedTape,
            needs_drive_load: mount.needs_drive_load,
            library_serial: mount.library_serial.clone(),
            barcode: mount.barcode.clone(),
            source_slot: mount.source_slot,
            drive_uuid: mount.drive_uuid.clone(),
            drive_serial: mount.drive_serial.clone(),
            reply,
        })
        .await
        .is_err()
    {
        compensate_open_mount(&pool, &mount).await;
        drop(drive_reservation);
        return Err(Status::unavailable(
            "drive actor is unavailable for automatic terminal recovery",
        ));
    }
    let actor_result = response.await.map_err(|_| {
        Status::unavailable("drive actor stopped during automatic terminal recovery")
    })?;
    if let Some(home_slot) = mount.home_slot {
        let parked = pool.park_cartridge(crate::write_owner::SeatedCartridge {
            bay: mount.bay,
            library_serial: mount.library_serial,
            barcode: mount.barcode,
            home_slot,
            tape_uuid: Some(tape_uuid),
            prior_session_id: None,
        });
        schedule_idle_dismount(state.clone(), parked);
    }
    drop(drive_reservation);

    let projection = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?
        .terminal_finalization(&tape_uuid)
        .map_err(crate::status_from_state_error)?;
    if projection.is_some_and(|projection| {
        projection.outcome == remanence_state::TerminalFinalizationOutcome::Finalized
    }) {
        return Ok(());
    }
    match actor_result {
        Ok(_) => Err(Status::internal(
            "automatic terminal recovery unexpectedly opened an Object session",
        )),
        Err(error) => Err(error),
    }
}

impl ReadSessionTarget {
    pub(crate) fn tape_uuid(&self) -> TapeUuid {
        match self {
            Self::Tape { tape_uuid } | Self::LoadedDrive { tape_uuid, .. } => *tape_uuid,
        }
    }
}

pub(crate) async fn open_read_session(
    state: &ApiState,
    target: ReadSessionTarget,
    resume_target: Option<crate::write_owner::ReadResumeTarget>,
) -> Result<pb::ReadSession, Status> {
    let state = state.clone();
    await_critical_task(
        "open_read_session",
        tokio::spawn(async move { open_read_session_critical(state, target, resume_target).await }),
    )
    .await
}

async fn open_read_session_critical(
    state: ApiState,
    target: ReadSessionTarget,
    resume_target: Option<crate::write_owner::ReadResumeTarget>,
) -> Result<pb::ReadSession, Status> {
    let pool = state.drive_pool()?.clone();
    let tape_uuid = target.tape_uuid();
    let _tape_reservation = pool.reserve_tape(tape_uuid)?;
    let (mount, drive_reservation) = match target {
        ReadSessionTarget::Tape { .. } => {
            let library_serial = state
                .default_library_serial
                .as_deref()
                .map(|serial| serial.as_str().to_string())
                .ok_or_else(|| {
                    Status::invalid_argument(
                        "tape-target read sessions require exactly one configured library in this slice",
                    )
                })?;
            resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?
        }
        ReadSessionTarget::LoadedDrive {
            library_serial,
            bay,
            ..
        } => {
            let reservation = pool.reserve_drive(bay)?;
            let mount = resolve_pinned_actor_mount(&state, &library_serial, bay, &tape_uuid)?;
            ensure_actor_mount_media_readiness_admitted(&state, &library_serial, &mount)?;
            (mount, reservation)
        }
    };
    open_read_session_on_mount(
        &state,
        &pool,
        mount,
        drive_reservation,
        tape_uuid,
        resume_target,
    )
    .await
}

async fn open_read_session_on_mount(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    mount: ActorMount,
    drive_reservation: crate::write_owner::DriveReservation,
    tape_uuid: TapeUuid,
    resume_target: Option<crate::write_owner::ReadResumeTarget>,
) -> Result<pb::ReadSession, Status> {
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
                barcode: mount.barcode.clone(),
                source_slot: mount.source_slot,
                drive_uuid: mount.drive_uuid.clone(),
                drive_serial: mount.drive_serial.clone(),
                resume_target,
                daemon_epoch: state.daemon_epoch,
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
            if should_compensate_open_mount(&err) {
                compensate_open_mount(pool, &mount).await;
            }
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
            source: crate::WriteObjectSource::Path(spool_path),
            archive_path,
            caller_object_id,
            expected_content_sha256,
            live_write_counter,
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    let outcome = reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))??;
    Ok(outcome.record)
}

pub(crate) async fn append_streamed(
    state: &ApiState,
    session_id: Uuid,
    source: crate::StreamedWriteSource,
    archive_path: std::path::PathBuf,
    caller_object_id: String,
    expected_content_sha256: [u8; 32],
) -> Result<crate::write_owner::AppendFinishOutcome, Status> {
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
            source: crate::WriteObjectSource::Streamed(source),
            archive_path,
            caller_object_id,
            expected_content_sha256: Some(expected_content_sha256),
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

pub(crate) async fn checkpoint_write_session(
    state: &ApiState,
    session_id: Uuid,
    trigger: crate::write_owner::CheckpointTrigger,
) -> Result<crate::write_owner::CheckpointActorReply, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::Checkpoint {
            session_id,
            trigger,
            expected_batch_id: None,
            reply: Some(reply_tx),
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))?
}

/// How a write session is being ended. An abort carries the caller's reason
/// when it gave one; a close cannot carry one, which is why this is an enum
/// and not the `abort: bool` plus a reason field it replaced.
pub(crate) enum SessionDisposition {
    Close,
    Abort { reason: Option<String> },
}

impl SessionDisposition {
    fn is_abort(&self) -> bool {
        matches!(self, SessionDisposition::Abort { .. })
    }
}

pub(crate) async fn close_write_session(
    state: &ApiState,
    session_id: Uuid,
) -> Result<pb::WriteSession, Status> {
    close_write_like(state, session_id, SessionDisposition::Close).await
}

pub(crate) async fn abort_write_session(
    state: &ApiState,
    session_id: Uuid,
    reason: Option<String>,
) -> Result<pb::WriteSession, Status> {
    close_write_like(state, session_id, SessionDisposition::Abort { reason }).await
}

async fn close_write_like(
    state: &ApiState,
    session_id: Uuid,
    disposition: SessionDisposition,
) -> Result<pb::WriteSession, Status> {
    let state = state.clone();
    await_critical_task(
        "close_write_session",
        tokio::spawn(
            async move { close_write_like_critical(state, session_id, disposition).await },
        ),
    )
    .await
}

async fn close_write_like_critical(
    state: ApiState,
    session_id: Uuid,
    disposition: SessionDisposition,
) -> Result<pb::WriteSession, Status> {
    let abort = disposition.is_abort();
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(mounted.bay)?;
    let (reply_tx, reply_rx) = oneshot::channel();
    let close_started = Instant::now();
    let actor_close_started = Instant::now();
    ensure_mounted_session_media_readiness_admitted(&state, "close session", &mounted)?;
    if let SessionDisposition::Abort { reason } = disposition {
        drive
            .send(crate::write_owner::DriveCommand::Abort {
                session_id,
                reason,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("drive actor unavailable"))?;
    } else {
        drive
            .send(crate::write_owner::DriveCommand::Close {
                session_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::internal("drive actor unavailable"))?;
    }
    let actor_reply = reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))??;
    let actor_close_elapsed = actor_close_started.elapsed();
    let actor_attributed = actor_reply
        .diagnostics
        .drive_snapshot
        .saturating_add(actor_reply.diagnostics.rewind)
        .saturating_add(actor_reply.diagnostics.ssc_unload)
        .saturating_add(actor_reply.diagnostics.session_audit_projection);
    let actor_unattributed = actor_close_elapsed.saturating_sub(actor_attributed);
    let finish_started = Instant::now();
    finish_mounted_session(&state, &pool, session_id, mounted);
    let finish_elapsed = finish_started.elapsed();
    let close_elapsed = close_started.elapsed();
    tracing::info!(
        target: "remanence_write_diag",
        phase = "close_unmount",
        session_id = %session_id,
        abort,
        unload = "skipped",
        commit_phases_completed_before_close = true,
        commit_phase_scope = "session_accumulated_append_finish",
        append_commits = actor_reply.session.objects_committed,
        filemark_write_drain_ms = crate::diagnostics::duration_ms(
            actor_reply.diagnostics.filemark_write_drain
        ),
        catalog_journal_fsync_ms = crate::diagnostics::duration_ms(
            actor_reply.diagnostics.catalog_journal_fsync
        ),
        drive_snapshot_ms = crate::diagnostics::duration_ms(
            actor_reply.diagnostics.drive_snapshot
        ),
        rewind_ms = crate::diagnostics::duration_ms(actor_reply.diagnostics.rewind),
        separate_rewind_command = false,
        ssc_unload_ms = crate::diagnostics::duration_ms(actor_reply.diagnostics.ssc_unload),
        ssc_unload_includes_rewind = false,
        session_audit_projection_ms = crate::diagnostics::duration_ms(
            actor_reply.diagnostics.session_audit_projection
        ),
        actor_unattributed_ms = crate::diagnostics::duration_ms(actor_unattributed),
        actor_close_ms = crate::diagnostics::duration_ms(actor_close_elapsed),
        park_ms = crate::diagnostics::duration_ms(finish_elapsed),
        changer_move_home_ms = 0.0,
        finish_mount_ms = 0.0,
        elapsed_ms = crate::diagnostics::duration_ms(close_elapsed),
        "remanence_write_diag",
    );
    Ok(actor_reply.session)
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
            reply: reply_tx,
        })
        .await
        .map_err(|_| Status::internal("drive actor unavailable"))?;
    let session = reply_rx
        .await
        .map_err(|_| Status::internal("drive actor dropped reply"))??;
    finish_mounted_session(&state, &pool, session_id, mounted);
    tracing::info!(
        target: "remanence_read_diag",
        phase = "close_unmount",
        session_id = %session_id,
        unload = "skipped",
        "remanence_read_diag",
    );
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
    chunk_tx: crate::read_core::ReadStreamSender,
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
    chunk_tx: crate::read_core::ReadStreamSender,
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

fn finish_mounted_session(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    session_id: Uuid,
    mounted: crate::write_owner::MountedSession,
) {
    if let Some(parked) = pool.finish_session(session_id, mounted) {
        schedule_idle_dismount(state.clone(), parked);
    }
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

fn should_compensate_open_mount(err: &Status) -> bool {
    !err.message().contains("media_readiness_state=")
}

async fn drive_unload(pool: &crate::write_owner::DrivePool, bay: u16) -> Result<Duration, Status> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DismountReason {
    Evicted,
    IdleTimeout,
    Shutdown,
}

const SHUTDOWN_RESERVATION_WAIT: Duration = Duration::from_secs(600);

impl DismountReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Evicted => "evicted",
            Self::IdleTimeout => "idle_timeout",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Perform the one canonical rewind/unload/move-home tail while the caller
/// holds the bay reservation. Session close only parks the cartridge; eviction,
/// idle expiry, and shutdown all converge here.
async fn dismount_reserved_cartridge(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    seated: &crate::write_owner::SeatedCartridge,
    reason: DismountReason,
) -> Result<(), Status> {
    ensure_seated_cartridge_media_readiness_admitted(state, seated, reason)?;
    ensure_seated_cartridge_matches_snapshot(state, seated)?;

    let started = Instant::now();
    let ssc_unload = drive_unload(pool, seated.bay).await?;
    let move_started = Instant::now();
    changer_move(pool, seated.bay, seated.home_slot).await?;
    let changer_move = move_started.elapsed();
    if let Some(parked) = pool.parked_at(seated.bay) {
        pool.forget_parked(&parked);
    }
    let session_id = seated
        .prior_session_id
        .map(|session_id| session_id.to_string())
        .unwrap_or_else(|| "(startup)".to_string());
    tracing::info!(
        target: "remanence_write_diag",
        phase = "close_unmount",
        session_id = %session_id,
        library_serial = %seated.library_serial,
        bay = seated.bay,
        home_slot = seated.home_slot,
        barcode = seated.barcode.as_deref().unwrap_or("(unknown)"),
        unload = reason.as_str(),
        rewind_ms = 0.0,
        separate_rewind_command = false,
        ssc_unload_ms = crate::diagnostics::duration_ms(ssc_unload),
        ssc_unload_includes_rewind = true,
        changer_move_home_ms = crate::diagnostics::duration_ms(changer_move),
        elapsed_ms = crate::diagnostics::duration_ms(started.elapsed()),
        "remanence_write_diag",
    );
    Ok(())
}

fn ensure_seated_cartridge_media_readiness_admitted(
    state: &ApiState,
    seated: &crate::write_owner::SeatedCartridge,
    reason: DismountReason,
) -> Result<(), Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    crate::ensure_media_readiness_admitted(
        &index,
        reason.as_str(),
        seated.library_serial.as_str(),
        Some(seated.bay),
        seated.barcode.as_deref(),
        true,
    )
}

fn ensure_seated_cartridge_matches_snapshot(
    state: &ApiState,
    seated: &crate::write_owner::SeatedCartridge,
) -> Result<(), Status> {
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library not found"))?;
    let bay = snapshot
        .report
        .libraries
        .iter()
        .find(|library| library.serial == seated.library_serial)
        .and_then(|library| {
            library
                .drive_bays
                .iter()
                .find(|bay| bay.element_address == seated.bay)
        })
        .ok_or_else(|| {
            Status::not_found(format!(
                "drive bay 0x{:04x} not found in library {}",
                seated.bay, seated.library_serial
            ))
        })?;
    if !bay.loaded || bay.loaded_tape != seated.barcode || bay.source_slot != Some(seated.home_slot)
    {
        return Err(Status::failed_precondition(format!(
            "drive bay 0x{:04x} seated cartridge changed before dismount",
            seated.bay
        )));
    }
    Ok(())
}

fn schedule_idle_dismount(state: ApiState, parked: crate::write_owner::ParkedCartridge) {
    let timeout_seconds = state.drive_idle_unload_seconds;
    if timeout_seconds == 0 {
        return;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!(
            bay = parked.seated.bay,
            "cannot schedule idle drive unload without a Tokio runtime"
        );
        return;
    };
    runtime.spawn(async move {
        let pool = match state.drive_pool() {
            Ok(pool) => pool.clone(),
            Err(_) => return,
        };
        tokio::time::sleep(Duration::from_secs(timeout_seconds)).await;
        loop {
            if pool.is_shutting_down() || !pool.parked_is_current(&parked) {
                return;
            }
            let reservation = match pool.reserve_drive(parked.seated.bay) {
                Ok(reservation) => reservation,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            if !pool.parked_is_current(&parked) {
                drop(reservation);
                return;
            }
            if let Err(err) = ensure_seated_cartridge_matches_snapshot(&state, &parked.seated) {
                tracing::warn!(
                    bay = parked.seated.bay,
                    error = %err,
                    "discarding stale idle drive record after inventory changed"
                );
                pool.forget_parked(&parked);
                drop(reservation);
                return;
            }
            match dismount_reserved_cartridge(
                &state,
                &pool,
                &parked.seated,
                DismountReason::IdleTimeout,
            )
            .await
            {
                Ok(()) => return,
                Err(err) => tracing::warn!(
                    bay = parked.seated.bay,
                    error = %err,
                    "idle drive unload failed; retrying after the configured timeout"
                ),
            }
            drop(reservation);
            tokio::time::sleep(Duration::from_secs(timeout_seconds)).await;
        }
    });
}

pub(crate) fn spawn_timer_idle_dismount_listener(
    state: ApiState,
    mut parked_rx: tokio::sync::mpsc::UnboundedReceiver<crate::write_owner::ParkedCartridge>,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::error!("cannot start timer-close idle-dismount listener without a Tokio runtime");
        return;
    };
    runtime.spawn(async move {
        while let Some(parked) = parked_rx.recv().await {
            schedule_idle_dismount(state.clone(), parked);
        }
    });
}

pub(crate) fn register_startup_seated_cartridges(
    state: &ApiState,
    report: &remanence_library::DiscoveryReport,
) {
    let Ok(pool) = state.drive_pool() else {
        return;
    };
    let Some(configured_library_serial) = state.default_library_serial.as_deref() else {
        return;
    };
    let voltags = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))
        .and_then(|index| crate::library::voltag_uuid_map(&index))
        .unwrap_or_else(|err| {
            tracing::warn!(error = %err, "cannot join startup seated cartridges to tape UUIDs");
            Default::default()
        });
    for library in report
        .libraries
        .iter()
        .filter(|library| library.serial == configured_library_serial.as_str())
    {
        for bay in &library.drive_bays {
            let Some(home_slot) = bay.source_slot.filter(|_| bay.loaded) else {
                continue;
            };
            if pool.drive_tx(bay.element_address).is_err() {
                continue;
            }
            let tape_uuid = bay
                .loaded_tape
                .as_ref()
                .and_then(|barcode| voltags.get(barcode))
                .and_then(|bytes| <[u8; 16]>::try_from(bytes.as_slice()).ok());
            // Design §6.5, startup/orphan-recovery row: a cartridge
            // already seated when the daemon starts is a load this
            // process never harvested — sessions can open on it with
            // no fresh mount, so no load harvest will run and no real
            // fence gets installed for it. Its fence state is
            // therefore uncertain, and the uncertainty is resolved as
            // false invalidation: durably advance the epoch and leave
            // the volume uncalibrated until a genuine next-load
            // harvest. An old cached map can then never be served for
            // it, whatever happens during this load.
            if let Some(tape_uuid) = tape_uuid {
                if let Err(err) = state
                    .calibration_store()
                    .record_possible_write_recovery(tape_uuid)
                {
                    tracing::warn!(
                        target: "remanence_calibration",
                        tape_uuid = %Uuid::from_bytes(tape_uuid),
                        error = %err,
                        "startup seated-cartridge calibration invalidation failed"
                    );
                }
            }
            let parked = pool.park_cartridge(crate::write_owner::SeatedCartridge {
                bay: bay.element_address,
                library_serial: library.serial.clone(),
                barcode: bay.loaded_tape.clone(),
                home_slot,
                tape_uuid,
                prior_session_id: None,
            });
            schedule_idle_dismount(state.clone(), parked);
        }
    }
}

pub(crate) async fn shutdown_drive_pool(state: &ApiState) -> Result<(), Status> {
    let Ok(pool) = state.drive_pool() else {
        return Ok(());
    };
    pool.begin_shutdown();
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library not found"))?;
    let configured_library_serial = state.default_library_serial.as_deref().ok_or_else(|| {
        Status::failed_precondition("drive pool has no configured library serial")
    })?;
    let library_drive_bays = snapshot
        .report
        .libraries
        .iter()
        .find(|library| library.serial == configured_library_serial.as_str())
        .ok_or_else(|| {
            Status::not_found(format!(
                "configured library {} not found during shutdown",
                configured_library_serial.as_str()
            ))
        })?
        .drive_bays
        .clone();
    drop(snapshot);
    let mut failures = Vec::new();
    for (_, (session_id, mounted)) in pool.sessions_by_bay() {
        let drive = match pool.drive_tx(mounted.bay) {
            Ok(drive) => drive,
            Err(err) => {
                failures.push(format!(
                    "session {session_id}: drive unavailable for shutdown checkpoint: {err}"
                ));
                continue;
            }
        };
        let (checkpoint_tx, checkpoint_rx) = oneshot::channel();
        if drive
            .send(crate::write_owner::DriveCommand::Checkpoint {
                session_id,
                trigger: crate::write_owner::CheckpointTrigger::Shutdown,
                expected_batch_id: None,
                reply: Some(checkpoint_tx),
            })
            .await
            .is_err()
        {
            failures.push(format!(
                "session {session_id}: drive actor unavailable for shutdown checkpoint"
            ));
            continue;
        }
        match checkpoint_rx.await {
            Ok(Ok(_)) => {
                let (close_tx, close_rx) = oneshot::channel();
                if drive
                    .send(crate::write_owner::DriveCommand::Close {
                        session_id,
                        reply: close_tx,
                    })
                    .await
                    .is_err()
                {
                    failures.push(format!(
                        "session {session_id}: drive actor unavailable for shutdown close"
                    ));
                    continue;
                }
                match close_rx.await {
                    Ok(Ok(_)) => finish_mounted_session(state, pool, session_id, mounted),
                    Ok(Err(err)) => failures.push(format!(
                        "session {session_id}: shutdown close failed: {err}"
                    )),
                    Err(_) => failures.push(format!(
                        "session {session_id}: drive actor dropped shutdown close reply"
                    )),
                }
            }
            Ok(Err(err)) if err.code() == tonic::Code::FailedPrecondition => {
                // Read sessions have no checkpoint barrier and remain handled by
                // the existing open-session shutdown diagnostic below.
            }
            Ok(Err(err)) => failures.push(format!(
                "session {session_id}: shutdown checkpoint failed: {err}"
            )),
            Err(_) => failures.push(format!(
                "session {session_id}: drive actor dropped shutdown checkpoint reply"
            )),
        }
    }
    let mut drive_bays = Vec::new();
    for bay in library_drive_bays {
        if pool.drive_tx(bay.element_address).is_ok() {
            drive_bays.push(bay.element_address);
        } else if bay.loaded && bay.source_slot.is_some() {
            failures.push(format!(
                "bay 0x{:04x}: seated cartridge has no drive actor",
                bay.element_address
            ));
        }
    }
    for bay in drive_bays {
        let reservation_wait_started = Instant::now();
        let reservation = loop {
            match pool.reserve_drive_for_shutdown(bay) {
                Ok(reservation) => break Some(reservation),
                Err(err) => {
                    if err.code() == tonic::Code::NotFound
                        || pool.sessions_by_bay().contains_key(&bay)
                    {
                        failures.push(format!("bay 0x{bay:04x}: {err}"));
                        break None;
                    }
                    if reservation_wait_started.elapsed() >= SHUTDOWN_RESERVATION_WAIT {
                        failures.push(format!(
                            "bay 0x{bay:04x}: timed out waiting for an idle drive reservation"
                        ));
                        break None;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        };
        let Some(reservation) = reservation else {
            continue;
        };
        let current = state.current_library_snapshot().and_then(|snapshot| {
            snapshot
                .report
                .libraries
                .iter()
                .find(|library| library.serial == configured_library_serial.as_str())
                .and_then(|library| {
                    library
                        .drive_bays
                        .iter()
                        .find(|candidate| candidate.element_address == bay)
                        .cloned()
                })
        });
        let Some(current) = current else {
            failures.push(format!("bay 0x{bay:04x}: missing from shutdown inventory"));
            drop(reservation);
            continue;
        };
        let Some(home_slot) = current.source_slot.filter(|_| current.loaded) else {
            drop(reservation);
            continue;
        };
        let parked = pool.parked_at(bay);
        let seated = crate::write_owner::SeatedCartridge {
            bay,
            library_serial: configured_library_serial.to_string(),
            barcode: current.loaded_tape,
            home_slot,
            tape_uuid: parked.as_ref().and_then(|parked| parked.seated.tape_uuid),
            prior_session_id: parked
                .as_ref()
                .and_then(|parked| parked.seated.prior_session_id),
        };
        if let Err(err) =
            dismount_reserved_cartridge(state, pool, &seated, DismountReason::Shutdown).await
        {
            failures.push(format!("bay 0x{bay:04x}: {err}"));
        }
        drop(reservation);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Status::internal(format!(
            "daemon shutdown could not unload every seated cartridge: {}",
            failures.join("; ")
        )))
    }
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

#[derive(Debug)]
enum ActorMountPlan {
    Ready(ActorMount),
    Evict {
        mount: ActorMount,
        seated: crate::write_owner::SeatedCartridge,
    },
}

impl ActorMountPlan {
    fn mount_mut(&mut self) -> &mut ActorMount {
        match self {
            Self::Ready(mount) | Self::Evict { mount, .. } => mount,
        }
    }
}

async fn resolve_and_reserve_actor_mount(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    library_serial: &str,
    tape_uuid: &TapeUuid,
) -> Result<(ActorMount, crate::write_owner::DriveReservation), Status> {
    const ATTEMPTS: usize = 2;
    for attempt in 0..ATTEMPTS {
        let busy_bays = pool.busy_bays();
        let plan = resolve_actor_mount(state, library_serial, tape_uuid, &busy_bays)?;
        let bay = match &plan {
            ActorMountPlan::Ready(mount) | ActorMountPlan::Evict { mount, .. } => mount.bay,
        };
        match pool.reserve_drive(bay) {
            Ok(reservation) => match plan {
                ActorMountPlan::Ready(mount) => {
                    ensure_actor_mount_media_readiness_admitted(state, library_serial, &mount)?;
                    return Ok((mount, reservation));
                }
                ActorMountPlan::Evict { mount, mut seated } => {
                    ensure_actor_mount_media_readiness_admitted(state, library_serial, &mount)?;
                    if let Some(parked) = pool.parked_at(seated.bay) {
                        if parked.seated.library_serial == seated.library_serial
                            && parked.seated.barcode == seated.barcode
                            && parked.seated.home_slot == seated.home_slot
                        {
                            seated.tape_uuid = parked.seated.tape_uuid;
                            seated.prior_session_id = parked.seated.prior_session_id;
                        }
                    }
                    dismount_reserved_cartridge(state, pool, &seated, DismountReason::Evicted)
                        .await?;
                    return Ok((mount, reservation));
                }
            },
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
) -> Result<ActorMountPlan, Status> {
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
    let mut plan = resolve_actor_mount_plan_from_library(library, voltag.as_str(), busy_bays)?;
    let mount = plan.mount_mut();
    mount.library_serial = library_serial.to_string();
    mount.barcode = barcode;
    enrich_actor_mount_from_catalog(&index, library_serial, mount)?;
    if let ActorMountPlan::Evict { seated, .. } = &mut plan {
        seated.library_serial = library_serial.to_string();
    }
    Ok(plan)
}

fn resolve_pinned_actor_mount(
    state: &ApiState,
    library_serial: &str,
    bay: u16,
    tape_uuid: &TapeUuid,
) -> Result<ActorMount, Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|err| Status::internal(err.to_string()))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let expected_barcode = tape.voltag.ok_or_else(|| {
        Status::failed_precondition(format!(
            "drive bay 0x{bay:04x} tape identity cannot be proven: catalog tape has no barcode"
        ))
    })?;
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library not found"))?;
    let library = snapshot
        .report
        .libraries
        .iter()
        .find(|library| library.serial == library_serial)
        .ok_or_else(|| Status::not_found(format!("library {library_serial} not found")))?;
    let drive_bay = library
        .drive_bays
        .iter()
        .find(|candidate| candidate.element_address == bay)
        .ok_or_else(|| Status::not_found(format!("drive bay 0x{bay:04x} not found")))?;
    if !drive_bay.loaded {
        return Err(Status::failed_precondition(format!(
            "drive bay 0x{bay:04x} is empty"
        )));
    }
    let observed_barcode = drive_bay.loaded_tape.as_deref().ok_or_else(|| {
        Status::failed_precondition(format!(
            "drive bay 0x{bay:04x} tape identity cannot be proven: loaded media has no readable barcode"
        ))
    })?;
    if observed_barcode != expected_barcode {
        return Err(Status::failed_precondition(format!(
            "drive bay 0x{bay:04x} tape identity cannot be proven: expected barcode {expected_barcode}, observed {observed_barcode}"
        )));
    }
    let mut mount = ActorMount {
        bay,
        barcode: Some(observed_barcode.to_string()),
        source_slot: None,
        home_slot: drive_bay.source_slot,
        needs_drive_load: false,
        library_serial: library_serial.to_string(),
        drive_uuid: None,
        drive_serial: None,
    };
    enrich_actor_mount_from_catalog(&index, library_serial, &mut mount)?;
    Ok(mount)
}

fn enrich_actor_mount_from_catalog(
    index: &CatalogIndex,
    library_serial: &str,
    mount: &mut ActorMount,
) -> Result<(), Status> {
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
        // Already an Option on the mount; it just used to be filled from a
        // String that was sometimes empty, so "no serial" arrived as Some("").
        mount.drive_serial = drive.serial;
    }
    Ok(())
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

fn resolve_actor_mount_plan_from_library(
    library: &Library,
    voltag: &str,
    busy_bays: &HashSet<u16>,
) -> Result<ActorMountPlan, Status> {
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
            Ok(ActorMountPlan::Ready(ActorMount {
                bay,
                barcode: Some(voltag.to_string()),
                source_slot: None,
                home_slot,
                needs_drive_load: false,
                library_serial: String::new(),
                drive_uuid: None,
                drive_serial: None,
            }))
        }
        Ok(LoadPlan::Load { slot, bay }) => {
            ensure_target_bay_can_receive(library, bay)?;
            Ok(ActorMountPlan::Ready(ActorMount {
                bay,
                barcode: Some(voltag.to_string()),
                source_slot: Some(slot),
                home_slot: Some(slot),
                needs_drive_load: true,
                library_serial: String::new(),
                drive_uuid: None,
                drive_serial: None,
            }))
        }
        Err(LoadError::NotInLibrary) => Err(Status::not_found(format!(
            "cartridge {voltag} not found in library inventory (slot or drive)"
        ))),
        Err(LoadError::NoFreeDrive) => {
            let source_slot = library
                .slots
                .iter()
                .find(|slot| slot.cartridge.as_deref() == Some(voltag))
                .map(|slot| slot.element_address)
                .ok_or_else(|| {
                    Status::not_found(format!(
                        "cartridge {voltag} not found in library inventory (slot or drive)"
                    ))
                })?;
            let candidate = library.drive_bays.iter().find(|bay| {
                bay.loaded
                    && !busy_bays.contains(&bay.element_address)
                    && bay
                        .installed
                        .as_ref()
                        .and_then(|drive| drive.sg_path.as_ref())
                        .is_some()
                    && bay.source_slot.is_some_and(|home_slot| {
                        library.slots.iter().any(|slot| {
                            slot.element_address == home_slot
                                && slot.accessible
                                && slot.exception.is_none()
                                && !slot.full
                        })
                    })
            });
            let Some(candidate) = candidate else {
                return Err(Status::failed_precondition(format!(
                    "no free drive bay available to load cartridge {voltag}"
                )));
            };
            let home_slot = candidate
                .source_slot
                .expect("eviction candidate must have an empty home slot");
            Ok(ActorMountPlan::Evict {
                mount: ActorMount {
                    bay: candidate.element_address,
                    barcode: Some(voltag.to_string()),
                    source_slot: Some(source_slot),
                    home_slot: Some(source_slot),
                    needs_drive_load: true,
                    library_serial: String::new(),
                    drive_uuid: None,
                    drive_serial: None,
                },
                seated: crate::write_owner::SeatedCartridge {
                    bay: candidate.element_address,
                    library_serial: String::new(),
                    barcode: candidate.loaded_tape.clone(),
                    home_slot,
                    tape_uuid: None,
                    prior_session_id: None,
                },
            })
        }
        Err(err) => Err(Status::internal(format!(
            "resolve load target for cartridge {voltag}: {err}"
        ))),
    }
}

#[cfg(test)]
fn resolve_actor_mount_from_library(
    library: &Library,
    voltag: &str,
    busy_bays: &HashSet<u16>,
) -> Result<ActorMount, Status> {
    match resolve_actor_mount_plan_from_library(library, voltag, busy_bays)? {
        ActorMountPlan::Ready(mount) => Ok(mount),
        ActorMountPlan::Evict { .. } => Err(Status::failed_precondition(format!(
            "no free drive bay available to load cartridge {voltag}"
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
    use remanence_parity::{
        CommittedBundle, CommittedBundleKind, ParityConfig, TapeFileEntry, TapeFileKind,
    };
    use remanence_state::{
        CheckpointJournalRecord, CheckpointObjectProjection,
        CheckpointObjectRecoveryRepresentation, CheckpointObjectRecoveryRow,
        NativeObjectCopyProjectionInput, NativeObjectProjectionInput, PoolSelectionPolicyName,
        ProvisionTapeInput, TapeJournalIndexInput, TapePoolConfig, TapePoolProjectionInput,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn batched_selection_avoids_no_free_bay_failure_for_loaded_fresh_tape() {
        const LEGACY_UUID: [u8; 16] = [1; 16];
        const FRESH_UUID: [u8; 16] = [2; 16];
        const BLOCK_SIZE: u32 = 256 * 1024;

        let temp = tempfile::tempdir().expect("temporary mixed-estate catalog");
        let mut index = CatalogIndex::open(temp.path().join("state.sqlite"))
            .expect("open mixed-estate catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "camera.copy-a".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        for (tape_uuid, voltag) in [(LEGACY_UUID, "LEG001L9"), (FRESH_UUID, "NEW002L9")] {
            index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid,
                    voltag: voltag.to_string(),
                    block_size: BLOCK_SIZE,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision pool tape");
            index
                .project_tape_pool_membership(tape_uuid, "camera.copy-a")
                .expect("assign pool tape");
        }
        index
            .project_committed_tape_file_bundle(
                TapeJournalIndexInput {
                    tape_uuid: LEGACY_UUID,
                    block_size: BLOCK_SIZE,
                    scheme: None,
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 1,
                        kind: TapeFileKind::Object,
                        block_count: 3,
                        physical_start_hint: Some(0),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        object_recovery_row: None,
                    }],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals: 3,
                },
            )
            .expect("project legacy tape usage");
        let cfg = TapePoolConfig {
            id: "camera.copy-a".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };

        let batched = select_tape_in_pool_for_write_session(
            &index,
            &cfg,
            0,
            &HashSet::new(),
            &temp.path().join("checkpoints"),
        )
        .expect("batched selection should choose the loaded fresh tape");
        assert_eq!(batched.tape_uuid, FRESH_UUID);

        let mut fresh_bay = drive_bay(0x0101, true, Some("NEW002L9"));
        fresh_bay.source_slot = None;
        let mut occupied_bay = drive_bay(0x0102, true, Some("OTHER1L9"));
        occupied_bay.source_slot = None;
        let library = test_library(
            vec![fresh_bay, occupied_bay],
            vec![slot(0x0401, "LEG001L9")],
        );
        let mount = resolve_actor_mount_from_library(&library, "NEW002L9", &HashSet::new())
            .expect("already-loaded fresh tape should not require a free bay");
        assert_eq!(mount.bay, 0x0101);
        assert!(!mount.needs_drive_load);
    }

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

    #[test]
    fn actor_mount_plan_evicts_only_an_idle_cart_with_an_empty_home_slot() {
        let mut loaded = drive_bay(0x0101, true, Some("OLD001L9"));
        loaded.source_slot = Some(0x0401);
        let library = test_library(
            vec![loaded],
            vec![empty_slot(0x0401), slot(0x0402, "RMN002L9")],
        );

        let plan = resolve_actor_mount_plan_from_library(&library, "RMN002L9", &HashSet::new())
            .expect("idle seated cart should be evictable");
        let ActorMountPlan::Evict { mount, seated } = plan else {
            panic!("expected an eviction plan");
        };
        assert_eq!(mount.bay, 0x0101);
        assert_eq!(mount.source_slot, Some(0x0402));
        assert!(mount.needs_drive_load);
        assert_eq!(seated.barcode.as_deref(), Some("OLD001L9"));
        assert_eq!(seated.home_slot, 0x0401);

        let err =
            resolve_actor_mount_plan_from_library(&library, "RMN002L9", &HashSet::from([0x0101]))
                .expect_err("a reserved seated cart must never be evicted");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("no free drive"), "{err}");
    }

    #[tokio::test]
    async fn canonical_dismount_tail_unloads_then_moves_home() {
        let harness = dismount_harness(0);
        let parked = harness.pool.park_cartridge(harness.seated.clone());
        let responder = tokio::spawn(reply_to_dismount_commands(
            harness.drive_rx,
            harness.changer_rx,
        ));
        let _reservation = harness
            .pool
            .reserve_drive(0x0101)
            .expect("reserve idle bay");
        dismount_reserved_cartridge(
            &harness.state,
            &harness.pool,
            &harness.seated,
            DismountReason::IdleTimeout,
        )
        .await
        .expect("dismount succeeds");
        responder.await.expect("mock actors join");
        assert!(!harness.pool.parked_is_current(&parked));
    }

    #[tokio::test]
    async fn idle_timeout_dispatches_the_canonical_dismount_tail() {
        let harness = dismount_harness(1);
        let parked = harness.pool.park_cartridge(harness.seated.clone());
        schedule_idle_dismount(harness.state.clone(), parked.clone());
        tokio::time::timeout(
            Duration::from_secs(2),
            reply_to_dismount_commands(harness.drive_rx, harness.changer_rx),
        )
        .await
        .expect("idle timeout must dispatch unload within two seconds");
        tokio::time::timeout(Duration::from_secs(1), async {
            while harness.pool.parked_is_current(&parked) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idle dismount must clear parked state");
        assert!(!harness.pool.parked_is_current(&parked));
    }

    #[tokio::test]
    async fn daemon_shutdown_dispatches_the_canonical_dismount_tail() {
        let harness = dismount_harness(0);
        let parked = harness.pool.park_cartridge(harness.seated.clone());
        let responder = tokio::spawn(reply_to_dismount_commands(
            harness.drive_rx,
            harness.changer_rx,
        ));
        tokio::time::timeout(Duration::from_secs(2), shutdown_drive_pool(&harness.state))
            .await
            .expect("shutdown must not hang")
            .expect("shutdown dismount succeeds");
        responder.await.expect("mock actors join");
        assert!(!harness.pool.parked_is_current(&parked));
    }

    #[test]
    fn startup_parking_is_scoped_to_the_configured_library_serial() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let temp = tempfile::Builder::new()
            .prefix("remanence-startup-parking-scope")
            .tempdir()
            .expect("tempdir");
        let index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("open catalog");
        let mut state = ApiState::new(index);
        state.default_library_serial = Some(Arc::new("LIB001".to_string()));
        state.drive_idle_unload_seconds = 0;
        let (drive_tx, _drive_rx) = tokio::sync::mpsc::channel(1);
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            std::collections::HashMap::from([(0x0101, drive_tx)]),
            Arc::new(std::collections::HashMap::from([(
                0x0101,
                AtomicBool::new(false),
            )])),
        );
        state.drive_pool = Some(pool.clone());
        let managed = test_library(
            vec![drive_bay(0x0101, true, Some("MAIN01L9"))],
            vec![empty_slot(0x0401)],
        );
        let mut foreign = test_library(
            vec![drive_bay(0x0101, true, Some("D2T001L9"))],
            vec![empty_slot(0x0401)],
        );
        foreign.serial = "D2LIB".to_string();
        let report = remanence_library::DiscoveryReport {
            libraries: vec![foreign, managed],
            warnings: Vec::new(),
        };

        register_startup_seated_cartridges(&state, &report);

        let parked = pool.parked_at(0x0101).expect("managed cartridge parked");
        assert_eq!(parked.seated.library_serial, "LIB001");
        assert_eq!(parked.seated.barcode.as_deref(), Some("MAIN01L9"));
    }

    #[tokio::test]
    async fn manual_finalize_returns_typed_busy_before_state_or_motion() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let temp = tempfile::Builder::new()
            .prefix("remanence-manual-finalize-busy")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("state.sqlite");
        let index = CatalogIndex::open(&index_path).expect("open catalog");
        let mut state = ApiState::new(index);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let (changer_tx, mut changer_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            std::collections::HashMap::from([(0x0101, drive_tx)]),
            Arc::new(std::collections::HashMap::from([(
                0x0101,
                AtomicBool::new(false),
            )])),
        );
        state.drive_pool = Some(pool.clone());
        let tape_uuid = [0xB5; 16];
        let _active_object_owner = pool.reserve_tape(tape_uuid).expect("reserve exact tape");

        let result = manual_finalize_tape(
            &state,
            ManualFinalizeTapeAdmission {
                candidate_operation_id: Uuid::from_u128(0xB501),
                actor: remanence_state::AuditActor::User("busy@example.invalid".to_string()),
                actor_fingerprint: "sha256:busy".to_string(),
                idempotency_key: Uuid::from_u128(0xB502),
                request_fingerprint: [0xB5; 32],
                tape_uuid,
                expected_pool_id: Some("busy.pool".to_string()),
                reason: "operator close-out while Object is active".to_string(),
            },
        )
        .await
        .expect("busy is a typed result");

        assert_eq!(result, crate::write_owner::ManualFinalizeTapeResult::Busy);
        assert!(
            drive_rx.try_recv().is_err(),
            "busy must issue no drive command"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "busy must issue no changer command"
        );
        let reopened = CatalogIndex::open_read_only(&index_path).expect("reopen catalog");
        assert!(reopened
            .idempotency_scope_record("sha256:busy", "finalize_tape", Uuid::from_u128(0xB502))
            .expect("query idempotency")
            .is_none());
    }

    /// Build the smallest ordinary checkpoint authority that can be closed by
    /// the asynchronous manual-finalization admission path.
    fn manual_finalize_checkpoint(tape_uuid: TapeUuid, block_size: u32) -> CheckpointJournalRecord {
        let object_id = Uuid::from_u128(0xA501).to_string();
        CheckpointJournalRecord {
            ordinal: 1,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 6,
            tape_uuid,
            batch_id: *Uuid::from_u128(0xA502).as_bytes(),
            next_tape_file_number: 2,
            block_size,
            objects: vec![CheckpointObjectProjection {
                object: NativeObjectProjectionInput {
                    object_id: object_id.clone(),
                    caller_object_id: Some("async-finalize-object".to_string()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(3 * u64::from(block_size)),
                    content_hash: Some(vec![0xA5; 32]),
                    metadata_hash: Some(vec![0xA6; 32]),
                    created_at_utc: Some("2026-08-09T00:00:00Z".to_string()),
                },
                files: Vec::new(),
                copy: NativeObjectCopyProjectionInput {
                    object_id: object_id.clone(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 1,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: "plaintext".to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0xA7; 32]),
                    stored_digest: Some(vec![0xA7; 32]),
                },
                block_size,
                block_count: 3,
                fresh_tape: true,
                total_committed_ordinals: 3,
                object_recovery_row: CheckpointObjectRecoveryRow {
                    tape_file_number: 1,
                    stored_block_count: 3,
                    object_id: object_id.into_bytes(),
                    representation: CheckpointObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: 1,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0xA8; 32],
                    },
                },
            }],
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: None,
            terminal_finalization: None,
            sealed_after_write: false,
        }
    }

    fn seed_automatic_after_replica_c(
        index_path: &Path,
        tape_uuid: TapeUuid,
        voltag: &str,
        pool_id: &str,
        block_size: u32,
    ) -> TapePoolConfig {
        let pool_config = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(block_size),
            min_object_size_bytes: 0,
        };
        let mut index = CatalogIndex::open(index_path).expect("open host-only route catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project host-only route pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: voltag.to_string(),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision host-only route tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign host-only route tape");

        let mut checkpoint = manual_finalize_checkpoint(tape_uuid, block_size);
        let object_id = Uuid::from_bytes(tape_uuid).to_string();
        checkpoint.objects[0]
            .object
            .object_id
            .clone_from(&object_id);
        checkpoint.objects[0].copy.object_id.clone_from(&object_id);
        checkpoint.objects[0].object_recovery_row.object_id = object_id.into_bytes();
        let checkpoint_dir = index_path
            .parent()
            .expect("catalog parent")
            .join("checkpoints");
        let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open host-only route checkpoint");
        journal
            .append(&checkpoint)
            .expect("append host-only route checkpoint");
        index
            .project_checkpoint_record(&checkpoint)
            .expect("project host-only route checkpoint");

        let mut lease = journal
            .acquire_exclusive()
            .expect("acquire host-only route checkpoint");
        let mut source =
            remanence_state::CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(
                &lease,
            )
            .expect("reconstruct host-only route rows");
        let summary = source.summary();
        let replica =
            remanence_parity::checked_tape_index_replica_layout(block_size, summary.counts)
                .expect("plan host-only route replica");
        let layout = remanence_parity::TerminalTailLayout::new(
            0,
            block_size,
            summary.scope.covered_prefix_tape_file_count,
            checkpoint.eod_lba,
            replica.replica_record_count,
            remanence_parity::index_separation_records(
                block_size,
                remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
            )
            .expect("plan host-only route gaps"),
        )
        .expect("plan host-only route layout");
        let edition_id = *Uuid::new_v4().as_bytes();
        let edition = remanence_parity::plan_tape_index_edition(
            remanence_parity::TapeIndexEditionDescriptor {
                tape_uuid,
                edition_id,
                edition_sequence: checkpoint.ordinal + 1,
                scope: summary.scope,
                counts: summary.counts,
                block_size,
                compression_enabled: false,
                writer_version: "mount-host-only-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: layout,
            },
            &mut source,
        )
        .expect("plan host-only route edition");
        drop(source);
        let intent = remanence_state::TerminalFinalizationIntent {
            tape_uuid,
            trigger: remanence_state::TerminalFinalizationTrigger::ReachedLowWatermark,
            manual: None,
            progress: remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
            recovery_required: false,
            edition_id,
            edition_sequence: edition.descriptor.edition_sequence,
            edition_digest: edition.edition_digest,
            writer_version: edition.descriptor.writer_version,
            write_timestamp: edition.descriptor.write_timestamp,
            terminal_prefix: None,
            layout: remanence_state::TerminalFinalizationLayout::try_from(layout)
                .expect("persist host-only route layout"),
        };
        lease
            .begin_terminal_finalization(&intent)
            .expect("begin host-only route finalization");
        for (expected, next) in [
            (
                remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
                remanence_state::TerminalFinalizationProgress::AfterReplicaA,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterReplicaA,
                remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
                remanence_state::TerminalFinalizationProgress::AfterReplicaB,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterReplicaB,
                remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
                remanence_state::TerminalFinalizationProgress::AfterReplicaC,
            ),
        ] {
            lease
                .advance_terminal_finalization(expected, next)
                .expect("advance host-only route finalization");
        }
        let current = lease
            .mark_terminal_recovery_required()
            .expect("classify host-only route recovery");
        index
            .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
                tape_uuid,
                trigger: current.trigger,
                operation_id: None,
                progress: current.progress,
                edition_digest: current.edition_digest,
                layout_digest: current.layout.layout_digest,
                outcome: remanence_state::TerminalFinalizationOutcome::RecoveryRequired,
                updated_at_utc: None,
            })
            .expect("project host-only route recovery");
        pool_config
    }

    #[tokio::test]
    async fn open_and_automatic_after_c_routes_complete_before_actor_capabilities() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        const BLOCK_SIZE: u32 = 256 * 1024;
        const OPEN_TAPE: TapeUuid = [0xB1; 16];
        const STARTUP_TAPE: TapeUuid = [0xB2; 16];
        const STALE_COMPANION_TAPE: TapeUuid = [0xB3; 16];
        const OWNED_TAPE: TapeUuid = [0xB4; 16];
        let temp = tempfile::Builder::new()
            .prefix("remanence-after-c-entry-routes")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("state.sqlite");
        let pool_id = "after-c-host-only";
        let pool_config =
            seed_automatic_after_replica_c(&index_path, OPEN_TAPE, "HST001L9", pool_id, BLOCK_SIZE);
        seed_automatic_after_replica_c(&index_path, STARTUP_TAPE, "HST002L9", pool_id, BLOCK_SIZE);
        seed_automatic_after_replica_c(
            &index_path,
            STALE_COMPANION_TAPE,
            "HST003L9",
            pool_id,
            BLOCK_SIZE,
        );
        seed_automatic_after_replica_c(&index_path, OWNED_TAPE, "HST004L9", pool_id, BLOCK_SIZE);
        let index = CatalogIndex::open(&index_path).expect("reopen host-only route catalog");
        let mut state = ApiState::new_with_pool_configs(index, [pool_config.clone()]);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let (changer_tx, mut changer_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            std::collections::HashMap::from([(0x0101, drive_tx)]),
            Arc::new(std::collections::HashMap::from([(
                0x0101,
                AtomicBool::new(false),
            )])),
        );
        state.drive_pool = Some(pool.clone());

        let competing_owner = pool
            .reserve_tape(OWNED_TAPE)
            .expect("reserve exact tape for concurrent recovery");
        let busy = open_write_session_reserved(
            &state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: OWNED_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            "unused-library".to_string(),
        )
        .await
        .expect_err("pinned open must lose to the existing exact-tape owner");
        assert_eq!(busy.code(), tonic::Code::FailedPrecondition);
        assert!(
            remanence_state::FileCheckpointJournal::open(
                state.checkpoint_journal_dir.as_path(),
                OWNED_TAPE,
            )
            .expect("open owned-tape checkpoint")
            .terminal_finalization_intent()
            .expect("read owned-tape intent")
            .is_some(),
            "busy refusal must happen before host preflight changes authority"
        );
        drop(competing_owner);

        let open_error = open_write_session_reserved(
            &state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: OPEN_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            "unused-library".to_string(),
        )
        .await
        .expect_err("host-only open recovery must reject later Object admission");
        assert_eq!(open_error.code(), tonic::Code::FailedPrecondition);
        assert!(
            open_error.message().contains("host-only open recovery"),
            "{open_error}"
        );
        assert!(
            drive_rx.try_recv().is_err(),
            "open route reached drive actor"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "open route reached changer actor"
        );

        let stale_journal = remanence_state::FileCheckpointJournal::open(
            state.checkpoint_journal_dir.as_path(),
            STALE_COMPANION_TAPE,
        )
        .expect("open stale-companion checkpoint");
        let mut companion_path = stale_journal.path().as_os_str().to_os_string();
        companion_path.push(".finalizing");
        let companion_path = PathBuf::from(companion_path);
        let companion_bytes = std::fs::read(&companion_path).expect("save exact companion bytes");
        recover_automatic_terminal_tape(&state, STALE_COMPANION_TAPE)
            .await
            .expect("first automatic completion seals the checkpoint");
        let sealed_before_retry = stale_journal
            .replay()
            .expect("read sealed authority before retained-companion retry");
        std::fs::write(&companion_path, companion_bytes)
            .expect("restore exact post-fsync stale companion");
        let selected_stale = crate::SelectedTape {
            pool_id: pool_id.to_string(),
            tape_uuid: STALE_COMPANION_TAPE,
            block_size: BLOCK_SIZE,
            parity_config: ParityConfig::None,
        };
        let mut projection_failure_index = CatalogIndex::open(
            temp.path()
                .join("stale-companion-projection-failure.sqlite"),
        )
        .expect("open empty projection-failure catalog");
        crate::write_owner::preflight_automatic_terminal_completion(
            &mut projection_failure_index,
            crate::write_owner::ManualFinalizePreflightConfig {
                checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
                audit_dir: state.audit_dir.as_path(),
                audit_fsync: state.audit_fsync,
                audit_append_lock: &state.audit_append_lock,
            },
            &selected_stale,
            &pool_config,
        )
        .expect_err("failed sealed projection must not consume retry authority");
        let pending = stale_journal
            .terminal_finalization_intent()
            .expect("read companion after projection failure")
            .expect("projection failure retains the exact companion");

        let stale_index_path = temp.path().join("stale-companion-state.sqlite");
        let mut stale_index =
            CatalogIndex::open(&stale_index_path).expect("open stale recovery catalog");
        stale_index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project stale recovery pool");
        stale_index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: STALE_COMPANION_TAPE,
                voltag: "HST003L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision stale recovery tape");
        stale_index
            .project_tape_pool_membership(STALE_COMPANION_TAPE, pool_id)
            .expect("assign stale recovery tape");
        stale_index
            .project_checkpoint_record(
                sealed_before_retry
                    .first()
                    .expect("sealed history retains ordinary authority"),
            )
            .expect("project ordinary authority into stale catalog");
        stale_index
            .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
                tape_uuid: STALE_COMPANION_TAPE,
                trigger: pending.trigger,
                operation_id: None,
                progress: pending.progress,
                edition_digest: pending.edition_digest,
                layout_digest: pending.layout.layout_digest,
                outcome: remanence_state::TerminalFinalizationOutcome::RecoveryRequired,
                updated_at_utc: None,
            })
            .expect("project stale recovery-required catalog state");
        let mut stale_state = ApiState::new_with_pool_configs(stale_index, [pool_config.clone()]);
        stale_state.drive_pool = Some(pool.clone());
        let stale_error = open_write_session_reserved(
            &stale_state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: STALE_COMPANION_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            "unused-library".to_string(),
        )
        .await
        .expect_err("stale companion retry must finish host-only and refuse Objects");
        assert_eq!(stale_error.code(), tonic::Code::FailedPrecondition);
        assert!(
            stale_journal
                .terminal_finalization_intent()
                .expect("read companion after retry")
                .is_none(),
            "exact sealed replay must retire the stale companion"
        );
        assert_eq!(
            stale_journal
                .replay()
                .expect("read sealed authority after retry"),
            sealed_before_retry,
            "host-only retry must not duplicate the sealed checkpoint"
        );
        assert_eq!(
            CatalogIndex::open(&stale_index_path)
                .expect("reopen healed stale catalog")
                .terminal_finalization(&STALE_COMPANION_TAPE)
                .expect("read healed stale projection")
                .expect("healed stale projection")
                .outcome,
            remanence_state::TerminalFinalizationOutcome::Finalized,
            "sealed retry must repair stale catalog authority before cleanup"
        );
        assert!(
            drive_rx.try_recv().is_err(),
            "stale-companion route reached drive actor"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "stale-companion route reached changer actor"
        );

        let mut lowered = pool_config;
        lowered.capacity_cap_bytes = Some(u64::from(BLOCK_SIZE));
        let lowered_index = CatalogIndex::open(&index_path).expect("open lowered-cap catalog");
        let mut lowered_state = ApiState::new_with_pool_configs(lowered_index, [lowered]);
        lowered_state.drive_pool = Some(pool);
        recover_automatic_terminal_tape(&lowered_state, STARTUP_TAPE)
            .await
            .expect("automatic route seals after C despite a newly lowered capacity cap");
        assert!(
            drive_rx.try_recv().is_err(),
            "automatic route reached drive actor"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "automatic route reached changer actor"
        );
        let final_index = CatalogIndex::open(&index_path).expect("read host-only route results");
        for tape_uuid in [OPEN_TAPE, STARTUP_TAPE, STALE_COMPANION_TAPE] {
            assert_eq!(
                final_index
                    .terminal_finalization(&tape_uuid)
                    .expect("read terminal result")
                    .expect("terminal result")
                    .outcome,
                remanence_state::TerminalFinalizationOutcome::Finalized
            );
        }
    }

    #[tokio::test]
    async fn manual_finalize_accepts_rejoins_redispatches_and_restarts_without_duplicates() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        const BLOCK_SIZE: u32 = 256 * 1024;
        let temp = tempfile::Builder::new()
            .prefix("remanence-manual-finalize-async")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("state.sqlite");
        let checkpoint_dir = temp.path().join("checkpoints");
        let tape_uuid = [0xA5; 16];
        let pool_id = "async-finalize";
        let pool_config = TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "ASY001L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign pool");
        let checkpoint = manual_finalize_checkpoint(tape_uuid, BLOCK_SIZE);
        remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("open checkpoint journal")
            .append(&checkpoint)
            .expect("append checkpoint");
        index
            .project_checkpoint_record(&checkpoint)
            .expect("project checkpoint");

        let mut state = ApiState::new_with_pool_configs(index, [pool_config]);
        state.default_library_serial = Some(Arc::new("LIB001".to_string()));
        let mut loaded = drive_bay(0x0101, true, Some("ASY001L9"));
        loaded.source_slot = None;
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    libraries: vec![test_library(vec![loaded], vec![])],
                    warnings: Vec::new(),
                },
                captured_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        ))));
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(2);
        let (changer_tx, mut changer_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            std::collections::HashMap::from([(0x0101, drive_tx)]),
            Arc::new(std::collections::HashMap::from([(
                0x0101,
                AtomicBool::new(false),
            )])),
        );
        state.drive_pool = Some(pool.clone());
        let admission = ManualFinalizeTapeAdmission {
            candidate_operation_id: Uuid::from_u128(0xA503),
            actor: remanence_state::AuditActor::User("async@example.invalid".to_string()),
            actor_fingerprint: "sha256:async-finalize".to_string(),
            idempotency_key: Uuid::from_u128(0xA504),
            request_fingerprint: [0xA5; 32],
            tape_uuid,
            expected_pool_id: Some(pool_id.to_string()),
            reason: "ship accepted tape".to_string(),
        };

        let accepted = manual_finalize_tape(&state, admission.clone())
            .await
            .expect("durable acceptance");
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(accepted) = accepted else {
            panic!("durable admission must not return BUSY");
        };
        assert_eq!(accepted.operation_id, Uuid::from_u128(0xA503));
        assert_eq!(
            accepted.projection.outcome,
            remanence_state::TerminalFinalizationOutcome::InProgress
        );

        let first_command = tokio::time::timeout(Duration::from_secs(1), drive_rx.recv())
            .await
            .expect("accepted worker dispatches")
            .expect("drive command");
        let first_reply = match first_command {
            crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } => {
                assert_eq!(request.candidate_operation_id, accepted.operation_id);
                reply
            }
            _ => panic!("unexpected worker command"),
        };
        let polled =
            <crate::CatalogService as crate::pb::catalog_server::Catalog>::get_tape_finalization(
                &state.catalog_service(),
                tonic::Request::new(crate::pb::GetTapeFinalizationRequest {
                    tape_uuid: tape_uuid.to_vec(),
                }),
            )
            .await
            .expect("poll accepted operation")
            .into_inner();
        assert_eq!(
            polled.operation_id,
            accepted.operation_id.as_bytes().as_slice()
        );
        assert_eq!(
            polled.outcome,
            crate::pb::TapeFinalizationOutcome::Finalizing as i32
        );

        // The first worker is blocked on its drive reply. An exact replay must
        // rejoin the durable operation and must not dispatch a second tail.
        let replay = manual_finalize_tape(&state, admission.clone())
            .await
            .expect("same-key replay rejoins");
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(replay) = replay else {
            panic!("same-key replay must not degrade to BUSY");
        };
        assert_eq!(replay, accepted);
        assert!(
            drive_rx.try_recv().is_err(),
            "replay duplicated tail dispatch"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "loaded-tape tail moved media"
        );

        // The same durable key cannot be hidden behind BUSY while its worker
        // owns the tape. A changed request fingerprint remains a typed
        // idempotency conflict and cannot dispatch another tail or motion.
        let mut conflict = admission.clone();
        conflict.request_fingerprint = [0xA6; 32];
        conflict.reason = "changed request".to_string();
        let conflict = manual_finalize_tape(&state, conflict)
            .await
            .expect_err("same-key changed request must conflict while worker owns tape");
        assert_eq!(conflict.code(), tonic::Code::AlreadyExists);
        assert_eq!(
            conflict.message(),
            "FinalizeTape idempotency key is already bound to a different request"
        );
        assert!(
            drive_rx.try_recv().is_err(),
            "idempotency conflict duplicated tail dispatch"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "idempotency conflict moved media"
        );

        // Dropping the actor reply simulates a worker dispatch failure after
        // acceptance. Once its reservation is gone, an explicit same-key retry
        // must start exactly one replacement worker while retaining the
        // recovery-required projection until physical reconciliation advances
        // durable progress.
        drop(first_reply);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(probe) = pool.reserve_tape(tape_uuid) {
                    drop(probe);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed worker releases exact-tape ownership");
        let mut recovery_index = CatalogIndex::open(&index_path).expect("open recovery projection");
        recovery_index
            .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
                tape_uuid,
                trigger: accepted.projection.trigger,
                operation_id: Some(accepted.operation_id),
                progress: accepted.projection.progress,
                edition_digest: accepted.projection.edition_digest,
                layout_digest: accepted.projection.layout_digest,
                outcome: remanence_state::TerminalFinalizationOutcome::RecoveryRequired,
                updated_at_utc: None,
            })
            .expect("project completion-unknown outcome");
        drop(recovery_index);
        let retried = manual_finalize_tape(&state, admission.clone())
            .await
            .expect("same-key retry restarts failed worker");
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(retried) = retried else {
            panic!("same-key retry must retain durable acceptance");
        };
        assert_eq!(retried.operation_id, accepted.operation_id);
        assert_eq!(
            retried.projection.outcome,
            remanence_state::TerminalFinalizationOutcome::RecoveryRequired,
            "an explicit repair retry must retain the safety classification before redispatch"
        );
        let retried_command = tokio::time::timeout(Duration::from_secs(1), drive_rx.recv())
            .await
            .expect("same-key retry redispatches")
            .expect("retry drive command");
        let retried_reply = match retried_command {
            crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } => {
                assert_eq!(request.candidate_operation_id, accepted.operation_id);
                reply
            }
            _ => panic!("unexpected retry command"),
        };
        assert!(
            drive_rx.try_recv().is_err(),
            "retry duplicated tail dispatch"
        );

        // A second lost worker models daemon shutdown. Startup must reacquire
        // exact-tape ownership and resume without another FinalizeTape RPC.
        drop(retried_reply);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(probe) = pool.reserve_tape(tape_uuid) {
                    drop(probe);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry worker releases exact-tape ownership");
        spawn_startup_terminal_recoveries(state.clone());
        let resumed = tokio::time::timeout(Duration::from_secs(1), drive_rx.recv())
            .await
            .expect("startup resumes manual intent")
            .expect("resumed drive command");
        match resumed {
            crate::write_owner::DriveCommand::FinalizeTape { request, reply, .. } => {
                assert_eq!(request.candidate_operation_id, accepted.operation_id);
                assert_eq!(request.idempotency_key, Uuid::from_u128(0xA504));
                assert_eq!(request.actor_fingerprint, "sha256:async-finalize");
                assert_eq!(request.request_fingerprint, [0xA5; 32]);
                assert_eq!(request.expected_pool_id.as_deref(), Some(pool_id));
                assert_eq!(request.reason, "ship accepted tape");
                let _ = reply.send(Err(Status::unavailable("end restart test")));
            }
            _ => panic!("unexpected startup command"),
        }
        assert!(
            drive_rx.try_recv().is_err(),
            "startup duplicated tail dispatch"
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(probe) = pool.reserve_tape(tape_uuid) {
                    drop(probe);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("startup worker releases exact-tape ownership");
        let journal = remanence_state::FileCheckpointJournal::open(&checkpoint_dir, tape_uuid)
            .expect("reopen manual terminal checkpoint");
        let mut lease = journal
            .acquire_exclusive_for_terminal_recovery()
            .expect("acquire manual terminal recovery");
        let mut recovery_index =
            CatalogIndex::open(&index_path).expect("open manual progress projection");
        let current = lease
            .terminal_finalization_intent()
            .expect("read manual finalization intent")
            .expect("manual finalization intent");
        assert_eq!(
            current.progress,
            remanence_state::TerminalFinalizationProgress::BeforeReplicaA
        );
        for (expected, next) in [
            (
                remanence_state::TerminalFinalizationProgress::BeforeReplicaA,
                remanence_state::TerminalFinalizationProgress::AfterReplicaA,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterReplicaA,
                remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterSeparationAb,
                remanence_state::TerminalFinalizationProgress::AfterReplicaB,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterReplicaB,
                remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
            ),
            (
                remanence_state::TerminalFinalizationProgress::AfterSeparationBc,
                remanence_state::TerminalFinalizationProgress::AfterReplicaC,
            ),
        ] {
            let current = lease
                .advance_terminal_finalization(expected, next)
                .expect("advance manual finalization to replica C");
            recovery_index
                .project_terminal_finalization(
                    remanence_state::TerminalFinalizationProjectionInput {
                        tape_uuid,
                        trigger: current.trigger,
                        operation_id: Some(accepted.operation_id),
                        progress: current.progress,
                        edition_digest: current.edition_digest,
                        layout_digest: current.layout.layout_digest,
                        outcome: remanence_state::TerminalFinalizationOutcome::InProgress,
                        updated_at_utc: None,
                    },
                )
                .expect("project manual component progress");
        }
        let current = lease
            .mark_terminal_recovery_required()
            .expect("retain manual recovery classification after C");
        recovery_index
            .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
                tape_uuid,
                trigger: current.trigger,
                operation_id: Some(accepted.operation_id),
                progress: current.progress,
                edition_digest: current.edition_digest,
                layout_digest: current.layout.layout_digest,
                outcome: remanence_state::TerminalFinalizationOutcome::RecoveryRequired,
                updated_at_utc: None,
            })
            .expect("project manual post-C recovery state");
        drop(recovery_index);
        drop(lease);

        let completed = manual_finalize_tape(&state, admission)
            .await
            .expect("manual API entry completes the post-C host suffix");
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(completed) = completed else {
            panic!("manual host-only completion must retain durable acceptance");
        };
        assert_eq!(
            completed.projection.outcome,
            remanence_state::TerminalFinalizationOutcome::Finalized
        );
        assert!(
            drive_rx.try_recv().is_err(),
            "manual post-C API route reached drive actor"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "manual post-C API route reached changer actor"
        );
    }

    struct DismountHarness {
        _temp: tempfile::TempDir,
        state: ApiState,
        pool: crate::write_owner::DrivePool,
        seated: crate::write_owner::SeatedCartridge,
        drive_rx: tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
        changer_rx: tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
    }

    /// Build a one-drive library whose actor channels expose dismount ordering.
    fn dismount_harness(drive_idle_unload_seconds: u64) -> DismountHarness {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let temp = tempfile::Builder::new()
            .prefix("remanence-lazy-dismount")
            .tempdir()
            .expect("tempdir");
        let index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("open catalog");
        let mut state = ApiState::new(index);
        state.default_library_serial = Some(Arc::new("LIB001".to_string()));
        state.drive_idle_unload_seconds = drive_idle_unload_seconds;
        let mut loaded = drive_bay(0x0101, true, Some("OLD001L9"));
        loaded.source_slot = Some(0x0401);
        let library = test_library(vec![loaded], vec![empty_slot(0x0401)]);
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    libraries: vec![library],
                    warnings: Vec::new(),
                },
                captured_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        ))));
        let (drive_tx, drive_rx) = tokio::sync::mpsc::channel(1);
        let (changer_tx, changer_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            std::collections::HashMap::from([(0x0101, drive_tx)]),
            Arc::new(std::collections::HashMap::from([(
                0x0101,
                AtomicBool::new(false),
            )])),
        );
        state.drive_pool = Some(pool.clone());
        DismountHarness {
            _temp: temp,
            state,
            pool,
            seated: crate::write_owner::SeatedCartridge {
                bay: 0x0101,
                library_serial: "LIB001".to_string(),
                barcode: Some("OLD001L9".to_string()),
                home_slot: 0x0401,
                tape_uuid: None,
                prior_session_id: None,
            },
            drive_rx,
            changer_rx,
        }
    }

    async fn reply_to_dismount_commands(
        mut drive_rx: tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
        mut changer_rx: tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
    ) {
        let crate::write_owner::DriveCommand::Unload { reply } =
            drive_rx.recv().await.expect("unload command")
        else {
            panic!("dismount must issue drive unload first");
        };
        reply
            .send(Ok(Duration::from_millis(7)))
            .expect("unload reply receiver");
        let crate::write_owner::ChangerCommand::Move { src, dst, reply } =
            changer_rx.recv().await.expect("move-home command")
        else {
            panic!("dismount must use the changer move command");
        };
        assert_eq!((src, dst), (0x0101, 0x0401));
        reply.send(Ok(())).expect("move reply receiver");
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
            exception: None,
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
            exception: None,
            full: true,
            cartridge: Some(cartridge.to_string()),
        }
    }

    fn empty_slot(element_address: u16) -> Slot {
        Slot {
            element_address,
            accessible: true,
            exception: None,
            full: false,
            cartridge: None,
        }
    }
}
