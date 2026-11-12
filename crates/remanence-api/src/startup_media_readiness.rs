//! Startup media-readiness reconciliation and transport/status mapping.

use crate::closeout_obligation::RetryCauseExt as _;
use std::time::Duration;

use remanence_library::{
    media_readiness_sense_label, media_readiness_sense_name, MediaReadiness,
    MediaReadinessWaitEvent, MediaReadinessWaitOptions, MEDIA_READINESS_MAX_INITIALIZING_LOADS,
};
use remanence_state::{
    CatalogIndex, MediaReadinessOperationRecord, MediaReadinessTransitionInput, StateError,
};
use tonic::Status;
use uuid::Uuid;

use crate::hex_encoding::bytes_to_hex;

/// Phase of the write-ahead transition a readiness wait records before the
/// SSC LOAD it sends when the drive asks for an initializing command.
pub(crate) const INITIALIZING_LOAD_PHASE: &str = "initializing_command_load";

/// Bound on the startup poll that follows a startup initializing LOAD. A
/// drive that only needed the LOAD reports ready within seconds; one still
/// conditioning media stays fenced in a resumable state for the next wait.
const STARTUP_INITIALIZING_LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Steady poll interval of that startup poll, after the shared fast ramp.
const STARTUP_INITIALIZING_LOAD_POLL: Duration = Duration::from_secs(5);

pub(crate) fn status_from_state_error(err: impl std::borrow::Borrow<StateError>) -> Status {
    let err = err.borrow();
    match err {
        StateError::TapeBusy { operation_id } => {
            crate::drive_pool::TapeBusy(*operation_id).status()
        }
        // Another owner holds the tape-log lease: busy, so a retry can succeed.
        StateError::TapeLog(_) if err.is_tape_log_busy() => Status::unavailable(err.to_string()),
        StateError::TapeLog(cause) => {
            use remanence_state::tape_log::store::StoreError;
            let retry = match cause.as_ref() {
                StoreError::Fenced | StoreError::Ambiguous(_) | StoreError::Header => {
                    crate::closeout_obligation::RetryCause::Operator
                }
                _ => crate::closeout_obligation::RetryCause::Transient,
            };
            retry.attach(Status::failed_precondition(err.to_string()))
        }
        StateError::ConfigInvalid(_) => Status::invalid_argument(err.to_string()),
        StateError::IdempotencyConflict(_) => Status::already_exists(err.to_string()),
        StateError::TapePoolAssignmentConflict(_)
        | StateError::TapeHoldsCommittedInstances { .. }
        | StateError::TapeProvisionConflict(_)
        | StateError::JournalReplayFailed(_) => Status::failed_precondition(err.to_string()),
        _ => Status::internal(err.to_string()),
    }
}

pub(crate) fn media_readiness_conflict_status(
    action: &str,
    conflicts: &[MediaReadinessOperationRecord],
) -> Status {
    let Some(first) = conflicts.first() else {
        return Status::internal("media-readiness admission conflict called without conflicts");
    };
    let id = match parse_media_readiness_operation_id(first) {
        Ok(id) => id,
        Err(error) => return error,
    };
    crate::readiness_wait::ReadinessFence(id).status(format!(
        "{action} is blocked by active media-readiness fence {} operation={} {} barcode={} state={}{}",
        first
            .quarantine_id
            .as_deref()
            .unwrap_or("(no-quarantine-id)"),
        first.operation_id,
        match &first.endpoint {
                remanence_state::DriveKey::Bay { library_serial, bay } => format!("library={library_serial} drive=0x{bay:04x}"),
                remanence_state::DriveKey::Standalone { .. } => format!("endpoint={}",first.endpoint.label()),
            },
        first.barcode.as_deref().unwrap_or("(unknown)"),
        first.state,
        media_readiness_record_sense_suffix(first)
    ))
}

/// ` sense=KK/AA/QQ (name)` for a readiness record that carries decoded
/// sense, or an empty string. Fence messages append it after `state=`.
pub(crate) fn media_readiness_record_sense_suffix(
    record: &MediaReadinessOperationRecord,
) -> String {
    media_readiness_record_sense_label(record)
        .map(|label| format!(" sense={label}"))
        .unwrap_or_default()
}

/// `KK/AA/QQ (name)` label of the last sense a readiness record stores.
pub(crate) fn media_readiness_record_sense_label(
    record: &MediaReadinessOperationRecord,
) -> Option<String> {
    let key = media_readiness_u8_field(record.last_sense_key)?;
    let asc = media_readiness_u8_field(record.last_asc)?;
    let ascq = media_readiness_u8_field(record.last_ascq)?;
    Some(media_readiness_sense_label(key, asc, ascq))
}

