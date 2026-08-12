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

pub(crate) fn handle_drive_wait_ready(
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

pub(crate) fn poll_drive_media_readiness(
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
    Err(Status::failed_precondition(
        session_open_admission_error_message(ctx, &conflicts),
    ))
}

pub(crate) fn session_open_admission_error_message(
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

pub(crate) fn session_open_media_family(barcode: Option<&str>) -> MediaFamily {
    if barcode
        .and_then(crate::lto_generation_from_voltag)
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
        MediaReadiness::BecomingReady { ascq: 0x02, .. } => true,
        MediaReadiness::NoMedium { .. } => ctx.needs_drive_load,
        _ => false,
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
    Status::failed_precondition(session_open_readiness_error_message(
        ctx,
        operation_id,
        "transport_unknown",
        detail.as_str(),
    ))
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
    Status::failed_precondition(session_open_readiness_error_message(
        ctx,
        operation_id,
        state,
        session_open_readiness_summary(readiness).as_str(),
    ))
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
    matches!(
        state,
        "aborted_unknown"
            | "timeout_unknown"
            | "transport_unknown"
            | "terminal_error"
            | "reservation_conflict"
    )
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
