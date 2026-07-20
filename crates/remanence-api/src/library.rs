//! S6a LibraryService read inspection: ListLibraries and GetLibrary.
//!
//! This module projects the daemon startup `DiscoveryReport` snapshot held in
//! `ApiState` into the Layer 5 library proto surface. It joins cartridge voltags
//! to catalog tape UUIDs through a read-only index connection and never
//! dispatches to the drive-session owner, so read inspection remains available
//! while write/read/reconcile work is active. Robotics mutations are S6b;
//! library event streaming is S6c.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::pin::Pin;
use std::time::Duration;

use ciborium::value::Value as CborValue;
use remanence_library::{DriveBay, IePort, Library, Slot};
use remanence_state::{
    AlarmRecord, CatalogIndex, DriveAnnotationInput, DriveCorrelationRollupRecord,
    DriveEventRecord, DriveHealthSnapshotRecord, DriveRecord,
};
use time::OffsetDateTime;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::{pb, ApiState};

/// Fixed namespace for deterministic UUIDv5 library identities.
///
/// Never change this constant: derived S6a library UUIDs depend on it. A
/// durable assigned identity that survives changer serial changes is a later
/// storage-backed slice.
const REMANENCE_LIBRARY_NS: Uuid = Uuid::from_bytes([
    0x52, 0x65, 0x6d, 0x6e, 0x4c, 0x69, 0x62, 0x72, 0x61, 0x72, 0x79, 0x4e, 0x53, 0x76, 0x31, 0x00,
]);

pub(crate) fn library_uuid(serial: &str) -> [u8; 16] {
    *Uuid::new_v5(&REMANENCE_LIBRARY_NS, serial.as_bytes()).as_bytes()
}

pub(crate) fn project_library(library: &Library) -> pb::Library {
    pb::Library {
        library_serial: library.serial.clone(),
        vendor: library.changer_inquiry.vendor_str().to_string(),
        product: library.changer_inquiry.product_str().to_string(),
        product_revision: library.changer_inquiry.revision_str().to_string(),
        library_uuid: library_uuid(&library.serial).to_vec(),
    }
}

pub(crate) fn drive_status(bay: &DriveBay, busy: bool) -> pb::drive::Status {
    match bay.installed.as_ref() {
        None => pb::drive::Status::DriveStatusUnreachable,
        Some(installed) if installed.sg_path.is_none() => pb::drive::Status::DriveStatusUnreachable,
        Some(_) if busy => pb::drive::Status::DriveStatusBusy,
        Some(_) if bay.loaded => pb::drive::Status::DriveStatusLoaded,
        Some(_) => pb::drive::Status::DriveStatusIdle,
    }
}

fn joined_tape_uuid(voltag: &Option<String>, voltags: &HashMap<String, Vec<u8>>) -> Vec<u8> {
    voltag
        .as_deref()
        .map(str::trim)
        .filter(|voltag| !voltag.is_empty())
        .and_then(|voltag| voltags.get(voltag).cloned())
        .unwrap_or_default()
}

pub(crate) fn project_drive(
    bay: &DriveBay,
    voltags: &HashMap<String, Vec<u8>>,
    busy_bays: &HashSet<u16>,
    fenced_bays: &HashSet<u16>,
) -> pb::Drive {
    let installed = bay.installed.as_ref();
    let busy = busy_bays.contains(&bay.element_address);
    let fenced = fenced_bays.contains(&bay.element_address);
    pb::Drive {
        element_address: u32::from(bay.element_address),
        drive_serial: installed
            .map(|drive| drive.serial.clone())
            .unwrap_or_default(),
        host_device_path: installed
            .and_then(|drive| drive.sg_path.as_ref())
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        vendor: installed
            .and_then(|drive| drive.vendor.clone())
            .unwrap_or_default(),
        product: installed
            .and_then(|drive| drive.product.clone())
            .unwrap_or_default(),
        loaded_tape_uuid: joined_tape_uuid(&bay.loaded_tape, voltags),
        status: if fenced {
            pb::drive::Status::DriveStatusFenced
        } else {
            drive_status(bay, busy)
        } as i32,
        drive_uuid: Vec::new(),
        cleaning_due: String::new(),
        fenced,
        lifetime_read_bytes: 0,
        lifetime_write_bytes: 0,
        counter_epoch: 0,
        session_id: Vec::new(),
        active_alert_names: Vec::new(),
        tape_io_staging_ring_buffers: 0,
        tape_io_effective_batch_blocks: 0,
        tape_io_gap_p50_us: 0,
        tape_io_gap_p95_us: 0,
        tape_io_gap_max_us: 0,
        tape_io_ioctl_p50_us: 0,
        tape_io_ioctl_p95_us: 0,
        tape_io_ioctl_max_us: 0,
        tape_io_cadence_us: 0,
        tape_io_effective_feed_bytes_per_second: 0,
        loaded_tape_barcode: bay.loaded_tape.clone().unwrap_or_default(),
        mount_age_seconds: 0,
        tape_io_window_feed_bytes_per_second: 0,
    }
}

pub(crate) fn project_slot(slot: &Slot, voltags: &HashMap<String, Vec<u8>>) -> pb::Slot {
    pb::Slot {
        element_address: u32::from(slot.element_address),
        voltag: slot.cartridge.clone().unwrap_or_default(),
        tape_uuid: joined_tape_uuid(&slot.cartridge, voltags),
    }
}

pub(crate) fn project_portal(port: &IePort, voltags: &HashMap<String, Vec<u8>>) -> pb::PortalSlot {
    pb::PortalSlot {
        element_address: u32::from(port.element_address),
        voltag: port.cartridge.clone().unwrap_or_default(),
        tape_uuid: joined_tape_uuid(&port.cartridge, voltags),
        last_direction: pb::portal_slot::Direction::PortalDirectionUnspecified as i32,
    }
}

pub(crate) fn project_library_state(
    library: &Library,
    captured_at: &OffsetDateTime,
    voltags: &HashMap<String, Vec<u8>>,
    busy_bays: &HashSet<u16>,
    fenced_bays: &HashSet<u16>,
) -> pb::LibraryState {
    pb::LibraryState {
        library: Some(project_library(library)),
        drives: library
            .drive_bays
            .iter()
            .map(|bay| project_drive(bay, voltags, busy_bays, fenced_bays))
            .collect(),
        slots: library
            .slots
            .iter()
            .map(|slot| project_slot(slot, voltags))
            .collect(),
        import_export_ports: library
            .ie_ports
            .iter()
            .map(|port| project_portal(port, voltags))
            .collect(),
        last_inventory_at: Some(prost_types::Timestamp {
            seconds: captured_at.unix_timestamp(),
            nanos: captured_at.nanosecond() as i32,
        }),
        managed: "rem".to_string(),
    }
}

pub(crate) fn voltag_uuid_map(index: &CatalogIndex) -> Result<HashMap<String, Vec<u8>>, Status> {
    let mut map = HashMap::new();
    for tape in index
        .list_tapes(None, remanence_state::TapeKindFilter::Data)
        .map_err(crate::status_from_state_error)?
    {
        if let Some(voltag) = tape.voltag {
            let voltag = voltag.trim().to_string();
            if !voltag.is_empty() {
                map.insert(voltag, tape.tape_uuid);
            }
        }
    }
    Ok(map)
}