/// Operation progress entries naming a readiness observation's sense, so a
/// watching client can show why the drive is not ready.
pub(crate) fn media_readiness_sense_progress(
    readiness: &MediaReadiness,
) -> Vec<(&'static str, String)> {
    let Some((key, asc, ascq)) = readiness.sense() else {
        return Vec::new();
    };
    let (sense_key, name_key) = remanence_library::MEDIA_READINESS_SENSE_PROGRESS_KEYS;
    let mut progress = vec![(sense_key, format!("{key:02x}/{asc:02x}/{ascq:02x}"))];
    if let Some(name) = media_readiness_sense_name(asc, ascq) {
        progress.push((name_key, name.to_string()));
    }
    progress
}

/// LOADs the operation may still send for an initializing-command request:
/// [`MEDIA_READINESS_MAX_INITIALIZING_LOADS`] minus the LOAD attempts it
/// recorded since it last reported ready, excluding conclusive pre-dispatch
/// shutdown refusals. Unknown outcomes continue to consume the allowance.
pub(crate) fn media_readiness_initializing_load_budget(
    index: &CatalogIndex,
    operation_id: Uuid,
) -> Result<u32, StateError> {
    Ok(MEDIA_READINESS_MAX_INITIALIZING_LOADS
        .saturating_sub(index.media_readiness_load_attempts_since_ready(operation_id)?))
}

/// Write-ahead transition recorded before the SSC LOAD a readiness wait
/// sends for an initializing-command request. `pre_ready_loading` with
/// opcode `1Bh` is the row the LOAD budget counts.
pub(crate) fn media_readiness_initializing_load_transition(
    operation_id: Uuid,
    observed: &MediaReadiness,
) -> MediaReadinessTransitionInput {
    let sense = observed.sense();
    let label = sense
        .map(|(key, asc, ascq)| media_readiness_sense_label(key, asc, ascq))
        .unwrap_or_else(|| "an initializing-command request".to_string());
    MediaReadinessTransitionInput {
        completion_evidence: None,
        operation_id,
        phase: Some(INITIALIZING_LOAD_PHASE.to_string()),
        state: "pre_ready_loading".to_string(),
        dirty_scope: Some("drive+tape".to_string()),
        last_cdb_opcode: Some(0x1b),
        last_sense_raw: None,
        last_sense_key: sense.map(|(key, _, _)| key),
        last_asc: sense.map(|(_, asc, _)| asc),
        last_ascq: sense.map(|(_, _, ascq)| ascq),
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: None,
        cancel_source: None,
        signal: None,
        evidence_path: None,
        last_error_json: Some(json_detail(
            INITIALIZING_LOAD_PHASE,
            format!("TEST UNIT READY reported {label}; sending one SSC LOAD").as_str(),
        )),
        quarantine_id: None,
    }
}

/// Transition recorded when the initializing SSC LOAD fails. The state
/// follows the failure's readiness classification, so a completion-unknown
/// or refused LOAD fences for operator release. The failure carries the LOAD's
/// own completion, which is the evidence the typed runtime requires before it
/// records the decoded sense and status beside it.
pub(crate) fn media_readiness_initializing_load_failed_transition(
    operation_id: Uuid,
    failure: &MediaReadiness,
    detail: &str,
) -> MediaReadinessTransitionInput {
    let state = media_readiness_state_name(failure).to_string();
    let quarantine_id = media_readiness_state_requires_release(state.as_str())
        .then(|| media_readiness_quarantine_id_for_operation(operation_id));
    MediaReadinessTransitionInput {
        completion_evidence: Some((&failure.outcome().evidence).into()),
        operation_id,
        phase: Some(INITIALIZING_LOAD_PHASE.to_string()),
        state,
        dirty_scope: Some("drive+tape".to_string()),
        last_cdb_opcode: Some(0x1b),
        last_sense_raw: media_readiness_sense_raw(failure),
        last_sense_key: media_readiness_sense_key(failure),
        last_asc: media_readiness_asc(failure),
        last_ascq: media_readiness_ascq(failure),
        last_host_status: None,
        last_driver_status: None,
        target_status: media_readiness_target_status(failure),
        transport_class: media_readiness_transport_class(failure),
        cancel_source: None,
        signal: None,
        evidence_path: None,
        last_error_json: Some(json_detail("initializing_command_load_failed", detail)),
        quarantine_id,
    }
}

/// Log a readiness observation that keeps a fence in place, naming the
/// drive's not-ready reason when the sense is known.
pub(crate) fn log_media_readiness_not_ready(
    operation_id: Uuid,
    endpoint: &str,
    phase: &str,
    state: &str,
    readiness: &MediaReadiness,
) {
    let (sense, sense_name) = match readiness.sense() {
        Some((key, asc, ascq)) => (
            format!("{key:02x}/{asc:02x}/{ascq:02x}"),
            media_readiness_sense_name(asc, ascq).unwrap_or("(unnamed)"),
        ),
        None => ("(none)".to_string(), "(none)"),
    };
    tracing::warn!(
        operation_id = %operation_id,
        endpoint,
        phase,
        state,
        sense = sense.as_str(),
        sense_name,
        detail = %readiness,
        "media-readiness fence remains active: drive not ready"
    );
}

