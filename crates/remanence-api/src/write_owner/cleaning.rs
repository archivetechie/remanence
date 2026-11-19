//! Drive-cleaning orchestration, frequency admission, retry, and alarm helpers.

#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
use ciborium::value::Value as CborValue;
#[cfg(test)]
use remanence_state::AuditEvent;
use remanence_state::{index::CleanRunRecord, CatalogIndex};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tonic::Status;

use super::actor_runtime::{
    clear_alarm_with_evidence, raise_alarm_with_evidence, WriteOwnerConfig,
};
use super::cleaning_source as source;
#[cfg(test)]
use super::robotics::record_library_event;
use crate::status_from_state_error;
use remanence_state::library_log::CleanPhase;

pub(crate) fn run_cleaning_sequence(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    drive_uuid: &[u8],
    trigger: &str,
) -> Result<(), Status> {
    library.with_motion_context(
        remanence_library::motion::MotionContext {
            parent_operation: Some(*handle.op_id_uuid().as_bytes()),
            ..Default::default()
        },
        |library| run_cleaning_sequence_owned(index, cfg, handle, library, drive_uuid, trigger),
    )
}

fn run_cleaning_sequence_owned(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    drive_uuid: &[u8],
    trigger: &str,
) -> Result<(), Status> {
    if let Some(lifecycle) = &cfg.lifecycle {
        lifecycle.check_admission()?;
    }
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
    let run = source::begin(index, cfg, drive_uuid, library_serial.as_str(), trigger)?;
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
        let _ = source::advance(
            index,
            cfg,
            run.run_id.as_str(),
            CleanPhase::Failed,
            Some(fence_detail.as_str()),
        );
        return Err(err);
    }
    let owned_fence = source::fence(
        index,
        cfg,
        handle,
        drive_uuid,
        library_serial.as_str(),
        drive_bay,
    )?;
    let mut no_move_admitted = true;
    let result = run_cleaning_body(
        index,
        cfg,
        handle,
        library,
        &run,
        drive_bay,
        owned_fence,
        &mut no_move_admitted,
    );
    if let Err(error) = &result {
        finish_failed_cleaning(
            index,
            cfg,
            handle,
            library,
            &run,
            drive_bay,
            owned_fence,
            no_move_admitted,
            error,
        );
    }
    result
}

