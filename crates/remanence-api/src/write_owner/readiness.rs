//! Session-open media-readiness probing, durable fencing, and transition evidence.

use std::time::Duration as StdDuration;

use remanence_library::{
    DriveHandle, MediaFamily, MediaReadiness, MediaReadinessPoll, MediaReadinessWaitEvent,
    MediaReadinessWaitOptions,
};
use remanence_state::CatalogIndex;
use tonic::Status;
use uuid::Uuid;

use super::SessionOpenReadinessContext;
use crate::{pb, status_from_state_error};

#[cfg(test)]
pub(crate) const SESSION_OPEN_CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_millis(0);
#[cfg(not(test))]
pub(crate) const SESSION_OPEN_CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_secs(1);

pub(crate) fn session_open_short_probe_or_load(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    ctx: SessionOpenReadinessContext<'_>,
) -> Result<(), Status> {
    session_open_reject_admission_conflicts(index, &ctx)?;
    let family = session_open_media_family(
        index
            .readiness_media_observation(ctx.barcode)
            .map_err(status_from_state_error)?,
    );
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

/// Run a readiness wait without a pool reservation in transport tests.
#[cfg(test)]
pub(crate) fn handle_drive_wait_ready(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    request: DriveReadinessPoll<'_>,
    handle: &crate::operations::OperationHandle,
) {
    handle_drive_wait_ready_and_release(index, drive, request, handle, None);
}

/// Release drive ownership before terminal progress makes an open retry eligible.
pub(crate) fn handle_drive_wait_ready_and_release(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    request: DriveReadinessPoll<'_>,
    handle: &crate::operations::OperationHandle,
    reservation: Option<super::DriveReservation>,
) {
    let bay = request.bay;
    handle.publish_state(
        pb::OperationState::Running,
        &[("phase", "readiness_poll"), ("state", "starting")],
    );
    let result = poll_drive_media_readiness(index, drive, request, handle);
    drop(reservation);

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
        Err(error) if error.contains(crate::readiness_wait::SHUTDOWN_REASON) => {
            publish_readiness_shutdown(index, handle, &error);
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
    handle.wake_readiness();
}

/// A shutdown end reports the last durable state without changing the fence.
pub(crate) fn publish_readiness_shutdown(
    index: &CatalogIndex,
    handle: &crate::operations::OperationHandle,
    error: &str,
) {
    let state = index
        .media_readiness_operation(handle.op_id_uuid())
        .ok()
        .flatten()
        .map(|record| record.state)
        .unwrap_or_else(|| "unknown".into());
    handle.publish(crate::operations::status_with_error(
        handle.op_id_uuid(),
        handle.operation_kind(),
        pb::OperationState::Cancelled,
        std::collections::HashMap::from([
            ("phase".into(), "shutdown".into()),
            ("state".into(), state),
        ]),
        error,
    ));
}

/// Durable state and sense triple of the last not-ready observation a wait
/// logged; the wait logs again only when either changes.
type LoggedReadiness = (&'static str, Option<(u8, u8, u8)>);

/// One drive-actor readiness wait: which operation it serves and whether
/// changer inventory shows the cartridge physically in the drive.
pub(crate) struct DriveReadinessPoll<'a> {
    pub(crate) bay: u16,
    pub(crate) operation_id: Uuid,
    pub(crate) family: MediaFamily,
    pub(crate) options: MediaReadinessWaitOptions,
    /// Changer inventory shows a cartridge in this drive. Only then may the
    /// wait send its SSC LOAD for an initializing-command request.
    pub(crate) cartridge_in_drive: bool,
    pub(crate) phase: &'a str,
}

/// Poll a drive's media readiness for a durable operation, recording every
/// observation, and send at most one SSC LOAD per operation when the drive
/// answers `NOT READY / INITIALIZING COMMAND REQUIRED` (`02/04/02`) for a
/// cartridge changer inventory places in the drive.
///
/// The LOAD budget comes from the operation's durable history, so a resumed
/// wait (a client's `ResumeMediaReadiness` after startup reconciliation, or
/// a second resume) does not repeat a LOAD the operation already sent; a
/// `ready` observation re-arms it. The attempt is recorded as a
/// `pre_ready_loading` transition before the LOAD reaches the drive.
pub(crate) fn poll_drive_media_readiness(
    index: &mut CatalogIndex,
    drive: &mut DriveHandle,
    request: DriveReadinessPoll<'_>,
    handle: &crate::operations::OperationHandle,
) -> Result<MediaReadinessPoll, String> {
    let DriveReadinessPoll {
        bay,
        operation_id,
        family,
        options,
        cartridge_in_drive,
        phase,
    } = request;
    let load_budget = if cartridge_in_drive {
        crate::startup_media_readiness::media_readiness_initializing_load_budget(
            index,
            operation_id,
        )
        .map_err(|error| format!("count media readiness LOAD attempts: {error}"))?
    } else {
        0
    };
    let endpoint = drive
        .library_serial()
        .map(|serial| format!("library={serial} drive=0x{bay:04x}"))
        .unwrap_or_else(|| format!("drive=0x{bay:04x}"));
    let mut last_logged: Option<LoggedReadiness> = None;
    drive.wait_for_media_readiness_with_initializing_load(
        family,
        None,
        options,
        load_budget,
        || {
            if handle.is_shutting_down() {
                Some(crate::readiness_wait::SHUTDOWN_REASON.to_string())
            } else {
                handle.is_cancelled().then(|| "operator cancels readiness wait".to_string())
            }
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
                let observed = (state, poll.readiness.sense());
                if !poll.readiness.is_ready() && last_logged != Some(observed) {
                    crate::startup_media_readiness::log_media_readiness_not_ready(
                        operation_id,
                        endpoint.as_str(),
                        phase,
                        state,
                        &poll.readiness,
                    );
                }
                last_logged = Some(observed);
                let sense = crate::startup_media_readiness::media_readiness_sense_progress(
                    &poll.readiness,
                );
                let mut progress = vec![
                    ("phase", "readiness_poll"),
                    ("state", state),
                    ("attempts", attempts.as_str()),
                    ("elapsed_seconds", elapsed_seconds.as_str()),
                ];
                progress.extend(sense.iter().map(|(key, value)| (*key, value.as_str())));
                handle.publish_state(pb::OperationState::Running, &progress);
                Ok(())
            }
            MediaReadinessWaitEvent::InitializingLoad(poll) => {
                index
                    .record_media_readiness_transition(
                        crate::startup_media_readiness::media_readiness_initializing_load_transition(
                            operation_id,
                            &poll.readiness,
                        ),
                    )
                    .map_err(|error| format!("record media readiness LOAD attempt: {error}"))?;
                tracing::warn!(
                    operation_id = %operation_id,
                    endpoint = endpoint.as_str(),
                    phase,
                    detail = %poll.readiness,
                    "drive asks for an initializing command; sending one SSC LOAD"
                );
                let sense = crate::startup_media_readiness::media_readiness_sense_progress(
                    &poll.readiness,
                );
                let mut progress = vec![
                    ("phase", crate::startup_media_readiness::INITIALIZING_LOAD_PHASE),
                    ("state", "pre_ready_loading"),
                ];
                progress.extend(sense.iter().map(|(key, value)| (*key, value.as_str())));
                handle.publish_state(pb::OperationState::Running, &progress);
                Ok(())
            }
            MediaReadinessWaitEvent::InitializingLoadFailed { readiness, detail } => {
                index
                    .record_media_readiness_transition(
                        crate::startup_media_readiness::media_readiness_initializing_load_failed_transition(
                            operation_id,
                            readiness,
                            detail,
                        ),
                    )
                    .map_err(|error| format!("record media readiness LOAD failure: {error}"))?;
                tracing::warn!(
                    operation_id = %operation_id,
                    endpoint = endpoint.as_str(),
                    phase,
                    detail,
                    "initializing SSC LOAD failed"
                );
                Ok(())
            }
            MediaReadinessWaitEvent::Cancelled(reason) => {
                if reason == crate::readiness_wait::SHUTDOWN_REASON { return Ok(()); }
                let record = index.media_readiness_operation(operation_id).map_err(|e| e.to_string())?.ok_or("readiness operation missing")?;
                let mut transition = crate::startup_media_readiness::startup_media_readiness_requires_release_transition(&record, operation_id);
                transition.phase = Some("cancelled".into());
                transition.cancel_source = Some("operator".into());
                transition.last_error_json = Some(session_open_json_detail("cancelled", "operator cancels readiness wait; explicit resume continues it"));
                transition.quarantine_id = record.quarantine_id;
                index.record_media_readiness_transition(transition).map(|_| ()).map_err(|e| e.to_string())
            },
        },
    )
}

pub(crate) fn session_open_reject_admission_conflicts(
    index: &mut CatalogIndex,
    ctx: &SessionOpenReadinessContext<'_>,
) -> Result<(), Status> {
    let conflicts = index
        .media_readiness_admission_conflicts(ctx.library_serial, Some(ctx.bay), ctx.barcode, false)
        .map_err(status_from_state_error)?;
    if conflicts.is_empty() {
        return Ok(());
    }
    Err(crate::readiness_wait::ReadinessFence(
        crate::startup_media_readiness::parse_media_readiness_operation_id(&conflicts[0])?,
    )
    .status(session_open_admission_error_message(ctx, &conflicts)))
}

pub(crate) fn session_open_admission_error_message(
    ctx: &SessionOpenReadinessContext<'_>,
    conflicts: &[remanence_state::MediaReadinessOperationRecord],
) -> String {
    let conflict_summary = conflicts
        .iter()
        .map(|record| {
            format!(
                "operation={} state={}{} {} barcode={} quarantine={}",
                record.operation_id,
                record.state,
                crate::startup_media_readiness::media_readiness_record_sense_suffix(record),
                match &record.endpoint {
                    remanence_state::DriveKey::Bay { bay, .. } => format!("drive=0x{bay:04x}"),
                    remanence_state::DriveKey::Standalone { .. } =>
                        format!("endpoint={}", record.endpoint.label()),
                },
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

pub(crate) fn session_open_immediate_load_then_probe(
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

pub(crate) fn session_open_short_probe_after_load(
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

pub(crate) fn session_open_media_family(
    observation: Option<crate::MediaObservation>,
) -> MediaFamily {
    if observation
        .map(|observation| observation.generation)
        .is_some_and(|generation| generation.generation_number() >= 9)
    {
        MediaFamily::Lto9OrLater
    } else {
        MediaFamily::Unknown
    }
}

pub(crate) fn session_open_readiness_requires_immediate_load(
    ctx: &SessionOpenReadinessContext<'_>,
    readiness: &MediaReadiness,
) -> bool {
    match readiness {
        MediaReadiness::NoMedium { .. } => ctx.needs_drive_load,
        readiness => readiness.requires_initializing_command(),
    }
}

pub(crate) fn session_open_readiness_should_retry_once(readiness: &MediaReadiness) -> bool {
    matches!(
        readiness,
        MediaReadiness::UnitAttention { .. } | MediaReadiness::TargetBusy { .. }
    )
}

pub(crate) fn record_session_open_command_fence_on_operation(
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
    crate::readiness_wait::ReadinessFence(operation_id).status(
        session_open_readiness_error_message(
            ctx,
            operation_id,
            "transport_unknown",
            detail.as_str(),
        ),
    )
}

pub(crate) fn record_session_open_mechanical_transition(
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

pub(crate) fn record_session_open_readiness_fence(
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

pub(crate) fn record_session_open_readiness_fence_on_operation(
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
    crate::startup_media_readiness::log_media_readiness_not_ready(
        operation_id,
        format!("library={} drive=0x{:04x}", ctx.library_serial, ctx.bay).as_str(),
        phase,
        state,
        readiness,
    );
    crate::readiness_wait::ReadinessFence(operation_id).status(
        session_open_readiness_error_message(
            ctx,
            operation_id,
            state,
            session_open_readiness_summary(readiness).as_str(),
        ),
    )
}

pub(crate) fn record_session_open_readiness_transition_on_operation(
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

pub(crate) fn record_session_open_readiness_transition(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    phase: &str,
    readiness: &MediaReadiness,
) -> Result<(), remanence_state::StateError> {
    record_session_open_readiness_poll_transition(index, operation_id, phase, readiness, false)
}

pub(crate) fn record_session_open_readiness_poll_transition(
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

pub(crate) fn record_session_open_readiness_operation(
    index: &mut CatalogIndex,
    operation_id: Uuid,
    ctx: &SessionOpenReadinessContext<'_>,
) -> Result<(), remanence_state::StateError> {
    index
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            endpoint: remanence_state::DriveKey::new(ctx.library_serial.to_string(), ctx.bay),
            changer_sg: None,
            drive_sg: None,
            drive_serial: ctx.drive_serial.map(ToOwned::to_owned),
            barcode: ctx.barcode.map(ToOwned::to_owned),
            source_slot: ctx.source_slot,
            media_generation: index
                .readiness_media_observation(ctx.barcode)?
                .map(|observation| observation.generation)
                .map(|generation| generation.generation_number()),
            phase: "session_open_short_probe".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .map(|_| ())
}

pub(crate) fn session_open_readiness_state(readiness: &MediaReadiness) -> &'static str {
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

pub(crate) fn session_open_state_requires_release(state: &str) -> bool {
    crate::startup_media_readiness::media_readiness_state_requires_release(state)
}

pub(crate) type SessionOpenReadinessEvidence = (
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub(crate) fn session_open_readiness_evidence(
    readiness: &MediaReadiness,
) -> SessionOpenReadinessEvidence {
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

pub(crate) fn session_open_readiness_summary(readiness: &MediaReadiness) -> String {
    match readiness {
        MediaReadiness::Ready => "ready".to_string(),
        MediaReadiness::BecomingReady { ascq: 0x02, .. } => {
            "drive asks for an initializing command on TEST UNIT READY sense 02/04/02; the cartridge sits unthreaded in the drive until an SSC LOAD arrives".to_string()
        }
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

pub(crate) fn session_open_quarantine_id(operation_id: Uuid) -> String {
    format!("mrq-{operation_id}")
}

pub(crate) fn session_open_json_detail(field: &str, value: &str) -> String {
    format!(
        "{{\"{}\":\"{}\"}}",
        session_open_json_escape(field),
        session_open_json_escape(value)
    )
}

pub(crate) fn session_open_json_escape(value: &str) -> String {
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

pub(crate) fn session_open_readiness_error_message(
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

pub(crate) fn session_open_recording_failure_status(
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