/// The refusal for a write admitted on an endpoint whose own LOAD or UNLOAD the motion authority
/// calls open or Unknown. It names the endpoint and the one remedy, a daemon restart, which
/// reconciles every configured library at startup. It names no CLI command: none only
/// reconciles (`rem load`, `rem move` and `rem unload` then move media, `rem tape init` writes a
/// label). It does not promise the restart works: the reconcile reads the library's inventory
/// and can itself fail, and that error is reported as it is.
pub(crate) fn endpoint_motion_unresolved_status(
    action: &str,
    endpoint: &remanence_state::DriveKey,
) -> Status {
    Status::failed_precondition(format!(
        "{action} blocked: motion on {} is unresolved, because a LOAD or UNLOAD there has no \
         confirmed outcome; restart the daemon to reconcile library motion",
        endpoint.label()
    ))
}

/// Refuse a write on `endpoint` while the motion authority holds an open or Unknown intent for
/// that endpoint itself. Motion on another endpoint, or a library-scope MOVE, does not count:
/// motion dispatch already refuses every further MOVE, LOAD and UNLOAD while one is unresolved,
/// and the writer's readiness probe and tape-identity check guard what is already loaded.
/// `runtime` is `None` only in unit-test fixtures with no typed authority.
pub(crate) fn ensure_endpoint_motion_resolved(
    runtime: Option<&remanence_state::library_runtime::LibraryRuntime>,
    action: &str,
    endpoint: &remanence_state::DriveKey,
) -> Result<(), Status> {
    if runtime.is_some_and(|runtime| runtime.endpoint_motion_unresolved(endpoint)) {
        return Err(endpoint_motion_unresolved_status(action, endpoint));
    }
    Ok(())
}

pub(crate) fn ensure_media_readiness_admitted(
    index: &CatalogIndex,
    motion: Option<&remanence_state::library_runtime::LibraryRuntime>,
    action: &str,
    library_serial: &str,
    drive_element: Option<u16>,
    barcode: Option<&str>,
    library_robotics: bool,
) -> Result<(), Status> {
    ensure_media_readiness_admitted_inner(
        index,
        motion,
        action,
        library_serial,
        drive_element,
        barcode,
        library_robotics,
        None,
    )
}

/// Only explicit tail recovery may admit its own typed suffix fences at G1.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ensure_media_readiness_admitted_inner(
    index: &CatalogIndex,
    motion: Option<&remanence_state::library_runtime::LibraryRuntime>,
    action: &str,
    library_serial: &str,
    drive_element: Option<u16>,
    barcode: Option<&str>,
    library_robotics: bool,
    recovering_tape: Option<crate::TapeUuid>,
) -> Result<(), Status> {
    let endpoint = drive_element.map(|bay| remanence_state::DriveKey::new(library_serial, bay));
    if index
        .tape_io_is_fenced(None, barcode, endpoint.as_ref())
        .map_err(status_from_state_error)?
    {
        // Only the supersede supplies its resolved tape UUID; a mismatch fails closed by scope.
        let allowed = match recovering_tape {
            Some(tape) => crate::tape_admission::only_recoverable_suffix_fences(
                index,
                motion,
                &tape,
                barcode,
                endpoint.as_ref(),
            )
            .map_err(status_from_state_error)?,
            None => false,
        };
        if !allowed {
            return Err(Status::failed_precondition(format!(
                "{action} blocked by active tape-I/O fence; inspect rem tape quarantine list"
            )));
        }
    }
    ensure_media_readiness_unconflicted(
        index,
        motion,
        action,
        library_serial,
        drive_element,
        barcode,
        library_robotics,
    )
}

/// The readiness half of [`ensure_media_readiness_admitted`], without the tape-I/O fence check. Ending a session
/// uses it: a session on a tape fenced since it opened still closes and releases its drive. It also refuses on
/// the endpoint's own unresolved motion (`motion`), after the readiness conflicts.
pub(crate) fn ensure_media_readiness_unconflicted(
    index: &CatalogIndex,
    motion: Option<&remanence_state::library_runtime::LibraryRuntime>,
    action: &str,
    library_serial: &str,
    drive_element: Option<u16>,
    barcode: Option<&str>,
    library_robotics: bool,
) -> Result<(), Status> {
    let conflicts = index
        .media_readiness_admission_conflicts(
            library_serial,
            drive_element,
            barcode,
            library_robotics,
        )
        .map_err(status_from_state_error)?;
    let Some(first) = conflicts.first() else {
        // No readiness fence names the target. The endpoint's own unresolved motion is the
        // other thing that must stop the work, and only the motion authority knows it.
        return match drive_element {
            Some(bay) => ensure_endpoint_motion_resolved(
                motion,
                action,
                &remanence_state::DriveKey::new(library_serial, bay),
            ),
            None => Ok(()),
        };
    };
    let status = media_readiness_conflict_status(action, &conflicts);
    if library_robotics
        && !conflicts.iter().any(|record| {
            media_readiness_fence_names_target(record, library_serial, drive_element, barcode)
        })
    {
        // Every conflict is a fence on some other drive. Robot moves in the
        // library stay refused while such a fence is active, so the request
        // waits for it even when other drive bays are idle.
        return Err(crate::readiness_wait::ReadinessFence(parse_media_readiness_operation_id(first)?).status(format!(
            "{}; robot moves in library {library_serial} are refused while that fence is active, so this request needs operation {} to finish first, even when other drive bays are idle",
            status.message(),
            first.operation_id,
        )));
    }
    Err(status)
}