fn fenced_drive_bays(index: &CatalogIndex, library_serial: &str) -> Result<HashSet<u16>, Status> {
    let mut bays = HashSet::new();
    for drive in index
        .list_drives(true, true)
        .map_err(crate::status_from_state_error)?
    {
        if !drive.fenced {
            continue;
        }
        if drive.last_library_serial.as_deref() != Some(library_serial) {
            continue;
        }
        if let Some(bay) = drive
            .last_element_address
            .and_then(|value| u16::try_from(value).ok())
        {
            bays.insert(bay);
        }
    }
    Ok(bays)
}

fn drive_record_to_proto(record: DriveRecord) -> pb::DriveCatalogEntry {
    pb::DriveCatalogEntry {
        drive_uuid: record.drive_uuid,
        serial: record.serial,
        identity_source: record.identity_source,
        actionable: record.actionable,
        vendor: record.vendor.unwrap_or_default(),
        product: record.product.unwrap_or_default(),
        firmware_rev: record.firmware_rev.unwrap_or_default(),
        managed: record.managed,
        state: record.state,
        cleaning_due: record.cleaning_due,
        fenced: record.fenced,
        first_seen_utc: crate::timestamp_from_rfc3339(record.first_seen_utc.as_str()),
        last_seen_utc: crate::timestamp_from_rfc3339(record.last_seen_utc.as_str()),
        last_library_serial: record.last_library_serial.unwrap_or_default(),
        last_element_address: record
            .last_element_address
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default(),
        purchase_date: record.purchase_date.unwrap_or_default(),
        warranty_until: record.warranty_until.unwrap_or_default(),
        cost: record.cost.unwrap_or_default(),
        notes: record.notes.unwrap_or_default(),
        retired_at_utc: record
            .retired_at_utc
            .as_deref()
            .and_then(crate::timestamp_from_rfc3339),
        retire_reason: record.retire_reason.unwrap_or_default(),
        correlation_rollups: Vec::new(),
    }
}

fn drive_record_to_proto_with_rollups(
    record: DriveRecord,
    rollups: Vec<DriveCorrelationRollupRecord>,
) -> pb::DriveCatalogEntry {
    let mut drive = drive_record_to_proto(record);
    drive.correlation_rollups = rollups
        .into_iter()
        .map(crate::correlation_rollup_to_proto)
        .collect();
    drive
}

fn drive_event_to_proto(record: DriveEventRecord) -> pb::DriveHistoryEvent {
    pb::DriveHistoryEvent {
        event_id: u64::try_from(record.event_id).unwrap_or_default(),
        drive_uuid: record.drive_uuid,
        event_kind: record.event_kind,
        at_utc: crate::timestamp_from_rfc3339(record.at_utc.as_str()),
        library_serial: record.library_serial.unwrap_or_default(),
        element_address: record
            .element_address
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or_default(),
        tape_uuid: record.tape_uuid.unwrap_or_default(),
        detail: record.detail.unwrap_or_default(),
    }
}

fn drive_snapshot_to_proto(record: DriveHealthSnapshotRecord) -> pb::DriveHealthSnapshot {
    pb::DriveHealthSnapshot {
        snapshot_id: u64::try_from(record.snapshot_id).unwrap_or_default(),
        drive_uuid: record.drive_uuid,
        at_utc: crate::timestamp_from_rfc3339(record.at_utc.as_str()),
        trigger: record.trigger,
        session_id: record.session_id.unwrap_or_default(),
        tape_alert_flags: record.tape_alert_flags.unwrap_or_default(),
        write_errors_corrected: record
            .write_errors_corrected
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_default(),
        write_errors_uncorrected: record
            .write_errors_uncorrected
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_default(),
        read_errors_corrected: record
            .read_errors_corrected
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_default(),
        read_errors_uncorrected: record
            .read_errors_uncorrected
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_default(),
        raw_pages: record.raw_pages.unwrap_or_default(),
    }
}

pub(crate) fn alarm_to_proto(record: AlarmRecord) -> pb::Alarm {
    pb::Alarm {
        alarm_id: u64::try_from(record.alarm_id).unwrap_or_default(),
        condition_key: record.condition_key,
        kind: record.kind,
        severity: record.severity,
        state: record.state,
        first_seen_utc: crate::timestamp_from_rfc3339(record.first_seen_utc.as_str()),
        last_seen_utc: crate::timestamp_from_rfc3339(record.last_seen_utc.as_str()),
        acked_by: record.acked_by.unwrap_or_default(),
        acked_at_utc: record
            .acked_at_utc
            .as_deref()
            .and_then(crate::timestamp_from_rfc3339),
        detail: record.detail.unwrap_or_default(),
    }
}

fn actor_label(actor: &remanence_state::AuditActor) -> String {
    match actor {
        remanence_state::AuditActor::System => "system".to_string(),
        remanence_state::AuditActor::User(value) | remanence_state::AuditActor::Service(value) => {
            value.clone()
        }
    }
}

fn validate_iso_date(value: &str, field: &str) -> Result<(), Status> {
    if value.is_empty() {
        return Ok(());
    }
    time::Date::parse(
        value,
        &time::format_description::well_known::Iso8601::DEFAULT,
    )
    .map(|_| ())
    .map_err(|_| Status::invalid_argument(format!("{field} must be YYYY-MM-DD")))
}

fn ensure_mutable_drive(record: &DriveRecord, allow_derived_identity: bool) -> Result<(), Status> {
    if record.identity_source == "Derived" && !allow_derived_identity {
        return Err(Status::failed_precondition(
            "drive identity is Derived; retry with allow_derived_identity",
        ));
    }
    if !record.actionable {
        return Err(Status::failed_precondition(
            "drive is non-actionable because its serial identity is blank or collided",
        ));
    }
    if record.managed == "foreign" {
        let owner = record.last_library_serial.as_deref().unwrap_or("unknown");
        return Err(Status::failed_precondition(format!(
            "foreign drives are read-only to Remanence; drive belongs to library {owner} — clean it via that library's own tooling"
        )));
    }
    Ok(())
}

/// LibraryService read inspection implementation. State-changing and streaming
/// methods remain explicit S6b/S6c stubs so the daemon can register the full
/// service surface now.
#[derive(Clone)]
pub struct LibraryServiceApi {
    pub(crate) state: ApiState,
}

impl LibraryServiceApi {
    fn resolve_library_serial(&self, requested_library_uuid: &[u8]) -> Result<String, Status> {
        if requested_library_uuid.is_empty() {
            return self
                .state
                .default_library_serial
                .as_ref()
                .map(|serial| serial.as_str().to_string())
                .ok_or_else(|| {
                    Status::invalid_argument(
                        "library_uuid is required when config does not name exactly one library",
                    )
                });
        }
        let requested = crate::decode_uuid_bytes(requested_library_uuid, "library_uuid")?;
        let snapshot = self
            .state
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library not found"))?;
        let library_serial = snapshot
            .report
            .libraries
            .iter()
            .find(|library| library_uuid(&library.serial) == requested)
            .map(|library| library.serial.clone())
            .ok_or_else(|| Status::not_found("library not found"))?;
        if self
            .state
            .default_library_serial
            .as_deref()
            .is_some_and(|operated| operated.as_str() != library_serial)
        {
            return Err(Status::failed_precondition(format!(
                "library {library_serial} is discovered but is not operated by this daemon"
            )));
        }
        Ok(library_serial)
    }

