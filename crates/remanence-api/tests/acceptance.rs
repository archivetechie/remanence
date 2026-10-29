//! Acceptance properties at the public RPC boundary over virtual libraries.
//!
//! All device opens resolve to ModelTransport. The transport observer supplies
//! deterministic scheduling cuts and distinguishes LOAD from UNLOAD; the model
//! command log remains the evidence that a command reaches a device.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ciborium::value::Value;
use remanence_api::pb::catalog_server::Catalog;
use remanence_api::pb::library_service_server::LibraryService;
use remanence_api::pb::read_session_service_server::ReadSessionService;
use remanence_api::pb::write_session_service_server::WriteSessionService;
use remanence_api::{build_tape_bootstrap, pb, ApiState};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::transport::TransferOutcome;
use remanence_library::{DiscoveryReport, ScsiError, SgTransport, StaticAllowlist, TimeoutClass};
use remanence_parity::{bootstrap::write_bootstrap_block, parse_parity_arg, ParityConfig};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex, RemConfig, SourceLayer,
    StateHandle, StatePaths,
};
use tonic::{Code, Request, Status};

const BAY: u16 = 0x100;
const SLOT: u16 = 0x400;
const BLOCK: u32 = 262_144;
const BOUND: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Cut {
    opcode: Option<u8>,
    entered: bool,
    released: bool,
}

#[derive(Default)]
struct Control {
    cut: Mutex<Cut>,
    resume: Condvar,
    // Full CDBs supplement the model's opcode-only command record.
    commands: Mutex<Vec<(DeviceRole, Vec<u8>)>>,
    fail_write: Mutex<bool>,
    not_ready: Mutex<bool>,
}

impl Control {
    fn arm(&self, opcode: u8) {
        *self.cut.lock().unwrap() = Cut {
            opcode: Some(opcode),
            entered: false,
            released: false,
        };
    }