/// Whether a fence names the requested drive or cartridge itself, as opposed
/// to blocking the request only because the request needs the library robot.
fn media_readiness_fence_names_target(
    record: &MediaReadinessOperationRecord,
    library_serial: &str,
    drive_element: Option<u16>,
    barcode: Option<&str>,
) -> bool {
    let same_drive = drive_element
        .is_some_and(|bay| record.endpoint == remanence_state::DriveKey::new(library_serial, bay));
    let same_barcode = barcode
        .map(str::trim)
        .filter(|barcode| !barcode.is_empty())
        .is_some_and(|barcode| {
            record
                .barcode
                .as_deref()
                .is_some_and(|other| other.trim().eq_ignore_ascii_case(barcode))
        });
    same_drive || same_barcode
}

/// Reconcile active media-readiness fences against the current selected-library
/// discovery snapshot using TEST UNIT READY probes. It may send one initializing
/// SSC LOAD per operation when inventory places the cartridge in the drive.
///
/// The count returned is the number of active fences found and accounted for (probed, kept, or
/// marked unresolved), not the number of records written: an unchanged requires-release fence is
/// counted and writes nothing under the typed runtime.
pub fn reconcile_media_readiness_on_startup(
    index: &mut CatalogIndex,
    report: &remanence_library::DiscoveryReport,
    policy: &remanence_library::StaticAllowlist,
) -> Result<usize, Status> {
    use remanence_library::AccessPolicy as _;

    let mut found = 0usize;
    for library in &report.libraries {
        if !policy.allows(library.serial.as_str()) {
            continue;
        }
        let active = index
            .list_active_media_readiness_operations(Some(library.serial.as_str()))
            .map_err(status_from_state_error)?;
        let unresolved_motion = index.library_runtime().is_some_and(|runtime| {
            remanence_library::motion::MotionAuthority::requires_reconciliation(
                &runtime,
                &remanence_library::motion::MotionCommand::Move {
                    library_serial: library.serial.clone(),
                    source: 0,
                    destination: 0,
                },
            )
        });
        if active.is_empty() && !unresolved_motion {
            continue;
        }
        let mut handle = library
            .open(policy)
            .map_err(|err| Status::internal(format!("startup readiness open library: {err}")))?;
        if let Some(authority) = index.library_runtime() {
            handle.set_motion_authority(std::sync::Arc::new(authority));
        }
        handle.reconcile_startup_motion().map_err(|e| {
            Status::failed_precondition(format!("startup motion reconciliation: {e}"))
        })?;
        let mut opened_drives = Vec::new();
        for bay in &library.drive_bays {
            let Some(installed) = bay.installed.as_ref() else {
                continue;
            };
            if installed.sg_path.is_none() {
                continue;
            }
            let bay_addr = bay.element_address;
            let drive = handle.open_drive(bay_addr, policy).map_err(|err| {
                Status::internal(format!(
                    "startup readiness open drive 0x{bay_addr:04x}: {err}"
                ))
            })?;
            opened_drives.push((bay_addr, drive));
        }
        found = found.saturating_add(reconcile_library_media_readiness_records_on_startup(
            index,
            library,
            &mut opened_drives,
            active,
        )?);
    }
    Ok(found)
}

/// One library's startup reconcile. Returns the number of active fences found, as
/// [`reconcile_media_readiness_on_startup`] does, not the number of records written.
pub(crate) fn reconcile_library_media_readiness_on_startup(
    index: &mut CatalogIndex,
    library: &remanence_library::Library,
    opened_drives: &mut [(u16, remanence_library::DriveHandle)],
) -> Result<usize, Status> {
    let active = index
        .list_active_media_readiness_operations(Some(library.serial.as_str()))
        .map_err(status_from_state_error)?;
    reconcile_library_media_readiness_records_on_startup(index, library, opened_drives, active)
}

