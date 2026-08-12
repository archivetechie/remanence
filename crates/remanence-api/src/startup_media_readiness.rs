//! Startup media-readiness reconciliation and transport/status mapping.

use remanence_state::{
    CatalogIndex, MediaReadinessOperationRecord, MediaReadinessTransitionInput, StateError,
};
use tonic::Status;
use uuid::Uuid;

use crate::hex_encoding::bytes_to_hex;

pub(crate) fn status_from_state_error(err: StateError) -> Status {
    match err {
        StateError::ConfigInvalid(_) => Status::invalid_argument(err.to_string()),
        StateError::IdempotencyConflict(_) => Status::already_exists(err.to_string()),
        StateError::TapePoolAssignmentConflict(_)
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
    Status::failed_precondition(format!(
        "{action} is blocked by active media-readiness fence {} operation={} library={} drive=0x{:04x} barcode={} state={}",
        first
            .quarantine_id
            .as_deref()
            .unwrap_or("(no-quarantine-id)"),
        first.operation_id,
        first.library_serial,
        first.drive_element,
        first.barcode.as_deref().unwrap_or("(unknown)"),
        first.state
    ))
}

pub(crate) fn ensure_media_readiness_admitted(
    index: &CatalogIndex,
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
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(media_readiness_conflict_status(action, &conflicts))
    }
}

/// Reconcile active media-readiness fences against the current selected-library
/// discovery snapshot using only TEST UNIT READY probes.
pub fn reconcile_media_readiness_on_startup(
    index: &mut CatalogIndex,
    report: &remanence_library::DiscoveryReport,
    policy: &remanence_library::StaticAllowlist,
) -> Result<usize, Status> {
    use remanence_library::AccessPolicy as _;

    let mut reconciled = 0usize;
    for library in &report.libraries {
        if !policy.allows(library.serial.as_str()) {
            continue;
        }
        let active = index
            .list_active_media_readiness_operations(Some(library.serial.as_str()))
            .map_err(status_from_state_error)?;
        if active.is_empty() {
            continue;
        }
        let mut handle = library
            .open(policy)
            .map_err(|err| Status::internal(format!("startup readiness open library: {err}")))?;
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
        reconciled =
            reconciled.saturating_add(reconcile_library_media_readiness_records_on_startup(
                index,
                library,
                &mut opened_drives,
                active,
            )?);
    }
    Ok(reconciled)
}

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
    let mut reconciled = 0usize;
    for record in records {
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
                    reconciled = reconciled.saturating_add(1);
                    continue;
                };
                let readiness = drive.probe_media_readiness(family);
                index
                    .record_media_readiness_transition(startup_media_readiness_probe_transition(
                        operation_id,
                        &readiness,
                    ))
                    .map_err(status_from_state_error)?;
                reconciled = reconciled.saturating_add(1);
            }
            StartupReadinessPlan::KeepFenced { transition } => {
                index
                    .record_media_readiness_transition(*transition)
                    .map_err(status_from_state_error)?;
                reconciled = reconciled.saturating_add(1);
            }
        }
    }
    if reconciled > 0 {
        tracing::info!(
            library_serial = library.serial.as_str(),
            count = reconciled,
            "reconciled active media-readiness fence(s) during startup"
        );
    }
    Ok(reconciled)
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
    let Ok(drive_element) = u16::try_from(record.drive_element) else {
        return StartupReadinessPlan::KeepFenced {
            transition: Box::new(startup_media_readiness_unresolved_transition(
                operation_id,
                format!(
                    "recorded drive element {} is outside u16 range",
                    record.drive_element
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
        cancel_source: None,
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

pub(crate) fn media_readiness_u8_field(value: Option<i64>) -> Option<u8> {
    value.and_then(|value| u8::try_from(value).ok())
}

pub(crate) fn media_readiness_state_name(
    readiness: &remanence_library::MediaReadiness,
) -> &'static str {
    match readiness {
        remanence_library::MediaReadiness::Ready => "ready",
        remanence_library::MediaReadiness::BecomingReady {
            media_initializing: true,
            ..
        } => "media_initializing",
        remanence_library::MediaReadiness::BecomingReady { .. } => "becoming_ready",
        remanence_library::MediaReadiness::UnitAttention { .. } => "unit_attention",
        remanence_library::MediaReadiness::TargetBusy { .. } => "target_busy",
        remanence_library::MediaReadiness::ReservationConflict => "reservation_conflict",
        remanence_library::MediaReadiness::TransportUnknown { .. } => "transport_unknown",
        remanence_library::MediaReadiness::NoMedium { .. }
        | remanence_library::MediaReadiness::RepeatedUnitAttention { .. }
        | remanence_library::MediaReadiness::TerminalNotReady { .. }
        | remanence_library::MediaReadiness::CheckCondition { .. }
        | remanence_library::MediaReadiness::UndecodedCheckCondition { .. }
        | remanence_library::MediaReadiness::TaskAborted
        | remanence_library::MediaReadiness::UnexpectedStatus { .. }
        | remanence_library::MediaReadiness::InvalidRequest { .. } => "terminal_error",
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
        remanence_library::MediaReadiness::BecomingReady { .. }
        | remanence_library::MediaReadiness::TerminalNotReady { .. } => Some(0x02),
        remanence_library::MediaReadiness::NoMedium { .. } => Some(0x02),
        remanence_library::MediaReadiness::UnitAttention { .. }
        | remanence_library::MediaReadiness::RepeatedUnitAttention { .. } => Some(0x06),
        remanence_library::MediaReadiness::CheckCondition { key, .. } => Some(*key),
        _ => None,
    }
}

pub(crate) fn media_readiness_asc(readiness: &remanence_library::MediaReadiness) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness::BecomingReady { .. }
        | remanence_library::MediaReadiness::TerminalNotReady { .. } => Some(0x04),
        remanence_library::MediaReadiness::NoMedium { .. } => Some(0x3a),
        remanence_library::MediaReadiness::UnitAttention { asc, .. }
        | remanence_library::MediaReadiness::RepeatedUnitAttention { asc, .. }
        | remanence_library::MediaReadiness::CheckCondition { asc, .. } => Some(*asc),
        _ => None,
    }
}

pub(crate) fn media_readiness_ascq(readiness: &remanence_library::MediaReadiness) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness::BecomingReady { ascq, .. }
        | remanence_library::MediaReadiness::NoMedium { ascq }
        | remanence_library::MediaReadiness::TerminalNotReady { ascq, .. }
        | remanence_library::MediaReadiness::UnitAttention { ascq, .. }
        | remanence_library::MediaReadiness::RepeatedUnitAttention { ascq, .. }
        | remanence_library::MediaReadiness::CheckCondition { ascq, .. } => Some(*ascq),
        _ => None,
    }
}