    async fn entered(&self) {
        tokio::time::timeout(BOUND, async {
            while !self.cut.lock().unwrap().entered {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("request reaches the scheduling cut");
    }

    fn release(&self) {
        self.cut.lock().unwrap().released = true;
        self.resume.notify_all();
    }

    fn before(&self, role: DeviceRole, cdb: &[u8]) -> Result<(), ScsiError> {
        let mut cut = self.cut.lock().unwrap();
        if cut.opcode == cdb.first().copied() && !cut.entered {
            cut.entered = true;
            let (guard, timeout) = self
                .resume
                .wait_timeout_while(cut, BOUND, |cut| !cut.released)
                .unwrap();
            assert!(!timeout.timed_out(), "test releases the scheduling cut");
            drop(guard);
        } else {
            drop(cut);
        }
        if cdb[0] == 0x0a && std::mem::take(&mut *self.fail_write.lock().unwrap()) {
            return Err(ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: vec![],
            });
        }
        self.commands.lock().unwrap().push((role, cdb.to_vec()));
        Ok(())
    }
}

struct Transport {
    inner: ModelTransport,
    control: Arc<Control>,
    world: SharedVirtualWorld,
}

impl SgTransport for Transport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.control.before(self.inner.role(), cdb)?;
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.control.before(self.inner.role(), cdb)?;
        let result = self.inner.execute_none(cdb);
        if result.is_ok() && cdb[0] == 0xa5 && u16::from_be_bytes([cdb[6], cdb[7]]) == BAY {
            // A newly seated cartridge requires threading. The world exposes
            // this drive state independently of the changer MOVE completion.
            self.world.lock().unwrap().leave_cartridge_unthreaded(BAY);
        }
        if cdb[0] == 0x00 && *self.control.not_ready.lock().unwrap() {
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = 2;
            sense[7] = 10;
            sense[12] = 4;
            sense[13] = 1;
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.control.before(self.inner.role(), cdb)?;
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

struct Rig {
    root: tempfile::TempDir,
    config: RemConfig,
    worlds: Vec<SharedVirtualWorld>,
    controls: Vec<Arc<Control>>,
    api: ApiState,
}

fn uuid(n: usize) -> [u8; 16] {
    [u8::try_from(n + 1).unwrap(); 16]
}
fn barcode(n: usize) -> String {
    format!("ACC{n:03}L9")
}
fn pool(n: usize) -> String {
    format!("pool-{n}")
}
fn serial(n: usize) -> String {
    format!("LIB-{n}")
}

fn config(root: &Path, libraries: usize) -> RemConfig {
    let mut text = format!(
        r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
drive_idle_unload_seconds = 1800
io_memory_ceiling = "128 MiB"
append_ring_bytes = "16 MiB"
[tape_io]
read_reservoir_bytes = "16 MiB"
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/audit"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
        root.display()
    );
    for n in 0..libraries {
        text.push_str(&format!(
            "\n[[libraries]]\nserial = \"{}\"\n[[tape_pools]]\nid = \"{}\"\nblock_size = {BLOCK}\n",
            serial(n),
            pool(n)
        ));
    }
    std::fs::write(root.join("config.toml"), &text).unwrap();
    remanence_state::parse_config_toml(&text).unwrap()
}

fn start(config: &RemConfig, worlds: &[SharedVirtualWorld], controls: &[Arc<Control>]) -> ApiState {
    std::fs::create_dir_all(config.daemon.state_dir.join("spool")).unwrap();
    let report = DiscoveryReport {
        libraries: worlds
            .iter()
            .map(|world| world.lock().unwrap().library_snapshot())
            .collect(),
        unattached_drives: vec![],
        complete: true,
        incomplete_evidence: vec![],
        warnings: vec![],
    };
    let policy = StaticAllowlist::new((0..worlds.len()).map(serial));
    ApiState::with_drive_pool_opener(
        CatalogIndex::open(&config.index.sqlite_path).unwrap(),
        config,
        report,
        policy,
        config.daemon.state_dir.join("spool"),
        64 * 1024 * 1024,
        |library, policy| {
            let n = (0..worlds.len())
                .find(|n| library.serial == serial(*n))
                .unwrap();
            let world = worlds[n].clone();
            let control = controls[n].clone();
            library.open_with(policy, move |path| {
                let role = world
                    .lock()
                    .unwrap()
                    .role_for_path(path)
                    .expect("only virtual device paths are admitted");
                Ok(Box::new(Transport {
                    inner: ModelTransport::new(world.clone(), role),
                    control: control.clone(),
                    world: world.clone(),
                }) as Box<dyn SgTransport>)
            })
        },
    )
    .unwrap()
}

impl Rig {
    fn new(libraries: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = config(root.path(), libraries);
        let paths = StatePaths::from_config(root.path().join("config.toml"), &config);
        let mut state = StateHandle::open_with_config(paths, config.clone()).unwrap();
        let mut worlds = Vec::new();
        for n in 0..libraries {
            let parity = if n == 1 {
                parse_parity_arg("custom:4,2,1").unwrap()
            } else {
                ParityConfig::None
            };
            let geometry = if n == 1 {
                "scheme=rs-cauchy-gf256-v1 data=4 parity=2 stripes=1"
            } else {
                "no-parity"
            };
            state
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::System,
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                    event: AuditEvent::TapeProvisioned,
                    subject: AuditSubject {
                        kind: "tape".into(),
                        id: Some(uuid::Uuid::from_bytes(uuid(n)).to_string()),
                    },
                    detail: BTreeMap::from([
                        ("tape_uuid".into(), Value::Bytes(uuid(n).to_vec())),
                        ("voltag".into(), Value::Text(barcode(n))),
                        ("block_size".into(), Value::Integer(BLOCK.into())),
                        ("geometry".into(), Value::Text(geometry.into())),
                    ]),
                })
                .unwrap();
            let payload = build_tape_bootstrap(
                uuid(n),
                BLOCK,
                parity.clone(),
                "2026-10-29T00:00:00Z",
                "acceptance",
            );
            let mut block = vec![0; BLOCK as usize];
            write_bootstrap_block(&payload, &mut block).unwrap();
            let mut tape = VirtualTape::empty(8 * 1024 * 1024 * 1024, BLOCK);
            tape.records = vec![Record::Block(block), Record::Filemark];
            tape.written_bytes = u64::from(BLOCK);
            let mut world =
                VirtualWorld::single_drive(serial(n), BAY, format!("DRIVE-{n}"), SLOT, 2);
            world.put_tape_in_slot(SLOT, barcode(n), tape.clone());
            // A second eligible tape makes drive exhaustion distinct from a
            // second claim on the already mounted tape.
            let spare_id = uuid(n + 10);
            let payload = build_tape_bootstrap(
                spare_id,
                BLOCK,
                parity.clone(),
                "2026-10-29T00:00:00Z",
                "acceptance",
            );
            let mut block = vec![0; BLOCK as usize];
            write_bootstrap_block(&payload, &mut block).unwrap();
            tape.records[0] = Record::Block(block);
            world.put_tape_in_slot(SLOT + 1, barcode(n + 10), tape);
            state
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::System,
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                    event: AuditEvent::TapeProvisioned,
                    subject: AuditSubject {
                        kind: "tape".into(),
                        id: Some(uuid::Uuid::from_bytes(spare_id).to_string()),
                    },
                    detail: BTreeMap::from([
                        ("tape_uuid".into(), Value::Bytes(spare_id.to_vec())),
                        ("voltag".into(), Value::Text(barcode(n + 10))),
                        ("block_size".into(), Value::Integer(BLOCK.into())),
                        ("geometry".into(), Value::Text(geometry.into())),
                    ]),
                })
                .unwrap();
            worlds.push(Arc::new(Mutex::new(world)));
        }
        state.replay_audit_projection().unwrap();
        for n in 0..libraries {
            for tape in [n, n + 10] {
                let generation = state
                    .catalog_index()
                    .get_tape_assignment_snapshot(&uuid(tape))
                    .unwrap()
                    .unwrap()
                    .assignment_generation;
                state
                    .assign_tape_pool(uuid(tape), generation, Some(&pool(n)))
                    .unwrap();
            }
        }
        drop(state);
        let controls = (0..libraries)
            .map(|_| Arc::new(Control::default()))
            .collect::<Vec<_>>();
        let api = start(&config, &worlds, &controls);
        Self {
            root,
            config,
            worlds,
            controls,
            api,
        }
    }

    async fn open(&self, n: usize) -> pb::WriteSession {
        tokio::time::timeout(BOUND, open(&self.api, n))
            .await
            .unwrap()
            .unwrap()
    }

    async fn close(&self, session: &pb::WriteSession) {
        self.api
            .write_session_service()
            .close_write_session(Request::new(pb::CloseWriteSessionRequest {
                session_id: session.session_id.clone(),
                ..Default::default()
            }))
            .await
            .unwrap();
    }

    async fn abort(&self, session: &pb::WriteSession) {
        self.api
            .write_session_service()
            .abort_write_session(Request::new(pb::AbortWriteSessionRequest {
                session_id: session.session_id.clone(),
                reason: Some("acceptance abort".into()),
                ..Default::default()
            }))
            .await
            .unwrap();
    }

    async fn checkpoint(&self, session: &pb::WriteSession) {
        self.api
            .write_session_service()
            .checkpoint_session(Request::new(pb::CheckpointSessionRequest {
                session_id: session.session_id.clone(),
                ..Default::default()
            }))
            .await
            .unwrap();
    }

    async fn assert_released(&self) {
        // Live status is cached; wait for a fresh observation, then check every
        // exposed field and the durable session rows below.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let status = self
                .api
                .library_service()
                .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
                .await
                .unwrap()
                .into_inner();
            if status.drive_assignments.iter().all(|drive| {
                drive.state == pb::drive_assignment::State::DriveAssignmentStateIdle as i32
            }) && status
                .libraries
                .iter()
                .flat_map(|library| &library.drives)
                .all(|drive| {
                    drive.status != pb::drive::Status::DriveStatusBusy as i32
                        && drive.session_id.is_none()
                })
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let status = self
            .api
            .library_service()
            .get_live_status(Request::new(pb::GetLiveStatusRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.drive_assignments.len(), self.worlds.len());
        for drive in status.drive_assignments {
            assert_eq!(
                drive.state,
                pb::drive_assignment::State::DriveAssignmentStateIdle as i32,
                "{drive:?}"
            );
            assert!(drive.current_session_id.is_none(), "{drive:?}");
        }
        for library in status.libraries {
            for drive in library.drives {
                assert!(drive.session_id.is_none(), "{drive:?}");
                assert_ne!(
                    drive.status,
                    pb::drive::Status::DriveStatusBusy as i32,
                    "{drive:?}"
                );
            }
        }
        let index = CatalogIndex::open(&self.config.index.sqlite_path).unwrap();
        assert!(
            index.non_terminal_sessions().unwrap().is_empty(),
            "no durable session remains active"
        );
        let connection = rusqlite::Connection::open(&self.config.index.sqlite_path).unwrap();
        let active: i64 = connection.query_row("SELECT count(*) FROM media_readiness_ops WHERE state NOT IN ('ready', 'released') AND coalesce(dirty_scope, 'drive+tape') <> 'none'", [], |row| row.get(0)).unwrap();
        assert_eq!(active, 0, "no durable readiness claim remains");
        for world in &self.worlds {
            assert!(world
                .lock()
                .unwrap()
                .drive_commands_in_flight
                .values()
                .all(|count| *count == 0));
        }
    }

    fn assert_loads(&self) {
        for (world, control) in self.worlds.iter().zip(&self.controls) {
            let world = world.lock().unwrap();
            let commands = control.commands.lock().unwrap();
            let recorded: Vec<_> = commands.iter().map(|(role, cdb)| (*role, cdb[0])).collect();
            let modeled: Vec<_> = world
                .command_log
                .iter()
                .map(|c| (c.role, c.opcode))
                .collect();
            assert_eq!(
                recorded, modeled,
                "every observed command reaches the model"
            );
            let mut placements = Vec::<usize>::new();
            let mut seated = false;
            for (role, cdb) in commands.iter() {
                if *role == DeviceRole::Changer && cdb[0] == 0xa5 {
                    let source = u16::from_be_bytes([cdb[4], cdb[5]]);
                    let destination = u16::from_be_bytes([cdb[6], cdb[7]]);
                    if destination == BAY {
                        placements.push(0);
                        seated = true;
                    }
                    if source == BAY {
                        seated = false;
                    }
                }
                if *role == (DeviceRole::Drive { bay: BAY }) && cdb[0] == 0x1b && cdb[4] & 1 != 0 {
                    assert!(seated, "LOAD belongs to a recorded placement");
                    *placements.last_mut().unwrap() += 1;
                }
            }
            assert!(!placements.is_empty(), "scenario exercises a placement");
            assert!(
                placements.iter().all(|loads| *loads == 1),
                "LOAD counts per placement: {placements:?}"
            );
        }
    }

    async fn finish(&self) {
        self.assert_released().await;
        // Each drive accepts another cartridge. This also exercises the home
        // slot after parking and exposes stranded drive, tape, or slot claims.
        for n in 0..self.worlds.len() {
            let session = tokio::time::timeout(BOUND, open_tape(&self.api, n + 10, n))
                .await
                .unwrap()
                .unwrap();
            self.close(&session).await;
            // A finalized tape remains readable. Reopening the original tape
            // checks its owner as well as the drive and the spare's home slot.
            let reader = self.api.read_session_service();
            let session = tokio::time::timeout(
                BOUND,
                reader.open_read_session(Request::new(pb::OpenReadSessionRequest {
                    target: Some(pb::open_read_session_request::Target::TapeTarget(
                        pb::TapeTarget {
                            tape_uuid: uuid(n).to_vec(),
                            mount_if_needed: true,
                            required_pool_id: pool(n),
                            allow_unpooled: false,
                        },
                    )),
                    ..Default::default()
                })),
            )
            .await
            .unwrap()
            .unwrap()
            .into_inner();
            reader
                .close_read_session(Request::new(pb::CloseReadSessionRequest {
                    session_id: session.session_id,
                    ..Default::default()
                }))
                .await
                .unwrap();
        }
        self.assert_released().await;
        self.assert_loads();
        self.api.begin_shutdown();
        self.api.shutdown_drive_pool().await.unwrap();
        self.assert_loads();
    }
}