pub(crate) fn reconcile_library_media_readiness_records_on_startup(
    index: &mut CatalogIndex,
    library: &remanence_library::Library,
    opened_drives: &mut [(u16, remanence_library::DriveHandle)],
    records: Vec<MediaReadinessOperationRecord>,
) -> Result<usize, Status> {
    let mut found = 0usize;
    for record in records {
        if index
            .tape_io_is_fenced(None, record.barcode.as_deref(), Some(&record.endpoint))
            .map_err(status_from_state_error)?
        {
            continue;
        }
        let operation_id = parse_media_readiness_operation_id(&record)?;
        match startup_media_readiness_probe_plan(&record, library, operation_id) {
            StartupReadinessPlan::Probe {
                operation_id,
                drive_element,
                family,
            } => {
                let Some((_, drive)) = opened_drives
                    .iter_mut()
                    .find(|(bay, _)| *bay == drive_element)
                else {
                    let transition = startup_media_readiness_unresolved_transition(
                        operation_id,
                        format!(
                            "drive 0x{drive_element:04x} is not openable in selected library {}",
                            library.serial
                        ),
                    );
                    index
                        .record_media_readiness_transition(transition)
                        .map_err(status_from_state_error)?;
                    found = found.saturating_add(1);
                    continue;
                };
                let observed = drive.probe_media_readiness(family);
                let load_budget = if observed.requires_initializing_command() {
                    media_readiness_initializing_load_budget(index, operation_id)
                        .map_err(status_from_state_error)?
                } else {
                    0
                };
                let readiness = if load_budget > 0 {
                    startup_media_readiness_initializing_load(
                        index,
                        drive,
                        operation_id,
                        family,
                        observed,
                        load_budget,
                    )?
                } else {
                    let mut transition =
                        startup_media_readiness_probe_transition(operation_id, &observed);
                    if record.cancel_source.as_deref() == Some("operator")
                        && crate::readiness_wait::is_resumable(&transition.state)
                    {
                        transition.cancel_source = Some("operator".into());
                    }
                    index
                        .record_media_readiness_transition(transition)
                        .map_err(status_from_state_error)?;
                    observed
                };
                if !readiness.is_ready() {
                    log_media_readiness_not_ready(
                        operation_id,
                        format!("library={} drive=0x{drive_element:04x}", library.serial).as_str(),
                        "startup_reconcile_tur",
                        media_readiness_state_name(&readiness),
                        &readiness,
                    );
                }
                found = found.saturating_add(1);
            }
            StartupReadinessPlan::KeepFenced { transition } => {
                // Under the typed runtime a requires-release fence is already durable and
                // nothing has changed it: startup deliberately does not probe one, so
                // re-recording it would write a non-observation, which the runtime refuses
                // (only a release may follow a fence). It is found and counted, and nothing is
                // appended. A record startup cannot reverify still records its unresolved
                // transition, since that changes the state.
                let unchanged_fence = index.library_runtime().is_some()
                    && media_readiness_state_requires_release(record.state.trim());
                if !unchanged_fence {
                    index
                        .record_media_readiness_transition(*transition)
                        .map_err(status_from_state_error)?;
                }
                found = found.saturating_add(1);
            }
        }
    }
    if found > 0 {
        tracing::info!(
            library_serial = library.serial.as_str(),
            fences_found = found,
            "found active media-readiness fence(s) during startup; an unchanged requires-release fence is already durable and writes no record"
        );
    }
    Ok(found)
}

/// Send the startup reconcile's one SSC LOAD to a drive that asks for an
/// initializing command, then poll briefly.
///
/// The probe plan has already matched the recorded drive serial and barcode
/// against changer inventory, so the cartridge is physically in this drive.
/// A daemon stopped between SSC UNLOAD and the changer move leaves it there
/// unthreaded, and the drive answers `02/04/02` until a LOAD arrives; a TUR
/// poll alone would keep the fence for the whole readiness timeout. Each
/// observation is recorded as a startup transition in its readiness state
/// (never `timeout_unknown`), so a drive still conditioning media after the
/// short startup poll stays resumable.
fn startup_media_readiness_initializing_load(
    index: &mut CatalogIndex,
    drive: &mut remanence_library::DriveHandle,
    operation_id: Uuid,
    family: remanence_library::MediaFamily,
    observed: MediaReadiness,
    load_budget: u32,
) -> Result<MediaReadiness, Status> {
    let cancel_source = index
        .media_readiness_operation(operation_id)
        .map_err(status_from_state_error)?
        .and_then(|record| record.cancel_source)
        .filter(|source| source == "operator");
    let poll = drive.with_readiness_operation(*operation_id.as_bytes(), |drive| drive
        .wait_for_media_readiness_with_initializing_load(
            family,
            Some(observed),
            MediaReadinessWaitOptions {
                wait: true,
                timeout: STARTUP_INITIALIZING_LOAD_TIMEOUT,
                poll_interval: STARTUP_INITIALIZING_LOAD_POLL,
            },
            load_budget,
            || None,
            |event| {
                let mut transition = match event {
                    MediaReadinessWaitEvent::Poll(poll) => {
                        startup_media_readiness_probe_transition(operation_id, &poll.readiness)
                    }
                    MediaReadinessWaitEvent::InitializingLoad(poll) => {
                        if index.library_runtime().is_some() { return Ok(()); }

                        tracing::warn!(
                            operation_id = %operation_id,
                            detail = %poll.readiness,
                            "startup readiness: drive asks for an initializing command; sending one SSC LOAD"
                        );
                        media_readiness_initializing_load_transition(operation_id, &poll.readiness)
                    }
                    MediaReadinessWaitEvent::InitializingLoadFailed { readiness, detail } => {
                        tracing::warn!(
                            operation_id = %operation_id,
                            detail,
                            "startup readiness: initializing SSC LOAD failed"
                        );
                        media_readiness_initializing_load_failed_transition(
                            operation_id,
                            readiness,
                            detail,
                        )
                    }
                    MediaReadinessWaitEvent::Cancelled(_) => return Ok(()),
                };
                if crate::readiness_wait::is_resumable(&transition.state) {
                    transition.cancel_source = cancel_source.clone();
                } else if media_readiness_state_requires_release(&transition.state) {
                    transition.cancel_source = Some("startup_reconcile".into());
                }
                index
                    .record_media_readiness_transition(transition)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            },
        ))
        .map_err(|error| {
            Status::internal(format!(
                "startup readiness LOAD for operation {operation_id}: {error}"
            ))
        })?;
    Ok(poll.readiness)
}