pub(crate) fn media_readiness_target_status(
    readiness: &remanence_library::MediaReadiness,
) -> Option<u8> {
    match readiness {
        remanence_library::MediaReadiness::TargetBusy { status }
        | remanence_library::MediaReadiness::UnexpectedStatus { status } => Some(*status),
        remanence_library::MediaReadiness::ReservationConflict => Some(0x18),
        remanence_library::MediaReadiness::TaskAborted => Some(0x40),
        _ => None,
    }
}

pub(crate) fn media_readiness_transport_class(
    readiness: &remanence_library::MediaReadiness,
) -> Option<String> {
    matches!(
        readiness,
        remanence_library::MediaReadiness::TransportUnknown { .. }
    )
    .then(|| "unknown".to_string())
}

pub(crate) fn media_readiness_sense_raw(
    readiness: &remanence_library::MediaReadiness,
) -> Option<String> {
    match readiness {
        remanence_library::MediaReadiness::UndecodedCheckCondition { sense } => {
            Some(bytes_to_hex(sense))
        }
        _ => None,
    }
}

pub(crate) fn media_readiness_error_json(
    readiness: &remanence_library::MediaReadiness,
) -> Option<String> {
    match readiness {
        remanence_library::MediaReadiness::RepeatedUnitAttention { .. } => Some(json_detail(
            "repeated_unit_attention",
            "unit attention repeated during startup readiness reconciliation",
        )),
        remanence_library::MediaReadiness::TerminalNotReady { action, .. } => {
            Some(json_detail("terminal_not_ready", action))
        }
        remanence_library::MediaReadiness::TransportUnknown { detail }
        | remanence_library::MediaReadiness::InvalidRequest { detail } => {
            Some(json_detail("startup_reconcile_error", detail))
        }
        remanence_library::MediaReadiness::UndecodedCheckCondition { .. } => Some(json_detail(
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