async fn open(api: &ApiState, n: usize) -> Result<pb::WriteSession, Status> {
    open_tape(api, n, n).await
}

async fn open_tape(
    api: &ApiState,
    n: usize,
    pool_number: usize,
) -> Result<pb::WriteSession, Status> {
    api.write_session_service()
        .open_write_session(Request::new(pb::OpenWriteSessionRequest {
            target: Some(pb::open_write_session_request::Target::TapeTarget(
                pb::TapeTarget {
                    tape_uuid: uuid(n).to_vec(),
                    mount_if_needed: true,
                    required_pool_id: pool(pool_number),
                    allow_unpooled: false,
                },
            )),
            ..Default::default()
        }))
        .await
        .map(tonic::Response::into_inner)
}

async fn append(
    api: &ApiState,
    session: &pb::WriteSession,
    name: &str,
) -> Result<pb::ObjectRecord, Status> {
    use pb::append_object_message::Payload;
    let id = session.session_id.clone();
    let messages = vec![
        Payload::Start(pb::AppendObjectStart {
            session_id: id.clone(),
            caller_object_id: name.into(),
            declared_size_bytes: Some(7),
            ..Default::default()
        }),
        Payload::Chunk(pb::AppendObjectChunk {
            session_id: id.clone(),
            data: b"payload".to_vec(),
        }),
        Payload::Finish(pb::AppendObjectFinish {
            session_id: id,
            ..Default::default()
        }),
    ]
    .into_iter()
    .map(|payload| {
        Ok(pb::AppendObjectMessage {
            payload: Some(payload),
        })
    });
    api.write_session_service()
        .append_object_from_stream(Request::new(tokio_stream::iter(messages)))
        .await
        .map(tonic::Response::into_inner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_release_claims_and_load_once_per_placement() {
    let rig = Rig::new(2);
    for n in 0..2 {
        for turn in 0..2 {
            let session = rig.open(n).await;
            append(&rig.api, &session, &format!("object-{n}-{turn}"))
                .await
                .unwrap();
            rig.checkpoint(&session).await;
            rig.close(&session).await;
            rig.assert_released().await;
        }
        let session = rig.open(n).await;
        rig.abort(&session).await;
        rig.assert_released().await;
        let session = rig.open(n).await;
        rig.close(&session).await;
    }
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_opens_admit_only_one_writer() {
    let rig = Rig::new(1);
    let (a, b) = tokio::join!(open(&rig.api, 0), open(&rig.api, 0));
    let (session, refusal) = match (a, b) {
        (Ok(a), Err(b)) | (Err(b), Ok(a)) => (a, b),
        other => panic!("one admission: {other:?}"),
    };
    assert_eq!(refusal.code(), Code::FailedPrecondition);
    assert_eq!(
        rig.worlds[0].lock().unwrap().tapes[&barcode(0)].written_bytes,
        u64::from(BLOCK),
        "neither open writes an Object"
    );
    let before = rig.worlds[0].lock().unwrap().command_log.clone();
    let closing = request_finalize(&rig.api, 0).await.unwrap();
    assert_eq!(closing.outcome, pb::TapeFinalizationOutcome::Busy as i32);
    let verification = rig
        .api
        .catalog_service()
        .verify_tape_index(Request::new(pb::VerifyTapeIndexRequest {
            tape_uuid: uuid(0).to_vec(),
        }))
        .await
        .unwrap_err();
    assert_eq!(verification.code(), Code::FailedPrecondition);
    assert_eq!(
        rig.worlds[0].lock().unwrap().command_log,
        before,
        "competing closeout and verification dispatch no media command"
    );
    append(&rig.api, &session, "winner").await.unwrap();
    rig.close(&session).await;
    rig.assert_released().await;
    let next = rig.open(0).await;
    rig.close(&next).await;
    rig.finish().await;
}

async fn finalize(api: &ApiState, n: usize) -> Result<pb::TapeFinalization, Status> {
    let accepted = request_finalize(api, n).await?;
    assert_ne!(
        accepted.outcome,
        pb::TapeFinalizationOutcome::Busy as i32,
        "uncontended finalization is accepted"
    );
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let result = api
                .catalog_service()
                .get_tape_finalization(Request::new(pb::GetTapeFinalizationRequest {
                    tape_uuid: uuid(n).to_vec(),
                }))
                .await?
                .into_inner();
            if result.outcome != pb::TapeFinalizationOutcome::Finalizing as i32 {
                assert_eq!(
                    result.outcome,
                    pb::TapeFinalizationOutcome::Finalized as i32,
                    "{result:?}"
                );
                return Ok(result);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("finalization completes: {accepted:?}"))
}

async fn request_finalize(api: &ApiState, n: usize) -> Result<pb::TapeFinalization, Status> {
    api.catalog_service()
        .finalize_tape(Request::new(pb::FinalizeTapeRequest {
            tape_uuid: uuid(n).to_vec(),
            expected_pool_id: Some(pool(n)),
            reason: "acceptance closeout".into(),
            idempotency_key: Some(pb::IdempotencyKey {
                value: uuid::Uuid::new_v4().as_bytes().to_vec(),
            }),
        }))
        .await
        .map(tonic::Response::into_inner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opens_race_finalization_and_verification_without_writes() {
    let rig = Rig::new(1);
    let session = rig.open(0).await;
    append(&rig.api, &session, "committed").await.unwrap();
    rig.close(&session).await;
    rig.controls[0].arm(0x0a);
    let api = rig.api.clone();
    let closing = tokio::spawn(async move { finalize(&api, 0).await });
    rig.controls[0].entered().await;
    let before = rig.worlds[0].lock().unwrap().tapes[&barcode(0)].written_bytes;
    let (a, b) = tokio::join!(open(&rig.api, 0), open(&rig.api, 0));
    for result in [a, b] {
        assert_eq!(result.unwrap_err().code(), Code::FailedPrecondition);
    }
    assert_eq!(
        rig.worlds[0].lock().unwrap().tapes[&barcode(0)].written_bytes,
        before
    );
    rig.controls[0].release();
    closing.await.unwrap().unwrap();
    rig.assert_released().await;

    rig.controls[0].arm(0x08);
    let api = rig.api.clone();
    let verifying = tokio::spawn(async move {
        api.catalog_service()
            .verify_tape_index(Request::new(pb::VerifyTapeIndexRequest {
                tape_uuid: uuid(0).to_vec(),
            }))
            .await
    });
    rig.controls[0].entered().await;
    let before = rig.worlds[0].lock().unwrap().tapes[&barcode(0)].written_bytes;
    let (a, b) = tokio::join!(open(&rig.api, 0), open(&rig.api, 0));
    for result in [a, b] {
        assert_eq!(result.unwrap_err().code(), Code::FailedPrecondition);
    }
    assert_eq!(
        rig.worlds[0].lock().unwrap().tapes[&barcode(0)].written_bytes,
        before
    );
    rig.controls[0].release();
    tokio::time::timeout(Duration::from_secs(120), verifying)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_with_seated_cartridge_does_not_repeat_load() {
    let mut rig = Rig::new(1);
    let session = rig.open(0).await;
    append(&rig.api, &session, "before-restart").await.unwrap();
    rig.close(&session).await;
    rig.assert_released().await;
    let seated = rig.worlds[0].lock().unwrap().clone();
    let commands = rig.controls[0].commands.lock().unwrap().clone();
    rig.api.begin_shutdown();
    rig.api.shutdown_drive_pool().await.unwrap();
    rig.assert_loads();
    // Stop the old workers, then retain the device state at the restart cut.
    // Shutdown's orderly eject is not part of an abrupt daemon restart.
    *rig.worlds[0].lock().unwrap() = seated;
    *rig.controls[0].commands.lock().unwrap() = commands;
    assert_eq!(
        rig.worlds[0].lock().unwrap().loaded_barcode(BAY),
        Some(barcode(0).as_str())
    );
    rig.api = start(&rig.config, &rig.worlds, &rig.controls);
    let session = rig.open(0).await;
    rig.close(&session).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "acceptance: fails on main; expected from step 5"]
async fn dropping_open_at_readiness_releases_its_claims() {
    let rig = Rig::new(1);
    rig.controls[0].arm(0x00);
    let api = rig.api.clone();
    let opening = tokio::spawn(async move { open(&api, 0).await });
    rig.controls[0].entered().await;
    let before = rig.controls[0].commands.lock().unwrap().len();
    opening.abort();
    assert!(opening.await.unwrap_err().is_cancelled());
    rig.controls[0].release();
    rig.assert_released().await;
    assert!(
        rig.controls[0].commands.lock().unwrap()[before..]
            .iter()
            .all(|(_, cdb)| {
                !(cdb[0] == 0x1b && cdb[4] & 1 != 0
                    || cdb[0] == 0xa5 && u16::from_be_bytes([cdb[6], cdb[7]]) == BAY)
            }),
        "withdrawn open places and loads nothing after cancellation"
    );
    let session = rig.open(0).await;
    rig.close(&session).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_changer_does_not_block_another_library() {
    let rig = Rig::new(2);
    rig.controls[0].arm(0xa5);
    let api = rig.api.clone();
    let first = tokio::spawn(async move { open(&api, 0).await });
    rig.controls[0].entered().await;
    let commands = rig.worlds[0].lock().unwrap().command_log.clone();
    let second = rig.open(1).await;
    append(&rig.api, &second, "independent").await.unwrap();
    rig.close(&second).await;
    assert_eq!(
        rig.worlds[0].lock().unwrap().command_log,
        commands,
        "other library does not dispatch to the blocked devices"
    );
    rig.controls[0].release();
    let first = first.await.unwrap().unwrap();
    rig.close(&first).await;
    rig.finish().await;
}

fn release_fences(rig: &Rig) {
    let paths = StatePaths::from_config(rig.root.path().join("config.toml"), &rig.config);
    let mut state = StateHandle::open_with_config(paths, rig.config.clone()).unwrap();
    let fences = state.catalog_index().list_active_tape_io_fences().unwrap();
    assert!(!fences.is_empty(), "failed append raises a durable fence");
    for fence in fences {
        assert_eq!(fence.tape_uuid, uuid(0));
        state
            .release_tape_io_fence(&fence.quarantine_id, "acceptance release")
            .unwrap()
            .unwrap();
    }
    assert!(state
        .catalog_index()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_append_and_fence_leave_other_library_usable() {
    let rig = Rig::new(2);
    let first = rig.open(0).await;
    *rig.controls[0].fail_write.lock().unwrap() = true;
    rig.controls[0].arm(0x0a);
    let api = rig.api.clone();
    let session = first.clone();
    let failing = tokio::spawn(async move { append(&api, &session, "failed").await });
    rig.controls[0].entered().await;
    let before = rig.worlds[0].lock().unwrap().command_log.clone();
    let second = rig.open(1).await;
    append(&rig.api, &second, "during-fault").await.unwrap();
    rig.close(&second).await;
    assert_eq!(rig.worlds[0].lock().unwrap().command_log, before);
    rig.controls[0].release();
    let failure = failing.await.unwrap().unwrap_err();
    assert!(
        !*rig.controls[0].fail_write.lock().unwrap(),
        "the append reaches the injected write failure"
    );
    assert!(
        matches!(
            failure.code(),
            Code::Internal | Code::Unavailable | Code::FailedPrecondition
        ),
        "{failure}"
    );
    rig.abort(&first).await;
    rig.assert_released().await;
    let before = rig.worlds[0].lock().unwrap().command_log.clone();
    let refused = open(&rig.api, 0).await.unwrap_err();
    assert_eq!(refused.code(), Code::FailedPrecondition);
    let second = rig.open(1).await;
    append(&rig.api, &second, "unaffected").await.unwrap();
    rig.close(&second).await;
    assert_eq!(rig.worlds[0].lock().unwrap().command_log, before);
    release_fences(&rig);
    let next = rig.open(0).await;
    rig.close(&next).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "acceptance: fails on main; expected from step 5"]
async fn readiness_wait_does_not_block_another_library_or_repeat_load() {
    let rig = Rig::new(2);
    *rig.controls[0].not_ready.lock().unwrap() = true;
    rig.controls[0].arm(0x00);
    let api = rig.api.clone();
    let first = tokio::spawn(async move { open(&api, 0).await });
    rig.controls[0].entered().await;
    rig.controls[0].release();
    let second = rig.open(1).await;
    append(&rig.api, &second, "ready-library").await.unwrap();
    rig.close(&second).await;
    *rig.controls[0].not_ready.lock().unwrap() = false;
    let first = tokio::time::timeout(BOUND, first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    rig.close(&first).await;
    rig.finish().await;
}

#[derive(Debug, PartialEq)]
struct Table {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

// No table or column is normalized away. SQLite's own sequence allocator is
// included when present; a differing timestamp is a projection discrepancy too.
fn snapshot(path: &Path) -> BTreeMap<String, Table> {
    let connection = rusqlite::Connection::open(path).unwrap();
    let tables = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|name| {
            let quoted = name.replace('"', "\"\"");
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{quoted}\""))
                .unwrap();
            let columns = statement
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let mut rows = statement
                .query_map([], |row| {
                    (0..columns.len())
                        .map(|column| row.get_ref(column).map(|value| format!("{value:?}")))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows.sort();
            (name, Table { columns, rows })
        })
        .collect()
}

fn compare_catalogs(live: &BTreeMap<String, Table>, rebuilt: &BTreeMap<String, Table>) {
    assert_eq!(
        live.keys().collect::<Vec<_>>(),
        rebuilt.keys().collect::<Vec<_>>(),
        "all schema tables exist after rebuild"
    );
    let mut differences = Vec::new();
    for (name, left) in live {
        let right = &rebuilt[name];
        assert_eq!(left.columns, right.columns, "{name} columns");
        if left != right {
            let changed = left
                .columns
                .iter()
                .enumerate()
                .filter_map(|(column, name)| {
                    let mut a = left.rows.iter().map(|row| &row[column]).collect::<Vec<_>>();
                    let mut b = right
                        .rows
                        .iter()
                        .map(|row| &row[column])
                        .collect::<Vec<_>>();
                    a.sort();
                    b.sort();
                    (a != b).then_some(name.as_str())
                })
                .collect::<Vec<_>>();
            differences.push(format!(
                "{name}: live={} rebuilt={} rows; differing columns={changed:?}",
                left.rows.len(),
                right.rows.len()
            ));
        }
    }
    assert!(
        differences.is_empty(),
        "catalog replay differs:\n{}",
        differences.join("\n")
    );
}

fn copy_authority(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_authority(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn rebuild(rig: &Rig) -> BTreeMap<String, Table> {
    let empty = tempfile::tempdir().unwrap();
    let config = config(empty.path(), rig.worlds.len());
    let paths = StatePaths::from_config(empty.path().join("config.toml"), &config);
    StateHandle::reset_catalog_with_config(paths.clone(), config.clone()).unwrap();
    copy_authority(&rig.config.journal.dir, &config.journal.dir);
    copy_authority(&rig.config.audit.dir, &config.audit.dir);
    let mut state = StateHandle::open_with_config(paths, config.clone()).unwrap();
    state.rebuild_index_from_journals().unwrap();
    remanence_api::reconcile_checkpoint_journal_projections(&mut state).unwrap();
    snapshot(&config.index.sqlite_path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "acceptance: fails on main; expected from step 4"]
async fn empty_catalog_replay_matches_every_live_row() {
    let rig = Rig::new(2);
    for n in 0..2 {
        for turn in 0..2 {
            let session = rig.open(n).await;
            append(&rig.api, &session, &format!("replay-{n}-{turn}"))
                .await
                .unwrap();
            rig.checkpoint(&session).await;
            rig.close(&session).await;
        }
        let session = rig.open(n).await;
        rig.abort(&session).await;
    }
    let failed = rig.open(0).await;
    *rig.controls[0].fail_write.lock().unwrap() = true;
    assert!(append(&rig.api, &failed, "failed-replay").await.is_err());
    rig.abort(&failed).await;
    release_fences(&rig);
    finalize(&rig.api, 1).await.unwrap();
    rig.finish().await;
    let live = snapshot(&rig.config.index.sqlite_path);
    let rebuilt = rebuild(&rig);
    compare_catalogs(&live, &rebuilt);
}

#[tokio::test]
#[ignore = "acceptance: fails on main; expected from step 5"]
async fn queued_open_with_no_free_drive_can_be_withdrawn() {
    let rig = Rig::new(1);
    let first = rig.open(0).await;
    let before = rig.worlds[0].lock().unwrap().command_log.clone();
    let api = rig.api.clone();
    let (started, entered) = tokio::sync::oneshot::channel();
    let mut queued = tokio::spawn(async move {
        started.send(()).unwrap();
        open_tape(&api, 10, 0).await
    });
    entered.await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), &mut queued).await;
    assert!(
        outcome.is_err(),
        "no-free-drive open remains queued: {outcome:?}"
    );
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());
    assert_eq!(
        rig.worlds[0].lock().unwrap().command_log,
        before,
        "withdrawal does not dispatch to a device"
    );
    rig.close(&first).await;
    rig.assert_released().await;
    let next = open_tape(&rig.api, 10, 0).await.unwrap();
    rig.close(&next).await;
    rig.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readiness_fence_resume_is_isolated_and_loads_once() {
    let rig = Rig::new(2);
    *rig.controls[0].not_ready.lock().unwrap() = true;
    let fence = open(&rig.api, 0).await.unwrap_err();
    assert_eq!(fence.code(), Code::FailedPrecondition);
    let operation_id = fence
        .metadata()
        .get_bin("remanence-readiness-operation-bin")
        .unwrap()
        .to_bytes()
        .unwrap()
        .to_vec();
    let before = rig.worlds[0].lock().unwrap().command_log.clone();
    let second = rig.open(1).await;
    append(&rig.api, &second, "during-readiness-fence")
        .await
        .unwrap();
    rig.close(&second).await;
    // Readiness may poll its own drive, but library 1 never moves or writes it.
    let after = rig.worlds[0].lock().unwrap().command_log.clone();
    assert!(after[before.len()..]
        .iter()
        .all(|command| !matches!(command.opcode, 0xa5 | 0x0a | 0x10 | 0x1b)));
    *rig.controls[0].not_ready.lock().unwrap() = false;
    rig.api
        .library_service()
        .resume_media_readiness(Request::new(pb::ResumeMediaReadinessRequest {
            operation_id,
            timeout_seconds: 10,
            poll_interval_seconds: 1,
        }))
        .await
        .unwrap();
    let first = tokio::time::timeout(BOUND, async {
        loop {
            match open(&rig.api, 0).await {
                Ok(session) => break session,
                Err(status) if status.code() == Code::FailedPrecondition => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(status) => panic!("resume admission: {status}"),
            }
        }
    })
    .await
    .unwrap();
    rig.close(&first).await;
    rig.finish().await;
}