pub(crate) enum StartupReadinessPlan {
    Probe {
        operation_id: Uuid,
        drive_element: u16,
        family: remanence_library::MediaFamily,
    },
    KeepFenced {
        transition: Box<MediaReadinessTransitionInput>,
    },
}

pub(crate) fn startup_media_readiness_probe_plan(
    record: &MediaReadinessOperationRecord,
    library: &remanence_library::Library,
    operation_id: Uuid,
) -> StartupReadinessPlan {
    if media_readiness_state_requires_release(record.state.trim()) {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_requires_release_transition(
                record,
                operation_id,
            )),
        };
    }
    let Some(drive_element) = record
        .endpoint
        .bay()
        .filter(|_| record.endpoint.library_serial() == Some(library.serial.as_str()))
    else {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                format!(
                    "recorded endpoint {} is not a bay in this library",
                    record.endpoint.label()
                ),
            )),
        };
    };
    let Some(bay) = library
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == drive_element)
    else {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                format!(
                    "recorded drive 0x{drive_element:04x} is absent from selected library {}",
                    library.serial
                ),
            )),
        };
    };
    let Some(expected_drive_serial) = record.drive_serial.as_deref() else {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                "record has no drive serial to reverify".to_string(),
            )),
        };
    };
    let actual_drive_serial = bay.installed.as_ref().map(|drive| drive.serial.as_str());
    if actual_drive_serial != Some(expected_drive_serial) {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                format!(
                    "expected drive serial {expected_drive_serial}, selected-library snapshot has {}",
                    actual_drive_serial.unwrap_or("(none)")
                ),
            )),
        };
    }
    let Some(expected_barcode) = record
        .barcode
        .as_deref()
        .map(str::trim)
        .filter(|barcode| !barcode.is_empty())
    else {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                "record has no barcode to reverify".to_string(),
            )),
        };
    };
    if !bay
        .loaded_tape
        .as_deref()
        .is_some_and(|actual| actual.trim().eq_ignore_ascii_case(expected_barcode))
    {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                format!(
                    "expected barcode {expected_barcode} in drive 0x{drive_element:04x}, selected-library snapshot has {}",
                    bay.loaded_tape.as_deref().unwrap_or("(none)")
                ),
            )),
        };
    }
    StartupReadinessPlan::Probe {
        operation_id,
        drive_element,
        family: media_readiness_family_from_record(record),
    }
}

pub(crate) fn parse_media_readiness_operation_id(
    record: &MediaReadinessOperationRecord,
) -> Result<Uuid, Status> {
    Uuid::parse_str(record.operation_id.as_str()).map_err(|err| {
        Status::internal(format!(
            "media-readiness operation id {} is invalid: {err}",
            record.operation_id
        ))
    })
}

pub(crate) fn media_readiness_family_from_record(
    record: &MediaReadinessOperationRecord,
) -> remanence_library::MediaFamily {
    if record
        .media_generation
        .is_some_and(|generation| generation >= 9)
    {
        remanence_library::MediaFamily::Lto9OrLater
    } else {
        remanence_library::MediaFamily::Unknown
    }
}