/// Run the fenced portion of cleaning. Every error returns to the owner's
/// single failure funnel, with the MOVE admission fact captured before branching.
#[allow(clippy::too_many_arguments)]
fn run_cleaning_body(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    run: &CleanRunRecord,
    drive_bay: u16,
    owned_fence: Option<u64>,
    no_move_admitted: &mut bool,
) -> Result<(), Status> {
    let clean_cfg = &cfg.cleaning;
    let library_serial = &run.library_serial;
    let drive_uuid = run.drive_uuid.as_slice();
    #[cfg(test)]
    if cfg.authority.runtime().is_none() {
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
        let tape = match source::register(index, cfg, voltag, slot.element_address) {
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
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("no-cln-cart:{library_serial}").as_str(),
            "no-cln-cart",
            "critical",
            Some(detail.as_str()),
        );
        let _ = source::advance(
            index,
            cfg,
            run.run_id.as_str(),
            CleanPhase::Failed,
            Some(detail.as_str()),
        );
        return Err(Status::failed_precondition(reason));
    }
    eligible_carts.sort_by_key(|(slot, _, _)| *slot);
    let (slot_address, voltag, tape_row) = eligible_carts.remove(0);
    let selected = source::select(
        index,
        cfg,
        run.run_id.as_str(),
        tape_row.tape_uuid.as_slice(),
        slot_address,
    )?;
    let run_id = selected.run_id.clone();
    let complete_timeout = parse_duration_or(&clean_cfg.complete_timeout, Duration::minutes(10));
    let mut motion =
        crate::drive_pool::MotionAdmission::new(cfg.lifecycle.clone(), cfg.endpoint(drive_bay));
    let maintenance = if cfg.authority.runtime().is_some() {
        Some(remanence_library::motion::MaintenanceCapability {
            fence: owned_fence
                .ok_or_else(|| Status::internal("cleaning load has no owned fence sequence"))?,
            run: run_id.clone(),
            cartridge: tape_row
                .tape_uuid
                .as_slice()
                .try_into()
                .map_err(|_| Status::internal("cleaning cartridge has invalid UUID"))?,
        })
    } else {
        None
    };
    let mut failed_load_after_completed_move = false;
    let loaded = retry_cleaning_move(
        index,
        cfg,
        run_id.as_str(),
        drive_uuid,
        "moving-in",
        true,
        || {
            library
                .with_motion_context(
                    remanence_library::motion::MotionContext {
                        parent_operation: Some(*handle.op_id_uuid().as_bytes()),
                        maintenance: maintenance.clone(),
                        ..Default::default()
                    },
                    |library| {
                        library.load_with_admission(
                            slot_address,
                            drive_bay,
                            &cfg.policy,
                            &mut || motion.admit(),
                        )
                    },
                )
                .map_err(|err| {
                    motion.refusal.take().unwrap_or_else(|| match &err {
                        remanence_library::LoadError::DriveLoad(error)
                            if matches!(
                                error.kind,
                                remanence_library::DriveOpErrorKind::MotionAuthority(_)
                            ) =>
                        {
                            failed_load_after_completed_move = true;
                            let remanence_library::DriveOpErrorKind::MotionAuthority(error) =
                                &error.kind
                            else {
                                unreachable!()
                            };
                            crate::motion_status::status(error, false)
                        }
                        remanence_library::LoadError::Move(error)
                            if matches!(
                                error.kind,
                                remanence_library::MoveErrorKind::MotionAuthority(_)
                            ) =>
                        {
                            let remanence_library::MoveErrorKind::MotionAuthority(error) =
                                &error.kind
                            else {
                                unreachable!()
                            };
                            crate::motion_status::status(error, false)
                        }
                        _ => Status::internal(format!("load cleaning cartridge: {err}")),
                    })
                })
        },
    );
    *no_move_admitted = !motion.dispatched;
    if let Err(error) = loaded {
        // A cleaning cartridge may eject as soon as MOVE seats it, before LOAD
        // admission can confirm the bay. Preserve that actual read-only eject
        // observation; never retry the already completed MOVE from an empty slot.
        if failed_load_after_completed_move
            && crate::motion_status::classified(&error)
                == Some(remanence_library::motion::MotionAuthorityFailure::EvidenceFailure)
            && cfg
                .authority
                .runtime()
                .is_some_and(|r| r.ensure_motion_allowed().is_ok())
            && observed_empty_cleaning_bay(library, drive_bay)?
        {
            source::cartridge_failed(
                index,
                cfg,
                &run_id,
                &tape_row.tape_uuid,
                true,
                "fast-eject before LOAD admission",
            )?;
            source::advance(
                index,
                cfg,
                &run_id,
                CleanPhase::Failed,
                Some("{\"reason\":\"fast-eject\"}"),
            )?;
            // DEC-5: GOOD followed by an eject leaves the LOAD Unknown. Observe
            // the changer through the shared funnel so later motion can proceed.
            // Record the re-read before the alarm, explaining a failed read
            // while preserving the failed run and its original refusal.
            let alarm_reason = match library.reconcile_startup_motion() {
                Ok(()) => "{\"reason\":\"fast-eject\"}".to_string(),
                Err(error) => {
                    tracing::warn!(
                        target: "remanence_cleaning",
                        %error,
                        "reconcile changer after cleaning cartridge fast-eject"
                    );
                    let at = OffsetDateTime::now_utc()
                        .format(&Rfc3339)
                        .expect("current UTC time has an RFC 3339 representation");
                    let reread = format!(
                        "at {at} an inventory re-read failed: {error}; robot moves wait for the next re-read"
                    );
                    format!(
                        "{{\"reason\":\"fast-eject\",\"reread\":\"{}\"}}",
                        crate::startup_media_readiness::json_escape(&reread)
                    )
                }
            };
            raise_alarm_with_evidence(
                index,
                cfg,
                &format!("cln-cart-expired:{voltag}"),
                "cln-cart-expired",
                "warning",
                Some(&alarm_reason),
            )?;
            return Err(Status::failed_precondition(
                "cleaning cartridge fast-ejected before LOAD admission",
            ));
        }
        if crate::motion_status::classified(&error)
            == Some(remanence_library::motion::MotionAuthorityFailure::EvidenceFailure)
        {
            let detail = format!("{{\"reason\":\"{}\"}}", json_escape_text(error.message()));
            source::advance(index, cfg, &run_id, CleanPhase::Failed, Some(&detail))?;
            raise_alarm_with_evidence(
                index,
                cfg,
                &format!("cleaning-needs-operator:{run_id}"),
                "cleaning-needs-operator",
                "warning",
                Some(&detail),
            )?;
        }
        return Err(error);
    }
    drop(motion);
    let load_completed = std::time::Instant::now();
    source::advance(
        index,
        cfg,
        run_id.as_str(),
        CleanPhase::Cleaning,
        Some("{\"phase\":\"cleaning\"}"),
    )?;
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
        let _ = source::advance(
            index,
            cfg,
            run_id.as_str(),
            CleanPhase::NeedsOperator,
            Some(detail.as_str()),
        );
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
        source::cartridge_failed(
            index,
            cfg,
            run_id.as_str(),
            tape_row.tape_uuid.as_slice(),
            true,
            "fast-eject",
        )?;
        source::advance(
            index,
            cfg,
            run_id.as_str(),
            CleanPhase::Failed,
            Some("{\"reason\":\"fast-eject\"}"),
        )?;
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
        let _ = source::advance(
            index,
            cfg,
            run_id.as_str(),
            CleanPhase::Failed,
            Some("{\"reason\":\"verify-read-failed\"}"),
        );
        Status::unavailable(format!("read TapeAlert page: {err}"))
    })?;
    let active_alerts = alerts.active();
    if alerts.is_set(22) {
        // Returning home must precede expiry/rejection: afterwards the library
        // log permits only a non-crediting Finish for this run.
        let returned = move_cleaning_cartridge_back(
            index,
            cfg,
            library,
            &run_id,
            drive_uuid,
            drive_bay,
            slot_address,
        );
        let marked =
            source::cartridge_failed(index, cfg, &run_id, &tape_row.tape_uuid, true, "flag-22");
        if returned.is_ok() {
            marked?;
        } else {
            let _ = marked;
        }
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            &format!("cln-cart-expired:{voltag}"),
            "cln-cart-expired",
            "warning",
            Some("{\"reason\":\"flag-22\"}"),
        );
        returned?;
        source::advance(
            index,
            cfg,
            &run_id,
            CleanPhase::Failed,
            Some("{\"reason\":\"flag-22\"}"),
        )?;
        return Err(Status::failed_precondition(
            "cleaning cartridge expired during cleaning",
        ));
    }
    if alerts.is_set(20) || alerts.is_set(21) {
        // Returning home must precede expiry/rejection: afterwards the library
        // log permits only a non-crediting Finish for this run.
        let returned = move_cleaning_cartridge_back(
            index,
            cfg,
            library,
            &run_id,
            drive_uuid,
            drive_bay,
            slot_address,
        );
        let marked = source::cartridge_failed(
            index,
            cfg,
            &run_id,
            &tape_row.tape_uuid,
            false,
            "corroboration",
        );
        if returned.is_ok() {
            marked?;
        } else {
            let _ = marked;
        }
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            &format!("cart-not-cleaning-behavior:{voltag}"),
            "cart-not-cleaning-behavior",
            "warning",
            Some("{\"reason\":\"corroboration\"}"),
        );
        returned?;
        source::advance(
            index,
            cfg,
            &run_id,
            CleanPhase::Failed,
            Some("{\"reason\":\"corroboration\"}"),
        )?;
        return Err(Status::failed_precondition(
            "cleaning cartridge behaved like data media",
        ));
    }
    move_cleaning_cartridge_back(
        index,
        cfg,
        library,
        &run_id,
        drive_uuid,
        drive_bay,
        slot_address,
    )?;
    let eject_observed = std::time::Instant::now();
    if eject_observed.duration_since(load_completed)
        < std::time::Duration::from_millis(min_cycle_millis)
    {
        source::cartridge_failed(
            index,
            cfg,
            run_id.as_str(),
            tape_row.tape_uuid.as_slice(),
            true,
            "fast-eject",
        )?;
        source::advance(
            index,
            cfg,
            run_id.as_str(),
            CleanPhase::Failed,
            Some("{\"reason\":\"fast-eject\"}"),
        )?;
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
    source::advance(
        index,
        cfg,
        run_id.as_str(),
        CleanPhase::Verifying,
        Some("{\"phase\":\"verifying\"}"),
    )?;
    source::finish_verified(
        index,
        cfg,
        run_id.as_str(),
        drive_uuid,
        tape_row.tape_uuid.as_slice(),
        detail.as_str(),
    )?;
    source::release(
        index,
        cfg,
        drive_uuid,
        owned_fence,
        "cleaning verified and cartridge returned to its observed home slot",
    )?;
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
    #[cfg(test)]
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
    #[cfg(test)]
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

/// Finish only this still-open run, then release its own obligation only when
/// admission or a fresh empty-bay observation proves the cartridge is out.
/// Cleanup failures never replace the error returned by the cleaning body.
#[allow(clippy::too_many_arguments)]
fn finish_failed_cleaning(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    handle: &crate::operations::OperationHandle,
    library: &mut remanence_library::LibraryHandle,
    run: &CleanRunRecord,
    drive_bay: u16,
    owned_fence: Option<u64>,
    no_move_admitted: bool,
    error: &Status,
) {
    match index.get_active_clean_run_by_drive(&run.drive_uuid) {
        Ok(Some(active)) if active.run_id == run.run_id => {
            let detail = if crate::drive_pool::DaemonShuttingDown::is_status(error) {
                "{\"outcome\":\"cancelled\",\"reason\":\"daemon shutting down\"}".to_owned()
            } else {
                format!("{{\"reason\":\"{}\"}}", json_escape_text(error.message()))
            };
            if let Err(error) =
                source::advance(index, cfg, &run.run_id, CleanPhase::Failed, Some(&detail))
            {
                tracing::warn!(%error, run_id = %run.run_id, "finish failed cleaning run");
            }
        }
        Ok(_) => (),
        Err(error) => {
            tracing::warn!(%error, run_id = %run.run_id, "read failed cleaning run before finish")
        }
    }
    let reason = if no_move_admitted {
        Some("cleaning run released its drive obligation: no MOVE was admitted")
    } else {
        match observed_empty_cleaning_bay(library, drive_bay) {
            Ok(true) => {
                Some("cleaning run released its drive obligation: the drive bay was seen empty")
            }
            Ok(false) => None,
            Err(error) => {
                tracing::warn!(%error, run_id = %run.run_id, "cannot prove cleaning cartridge out of drive");
                None
            }
        }
    };
    let Some(reason) = reason else {
        return;
    };
    if let Err(error) = source::release(index, cfg, &run.drive_uuid, owned_fence, reason) {
        tracing::warn!(%error, run_id = %run.run_id, "release failed cleaning run's drive obligation");
        return;
    }
    if let Err(error) = clear_alarm_with_evidence(
        index,
        cfg,
        &format!("cleaning-needs-operator:{}", run.run_id),
    ) {
        tracing::warn!(%error, run_id = %run.run_id, "clear released cleaning run's alarm");
    }
    #[cfg(test)]
    if cfg.authority.runtime().is_none() {
        let _ = record_library_event(
            index,
            cfg,
            handle,
            &run.library_serial,
            AuditEvent::DriveUnfenced,
            BTreeMap::from([
                (
                    "drive_uuid".to_owned(),
                    CborValue::Bytes(run.drive_uuid.clone()),
                ),
                (
                    "component".to_owned(),
                    CborValue::Text("cleaning".to_owned()),
                ),
            ]),
        );
    }
    #[cfg(not(test))]
    let _ = handle;
}

/// Share the existing dismount admission and retry across success and TapeAlert
/// ends, retaining the guard through UNLOAD and the MOVE to the home slot.
fn move_cleaning_cartridge_back(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    library: &mut remanence_library::LibraryHandle,
    run_id: &str,
    drive_uuid: &[u8],
    drive_bay: u16,
    slot_address: u16,
) -> Result<(), Status> {
    source::advance(
        index,
        cfg,
        run_id,
        CleanPhase::MovingBack,
        Some("{\"phase\":\"moving-back\"}"),
    )?;
    // Admission happens at physical UNLOAD dispatch. Retain it across both
    // retries and MOVE so shutdown cannot strand the unthreaded cartridge.
    let mut dismount = None;
    let shutdown_refusal = std::cell::RefCell::new(None);
    let mut admit = || {
        if dismount.is_none() {
            dismount = cfg
                .lifecycle
                .as_ref()
                .map(|l| l.begin_dismount(&cfg.endpoint(drive_bay)))
                .transpose()
                .map_err(|error| {
                    let message = error.message().to_string();
                    *shutdown_refusal.borrow_mut() = Some(error);
                    message
                })?;
        }
        Ok(())
    };
    library.set_changer_ready_wait(cfg.changer_ready_wait.clone().without_stop_signals());
    retry_cleaning_move(index, cfg, run_id, drive_uuid, "moving-back", false, || {
        crate::mount_support::unload_library_bay_with_admission(
            library,
            drive_bay,
            Some(slot_address),
            &cfg.policy,
            None,
            std::path::Path::new(crate::mount_support::DEVICE_LOCK_ROOT),
            &mut admit,
        )
        .map_err(|err| {
            shutdown_refusal
                .borrow_mut()
                .take()
                .unwrap_or_else(|| Status::internal(format!("unload cleaning cartridge: {err}")))
        })?;
        Ok(())
    })?;
    drop(dismount);
    Ok(())
}

pub(crate) fn cleaning_too_soon(
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

pub(crate) fn parse_duration_or(value: &str, default: Duration) -> Duration {
    parse_simple_duration(value).unwrap_or(default)
}

pub(crate) fn parse_simple_duration(value: &str) -> Option<Duration> {
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

pub(crate) fn json_escape_text(value: &str) -> String {
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

/// Only a fresh, clean inventory with the bay present can prove an eject.
fn observed_empty_cleaning_bay(
    library: &mut remanence_library::LibraryHandle,
    bay: u16,
) -> Result<bool, Status> {
    library
        .refresh()
        .map_err(|e| Status::unavailable(format!("observe cleaning eject: {e}")))?;
    Ok(!library.is_dirty()
        && library
            .library()
            .drive_bays
            .iter()
            .find(|v| v.element_address == bay)
            .is_some_and(|v| !v.loaded))
}

pub(crate) fn cleaning_drive_is_idle(
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

pub(crate) fn retry_cleaning_move(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    run_id: &str,
    drive_uuid: &[u8],
    label: &str,
    defer_load_evidence: bool,
    mut op: impl FnMut() -> Result<(), Status>,
) -> Result<(), Status> {
    let mut last_err = None;
    let mut first_err = None;
    let mut authority_refusal = None;
    for attempt in 0..2 {
        match op() {
            Ok(()) => return Ok(()),
            Err(err) if crate::drive_pool::DaemonShuttingDown::is_status(&err) => return Err(err),
            Err(err) => {
                if defer_load_evidence
                    && crate::motion_status::classified(&err)
                        == Some(remanence_library::motion::MotionAuthorityFailure::EvidenceFailure)
                {
                    return Err(err);
                }
                first_err.get_or_insert_with(|| err.message().to_string());
                last_err = Some(err.message().to_string());
                if crate::motion_status::classified(&err).is_some() {
                    authority_refusal = Some(err);
                    break;
                }
                if attempt == 0 {
                    tracing::warn!("{label} failed once during cleaning; retrying");
                }
            }
        }
    }
    let last = last_err.unwrap_or_else(|| "move failed".to_string());
    let err = match first_err {
        Some(first) if first != last => format!("{first}; retry: {last}"),
        _ => last,
    };
    let detail = format!(
        "{{\"run_id\":\"{}\",\"drive_uuid\":\"{}\",\"recovery_step\":\"{}\",\"error\":\"{}\"}}",
        json_escape_text(run_id),
        json_escape_text(&crate::bytes_to_hex(drive_uuid)),
        json_escape_text(label),
        json_escape_text(&err),
    );
    let _ = source::advance(
        index,
        cfg,
        run_id,
        CleanPhase::Failed,
        Some(detail.as_str()),
    );
    let _ = raise_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(detail.as_str()),
    );
    Err(authority_refusal.unwrap_or_else(|| Status::internal(err)))
}

#[cfg(test)]
mod tests {
    use super::observed_empty_cleaning_bay;

    /// T7: a successful RES with a changed element count leaves a stale empty
    /// snapshot; it cannot prove that the cleaning cartridge is out.
    #[test]
    fn k67_dirty_inventory_cannot_prove_empty_cleaning_bay() {
        use remanence_library::{transport::FixtureTransport, StaticAllowlist};
        let snapshot =
            remanence_chaos::model::VirtualWorld::single_drive("LIB-T7", 0x100, "DRV-T7", 0x400, 1)
                .library_snapshot();
        let inquiry = include_bytes!("../../../../fixtures/inquiry/changer-msl-g3.bin").to_vec();
        let mut serial = vec![8, 0x80, 0, 6];
        serial.extend_from_slice(b"LIB-T7");
        let responses = vec![inquiry, serial, vec![0; 8], vec![0; 8]];
        let mut library = snapshot
            .open_with(&StaticAllowlist::new(["LIB-T7"]), move |_| {
                Ok(Box::new(
                    FixtureTransport::new().with_responses(responses.clone()),
                ))
            })
            .unwrap();
        assert!(!library.library().drive_bays[0].loaded);
        assert!(!library.is_dirty());
        let empty = observed_empty_cleaning_bay(&mut library, 0x100).unwrap();
        assert!(library.is_dirty(), "canned RES must cause a shape mismatch");
        assert!(
            !library.library().drive_bays[0].loaded,
            "old snapshot retained"
        );
        assert!(!empty, "a dirty inventory is no empty-bay proof");
    }
}
