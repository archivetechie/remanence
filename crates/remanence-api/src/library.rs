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

use crate::api_state::ApiState;
use crate::pb;

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

/// Join a changer-reported voltag to a catalogued tape UUID.
///
/// Returns `None` when there is no cartridge, when the voltag is blank, or when
/// the catalog holds no tape under it. Those are all "no identity available",
/// which is what the caller must be told -- previously this returned an empty
/// vector for every one of them, and an empty vector is also what a caller sees
/// for a cartridge whose identity simply was not looked up.
fn joined_tape_uuid(
    voltag: &Option<String>,
    voltags: &HashMap<String, Vec<u8>>,
) -> Option<Vec<u8>> {
    voltag
        .as_deref()
        .map(str::trim)
        .filter(|voltag| !voltag.is_empty())
        .and_then(|voltag| voltags.get(voltag).cloned())
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
        open_epoch_start: None,
        open_epoch_end: None,
        // Known here, because the changer just told us: the bay, whether it is
        // fenced, and whatever a host-side INQUIRY managed to resolve.
        element_address: Some(u32::from(bay.element_address)),
        drive_serial: installed.map(|drive| drive.serial.clone()),
        host_device_path: installed
            .and_then(|drive| drive.sg_path.as_ref())
            .map(|path| path.display().to_string()),
        vendor: installed.and_then(|drive| drive.vendor.clone()),
        product: installed.and_then(|drive| drive.product.clone()),
        loaded_tape_uuid: joined_tape_uuid(&bay.loaded_tape, voltags),
        loaded_tape_barcode: bay.loaded_tape.clone(),
        fenced: Some(fenced),
        status: if fenced {
            pb::drive::Status::DriveStatusFenced
        } else {
            drive_status(bay, busy)
        } as i32,
        // Every row is born without catalog identity: the changer knows the bay,
        // the catalog knows who lives in it, and the join happens later. The
        // state below says which answer the join got, so an absent identity is
        // never mistaken for an empty one.
        drive_uuid: None,
        catalog_state: pb::DriveCatalogState::Uncatalogued as i32,

        // NOT DETERMINED AT THIS LAYER. This function projects what the CHANGER
        // reports; catalog history and transfer telemetry arrive later, from
        // other sources, and may never arrive at all.
        //
        // These were previously seeded with 0 and an empty string. That is the
        // single most misleading thing this file has ever done: a drive that had
        // never been asked about its lifetime bytes reported having written
        // zero, and a bay with no transfer in progress reported a flawless 0us
        // I/O gap. Nothing downstream could tell those apart from real readings,
        // and nothing downstream was at fault for failing to.
        //
        // `None` says the only true thing: nobody has asked yet.
        cleaning_due: None,
        lifetime_read_bytes: None,
        lifetime_write_bytes: None,
        counter_epoch: None,
        session_id: None,
        active_alert_names: Vec::new(),
        mount_age_seconds: None,
        tape_io_staging_ring_buffers: None,
        tape_io_effective_batch_blocks: None,
        tape_io_gap_p50_us: None,
        tape_io_gap_p95_us: None,
        tape_io_gap_max_us: None,
        tape_io_ioctl_p50_us: None,
        tape_io_ioctl_p95_us: None,
        tape_io_ioctl_max_us: None,
        tape_io_cadence_us: None,
        tape_io_effective_feed_bytes_per_second: None,
        tape_io_window_feed_bytes_per_second: None,
    }
}

pub(crate) fn project_slot(slot: &Slot, voltags: &HashMap<String, Vec<u8>>) -> pb::Slot {
    pb::Slot {
        element_address: u32::from(slot.element_address),
        // Absent means the slot is empty, or holds a cartridge whose label the
        // changer could not read. Both are "no barcode available"; neither is a
        // cartridge whose barcode is the empty string.
        voltag: slot.cartridge.clone(),
        tape_uuid: joined_tape_uuid(&slot.cartridge, voltags),
    }
}

pub(crate) fn project_portal(port: &IePort, voltags: &HashMap<String, Vec<u8>>) -> pb::PortalSlot {
    pb::PortalSlot {
        element_address: u32::from(port.element_address),
        voltag: port.cartridge.clone(),
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
    changer_wait: Option<String>,
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
        changer_wait,
    }
}