    async fn dispatch_robotics(
        &self,
        actor: remanence_state::AuditActor,
        library_uuid: &[u8],
        operation_kind: &'static str,
        action: crate::write_owner::RoboticsAction,
        detail: BTreeMap<String, CborValue>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let library_serial = self.resolve_library_serial(library_uuid)?;
        let pool = self.state.drive_pool()?.clone();
        pool.reserve_all_exclusive()?;
        let index = match self.state.index() {
            Ok(index) => index,
            Err(err) => {
                pool.release_all();
                return Err(err);
            }
        };
        if let Err(err) = crate::ensure_media_readiness_admitted(
            &index,
            operation_kind,
            &library_serial,
            None,
            None,
            true,
        ) {
            pool.release_all();
            return Err(err);
        }
        let operation_id = Uuid::new_v4();
        if let Err(err) = self.state.record_library_request_received(
            actor,
            operation_id,
            operation_kind,
            &library_serial,
            detail,
        ) {
            pool.release_all();
            return Err(err);
        }
        let handle = self.state.operations.register(operation_id, operation_kind);
        match pool
            .changer_tx()
            .try_send(crate::write_owner::ChangerCommand::Robotics {
                library_serial,
                action,
                handle: handle.clone(),
            }) {
            Ok(()) => Ok(Response::new(pb::OperationRef {
                operation_id: operation_id.as_bytes().to_vec(),
            })),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let error = "drive-session owner is busy";
                pool.release_all();
                if let Err(err) =
                    self.state
                        .record_operation_failed(operation_id, operation_kind, error)
                {
                    let audit_error = format!("{error}; audit record failed: {err}");
                    handle.publish_failed(audit_error.as_str(), &[("phase", "dispatch")]);
                } else {
                    handle.publish_failed(error, &[("phase", "dispatch")]);
                }
                Err(Status::failed_precondition(error))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let error = "drive-session owner is stopped";
                pool.release_all();
                if let Err(err) =
                    self.state
                        .record_operation_failed(operation_id, operation_kind, error)
                {
                    let audit_error = format!("{error}; audit record failed: {err}");
                    handle.publish_failed(audit_error.as_str(), &[("phase", "dispatch")]);
                } else {
                    handle.publish_failed(error, &[("phase", "dispatch")]);
                }
                Err(Status::unavailable(error))
            }
        }
    }
}

pub(crate) fn narrow_element(address: u32, field: &str) -> Result<u16, Status> {
    u16::try_from(address)
        .map_err(|_| Status::invalid_argument(format!("{field} exceeds the 16-bit element range")))
}

fn cbor_u16(value: u16) -> CborValue {
    CborValue::Integer(u64::from(value).into())
}

