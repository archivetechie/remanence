//! `rem drive unload` against a fake daemon that keeps a small library inventory.
//! The fake refuses a busy library synchronously, as drive reservation does, and
//! reports an empty bay as a failed operation, as the changer path does.
use super::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};

const LIBRARY_SERIAL: &str = "RMT-FAKE";
const LIBRARY_UUID: [u8; 16] = [0x52; 16];
const BAY: u16 = 0x0101;
const EMPTY_BAY: u16 = 0x0102;
const SOURCE_SLOT: u32 = 0x03ea;
const OTHER_SLOT: u32 = 0x03f0;
const BARCODE: &str = "ACM003L9";

#[derive(Default)]
struct Bench {
    /// Drive bay to the seated barcode and its recorded source slot.
    bays: BTreeMap<u32, Option<(String, u32)>>,
    slots: BTreeMap<u32, Option<String>>,
    /// A synchronous refusal, as a held drive reservation produces.
    busy: Option<String>,
    requests: Vec<pb::UnloadDriveRequest>,
    operations: BTreeMap<Vec<u8>, pb::OperationStatus>,
}

type Shared = Arc<Mutex<Bench>>;

fn bench() -> Shared {
    Arc::new(Mutex::new(Bench {
        bays: BTreeMap::from([
            (u32::from(BAY), Some((BARCODE.to_string(), SOURCE_SLOT))),
            (u32::from(EMPTY_BAY), None),
        ]),
        slots: BTreeMap::from([(SOURCE_SLOT, None), (OTHER_SLOT, None)]),
        ..Bench::default()
    }))
}

fn library() -> pb::Library {
    pb::Library {
        library_serial: LIBRARY_SERIAL.to_string(),
        library_uuid: LIBRARY_UUID.to_vec(),
        ..Default::default()
    }
}

fn status(
    id: &[u8],
    state: pb::OperationState,
    phase: &str,
    error_summary: &str,
) -> pb::OperationStatus {
    pb::OperationStatus {
        operation_id: id.to_vec(),
        operation_kind: "unload_drive".to_string(),
        state: state as i32,
        progress: HashMap::from([("phase".to_string(), phase.to_string())]),
        error_summary: error_summary.to_string(),
        ..Default::default()
    }
}

fn unused<T>() -> Result<Response<T>, Status> {
    Err(Status::unimplemented("unused by drive unload fixture"))
}

#[derive(Clone)]
struct FakeLibrary(Shared);