pub(crate) fn startup_media_readiness_probe_transition(
    operation_id: Uuid,
    readiness: &remanence_library::MediaReadiness,
) -> MediaReadinessTransitionInput {
    let state = media_readiness_state_name(readiness).to_string();
    let quarantine_id = media_readiness_state_requires_release(state.as_str())
        .then(|| media_readiness_quarantine_id_for_operation(operation_id));
    MediaReadinessTransitionInput {
        completion_evidence: Some((&readiness.outcome().evidence).into()),
        operation_id,
        phase: Some("startup_reconcile_tur".to_string()),
        state,
        dirty_scope: Some(if readiness.is_ready() {
            "none".to_string()
        } else {
            "drive+tape".to_string()
        }),
        last_cdb_opcode: Some(0x00),
        last_sense_raw: media_readiness_sense_raw(readiness),
        last_sense_key: media_readiness_sense_key(readiness),
        last_asc: media_readiness_asc(readiness),
        last_ascq: media_readiness_ascq(readiness),
        last_host_status: None,
        last_driver_status: None,
        target_status: media_readiness_target_status(readiness),
        transport_class: media_readiness_transport_class(readiness),
        cancel_source: quarantine_id
            .as_ref()
            .map(|_| "startup_reconcile".to_string()),
        signal: None,
        evidence_path: None,
        last_error_json: media_readiness_error_json(readiness),
        quarantine_id,
    }
}

pub(crate) fn startup_media_readiness_unresolved_transition(
    operation_id: Uuid,
    detail: String,
) -> MediaReadinessTransitionInput {
    MediaReadinessTransitionInput {
        completion_evidence: None,
        operation_id,
        phase: Some("startup_reconcile".to_string()),
        state: "aborted_unknown".to_string(),
        dirty_scope: Some("selected-library-snapshot".to_string()),
        last_cdb_opcode: None,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: Some("unknown".to_string()),
        cancel_source: Some("startup_reconcile".to_string()),
        signal: None,
        evidence_path: None,
        last_error_json: Some(json_detail("startup_reconcile_unresolved", detail.as_str())),
        quarantine_id: Some(media_readiness_quarantine_id_for_operation(operation_id)),
    }
}

pub(crate) fn startup_media_readiness_requires_release_transition(
    record: &MediaReadinessOperationRecord,
    operation_id: Uuid,
) -> MediaReadinessTransitionInput {
    let state = record.state.trim();
    MediaReadinessTransitionInput {
        completion_evidence: None,
        operation_id,
        phase: Some("startup_reconcile_requires_release".to_string()),
        state: state.to_string(),
        dirty_scope: Some(
            record
                .dirty_scope
                .as_deref()
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .unwrap_or("drive+tape")
                .to_string(),
        ),
        last_cdb_opcode: None,
        last_sense_raw: record.last_sense_raw.clone(),
        last_sense_key: media_readiness_u8_field(record.last_sense_key),
        last_asc: media_readiness_u8_field(record.last_asc),
        last_ascq: media_readiness_u8_field(record.last_ascq),
        last_host_status: record.last_host_status,
        last_driver_status: record.last_driver_status,
        target_status: media_readiness_u8_field(record.target_status),
        transport_class: record.transport_class.clone(),
        cancel_source: Some("startup_reconcile".to_string()),
        signal: None,
        evidence_path: record.evidence_path.clone(),
        last_error_json: Some(json_detail(
            "startup_reconcile_requires_release",
            format!("state {state} requires operator release before startup TUR").as_str(),
        )),
        quarantine_id: record
            .quarantine_id
            .clone()
            .or_else(|| Some(media_readiness_quarantine_id_for_operation(operation_id))),
    }
}

/// An operator's cancel of a readiness wait, as an open observation. A cancel is a pause, not an
/// end: the row keeps its state, scope and quarantine, takes phase `cancelled` and `cancel_source`
/// `operator`, and an explicit resume continues it with its retained LOAD budget. It issues no
/// command, so it carries no completion evidence and none of the evidence-derived columns (sense,
/// statuses, transport class): the row's sense reads empty until the next poll, and every reader
/// of it is display only. The daemon's wait and the cancel RPC both build it through here, so
/// whichever records second is an exact retry of the first, but only when no poll lands between
/// the two records: the RPC reads the row before it appends, so a poll in between can leave its
/// cancel naming the older waiting state (display only; no deciding reader tells the six waiting
/// states apart).
pub(crate) fn operator_cancel_readiness_transition(
    record: &MediaReadinessOperationRecord,
    operation_id: Uuid,
    cancelled_by: Option<&str>,
) -> MediaReadinessTransitionInput {
    let detail = match cancelled_by {
        Some(actor) => {
            format!("operator {actor} cancels readiness wait; explicit resume continues it")
        }
        None => "operator cancels readiness wait; explicit resume continues it".to_string(),
    };
    MediaReadinessTransitionInput {
        completion_evidence: None,
        operation_id,
        phase: Some("cancelled".to_string()),
        state: record.state.trim().to_string(),
        dirty_scope: Some(
            record
                .dirty_scope
                .as_deref()
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .unwrap_or("drive+tape")
                .to_string(),
        ),
        last_cdb_opcode: None,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: None,
        cancel_source: Some("operator".to_string()),
        signal: None,
        evidence_path: None,
        last_error_json: Some(json_detail("cancelled", detail.as_str())),
        quarantine_id: record.quarantine_id.clone(),
    }
}