#[tonic::async_trait]
impl pb::library_service_server::LibraryService for LibraryServiceApi {
    async fn list_libraries(
        &self,
        request: Request<()>,
    ) -> Result<Response<pb::ListLibrariesResponse>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let libraries = match self.state.current_library_snapshot() {
            Some(snapshot) => snapshot
                .report
                .libraries
                .iter()
                .map(project_library)
                .collect(),
            None => Vec::new(),
        };
        Ok(Response::new(pb::ListLibrariesResponse { libraries }))
    }

    async fn get_library(
        &self,
        request: Request<pb::GetLibraryRequest>,
    ) -> Result<Response<pb::LibraryState>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let requested =
            crate::decode_uuid_bytes(&request.into_inner().library_uuid, "library_uuid")?;
        let snapshot = self
            .state
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library not found"))?;
        let library = snapshot
            .report
            .libraries
            .iter()
            .find(|library| library_uuid(&library.serial) == requested)
            .ok_or_else(|| Status::not_found("library not found"))?;
        let index = self.state.index()?;
        let voltags = voltag_uuid_map(&index)?;
        let busy_bays = self.state.busy_drive_bays();
        let fenced_bays = fenced_drive_bays(&index, library.serial.as_str())?;
        Ok(Response::new(project_library_state(
            library,
            &snapshot.captured_at,
            &voltags,
            &busy_bays,
            &fenced_bays,
        )))
    }

    async fn refresh_inventory(
        &self,
        request: Request<pb::RefreshInventoryRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "RefreshInventory",
        )?;
        self.dispatch_robotics(
            actor,
            &request.library_uuid,
            "refresh_inventory",
            crate::write_owner::RoboticsAction::Refresh,
            BTreeMap::new(),
        )
        .await
    }

    async fn list_drives(
        &self,
        request: Request<pb::ListDrivesRequest>,
    ) -> Result<Response<pb::ListDrivesResponse>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let request = request.into_inner();
        crate::ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let drives = self
            .state
            .index()?
            .list_drives(request.include_foreign, request.include_retired)
            .map_err(crate::status_from_state_error)?
            .into_iter()
            .map(drive_record_to_proto)
            .collect();
        Ok(Response::new(pb::ListDrivesResponse {
            drives,
            next_page_token: None,
        }))
    }

    async fn get_drive(
        &self,
        request: Request<pb::GetDriveRequest>,
    ) -> Result<Response<pb::DriveCatalogEntry>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let selector = request.into_inner().drive;
        let index = self.state.index()?;
        let drive = index
            .get_drive_by_selector(selector.as_str())
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        let rollups = index
            .drive_tape_correlation_rollups(&drive.drive_uuid)
            .map_err(crate::status_from_state_error)?;
        Ok(Response::new(drive_record_to_proto_with_rollups(
            drive, rollups,
        )))
    }

    async fn get_drive_history(
        &self,
        request: Request<pb::GetDriveHistoryRequest>,
    ) -> Result<Response<pb::GetDriveHistoryResponse>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let request = request.into_inner();
        crate::ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let index = self.state.index()?;
        let drive = index
            .get_drive_by_selector(request.drive.as_str())
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        let events = if request.include_events || !request.include_snapshots {
            index
                .list_drive_events(&drive.drive_uuid)
                .map_err(crate::status_from_state_error)?
                .into_iter()
                .map(drive_event_to_proto)
                .collect()
        } else {
            Vec::new()
        };
        let snapshots = if request.include_snapshots {
            index
                .list_drive_health_snapshots(&drive.drive_uuid)
                .map_err(crate::status_from_state_error)?
                .into_iter()
                .map(drive_snapshot_to_proto)
                .collect()
        } else {
            Vec::new()
        };
        Ok(Response::new(pb::GetDriveHistoryResponse {
            drive: Some(drive_record_to_proto(drive)),
            events,
            snapshots,
            next_page_token: None,
        }))
    }

    async fn annotate_drive(
        &self,
        request: Request<pb::AnnotateDriveRequest>,
    ) -> Result<Response<pb::DriveCatalogEntry>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Write)?;
        let request = request.into_inner();
        crate::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        validate_iso_date(request.purchase_date.as_str(), "purchase_date")?;
        validate_iso_date(request.warranty_until.as_str(), "warranty_until")?;
        let mut index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        ensure_mutable_drive(&drive, request.allow_derived_identity)?;
        let drive_uuid = request.drive_uuid.clone();
        let stored = index
            .annotate_drive(DriveAnnotationInput {
                drive_uuid: request.drive_uuid,
                purchase_date: (!request.purchase_date.is_empty()).then_some(request.purchase_date),
                warranty_until: (!request.warranty_until.is_empty())
                    .then_some(request.warranty_until),
                cost: (!request.cost.is_empty()).then_some(request.cost),
                note: (!request.note.is_empty()).then_some(request.note),
                notes_set: (!request.notes_set.is_empty()).then_some(request.notes_set),
                annotated_at_utc: None,
            })
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        let mut detail = BTreeMap::new();
        detail.insert(
            "drive_uuid".to_string(),
            CborValue::Bytes(drive_uuid.clone()),
        );
        for (key, value) in [
            ("purchase_date", stored.purchase_date.as_ref()),
            ("warranty_until", stored.warranty_until.as_ref()),
            ("cost", stored.cost.as_ref()),
            ("notes", stored.notes.as_ref()),
        ] {
            if let Some(value) = value {
                detail.insert(key.to_string(), CborValue::Text(value.clone()));
            }
        }
        self.state.record_drive_audit(
            actor,
            remanence_state::AuditEvent::DriveAnnotated,
            drive_uuid.as_slice(),
            detail,
        )?;
        Ok(Response::new(drive_record_to_proto(stored)))
    }

    async fn retire_drive(
        &self,
        request: Request<pb::RetireDriveRequest>,
    ) -> Result<Response<pb::RetireDriveResponse>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Lifecycle)?;
        let request = request.into_inner();
        crate::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        if !request.i_understand_fleet_removal_is_permanent {
            return Err(Status::failed_precondition(
                "RetireDrive requires i_understand_fleet_removal_is_permanent=true",
            ));
        }
        if request.reason.trim().is_empty() {
            return Err(Status::invalid_argument("reason must not be empty"));
        }
        let mut index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        ensure_mutable_drive(&drive, request.allow_derived_identity)?;
        if let Some(element) = drive
            .last_element_address
            .and_then(|value| u16::try_from(value).ok())
        {
            if self.state.busy_drive_bays().contains(&element) {
                return Err(Status::failed_precondition(
                    "drive has an active session or reserved operation",
                ));
            }
        }
        let outcome = index
            .retire_drive(&request.drive_uuid, request.reason.as_str())
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        if outcome.newly_retired {
            let mut detail = BTreeMap::new();
            detail.insert(
                "drive_uuid".to_string(),
                CborValue::Bytes(request.drive_uuid.clone()),
            );
            detail.insert("reason".to_string(), CborValue::Text(request.reason));
            self.state.record_drive_audit(
                actor,
                remanence_state::AuditEvent::DriveRetired,
                request.drive_uuid.as_slice(),
                detail,
            )?;
        }
        Ok(Response::new(pb::RetireDriveResponse {
            drive: Some(drive_record_to_proto(outcome.drive)),
            newly_retired: outcome.newly_retired,
        }))
    }

    async fn poll_drive(
        &self,
        request: Request<pb::PollDriveRequest>,
    ) -> Result<Response<pb::DriveHealthSnapshot>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        let drive = self
            .state
            .index()?
            .get_drive_by_selector(request.drive.as_str())
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        ensure_mutable_drive(&drive, request.allow_derived_identity)?;
        if drive.managed != "rem" {
            return Err(Status::failed_precondition(
                "manual drive poll is only available for managed drives",
            ));
        }
        if drive.state != "active" {
            return Err(Status::failed_precondition("cannot poll a retired drive"));
        }
        let bay = drive
            .last_element_address
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| Status::failed_precondition("drive has no current bay"))?;
        let snapshot = self
            .state
            .drive_pool()?
            .poll_drive_health(bay, drive.drive_uuid)
            .await?;
        Ok(Response::new(drive_snapshot_to_proto(snapshot)))
    }

    async fn clean_drive(
        &self,
        request: Request<pb::CleanDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        let index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        ensure_mutable_drive(&drive, request.allow_derived_identity)?;
        if drive.managed != "rem" {
            return Err(Status::failed_precondition(
                "cleaning is only available for managed drives",
            ));
        }
        if drive.state != "active" {
            return Err(Status::failed_precondition("cannot clean a retired drive"));
        }
        let library_serial = drive.last_library_serial.clone().ok_or_else(|| {
            Status::failed_precondition("drive has no current library assignment")
        })?;
        let mut detail = BTreeMap::new();
        detail.insert(
            "drive_uuid".to_string(),
            CborValue::Bytes(request.drive_uuid.clone()),
        );
        detail.insert(
            "component".to_string(),
            CborValue::Text("cleaning".to_string()),
        );
        let action = crate::write_owner::RoboticsAction::Clean {
            drive_uuid: request.drive_uuid.clone(),
            trigger: "manual".to_string(),
        };
        self.dispatch_robotics(
            actor,
            &library_uuid(library_serial.as_str()),
            "clean_drive",
            action,
            detail,
        )
        .await
    }

    async fn list_alarms(
        &self,
        request: Request<pb::ListAlarmsRequest>,
    ) -> Result<Response<pb::ListAlarmsResponse>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let request = request.into_inner();
        crate::ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let alarms = self
            .state
            .index()?
            .list_alarms(request.include_cleared)
            .map_err(crate::status_from_state_error)?
            .into_iter()
            .map(alarm_to_proto)
            .collect();
        Ok(Response::new(pb::ListAlarmsResponse {
            alarms,
            next_page_token: None,
        }))
    }

    async fn ack_alarm(
        &self,
        request: Request<pb::AckAlarmRequest>,
    ) -> Result<Response<pb::Alarm>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "AckAlarm")?;
        let mut index = self.state.index_write()?;
        let alarm = index
            .ack_alarm(request.condition_key.as_str(), actor_label(&actor).as_str())
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("alarm not found or already cleared"))?;
        self.state
            .record_alarm_acked(actor, request.condition_key.as_str())?;
        Ok(Response::new(alarm_to_proto(alarm)))
    }

    async fn get_live_status(
        &self,
        request: Request<pb::GetLiveStatusRequest>,
    ) -> Result<Response<pb::GetLiveStatusResponse>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        let response = self.state.live_status_response().await?;
        Ok(Response::new(response))
    }

    async fn move_medium(
        &self,
        request: Request<pb::MoveMediumRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "MoveMedium")?;
        let src = narrow_element(request.source_element_address, "source_element_address")?;
        let dst = narrow_element(
            request.destination_element_address,
            "destination_element_address",
        )?;
        let mut detail = BTreeMap::new();
        detail.insert("src".to_string(), cbor_u16(src));
        detail.insert("dst".to_string(), cbor_u16(dst));
        self.dispatch_robotics(
            actor,
            &request.library_uuid,
            "move_medium",
            crate::write_owner::RoboticsAction::Move { src, dst },
            detail,
        )
        .await
    }

    async fn load_drive(
        &self,
        request: Request<pb::LoadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "LoadDrive")?;
        let slot = narrow_element(request.slot_element_address, "slot_element_address")?;
        let bay = narrow_element(request.drive_element_address, "drive_element_address")?;
        let mut detail = BTreeMap::new();
        detail.insert("slot".to_string(), cbor_u16(slot));
        detail.insert("bay".to_string(), cbor_u16(bay));
        self.dispatch_robotics(
            actor,
            &request.library_uuid,
            "load_drive",
            crate::write_owner::RoboticsAction::Load {
                slot,
                bay,
                wait_ready: !request.no_wait,
            },
            detail,
        )
        .await
    }

    async fn resume_media_readiness(
        &self,
        request: Request<pb::ResumeMediaReadinessRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::OperationControl)?;
        let request = request.into_inner();
        if request.timeout_seconds == 0 || request.poll_interval_seconds == 0 {
            return Err(Status::invalid_argument(
                "media-readiness timeout and poll interval must be non-zero",
            ));
        }
        let operation_uuid = crate::decode_uuid_bytes(
            request.operation_id.as_slice(),
            "media_readiness_operation_id",
        )?;
        let operation_id = Uuid::from_bytes(operation_uuid);
        let record = self
            .state
            .index()?
            .media_readiness_operation(operation_id)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("media-readiness operation not found"))?;
        const RESUMABLE_STATES: &[&str] = &[
            "planned",
            "pre_ready_loading",
            "media_initializing",
            "becoming_ready",
            "target_busy",
            "unit_attention",
        ];
        if !RESUMABLE_STATES.contains(&record.state.as_str()) {
            return Err(Status::failed_precondition(format!(
                "media-readiness operation {operation_id} is state {} and cannot be resumed",
                record.state
            )));
        }
        if self
            .state
            .default_library_serial
            .as_deref()
            .is_some_and(|serial| serial.as_str() != record.library_serial)
        {
            return Err(Status::failed_precondition(format!(
                "media-readiness operation {operation_id} belongs to library {}, which this daemon does not operate",
                record.library_serial
            )));
        }
        let bay = u16::try_from(record.drive_element).map_err(|_| {
            Status::failed_precondition(format!(
                "media-readiness operation {operation_id} has invalid drive element {}",
                record.drive_element
            ))
        })?;
        let snapshot = self
            .state
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library snapshot unavailable"))?;
        let library = snapshot
            .report
            .library(record.library_serial.as_str())
            .ok_or_else(|| Status::not_found("media-readiness library not found"))?;
        let drive_bay = library
            .drive_bays
            .iter()
            .find(|candidate| candidate.element_address == bay)
            .ok_or_else(|| Status::not_found("media-readiness drive bay not found"))?;
        if let Some(expected) = record.drive_serial.as_deref() {
            let actual = drive_bay
                .installed
                .as_ref()
                .map(|drive| drive.serial.as_str());
            if actual != Some(expected) {
                return Err(Status::failed_precondition(format!(
                    "media-readiness operation {operation_id} expected drive serial {expected}, snapshot has {}",
                    actual.unwrap_or("(none)")
                )));
            }
        }
        if let Some(expected) = record.barcode.as_deref() {
            if drive_bay.loaded_tape.as_deref() != Some(expected) {
                return Err(Status::failed_precondition(format!(
                    "media-readiness operation {operation_id} expected barcode {expected} in drive 0x{bay:04x}, snapshot has {}",
                    drive_bay.loaded_tape.as_deref().unwrap_or("(none)")
                )));
            }
        }

        let pool = self.state.drive_pool()?.clone();
        let reservation = pool.reserve_drive(bay)?;
        let drive_tx = pool.drive_tx(bay)?;
        let handle = self
            .state
            .operations
            .register(operation_id, "media_readiness_wait");
        let family = if record
            .media_generation
            .is_some_and(|generation| generation >= 9)
        {
            remanence_library::MediaFamily::Lto9OrLater
        } else {
            remanence_library::MediaFamily::Unknown
        };
        let command = crate::write_owner::DriveCommand::WaitReady {
            operation_id,
            family,
            options: remanence_library::MediaReadinessWaitOptions {
                wait: true,
                timeout: Duration::from_secs(u64::from(request.timeout_seconds)),
                poll_interval: Duration::from_secs(u64::from(request.poll_interval_seconds)),
            },
            handle: handle.clone(),
            reservation,
        };
        match drive_tx.try_send(command) {
            Ok(()) => Ok(Response::new(pb::OperationRef {
                operation_id: operation_id.as_bytes().to_vec(),
            })),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                handle.publish_failed("drive-session owner is busy", &[("phase", "dispatch")]);
                Err(Status::failed_precondition("drive-session owner is busy"))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                handle.publish_failed("drive-session owner is stopped", &[("phase", "dispatch")]);
                Err(Status::unavailable("drive-session owner is stopped"))
            }
        }
    }

    async fn unload_drive(
        &self,
        request: Request<pb::UnloadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor = crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "UnloadDrive")?;
        let bay = narrow_element(request.drive_element_address, "drive_element_address")?;
        let destination = if request.destination_slot_address == 0 {
            None
        } else {
            Some(narrow_element(
                request.destination_slot_address,
                "destination_slot_address",
            )?)
        };
        let mut detail = BTreeMap::new();
        detail.insert("bay".to_string(), cbor_u16(bay));
        if let Some(dst) = destination {
            detail.insert("destination".to_string(), cbor_u16(dst));
        }
        self.dispatch_robotics(
            actor,
            &request.library_uuid,
            "unload_drive",
            crate::write_owner::RoboticsAction::Unload { bay, destination },
            detail,
        )
        .await
    }

    async fn import_element(
        &self,
        request: Request<pb::ImportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "ImportElement")?;
        Err(Status::unimplemented("ImportElement is S6b"))
    }

    async fn export_element(
        &self,
        request: Request<pb::ExportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "ExportElement")?;
        Err(Status::unimplemented("ExportElement is S6b"))
    }

    type StreamLibraryEventsStream =
        Pin<Box<dyn Stream<Item = Result<pb::LibraryEvent, Status>> + Send + 'static>>;

    async fn stream_library_events(
        &self,
        request: Request<pb::StreamLibraryEventsRequest>,
    ) -> Result<Response<Self::StreamLibraryEventsStream>, Status> {
        crate::authorize_request(&request, crate::AuthPermission::Read)?;
        Err(Status::unimplemented("StreamLibraryEvents is S6c"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use remanence_library::scsi::{DeviceType, Inquiry};
    use remanence_library::{
        DiscoveryReport, DriveBay, ElementException, ElementLayout, IdentitySource, IePort,
        InstalledDrive, Library, Slot,
    };
    use remanence_state::{DriveObservationInput, FileAuditLog};

    use super::*;
    use crate::pb::library_service_server::LibraryService as _;

    fn mk_inquiry() -> Inquiry {
        Inquiry {
            device_type: DeviceType::MediumChanger,
            peripheral_qualifier: 0,
            removable: true,
            version: 7,
            response_data_format: 2,
            additional_length: 31,
            vendor: *b"HPE     ",
            product: *b"MSL3040         ",
            revision: *b"6.40",
        }
    }

    fn mk_library() -> Library {
        Library {
            serial: "DEC418146K_LL02".to_string(),
            changer_sg: PathBuf::from("/dev/sg7"),
            changer_sysfs: PathBuf::from("/sys/test"),
            changer_inquiry: mk_inquiry(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 1,
                drive_count: 1,
                slot_start: 0x03e8,
                slot_count: 1,
                ie_start: 0x10,
                ie_count: 1,
            },
            drive_bays: vec![DriveBay {
                element_address: 1,
                accessible: true,
                exception: None,
                installed: Some(InstalledDrive {
                    serial: "8031BDC7D1".to_string(),
                    identity_source: IdentitySource::DvcidInline,
                    vendor: Some("HPE".to_string()),
                    product: Some("Ultrium 9-SCSI".to_string()),
                    revision: Some("R1.0".to_string()),
                    sg_path: Some(PathBuf::from("/dev/sg8")),
                    sysfs_path: None,
                }),
                loaded: true,
                loaded_tape: Some("S30002L9".to_string()),
                source_slot: Some(0x040a),
            }],
            slots: vec![Slot {
                element_address: 0x03e9,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("CLNU01L9".to_string()),
            }],
            ie_ports: vec![IePort {
                element_address: 0x10,
                accessible: true,
                exception: None,
                full: false,
                cartridge: None,
                import_enabled: true,
                export_enabled: true,
            }],
        }
    }

    fn test_index() -> CatalogIndex {
        let dir = std::env::temp_dir().join(format!("remanence-api-lib-{}", Uuid::new_v4()));
        CatalogIndex::open(dir.join("state.sqlite")).expect("open test index")
    }

    fn state_with_snapshot() -> ApiState {
        let mut state = ApiState::new(test_index());
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::LibrarySnapshot {
                report: DiscoveryReport {
                    libraries: vec![mk_library()],
                    warnings: Vec::new(),
                },
                captured_at: OffsetDateTime::UNIX_EPOCH,
            },
        ))));
        state
    }

    fn observe_test_drive(
        index: &mut CatalogIndex,
        serial: &str,
        identity_source: &str,
        bay: i64,
    ) -> Vec<u8> {
        index
            .observe_drive(DriveObservationInput {
                serial: serial.to_string(),
                identity_source: identity_source.to_string(),
                vendor: Some("HPE".to_string()),
                product: Some("Ultrium 9-SCSI".to_string()),
                firmware_rev: Some("R1.0".to_string()),
                managed: "rem".to_string(),
                library_serial: Some("DEC418146K_LL02".to_string()),
                element_address: Some(bay),
                observed_at_utc: Some("2026-07-04T00:00:00Z".to_string()),
            })
            .expect("observe test drive")
            .drive_uuid
    }

    #[test]
    fn library_uuid_is_deterministic_and_serial_specific() {
        assert_eq!(
            library_uuid("DEC418146K_LL02"),
            library_uuid("DEC418146K_LL02")
        );
        assert_ne!(library_uuid("AAA"), library_uuid("BBB"));
    }

    #[test]
    fn project_library_maps_inquiry_and_uuid() {
        let projected = project_library(&mk_library());
        assert_eq!(projected.library_serial, "DEC418146K_LL02");
        assert_eq!(projected.vendor, "HPE");
        assert_eq!(projected.product, "MSL3040");
        assert_eq!(projected.product_revision, "6.40");
        assert_eq!(
            projected.library_uuid,
            library_uuid("DEC418146K_LL02").to_vec()
        );
    }

    #[test]
    fn drive_status_covers_each_case() {
        let drive = |sg_path: Option<PathBuf>| InstalledDrive {
            serial: "S".to_string(),
            identity_source: IdentitySource::DvcidInline,
            vendor: None,
            product: None,
            revision: None,
            sg_path,
            sysfs_path: None,
        };
        let bay = |installed: Option<InstalledDrive>, loaded: bool| DriveBay {
            element_address: 1,
            accessible: true,
            exception: None,
            installed,
            loaded,
            loaded_tape: None,
            source_slot: None,
        };
        assert_eq!(
            drive_status(&bay(None, false), false),
            pb::drive::Status::DriveStatusUnreachable
        );
        assert_eq!(
            drive_status(&bay(Some(drive(None)), true), true),
            pb::drive::Status::DriveStatusUnreachable
        );
        assert_eq!(
            drive_status(
                &bay(Some(drive(Some(PathBuf::from("/dev/sg8")))), true),
                false
            ),
            pb::drive::Status::DriveStatusLoaded
        );
        assert_eq!(
            drive_status(
                &bay(Some(drive(Some(PathBuf::from("/dev/sg8")))), false),
                false
            ),
            pb::drive::Status::DriveStatusIdle
        );
        assert_eq!(
            drive_status(
                &bay(Some(drive(Some(PathBuf::from("/dev/sg8")))), true),
                true
            ),
            pb::drive::Status::DriveStatusBusy
        );
    }

    #[test]
    fn joined_tape_uuid_hits_and_misses() {
        let mut voltags = HashMap::new();
        voltags.insert("S30002L9".to_string(), vec![9u8; 16]);
        assert_eq!(
            joined_tape_uuid(&Some("S30002L9".to_string()), &voltags),
            vec![9u8; 16]
        );
        assert!(joined_tape_uuid(&Some("NOPE".to_string()), &voltags).is_empty());
        assert!(joined_tape_uuid(&None, &voltags).is_empty());
    }

    #[test]
    fn project_library_state_projects_and_joins() {
        let mut voltags = HashMap::new();
        voltags.insert("S30002L9".to_string(), vec![9u8; 16]);
        voltags.insert("CLNU01L9".to_string(), vec![7u8; 16]);
        let state = project_library_state(
            &mk_library(),
            &OffsetDateTime::UNIX_EPOCH,
            &voltags,
            &HashSet::new(),
            &HashSet::new(),
        );
        assert_eq!(
            state.library.expect("library").library_serial,
            "DEC418146K_LL02"
        );
        assert_eq!(state.drives.len(), 1);
        assert_eq!(state.drives[0].loaded_tape_uuid, vec![9u8; 16]);
        assert_eq!(state.drives[0].loaded_tape_barcode, "S30002L9");
        assert_eq!(
            state.drives[0].status,
            pb::drive::Status::DriveStatusLoaded as i32
        );
        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.slots[0].voltag, "CLNU01L9");
        assert_eq!(state.slots[0].tape_uuid, vec![7u8; 16]);
        assert_eq!(state.import_export_ports.len(), 1);
        assert!(state.import_export_ports[0].tape_uuid.is_empty());
        assert_eq!(
            state.import_export_ports[0].last_direction,
            pb::portal_slot::Direction::PortalDirectionUnspecified as i32
        );
        assert_eq!(state.last_inventory_at.expect("timestamp").seconds, 0);
    }

    #[test]
    fn project_library_state_marks_busy_drive() {
        let mut voltags = HashMap::new();
        voltags.insert("S30002L9".to_string(), vec![9u8; 16]);
        let state = project_library_state(
            &mk_library(),
            &OffsetDateTime::UNIX_EPOCH,
            &voltags,
            &HashSet::from([1]),
            &HashSet::new(),
        );

        assert_eq!(
            state.drives[0].status,
            pb::drive::Status::DriveStatusBusy as i32
        );
    }

    #[test]
    fn project_library_state_marks_fenced_drive() {
        let state = project_library_state(
            &mk_library(),
            &OffsetDateTime::UNIX_EPOCH,
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::from([1]),
        );

        assert_eq!(
            state.drives[0].status,
            pb::drive::Status::DriveStatusFenced as i32
        );
        assert!(state.drives[0].fenced);
    }

    #[test]
    fn project_library_state_does_not_publish_element_exception_fields() {
        let mut library = mk_library();
        library.drive_bays[0].exception = Some(ElementException {
            asc: 0x04,
            ascq: 0x01,
        });
        library.slots[0].exception = Some(ElementException {
            asc: 0x3b,
            ascq: 0x12,
        });
        library.ie_ports[0].exception = Some(ElementException {
            asc: 0x00,
            ascq: 0x00,
        });

        let state = project_library_state(
            &library,
            &OffsetDateTime::UNIX_EPOCH,
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
        );

        let debug = format!("{state:?}");
        assert!(
            !debug.to_ascii_lowercase().contains("exception"),
            "daemon proto state must not expose RES exception evidence: {debug}"
        );
        assert_eq!(state.drives[0].element_address, 1);
        assert_eq!(state.slots[0].element_address, 0x03e9);
        assert_eq!(state.import_export_ports[0].element_address, 0x10);
    }

    #[tokio::test]
    async fn list_libraries_projects_snapshot() {
        let response = state_with_snapshot()
            .library_service()
            .list_libraries(Request::new(()))
            .await
            .expect("list libraries")
            .into_inner();
        assert_eq!(response.libraries.len(), 1);
        assert_eq!(response.libraries[0].library_serial, "DEC418146K_LL02");
    }

    #[tokio::test]
    async fn list_libraries_empty_without_snapshot() {
        let response = ApiState::new(test_index())
            .library_service()
            .list_libraries(Request::new(()))
            .await
            .expect("list libraries")
            .into_inner();
        assert!(response.libraries.is_empty());
    }

    #[tokio::test]
    async fn get_library_returns_state_for_known_uuid() {
        let response = state_with_snapshot()
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
            }))
            .await
            .expect("get library")
            .into_inner();
        assert_eq!(
            response.library.expect("library").library_serial,
            "DEC418146K_LL02"
        );
        assert_eq!(response.drives.len(), 1);
        assert_eq!(response.slots.len(), 1);
        assert!(response.drives[0].loaded_tape_uuid.is_empty());
        assert_eq!(response.last_inventory_at.expect("timestamp").seconds, 0);
    }

    #[tokio::test]
    async fn get_library_unknown_uuid_is_not_found() {
        let err = state_with_snapshot()
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: vec![0u8; 16],
            }))
            .await
            .expect_err("unknown uuid");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_library_bad_uuid_is_invalid_argument() {
        let err = state_with_snapshot()
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: vec![1, 2, 3],
            }))
            .await
            .expect_err("bad uuid");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_library_missing_snapshot_is_not_found() {
        let err = ApiState::new(test_index())
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: vec![0u8; 16],
            }))
            .await
            .expect_err("missing snapshot");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_library_succeeds_while_owner_busy() {
        let mut state = state_with_snapshot();
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        state.drive_pool = Some(crate::write_owner::DrivePool::new(
            changer_tx,
            HashMap::new(),
            Arc::new(HashMap::from([(1, AtomicBool::new(true))])),
        ));
        let response = state
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
            }))
            .await;
        assert!(
            response.is_ok(),
            "inventory reads must succeed while a session is busy"
        );
        assert_eq!(
            response.expect("response").into_inner().drives[0].status,
            pb::drive::Status::DriveStatusBusy as i32
        );
    }

    #[test]
    fn narrow_element_accepts_u16_and_rejects_overflow() {
        assert_eq!(narrow_element(0x0400, "x").unwrap(), 0x0400);
        assert_eq!(narrow_element(0, "x").unwrap(), 0);
        assert_eq!(
            narrow_element(0x1_0000, "x").unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn refresh_inventory_without_owner_is_unavailable() {
        let err = state_with_snapshot()
            .library_service()
            .refresh_inventory(Request::new(pb::RefreshInventoryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                idempotency_key: None,
            }))
            .await
            .expect_err("no owner");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn refresh_inventory_rejects_readonly_role_before_owner_check() {
        let mut request = Request::new(pb::RefreshInventoryRequest {
            library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
            idempotency_key: None,
        });
        request
            .metadata_mut()
            .insert("x-remanence-role", "readonly".parse().unwrap());

        let err = state_with_snapshot()
            .library_service()
            .refresh_inventory(request)
            .await
            .expect_err("readonly role must not dispatch robotics");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn refresh_inventory_rejects_unenforced_idempotency_before_owner_check() {
        let err = state_with_snapshot()
            .library_service()
            .refresh_inventory(Request::new(pb::RefreshInventoryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                idempotency_key: Some(pb::IdempotencyKey {
                    value: Uuid::new_v4().as_bytes().to_vec(),
                }),
            }))
            .await
            .expect_err("unenforced idempotency key must not dispatch robotics");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn refresh_inventory_empty_library_uuid_uses_default_library() {
        let mut state = state_with_snapshot();
        state.default_library_serial = Some(Arc::new("DEC418146K_LL02".to_string()));
        let err = state
            .library_service()
            .refresh_inventory(Request::new(pb::RefreshInventoryRequest {
                library_uuid: Vec::new(),
                idempotency_key: None,
            }))
            .await
            .expect_err("no owner");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[test]
    fn robotics_resolution_rejects_discovered_non_operated_library() {
        let mut state = state_with_snapshot();
        state.default_library_serial = Some(Arc::new("DEC418146K_LL02".to_string()));
        let mut second_library = mk_library();
        second_library.serial = "D2LIB".to_string();
        let current = state.current_library_snapshot().expect("library snapshot");
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::LibrarySnapshot {
                report: DiscoveryReport {
                    libraries: vec![current.report.libraries[0].clone(), second_library],
                    warnings: Vec::new(),
                },
                captured_at: current.captured_at,
            },
        ))));

        let error = state
            .library_service()
            .resolve_library_serial(&library_uuid("D2LIB"))
            .expect_err("a discovered second library must not alias the operated changer");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            error.message(),
            "library D2LIB is discovered but is not operated by this daemon"
        );
    }

    #[tokio::test]
    async fn poll_drive_requires_robotics_permission_before_drive_pool_lookup() {
        let mut index = test_index();
        observe_test_drive(&mut index, "DRV-POLL", "DvcidAndInquiry", 0x0100);
        let mut request = Request::new(pb::PollDriveRequest {
            drive: "DRV-POLL".to_string(),
            allow_derived_identity: false,
        });
        request
            .metadata_mut()
            .insert("x-remanence-role", "readonly".parse().unwrap());

        let err = ApiState::new(index)
            .library_service()
            .poll_drive(request)
            .await
            .expect_err("readonly role must not poll drive health");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn poll_drive_rejects_derived_identity_without_opt_in() {
        let mut index = test_index();
        observe_test_drive(&mut index, "DRV-POLL-DERIVED", "Derived", 0x0100);

        let err = ApiState::new(index)
            .library_service()
            .poll_drive(Request::new(pb::PollDriveRequest {
                drive: "DRV-POLL-DERIVED".to_string(),
                allow_derived_identity: false,
            }))
            .await
            .expect_err("derived identity must reject before drive pool lookup");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("Derived"));
    }

    #[tokio::test]
    async fn retire_drive_rejects_missing_ack_before_lookup() {
        let err = ApiState::new(test_index())
            .library_service()
            .retire_drive(Request::new(pb::RetireDriveRequest {
                drive_uuid: Uuid::new_v4().as_bytes().to_vec(),
                reason: "removed from fleet".to_string(),
                i_understand_fleet_removal_is_permanent: false,
                allow_derived_identity: false,
            }))
            .await
            .expect_err("missing ack must reject");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("i_understand"));
    }

    #[tokio::test]
    async fn retire_drive_rejects_derived_identity_without_opt_in() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-DERIVED", "Derived", 0x0100);
        let err = ApiState::new(index)
            .library_service()
            .retire_drive(Request::new(pb::RetireDriveRequest {
                drive_uuid,
                reason: "removed from fleet".to_string(),
                i_understand_fleet_removal_is_permanent: true,
                allow_derived_identity: false,
            }))
            .await
            .expect_err("derived identity must reject without opt-in");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("Derived"));
    }

    #[tokio::test]
    async fn retire_drive_rejects_busy_drive_bay() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-BUSY", "DvcidAndInquiry", 0x0101);
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
        let mut state = ApiState::new(index);
        state.drive_pool = Some(crate::write_owner::DrivePool::new(
            changer_tx,
            HashMap::new(),
            reservations,
        ));

        let err = state
            .library_service()
            .retire_drive(Request::new(pb::RetireDriveRequest {
                drive_uuid,
                reason: "removed from fleet".to_string(),
                i_understand_fleet_removal_is_permanent: true,
                allow_derived_identity: false,
            }))
            .await
            .expect_err("busy bay must reject retire");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert!(err.message().contains("active session"));
    }

    #[tokio::test]
    async fn ack_alarm_rejects_cleared_alarm_without_audit_event() {
        let mut index = test_index();
        index
            .raise_alarm("no-cln-cart:mainlib", "no-cln-cart", "critical", Some("{}"))
            .expect("raise alarm");
        index
            .clear_alarm("no-cln-cart:mainlib")
            .expect("clear alarm")
            .expect("cleared row");
        let state = ApiState::new(index);

        let err = state
            .library_service()
            .ack_alarm(Request::new(pb::AckAlarmRequest {
                condition_key: "no-cln-cart:mainlib".to_string(),
                idempotency_key: None,
            }))
            .await
            .expect_err("cleared alarm must not ack");
        assert_eq!(err.code(), tonic::Code::NotFound);
        let audit_records = if state.audit_dir.exists() {
            FileAuditLog::replay(state.audit_dir.as_ref()).expect("replay audit")
        } else {
            Vec::new()
        };
        assert!(
            audit_records
                .iter()
                .all(|record| record.event != remanence_state::AuditEvent::AlarmAcked),
            "cleared alarm ack must not append AlarmAcked audit: {audit_records:?}"
        );
    }

    #[tokio::test]
    async fn get_live_status_projects_drive_states_counters_and_alarms() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-LIVE", "DvcidAndInquiry", 1);
        index
            .begin_clean_run(&drive_uuid, "DEC418146K_LL02", "manual", None)
            .expect("begin clean run");
        index
            .raise_alarm(
                "drive-cleaning-abnormal-frequency:DEC418146K_LL02",
                "drive-cleaning-abnormal-frequency",
                "warning",
                Some("{}"),
            )
            .expect("raise alarm");
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());
        state.record_drive_read_bytes(Some(drive_uuid.as_slice()), 1_024);
        state.record_drive_write_bytes(Some(drive_uuid.as_slice()), 2_048);
        let counters = state.drive_counters(drive_uuid.as_slice());
        counters.configure_tape_io(4, 16);
        counters.record_tape_io_diagnostics(remanence_library::PipelinedWriteDiagnostics {
            gap_p50_us: 100,
            gap_p95_us: 250,
            gap_max_us: 400,
            cadence_us: 1_100,
            effective_feed_bytes_per_second: 300_000_000,
            ..Default::default()
        });

        let response = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("live status")
            .into_inner();

        assert_eq!(response.daemon_epoch, state.daemon_epoch);
        assert_eq!(response.libraries.len(), 1);
        assert_eq!(response.alarms.len(), 1);
        let library = &response.libraries[0];
        assert_eq!(library.managed, "rem");
        assert_eq!(library.drives.len(), 1);
        let drive = &library.drives[0];
        assert_eq!(drive.drive_uuid, drive_uuid);
        assert_eq!(drive.lifetime_read_bytes, 1_024);
        assert_eq!(drive.lifetime_write_bytes, 2_048);
        assert_eq!(drive.status, pb::drive::Status::DriveStatusCleaning as i32);
        assert_eq!(drive.active_alert_names, vec!["cleaning".to_string()]);
        assert_ne!(drive.counter_epoch, 0);
        assert_eq!(drive.tape_io_staging_ring_buffers, 4);
        assert_eq!(drive.tape_io_effective_batch_blocks, 16);
        assert_eq!(drive.tape_io_gap_p95_us, 250);
        assert_eq!(drive.tape_io_cadence_us, 1_100);
        assert_eq!(drive.tape_io_effective_feed_bytes_per_second, 300_000_000);
        assert_eq!(drive.loaded_tape_barcode, "S30002L9");
        assert!(drive.tape_io_window_feed_bytes_per_second > 0);

        state.record_drive_write_bytes(Some(drive_uuid.as_slice()), 4_096);
        let cached = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("cached live status")
            .into_inner();
        let cached_drive = &cached.libraries[0].drives[0];
        assert_eq!(cached_drive.lifetime_write_bytes, 6_144);
        assert!(cached_drive.tape_io_window_feed_bytes_per_second > 0);
    }

    #[tokio::test]
    async fn get_live_status_projects_advisory_bay_assignment_and_atomic_enforcement() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-ASSIGN", "DvcidAndInquiry", 1);
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(1, AtomicBool::new(false))]));
        let pool = crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations);
        state.drive_pool = Some(pool.clone());

        let idle = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("idle live status")
            .into_inner();
        assert_eq!(idle.drive_assignments.len(), 1);
        let assignment = &idle.drive_assignments[0];
        assert_eq!(assignment.library_serial, "DEC418146K_LL02");
        assert_eq!(assignment.bay, 1);
        assert_eq!(assignment.drive_uuid, drive_uuid);
        assert_eq!(
            assignment.state,
            pb::drive_assignment::State::DriveAssignmentStateIdle as i32
        );
        assert!(assignment.current_session_id.is_empty());

        let _reservation = pool.reserve_drive(1).expect("reserve bay for live session");
        let session_id = Uuid::new_v4();
        let tape_uuid = Uuid::new_v4();
        pool.record_session(
            session_id,
            crate::write_owner::MountedSession {
                bay: 1,
                library_serial: "DEC418146K_LL02".to_string(),
                barcode: Some("S30002L9".to_string()),
                home_slot: Some(0x040a),
                tape_uuid: *tape_uuid.as_bytes(),
                drive_uuid: Some(drive_uuid.clone()),
            },
        );

        let active = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("active live status")
            .into_inner();
        let assignment = &active.drive_assignments[0];
        assert_eq!(
            assignment.state,
            pb::drive_assignment::State::DriveAssignmentStateActive as i32
        );
        assert_eq!(assignment.current_session_id, session_id.as_bytes());
        assert_eq!(assignment.loaded_tape_uuid, tape_uuid.as_bytes());

        let second = pool
            .reserve_drive(1)
            .expect_err("atomic bay reservation must still reject a second open");
        assert_eq!(second.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn get_live_status_marks_fenced_drives() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-FENCED", "DvcidAndInquiry", 1);
        index
            .set_drive_fenced(drive_uuid.as_slice(), true)
            .expect("fence drive");
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());

        let response = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("live status")
            .into_inner();

        let drive = &response.libraries[0].drives[0];
        assert_eq!(drive.status, pb::drive::Status::DriveStatusFenced as i32);
        assert!(drive.fenced);
    }

    #[tokio::test]
    async fn live_status_counter_epoch_changes_across_state_restarts() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-EPOCH", "DvcidAndInquiry", 1);

        let mut first = state_with_snapshot();
        first.index_path = Arc::new(index.path().to_path_buf());
        first.record_drive_read_bytes(Some(drive_uuid.as_slice()), 1);
        let first_epoch = first
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("live status")
            .into_inner()
            .libraries[0]
            .drives[0]
            .counter_epoch;

        let mut second = state_with_snapshot();
        second.index_path = Arc::new(index.path().to_path_buf());
        let second_epoch = second
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("live status")
            .into_inner()
            .libraries[0]
            .drives[0]
            .counter_epoch;

        assert_ne!(first_epoch, 0);
        assert_ne!(second_epoch, 0);
        assert_ne!(first_epoch, second_epoch);
    }

    #[tokio::test]
    async fn move_medium_rejects_overflow_element_before_dispatch() {
        let err = state_with_snapshot()
            .library_service()
            .move_medium(Request::new(pb::MoveMediumRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                source_element_address: 0x1_0000,
                destination_element_address: 0x0100,
                idempotency_key: None,
            }))
            .await
            .expect_err("overflow");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn move_medium_unknown_library_is_not_found() {
        let err = state_with_snapshot()
            .library_service()
            .move_medium(Request::new(pb::MoveMediumRequest {
                library_uuid: vec![0u8; 16],
                source_element_address: 0x0400,
                destination_element_address: 0x0100,
                idempotency_key: None,
            }))
            .await
            .expect_err("unknown library");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn unload_drive_zero_destination_resolves_without_owner_error_path() {
        let err = state_with_snapshot()
            .library_service()
            .unload_drive(Request::new(pb::UnloadDriveRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                drive_element_address: 0x0001,
                destination_slot_address: 0,
                idempotency_key: None,
            }))
            .await
            .expect_err("no owner");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }
}