#[tonic::async_trait]
impl pb::library_service_server::LibraryService for FakeLibrary {
    async fn release_tape_io_fence(
        &self,
        _: Request<pb::ReleaseTapeIoFenceRequest>,
    ) -> Result<Response<pb::TapeIoFence>, Status> {
        unused()
    }
    async fn list_libraries(
        &self,
        _: Request<()>,
    ) -> Result<Response<pb::ListLibrariesResponse>, Status> {
        Ok(Response::new(pb::ListLibrariesResponse {
            libraries: vec![library()],
        }))
    }
    async fn get_library(
        &self,
        request: Request<pb::GetLibraryRequest>,
    ) -> Result<Response<pb::LibraryState>, Status> {
        assert_eq!(request.into_inner().library_uuid, LIBRARY_UUID);
        let bench = self.0.lock().unwrap();
        Ok(Response::new(pb::LibraryState {
            library: Some(library()),
            drives: bench
                .bays
                .iter()
                .map(|(bay, seated)| pb::Drive {
                    element_address: Some(*bay),
                    loaded_tape_barcode: seated.as_ref().map(|(barcode, _)| barcode.clone()),
                    ..Default::default()
                })
                .collect(),
            slots: bench
                .slots
                .iter()
                .map(|(slot, voltag)| pb::Slot {
                    element_address: *slot,
                    voltag: voltag.clone(),
                    tape_uuid: None,
                })
                .collect(),
            ..Default::default()
        }))
    }
    async fn refresh_inventory(
        &self,
        _: Request<pb::RefreshInventoryRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    async fn list_drives(
        &self,
        _: Request<pb::ListDrivesRequest>,
    ) -> Result<Response<pb::ListDrivesResponse>, Status> {
        unused()
    }
    async fn get_drive(
        &self,
        _: Request<pb::GetDriveRequest>,
    ) -> Result<Response<pb::DriveCatalogEntry>, Status> {
        unused()
    }
    async fn get_drive_history(
        &self,
        _: Request<pb::GetDriveHistoryRequest>,
    ) -> Result<Response<pb::GetDriveHistoryResponse>, Status> {
        unused()
    }
    async fn annotate_drive(
        &self,
        _: Request<pb::AnnotateDriveRequest>,
    ) -> Result<Response<pb::DriveCatalogEntry>, Status> {
        unused()
    }
    async fn retire_drive(
        &self,
        _: Request<pb::RetireDriveRequest>,
    ) -> Result<Response<pb::RetireDriveResponse>, Status> {
        unused()
    }
    async fn reinstate_drive(
        &self,
        _: Request<pb::ReinstateDriveRequest>,
    ) -> Result<Response<pb::ReinstateDriveResponse>, Status> {
        unused()
    }
    async fn poll_drive(
        &self,
        _: Request<pb::PollDriveRequest>,
    ) -> Result<Response<pb::DriveHealthSnapshot>, Status> {
        unused()
    }
    async fn clean_drive(
        &self,
        _: Request<pb::CleanDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    async fn resume_media_readiness(
        &self,
        _: Request<pb::ResumeMediaReadinessRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    async fn list_alarms(
        &self,
        _: Request<pb::ListAlarmsRequest>,
    ) -> Result<Response<pb::ListAlarmsResponse>, Status> {
        unused()
    }
    async fn ack_alarm(
        &self,
        _: Request<pb::AckAlarmRequest>,
    ) -> Result<Response<pb::Alarm>, Status> {
        unused()
    }
    async fn get_live_status(
        &self,
        _: Request<pb::GetLiveStatusRequest>,
    ) -> Result<Response<pb::GetLiveStatusResponse>, Status> {
        unused()
    }
    async fn move_medium(
        &self,
        _: Request<pb::MoveMediumRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    async fn load_drive(
        &self,
        _: Request<pb::LoadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    async fn unload_drive(
        &self,
        request: Request<pb::UnloadDriveRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let request = request.into_inner();
        let mut bench = self.0.lock().unwrap();
        bench.requests.push(request.clone());
        if request.library_uuid != LIBRARY_UUID {
            return Err(Status::not_found("library not found"));
        }
        if let Some(message) = bench.busy.clone() {
            return Err(Status::failed_precondition(message));
        }
        let bay = request.drive_element_address;
        let moved = match bench.bays.get(&bay).cloned() {
            None => Err(format!("element 0x{bay:04x} is not a drive bay")),
            Some(None) => Err(format!("source element 0x{bay:04x} is empty")),
            Some(Some((barcode, source))) => {
                let dst = match request.destination_slot_address {
                    0 => source,
                    explicit => explicit,
                };
                match bench.slots.get(&dst) {
                    Some(None) => {
                        bench.slots.insert(dst, Some(barcode));
                        bench.bays.insert(bay, None);
                        Ok(())
                    }
                    Some(Some(_)) => Err(format!("destination element 0x{dst:04x} is full")),
                    None => Err(format!("element 0x{dst:04x} is not a storage slot")),
                }
            }
        };
        let id = Uuid::new_v4().as_bytes().to_vec();
        let terminal = match moved {
            Ok(()) => status(&id, pb::OperationState::Succeeded, "done", ""),
            Err(message) => status(&id, pb::OperationState::Failed, "execute", &message),
        };
        bench.operations.insert(id.clone(), terminal);
        Ok(Response::new(pb::OperationRef { operation_id: id }))
    }
    async fn import_element(
        &self,
        _: Request<pb::ImportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    async fn export_element(
        &self,
        _: Request<pb::ExportElementRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        unused()
    }
    type StreamLibraryEventsStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<pb::LibraryEvent, Status>>>;
    async fn stream_library_events(
        &self,
        _: Request<pb::StreamLibraryEventsRequest>,
    ) -> Result<Response<Self::StreamLibraryEventsStream>, Status> {
        unused()
    }
}

#[derive(Clone)]
struct FakeDaemon(Shared);

#[tonic::async_trait]
impl pb::daemon_server::Daemon for FakeDaemon {
    async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
        Ok(Response::new(pb::HealthResponse {
            observed_endpoints: crate::capability_tests::endpoints(),
            ..Default::default()
        }))
    }
    async fn version(&self, _: Request<()>) -> Result<Response<pb::VersionResponse>, Status> {
        unused()
    }
    async fn get_operation(
        &self,
        _: Request<pb::GetOperationRequest>,
    ) -> Result<Response<pb::OperationStatus>, Status> {
        unused()
    }
    async fn list_operations(
        &self,
        _: Request<pb::ListOperationsRequest>,
    ) -> Result<Response<pb::ListOperationsResponse>, Status> {
        unused()
    }
    async fn cancel_operation(
        &self,
        _: Request<pb::CancelOperationRequest>,
    ) -> Result<Response<pb::CancelOperationResponse>, Status> {
        unused()
    }
    type WatchOperationStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<pb::OperationStatus, Status>>>;
    async fn watch_operation(
        &self,
        request: Request<pb::GetOperationRequest>,
    ) -> Result<Response<Self::WatchOperationStream>, Status> {
        let id = request.into_inner().operation_id;
        let terminal = self
            .0
            .lock()
            .unwrap()
            .operations
            .get(&id)
            .cloned()
            .ok_or_else(|| Status::not_found("operation not found"))?;
        // The daemon publishes removal preparation before the changer move.
        let running = status(&id, pb::OperationState::Running, "prepare_removal", "");
        Ok(Response::new(tokio_stream::iter(vec![
            Ok(running),
            Ok(terminal),
        ])))
    }
}

struct FakeServer {
    endpoint: String,
    _runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

fn serve(bench: Shared) -> FakeServer {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rem.sock");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let listener = {
        let _guard = runtime.enter();
        tokio::net::UnixListener::bind(&socket).unwrap()
    };
    runtime.spawn(
        tonic::transport::Server::builder()
            .add_service(pb::daemon_server::DaemonServer::new(FakeDaemon(
                bench.clone(),
            )))
            .add_service(pb::library_service_server::LibraryServiceServer::new(
                FakeLibrary(bench),
            ))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    FakeServer {
        endpoint: format!("unix:{}", socket.display()),
        _runtime: runtime,
        _dir: dir,
    }
}

fn rem(server: &FakeServer, args: &[&str]) -> (ExitCode, String, String) {
    let mut argv = vec!["rem", "drive", "unload", "--endpoint", &server.endpoint];
    argv.extend_from_slice(args);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run(
        Cli::try_parse_from(argv).unwrap(),
        || panic!("drive unload reached discovery"),
        &mut out,
        &mut err,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn seated(bench: &Shared, bay: u16) -> Option<String> {
    bench.lock().unwrap().bays[&u32::from(bay)]
        .as_ref()
        .map(|(barcode, _)| barcode.clone())
}

fn slot(bench: &Shared, slot: u32) -> Option<String> {
    bench.lock().unwrap().slots[&slot].clone()
}

#[test]
fn rem_drive_unload_parses_bay_destination_and_library() {
    let default = Cli::parse_from([
        "rem",
        "drive",
        "unload",
        "--library",
        "mainlib",
        "--bay",
        "0x0101",
    ]);
    assert!(matches!(
        default.command,
        RemCommand::Drive {
            command: DriveClientCommand::Unload(DriveUnloadArgs {
                ref library,
                bay: 0x0101,
                dest: None,
            }),
            ..
        } if library == "mainlib"
    ));
    let explicit = Cli::parse_from([
        "rem",
        "drive",
        "unload",
        "--library",
        "mainlib",
        "--bay",
        "257",
        "--dest",
        "0x03f0",
    ]);
    assert!(matches!(
        explicit.command,
        RemCommand::Drive {
            command: DriveClientCommand::Unload(DriveUnloadArgs {
                bay: 0x0101,
                dest: Some(0x03f0),
                ..
            }),
            ..
        }
    ));
    assert!(Cli::try_parse_from(["rem", "drive", "unload", "--library", "mainlib"]).is_err());
    assert!(Cli::try_parse_from(["rem", "drive", "unload", "--bay", "0x0101"]).is_err());
    assert!(Cli::try_parse_from([
        "rem",
        "drive",
        "unload",
        "--library",
        "mainlib",
        "--bay",
        "0x0101",
        "--dest",
        "slot-a",
    ])
    .is_err());
}

#[test]
fn rem_drive_unload_returns_cartridge_to_its_source_slot_by_default() {
    let bench = bench();
    let server = serve(bench.clone());
    let (code, out, err) = rem(&server, &["--library", LIBRARY_SERIAL, "--bay", "0x0101"]);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(out.contains("succeeded  kind=unload_drive"), "{out}");
    assert!(out.contains("phase: done"), "{out}");
    assert!(
        out.ends_with("unloaded ACM003L9 from bay 0x0101 to slot 0x03ea\n"),
        "{out}"
    );
    let requests = bench.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].library_uuid, LIBRARY_UUID);
    assert_eq!(requests[0].drive_element_address, u32::from(BAY));
    assert_eq!(requests[0].destination_slot_address, 0);
    assert_eq!(seated(&bench, BAY), None);
    assert_eq!(slot(&bench, SOURCE_SLOT).as_deref(), Some(BARCODE));
}

#[test]
fn rem_drive_unload_sends_an_explicit_destination() {
    let bench = bench();
    let server = serve(bench.clone());
    let uuid = Uuid::from_bytes(LIBRARY_UUID).to_string();
    let (code, out, err) = rem(
        &server,
        &["--library", &uuid, "--bay", "0x0101", "--dest", "0x03f0"],
    );
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    assert!(
        out.ends_with("unloaded ACM003L9 from bay 0x0101 to slot 0x03f0\n"),
        "{out}"
    );
    assert_eq!(
        bench.lock().unwrap().requests[0].destination_slot_address,
        OTHER_SLOT
    );
    assert_eq!(slot(&bench, SOURCE_SLOT), None);
    assert_eq!(slot(&bench, OTHER_SLOT).as_deref(), Some(BARCODE));
}

#[test]
fn rem_drive_unload_refuses_destination_zero_before_the_daemon() {
    let bench = bench();
    let server = serve(bench.clone());
    let (code, out, err) = rem(
        &server,
        &[
            "--library",
            LIBRARY_SERIAL,
            "--bay",
            "0x0101",
            "--dest",
            "0",
        ],
    );
    assert_eq!(code, ExitCode::from(1));
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("--dest must name a storage slot"), "{err}");
    assert!(bench.lock().unwrap().requests.is_empty());
}

#[test]
fn rem_drive_unload_reports_an_empty_bay_refusal() {
    let bench = bench();
    let server = serve(bench.clone());
    let (code, out, err) = rem(&server, &["--library", LIBRARY_SERIAL, "--bay", "0x0102"]);
    assert_eq!(code, ExitCode::from(1));
    assert!(out.is_empty(), "{out}");
    assert!(err.starts_with("error: operation "), "{err}");
    assert!(
        err.ends_with("finished failed: source element 0x0102 is empty\n"),
        "{err}"
    );
}

#[test]
fn rem_drive_unload_reports_a_busy_bay_refusal() {
    let bench = bench();
    bench.lock().unwrap().busy =
        Some("drives are busy; held by write_session on bay 0x0101".to_string());
    let server = serve(bench.clone());
    let (code, out, err) = rem(&server, &["--library", LIBRARY_SERIAL, "--bay", "0x0101"]);
    assert_eq!(code, ExitCode::from(1));
    assert!(out.is_empty(), "{out}");
    assert_eq!(
        err,
        "error: daemon returned failed_precondition: drives are busy; held by write_session on bay 0x0101\n"
    );
    assert_eq!(seated(&bench, BAY).as_deref(), Some(BARCODE));

    let (code, out, err) = rem(
        &server,
        &["--library", LIBRARY_SERIAL, "--bay", "0x0101", "--json"],
    );
    assert_eq!(code, ExitCode::from(1));
    assert!(out.is_empty(), "{out}");
    let error: Value = serde_json::from_str(&err).unwrap();
    assert_eq!(error["schema"], "rem.error.v1");
    assert_eq!(error["code"], "failed_precondition");
    assert_eq!(
        error["message"],
        "daemon returned failed_precondition: drives are busy; held by write_session on bay 0x0101"
    );
}

#[test]
fn rem_drive_unload_json_reports_operation_and_location() {
    let bench = bench();
    let server = serve(bench.clone());
    let (code, out, err) = rem(
        &server,
        &["--library", LIBRARY_SERIAL, "--bay", "0x0101", "--json"],
    );
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    let value: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["schema"], "rem.operation.status.v1");
    assert_eq!(value["kind"], "item");
    assert_eq!(
        value["observed_endpoints"],
        json!(endpoint_json(&crate::capability_tests::endpoints()))
    );
    let data = &value["data"];
    assert_eq!(data["operation_kind"], "unload_drive");
    assert_eq!(data["state"], "succeeded");
    assert_eq!(data["progress"], json!({"phase": "done"}));
    assert_eq!(data["error_summary"], "");
    assert!(Uuid::parse_str(data["operation_id"].as_str().unwrap()).is_ok());
    assert_eq!(
        data["location"],
        json!({
            "library_serial": LIBRARY_SERIAL,
            "bay": BAY,
            "barcode": BARCODE,
            "slot": SOURCE_SLOT,
        })
    );
}

/// Like `drive load`, the command sends no idempotency key, so a retry is a new
/// request: after a completed unload the daemon refuses the now-empty bay.
#[test]
fn rem_drive_unload_retry_is_a_new_request_refused_once_the_bay_is_empty() {
    let bench = bench();
    let server = serve(bench.clone());
    let args = ["--library", LIBRARY_SERIAL, "--bay", "0x0101"];
    let (code, _, err) = rem(&server, &args);
    assert_eq!(code, ExitCode::SUCCESS, "{err}");
    let (code, out, err) = rem(&server, &args);
    assert_eq!(code, ExitCode::from(1));
    assert!(out.is_empty(), "{out}");
    assert!(
        err.ends_with("finished failed: source element 0x0101 is empty\n"),
        "{err}"
    );
    let requests = bench.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 2);
    assert!(requests
        .iter()
        .all(|request| request.idempotency_key.is_none()));
    assert_eq!(slot(&bench, SOURCE_SLOT).as_deref(), Some(BARCODE));
    assert_eq!(slot(&bench, OTHER_SLOT), None);
}

#[test]
fn unloaded_slot_prefers_inventory_then_requested_destination() {
    let slots = [
        pb::Slot {
            element_address: SOURCE_SLOT,
            voltag: Some(BARCODE.to_string()),
            tape_uuid: None,
        },
        pb::Slot {
            element_address: OTHER_SLOT,
            voltag: None,
            tape_uuid: None,
        },
    ];
    assert_eq!(
        unloaded_slot(Some(BARCODE), Some(0x03f0), Some(&slots)),
        Some(SOURCE_SLOT)
    );
    assert_eq!(
        unloaded_slot(None, Some(0x03f0), Some(&slots)),
        Some(OTHER_SLOT)
    );
    assert_eq!(unloaded_slot(Some(BARCODE), None, None), None);
    assert_eq!(unloaded_slot(Some("OTHER0L9"), None, Some(&slots)), None);
}