pub(crate) fn voltag_uuid_map(index: &CatalogIndex) -> Result<HashMap<String, Vec<u8>>, Status> {
    let mut map = HashMap::new();
    for tape in index
        .list_tapes(None, remanence_state::TapeKindFilter::Data)
        .map_err(crate::startup_media_readiness::status_from_state_error)?
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

fn drive_record_to_proto(record: DriveRecord) -> pb::DriveCatalogEntry {
    pb::DriveCatalogEntry {
        drive_uuid: record.drive_uuid,
        serial: record.serial,
        identity_source: record.identity_source,
        actionable: record.actionable,
        // The record already carries these as Option, straight from nullable
        // columns. They used to be flattened here, at the last possible moment
        // before the wire -- the absence survived storage and retrieval intact
        // and was destroyed on the way out.
        vendor: record.vendor,
        product: record.product,
        firmware_rev: record.firmware_rev,
        managed: record.managed,
        state: record.state,
        cleaning_due: record.cleaning_due,
        fenced: record.fenced,
        first_seen_utc: crate::catalog_conversion::timestamp_from_rfc3339(
            record.first_seen_utc.as_str(),
        ),
        last_seen_utc: crate::catalog_conversion::timestamp_from_rfc3339(
            record.last_seen_utc.as_str(),
        ),
        last_library_serial: record.last_library_serial,
        // NOTE: a None here means either "never seen in a library" or "the
        // stored bay number would not fit a u32", and those are not the same
        // fact -- the second is corrupt data, not an absence. Separating them
        // needs an error path this projection does not have; tracked as
        // remaining work rather than silently widened here.
        last_element_address: record
            .last_element_address
            .and_then(|value| u32::try_from(value).ok()),
        purchase_date: record.purchase_date,
        warranty_until: record.warranty_until,
        cost: record.cost,
        notes: record.notes,
        retired_at_utc: record
            .retired_at_utc
            .as_deref()
            .and_then(crate::catalog_conversion::timestamp_from_rfc3339),
        retire_reason: record.retire_reason,
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
        .map(crate::catalog_conversion::correlation_rollup_to_proto)
        .collect();
    drive
}

fn drive_event_to_proto(record: DriveEventRecord) -> pb::DriveHistoryEvent {
    pb::DriveHistoryEvent {
        event_id: u64::try_from(record.event_id).unwrap_or_default(),
        drive_uuid: record.drive_uuid,
        event_kind: record.event_kind,
        at_utc: crate::catalog_conversion::timestamp_from_rfc3339(record.at_utc.as_str()),
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
    let session_uuid = record
        .session_id
        .as_deref()
        .and_then(|session_id| uuid::Uuid::parse_str(session_id).ok())
        .map(|session_id| session_id.as_bytes().to_vec());
    pb::DriveHealthSnapshot {
        snapshot_id: u64::try_from(record.snapshot_id).unwrap_or_default(),
        drive_uuid: record.drive_uuid,
        at_utc: crate::catalog_conversion::timestamp_from_rfc3339(record.at_utc.as_str()),
        trigger: record.trigger,
        session_id: record.session_id,
        tape_alert_flags: record.tape_alert_flags,
        // Zero corrected errors is a fine drive. An unreadable log page is a
        // drive nobody can vouch for. These four used to report the second as
        // the first, which is the most flattering possible way to be wrong.
        write_errors_corrected: record
            .write_errors_corrected
            .and_then(|value| u64::try_from(value).ok()),
        write_errors_uncorrected: record
            .write_errors_uncorrected
            .and_then(|value| u64::try_from(value).ok()),
        read_errors_corrected: record
            .read_errors_corrected
            .and_then(|value| u64::try_from(value).ok()),
        read_errors_uncorrected: record
            .read_errors_uncorrected
            .and_then(|value| u64::try_from(value).ok()),
        raw_pages: record.raw_pages,
        session_uuid,
    }
}

pub(crate) fn alarm_to_proto(record: AlarmRecord) -> pb::Alarm {
    pb::Alarm {
        alarm_id: u64::try_from(record.alarm_id).unwrap_or_default(),
        condition_key: record.condition_key,
        kind: record.kind,
        severity: record.severity,
        state: record.state,
        first_seen_utc: crate::catalog_conversion::timestamp_from_rfc3339(
            record.first_seen_utc.as_str(),
        ),
        last_seen_utc: crate::catalog_conversion::timestamp_from_rfc3339(
            record.last_seen_utc.as_str(),
        ),
        acked_by: record.acked_by.unwrap_or_default(),
        acked_at_utc: record
            .acked_at_utc
            .as_deref()
            .and_then(crate::catalog_conversion::timestamp_from_rfc3339),
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
        let requested =
            crate::catalog_conversion::decode_uuid_bytes(requested_library_uuid, "library_uuid")?;
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
        if !self.state.operates_library(&library_serial) {
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
        let changer = pool.changer_tx(&library_serial)?;
        pool.reserve_library_exclusive(&library_serial)?;
        let index = match self.state.index() {
            Ok(index) => index,
            Err(err) => {
                pool.release_library(&library_serial);
                return Err(err);
            }
        };
        if let Err(err) = crate::startup_media_readiness::ensure_media_readiness_admitted(
            &index,
            operation_kind,
            &library_serial,
            None,
            None,
            true,
        ) {
            pool.release_library(&library_serial);
            return Err(err);
        }
        let operation_id = Uuid::new_v4();
        pool.set_library_holder(&library_serial, operation_kind, operation_id);
        if let Err(err) = self.state.record_library_request_received(
            actor,
            operation_id,
            operation_kind,
            &library_serial,
            detail,
        ) {
            pool.release_library(&library_serial);
            return Err(err);
        }
        let mut handle = self.state.operations.register(operation_id, operation_kind);
        if matches!(
            action,
            crate::write_owner::RoboticsAction::Load {
                wait_ready: true,
                ..
            }
        ) {
            handle = handle.with_readiness_lifetime(
                self.state.readiness_shutdown.clone(),
                pool.readiness_wake.clone(),
            );
        }
        match changer.try_reserve_owned() {
            Ok(permit) => {
                tokio::spawn(async move {
                    handle.publish_state(
                        pb::OperationState::Running,
                        &[("phase", "prepare_removal")],
                    );
                    let source_bay = match &action {
                        crate::write_owner::RoboticsAction::Unload { bay, .. } => Some(*bay),
                        crate::write_owner::RoboticsAction::Move { src, .. } => Some(*src),
                        _ => None,
                    };
                    if let Some(bay) = source_bay {
                        if let Ok(drive) =
                            pool.drive_tx(&crate::drive_pool::DriveKey::new(&library_serial, bay))
                        {
                            let (reply, received) = tokio::sync::oneshot::channel();
                            if drive
                                .send(crate::write_owner::DriveCommand::PrepareRemoval {
                                    reply,
                                    progress: Some(handle.clone()),
                                })
                                .await
                                .is_ok()
                            {
                                if let Ok(Err(error)) = received.await {
                                    tracing::error!(%error, "epoch close fails; removing cartridge");
                                }
                            }
                        }
                    }
                    permit.send(crate::write_owner::ChangerCommand::Robotics {
                        library_serial,
                        action,
                        handle,
                    });
                });
                Ok(Response::new(pb::OperationRef {
                    operation_id: operation_id.as_bytes().to_vec(),
                }))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let error = "drive-session owner is busy";
                pool.release_library(&library_serial);
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
                pool.release_library(&library_serial);
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
    async fn release_tape_io_fence(
        &self,
        request: Request<pb::ReleaseTapeIoFenceRequest>,
    ) -> Result<Response<pb::TapeIoFence>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::OperationControl)?;
        let actor = crate::auth::actor_from_request(&request);
        let request = request.into_inner();
        if request.acknowledgment.trim().is_empty() {
            return Err(Status::invalid_argument("acknowledgment must not be empty"));
        }
        let state = self.state.clone();
        let record = tokio::task::spawn_blocking(move || {
            let _guard = state
                .audit_append_lock
                .lock()
                .map_err(|_| Status::internal("audit append lock poisoned"))?;
            remanence_state::fence::release(
                &mut state.index_write()?,
                &state.audit_dir,
                &request.quarantine_id,
                &request.acknowledgment,
                actor,
            )
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("tape-I/O fence not found"))
        })
        .await
        .map_err(|error| Status::internal(error.to_string()))??;
        if let Some(pool) = &self.state.drive_pool {
            pool.readiness_wake.notify.notify_one();
        }
        Ok(Response::new(
            crate::daemon_catalog_services::tape_io_fence_to_proto(record),
        ))
    }

    async fn list_libraries(
        &self,
        request: Request<()>,
    ) -> Result<Response<pb::ListLibrariesResponse>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
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
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
        let requested = crate::catalog_conversion::decode_uuid_bytes(
            &request.into_inner().library_uuid,
            "library_uuid",
        )?;
        // GetLibrary promises the current inventory snapshot. Unlike the
        // polling-oriented GetLiveStatus surface, it must not reuse a response
        // cached before a just-completed changer refresh.
        let response = self.state.fresh_live_status_response().await?;
        let library = response
            .libraries
            .into_iter()
            .find(|state| {
                state
                    .library
                    .as_ref()
                    .is_some_and(|library| library.library_uuid.as_slice() == requested)
            })
            .ok_or_else(|| Status::not_found("library not found"))?;
        Ok(Response::new(library))
    }

    async fn refresh_inventory(
        &self,
        request: Request<pb::RefreshInventoryRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
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
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
        let request = request.into_inner();
        crate::catalog_request::ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let drives = self
            .state
            .index()?
            .list_drives(request.include_foreign, request.include_retired)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
        let selector = request.into_inner().drive;
        let index = self.state.index()?;
        let drive = index
            .get_drive_by_selector(selector.as_str())
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        let rollups = index
            .drive_tape_correlation_rollups(&drive.drive_uuid)
            .map_err(crate::startup_media_readiness::status_from_state_error)?;
        Ok(Response::new(drive_record_to_proto_with_rollups(
            drive, rollups,
        )))
    }

    async fn get_drive_history(
        &self,
        request: Request<pb::GetDriveHistoryRequest>,
    ) -> Result<Response<pb::GetDriveHistoryResponse>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
        let request = request.into_inner();
        crate::catalog_request::ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let index = self.state.index()?;
        let drive = index
            .get_drive_by_selector(request.drive.as_str())
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        let events = if request.include_events || !request.include_snapshots {
            index
                .list_drive_events(&drive.drive_uuid)
                .map_err(crate::startup_media_readiness::status_from_state_error)?
                .into_iter()
                .map(drive_event_to_proto)
                .collect()
        } else {
            Vec::new()
        };
        let snapshots = if request.include_snapshots {
            index
                .list_drive_health_snapshots(&drive.drive_uuid)
                .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        let actor = crate::auth::authorize_request(&request, crate::auth::AuthPermission::Write)?;
        let request = request.into_inner();
        crate::catalog_conversion::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        validate_iso_date(request.purchase_date.as_str(), "purchase_date")?;
        validate_iso_date(request.warranty_until.as_str(), "warranty_until")?;
        let mut index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Lifecycle)?;
        let request = request.into_inner();
        crate::catalog_conversion::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        // Retirement no longer carries a permanence acknowledgement: it is
        // reversible through ReinstateDrive. The reason remains mandatory —
        // it is what the audit trail and the reinstatement record cite.
        if request.reason.trim().is_empty() {
            return Err(Status::invalid_argument("reason must not be empty"));
        }
        let mut index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        ensure_mutable_drive(&drive, request.allow_derived_identity)?;
        if let Some(element) = drive
            .last_element_address
            .and_then(|value| u16::try_from(value).ok())
        {
            if drive
                .last_library_serial
                .as_deref()
                .is_some_and(|serial| self.state.busy_drive_bays(serial).contains(&element))
            {
                return Err(Status::failed_precondition(
                    "drive has an active session or reserved operation",
                ));
            }
        }
        let outcome = index
            .retire_drive(&request.drive_uuid, request.reason.as_str())
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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

    async fn reinstate_drive(
        &self,
        request: Request<pb::ReinstateDriveRequest>,
    ) -> Result<Response<pb::ReinstateDriveResponse>, Status> {
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Lifecycle)?;
        let request = request.into_inner();
        crate::catalog_conversion::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        if request.reason.trim().is_empty() {
            return Err(Status::invalid_argument("reason must not be empty"));
        }
        let mut index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        ensure_mutable_drive(&drive, request.allow_derived_identity)?;
        let outcome = index
            .reinstate_drive(&request.drive_uuid, request.reason.as_str())
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("drive not found"))?;
        if outcome.newly_reinstated {
            let mut detail = BTreeMap::new();
            detail.insert(
                "drive_uuid".to_string(),
                CborValue::Bytes(request.drive_uuid.clone()),
            );
            detail.insert("reason".to_string(), CborValue::Text(request.reason));
            self.state.record_drive_audit(
                actor,
                remanence_state::AuditEvent::DriveReinstated,
                request.drive_uuid.as_slice(),
                detail,
            )?;
        }
        Ok(Response::new(pb::ReinstateDriveResponse {
            drive: Some(drive_record_to_proto(outcome.drive)),
            newly_reinstated: outcome.newly_reinstated,
        }))
    }

    async fn poll_drive(
        &self,
        request: Request<pb::PollDriveRequest>,
    ) -> Result<Response<pb::DriveHealthSnapshot>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        let drive = self
            .state
            .index()?
            .get_drive_by_selector(request.drive.as_str())
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        let library_serial = drive.last_library_serial.clone().ok_or_else(|| {
            Status::failed_precondition("drive has no current library assignment")
        })?;
        let snapshot = self
            .state
            .drive_pool()?
            .poll_drive_health(
                &crate::drive_pool::DriveKey::new(library_serial, bay),
                drive.drive_uuid,
            )
            .await?;
        Ok(Response::new(drive_snapshot_to_proto(snapshot)))
    }

    async fn clean_drive(
        &self,
        request: Request<pb::CleanDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::decode_uuid_bytes(&request.drive_uuid, "drive_uuid")?;
        let index = self.state.index_write()?;
        let drive = index
            .get_drive_by_uuid(&request.drive_uuid)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
        let request = request.into_inner();
        crate::catalog_request::ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let alarms = self
            .state
            .index()?
            .list_alarms(request.include_cleared)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
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
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "AckAlarm",
        )?;
        let mut index = self.state.index_write()?;
        let alarm = index
            .ack_alarm(request.condition_key.as_str(), actor_label(&actor).as_str())
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("alarm not found or already cleared"))?;
        self.state
            .record_alarm_acked(actor, request.condition_key.as_str())?;
        Ok(Response::new(alarm_to_proto(alarm)))
    }

    async fn get_live_status(
        &self,
        request: Request<pb::GetLiveStatusRequest>,
    ) -> Result<Response<pb::GetLiveStatusResponse>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
        let response = self.state.live_status_response().await?;
        Ok(Response::new(response))
    }

    async fn move_medium(
        &self,
        request: Request<pb::MoveMediumRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "MoveMedium",
        )?;
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
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "LoadDrive",
        )?;
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
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::OperationControl)?;
        let request = request.into_inner();
        if request.timeout_seconds == 0 || request.poll_interval_seconds == 0 {
            return Err(Status::invalid_argument(
                "media-readiness timeout and poll interval must be non-zero",
            ));
        }
        let operation_uuid = crate::catalog_conversion::decode_uuid_bytes(
            request.operation_id.as_slice(),
            "media_readiness_operation_id",
        )?;
        let operation_id = Uuid::from_bytes(operation_uuid);
        self.state
            .start_readiness_wait(
                operation_id,
                request
                    .timeout_seconds
                    .max(crate::readiness_wait::DEFAULT_READY_TIMEOUT_SECONDS),
                request.poll_interval_seconds,
                "client",
                "session_open",
            )
            .map(Response::new)
    }

    async fn unload_drive(
        &self,
        request: Request<pb::UnloadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor =
            crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "UnloadDrive",
        )?;
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
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "ImportElement",
        )?;
        Err(Status::unimplemented("ImportElement is S6b"))
    }

    async fn export_element(
        &self,
        request: Request<pb::ExportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Robotics)?;
        let request = request.into_inner();
        crate::catalog_conversion::reject_unimplemented_idempotency(
            request.idempotency_key.as_ref(),
            "ExportElement",
        )?;
        Err(Status::unimplemented("ExportElement is S6b"))
    }

    type StreamLibraryEventsStream =
        Pin<Box<dyn Stream<Item = Result<pb::LibraryEvent, Status>> + Send + 'static>>;

    async fn stream_library_events(
        &self,
        request: Request<pb::StreamLibraryEventsRequest>,
    ) -> Result<Response<Self::StreamLibraryEventsStream>, Status> {
        crate::auth::authorize_request(&request, crate::auth::AuthPermission::Read)?;
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

    #[test]
    fn drive_snapshot_uses_canonical_binary_session_uuid() {
        let session_id = uuid::Uuid::from_u128(0x1234);
        let snapshot = drive_snapshot_to_proto(DriveHealthSnapshotRecord {
            snapshot_id: 1,
            drive_uuid: vec![1; 16],
            at_utc: "2026-07-21T00:00:00Z".to_string(),
            trigger: "manual".to_string(),
            session_id: Some(session_id.to_string()),
            tape_alert_flags: None,
            write_errors_corrected: None,
            write_errors_uncorrected: None,
            read_errors_corrected: None,
            read_errors_uncorrected: None,
            raw_pages: None,
        });

        assert_eq!(snapshot.session_uuid, Some(session_id.as_bytes().to_vec()));
        assert_eq!(snapshot.session_id, Some(session_id.to_string()));
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
        state.default_library_serial = Some(Arc::new("DEC418146K_LL02".to_string()));
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::live_status::LibrarySnapshot {
                report: DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
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
        observe_test_drive_in_library(index, serial, identity_source, "DEC418146K_LL02", bay)
    }

    fn observe_test_drive_in_library(
        index: &mut CatalogIndex,
        serial: &str,
        identity_source: &str,
        library_serial: &str,
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
                library_serial: Some(library_serial.to_string()),
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
            Some(vec![9u8; 16])
        );
        assert!(joined_tape_uuid(&Some("NOPE".to_string()), &voltags).is_none());
        assert!(joined_tape_uuid(&None, &voltags).is_none());
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
            None,
        );
        assert_eq!(
            state.library.expect("library").library_serial,
            "DEC418146K_LL02"
        );
        assert_eq!(state.drives.len(), 1);
        assert_eq!(state.drives[0].loaded_tape_uuid, Some(vec![9u8; 16]));
        assert_eq!(
            state.drives[0].loaded_tape_barcode.as_deref(),
            Some("S30002L9")
        );
        assert_eq!(
            state.drives[0].status,
            pb::drive::Status::DriveStatusLoaded as i32
        );
        assert_eq!(state.slots.len(), 1);
        assert_eq!(state.slots[0].voltag.as_deref(), Some("CLNU01L9"));
        assert_eq!(state.slots[0].tape_uuid, Some(vec![7u8; 16]));
        assert_eq!(state.import_export_ports.len(), 1);
        assert!(state.import_export_ports[0].tape_uuid.is_none());
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
            None,
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
            None,
        );

        assert_eq!(
            state.drives[0].status,
            pb::drive::Status::DriveStatusFenced as i32
        );
        assert_eq!(state.drives[0].fenced, Some(true));
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
            None,
        );

        let debug = format!("{state:?}");
        assert!(
            !debug.to_ascii_lowercase().contains("exception"),
            "daemon proto state must not expose RES exception evidence: {debug}"
        );
        assert_eq!(state.drives[0].element_address, Some(1));
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
    async fn get_library_returns_catalog_enriched_state_for_known_uuid() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "8031BDC7D1", "DvcidInline", 1);
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());
        let response = state
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
        assert!(response.drives[0].loaded_tape_uuid.is_none());
        assert_eq!(
            response.drives[0].drive_uuid.as_deref(),
            Some(drive_uuid.as_slice())
        );
        assert_eq!(
            response.drives[0].catalog_state,
            pb::DriveCatalogState::Cataloged as i32
        );
        assert_eq!(response.last_inventory_at.expect("timestamp").seconds, 0);
    }

    #[tokio::test]
    async fn get_library_bypasses_a_live_status_response_cached_before_refresh() {
        let state = state_with_snapshot();
        state
            .live_status_response()
            .await
            .expect("seed live-status cache");

        let mut refreshed_library = mk_library();
        refreshed_library.drive_bays[0]
            .installed
            .as_mut()
            .expect("refreshed drive")
            .serial = "REFRESHED-DRIVE".to_string();
        *state
            .library_snapshot
            .as_ref()
            .expect("library snapshot")
            .write()
            .expect("snapshot write lock") = Arc::new(crate::live_status::LibrarySnapshot {
            report: DiscoveryReport {
                unattached_drives: Vec::new(),
                complete: true,
                incomplete_evidence: Vec::new(),
                libraries: vec![refreshed_library],
                warnings: Vec::new(),
            },
            captured_at: OffsetDateTime::UNIX_EPOCH,
        });

        let response = state
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
            }))
            .await
            .expect("get refreshed library")
            .into_inner();

        assert_eq!(
            response.drives[0].drive_serial.as_deref(),
            Some("REFRESHED-DRIVE")
        );
    }

    /// Regression: identical changer bay numbers must not cross library boundaries.
    #[tokio::test]
    async fn get_library_scopes_same_bay_drive_identity_to_requested_library() {
        let first_library = mk_library();
        let mut second_library = mk_library();
        second_library.serial = "SECOND-LIBRARY".to_string();
        second_library.drive_bays[0]
            .installed
            .as_mut()
            .expect("second library drive")
            .serial = "SECOND-DRIVE".to_string();

        let mut index = test_index();
        let first_drive_uuid = observe_test_drive_in_library(
            &mut index,
            "8031BDC7D1",
            "DvcidInline",
            first_library.serial.as_str(),
            1,
        );
        let second_drive_uuid = observe_test_drive_in_library(
            &mut index,
            "SECOND-DRIVE",
            "DvcidInline",
            second_library.serial.as_str(),
            1,
        );
        assert_ne!(first_drive_uuid, second_drive_uuid);

        let mut state = ApiState::new(index);
        state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
            crate::live_status::LibrarySnapshot {
                report: DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
                    libraries: vec![first_library, second_library],
                    warnings: Vec::new(),
                },
                captured_at: OffsetDateTime::UNIX_EPOCH,
            },
        ))));

        let response = state
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: library_uuid("SECOND-LIBRARY").to_vec(),
            }))
            .await
            .expect("get second library")
            .into_inner();

        assert_eq!(
            response.library.expect("library").library_serial,
            "SECOND-LIBRARY"
        );
        assert_eq!(response.drives.len(), 1);
        assert_eq!(
            response.drives[0].drive_serial.as_deref(),
            Some("SECOND-DRIVE")
        );
        assert_eq!(
            response.drives[0].drive_uuid.as_deref(),
            Some(second_drive_uuid.as_slice())
        );
        assert_ne!(
            response.drives[0].drive_uuid.as_deref(),
            Some(first_drive_uuid.as_slice())
        );
        assert_eq!(
            response.drives[0].catalog_state,
            pb::DriveCatalogState::Cataloged as i32
        );
    }

    #[tokio::test]
    async fn get_library_explains_a_genuinely_uncatalogued_bay() {
        let response = state_with_snapshot()
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
            }))
            .await
            .expect("get library")
            .into_inner();

        assert!(response.drives[0].drive_uuid.is_none());
        assert_eq!(
            response.drives[0].catalog_state,
            pb::DriveCatalogState::Uncatalogued as i32
        );
    }

    #[tokio::test]
    async fn get_library_preserves_retired_drive_identity() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "8031BDC7D1", "DvcidInline", 1);
        index
            .retire_drive(drive_uuid.as_slice(), "get-library regression")
            .expect("retire drive")
            .expect("drive exists");
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());

        let response = state
            .library_service()
            .get_library(Request::new(pb::GetLibraryRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
            }))
            .await
            .expect("get library")
            .into_inner();

        assert_eq!(response.drives[0].drive_uuid, Some(drive_uuid));
        assert_eq!(
            response.drives[0].catalog_state,
            pb::DriveCatalogState::Retired as i32
        );
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

    /// The robotics RPC returns before epoch closure and publishes that work as progress.
    #[tokio::test]
    async fn robotics_returns_operation_while_epoch_close_waits() {
        let mut state = state_with_snapshot();
        let (changer_tx, mut changer_rx) = tokio::sync::mpsc::channel(1);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
            changer_tx,
            HashMap::from([(1, drive_tx)]),
            Arc::new(HashMap::from([(1, AtomicBool::new(false))])),
        );
        state.drive_pool = Some(pool.clone());
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            state.library_service().dispatch_robotics(
                remanence_state::AuditActor::System,
                &[],
                "unload_drive",
                crate::write_owner::RoboticsAction::Unload {
                    bay: 1,
                    destination: None,
                },
                BTreeMap::new(),
            ),
        )
        .await
        .expect("RPC does not wait for epoch close")
        .unwrap()
        .into_inner();
        let id = Uuid::from_slice(&result.operation_id).unwrap();
        let crate::write_owner::DriveCommand::PrepareRemoval { reply, progress } =
            drive_rx.recv().await.unwrap()
        else {
            panic!("removal preparation precedes robotics");
        };
        assert!(changer_rx.try_recv().is_err());
        assert_eq!(
            state
                .operations
                .latest(&id)
                .unwrap()
                .progress
                .get("phase")
                .map(String::as_str),
            Some("prepare_removal")
        );
        progress
            .unwrap()
            .publish_state(pb::OperationState::Running, &[("phase", "epoch_close")]);
        assert_eq!(
            state
                .operations
                .latest(&id)
                .unwrap()
                .progress
                .get("phase")
                .map(String::as_str),
            Some("epoch_close")
        );
        reply.send(Ok(())).unwrap();
        assert!(matches!(
            changer_rx.recv().await.unwrap(),
            crate::write_owner::ChangerCommand::Robotics { .. }
        ));
        pool.release_library("DEC418146K_LL02");
    }

    #[tokio::test]
    async fn get_library_succeeds_while_owner_busy() {
        let mut state = state_with_snapshot();
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        state.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
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
            crate::live_status::LibrarySnapshot {
                report: DiscoveryReport {
                    unattached_drives: Vec::new(),
                    complete: true,
                    incomplete_evidence: Vec::new(),
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

    /// The permanence acknowledgement is gone — retirement is reversible — but
    /// the reason is not: it is what the audit trail and any later
    /// reinstatement record cite.
    #[tokio::test]
    async fn retire_drive_rejects_empty_reason_before_lookup() {
        let err = ApiState::new(test_index())
            .library_service()
            .retire_drive(Request::new(pb::RetireDriveRequest {
                drive_uuid: Uuid::new_v4().as_bytes().to_vec(),
                reason: "   ".to_string(),
                allow_derived_identity: false,
            }))
            .await
            .expect_err("an empty reason must reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("reason"));
    }

    #[tokio::test]
    async fn reinstate_drive_rejects_empty_reason_before_lookup() {
        let err = ApiState::new(test_index())
            .library_service()
            .reinstate_drive(Request::new(pb::ReinstateDriveRequest {
                drive_uuid: Uuid::new_v4().as_bytes().to_vec(),
                reason: String::new(),
                allow_derived_identity: false,
            }))
            .await
            .expect_err("an empty reason must reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(err.message().contains("reason"));
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
        state.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
            changer_tx,
            HashMap::new(),
            reservations,
        ));

        let err = state
            .library_service()
            .retire_drive(Request::new(pb::RetireDriveRequest {
                drive_uuid,
                reason: "removed from fleet".to_string(),
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
        assert_eq!(drive.drive_uuid.as_deref(), Some(drive_uuid.as_slice()));
        assert_eq!(drive.lifetime_read_bytes, Some(1_024));
        assert_eq!(drive.lifetime_write_bytes, Some(2_048));
        assert_eq!(drive.status, pb::drive::Status::DriveStatusCleaning as i32);
        assert_eq!(drive.active_alert_names, vec!["cleaning".to_string()]);
        assert!(drive.counter_epoch.is_some_and(|epoch| epoch != 0));
        assert_eq!(drive.tape_io_staging_ring_buffers, Some(4));
        assert_eq!(drive.tape_io_effective_batch_blocks, Some(16));
        assert_eq!(drive.tape_io_gap_p95_us, Some(250));
        assert_eq!(drive.tape_io_cadence_us, Some(1_100));
        assert_eq!(
            drive.tape_io_effective_feed_bytes_per_second,
            Some(300_000_000)
        );
        assert_eq!(drive.loaded_tape_barcode.as_deref(), Some("S30002L9"));
        assert!(drive.tape_io_window_feed_bytes_per_second > Some(0));

        state.record_drive_write_bytes(Some(drive_uuid.as_slice()), 4_096);
        let cached = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("cached live status")
            .into_inner();
        let cached_drive = &cached.libraries[0].drives[0];
        assert_eq!(cached_drive.lifetime_write_bytes, Some(6_144));
        assert!(cached_drive.tape_io_window_feed_bytes_per_second > Some(0));
    }

    #[tokio::test]
    async fn get_live_status_projects_advisory_bay_assignment_and_atomic_enforcement() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-ASSIGN", "DvcidAndInquiry", 1);
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(1, AtomicBool::new(false))]));
        let pool = crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
            changer_tx,
            HashMap::new(),
            reservations,
        );
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
        assert_eq!(
            assignment.drive_uuid.as_deref(),
            Some(drive_uuid.as_slice())
        );
        assert_eq!(
            assignment.state,
            pb::drive_assignment::State::DriveAssignmentStateIdle as i32
        );
        assert!(assignment.current_session_id.is_none());

        let _reservation = pool
            .reserve_drive(&crate::drive_pool::DriveKey::new("DEC418146K_LL02", 1))
            .expect("reserve bay for live session");
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
        assert_eq!(
            assignment.current_session_id,
            Some(session_id.as_bytes().to_vec())
        );
        assert_eq!(
            assignment.loaded_tape_uuid,
            Some(tape_uuid.as_bytes().to_vec())
        );

        let second = pool
            .reserve_drive(&crate::drive_pool::DriveKey::new("DEC418146K_LL02", 1))
            .expect_err("atomic bay reservation must still reject a second open");
        assert_eq!(second.code(), tonic::Code::FailedPrecondition);
    }

    /// A pool open that needs the robot is refused by another drive's
    /// readiness fence, and a client that then resumes that fence finds the
    /// drive held by the first waiter. The refusal names the library-wide
    /// serialization, not just a busy drive.
    #[tokio::test]
    async fn resume_of_a_held_fenced_drive_names_the_robot_serialization() {
        let mut state = state_with_snapshot();
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let (drive_tx, _drive_rx) = tokio::sync::mpsc::channel(1);
        let pool = crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
            changer_tx,
            HashMap::from([(1, drive_tx)]),
            Arc::new(HashMap::from([(1, AtomicBool::new(false))])),
        );
        state.drive_pool = Some(pool.clone());
        let operation_id = Uuid::from_u128(0x5eed);
        state
            .index_write()
            .expect("open writable catalog")
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id,
                run_id: None,
                endpoint: crate::drive_pool::DriveKey::new("DEC418146K_LL02", 1),
                changer_sg: None,
                drive_sg: None,
                drive_serial: Some("8031BDC7D1".to_string()),
                barcode: Some("S30002L9".to_string()),
                source_slot: Some(0x040a),
                media_generation: Some(9),
                phase: "session_open_short_probe".to_string(),
                state: "becoming_ready".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record session-open readiness fence");
        state
            .index_write()
            .expect("open writable catalog")
            .record_media_readiness_transition(
                crate::startup_media_readiness::media_readiness_initializing_load_transition(
                    operation_id,
                    &remanence_library::MediaReadiness::BecomingReady {
                        ascq: 0x02,
                        media_initializing: true,
                    },
                ),
            )
            .expect("record the fence's last sense");
        let _first_waiter = pool
            .reserve_drive(&crate::drive_pool::DriveKey::new("DEC418146K_LL02", 1))
            .expect("first waiter holds the fenced drive");

        let error = state
            .library_service()
            .resume_media_readiness(Request::new(pb::ResumeMediaReadinessRequest {
                operation_id: operation_id.as_bytes().to_vec(),
                timeout_seconds: 60,
                poll_interval_seconds: 1,
            }))
            .await
            .expect_err("a second waiter must not take the held drive");

        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        let message = error.message();
        assert!(
            message.starts_with("DEC418146K_LL02 drive bay 0x0001 is busy: "),
            "{message}"
        );
        assert!(message.contains(&operation_id.to_string()), "{message}");
        assert!(
            message.contains("Robot moves in library DEC418146K_LL02 are refused"),
            "{message}"
        );
        assert!(
            message.contains("even when other drive bays are idle"),
            "{message}"
        );
        assert!(
            message.contains("is unresolved, sense=02/04/02 (initializing command required). "),
            "{message}"
        );
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
        assert_eq!(drive.fenced, Some(true));
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

        assert!(first_epoch.is_some_and(|epoch| epoch != 0));
        assert!(second_epoch.is_some_and(|epoch| epoch != 0));
        assert_ne!(first_epoch, second_epoch);
    }

    /// A drive that is retired but still bolted into the library keeps its
    /// identity in live status and is reported as retired.
    ///
    /// This is the TOPX.1 defect. The catalog lookup used to exclude retired
    /// rows, and the caller had no branch for "no row found", so the projected
    /// drive kept its zero-seeded identity and the operator saw a nameless row
    /// with no explanation — permanently, since nothing un-retires a drive.
    #[tokio::test]
    async fn live_status_reports_a_retired_drive_with_its_identity() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-RETIRED", "DvcidAndInquiry", 1);
        index
            .retire_drive(drive_uuid.as_slice(), "panel test")
            .expect("retire")
            .expect("drive exists");

        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());
        let response = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("live status")
            .into_inner();
        let drive = &response.libraries[0].drives[0];

        assert_eq!(
            drive.drive_uuid,
            Some(drive_uuid),
            "a retired drive that is still installed must keep its identity"
        );
        assert_eq!(
            drive.catalog_state,
            pb::DriveCatalogState::Retired as i32,
            "and must be reported as retired rather than left unexplained"
        );
    }

    /// A bay the changer reports but the catalog has never seen is reported as
    /// uncatalogued, not as a drive whose identity happens to be empty.
    #[tokio::test]
    async fn live_status_marks_an_uncatalogued_bay() {
        let index = test_index();
        let mut state = state_with_snapshot();
        state.index_path = Arc::new(index.path().to_path_buf());
        let response = state
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .expect("live status")
            .into_inner();
        let drive = &response.libraries[0].drives[0];

        assert!(drive.drive_uuid.is_none());
        assert_eq!(
            drive.catalog_state,
            pb::DriveCatalogState::Uncatalogued as i32,
            "an empty identity must be explained by the catalog state beside it"
        );
    }

    /// Reinstatement returns a retired drive to service and is auditable.
    #[tokio::test]
    async fn reinstate_returns_a_retired_drive_to_service() {
        let mut index = test_index();
        let drive_uuid = observe_test_drive(&mut index, "DRV-BACK", "DvcidAndInquiry", 1);
        index
            .retire_drive(drive_uuid.as_slice(), "retired in error")
            .expect("retire")
            .expect("drive exists");

        let outcome = index
            .reinstate_drive(drive_uuid.as_slice(), "returned to the fleet")
            .expect("reinstate")
            .expect("drive exists");
        assert!(outcome.newly_reinstated);
        assert_eq!(outcome.drive.state, "active");
        assert!(
            outcome.drive.retired_at_utc.is_none(),
            "a reinstated drive must not still carry a retirement timestamp"
        );

        // Idempotent: reinstating an active drive is a no-op, not an error.
        let again = index
            .reinstate_drive(drive_uuid.as_slice(), "again")
            .expect("reinstate")
            .expect("drive exists");
        assert!(!again.newly_reinstated);

        let events = index
            .list_drive_events(drive_uuid.as_slice())
            .expect("events");
        assert!(
            events.iter().any(|e| e.event_kind == "reinstated"),
            "reinstatement must leave an audit trail: {events:?}"
        );
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
    /// Build a fenced drive with a controlled actor queue and real reservation pool.
    fn auto_resume_fixture(
        state_name: &str,
    ) -> (
        ApiState,
        Uuid,
        tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    ) {
        let mut state = state_with_snapshot();
        let (changer, _) = tokio::sync::mpsc::channel(1);
        let (drive, rx) = tokio::sync::mpsc::channel(4);
        state.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
            changer,
            HashMap::from([(1, drive)]),
            Arc::new(HashMap::from([(1, AtomicBool::new(false))])),
        ));
        let id = Uuid::new_v4();
        state
            .index_write()
            .unwrap()
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: id,
                run_id: None,
                endpoint: remanence_state::DriveKey::new("DEC418146K_LL02", 1),
                changer_sg: None,
                drive_sg: None,
                drive_serial: Some("8031BDC7D1".into()),
                barcode: Some("S30002L9".into()),
                source_slot: Some(0x040a),
                media_generation: Some(9),
                phase: "session_open_short_probe".into(),
                state: state_name.into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        (state, id, rx)
    }

    /// Client-first and daemon-first starts share one command and one reservation.
    #[tokio::test]
    async fn auto_resume_and_client_attach_in_both_orders() {
        use crate::readiness_wait::*;
        for client_first in [false, true] {
            let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
            if client_first {
                state
                    .start_readiness_wait(
                        id,
                        DEFAULT_READY_TIMEOUT_SECONDS,
                        DEFAULT_READY_POLL_SECONDS,
                        "client",
                        "session_open",
                    )
                    .unwrap();
            }
            spawn_auto_resume(state.clone());
            let command = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let attached = state
                .library_service()
                .resume_media_readiness(Request::new(pb::ResumeMediaReadinessRequest {
                    operation_id: id.as_bytes().to_vec(),
                    timeout_seconds: 1,
                    poll_interval_seconds: 1,
                }))
                .await
                .unwrap()
                .into_inner();
            assert_eq!(attached.operation_id, id.as_bytes());
            let crate::write_owner::DriveCommand::WaitReady {
                options, handle, ..
            } = &command
            else {
                panic!("wait command")
            };
            assert_eq!(options.timeout.as_secs(), 9_000);
            assert_eq!(options.poll_interval.as_secs(), 30);
            assert_eq!(
                state.operations.latest(&id).unwrap().progress["started_by"],
                if client_first { "client" } else { "daemon" }
            );
            handle.publish_state(pb::OperationState::Running, &[("state", "unit_attention")]);
            assert!(state
                .operations
                .latest(&id)
                .unwrap()
                .progress
                .contains_key("started_by"));
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(rx.try_recv().is_err());
            state
                .readiness_shutdown
                .store(true, std::sync::atomic::Ordering::Release);
            drop(command);
        }
    }

    /// Runtime discovery retries the refusing caller's reservation and admits robotics after READY.
    #[tokio::test]
    async fn auto_resume_waits_for_reservation_release() {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        let held = state
            .drive_pool()
            .unwrap()
            .reserve_drive(&remanence_state::DriveKey::new("DEC418146K_LL02", 1))
            .unwrap();
        let load = state.operations.register(id, "load_drive");
        load.publish_state(pb::OperationState::Running, &[]);
        crate::readiness_wait::spawn_auto_resume(state.clone());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(rx.try_recv().is_err());
        assert!(
            crate::startup_media_readiness::ensure_media_readiness_admitted(
                &state.index().unwrap(),
                "move",
                "DEC418146K_LL02",
                None,
                None,
                true
            )
            .is_err()
        );
        load.publish_failed("load leaves a resumable fence", &[]);
        drop(held);
        state
            .drive_pool()
            .unwrap()
            .readiness_wake
            .notify
            .notify_one();
        let command = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        crate::write_owner::run_readiness_test_command(&state, command).await;
        crate::startup_media_readiness::ensure_media_readiness_admitted(
            &state.index().unwrap(),
            "move",
            "DEC418146K_LL02",
            None,
            None,
            true,
        )
        .unwrap();
        state
            .readiness_shutdown
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Tape-I/O fencing suppresses automatic readiness until the daemon releases it.
    #[tokio::test]
    async fn auto_resume_skips_tape_io_fence_until_rpc_release() {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        let record = state
            .index()
            .unwrap()
            .media_readiness_operation(id)
            .unwrap()
            .unwrap();
        let fence = state
            .index_write()
            .unwrap()
            .record_tape_io_fence_at_endpoint(
                remanence_state::TapeIoFenceInput {
                    tape_uuid: [0; 16],
                    barcode: record.barcode,
                    reason: "transfer_error".into(),
                    evidence_json: None,
                },
                Some(&record.endpoint),
            )
            .unwrap();
        crate::readiness_wait::spawn_auto_resume(state.clone());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(rx.try_recv().is_err());
        assert!(state.operations.latest(&id).is_none());
        <crate::LibraryServiceApi as pb::library_service_server::LibraryService>::release_tape_io_fence(
            &state.library_service(),
            Request::new(pb::ReleaseTapeIoFenceRequest {
                quarantine_id: fence.quarantine_id,
                acknowledgment: "drive verified".into(),
            }),
        )
        .await
        .unwrap();
        let command = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            command,
            crate::write_owner::DriveCommand::WaitReady { .. }
        ));
        state.begin_shutdown();
    }

    /// Release-only states and durable operator cancellation never dispatch automatically.
    #[tokio::test]
    async fn auto_resume_excludes_release_states_and_operator_cancel() {
        for name in [
            "aborted_unknown",
            "timeout_unknown",
            "transport_unknown",
            "terminal_error",
            "reservation_conflict",
            "cancelled",
        ] {
            let (state, id, mut rx) = auto_resume_fixture(if name == "cancelled" {
                "becoming_ready"
            } else {
                name
            });
            if name == "cancelled" {
                state
                    .start_readiness_wait(id, 9000, 30, "client", "session_open")
                    .unwrap();
                let command = rx.recv().await.unwrap();
                state.operations.request_cancel(&id).unwrap();
                crate::write_owner::run_readiness_test_command(&state, command).await;
                assert_eq!(
                    state
                        .index()
                        .unwrap()
                        .media_readiness_operation(id)
                        .unwrap()
                        .unwrap()
                        .cancel_source
                        .as_deref(),
                    Some("operator")
                );
            }
            crate::readiness_wait::spawn_auto_resume(state.clone());
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            assert!(rx.try_recv().is_err(), "{name}");
            if name == "cancelled" {
                state
                    .start_readiness_wait(id, 9000, 30, "client", "session_open")
                    .unwrap();
                assert!(rx.try_recv().is_ok());
            }
            state.begin_shutdown();
        }
    }

    /// A single deadline and terminal readiness outcomes preserve the original refusal.
    #[tokio::test]
    async fn readiness_wait_deadline_failure_and_cancel_are_bounded() {
        for outcome in [
            pb::OperationState::Running,
            pb::OperationState::Failed,
            pb::OperationState::Cancelled,
            pb::OperationState::CompletionUnknown,
        ] {
            let (state, id, _rx) = auto_resume_fixture("becoming_ready");
            let readiness = state.operations.register(id, "media_readiness_wait");
            readiness.publish_state(outcome, &[]);
            let owner = state
                .operations
                .register(Uuid::new_v4(), "tape_verification");
            let refusal =
                crate::readiness_wait::ReadinessFence(id).status("original robot refusal");
            let error = crate::readiness_wait::wait_after_refusal(
                &state,
                &owner,
                refusal,
                std::time::Duration::from_millis(20),
                &mut None,
            )
            .await
            .unwrap_err();
            assert!(error.message().starts_with("original robot refusal;"));
            assert!(error.message().contains("becoming_ready"));
            assert!(error.message().contains("deadline expired"));
        }
        let (state, id, _rx) = auto_resume_fixture("becoming_ready");
        let readiness = state.operations.register(id, "media_readiness_wait");
        readiness.publish_state(pb::OperationState::Running, &[]);
        let owner = state
            .operations
            .register(Uuid::new_v4(), "tape_verification");
        state
            .operations
            .request_cancel(&owner.op_id_uuid())
            .unwrap();
        let error = crate::readiness_wait::wait_after_refusal(
            &state,
            &owner,
            crate::readiness_wait::ReadinessFence(id).status("refusal"),
            std::time::Duration::from_secs(1),
            &mut None,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Cancelled);
        assert!(!readiness.is_cancelled());
    }
    /// A recording failure keeps the row resumable and waits for the fixed sweep.
    #[tokio::test(start_paused = true)]
    async fn auto_resume_recording_failure_waits_for_sweep() {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        crate::readiness_wait::spawn_auto_resume(state.clone());
        discovery_turn().await;
        let command = rx.try_recv().unwrap();
        crate::write_owner::fail_readiness_test_command(&state, command);
        assert_eq!(
            state.operations.latest(&id).unwrap().progress["state"],
            "recording_failed"
        );
        let pool = state.drive_pool().unwrap();
        for _ in 0..119 {
            pool.readiness_wake.notify.notify_one();
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            discovery_turn().await;
            assert!(rx.try_recv().is_err());
        }
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        tokio::time::advance(std::time::Duration::from_millis(250)).await;
        discovery_turn().await;
        assert!(rx.try_recv().is_ok());
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        state.begin_shutdown();
    }

    /// A cancelled wait whose write fails remains sweep-only, including repeated wakes.
    #[tokio::test(start_paused = true)]
    async fn auto_resume_unrecorded_cancel_waits_for_sweep() {
        for client_first in [false, true] {
            let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
            if client_first {
                state
                    .start_readiness_wait(id, 9000, 30, "client", "session_open")
                    .unwrap();
            }
            crate::readiness_wait::spawn_auto_resume(state.clone());
            discovery_turn().await;
            let command = rx.try_recv().unwrap();
            state.operations.request_cancel(&id).unwrap();
            crate::write_owner::fail_readiness_test_command(&state, command);
            let status = state.operations.latest(&id).unwrap();
            assert_eq!(status.state, pb::OperationState::Failed as i32);
            assert!(
                status.error_summary.contains("cancel is not recorded"),
                "{status:?}"
            );
            assert!(state
                .index()
                .unwrap()
                .media_readiness_operation(id)
                .unwrap()
                .unwrap()
                .cancel_source
                .is_none());
            for _ in 0..119 {
                state
                    .drive_pool()
                    .unwrap()
                    .readiness_wake
                    .notify
                    .notify_one();
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                discovery_turn().await;
                assert!(rx.try_recv().is_err());
            }
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            discovery_turn().await;
            assert!(rx.try_recv().is_ok());
            state.begin_shutdown();
        }
    }

    /// A catalog-open failure ends callers promptly but discovery retains its sweep.
    #[tokio::test(start_paused = true)]
    async fn readiness_catalog_failure_ends_waiter() {
        assert_permanent_readiness_failure(false).await;
    }

    /// A stopped actor ends callers promptly but discovery retains its sweep.
    #[tokio::test(start_paused = true)]
    async fn readiness_stopped_actor_ends_waiter() {
        assert_permanent_readiness_failure(true).await;
    }

    /// Exercise permanent dispatch failure and fixed-sweep retry through the actor channel.
    async fn assert_permanent_readiness_failure(closed: bool) {
        let (state, id, rx) = auto_resume_fixture("becoming_ready");
        let actor = if closed {
            drop(rx);
            None
        } else {
            Some(tokio::task::spawn_blocking(move || {
                crate::write_owner::fail_readiness_actor(rx);
            }))
        };
        crate::readiness_wait::spawn_auto_resume(state.clone());
        // Yield to the blocking failed actor without advancing the discovery clock.
        for _ in 0..10000 {
            if state
                .operations
                .latest(&id)
                .is_some_and(|s| s.state == pb::OperationState::Failed as i32)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let status = state.operations.latest(&id).unwrap();
        assert_eq!(status.state, pb::OperationState::Failed as i32);
        let owner = state
            .operations
            .register(Uuid::new_v4(), "tape_verification");
        let error = {
            let mut deadline = None;
            let wait = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                crate::readiness_wait::wait_after_refusal(
                    &state,
                    &owner,
                    crate::readiness_wait::ReadinessFence(id).status("original refusal"),
                    std::time::Duration::from_secs(900),
                    &mut deadline,
                ),
            );
            tokio::pin!(wait);
            tokio::select! {
                biased;
                result = &mut wait => result,
                _ = tokio::task::yield_now() => {
                    // A blocking actor prevents automatic advancement of paused time.
                    tokio::time::advance(std::time::Duration::from_secs(1)).await;
                    wait.await
                }
            }
        };
        let error = match error {
            Ok(result) => result.unwrap_err(),
            Err(error) => {
                state.begin_shutdown();
                drop(state);
                if let Some(actor) = actor {
                    actor.await.unwrap();
                }
                panic!("permanent failure ends promptly: {error}");
            }
        };
        assert!(error.message().contains(&status.error_summary), "{error}");
        let pool = state.drive_pool().unwrap();
        for _ in 0..119 {
            pool.readiness_wake.notify.notify_one();
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            discovery_turn().await;
        }
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        tokio::time::advance(std::time::Duration::from_millis(250)).await;
        discovery_turn().await;
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        state.begin_shutdown();
        drop(state);
        if let Some(actor) = actor {
            actor.await.unwrap();
        }
    }

    /// Resume attaches to the live load and still refuses unrelated ownership.
    #[tokio::test]
    async fn readiness_resume_attaches_to_live_load_only() {
        use tokio_stream::StreamExt;
        for kind in ["load_drive", "media_readiness_wait", "tape_verification"] {
            let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
            let handle = state.operations.register(id, kind);
            handle.publish_state(pb::OperationState::Running, &[]);
            let response = state
                .library_service()
                .resume_media_readiness(Request::new(pb::ResumeMediaReadinessRequest {
                    operation_id: id.as_bytes().to_vec(),
                    timeout_seconds: 9000,
                    poll_interval_seconds: 30,
                }))
                .await;
            if kind == "tape_verification" {
                assert_eq!(
                    response.unwrap_err().code(),
                    tonic::Code::FailedPrecondition
                );
            } else {
                assert_eq!(response.unwrap().into_inner().operation_id, id.as_bytes());
                let mut watch = state.operations.watch(&id).unwrap();
                assert_eq!(
                    watch.next().await.unwrap().unwrap().state,
                    pb::OperationState::Running as i32
                );
                handle.publish_state(pb::OperationState::Succeeded, &[]);
                assert_eq!(
                    watch.next().await.unwrap().unwrap().state,
                    pb::OperationState::Succeeded as i32
                );
                assert!(watch.next().await.is_none());
            }
            assert!(rx.try_recv().is_err());
        }
    }

    /// Allow notified discovery tasks to run without advancing the paused clock.
    async fn discovery_turn() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    /// A busy window expires despite repeated wakes; only the sweep retries it.
    #[tokio::test(start_paused = true)]
    async fn auto_resume_retries_after_reservation_window_expires() {
        let (state, _id, mut rx) = auto_resume_fixture("becoming_ready");
        let pool = state.drive_pool().unwrap();
        let held = pool
            .reserve_drive(&remanence_state::DriveKey::new("DEC418146K_LL02", 1))
            .unwrap();
        crate::readiness_wait::spawn_auto_resume(state.clone());
        discovery_turn().await;
        for _ in 0..24 {
            pool.readiness_wake.notify.notify_one();
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            discovery_turn().await;
        }
        let attempts = pool
            .readiness_wake
            .start_attempts
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(attempts, 20);
        drop(held);
        pool.readiness_wake.notify.notify_one();
        discovery_turn().await;
        assert!(rx.try_recv().is_err());
        tokio::time::advance(std::time::Duration::from_secs(24)).await;
        discovery_turn().await;
        assert!(rx.try_recv().is_ok());
        state.begin_shutdown();
    }

    /// A non-busy refusal gets one attempt per sweep, regardless of wakes.
    #[tokio::test(start_paused = true)]
    async fn auto_resume_refused_start_waits_for_sweep() {
        let (mut state, _id, mut rx) = auto_resume_fixture("becoming_ready");
        state.default_library_serial = Some(Arc::new("OTHER".into()));
        let snapshot = state.library_snapshot.as_ref().unwrap();
        let mut updated = {
            let current = snapshot.read().unwrap();
            crate::live_status::LibrarySnapshot {
                report: current.report.clone(),
                captured_at: current.captured_at,
            }
        };
        updated.report.libraries[0].drive_bays[0].loaded_tape = Some("DIFFERENT".into());
        *snapshot.write().unwrap() = Arc::new(updated);
        let pool = state.drive_pool().unwrap();
        crate::readiness_wait::spawn_auto_resume(state.clone());
        discovery_turn().await;
        for _ in 0..119 {
            pool.readiness_wake.notify.notify_one();
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            discovery_turn().await;
        }
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        tokio::time::advance(std::time::Duration::from_millis(250)).await;
        discovery_turn().await;
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert!(rx.try_recv().is_err());
        state.begin_shutdown();
    }

    /// A live LoadDrive owns its fence id even before it publishes progress.
    #[tokio::test(start_paused = true)]
    async fn auto_resume_skips_live_load_operation() {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        let load = state.operations.register(id, "load_drive");
        let pool = state.drive_pool().unwrap();
        crate::readiness_wait::spawn_auto_resume(state.clone());
        for _ in 0..160 {
            pool.readiness_wake.notify.notify_one();
            tokio::time::advance(std::time::Duration::from_millis(250)).await;
            discovery_turn().await;
        }
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        load.publish_failed("load leaves a resumable fence", &[]);
        pool.readiness_wake.notify.notify_one();
        discovery_turn().await;
        assert!(rx.try_recv().is_ok());
        assert_eq!(
            pool.readiness_wake
                .start_attempts
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        state.begin_shutdown();
    }

    /// Retried terminal waits do not terminate a verification or finalize observer.
    #[tokio::test(start_paused = true)]
    async fn readiness_observer_waits_through_retry_and_load_release() {
        for kind in ["tape_verification", "finalize_tape"] {
            for ended in [pb::OperationState::Failed, pb::OperationState::Cancelled] {
                let (state, id, _rx) = auto_resume_fixture("becoming_ready");
                let wait = state.operations.register(id, "media_readiness_wait");
                wait.publish_state(ended, &[("phase", "shutdown")]);
                let observer = state.operations.register(Uuid::new_v4(), kind);
                let watching = state.clone();
                let task = tokio::spawn(async move {
                    crate::readiness_wait::wait_after_refusal(
                        &watching,
                        &observer,
                        crate::readiness_wait::ReadinessFence(id).status("refusal"),
                        std::time::Duration::from_secs(60),
                        &mut None,
                    )
                    .await
                });
                discovery_turn().await;
                assert!(!task.is_finished());
                let load = state.operations.register(id, "load_drive");
                load.publish_state(pb::OperationState::Running, &[]);
                state
                    .index_write()
                    .unwrap()
                    .record_media_readiness_transition(
                        crate::startup_media_readiness::startup_media_readiness_probe_transition(
                            id,
                            &remanence_library::MediaReadiness::Ready,
                        ),
                    )
                    .unwrap();
                tokio::time::advance(std::time::Duration::from_secs(1)).await;
                discovery_turn().await;
                assert!(!task.is_finished());
                load.publish_state(pb::OperationState::Succeeded, &[]);
                tokio::time::advance(std::time::Duration::from_millis(250)).await;
                discovery_turn().await;
                task.await.unwrap().unwrap();
            }
        }
    }

    /// A fence observer retains its read connection between polls.
    #[tokio::test(start_paused = true)]
    async fn readiness_observer_reuses_open_catalog() {
        assert_readiness_reuses_catalog(false).await;
    }

    /// Discovery passes its existing read connection into the start path.
    #[tokio::test(start_paused = true)]
    async fn readiness_discovery_reuses_open_catalog() {
        assert_readiness_reuses_catalog(true).await;
    }

    /// Removing the path prevents reopening while existing connections still read.
    async fn assert_readiness_reuses_catalog(discovery: bool) {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        let pool = state.drive_pool().unwrap();
        let held = pool
            .reserve_drive(&remanence_state::DriveKey::new("DEC418146K_LL02", 1))
            .unwrap();
        let observer = state.operations.register(Uuid::new_v4(), "finalize_tape");
        let watching = state.clone();
        let task = if discovery {
            crate::readiness_wait::spawn_auto_resume(state.clone());
            None
        } else {
            Some(tokio::spawn(async move {
                crate::readiness_wait::wait_after_refusal(
                    &watching,
                    &observer,
                    crate::readiness_wait::ReadinessFence(id).status("refusal"),
                    std::time::Duration::from_secs(60),
                    &mut None,
                )
                .await
            }))
        };
        discovery_turn().await;
        let moved = state.index_path.with_extension("moved");
        std::fs::rename(state.index_path.as_ref(), &moved).unwrap();
        drop(held);
        tokio::time::advance(std::time::Duration::from_millis(250)).await;
        discovery_turn().await;
        if discovery {
            assert!(rx.try_recv().is_ok());
        } else {
            assert!(!task.as_ref().unwrap().is_finished());
        }
        std::fs::rename(&moved, state.index_path.as_ref()).unwrap();
        if let Some(task) = task {
            task.abort();
        }
        state.begin_shutdown();
    }

    /// LoadDrive admission attaches the same shutdown signal as standalone waits.
    #[tokio::test]
    async fn load_drive_wait_uses_readiness_shutdown_lifetime() {
        let (mut state, _id, _rx) = auto_resume_fixture("ready");
        let (changer, mut commands) = tokio::sync::mpsc::channel(1);
        let (drive, _drive_rx) = tokio::sync::mpsc::channel(1);
        state.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
            "DEC418146K_LL02",
            changer,
            HashMap::from([(1, drive)]),
            Arc::new(HashMap::from([(1, AtomicBool::new(false))])),
        ));
        state
            .library_service()
            .load_drive(Request::new(pb::LoadDriveRequest {
                library_uuid: library_uuid("DEC418146K_LL02").to_vec(),
                slot_element_address: 0x400,
                drive_element_address: 1,
                no_wait: false,
                ..Default::default()
            }))
            .await
            .unwrap();
        let crate::write_owner::ChangerCommand::Robotics { handle, .. } =
            commands.recv().await.unwrap()
        else {
            panic!("load")
        };
        assert!(!handle.is_shutting_down());
        state.begin_shutdown();
        assert!(handle.is_shutting_down());
        state
            .drive_pool()
            .unwrap()
            .release_library("DEC418146K_LL02");
    }

    /// Dispatch congestion does not permanently stop automatic admission.
    #[tokio::test]
    async fn auto_resume_retries_after_full_drive_channel() {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        let tx = state
            .drive_pool()
            .unwrap()
            .drive_tx(&remanence_state::DriveKey::new("DEC418146K_LL02", 1))
            .unwrap();
        for _ in 0..4 {
            let (reply, _) = tokio::sync::oneshot::channel();
            tx.try_send(crate::write_owner::DriveCommand::Unload { reply })
                .unwrap();
        }
        assert!(state
            .start_readiness_wait(id, 9000, 30, "daemon", "session_open")
            .is_err());
        assert_eq!(
            state.operations.latest(&id).unwrap().state,
            pb::OperationState::Failed as i32
        );
        for _ in 0..4 {
            drop(rx.recv().await.unwrap());
        }
        crate::readiness_wait::spawn_auto_resume(state.clone());
        let command = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        crate::write_owner::run_readiness_test_command(&state, command).await;
        state.begin_shutdown();
    }

    /// A short caller deadline never becomes the daemon's physical wait deadline.
    #[tokio::test]
    async fn resume_starts_with_at_least_daemon_timeout() {
        for (requested, expected) in [(1, 9000), (10000, 10000)] {
            let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
            state
                .library_service()
                .resume_media_readiness(Request::new(pb::ResumeMediaReadinessRequest {
                    operation_id: id.as_bytes().to_vec(),
                    timeout_seconds: requested,
                    poll_interval_seconds: 1,
                }))
                .await
                .unwrap();
            let command = rx.recv().await.unwrap();
            let crate::write_owner::DriveCommand::WaitReady { options, .. } = &command else {
                panic!("wait")
            };
            assert_eq!(options.timeout.as_secs(), expected);
            drop(command);
            state.begin_shutdown();
        }
    }

    /// A LoadDrive failure with the fence id does not stop a readiness observer.
    #[tokio::test]
    async fn readiness_observer_ignores_other_operation_kinds() {
        let (state, id, _rx) = auto_resume_fixture("ready");
        state
            .operations
            .register(id, "load_drive")
            .publish_failed("load left a fence", &[]);
        let observer = state
            .operations
            .register(Uuid::new_v4(), "tape_verification");
        crate::readiness_wait::wait_after_refusal(
            &state,
            &observer,
            crate::readiness_wait::ReadinessFence(id).status("original refusal"),
            std::time::Duration::from_secs(1),
            &mut None,
        )
        .await
        .unwrap();
    }

    /// Only the same readiness holder delays an observer of a ready row.
    #[tokio::test]
    async fn ready_fence_waits_only_for_its_own_holder() {
        let (state, id, _rx) = auto_resume_fixture("ready");
        let pool = state.drive_pool().unwrap();
        let key = remanence_state::DriveKey::new("DEC418146K_LL02", 1);
        let _held = pool.reserve_drive(&key).unwrap();
        let observer = state.operations.register(Uuid::new_v4(), "finalize_tape");
        for (kind, owner, blocked) in [
            ("media_readiness_wait", id, true),
            ("media_readiness_wait", Uuid::new_v4(), false),
            ("read_session", id, false),
        ] {
            pool.set_holder(&key, None, kind, owner);
            let outcome = tokio::time::timeout(
                std::time::Duration::from_millis(40),
                crate::readiness_wait::wait_after_refusal(
                    &state,
                    &observer,
                    crate::readiness_wait::ReadinessFence(id).status("original refusal"),
                    std::time::Duration::from_secs(1),
                    &mut None,
                ),
            )
            .await;
            assert_eq!(outcome.is_err(), blocked);
            if !blocked {
                outcome.unwrap().unwrap();
            }
        }
    }
    /// A daemon without drive actors creates no discovery task or catalog polling.
    #[test]
    fn auto_resume_without_drive_pool_does_not_spawn() {
        crate::readiness_wait::spawn_auto_resume(ApiState::new(test_index()));
    }

    /// Idle discovery performs no short-interval scans, including after a pending fence clears.
    #[tokio::test]
    async fn auto_resume_sleeps_when_idle_and_after_fence_clears() {
        let (state, id, mut rx) = auto_resume_fixture("becoming_ready");
        let pool = state.drive_pool().unwrap();
        let _held = pool
            .reserve_drive(&remanence_state::DriveKey::new("DEC418146K_LL02", 1))
            .unwrap();
        crate::readiness_wait::spawn_auto_resume(state.clone());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // An independent readiness probe clears the row while discovery retries its reservation.
        state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    id,
                    &remanence_library::MediaReadiness::Ready,
                ),
            )
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let scans = pool
            .readiness_wake
            .scans
            .load(std::sync::atomic::Ordering::Relaxed);
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        assert_eq!(
            pool.readiness_wake
                .scans
                .load(std::sync::atomic::Ordering::Relaxed),
            scans
        );
        assert!(rx.try_recv().is_err());
        state.begin_shutdown();
    }
}
