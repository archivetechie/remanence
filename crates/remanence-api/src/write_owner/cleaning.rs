//! Drive-cleaning orchestration, frequency admission, retry, and alarm helpers.

use std::collections::BTreeMap;

use ciborium::value::Value as CborValue;
use remanence_state::{AuditEvent, CatalogIndex};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tonic::Status;

use super::actor_runtime::{
    clear_alarm_with_evidence, raise_alarm_with_evidence, WriteOwnerConfig,
};
use super::robotics::record_library_event;
use crate::status_from_state_error;

pub(crate) fn run_cleaning_sequence(
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
    let run = index
        .begin_clean_run(drive_uuid, library_serial.as_str(), trigger, None)
        .map_err(status_from_state_error)?;
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
        let _ =
            index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(fence_detail.as_str()));
        return Err(err);
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
        let tape = match index.ensure_cleaning_cartridge(voltag) {
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
        let _ = clear_alarm_with_evidence(
            index,
            cfg,
            format!("cleaning-needs-operator:{}", run.run_id).as_str(),
        );
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
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
            format!("no-cln-cart:{library_serial}").as_str(),
            "no-cln-cart",
            "critical",
            Some(detail.as_str()),
        );
        let _ = index.terminalize_clean_run(run.run_id.as_str(), "failed", Some(detail.as_str()));
        return Err(Status::failed_precondition(reason));
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
    retry_cleaning_move(index, cfg, run_id.as_str(), drive_uuid, "moving-in", || {
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
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
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
        let _ = raise_alarm_with_evidence(
            index,
            cfg,
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
    retry_cleaning_move(
        index,
        cfg,
        run_id.as_str(),
        drive_uuid,
        "moving-back",
        || {
            library
                .unload(drive_bay, Some(slot_address), &cfg.policy)
                .map_err(|err| format!("unload cleaning cartridge: {err}"))?;
            Ok(())
        },
    )?;
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
    let _ = raise_alarm_with_evidence(
        index,
        cfg,
        format!("cleaning-needs-operator:{}", run_id).as_str(),
        "cleaning-needs-operator",
        "warning",
        Some(detail.as_str()),
    );
    Err(Status::internal(err))
}
