//! Drive inventory collection and foreign-drive health polling.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use remanence_state::{
    AuditActor, AuditEvent, CatalogIndex, DriveHealthSnapshotInput, RemConfig, SourceLayer,
    StateError,
};
use tonic::Status;

use crate::audit_projection::{
    alarm_audit_detail, append_and_project_audit, drive_health_audit_detail, ProjectedAuditInput,
};
use crate::hex_encoding::bytes_to_hex;
use crate::startup_media_readiness::status_from_state_error;

pub(crate) fn spawn_drive_collection_workers(
    index_path: PathBuf,
    report: remanence_library::DiscoveryReport,
    config: &RemConfig,
    drive_pool: crate::write_owner::DrivePool,
    audit_append_lock: Arc<std::sync::Mutex<()>>,
) {
    let heartbeat = parse_duration_or(&config.drives.heartbeat, Duration::from_secs(60 * 60));
    let heartbeat_index_path = index_path.clone();
    let heartbeat_pool = drive_pool.clone();
    std::thread::Builder::new()
        .name("rem-drive-heartbeat".to_string())
        .spawn(move || loop {
            std::thread::sleep(heartbeat);
            if let Err(err) = touch_managed_drive_heartbeats(&heartbeat_index_path, &heartbeat_pool)
            {
                tracing::warn!("managed drive heartbeat failed: {err}");
            }
        })
        .expect("spawn managed drive heartbeat worker");

    let foreign_poll = parse_duration_or(
        &config.drives.foreign_counter_poll,
        Duration::from_secs(60 * 60),
    );
    let drives_cfg = config.drives.clone();
    let audit = AuditAppendContext {
        dir: config.audit.dir.clone(),
        fsync: config.audit.fsync,
        lock: audit_append_lock,
    };
    let daemon_libraries = config
        .libraries
        .iter()
        .map(|library| library.serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
        .collect::<std::collections::HashSet<_>>();
    std::thread::Builder::new()
        .name("rem-foreign-drive-poll".to_string())
        .spawn(move || {
            foreign_drive_poll_loop(
                index_path,
                report,
                drives_cfg,
                daemon_libraries,
                audit,
                foreign_poll,
            )
        })
        .expect("spawn foreign drive poll worker");
}

#[derive(Clone)]
pub(crate) struct AuditAppendContext {
    pub(crate) dir: PathBuf,
    pub(crate) fsync: bool,
    pub(crate) lock: Arc<std::sync::Mutex<()>>,
}

pub(crate) fn touch_managed_drive_heartbeats(
    index_path: &Path,
    drive_pool: &crate::write_owner::DrivePool,
) -> Result<(), StateError> {
    let index = CatalogIndex::open(index_path)?;
    for drive in index.list_drives(false, false)? {
        if drive.managed == "rem" && drive.state == "active" {
            let Some(bay) = drive
                .last_element_address
                .and_then(|address| u16::try_from(address).ok())
            else {
                continue;
            };
            let Some(library_serial) = drive.last_library_serial.as_deref() else {
                continue;
            };
            let drive_key = crate::drive_pool::DriveKey::new(library_serial.to_string(), bay);
            if let Err(err) = drive_pool.heartbeat_drive(&drive_key, drive.drive_uuid.clone()) {
                tracing::warn!(
                    "managed drive heartbeat skipped for {}: {err}",
                    drive.serial.as_deref().unwrap_or("<no serial>")
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn foreign_drive_poll_loop(
    index_path: PathBuf,
    report: remanence_library::DiscoveryReport,
    drives_cfg: remanence_state::DrivesConfig,
    daemon_libraries: std::collections::HashSet<String>,
    audit: AuditAppendContext,
    base_cadence: Duration,
) {
    let mut backoff = Duration::from_secs(0);
    loop {
        let delay = if backoff.is_zero() {
            base_cadence
        } else {
            backoff
        };
        std::thread::sleep(delay);
        match poll_foreign_drive_counters_once(
            &index_path,
            &report,
            &drives_cfg,
            &daemon_libraries,
            &audit,
        ) {
            Ok(()) => backoff = Duration::from_secs(0),
            Err(ForeignPollError::Retryable(message)) => {
                tracing::warn!("foreign drive counter poll retryable failure: {message}");
                backoff = next_backoff(backoff, base_cadence);
            }
            Err(ForeignPollError::Permanent(message)) => {
                tracing::warn!("foreign drive counter poll failed: {message}");
                backoff = Duration::from_secs(0);
            }
        }
    }
}

pub(crate) fn poll_foreign_drive_counters_once(
    index_path: &Path,
    report: &remanence_library::DiscoveryReport,
    drives_cfg: &remanence_state::DrivesConfig,
    daemon_libraries: &std::collections::HashSet<String>,
    audit: &AuditAppendContext,
) -> Result<(), ForeignPollError> {
    poll_foreign_drive_counters_once_with_reader(
        index_path,
        report,
        drives_cfg,
        daemon_libraries,
        audit,
        read_foreign_drive_snapshot,
    )
}

pub(crate) fn poll_foreign_drive_counters_once_with_reader(
    index_path: &Path,
    report: &remanence_library::DiscoveryReport,
    drives_cfg: &remanence_state::DrivesConfig,
    daemon_libraries: &std::collections::HashSet<String>,
    audit: &AuditAppendContext,
    mut read_snapshot: impl FnMut(&Path, bool) -> Result<ForeignDriveSnapshot, ForeignPollError>,
) -> Result<(), ForeignPollError> {
    let mut index = CatalogIndex::open(index_path)
        .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
    for library in &report.libraries {
        if library_is_managed(library.serial.as_str(), drives_cfg, daemon_libraries) {
            continue;
        }
        for bay in &library.drive_bays {
            let Some(installed) = bay.installed.as_ref() else {
                continue;
            };
            let Some(sg_path) = installed.sg_path.as_ref() else {
                continue;
            };
            let installed_serial = installed.serial.trim();
            if installed_serial.is_empty() {
                continue;
            }
            let Some(drive) = index
                .get_actionable_drive_at(library.serial.as_str(), i64::from(bay.element_address))
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?
            else {
                tracing::warn!(
                    "skipping foreign drive counter attribution for unresolved or ambiguous bay library_serial={} element_address={} serial={}",
                    library.serial,
                    bay.element_address,
                    installed_serial
                );
                continue;
            };
            if drive.serial.as_deref() != Some(installed_serial) {
                tracing::warn!(
                    "skipping foreign drive counter attribution for bay serial mismatch library_serial={} element_address={} observed_serial={} catalog_serial={}",
                    library.serial,
                    bay.element_address,
                    installed_serial,
                    drive.serial.as_deref().unwrap_or("<no serial>")
                );
                continue;
            }
            if drive.managed != "foreign" || drive.state != "active" {
                continue;
            }
            let snapshot = read_snapshot(sg_path, drives_cfg.foreign_tapealert)?;
            let tape_alert_flags = snapshot.tape_alert_flags.clone();
            let recorded_snapshot = index
                .record_drive_health_snapshot(DriveHealthSnapshotInput {
                    drive_uuid: drive.drive_uuid.clone(),
                    trigger: "foreign-counter".to_string(),
                    session_id: None,
                    tape_alert_flags,
                    write_errors_corrected: snapshot.write_errors_corrected.and_then(u64_to_i64),
                    write_errors_uncorrected: snapshot
                        .write_errors_uncorrected
                        .and_then(u64_to_i64),
                    read_errors_corrected: snapshot.read_errors_corrected.and_then(u64_to_i64),
                    read_errors_uncorrected: snapshot.read_errors_uncorrected.and_then(u64_to_i64),
                    raw_pages: Some(
                        "{\"write_error_counter\":true,\"read_error_counter\":true}".to_string(),
                    ),
                    at_utc: None,
                })
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            let health_detail = drive_health_audit_detail(&index, &recorded_snapshot)
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            append_and_project_audit(
                &mut index,
                audit.dir.as_path(),
                audit.fsync,
                &audit.lock,
                ProjectedAuditInput {
                    actor: AuditActor::System,
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                    event: AuditEvent::DriveHealthObserved,
                    subject_kind: "drive",
                    subject_id: Some(bytes_to_hex(recorded_snapshot.drive_uuid.as_slice())),
                    detail: health_detail,
                },
            )
            .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            let alarm = index
                .observe_foreign_drive_tapealert_advisory(
                    &drive.drive_uuid,
                    snapshot.tape_alert_flags.as_deref(),
                )
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            if let Some(alarm) = alarm {
                let event = if alarm.state == "cleared" {
                    AuditEvent::AlarmCleared
                } else {
                    AuditEvent::AlarmRaised
                };
                append_and_project_audit(
                    &mut index,
                    audit.dir.as_path(),
                    audit.fsync,
                    &audit.lock,
                    ProjectedAuditInput {
                        actor: AuditActor::System,
                        source_layer: SourceLayer::Layer4,
                        operation_id: None,
                        session_id: None,
                        idempotency_key: None,
                        event,
                        subject_kind: "alarm",
                        subject_id: Some(alarm.condition_key.clone()),
                        detail: alarm_audit_detail(&alarm),
                    },
                )
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            }
            index
                .touch_drive_last_seen(&drive.drive_uuid)
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
        }
    }
    Ok(())
}

pub(crate) fn library_is_managed(
    serial: &str,
    drives_cfg: &remanence_state::DrivesConfig,
    daemon_libraries: &std::collections::HashSet<String>,
) -> bool {
    let configured = drives_cfg
        .managed_libraries
        .iter()
        .map(|serial| serial.trim())
        .filter(|serial| !serial.is_empty())
        .collect::<std::collections::HashSet<_>>();
    if configured.is_empty() {
        daemon_libraries.contains(serial)
    } else {
        configured.contains(serial)
    }
}

#[derive(Debug)]
pub(crate) enum ForeignPollError {
    Retryable(String),
    Permanent(String),
}

pub(crate) struct ForeignDriveSnapshot {
    pub(crate) tape_alert_flags: Option<String>,
    pub(crate) write_errors_corrected: Option<u64>,
    pub(crate) write_errors_uncorrected: Option<u64>,
    pub(crate) read_errors_corrected: Option<u64>,
    pub(crate) read_errors_uncorrected: Option<u64>,
}

#[cfg(target_os = "linux")]
pub(crate) fn read_foreign_drive_snapshot(
    sg_path: &Path,
    foreign_tapealert: bool,
) -> Result<ForeignDriveSnapshot, ForeignPollError> {
    let inner = remanence_library::LinuxSgTransport::open(sg_path)
        .map_err(|err| ForeignPollError::Permanent(format!("open {}: {err}", sg_path.display())))?;
    let mut transport =
        remanence_library::ForeignDriveTransport::with_tapealert(inner, foreign_tapealert);
    let write = read_error_counter_page_from_transport(
        &mut transport,
        remanence_library::drive_log_sense::PAGE_WRITE_ERROR_COUNTER,
        remanence_library::drive_log_sense::build_write_error_counter_cdb,
    )?;
    let read = read_error_counter_page_from_transport(
        &mut transport,
        remanence_library::drive_log_sense::PAGE_READ_ERROR_COUNTER,
        remanence_library::drive_log_sense::build_read_error_counter_cdb,
    )?;
    let tape_alert_flags = if foreign_tapealert {
        Some(read_tape_alert_flags_from_transport(&mut transport)?)
    } else {
        None
    };
    Ok(ForeignDriveSnapshot {
        tape_alert_flags,
        write_errors_corrected: write.errors_corrected,
        write_errors_uncorrected: write.errors_uncorrected,
        read_errors_corrected: read.errors_corrected,
        read_errors_uncorrected: read.errors_uncorrected,
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_foreign_drive_snapshot(
    _sg_path: &Path,
    _foreign_tapealert: bool,
) -> Result<ForeignDriveSnapshot, ForeignPollError> {
    Err(ForeignPollError::Permanent(
        "foreign drive polling requires Linux SG_IO".to_string(),
    ))
}

#[cfg(target_os = "linux")]
pub(crate) fn read_error_counter_page_from_transport<T: remanence_library::SgTransport>(
    transport: &mut T,
    page_code: u8,
    build_cdb: fn(u16) -> [u8; 10],
) -> Result<remanence_library::drive_log_sense::ErrorCounterPage, ForeignPollError> {
    let cdb = build_cdb(remanence_library::drive_log_sense::ERROR_COUNTER_RESPONSE_LEN);
    let mut buf = [0u8; remanence_library::drive_log_sense::ERROR_COUNTER_RESPONSE_LEN as usize];
    transport.set_timeout_for(remanence_library::TimeoutClass::TapeStatus);
    let outcome = transport
        .execute_in(&cdb, &mut buf)
        .map_err(foreign_poll_error_from_scsi)?;
    let bytes = (outcome.bytes_transferred as usize).min(buf.len());
    remanence_library::drive_log_sense::parse_error_counter_response(&buf[..bytes], page_code)
        .map_err(foreign_poll_error_from_scsi)
}

#[cfg(target_os = "linux")]
pub(crate) fn read_tape_alert_flags_from_transport<T: remanence_library::SgTransport>(
    transport: &mut T,
) -> Result<String, ForeignPollError> {
    let cdb = remanence_library::drive_log_sense::build_tape_alert_cdb(
        remanence_library::drive_log_sense::TAPE_ALERT_RESPONSE_LEN,
    );
    let mut buf = [0u8; remanence_library::drive_log_sense::TAPE_ALERT_RESPONSE_LEN as usize];
    transport.set_timeout_for(remanence_library::TimeoutClass::TapeStatus);
    let outcome = transport
        .execute_in(&cdb, &mut buf)
        .map_err(foreign_poll_error_from_scsi)?;
    let bytes = (outcome.bytes_transferred as usize).min(buf.len());
    let alerts = remanence_library::drive_log_sense::parse_response(&buf[..bytes])
        .map_err(foreign_poll_error_from_scsi)?;
    Ok(tape_alert_flags_json(alerts.active()))
}

#[cfg(target_os = "linux")]
pub(crate) fn foreign_poll_error_from_scsi(err: remanence_library::ScsiError) -> ForeignPollError {
    if is_retryable_foreign_scsi_error(&err) {
        ForeignPollError::Retryable(err.to_string())
    } else {
        ForeignPollError::Permanent(err.to_string())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn is_retryable_foreign_scsi_error(err: &remanence_library::ScsiError) -> bool {
    match err {
        remanence_library::ScsiError::UnexpectedStatus { status } => {
            matches!(*status, 0x08 | 0x18)
        }
        remanence_library::ScsiError::CheckCondition { sense, .. }
        | remanence_library::ScsiError::TransportError { sense, .. } => {
            remanence_scsi_unit_attention(sense)
        }
        _ => false,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn remanence_scsi_unit_attention(sense: &[u8]) -> bool {
    remanence_library::decode_scsi_sense(sense).is_some_and(|sense| sense.key == 0x06)
}

pub(crate) fn next_backoff(current: Duration, max: Duration) -> Duration {
    let next = if current.is_zero() {
        Duration::from_secs(5)
    } else {
        current.saturating_mul(2)
    };
    next.min(max)
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
    let count = digits.parse::<u64>().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(count)),
        "s" => Some(Duration::from_secs(count)),
        "m" => Some(Duration::from_secs(count.saturating_mul(60))),
        "h" => Some(Duration::from_secs(count.saturating_mul(60 * 60))),
        _ => None,
    }
}

pub(crate) fn tape_alert_flags_json(flags: &std::collections::BTreeSet<u8>) -> String {
    let body = flags
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

pub(crate) fn u64_to_i64(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}
pub(crate) fn identity_source_name(source: remanence_library::IdentitySource) -> &'static str {
    match source {
        remanence_library::IdentitySource::DvcidInline => "DvcidInline",
        remanence_library::IdentitySource::DvcidAndInquiry => "DvcidAndInquiry",
        remanence_library::IdentitySource::Derived => "Derived",
    }
}

pub(crate) fn drive_managed_library_serials(config: &RemConfig) -> HashSet<String> {
    let configured = config
        .drives
        .managed_libraries
        .iter()
        .map(|serial| serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
        .collect::<HashSet<_>>();
    if !configured.is_empty() {
        return configured;
    }
    config
        .libraries
        .iter()
        .map(|library| library.serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
        .collect()
}

pub(crate) fn observe_drive_catalog_from_libraries<'a>(
    index: &mut CatalogIndex,
    libraries: impl IntoIterator<Item = &'a remanence_library::Library>,
    managed_library_serials: &HashSet<String>,
) -> Result<(), Status> {
    let observations = libraries
        .into_iter()
        .flat_map(|library| {
            let managed = managed_library_serials.contains(library.serial.as_str());
            library.drive_bays.iter().filter_map(move |bay| {
                let installed = bay.installed.as_ref()?;
                Some(remanence_state::DriveObservationInput {
                    serial: installed.serial.clone(),
                    identity_source: identity_source_name(installed.identity_source).to_string(),
                    vendor: installed.vendor.clone(),
                    product: installed.product.clone(),
                    firmware_rev: installed.revision.clone(),
                    managed: if managed { "rem" } else { "foreign" }.to_string(),
                    library_serial: Some(library.serial.clone()),
                    element_address: Some(i64::from(bay.element_address)),
                    observed_at_utc: None,
                })
            })
        })
        .collect::<Vec<_>>();
    index
        .observe_drive_inventory_snapshot(observations)
        .map(|_| ())
        .map_err(status_from_state_error)
}
