//! Hardware mount bridge shared by CLI and Layer 5 actor orchestration.

use remanence_library::{
    resolve_load_target, AccessPolicy, DriveHandle, Library, LibraryHandle, LoadError, LoadPlan,
};
use remanence_state::{CatalogIndex, StateError};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::hex_encoding::bytes_to_hex;
use crate::pb;
use crate::pool_write::TapeUuid;
use crate::startup_media_readiness::status_from_state_error;

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
    actor: remanence_state::AuditActor,
    state: &ApiState,
    target: WriteSessionTarget,
    library_constraint: Option<String>,
) -> Result<pb::WriteSession, Status> {
    let state = state.clone();
    await_critical_task(
        "open_write_session",
        tokio::spawn(async move {
            open_write_session_critical(actor, state, target, library_constraint).await
        }),
    )
    .await
}

async fn open_write_session_critical(
    actor: remanence_state::AuditActor,
    state: ApiState,
    target: WriteSessionTarget,
    library_constraint: Option<String>,
) -> Result<pb::WriteSession, Status> {
    let pool = state.drive_pool()?.clone();
    open_write_session_reserved(actor, &state, &pool, target, library_constraint).await
}

async fn open_write_session_reserved(
    actor: remanence_state::AuditActor,
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    target: WriteSessionTarget,
    library_constraint: Option<String>,
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
        WriteSessionTarget::Pool { .. } => {
            let located_tapes =
                located_tapes_in_operated_libraries(state, pool, library_constraint.as_deref())?;
            let allowed_tape_uuids = located_tapes.keys().copied().collect::<HashSet<_>>();
            loop {
                select_attempts = select_attempts.saturating_add(1);
                let selected = crate::pool_write::select_tape_in_pool_for_write_session_scoped(
                    &index,
                    &pool_cfg,
                    0,
                    &pool.mounted_tape_uuids(),
                    state.checkpoint_journal_dir.as_path(),
                    &allowed_tape_uuids,
                )
                .map_err(crate::write_owner::status_from_select_tape_error)?;
                match pool.reserve_tape(selected.tape_uuid) {
                    Ok(reservation) => {
                        break ReservedWriteDisposition::Writable {
                            selected,
                            reservation,
                        };
                    }
                    Err(err)
                        if err.code() == tonic::Code::FailedPrecondition
                            && err.message() == "tape is already mounted" =>
                    {
                        continue
                    }
                    Err(err) => return Err(err),
                }
            }
        }
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
    let library_serial = resolve_tape_library_serial(state, pool, &tape_uuid)?;
    if let Some(expected) = library_constraint.as_deref() {
        if library_serial != expected {
            return Err(Status::failed_precondition(format!(
                "selected tape is in library {library_serial}, not constrained library {expected}"
            )));
        }
    }
    let open_started = Instant::now();
    let resolve_started = Instant::now();
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(state, pool, &library_serial, &tape_uuid).await?;
    let resolve_elapsed = resolve_started.elapsed();
    let drive = pool.drive_tx(&mount.drive_key())?;
    let pool_id_for_diag = pool_cfg.id.clone();
    let mut changer_move_ms = 0.0;
    if let Some(slot) = mount.source_slot {
        let changer_started = Instant::now();
        changer_move(pool, &mount.library_serial, slot, mount.bay).await?;
        changer_move_ms = crate::diagnostics::duration_ms(changer_started.elapsed());
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    let actor_open_started = Instant::now();
    let open_result: Result<pb::WriteSession, Status> = async {
        drive
            .send(crate::write_owner::DriveCommand::OpenWrite {
                actor,
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
    pool.set_holder(
        &mount.drive_key(),
        mount.drive_serial.as_deref(),
        "write session",
        session_id,
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if tape.kind != "data" {
        return Err(Status::failed_precondition(format!(
            "tape is a {} cartridge, not a data tape",
            tape.kind
        )));
    }
    let library_serial = resolve_tape_library_serial(&state, &pool, &tape_uuid)?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?;
    pool.set_internal_holder(
        &mount.drive_key(),
        mount.drive_serial.as_deref(),
        "inventory",
    );
    let drive = pool.drive_tx(&mount.drive_key())?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, &mount.library_serial, slot, mount.bay).await {
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
    handle: crate::operations::OperationHandle,
    progress: crate::tape_verification::Progress,
) -> Result<pb::TapeIndexVerification, Status> {
    let state = state.clone();
    await_critical_task(
        "verify_tape_index",
        tokio::spawn(async move {
            verify_tape_index_critical(state, tape_uuid, handle, progress).await
        }),
    )
    .await
}

async fn verify_tape_index_critical(
    state: ApiState,
    tape_uuid: TapeUuid,
    handle: crate::operations::OperationHandle,
    progress: crate::tape_verification::Progress,
) -> Result<pb::TapeIndexVerification, Status> {
    let pool = state.drive_pool()?.clone();
    let _tape_reservation = pool.reserve_tape(tape_uuid)?;
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if tape.kind != "data" {
        return Err(Status::failed_precondition(format!(
            "tape is a {} cartridge, not a data tape",
            tape.kind
        )));
    }
    let library_serial = resolve_tape_library_serial(&state, &pool, &tape_uuid)?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?;
    crate::tape_verification::check_cancelled(&handle)?;
    progress.mounted(mount.drive_serial.as_deref(), mount.bay);
    pool.set_holder(
        &mount.drive_key(),
        mount.drive_serial.as_deref(),
        "tape_verification",
        handle.op_id_uuid(),
    );
    let drive = pool.drive_tx(&mount.drive_key())?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, &mount.library_serial, slot, mount.bay).await {
            drop(drive_reservation);
            return Err(error);
        }
    }
    let (reply, response) = oneshot::channel();
    if drive
        .send(crate::write_owner::DriveCommand::VerifyTapeIndex {
            handle,
            progress,
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

/// Shared no-motion admission for daemon and standalone finalization.
pub(crate) fn prepare_manual_finalize_request(
    index: &CatalogIndex,
    mut admission: ManualFinalizeTapeAdmission,
    pool_config: impl Fn(&str) -> Result<remanence_state::TapePoolConfig, Status>,
) -> Result<
    (
        remanence_state::TapeRecord,
        crate::write_owner::ManualFinalizeTapeActorRequest,
    ),
    Status,
> {
    let tape = index
        .get_tape(&admission.tape_uuid)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if tape.kind != "data" {
        return Err(Status::failed_precondition(format!(
            "tape is a {} cartridge, not a data tape",
            tape.kind
        )));
    }
    let assignment = index
        .get_tape_assignment_snapshot(&admission.tape_uuid)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        Some(pool_id) => Some(pool_config(pool_id)?),
        None => None,
    };
    let actor_request = crate::write_owner::ManualFinalizeTapeActorRequest {
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
    Ok((tape, actor_request))
}

/// Acquire the same exact-tape owner used by append and dispatch terminal
/// close-out only after the pool assignment and generation have been captured.
/// A conflicting Object/read session therefore returns busy before audit,
/// intent publication, changer motion, or tape motion.
pub(crate) async fn manual_finalize_tape(
    state: &ApiState,
    admission: ManualFinalizeTapeAdmission,
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
    let (tape, mut actor_request) =
        prepare_manual_finalize_request(&index, admission, |pool_id| state.pool_config(pool_id))?;
    let pending_intent = remanence_state::FileCheckpointJournal::open(
        state.checkpoint_journal_dir.as_path(),
        actor_request.tape_uuid,
    )
    .and_then(|j| j.terminal_finalization_intent())
    .map_err(crate::status_from_state_error)?;
    let prefix = if pending_intent.is_none()
        && tape.scheme_id.is_some()
        && tape.highest_protected_ordinal < tape.total_committed_ordinals
    {
        Some(plan_epoch_finalization(state, &pool, &actor_request).await?)
    } else {
        None
    };
    if let Some(reply) = crate::write_owner::preflight_manual_finalize_with_prefix(
        &mut index,
        crate::write_owner::ManualFinalizePreflightConfig {
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
            audit_dir: state.audit_dir.as_path(),
            audit_fsync: state.audit_fsync,
            audit_append_lock: &state.audit_append_lock,
        },
        tape.voltag.as_deref(),
        &mut actor_request,
        prefix.as_ref(),
    )? {
        return Ok(crate::write_owner::ManualFinalizeTapeResult::Accepted(
            reply,
        ));
    }
    let projection = index
        .terminal_finalization(&actor_request.tape_uuid)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
        .ok_or_else(|| {
            Status::internal("durably accepted FinalizeTape has no finalization projection")
        })?;
    let accepted = crate::write_owner::ManualFinalizeTapeActorReply {
        operation_id: actor_request.candidate_operation_id,
        projection,
    };
    drop(index);

    let handle = start_finalize_attempt(state, &actor_request)?;
    let worker_state = state.clone();
    tokio::spawn(async move {
        let tape_uuid = actor_request.tape_uuid;
        let operation_id = actor_request.candidate_operation_id;
        if let Err(error) = run_started_manual_finalize_worker(
            worker_state,
            pool,
            tape_reservation,
            actor_request,
            Duration::from_secs(crate::readiness_wait::DEFAULT_OPERATION_READY_WAIT_SECONDS),
            handle,
        )
        .await
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
    actor_request: crate::write_owner::ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    run_manual_finalize_worker_with_budget(
        state,
        pool,
        _tape_reservation,
        actor_request,
        Duration::from_secs(crate::readiness_wait::DEFAULT_OPERATION_READY_WAIT_SECONDS),
    )
    .await
}

/// Keep one readiness deadline across every session-open attempt.
async fn run_manual_finalize_worker_with_budget(
    state: ApiState,
    pool: crate::write_owner::DrivePool,
    _tape_reservation: crate::write_owner::TapeReservation,
    actor_request: crate::write_owner::ManualFinalizeTapeActorRequest,
    ready_wait: Duration,
) -> Result<(), Status> {
    let handle = start_finalize_attempt(&state, &actor_request)?;
    run_started_manual_finalize_worker(
        state,
        pool,
        _tape_reservation,
        actor_request,
        ready_wait,
        handle,
    )
    .await
}

/// Publish the new attempt before returning acceptance or launching its worker.
fn start_finalize_attempt(
    state: &ApiState,
    request: &crate::write_owner::ManualFinalizeTapeActorRequest,
) -> Result<crate::operations::OperationHandle, Status> {
    record_finalize_attempt(
        state,
        request,
        remanence_state::AuditEvent::OperationStarted,
        None,
    )?;
    let handle = state
        .operations
        .register(request.candidate_operation_id, "finalize_tape");
    handle.publish_state(pb::OperationState::Running, &[]);
    Ok(handle)
}

/// Run an attempt whose durable and live RUNNING state is already visible.
async fn run_started_manual_finalize_worker(
    state: ApiState,
    pool: crate::write_owner::DrivePool,
    _tape_reservation: crate::write_owner::TapeReservation,
    actor_request: crate::write_owner::ManualFinalizeTapeActorRequest,
    ready_wait: Duration,
    handle: crate::operations::OperationHandle,
) -> Result<(), Status> {
    let id = actor_request.candidate_operation_id;
    let mut deadline = None;
    let result: Result<(), Status> = async {
        loop {
            match manual_finalize_attempt(state.clone(), pool.clone(), actor_request.clone()).await
            {
                Ok(()) => break Ok(()),
                Err(error) => {
                    crate::readiness_wait::wait_after_refusal(
                        &state,
                        &handle,
                        error,
                        ready_wait,
                        &mut deadline,
                    )
                    .await?
                }
            }
        }
    }
    .await;
    match result {
        Ok(()) => {
            handle.publish_state(pb::OperationState::Succeeded, &[]);
            Ok(())
        }
        Err(error) => {
            let summary = format!("{}; FinalizeTape with the same idempotency key resumes the finalization under the same operation id {id}", error.message());
            let recorded = record_finalize_attempt(
                &state,
                &actor_request,
                remanence_state::AuditEvent::OperationFailed,
                Some(&summary),
            );
            handle.publish_failed(&summary, &[]);
            if let Err(audit_error) = recorded {
                return Err(Status::internal(format!(
                    "{summary}; persist attempt failure: {audit_error}"
                )));
            }
            Err(Status::new(error.code(), summary))
        }
    }
}

/// Persist attempt state independently of the durable finalization intent.
fn record_finalize_attempt(
    state: &ApiState,
    request: &crate::write_owner::ManualFinalizeTapeActorRequest,
    event: remanence_state::AuditEvent,
    summary: Option<&str>,
) -> Result<(), Status> {
    let mut detail = std::collections::BTreeMap::from([
        (
            "finalize_attempt".into(),
            ciborium::value::Value::Bool(true),
        ),
        (
            "tape_uuid".into(),
            ciborium::value::Value::Bytes(request.tape_uuid.to_vec()),
        ),
        (
            "actor_fingerprint".into(),
            ciborium::value::Value::Text(request.actor_fingerprint.clone()),
        ),
    ]);
    if let Some(summary) = summary {
        detail.insert(
            "error_summary".into(),
            ciborium::value::Value::Text(summary.into()),
        );
    }
    crate::audit_projection::append_operation_audit(
        &mut state.index_write()?,
        state.audit_dir.as_ref(),
        state.audit_fsync,
        &state.audit_append_lock,
        crate::audit_projection::OperationAuditInput {
            actor: remanence_state::AuditActor::System,
            operation_id: request.candidate_operation_id,
            operation_kind: "finalize_tape",
            event,
            subject_kind: "tape",
            subject_id: Some(Uuid::from_bytes(request.tape_uuid).to_string()),
            idempotency_key: Some(request.idempotency_key),
            detail,
        },
    )
}

/// Try the session open and physical finalization with one drive reservation.
async fn manual_finalize_attempt(
    state: ApiState,
    pool: crate::write_owner::DrivePool,
    mut actor_request: crate::write_owner::ManualFinalizeTapeActorRequest,
) -> Result<(), Status> {
    let tape_uuid = actor_request.tape_uuid;
    let mut index = CatalogIndex::open(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let barcode = index
        .get_tape(&tape_uuid)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
    let library_serial = resolve_tape_library_serial(&state, &pool, &tape_uuid)?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(&state, &pool, library_serial.as_str(), &tape_uuid).await?;
    pool.set_holder(
        &mount.drive_key(),
        mount.drive_serial.as_deref(),
        "finalize_tape",
        actor_request.candidate_operation_id,
    );
    let drive = pool.drive_tx(&mount.drive_key())?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, &mount.library_serial, slot, mount.bay).await {
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
            if intent.manual.is_some() {
                let reconciled = (|| {
                    let mut lease = journal.acquire_exclusive_for_terminal_recovery()?;
                    let index = CatalogIndex::open(state.index_path.as_ref())?;
                    crate::write_owner::reconcile_manual_terminal_acceptance(
                        &index, &mut lease, &intent,
                    )
                    .map_err(|status| {
                        remanence_state::StateError::JournalReplayFailed(status.to_string())
                    })
                })();
                match reconciled {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        tracing::error!(tape_uuid = %Uuid::from_bytes(tape_uuid), %error, "cannot reconcile startup manual acceptance");
                        continue;
                    }
                }
            }
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?
        .ok_or_else(|| Status::not_found("manual terminal-recovery tape not found"))?;
    let assignment = index
        .get_tape_assignment_snapshot(&intent.tape_uuid)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        standalone_endpoint: None,
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
    let library_serial = resolve_tape_library_serial(state, &pool, &tape_uuid)?;
    let (mount, drive_reservation) =
        resolve_and_reserve_actor_mount(state, &pool, &library_serial, &tape_uuid).await?;
    let drive = pool.drive_tx(&mount.drive_key())?;
    if let Some(slot) = mount.source_slot {
        if let Err(error) = changer_move(&pool, &mount.library_serial, slot, mount.bay).await {
            drop(drive_reservation);
            return Err(error);
        }
    }
    let (reply, response) = oneshot::channel();
    if drive
        .send(crate::write_owner::DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
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
        .map_err(crate::startup_media_readiness::status_from_state_error)?;
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
            let library_serial = resolve_tape_library_serial(&state, &pool, &tape_uuid)?;
            resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?
        }
        ReadSessionTarget::LoadedDrive {
            library_serial,
            bay,
            ..
        } => {
            let drive_key = crate::drive_pool::DriveKey::new(library_serial.clone(), bay);
            let reservation = pool.reserve_drive(&drive_key)?;
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
    let drive = pool.drive_tx(&mount.drive_key())?;
    if let Some(slot) = mount.source_slot {
        changer_move(pool, &mount.library_serial, slot, mount.bay).await?;
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
    pool.set_holder(
        &mount.drive_key(),
        mount.drive_serial.as_deref(),
        "read session",
        session_id,
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

pub(crate) struct AppendFinishRequest {
    pub(crate) spool_path: std::path::PathBuf,
    pub(crate) content_sha256: [u8; 32],
    pub(crate) received_bytes: u64,
    pub(crate) staging_reason: &'static str,
    pub(crate) mtime: Option<String>,
    pub(crate) archive_path: std::path::PathBuf,
    pub(crate) caller_object_id: String,
    pub(crate) expected_content_sha256: Option<[u8; 32]>,
    pub(crate) expected_object_id: Option<[u8; 16]>,
    pub(crate) input_kind: crate::WriteObjectInputKind,
}

pub(crate) async fn append_finish(
    state: &ApiState,
    session_id: Uuid,
    request: AppendFinishRequest,
) -> Result<pb::ObjectRecord, Status> {
    let pool = state.drive_pool()?.clone();
    let mounted = pool.session(session_id)?;
    let drive = pool.drive_tx(&mounted.drive_key())?;
    let live_write_counter = mounted
        .drive_uuid
        .as_deref()
        .map(|drive_uuid| state.drive_counters(drive_uuid));
    let (reply_tx, reply_rx) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::AppendFinish {
            session_id,
            source: crate::WriteObjectSource::Spool {
                path: request.spool_path,
                mtime: request.mtime,
                content_sha256: request.content_sha256,
                received_bytes: request.received_bytes,
                staging_reason: request.staging_reason,
            },
            archive_path: request.archive_path,
            caller_object_id: request.caller_object_id,
            expected_content_sha256: request.expected_content_sha256,
            expected_object_id: request.expected_object_id,
            input_kind: request.input_kind,
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
            expected_object_id: None,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    let drive = pool.drive_tx(&mounted.drive_key())?;
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
    library_serial: &str,
    src: u16,
    dst: u16,
) -> Result<(), Status> {
    let changer = pool.changer_tx(library_serial)?;
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
        let _ = drive_unload(pool, &mount.drive_key()).await;
        let _ = changer_move(pool, &mount.library_serial, mount.bay, slot).await;
    }
}

fn should_compensate_open_mount(err: &Status) -> bool {
    !err.message().contains("media_readiness_state=")
}

async fn drive_unload(
    pool: &crate::write_owner::DrivePool,
    drive_key: &crate::drive_pool::DriveKey,
) -> Result<Duration, Status> {
    let drive = pool.drive_tx(drive_key)?;
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
///
/// The SSC UNLOAD and the changer move form one unit with respect to
/// shutdown. A cartridge unloaded but never moved stays unthreaded in the
/// drive, which then answers NOT READY / INITIALIZING COMMAND REQUIRED
/// (`02/04/02`) until a host sends LOAD, and every later readiness wait on it
/// stalls. So:
///
/// - The pair runs on a detached task that owns the bay reservation.
///   Dropping the caller's future (an eviction inside an RPC whose client
///   disconnects) cannot separate the UNLOAD from the move, and no other
///   operation can reserve the bay until the move has finished.
/// - An idle or eviction dismount does not start its UNLOAD once shutdown
///   has begun; the shutdown pass dismounts that cartridge instead. The check
///   runs under the reservation: either the UNLOAD starts before shutdown
///   begins, and the shutdown pass waits for the reservation the task
///   releases only after the move, or the UNLOAD never starts.
/// - The shutdown pass takes each bay reservation before its own UNLOAD and
///   awaits the move before it moves on, and the daemon serves until that
///   pass returns, so a graceful stop never exits between the two commands.
///
/// A hard kill between the commands or a failed changer move leaves the cartridge
/// unthreaded; session open and media-readiness waits then send one SSC LOAD
/// to thread it.
///
/// On success the reservation returns to the caller; on failure it is
/// released once the task has finished with the drive.
async fn dismount_reserved_cartridge(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    seated: &crate::write_owner::SeatedCartridge,
    reason: DismountReason,
    reservation: crate::write_owner::DriveReservation,
) -> Result<crate::write_owner::DriveReservation, Status> {
    let holder_kind = match reason {
        DismountReason::Evicted => "eviction_dismount",
        DismountReason::Shutdown => "shutdown_dismount",
        DismountReason::IdleTimeout => "parked_dismount",
    };
    pool.set_internal_holder(&seated.drive_key(), None, holder_kind);
    ensure_seated_cartridge_media_readiness_admitted(state, seated, reason)?;
    ensure_seated_cartridge_matches_snapshot(state, seated)?;

    let pool = pool.clone();
    let seated = seated.clone();
    let bay = seated.bay;
    tokio::spawn(async move {
        dismount_pair(&pool, &seated, reason).await?;
        Ok(reservation)
    })
    .await
    .map_err(|err| Status::internal(format!("dismount task for drive bay 0x{bay:04x}: {err}")))?
}

/// The SSC UNLOAD and changer move of [`dismount_reserved_cartridge`], run
/// on its detached task while that task owns the bay reservation.
async fn dismount_pair(
    pool: &crate::write_owner::DrivePool,
    seated: &crate::write_owner::SeatedCartridge,
    reason: DismountReason,
) -> Result<(), Status> {
    if reason != DismountReason::Shutdown && pool.is_shutting_down() {
        return Err(Status::unavailable(format!(
            "daemon shutdown began before the {} dismount of drive bay 0x{:04x} started its unload; the shutdown pass dismounts it",
            reason.as_str(),
            seated.bay
        )));
    }
    let started = Instant::now();
    let ssc_unload = drive_unload(pool, &seated.drive_key()).await?;
    let move_started = Instant::now();
    if let Err(err) = changer_move(pool, &seated.library_serial, seated.bay, seated.home_slot).await
    {
        tracing::error!(
            library_serial = %seated.library_serial,
            bay = seated.bay,
            home_slot = seated.home_slot,
            barcode = seated.barcode.as_deref().unwrap_or("(unknown)"),
            unload = reason.as_str(),
            error = %err,
            "drive unloaded but the move home failed; the cartridge stays unthreaded in the drive until a changer move or an SSC LOAD"
        );
        return Err(Status::new(
            err.code(),
            format!(
                "drive bay 0x{:04x} unloaded but the move to slot 0x{:04x} failed; cartridge {} stays unthreaded in the drive: {}",
                seated.bay,
                seated.home_slot,
                seated.barcode.as_deref().unwrap_or("(unknown)"),
                err.message()
            ),
        ));
    }
    let changer_move = move_started.elapsed();
    if let Some(parked) = pool.parked_at(&seated.drive_key()) {
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
    crate::startup_media_readiness::ensure_media_readiness_admitted(
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
    let Ok(pool) = state.drive_pool().cloned() else {
        return;
    };
    let key = parked.seated.drive_key();
    let mut timers = pool.parked.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(previous) = timers.idle_timers.remove(&key) {
        previous.abort();
    }
    let task = runtime.spawn(async move {
        let pool = match state.drive_pool() {
            Ok(pool) => pool.clone(),
            Err(_) => return,
        };
        tokio::time::sleep(Duration::from_secs(timeout_seconds)).await;
        loop {
            if pool.is_shutting_down() || !pool.parked_is_current(&parked) {
                return;
            }
            let reservation = match pool.reserve_drive(&parked.seated.drive_key()) {
                Ok(reservation) => reservation,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            pool.set_internal_holder(&parked.seated.drive_key(), None, "parked_dismount");
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
                reservation,
            )
            .await
            {
                Ok(_reservation) => return,
                Err(_) if pool.is_shutting_down() => return,
                Err(err) => tracing::warn!(
                    bay = parked.seated.bay,
                    error = %err,
                    "idle drive unload failed; retrying after the configured timeout"
                ),
            }
            tokio::time::sleep(Duration::from_secs(timeout_seconds)).await;
        }
    });
    timers.idle_timers.insert(key, task.abort_handle());
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
        .filter(|library| pool.operates_library(&library.serial))
    {
        for bay in &library.drive_bays {
            let Some(home_slot) = bay.source_slot.filter(|_| bay.loaded) else {
                continue;
            };
            let drive_key =
                crate::drive_pool::DriveKey::new(library.serial.clone(), bay.element_address);
            if pool.drive_tx(&drive_key).is_err() {
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
    state.begin_shutdown();
    let Ok(pool) = state.drive_pool() else {
        return Ok(());
    };
    pool.begin_shutdown();
    let mut failures = Vec::new();
    for (_, (session_id, mounted)) in pool.sessions_by_drive() {
        let drive = match pool.drive_tx(&mounted.drive_key()) {
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
            Ok(Err(err))
                if err.code() == tonic::Code::FailedPrecondition
                    && err.message() == "active session is a read session" =>
            {
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
    for drive_key in pool.drive_keys() {
        let remanence_state::DriveKey::Bay {
            library_serial,
            bay,
        } = &drive_key
        else {
            continue; // Daemon changer actors only dismount library endpoints.
        };
        let reservation_wait_started = Instant::now();
        let reservation = loop {
            match pool.reserve_drive_for_shutdown(&drive_key) {
                Ok(reservation) => break Some(reservation),
                Err(err) => {
                    if err.code() == tonic::Code::NotFound
                        || pool.sessions_by_drive().contains_key(&drive_key)
                    {
                        failures.push(format!("{} bay 0x{:04x}: {err}", *library_serial, *bay));
                        break None;
                    }
                    if reservation_wait_started.elapsed() >= SHUTDOWN_RESERVATION_WAIT {
                        failures.push(format!(
                            "{} bay 0x{:04x}: timed out waiting for an idle drive reservation",
                            *library_serial, *bay
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
                .find(|library| library.serial == *library_serial)
                .and_then(|library| {
                    library
                        .drive_bays
                        .iter()
                        .find(|candidate| candidate.element_address == *bay)
                        .cloned()
                })
        });
        let Some(current) = current else {
            failures.push(format!(
                "{} bay 0x{:04x}: missing from shutdown inventory",
                *library_serial, *bay
            ));
            drop(reservation);
            continue;
        };
        let Some(home_slot) = current.source_slot.filter(|_| current.loaded) else {
            drop(reservation);
            continue;
        };
        let parked = pool.parked_at(&drive_key);
        let seated = crate::write_owner::SeatedCartridge {
            bay: *bay,
            library_serial: library_serial.clone(),
            barcode: current.loaded_tape,
            home_slot,
            tape_uuid: parked.as_ref().and_then(|parked| parked.seated.tape_uuid),
            prior_session_id: parked
                .as_ref()
                .and_then(|parked| parked.seated.prior_session_id),
        };
        if let Err(err) =
            dismount_reserved_cartridge(state, pool, &seated, DismountReason::Shutdown, reservation)
                .await
        {
            failures.push(format!("{} bay 0x{:04x}: {err}", *library_serial, *bay));
        }
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

impl ActorMount {
    fn drive_key(&self) -> crate::drive_pool::DriveKey {
        crate::drive_pool::DriveKey::new(self.library_serial.clone(), self.bay)
    }
}

fn library_contains_barcode(library: &remanence_library::Library, barcode: &str) -> bool {
    library
        .drive_bays
        .iter()
        .any(|bay| bay.loaded && bay.loaded_tape.as_deref() == Some(barcode))
        || library
            .slots
            .iter()
            .any(|slot| slot.full && slot.cartridge.as_deref() == Some(barcode))
}

/// Resolve media ownership from the current physical inventory. Tape UUIDs are
/// global, but changer motion must always be routed to exactly one operated
/// logical library.
pub(crate) fn resolve_tape_library_serial(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    tape_uuid: &TapeUuid,
) -> Result<String, Status> {
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let tape = index
        .get_tape(tape_uuid)
        .map_err(status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    let barcode = tape.voltag.as_deref().ok_or_else(|| {
        Status::failed_precondition("tape has no catalogued voltag and cannot be located")
    })?;
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library snapshot unavailable"))?;
    let mut matches = snapshot
        .report
        .libraries
        .iter()
        .filter(|library| pool.operates_library(&library.serial))
        .filter(|library| library_contains_barcode(library, barcode))
        .map(|library| library.serial.clone())
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [library_serial] => Ok(library_serial.clone()),
        [] => Err(Status::not_found(format!(
            "cartridge {barcode} is not present in any library operated by this daemon"
        ))),
        _ => Err(Status::failed_precondition(format!(
            "cartridge {barcode} appears in multiple operated libraries: {}",
            matches.join(", ")
        ))),
    }
}

fn located_tapes_in_operated_libraries(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    library_constraint: Option<&str>,
) -> Result<HashMap<TapeUuid, String>, Status> {
    if let Some(library_serial) = library_constraint {
        if !pool.operates_library(library_serial) {
            return Err(Status::failed_precondition(format!(
                "library {library_serial} is not operated by this daemon"
            )));
        }
    }
    let index = CatalogIndex::open_read_only(state.index_path.as_ref())
        .map_err(|error| Status::internal(error.to_string()))?;
    let voltags = crate::library::voltag_uuid_map(&index)?;
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library snapshot unavailable"))?;
    let mut locations = HashMap::new();
    for library in snapshot.report.libraries.iter().filter(|library| {
        pool.operates_library(&library.serial)
            && library_constraint.is_none_or(|expected| expected == library.serial)
    }) {
        let barcodes = library
            .drive_bays
            .iter()
            .filter(|bay| bay.loaded)
            .filter_map(|bay| bay.loaded_tape.as_deref())
            .chain(
                library
                    .slots
                    .iter()
                    .filter(|slot| slot.full)
                    .filter_map(|slot| slot.cartridge.as_deref()),
            );
        for barcode in barcodes {
            let Some(tape_uuid) = voltags
                .get(barcode)
                .and_then(|bytes| <TapeUuid>::try_from(bytes.as_slice()).ok())
            else {
                continue;
            };
            if let Some(previous) = locations.insert(tape_uuid, library.serial.clone()) {
                if previous != library.serial {
                    return Err(Status::failed_precondition(format!(
                        "catalogued cartridge {barcode} appears in multiple operated libraries: {previous}, {}",
                        library.serial
                    )));
                }
            }
        }
    }
    Ok(locations)
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
        let busy_bays = pool.busy_bays(library_serial);
        let plan =
            resolve_actor_mount(state, library_serial, tape_uuid, &busy_bays).map_err(|error| {
                if error.code() == tonic::Code::FailedPrecondition
                    && (error.message().contains("busy")
                        || error.message().contains("no free drive"))
                {
                    let specific = busy_bays
                        .iter()
                        .copied()
                        .find(|bay| error.message() == format!("drive bay 0x{bay:04x} is busy"));
                    pool.append_holders(error, |key| {
                        key.library_serial() == Some(library_serial)
                            && specific.is_none_or(|bay| key.bay() == Some(bay))
                    })
                } else {
                    error
                }
            })?;
        let bay = match &plan {
            ActorMountPlan::Ready(mount) | ActorMountPlan::Evict { mount, .. } => mount.bay,
        };
        let drive_key = crate::drive_pool::DriveKey::new(library_serial.to_string(), bay);
        match pool.reserve_drive(&drive_key) {
            Ok(reservation) => match plan {
                ActorMountPlan::Ready(mount) => {
                    ensure_actor_mount_media_readiness_admitted(state, library_serial, &mount)?;
                    return Ok((mount, reservation));
                }
                ActorMountPlan::Evict { mount, mut seated } => {
                    ensure_actor_mount_media_readiness_admitted(state, library_serial, &mount)?;
                    if let Some(parked) = pool.parked_at(&seated.drive_key()) {
                        if parked.seated.library_serial == seated.library_serial
                            && parked.seated.barcode == seated.barcode
                            && parked.seated.home_slot == seated.home_slot
                        {
                            seated.tape_uuid = parked.seated.tape_uuid;
                            seated.prior_session_id = parked.seated.prior_session_id;
                        }
                    }
                    let reservation = dismount_reserved_cartridge(
                        state,
                        pool,
                        &seated,
                        DismountReason::Evicted,
                        reservation,
                    )
                    .await?;
                    return Ok((mount, reservation));
                }
            },
            Err(err)
                if err.code() == tonic::Code::FailedPrecondition
                    && err.message().split("; held by: ").next()
                        == Some(format!("{} is busy", drive_key.label()).as_str())
                    && attempt + 1 < ATTEMPTS =>
            {
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
    // Validate inventory before a library-wide fence masks a missing cartridge.
    // Ignore reservations here so a readiness owner still yields its fence id.
    resolve_actor_mount_plan_from_library(library, voltag.as_str(), &HashSet::new())?;
    // Surface the fence identity even when its readiness waiter owns the bay.
    let seated = library
        .drive_bays
        .iter()
        .find(|bay| bay.loaded && bay.loaded_tape.as_deref() == Some(voltag.as_str()));
    crate::startup_media_readiness::ensure_media_readiness_admitted(
        &index,
        "open session",
        library_serial,
        seated.map(|bay| bay.element_address),
        barcode.as_deref(),
        seated.is_none_or(|bay| bay.source_slot.is_some()),
    )?;
    let mut plan = resolve_actor_mount_plan_from_library(library, voltag.as_str(), busy_bays)?;
    let mount = plan.mount_mut();
    mount.library_serial = library_serial.to_string();
    mount.barcode = barcode;
    enrich_actor_mount_from_catalog(&index, library_serial, mount)?;
    if mount.drive_serial.is_none() {
        mount.drive_serial = library
            .drive_bays
            .iter()
            .find(|drive| drive.element_address == mount.bay)
            .and_then(|drive| drive.installed.as_ref())
            .map(|drive| drive.serial.clone());
    }
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
    crate::startup_media_readiness::ensure_media_readiness_admitted(
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
    crate::startup_media_readiness::ensure_media_readiness_admitted(
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

/// Reserve the exact tape and drive for the shared explicit recovery operation.
/// The spawned owner retains reservations even if the RPC client disconnects.
pub(crate) async fn supersede_tail(
    state: &ApiState,
    request: crate::pb::SupersedeTailRequest,
    actor: remanence_state::AuditActor,
) -> Result<crate::pb::SupersedeTailResponse, Status> {
    let state = state.clone();
    tokio::spawn(async move {
        let tape_uuid =
            crate::catalog_conversion::decode_uuid_bytes(&request.tape_uuid, "tape_uuid")?;
        let pool = state.drive_pool()?.clone();
        let _tape_reservation = pool.reserve_tape(tape_uuid)?;
        let library_serial = resolve_tape_library_serial(&state, &pool, &tape_uuid)?;
        let (mount, _drive_reservation) =
            resolve_and_reserve_actor_mount(&state, &pool, &library_serial, &tape_uuid).await?;
        pool.set_internal_holder(
            &mount.drive_key(),
            mount.drive_serial.as_deref(),
            "supersede_tail",
        );
        let drive = pool.drive_tx(&mount.drive_key())?;
        if let Some(slot) = mount.source_slot {
            changer_move(&pool, &mount.library_serial, slot, mount.bay).await?;
        }
        let (reply, response) = oneshot::channel();
        let command = crate::write_owner::DriveCommand::SupersedeTail {
            request,
            actor,
            needs_drive_load: mount.needs_drive_load,
            library_serial: mount.library_serial.clone(),
            barcode: mount.barcode.clone(),
            source_slot: mount.source_slot,
            drive_serial: mount.drive_serial.clone(),
            reply,
        };
        if drive.send(command).await.is_err() {
            compensate_open_mount(&pool, &mount).await;
            return Err(Status::unavailable("drive actor is unavailable"));
        }
        let result = response
            .await
            .map_err(|_| Status::unavailable("drive actor stopped during tail recovery"))?;
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
        result
    })
    .await
    .map_err(|e| Status::internal(e.to_string()))?
}

/// Plan terminal metadata under exact-tape ownership without writing a barrier close.
async fn plan_epoch_finalization(
    state: &ApiState,
    pool: &crate::write_owner::DrivePool,
    request: &crate::write_owner::ManualFinalizeTapeActorRequest,
) -> Result<remanence_parity::TerminalPrefixPlan, Status> {
    let library = resolve_tape_library_serial(state, pool, &request.tape_uuid)?;
    let (mount, reservation) =
        resolve_and_reserve_actor_mount(state, pool, &library, &request.tape_uuid).await?;
    if let Some(slot) = mount.source_slot {
        changer_move(pool, &mount.library_serial, slot, mount.bay).await?;
    }
    // Bind the generation before the actor retains a newly rebuilt planning state.
    let parked = mount.home_slot.map(|home_slot| {
        pool.parked_at(&mount.drive_key())
            .filter(|p| p.seated.tape_uuid == Some(request.tape_uuid))
            .unwrap_or_else(|| {
                pool.park_cartridge(crate::write_owner::SeatedCartridge {
                    bay: mount.bay,
                    library_serial: mount.library_serial.clone(),
                    barcode: mount.barcode.clone(),
                    home_slot,
                    tape_uuid: Some(request.tape_uuid),
                    prior_session_id: None,
                })
            })
    });
    let drive = pool.drive_tx(&mount.drive_key())?;
    let (reply, received) = oneshot::channel();
    drive
        .send(crate::write_owner::DriveCommand::PlanFinalization {
            request: request.clone(),
            needs_drive_load: mount.needs_drive_load,
            barcode: mount.barcode.clone(),
            source_slot: mount.source_slot,
            drive_serial: mount.drive_serial.clone(),
            reply,
        })
        .await
        .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
    let result = received
        .await
        .map_err(|_| Status::unavailable("drive actor stopped during epoch close"))?;
    if let Some(parked) = parked {
        schedule_idle_dismount(state.clone(), parked);
    }
    drop(reservation);
    result
}

/// Exercise the real idle timer and eviction removal entry points with model actors.
#[cfg(test)]
pub(crate) async fn test_remove_seated_epoch(
    state: &ApiState,
    path: &str,
    seated: crate::write_owner::SeatedCartridge,
) {
    let pool = state.drive_pool().unwrap();
    if path == "idle" {
        let parked = pool.parked_at(&seated.drive_key()).unwrap();
        schedule_idle_dismount(state.clone(), parked.clone());
        tokio::time::timeout(Duration::from_secs(10), async {
            while pool.parked_is_current(&parked) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("idle removal finishes");
    } else {
        let reservation = pool.reserve_drive(&seated.drive_key()).unwrap();
        dismount_reserved_cartridge(state, pool, &seated, DismountReason::Evicted, reservation)
            .await
            .unwrap();
    }
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
            scheme: None,
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

        let batched = crate::select_tape_in_pool_for_write_session(
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
    fn tape_location_resolution_selects_the_unique_operated_library() {
        const TAPE_UUID: TapeUuid = [0xA1; 16];
        let temp = tempfile::tempdir().expect("temporary catalog");
        let mut index =
            CatalogIndex::open(temp.path().join("state.sqlite")).expect("open temporary catalog");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: TAPE_UUID,
                voltag: "LOC001L9".to_string(),
                block_size: 256 * 1024,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        let mut state = ApiState::new(index);
        let library_a = test_library(vec![], vec![slot(0x0401, "OTHER1L9")]);
        let mut library_b = test_library(vec![], vec![slot(0x0401, "LOC001L9")]);
        library_b.serial = "LIB002".to_string();
        state.library_snapshot = Some(std::sync::Arc::new(std::sync::RwLock::new(
            std::sync::Arc::new(crate::live_status::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
                    libraries: vec![library_a, library_b],
                    warnings: Vec::new(),
                },
                captured_at: time::OffsetDateTime::UNIX_EPOCH,
            }),
        )));
        let (changer_a, _) = tokio::sync::mpsc::channel(1);
        let (changer_b, _) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new_with_lifecycle(
            HashMap::from([
                ("LIB001".to_string(), changer_a),
                ("LIB002".to_string(), changer_b),
            ]),
            HashMap::new(),
            std::sync::Arc::new(HashMap::new()),
            crate::drive_pool::DrivePoolLifecycle::default(),
        );

        assert_eq!(
            resolve_tape_library_serial(&state, &pool, &TAPE_UUID)
                .expect("resolve unique physical location"),
            "LIB002"
        );
    }

    #[test]
    fn tape_location_resolution_rejects_duplicate_operated_locations() {
        const TAPE_UUID: TapeUuid = [0xA2; 16];
        let temp = tempfile::tempdir().expect("temporary catalog");
        let mut index =
            CatalogIndex::open(temp.path().join("state.sqlite")).expect("open temporary catalog");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: TAPE_UUID,
                voltag: "DUP001L9".to_string(),
                block_size: 256 * 1024,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        let mut state = ApiState::new(index);
        let library_a = test_library(vec![], vec![slot(0x0401, "DUP001L9")]);
        let mut library_b = test_library(vec![], vec![slot(0x0401, "DUP001L9")]);
        library_b.serial = "LIB002".to_string();
        state.library_snapshot = Some(std::sync::Arc::new(std::sync::RwLock::new(
            std::sync::Arc::new(crate::live_status::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
                    libraries: vec![library_a, library_b],
                    warnings: Vec::new(),
                },
                captured_at: time::OffsetDateTime::UNIX_EPOCH,
            }),
        )));
        let (changer_a, _) = tokio::sync::mpsc::channel(1);
        let (changer_b, _) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new_with_lifecycle(
            HashMap::from([
                ("LIB001".to_string(), changer_a),
                ("LIB002".to_string(), changer_b),
            ]),
            HashMap::new(),
            std::sync::Arc::new(HashMap::new()),
            crate::drive_pool::DrivePoolLifecycle::default(),
        );

        let err = resolve_tape_library_serial(&state, &pool, &TAPE_UUID)
            .expect_err("duplicate physical location must fail closed");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("LIB001, LIB002"), "{err}");
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

    const POOL_A: &str = "fieldtest-a";
    const POOL_B: &str = "fieldtest-b";
    const POOL_TAPE_A: TapeUuid = [0xA1; 16];
    const POOL_TAPE_B: TapeUuid = [0xB1; 16];

    struct ThreeBayHarness {
        _temp: tempfile::TempDir,
        state: ApiState,
        pool: crate::write_owner::DrivePool,
        drive_rxs: HashMap<u16, mpsc::Receiver<crate::write_owner::DriveCommand>>,
        changer_rx: mpsc::Receiver<crate::write_owner::ChangerCommand>,
    }

    /// Three drive bays in LIB001 and one catalogued cartridge for each of two
    /// pools: FTA001L9 in fieldtest-a and FTB001L9 in fieldtest-b. The caller
    /// places the cartridges in the snapshot.
    fn three_bay_two_pool_harness(drive_bays: Vec<DriveBay>, slots: Vec<Slot>) -> ThreeBayHarness {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        const BLOCK_SIZE: u32 = 256 * 1024;
        let temp = tempfile::Builder::new()
            .prefix("remanence-three-bay-pools")
            .tempdir()
            .expect("tempdir");
        let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("open catalog");
        let mut pool_configs = Vec::new();
        for (pool_id, tape_uuid, voltag) in [
            (POOL_A, POOL_TAPE_A, "FTA001L9"),
            (POOL_B, POOL_TAPE_B, "FTB001L9"),
        ] {
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
                    voltag: voltag.to_string(),
                    block_size: BLOCK_SIZE,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision pool tape");
            index
                .project_tape_pool_membership(tape_uuid, pool_id)
                .expect("assign pool tape");
            pool_configs.push(TapePoolConfig {
                scheme: None,
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
            });
        }
        let mut state = ApiState::new_with_pool_configs(index, pool_configs);
        state.default_library_serial = Some(Arc::new("LIB001".to_string()));
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::live_status::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
                    libraries: vec![test_library(drive_bays, slots)],
                    warnings: Vec::new(),
                },
                captured_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        ))));
        let mut drive_txs = HashMap::new();
        let mut drive_rxs = HashMap::new();
        let mut reservations = HashMap::new();
        for bay in [0x0101, 0x0102, 0x0103] {
            let (tx, rx) = mpsc::channel(1);
            drive_txs.insert(bay, tx);
            drive_rxs.insert(bay, rx);
            reservations.insert(bay, AtomicBool::new(false));
        }
        let (changer_tx, changer_rx) = mpsc::channel(1);
        let pool =
            crate::write_owner::DrivePool::new(changer_tx, drive_txs, Arc::new(reservations));
        state.drive_pool = Some(pool.clone());
        ThreeBayHarness {
            _temp: temp,
            state,
            pool,
            drive_rxs,
            changer_rx,
        }
    }

    /// RPC lifecycle retains the exact actor verdict, releases ownership and replays freshness.
    /// Busy diagnostics preserve the bay-full refusal when no reservation caused it.
    #[tokio::test]
    async fn bay_full_refusal_has_no_false_holder_list() {
        let harness = three_bay_two_pool_harness(
            vec![
                drive_bay(0x0101, true, Some("FTA001L9")),
                drive_bay(0x0102, true, Some("OTHER2")),
                drive_bay(0x0103, true, Some("OTHER3")),
            ],
            vec![slot(0x0402, "FTB001L9")],
        );
        let error =
            resolve_and_reserve_actor_mount(&harness.state, &harness.pool, "LIB001", &POOL_TAPE_B)
                .await
                .unwrap_err();
        assert_eq!(
            error.message(),
            "no free drive bay available to load cartridge FTB001L9"
        );
    }

    /// Inventory and tail recovery publish their internal identity before actor work.
    #[tokio::test]
    async fn inventory_and_supersede_tail_reservations_name_holders() {
        let mut harness = three_bay_two_pool_harness(
            vec![
                drive_bay(0x0101, true, Some("FTA001L9")),
                drive_bay(0x0102, false, None),
                drive_bay(0x0103, false, None),
            ],
            vec![slot(0x0402, "FTB001L9")],
        );
        for kind in ["inventory", "supersede_tail"] {
            let state = harness.state.clone();
            let task = tokio::spawn(async move {
                if kind == "inventory" {
                    let (tx, _rx) = mpsc::channel(1);
                    tape_inventory_critical(state, POOL_TAPE_A, tx).await
                } else {
                    supersede_tail(
                        &state,
                        pb::SupersedeTailRequest {
                            tape_uuid: POOL_TAPE_A.to_vec(),
                            ..Default::default()
                        },
                        remanence_state::AuditActor::System,
                    )
                    .await
                    .map(|_| ())
                }
            });
            let command = harness
                .drive_rxs
                .get_mut(&0x0101)
                .unwrap()
                .recv()
                .await
                .unwrap();
            let error = harness
                .pool
                .reserve_drive(&crate::drive_pool::DriveKey::new("LIB001", 0x0101))
                .unwrap_err();
            assert!(
                error.message().contains(&format!("held by {kind}")),
                "{error}"
            );
            assert!(!error.message().contains("admission or cleanup"));
            match command {
                crate::write_owner::DriveCommand::TapeInventory { reply, .. } => {
                    reply.send(Ok(())).unwrap()
                }
                crate::write_owner::DriveCommand::SupersedeTail { reply, .. } => reply
                    .send(Ok(pb::SupersedeTailResponse::default()))
                    .unwrap(),
                _ => panic!("unexpected actor command"),
            }
            task.await.unwrap().unwrap();
            assert!(harness.pool.busy_bays("LIB001").is_empty());
        }
    }

    /// The timer retains its named holder through both halves of dismount.
    #[tokio::test]
    async fn parked_dismount_reservation_names_holder() {
        let mut harness = dismount_harness(1);
        let parked = harness.pool.park_cartridge(harness.seated.clone());
        schedule_idle_dismount(harness.state.clone(), parked);
        let command = tokio::time::timeout(Duration::from_secs(3), harness.drive_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let error = harness
            .pool
            .reserve_drive(&harness.seated.drive_key())
            .unwrap_err();
        assert!(
            error.message().contains("held by parked_dismount"),
            "{error}"
        );
        let crate::write_owner::DriveCommand::Unload { reply } = command else {
            panic!("unload")
        };
        reply.send(Ok(Duration::ZERO)).unwrap();
        let crate::write_owner::ChangerCommand::Move { reply, .. } =
            harness.changer_rx.recv().await.unwrap()
        else {
            panic!("move")
        };
        let error = harness
            .pool
            .reserve_drive(&harness.seated.drive_key())
            .unwrap_err();
        assert!(error.message().contains("held by parked_dismount"));
        reply.send(Ok(())).unwrap();
    }

    #[tokio::test]
    async fn verification_operation_result_cancel_release_and_replay() {
        use crate::pb::catalog_server::Catalog;
        use tokio_stream::StreamExt;
        let mut harness = three_bay_two_pool_harness(
            vec![
                drive_bay(0x0101, true, Some("FTA001L9")),
                drive_bay(0x0102, false, None),
                drive_bay(0x0103, false, None),
            ],
            vec![slot(0x0402, "FTB001L9")],
        );
        let service = harness.state.catalog_service();
        let expected = pb::TapeIndexVerification {
            tape_uuid: POOL_TAPE_A.to_vec(),
            state: pb::TapeIndexVerificationState::VerifiedComplete as i32,
            edition_digest: Some(vec![7; 32]),
            ..Default::default()
        };
        for cancelled in [true, false] {
            let operation = service
                .start_tape_verification(tonic::Request::new(pb::StartTapeVerificationRequest {
                    tape_uuid: POOL_TAPE_A.to_vec(),
                }))
                .await
                .unwrap()
                .into_inner();
            let id = Uuid::from_slice(&operation.operation_id).unwrap();
            let mut watch = harness.state.operations.watch(&id).unwrap();
            let command = harness
                .drive_rxs
                .get_mut(&0x0101)
                .unwrap()
                .recv()
                .await
                .unwrap();
            let crate::write_owner::DriveCommand::VerifyTapeIndex { handle, reply, .. } = command
            else {
                panic!("verification command")
            };
            assert_eq!(
                service
                    .get_tape_verification(tonic::Request::new(pb::GetOperationRequest {
                        operation_id: operation.operation_id.clone()
                    }))
                    .await
                    .unwrap_err()
                    .code(),
                tonic::Code::FailedPrecondition
            );
            let refusal = harness
                .pool
                .reserve_drive(&crate::drive_pool::DriveKey::new("LIB001", 0x0101))
                .unwrap_err();
            assert!(refusal
                .message()
                .contains(&format!("tape_verification {id}")));
            let other_reservations = [0x0102, 0x0103].map(|bay| {
                harness
                    .pool
                    .reserve_drive(&crate::drive_pool::DriveKey::new("LIB001", bay))
                    .unwrap()
            });
            let read_refusal = open_read_session(
                &harness.state,
                ReadSessionTarget::Tape {
                    tape_uuid: POOL_TAPE_B,
                },
                None,
            )
            .await
            .unwrap_err();
            assert!(
                read_refusal
                    .message()
                    .contains(&format!("tape_verification {id}")),
                "{read_refusal}"
            );
            assert!(read_refusal.message().contains("DRV0101"));
            let write_refusal = open_write_session(
                remanence_state::AuditActor::System,
                &harness.state,
                WriteSessionTarget::PinnedTape {
                    tape_uuid: POOL_TAPE_B,
                    required_pool_id: POOL_B.into(),
                },
                None,
            )
            .await
            .unwrap_err();
            assert!(
                write_refusal
                    .message()
                    .contains(&format!("tape_verification {id}")),
                "{write_refusal}"
            );
            assert!(write_refusal.message().contains("DRV0101"));
            drop(other_reservations);
            if cancelled {
                harness.state.operations.request_cancel(&id).unwrap();
                assert!(handle.is_cancelled());
                // A late cancellation still suppresses an actor's completed verdict.
                reply.send(Ok(expected.clone())).unwrap();
            } else {
                reply.send(Ok(expected.clone())).unwrap();
            }
            let terminal = loop {
                let status = watch.next().await.unwrap().unwrap();
                if crate::operations::is_terminal(&status) {
                    break status;
                }
            };
            assert_eq!(
                terminal.state,
                if cancelled {
                    pb::OperationState::Cancelled
                } else {
                    pb::OperationState::Succeeded
                } as i32
            );
            assert!(harness.pool.busy_bays("LIB001").is_empty());
            harness
                .state
                .record_cancel_requested(remanence_state::AuditActor::System, id, None, true)
                .unwrap();
            let durable = harness
                .state
                .index()
                .unwrap()
                .get_operation(&id.to_string())
                .unwrap()
                .unwrap();
            assert_eq!(
                durable.state,
                if cancelled {
                    "cancelled_before_dispatch"
                } else {
                    "finished"
                }
            );
            let tape = harness
                .state
                .index()
                .unwrap()
                .get_tape(&POOL_TAPE_A)
                .unwrap()
                .unwrap();
            let fetched = service
                .get_tape_verification(tonic::Request::new(pb::GetOperationRequest {
                    operation_id: operation.operation_id,
                }))
                .await;
            if cancelled {
                assert!(tape.last_verified_at_utc.is_none());
                assert_eq!(fetched.unwrap_err().code(), tonic::Code::FailedPrecondition);
            } else {
                assert!(tape.last_verified_at_utc.is_some());
                assert_eq!(fetched.unwrap().into_inner(), expected);
            }
        }
        let unary = tokio::spawn(async move {
            service
                .verify_tape_index(tonic::Request::new(pb::VerifyTapeIndexRequest {
                    tape_uuid: POOL_TAPE_A.to_vec(),
                }))
                .await
        });
        let command = harness
            .drive_rxs
            .get_mut(&0x0101)
            .unwrap()
            .recv()
            .await
            .unwrap();
        let crate::write_owner::DriveCommand::VerifyTapeIndex { reply, .. } = command else {
            panic!("verification command")
        };
        reply.send(Ok(expected.clone())).unwrap();
        assert_eq!(unary.await.unwrap().unwrap().into_inner(), expected);
        let records =
            remanence_state::FileAuditLog::replay(harness.state.audit_dir.as_ref()).unwrap();
        assert!(records
            .iter()
            .any(|r| r.event == remanence_state::AuditEvent::CancelledBeforeDispatch));
        let mut index = harness.state.index_write().unwrap();
        index
            .rebuild_from_authoritative_sources(&records, &[])
            .unwrap();
        let tape = index.get_tape(&POOL_TAPE_A).unwrap().unwrap();
        assert_eq!(tape.last_verified_edition_digest, Some(vec![7; 32]));
    }

    fn spawn_pool_open(
        state: &ApiState,
        pool_id: &str,
    ) -> tokio::task::JoinHandle<Result<pb::WriteSession, Status>> {
        let state = state.clone();
        let target = WriteSessionTarget::Pool {
            pool_id: pool_id.to_string(),
        };
        tokio::spawn(async move {
            open_write_session(remanence_state::AuditActor::System, &state, target, None).await
        })
    }

    /// Wait for the drive actor of one bay to receive a write open and return
    /// the selected tape with the reply channel, which the caller holds.
    async fn expect_open_write(
        rx: &mut mpsc::Receiver<crate::write_owner::DriveCommand>,
    ) -> (TapeUuid, oneshot::Sender<Result<pb::WriteSession, Status>>) {
        let command = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("drive actor receives the open within five seconds")
            .expect("drive command channel stays open");
        let crate::write_owner::DriveCommand::OpenWrite {
            selected, reply, ..
        } = command
        else {
            panic!("the drive actor must receive a write open");
        };
        (selected.tape_uuid, reply)
    }

    fn write_session_reply(session_id: Uuid) -> Result<pb::WriteSession, Status> {
        Ok(pb::WriteSession {
            session_id: session_id.as_bytes().to_vec(),
            ..Default::default()
        })
    }

    /// Two pool opens on different pools in flight together: while the first
    /// still holds bay 0x0101 inside its drive actor, the second takes the
    /// next idle empty bay rather than the held one.
    #[tokio::test]
    async fn concurrent_pool_opens_on_different_pools_take_different_idle_bays() {
        let mut harness = three_bay_two_pool_harness(
            vec![
                drive_bay(0x0101, false, None),
                drive_bay(0x0102, false, None),
                drive_bay(0x0103, false, None),
            ],
            vec![slot(0x0401, "FTA001L9"), slot(0x0402, "FTB001L9")],
        );
        let moves = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut changer_rx = harness.changer_rx;
        let changer = tokio::spawn({
            let moves = moves.clone();
            async move {
                while let Some(command) = changer_rx.recv().await {
                    let crate::write_owner::ChangerCommand::Move { src, dst, reply } = command
                    else {
                        panic!("pool opens only move media");
                    };
                    moves.lock().expect("moves lock").push((src, dst));
                    reply.send(Ok(())).expect("move reply receiver");
                }
            }
        });

        let open_a = spawn_pool_open(&harness.state, POOL_A);
        let (tape_a, reply_a) =
            expect_open_write(harness.drive_rxs.get_mut(&0x0101).expect("bay 0x0101")).await;
        assert_eq!(tape_a, POOL_TAPE_A);
        assert_eq!(
            harness.pool.busy_bays("LIB001"),
            HashSet::from([0x0101]),
            "the first open holds its bay while its drive actor opens"
        );

        let open_b = spawn_pool_open(&harness.state, POOL_B);
        let (tape_b, reply_b) =
            expect_open_write(harness.drive_rxs.get_mut(&0x0102).expect("bay 0x0102")).await;
        assert_eq!(tape_b, POOL_TAPE_B);
        let session_b = Uuid::new_v4();
        reply_b
            .send(write_session_reply(session_b))
            .expect("open b reply receiver");
        open_b
            .await
            .expect("open b task joins")
            .expect("open b lands on an idle bay");

        let session_a = Uuid::new_v4();
        reply_a
            .send(write_session_reply(session_a))
            .expect("open a reply receiver");
        open_a
            .await
            .expect("open a task joins")
            .expect("open a keeps its bay");

        assert_eq!(
            harness.pool.session(session_a).expect("session a").bay,
            0x0101
        );
        assert_eq!(
            harness.pool.session(session_b).expect("session b").bay,
            0x0102
        );
        assert_eq!(
            harness.pool.busy_bays("LIB001"),
            HashSet::from([0x0101, 0x0102])
        );
        assert!(
            harness
                .drive_rxs
                .get_mut(&0x0103)
                .expect("bay 0x0103")
                .try_recv()
                .is_err(),
            "the third bay stays idle"
        );
        changer.abort();
        assert_eq!(
            *moves.lock().expect("moves lock"),
            vec![(0x0401, 0x0101), (0x0402, 0x0102)]
        );
    }

    /// The field shape behind "drive bay 0x0001 is busy": a session-open
    /// readiness fence on one drive refuses another pool's open before any
    /// robot motion, although other bays are idle and empty. The refusal
    /// names the fenced drive's operation and says robot moves wait for it.
    #[tokio::test]
    async fn readiness_fence_on_one_drive_refuses_a_robot_open_on_an_idle_bay() {
        let mut fenced = drive_bay(0x0101, true, Some("FTA001L9"));
        fenced.source_slot = Some(0x0401);
        let mut harness = three_bay_two_pool_harness(
            vec![
                fenced,
                drive_bay(0x0102, false, None),
                drive_bay(0x0103, false, None),
            ],
            vec![empty_slot(0x0401), slot(0x0402, "FTB001L9")],
        );
        let operation_id = Uuid::from_u128(0xfe4ce);
        CatalogIndex::open(harness.state.index_path.as_ref())
            .expect("open writable catalog")
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id,
                run_id: None,
                endpoint: crate::drive_pool::DriveKey::new("LIB001", 0x0101),
                changer_sg: None,
                drive_sg: None,
                drive_serial: Some("DRV0101".to_string()),
                barcode: Some("FTA001L9".to_string()),
                source_slot: Some(0x0401),
                media_generation: Some(9),
                phase: "session_open_short_probe".to_string(),
                state: "becoming_ready".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record session-open readiness fence");
        CatalogIndex::open(harness.state.index_path.as_ref())
            .expect("open writable catalog")
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    operation_id,
                    &remanence_library::MediaReadiness::BecomingReady {
                        ascq: 0x02,
                        media_initializing: true,
                    },
                ),
            )
            .expect("record the fence's last sense");

        let error = spawn_pool_open(&harness.state, POOL_B)
            .await
            .expect("open b task joins")
            .expect_err("the readiness fence refuses an open that needs the robot");

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        let message = error.message();
        assert!(
            message.contains(&format!("operation={operation_id}")),
            "{message}"
        );
        assert!(message.contains("drive=0x0101"), "{message}");
        assert!(
            message.contains("robot moves in library LIB001 are refused"),
            "{message}"
        );
        assert!(
            message.contains("even when other drive bays are idle"),
            "{message}"
        );
        assert!(
            message.contains(" sense=02/04/02 (initializing command required); robot moves"),
            "{message}"
        );
        assert!(
            harness.pool.busy_bays("LIB001").is_empty(),
            "the refused open releases the idle bay it planned to use"
        );
        assert!(harness.changer_rx.try_recv().is_err(), "no robot motion");
        for rx in harness.drive_rxs.values_mut() {
            assert!(rx.try_recv().is_err(), "no drive actor command");
        }
    }

    #[tokio::test]
    async fn canonical_dismount_tail_unloads_then_moves_home() {
        let harness = dismount_harness(0);
        let parked = harness.pool.park_cartridge(harness.seated.clone());
        let responder = tokio::spawn(reply_to_dismount_commands(
            harness.drive_rx,
            harness.changer_rx,
        ));
        let reservation = harness
            .pool
            .reserve_drive(&crate::drive_pool::DriveKey::new("LIB001", 0x0101))
            .expect("reserve idle bay");
        let _reservation = dismount_reserved_cartridge(
            &harness.state,
            &harness.pool,
            &harness.seated,
            DismountReason::IdleTimeout,
            reservation,
        )
        .await
        .expect("dismount succeeds");
        responder.await.expect("mock actors join");
        assert!(!harness.pool.parked_is_current(&parked));
    }

    /// A physical unload failure prevents unsafe changer motion.
    #[tokio::test]
    async fn shutdown_physical_unload_failure_keeps_cartridge_out_of_the_changer_queue() {
        let mut harness = dismount_harness(0);
        let pool = harness.pool.clone();
        let seated = harness.seated.clone();
        let task =
            tokio::spawn(
                async move { dismount_pair(&pool, &seated, DismountReason::Shutdown).await },
            );
        let Some(crate::write_owner::DriveCommand::Unload { reply }) =
            harness.drive_rx.recv().await
        else {
            panic!("shutdown starts at the drive actor");
        };
        reply
            .send(Err(Status::unavailable("physical unload fails")))
            .unwrap();
        assert!(task.await.unwrap().is_err());
        assert!(harness.changer_rx.try_recv().is_err());
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

    /// Re-parking renews one timer even when the cartridge generation stays unchanged.
    #[tokio::test]
    async fn repark_renews_idle_deadline_without_duplicate_timer() {
        let mut harness = dismount_harness(1);
        let first = harness.pool.park_cartridge(harness.seated.clone());
        schedule_idle_dismount(harness.state.clone(), first.clone());
        tokio::time::sleep(Duration::from_millis(650)).await;
        let second = harness.pool.park_cartridge(harness.seated.clone());
        assert_eq!(first.generation, second.generation);
        schedule_idle_dismount(harness.state.clone(), second);
        assert_eq!(harness.pool.parked.lock().unwrap().idle_timers.len(), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(600), harness.drive_rx.recv())
                .await
                .is_err(),
            "the old deadline must not unload the re-parked cartridge"
        );
        tokio::time::timeout(
            Duration::from_secs(2),
            reply_to_dismount_commands(harness.drive_rx, harness.changer_rx),
        )
        .await
        .expect("renewed timer dispatches once");
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

    /// A refused changer move names the unthreaded tape and releases its bay.
    #[tokio::test]
    async fn failed_move_after_unload_names_cartridge_and_releases_reservation() {
        let mut harness = dismount_harness(0);
        let drive_key = harness.seated.drive_key();
        let reservation = harness.pool.reserve_drive(&drive_key).expect("reserve bay");
        let (state, pool, seated) = (
            harness.state.clone(),
            harness.pool.clone(),
            harness.seated.clone(),
        );
        let caller = tokio::spawn(async move {
            dismount_reserved_cartridge(
                &state,
                &pool,
                &seated,
                DismountReason::Shutdown,
                reservation,
            )
            .await
        });
        let Some(crate::write_owner::DriveCommand::Unload { reply }) =
            tokio::time::timeout(Duration::from_secs(1), harness.drive_rx.recv())
                .await
                .expect("unload deadline")
        else {
            panic!("unload command")
        };
        reply
            .send(Ok(Duration::from_millis(7)))
            .expect("unload reply");
        let Some(crate::write_owner::ChangerCommand::Move { src, dst, reply }) =
            tokio::time::timeout(Duration::from_secs(1), harness.changer_rx.recv())
                .await
                .expect("move deadline")
        else {
            panic!("move command")
        };
        assert_eq!((src, dst), (0x0101, 0x0401));
        assert!(harness.pool.reserve_drive(&drive_key).is_err());
        reply
            .send(Err(Status::failed_precondition(
                "destination slot unavailable",
            )))
            .expect("move reply");
        let error = tokio::time::timeout(Duration::from_secs(1), caller)
            .await
            .expect("dismount deadline")
            .expect("dismount joins")
            .expect_err("move fails");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        for text in [
            "OLD001L9",
            "unthreaded",
            "0x0101",
            "0x0401",
            "destination slot unavailable",
        ] {
            assert!(error.message().contains(text), "missing {text}: {error}");
        }
        drop(
            harness
                .pool
                .reserve_drive(&drive_key)
                .expect("failed move releases bay"),
        );
        assert!(
            harness.drive_rx.try_recv().is_err(),
            "no extra drive command"
        );
        assert!(
            harness.changer_rx.try_recv().is_err(),
            "no extra changer command"
        );
    }

    #[tokio::test]
    async fn dismount_does_not_start_its_unload_once_shutdown_has_begun() {
        let drive_key = crate::drive_pool::DriveKey::new("LIB001", 0x0101);
        for reason in [DismountReason::IdleTimeout, DismountReason::Evicted] {
            let mut harness = dismount_harness(0);
            let reservation = harness
                .pool
                .reserve_drive(&drive_key)
                .expect("reserve idle bay");
            harness.pool.begin_shutdown();

            let err = tokio::time::timeout(
                Duration::from_secs(1),
                dismount_reserved_cartridge(
                    &harness.state,
                    &harness.pool,
                    &harness.seated,
                    reason,
                    reservation,
                ),
            )
            .await
            .expect("shutdown guard completes within the deadline")
            .expect_err("no new dismount starts once shutdown has begun");

            assert_eq!(err.code(), tonic::Code::Unavailable, "{err}");
            assert!(
                harness.drive_rx.try_recv().is_err(),
                "{reason:?} dismount must not send SSC UNLOAD after shutdown began"
            );
            assert!(harness.changer_rx.try_recv().is_err());
            drop(
                harness
                    .pool
                    .reserve_drive_for_shutdown(&drive_key)
                    .expect("the refused dismount releases the bay for the shutdown pass"),
            );
        }
    }

    #[tokio::test]
    async fn dismount_finishes_its_move_when_the_caller_goes_away() {
        let drive_key = crate::drive_pool::DriveKey::new("LIB001", 0x0101);
        let mut harness = dismount_harness(0);
        let reservation = harness
            .pool
            .reserve_drive(&drive_key)
            .expect("reserve idle bay");
        let (state, pool, seated) = (
            harness.state.clone(),
            harness.pool.clone(),
            harness.seated.clone(),
        );
        let caller = tokio::spawn(async move {
            dismount_reserved_cartridge(
                &state,
                &pool,
                &seated,
                DismountReason::Evicted,
                reservation,
            )
            .await
        });
        let Some(crate::write_owner::DriveCommand::Unload { reply }) =
            tokio::time::timeout(Duration::from_secs(1), harness.drive_rx.recv())
                .await
                .expect("unload arrives within the deadline")
        else {
            panic!("dismount must issue drive unload first");
        };

        // The eviction's RPC goes away while the drive is unloading.
        caller.abort();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), caller)
                .await
                .expect("cancelled caller joins")
                .is_err(),
            "caller future was cancelled"
        );
        reply
            .send(Ok(Duration::from_millis(7)))
            .expect("the detached dismount still awaits the unload");

        let Some(crate::write_owner::ChangerCommand::Move { src, dst, reply }) =
            tokio::time::timeout(Duration::from_secs(1), harness.changer_rx.recv())
                .await
                .expect("move arrives within the deadline")
        else {
            panic!("the move home must follow the unload");
        };
        assert_eq!((src, dst), (0x0101, 0x0401));
        assert!(
            harness.pool.reserve_drive(&drive_key).is_err(),
            "the bay stays reserved until the move finishes"
        );
        reply.send(Ok(())).expect("move reply receiver");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(reservation) = harness.pool.reserve_drive(&drive_key) {
                    drop(reservation);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the finished dismount releases the bay");
    }

    #[tokio::test]
    async fn shutdown_waits_for_an_in_flight_dismount_to_finish_its_move() {
        let drive_key = crate::drive_pool::DriveKey::new("LIB001", 0x0101);
        let mut harness = dismount_harness(0);
        let reservation = harness
            .pool
            .reserve_drive(&drive_key)
            .expect("reserve idle bay");
        let (state, pool, seated) = (
            harness.state.clone(),
            harness.pool.clone(),
            harness.seated.clone(),
        );
        let idle = tokio::spawn(async move {
            dismount_reserved_cartridge(
                &state,
                &pool,
                &seated,
                DismountReason::IdleTimeout,
                reservation,
            )
            .await
            .map(drop)
        });
        let Some(crate::write_owner::DriveCommand::Unload { reply }) =
            harness.drive_rx.recv().await
        else {
            panic!("dismount must issue drive unload first");
        };

        // SIGTERM arrives while the drive is unloading.
        let shutdown_state = harness.state.clone();
        let shutdown = tokio::spawn(async move { shutdown_drive_pool(&shutdown_state).await });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !shutdown.is_finished(),
            "shutdown must not finish between the unload and the move"
        );

        reply
            .send(Ok(Duration::from_millis(7)))
            .expect("unload reply receiver");
        let Some(crate::write_owner::ChangerCommand::Move { src, dst, reply }) =
            harness.changer_rx.recv().await
        else {
            panic!("the move home must follow the unload");
        };
        assert_eq!((src, dst), (0x0101, 0x0401));
        // The changer actor publishes the moved inventory before it replies.
        let home = test_library(
            vec![drive_bay(0x0101, false, None)],
            vec![slot(0x0401, "OLD001L9")],
        );
        *harness
            .state
            .library_snapshot
            .as_ref()
            .expect("library snapshot")
            .write()
            .expect("snapshot lock") = std::sync::Arc::new(crate::live_status::LibrarySnapshot {
            report: remanence_library::DiscoveryReport {
                unattached_drives: Vec::new(),
                complete: true,
                incomplete_evidence: Vec::new(),
                libraries: vec![home],
                warnings: Vec::new(),
            },
            captured_at: time::OffsetDateTime::UNIX_EPOCH,
        });
        reply.send(Ok(())).expect("move reply receiver");

        idle.await
            .expect("idle dismount task joins")
            .expect("idle dismount finishes");
        tokio::time::timeout(Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown finishes once the move is done")
            .expect("shutdown task joins")
            .expect("shutdown succeeds");
        assert!(
            harness.drive_rx.try_recv().is_err(),
            "shutdown must not unload the cartridge a second time"
        );
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
            unattached_drives: Vec::new(),
            complete: true,
            incomplete_evidence: Vec::new(),
            libraries: vec![foreign, managed],
            warnings: Vec::new(),
        };

        register_startup_seated_cartridges(&state, &report);

        let parked = pool
            .parked_at(&crate::drive_pool::DriveKey::new("LIB001", 0x0101))
            .expect("managed cartridge parked");
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

    #[tokio::test]
    async fn finalize_tape_busy_response_carries_no_fabricated_progress() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let temp = tempfile::Builder::new()
            .prefix("remanence-finalize-busy-presence")
            .tempdir()
            .expect("tempdir");
        let index = CatalogIndex::open(temp.path().join("state.sqlite")).expect("open catalog");
        let mut state = ApiState::new(index);
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
        let tape_uuid = [0xB6; 16];
        let _active_object_owner = pool.reserve_tape(tape_uuid).expect("reserve exact tape");

        let response =
            <crate::CatalogService as crate::pb::catalog_server::Catalog>::finalize_tape(
                &state.catalog_service(),
                tonic::Request::new(crate::pb::FinalizeTapeRequest {
                    tape_uuid: tape_uuid.to_vec(),
                    expected_pool_id: None,
                    reason: "operator close-out while Object is active".to_string(),
                    idempotency_key: Some(crate::pb::IdempotencyKey {
                        value: Uuid::from_u128(0xB602).as_bytes().to_vec(),
                    }),
                }),
            )
            .await
            .expect("busy is a typed response")
            .into_inner();

        assert_eq!(
            response.outcome,
            crate::pb::TapeFinalizationOutcome::Busy as i32
        );
        assert_eq!(response.tape_uuid, tape_uuid.to_vec());
        // The daemon never read this tape: no operation, progress or digests.
        assert_eq!(response.operation_id, None);
        assert_eq!(response.completed_replicas, None);
        assert_eq!(response.edition_digest, None);
        assert_eq!(response.layout_digest, None);
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
            scheme: None,
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
        const SEALED_NO_COMPANION_TAPE: TapeUuid = [0xB5; 16];
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
        seed_automatic_after_replica_c(
            &index_path,
            SEALED_NO_COMPANION_TAPE,
            "HST005L9",
            pool_id,
            BLOCK_SIZE,
        );
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
            remanence_state::AuditActor::System,
            &state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: OWNED_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            None,
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
            remanence_state::AuditActor::System,
            &state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: OPEN_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            None,
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
            standalone_endpoint: None,
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
            remanence_state::AuditActor::System,
            &stale_state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: STALE_COMPANION_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            None,
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

        // Recreate the permitted uninterrupted-owner window: the sealed
        // checkpoint is durable and its companion is gone, but SQLite/audit
        // bookkeeping has not completed. A same-process pinned retry must
        // route that sealed record through host-only preflight without media.
        recover_automatic_terminal_tape(&state, SEALED_NO_COMPANION_TAPE)
            .await
            .expect("seed sealed automatic authority without a companion");
        let sealed_no_companion_journal = remanence_state::FileCheckpointJournal::open(
            state.checkpoint_journal_dir.as_path(),
            SEALED_NO_COMPANION_TAPE,
        )
        .expect("open sealed no-companion checkpoint");
        assert!(
            sealed_no_companion_journal
                .terminal_finalization_intent()
                .expect("read retired no-companion intent")
                .is_none(),
            "sealed completion must have retired its companion"
        );
        rusqlite::Connection::open(&index_path)
            .expect("open no-companion projection fixture")
            .execute(
                "update tapes set state = 'finalizing', finalization_outcome = 'in_progress' where tape_uuid = ?1",
                rusqlite::params![SEALED_NO_COMPANION_TAPE.to_vec()],
            )
            .expect("downgrade only the no-companion SQLite projection");
        let no_companion_error = open_write_session_reserved(
            remanence_state::AuditActor::System,
            &state,
            &pool,
            WriteSessionTarget::PinnedTape {
                tape_uuid: SEALED_NO_COMPANION_TAPE,
                required_pool_id: pool_id.to_string(),
            },
            None,
        )
        .await
        .expect_err("sealed no-companion retry must repair host-only and refuse Objects");
        assert_eq!(no_companion_error.code(), tonic::Code::FailedPrecondition);
        assert!(
            no_companion_error
                .message()
                .contains("host-only open recovery"),
            "{no_companion_error}"
        );
        assert_eq!(
            CatalogIndex::open(&index_path)
                .expect("reopen no-companion catalog")
                .terminal_finalization(&SEALED_NO_COMPANION_TAPE)
                .expect("read no-companion terminal result")
                .expect("no-companion terminal result")
                .outcome,
            remanence_state::TerminalFinalizationOutcome::Finalized
        );
        assert!(
            drive_rx.try_recv().is_err(),
            "sealed no-companion route reached drive actor"
        );
        assert!(
            changer_rx.try_recv().is_err(),
            "sealed no-companion route reached changer actor"
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
        for tape_uuid in [
            OPEN_TAPE,
            STARTUP_TAPE,
            STALE_COMPANION_TAPE,
            SEALED_NO_COMPANION_TAPE,
        ] {
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
            scheme: None,
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
            crate::live_status::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
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
        let readiness_id = Uuid::new_v4();
        state
            .index_write()
            .unwrap()
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: readiness_id,
                run_id: None,
                endpoint: remanence_state::DriveKey::new("LIB001", 0x0101),
                changer_sg: None,
                drive_sg: None,
                drive_serial: None,
                barcode: Some("ASY001L9".into()),
                source_slot: None,
                media_generation: Some(9),
                phase: "session_open_short_probe".into(),
                state: "becoming_ready".into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        first_reply
            .send(Err(crate::readiness_wait::ReadinessFence(readiness_id)
                .status("finalize session open: drive becomes ready")))
            .unwrap();
        crate::readiness_wait::spawn_auto_resume(state.clone());
        let wait = tokio::time::timeout(Duration::from_secs(2), drive_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let crate::write_owner::DriveCommand::WaitReady {
            handle,
            reservation,
            ..
        } = wait
        else {
            panic!("automatic readiness command");
        };
        assert_eq!(
            state
                .operations
                .latest(&accepted.operation_id)
                .unwrap()
                .progress["phase"],
            "readiness"
        );
        state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    readiness_id,
                    &remanence_library::MediaReadiness::Ready,
                ),
            )
            .unwrap();
        handle.publish_state(pb::OperationState::Succeeded, &[("state", "ready")]);
        drop(reservation);
        let crate::write_owner::DriveCommand::FinalizeTape {
            reply: first_reply, ..
        } = tokio::time::timeout(Duration::from_secs(2), drive_rx.recv())
            .await
            .unwrap()
            .unwrap()
        else {
            panic!("retry finalize open");
        };
        state.begin_shutdown();
        // Let discovery observe shutdown before the test starts another attempt.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
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
            polled.operation_id.as_deref(),
            Some(accepted.operation_id.as_bytes().as_slice())
        );
        assert_eq!(
            polled.completed_replicas,
            Some(0),
            "an accepted operation before replica A reports a present zero"
        );
        assert_eq!(polled.edition_digest.as_ref().map(Vec::len), Some(32));
        assert_eq!(polled.layout_digest.as_ref().map(Vec::len), Some(32));
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
        let failed = state
            .index()
            .unwrap()
            .get_operation(&accepted.operation_id.to_string())
            .unwrap()
            .unwrap();
        assert_eq!(failed.state, "failed");
        let summary = failed.error_summary.unwrap();
        assert!(summary.contains("same idempotency key"));
        let polled =
            <crate::CatalogService as crate::pb::catalog_server::Catalog>::get_tape_finalization(
                &state.catalog_service(),
                tonic::Request::new(pb::GetTapeFinalizationRequest {
                    tape_uuid: tape_uuid.to_vec(),
                }),
            )
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            polled.outcome,
            pb::TapeFinalizationOutcome::Finalizing as i32
        );
        assert_eq!(polled.detail, summary);
        // A fresh registry uses the durable audit projection for GetOperation.
        let reopened = ApiState::new(CatalogIndex::open(&index_path).unwrap());
        let stored = <crate::DaemonService as crate::pb::daemon_server::Daemon>::get_operation(
            &reopened.daemon_service(),
            tonic::Request::new(pb::GetOperationRequest {
                operation_id: accepted.operation_id.as_bytes().to_vec(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(stored.state, pb::OperationState::Failed as i32);
        assert_eq!(stored.error_summary, summary);
        state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    readiness_id,
                    &remanence_library::MediaReadiness::BecomingReady {
                        ascq: 1,
                        media_initializing: false,
                    },
                ),
            )
            .unwrap();
        let (_, retry_request) =
            prepare_manual_finalize_request(&state.index().unwrap(), admission.clone(), |id| {
                state.pool_config(id)
            })
            .unwrap();
        let guard = pool.reserve_tape(tape_uuid).unwrap();
        let shutdown_error = tokio::time::timeout(
            Duration::from_secs(2),
            run_manual_finalize_worker_with_budget(
                state.clone(),
                pool.clone(),
                guard,
                retry_request.clone(),
                Duration::from_secs(900),
            ),
        )
        .await
        .expect("shutdown ends the finalize attempt")
        .unwrap_err();
        assert!(shutdown_error.message().contains("shutdown"));
        assert!(shutdown_error.message().contains("same idempotency key"));
        assert_eq!(
            state
                .operations
                .latest(&accepted.operation_id)
                .unwrap()
                .state,
            pb::OperationState::Failed as i32
        );
        state
            .readiness_shutdown
            .store(false, std::sync::atomic::Ordering::Release);
        let guard = pool.reserve_tape(tape_uuid).unwrap();
        let deadline_error = run_manual_finalize_worker_with_budget(
            state.clone(),
            pool.clone(),
            guard,
            retry_request,
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(deadline_error.message().contains("deadline expired"));
        assert!(deadline_error.message().contains("same idempotency key"));
        assert_eq!(
            state
                .index()
                .unwrap()
                .get_operation(&accepted.operation_id.to_string())
                .unwrap()
                .unwrap()
                .state,
            "failed"
        );
        assert_eq!(
            state
                .index()
                .unwrap()
                .terminal_finalization(&tape_uuid)
                .unwrap()
                .unwrap()
                .outcome,
            remanence_state::TerminalFinalizationOutcome::InProgress
        );
        state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    readiness_id,
                    &remanence_library::MediaReadiness::Ready,
                ),
            )
            .unwrap();
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
        // A --wait poll immediately after acceptance sees the new attempt, before the worker runs.
        assert_eq!(
            state
                .operations
                .latest(&accepted.operation_id)
                .unwrap()
                .state,
            pb::OperationState::Running as i32
        );
        assert_eq!(
            state
                .index()
                .unwrap()
                .get_operation(&accepted.operation_id.to_string())
                .unwrap()
                .unwrap()
                .state,
            "running"
        );
        let polled = <crate::DaemonService as crate::pb::daemon_server::Daemon>::get_operation(
            &state.daemon_service(),
            tonic::Request::new(pb::GetOperationRequest {
                operation_id: accepted.operation_id.as_bytes().to_vec(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(polled.state, pb::OperationState::Running as i32);
        let retried_command = tokio::time::timeout(Duration::from_secs(1), drive_rx.recv())
            .await
            .expect("same-key retry redispatches")
            .expect("retry drive command");
        assert_eq!(
            state
                .index()
                .unwrap()
                .get_operation(&accepted.operation_id.to_string())
                .unwrap()
                .unwrap()
                .state,
            "running"
        );
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
        assert_eq!(
            state
                .index()
                .unwrap()
                .get_operation(&accepted.operation_id.to_string())
                .unwrap()
                .unwrap()
                .state,
            "running"
        );
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
        let completed_operation =
            <crate::DaemonService as crate::pb::daemon_server::Daemon>::get_operation(
                &state.daemon_service(),
                tonic::Request::new(pb::GetOperationRequest {
                    operation_id: accepted.operation_id.as_bytes().to_vec(),
                }),
            )
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            completed_operation.state,
            pb::OperationState::Succeeded as i32
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

    /// An unrelated fence does not hide a catalogued cartridge absent from inventory.
    #[test]
    fn absent_tape_precedes_library_readiness_fence() {
        let harness = dismount_harness(0);
        let tape_uuid = [0x91; 16];
        let mut index = harness.state.index_write().unwrap();
        index
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: "ABSENTL9".into(),
                block_size: 262144,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap();
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: Uuid::new_v4(),
                run_id: None,
                endpoint: remanence_state::DriveKey::new("LIB001", 0x101),
                changer_sg: None,
                drive_sg: None,
                drive_serial: None,
                barcode: Some("OLD001L9".into()),
                source_slot: Some(0x401),
                media_generation: Some(9),
                phase: "readiness_poll".into(),
                state: "becoming_ready".into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        let error = resolve_actor_mount(
            &harness.state,
            "LIB001",
            &tape_uuid,
            &HashSet::from([0x101]),
        )
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::NotFound);
        assert!(crate::readiness_wait::ReadinessFence::from_status(&error).is_none());
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
            crate::live_status::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
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

    /// A client can reuse the ready fenced bay while finalize mounts a slot tape in another bay.
    #[tokio::test]
    async fn finalize_waits_for_another_drive_fence_then_uses_free_bay() {
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
            scheme: None,
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
        let mut loaded = drive_bay(0x0101, false, None);
        loaded.source_slot = None;
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::live_status::LibrarySnapshot {
                report: remanence_library::DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
                    libraries: vec![test_library(
                        vec![drive_bay(0x0100, true, Some("OTHERL9")), loaded],
                        vec![slot(0x0401, "ASY001L9")],
                    )],
                    warnings: Vec::new(),
                },
                captured_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        ))));
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(2);
        let (changer_tx, mut changer_rx) = tokio::sync::mpsc::channel(1);
        let (foreign_tx, _foreign_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            std::collections::HashMap::from([(0x0100, foreign_tx), (0x0101, drive_tx)]),
            Arc::new(std::collections::HashMap::from([
                (0x0100, AtomicBool::new(false)),
                (0x0101, AtomicBool::new(false)),
            ])),
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

        state.drive_idle_unload_seconds = 0;
        let readiness_id = Uuid::new_v4();
        state
            .index_write()
            .unwrap()
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: readiness_id,
                run_id: None,
                endpoint: remanence_state::DriveKey::new("LIB001", 0x100),
                changer_sg: None,
                drive_sg: None,
                drive_serial: Some("DRV0100".into()),
                barcode: Some("OTHERL9".into()),
                source_slot: None,
                media_generation: Some(9),
                phase: "session_open_short_probe".into(),
                state: "becoming_ready".into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        let key = remanence_state::DriveKey::new("LIB001", 0x100);
        let readiness_reservation = pool.reserve_drive(&key).unwrap();
        pool.set_holder(&key, None, "media_readiness_wait", readiness_id);
        let result = manual_finalize_tape(&state, admission.clone())
            .await
            .unwrap();
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(accepted) = result else {
            panic!("accept")
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while !state
                .operations
                .latest(&accepted.operation_id)
                .is_some_and(|s| {
                    s.progress.get("readiness_operation_id") == Some(&readiness_id.to_string())
                })
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    readiness_id,
                    &remanence_library::MediaReadiness::Ready,
                ),
            )
            .unwrap();
        drop(readiness_reservation);
        let _client = pool.reserve_drive(&key).unwrap();
        pool.set_holder(&key, None, "read_session", Uuid::new_v4());
        let command = tokio::time::timeout(Duration::from_secs(2), changer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let crate::write_owner::ChangerCommand::Move { src, dst, reply } = command else {
            panic!("move")
        };
        assert_eq!((src, dst), (0x401, 0x101));
        reply.send(Ok(())).unwrap();
        let command = tokio::time::timeout(Duration::from_secs(1), drive_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let crate::write_owner::DriveCommand::FinalizeTape {
            request,
            needs_drive_load,
            reply,
            ..
        } = command
        else {
            panic!("finalize")
        };
        assert!(needs_drive_load);
        assert_eq!(request.candidate_operation_id, accepted.operation_id);
        reply.send(Ok(accepted)).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while state
                .operations
                .latest(&admission.candidate_operation_id)
                .unwrap()
                .state
                != pb::OperationState::Succeeded as i32
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
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