pub(crate) fn media_readiness_u8_field(value: Option<i64>) -> Option<u8> {
    value.and_then(|value| u8::try_from(value).ok())
}

pub(crate) fn media_readiness_state_name(
    readiness: &remanence_library::MediaReadiness,
) -> &'static str {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::Ready,
            ..
        } => "ready",
        remanence_library::MediaReadiness {
            kind:
                remanence_library::MediaReadinessKind::BecomingReady {
                    media_initializing: true,
                    ..
                },
            ..
        } => "media_initializing",
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::BecomingReady { .. },
            ..
        } => "becoming_ready",
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UnitAttention { .. },
            ..
        } => "unit_attention",
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TargetBusy { .. },
            ..
        } => "target_busy",
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::ReservationConflict,
            ..
        } => "reservation_conflict",
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TransportUnknown { .. },
            ..
        } => "transport_unknown",
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::NoMedium { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::RepeatedUnitAttention { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TerminalNotReady { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::CheckCondition { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UndecodedCheckCondition { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TaskAborted,
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UnexpectedStatus { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::InvalidRequest { .. },
            ..
        } => "terminal_error",
    }
}

pub(crate) fn media_readiness_state_requires_release(state: &str) -> bool {
    matches!(
        state,
        "aborted_unknown"
            | "timeout_unknown"
            | "transport_unknown"
            | "terminal_error"
            | "reservation_conflict"
    )
}

pub(crate) fn media_readiness_quarantine_id_for_operation(operation_id: Uuid) -> String {
    format!("mrq-{operation_id}")
}

pub(crate) fn media_readiness_sense_key(
    readiness: &remanence_library::MediaReadiness,
) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::BecomingReady { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TerminalNotReady { .. },
            ..
        } => Some(0x02),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::NoMedium { .. },
            ..
        } => Some(0x02),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UnitAttention { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::RepeatedUnitAttention { .. },
            ..
        } => Some(0x06),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::CheckCondition { key, .. },
            ..
        } => Some(*key),
        _ => None,
    }
}

pub(crate) fn media_readiness_asc(readiness: &remanence_library::MediaReadiness) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::BecomingReady { .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TerminalNotReady { .. },
            ..
        } => Some(0x04),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::NoMedium { .. },
            ..
        } => Some(0x3a),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UnitAttention { asc, .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::RepeatedUnitAttention { asc, .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::CheckCondition { asc, .. },
            ..
        } => Some(*asc),
        _ => None,
    }
}

pub(crate) fn media_readiness_ascq(readiness: &remanence_library::MediaReadiness) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::BecomingReady { ascq, .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::NoMedium { ascq },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TerminalNotReady { ascq, .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UnitAttention { ascq, .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::RepeatedUnitAttention { ascq, .. },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::CheckCondition { ascq, .. },
            ..
        } => Some(*ascq),
        _ => None,
    }
}

pub(crate) fn media_readiness_target_status(
    readiness: &remanence_library::MediaReadiness,
) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TargetBusy { status },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UnexpectedStatus { status },
            ..
        } => Some(*status),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::ReservationConflict,
            ..
        } => Some(0x18),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TaskAborted,
            ..
        } => Some(0x40),
        _ => None,
    }
}

pub(crate) fn media_readiness_transport_class(
    readiness: &remanence_library::MediaReadiness,
) -> Option<String> {
    matches!(
        readiness,
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TransportUnknown { .. },
            ..
        }
    )
    .then(|| "unknown".to_string())
}

pub(crate) fn media_readiness_sense_raw(
    readiness: &remanence_library::MediaReadiness,
) -> Option<String> {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UndecodedCheckCondition { sense },
            ..
        } => Some(bytes_to_hex(sense)),
        _ => None,
    }
}

pub(crate) fn media_readiness_error_json(
    readiness: &remanence_library::MediaReadiness,
) -> Option<String> {
    match readiness {
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::RepeatedUnitAttention { .. },
            ..
        } => Some(json_detail(
            "repeated_unit_attention",
            "unit attention repeated during startup readiness reconciliation",
        )),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TerminalNotReady { action, .. },
            ..
        } => Some(json_detail("terminal_not_ready", action)),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::TransportUnknown { detail },
            ..
        }
        | remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::InvalidRequest { detail },
            ..
        } => Some(json_detail("startup_reconcile_error", detail)),
        remanence_library::MediaReadiness {
            kind: remanence_library::MediaReadinessKind::UndecodedCheckCondition { .. },
            ..
        } => Some(json_detail(
            "undecoded_check_condition",
            "TEST UNIT READY returned undecoded sense data",
        )),
        _ => None,
    }
}

pub(crate) fn json_detail(kind: &str, detail: &str) -> String {
    format!(
        "{{\"kind\":\"{}\",\"detail\":\"{}\"}}",
        json_escape(kind),
        json_escape(detail)
    )
}

pub(crate) fn json_escape(value: &str) -> String {
    let mut escaped = String::new();
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
