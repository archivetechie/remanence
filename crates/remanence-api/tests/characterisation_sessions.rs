//! Client-visible storage contracts over gRPC and virtual tape media.
//!
//! | ID | Observable behaviour | Boundary coverage and limits |
//! | --- | --- | --- |
//! | B01 | Acknowledged objects survive process exit and remain listed and readable. | API crash, restart and read tests; CLI fresh-command reads. |
//! | B02 | Later sessions and cartridge removal preserve earlier committed data. | API successive-session and restart tests; CLI successive puts. |
//! | B03 | Checkpoint and close return the durable objects the caller can release. | API explicit and automatic checkpoint tests. |
//! | B04 | Retrying the same object returns its identity without writing another copy. | API retry and CLI unchanged-file tests. |
//! | B05 | Abort preserves committed objects and hides unfinished objects. | API abort test; standalone exposes no abort command. |
//! | B06 | Conflicting access is refused while a tape is active and succeeds after close. | API conflicting-open test; internal handoff races remain lower-level tests. |
//! | B07 | Loading waits without data commands and eventually admits access without operator action. | API delayed-load and interrupted-unload tests. |
//! | B08 | Operators can observe loading, cancel work and resume safely after interruption. | API health during loading; cancellation/restart state matrix remains lower-level coverage. |
//! | B09 | Wrong pool, wrong cartridge and unsafe append targets are refused before media writes. | API pool, identity and unexpected-tail tests; CLI pool refusal. |
//! | B10 | Uncertain or failed writes do not become successful committed copies. | API lost-write-reply test; host journal and catalog failure cuts remain lower-level tests. |
//! | B11 | Finalized tapes remain readable and refuse more writes. | API finalization and bounded read-reopen tests; standalone lifecycle routing changes intentionally. |
//! | B12 | Catalog replay preserves committed copies, tape state and operator decisions. | Restart object/copy reads are covered; operator-state rebuild matrix has no public rebuild RPC. |
//! | B13 | State opens only when its format can be interpreted without losing data. | Format and migration tests remain; historical migration is outside the retained contract. |
//! | B14 | Resource exhaustion refuses work without damaging committed data. | API oversized-append test; internal memory-pressure cuts remain lower-level tests. |
//! | B15 | Standalone commands list and restore the files they acknowledge. | CLI dispatcher tests on fresh command invocations. |
//!
//! The fixture supplies media and initial configuration only. All session actions
//! and assertions on stored objects use generated RPC clients over HTTP/2.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper_util::rt::TokioIo;
use remanence_api::{pb, ApiState};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::transport::TransferOutcome;
use remanence_library::{DiscoveryReport, ScsiError, SgTransport, StaticAllowlist, TimeoutClass};
use remanence_parity::{bootstrap::write_bootstrap_block, ParityConfig};
use remanence_state::{CatalogIndex, ProvisionTapeInput, RemConfig};
use sha2::{Digest, Sha256};
use tonic::transport::{Channel, Endpoint, Server};

const UUID: [u8; 16] = [0x35; 16];
const BLOCK: u32 = 262144;
const LIBRARY: &str = "CHARACTERISATION-LIBRARY";
const BAY: u16 = 0x100;
const BARCODE: &str = "CHAR01L9";

struct Harness {
    root: std::path::PathBuf,
    _temp: Option<tempfile::TempDir>,
    world: SharedVirtualWorld,
    config: RemConfig,
    injection: Arc<Mutex<Injection>>,
}

impl Harness {
    fn new(parity: bool) -> Self {
        let temp = if let Some(parent) = std::env::var_os("CHARACTERISATION_CRASH_PARENT") {
            tempfile::tempdir_in(parent).unwrap()
        } else {
            tempfile::tempdir().unwrap()
        };
        let root = temp.path().to_path_buf();
        let path = root.display();
        let config = remanence_state::parse_config_toml(&format!(
            r#"
[daemon]
state_dir = "{path}"
default_idle_timeout_seconds = 1800
io_memory_ceiling = "64 MiB"
append_ring_bytes = "16 MiB"
checkpoint_max_objects = 100
checkpoint_max_age_seconds = 3600
[tape_io]
read_reservoir_bytes = "16 MiB"
[[libraries]]
serial = "{LIBRARY}"
[[tape_pools]]
id = "archive"
block_size = 262144
[[tape_pools]]
id = "other"
block_size = 262144
[journal]
dir = "{path}/journals"
require_trusted_volume = false
[audit]
dir = "{path}/audit"
fsync = true
[index]
sqlite_path = "{path}/index.sqlite"
[cache]
tape_catalog_dir = "{path}/cache"
"#
        ))
        .unwrap();
        let parity = if parity {
            ParityConfig::Scheme(remanence_parity::ParityScheme {
                id: remanence_parity::SchemeId::new_static("rs-test"),
                data_blocks_per_stripe: 2,
                parity_blocks_per_stripe: 1,
                stripes_per_neighborhood: 3,
            })
        } else {
            ParityConfig::None
        };
        let mut index = CatalogIndex::open(&config.index.sqlite_path).unwrap();
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: UUID,
                voltag: BARCODE.into(),
                block_size: BLOCK,
                parity: parity.clone(),
                force: false,
            })
            .unwrap();
        index.project_tape_pool_membership(UUID, "archive").unwrap();
        let bootstrap = remanence_api::build_tape_bootstrap(
            UUID,
            BLOCK,
            parity,
            "2026-10-29T00:00:00Z",
            "characterisation",
        );
        let mut block = vec![0; BLOCK as usize];
        write_bootstrap_block(&bootstrap, &mut block).unwrap();
        let mut tape = VirtualTape::empty(18_000_000_000_000, BLOCK);
        tape.records = vec![Record::Block(block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK);
        let mut world =
            VirtualWorld::single_drive(LIBRARY, BAY, "CHARACTERISATION-DRIVE", 0x400, 2);
        world.put_tape_in_drive(BAY, BARCODE, Some(0x400), tape);
        Self {
            root,
            _temp: Some(temp),
            world: Arc::new(Mutex::new(world)),
            config,
            injection: Arc::default(),
        }
    }

    async fn start(&self) -> Client {
        let report = DiscoveryReport {
            libraries: vec![self.world.lock().unwrap().library_snapshot()],
            unattached_drives: vec![],
            complete: true,
            incomplete_evidence: vec![],
            warnings: vec![],
        };
        let world = Arc::clone(&self.world);
        let injection = Arc::clone(&self.injection);
        let spool = self.root.join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let state = ApiState::with_drive_pool_opener(
            CatalogIndex::open(&self.config.index.sqlite_path).unwrap(),
            &self.config,
            report,
            StaticAllowlist::new([LIBRARY]),
            spool,
            64 * 1024 * 1024,
            move |library, policy| {
                let world = Arc::clone(&world);
                let injection = Arc::clone(&injection);
                library.open_with(policy, move |path| {
                    let role = world
                        .lock()
                        .unwrap()
                        .role_for_path(path)
                        .expect("model path");
                    Ok(Box::new(Transport {
                        inner: ModelTransport::new(Arc::clone(&world), role),
                        role,
                        injection: Arc::clone(&injection),
                    }) as Box<dyn SgTransport>)
                })
            },
        )
        .unwrap();
        let (client, server) = tokio::io::duplex(1024 * 1024);
        let task = tokio::spawn(
            Server::builder()
                .add_service(
                    pb::write_session_service_server::WriteSessionServiceServer::new(
                        state.write_session_service(),
                    ),
                )
                .add_service(
                    pb::read_session_service_server::ReadSessionServiceServer::new(
                        state.read_session_service(),
                    ),
                )
                .add_service(pb::catalog_server::CatalogServer::new(
                    state.catalog_service(),
                ))
                .add_service(pb::daemon_server::DaemonServer::new(state.daemon_service()))
                .add_service(pb::library_service_server::LibraryServiceServer::new(
                    state.library_service(),
                ))
                .serve_with_incoming(tokio_stream::iter([Ok::<_, std::io::Error>(server)])),
        );
        let client = Arc::new(Mutex::new(Some(client)));
        let channel = Endpoint::from_static("http://model.invalid")
            .connect_with_connector(tower::service_fn(move |_| {
                let client = client.lock().unwrap().take();
                async move {
                    client
                        .map(TokioIo::new)
                        .ok_or_else(|| std::io::Error::other("connection already used"))
                }
            }))
            .await
            .unwrap();
        Client {
            channel,
            state,
            task,
        }
    }
}

struct Client {
    channel: Channel,
    state: ApiState,
    task: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    fn writer(&self) -> pb::write_session_service_client::WriteSessionServiceClient<Channel> {
        pb::write_session_service_client::WriteSessionServiceClient::new(self.channel.clone())
    }
    async fn open(&self) -> pb::WriteSession {
        self.writer()
            .open_write_session(open_request("archive"))
            .await
            .unwrap()
            .into_inner()
    }
    async fn append(
        &self,
        session: &pb::WriteSession,
        name: &str,
        bytes: &[u8],
    ) -> pb::ObjectRecord {
        self.try_append(session, name, bytes).await.unwrap()
    }
    async fn try_append(
        &self,
        session: &pb::WriteSession,
        name: &str,
        bytes: &[u8],
    ) -> Result<pb::ObjectRecord, tonic::Status> {
        use pb::append_object_message::Payload;
        let digest = Sha256::digest(bytes).to_vec();
        let messages = vec![
            Payload::Start(pb::AppendObjectStart {
                session_id: session.session_id.clone(),
                caller_object_id: name.into(),
                declared_size_bytes: Some(bytes.len() as u64),
                expected_content_sha256: Some(digest.clone()),
                source_replay_capability: pb::SourceReplayCapability::ReplayFromStart as i32,
                ..Default::default()
            }),
            Payload::Chunk(pb::AppendObjectChunk {
                session_id: session.session_id.clone(),
                data: bytes.to_vec(),
            }),
            Payload::Finish(pb::AppendObjectFinish {
                session_id: session.session_id.clone(),
                expected_content_sha256: Some(digest),
                ..Default::default()
            }),
        ];
        self.writer()
            .append_object(tokio_stream::iter(messages.into_iter().map(|payload| {
                pb::AppendObjectMessage {
                    payload: Some(payload),
                }
            })))
            .await
            .map(tonic::Response::into_inner)
    }
    async fn checkpoint(&self, session: &pb::WriteSession) -> pb::CheckpointSessionResponse {
        self.writer()
            .checkpoint_session(pb::CheckpointSessionRequest {
                session_id: session.session_id.clone(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
    }
    async fn close(&self, session: &pb::WriteSession) -> pb::WriteSession {
        self.writer()
            .close_write_session(pb::CloseWriteSessionRequest {
                session_id: session.session_id.clone(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner()
    }
    async fn assert_object(&self, object: &pb::ObjectRecord, bytes: &[u8]) {
        let catalog = pb::catalog_client::CatalogClient::new(self.channel.clone())
            .get_object(pb::GetObjectRequest {
                pool_id: String::new(),
                key: Some(pb::get_object_request::Key::ObjectId(
                    object.object_id.clone(),
                )),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(catalog.object_id, object.object_id);
        assert!(!catalog.copies.is_empty());
        let mut reader =
            pb::read_session_service_client::ReadSessionServiceClient::new(self.channel.clone());
        let request = pb::OpenReadSessionRequest {
            target: Some(pb::open_read_session_request::Target::TapeTarget(
                tape_target("archive"),
            )),
            ..Default::default()
        };
        // Completion can precede release of the operation's drive by a task turn.
        let session = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match reader.open_read_session(request.clone()).await {
                    Ok(session) => break session.into_inner(),
                    Err(error) => {
                        assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}")
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("completed work releases the tape for reading");
        let mut stream = reader
            .read_object_range(pb::ReadObjectRangeRequest {
                session_id: session.session_id.clone(),
                object_id: object.object_id.clone(),
                ..Default::default()
            })
            .await
            .unwrap()
            .into_inner();
        let mut actual = Vec::new();
        while let Some(chunk) = stream.message().await.unwrap() {
            actual.extend(chunk.data);
        }
        assert_eq!(actual, bytes);
        reader
            .close_read_session(pb::CloseReadSessionRequest {
                session_id: session.session_id,
                ..Default::default()
            })
            .await
            .unwrap();
    }
}

fn tape_target(pool: &str) -> pb::TapeTarget {
    pb::TapeTarget {
        tape_uuid: UUID.to_vec(),
        mount_if_needed: true,
        required_pool_id: pool.into(),
        allow_unpooled: false,
    }
}
fn open_request(pool: &str) -> pb::OpenWriteSessionRequest {
    pb::OpenWriteSessionRequest {
        target: Some(pb::open_write_session_request::Target::TapeTarget(
            tape_target(pool),
        )),
        ..Default::default()
    }
}

#[tokio::test]
async fn checkpoint_and_close_report_objects_that_remain_readable_after_restart() {
    for parity in [false, true] {
        let fixture = Harness::new(parity);
        let client = fixture.start().await;
        let session = client.open().await;
        let first = client.append(&session, "first", b"checkpoint bytes").await;
        let second = client
            .append(&session, "second", b"more checkpoint bytes")
            .await;
        let checkpoint = client.checkpoint(&session).await;
        assert_eq!(
            checkpoint
                .committed_objects
                .iter()
                .map(|o| &o.object_id)
                .collect::<Vec<_>>(),
            vec![&first.object_id, &second.object_id]
        );
        assert_eq!(checkpoint.committed_copies.len(), 2);
        assert!(client
            .checkpoint(&session)
            .await
            .committed_objects
            .is_empty());
        let third = client.append(&session, "third", b"close bytes").await;
        let closed = client.close(&session).await;
        assert_eq!(closed.checkpointed_objects.len(), 1);
        assert_eq!(closed.checkpointed_objects[0].object_id, third.object_id);
        client.state.shutdown_drive_pool().await.unwrap();
        drop(client);
        let client = fixture.start().await;
        client.assert_object(&first, b"checkpoint bytes").await;
        client
            .assert_object(&second, b"more checkpoint bytes")
            .await;
        client.assert_object(&third, b"close bytes").await;
        client.state.shutdown_drive_pool().await.unwrap();
    }
}

#[tokio::test]
async fn later_session_preserves_earlier_objects() {
    let fixture = Harness::new(true);
    let client = fixture.start().await;
    let session = client.open().await;
    let first = client.append(&session, "first", b"first session").await;
    client.close(&session).await;
    let session = client.open().await;
    let second = client.append(&session, "second", b"second session").await;
    client.close(&session).await;
    client.assert_object(&first, b"first session").await;
    client.assert_object(&second, b"second session").await;
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn active_write_excludes_conflicting_access_until_close() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let error = client
        .writer()
        .open_write_session(open_request("archive"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    client.close(&session).await;
    let next = client.open().await;
    client.close(&next).await;
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn wrong_pool_refuses_write_and_preserves_existing_object() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let object = client.append(&session, "first", b"right pool").await;
    client.close(&session).await;
    let error = client
        .writer()
        .open_write_session(open_request("other"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    client.assert_object(&object, b"right pool").await;
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn abort_keeps_checkpointed_data_and_hides_uncommitted_object() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let committed = client
        .append(&session, "committed", b"keep these bytes")
        .await;
    client.checkpoint(&session).await;
    let pending = client
        .append(&session, "pending", b"discard these bytes")
        .await;
    client
        .writer()
        .abort_write_session(pb::AbortWriteSessionRequest {
            session_id: session.session_id,
            reason: Some("caller stops".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let error = pb::catalog_client::CatalogClient::new(client.channel.clone())
        .get_object(pb::GetObjectRequest {
            pool_id: String::new(),
            key: Some(pb::get_object_request::Key::ObjectId(pending.object_id)),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
    client.assert_object(&committed, b"keep these bytes").await;
    client.state.shutdown_drive_pool().await.unwrap();
}

/// Raw transport evidence supplements the world model; it never supplies RPC outcomes.
#[derive(Default)]
struct Injection {
    load_delay: Duration,
    waiting_until: Option<Instant>,
    waiting_commands: Vec<u8>,
    move_full: bool,
    move_failures: usize,
    kernel_omits_residual: bool,
    corrected_eod: usize,
    lose_write_reply: bool,
    lost_write_replies: usize,
}
struct Transport {
    inner: ModelTransport,
    role: DeviceRole,
    injection: Arc<Mutex<Injection>>,
}
impl Transport {
    fn before(&self, cdb: &[u8]) -> Result<(), ScsiError> {
        let mut injection = self.injection.lock().unwrap();
        if matches!(self.role, DeviceRole::Drive { .. })
            && injection
                .waiting_until
                .is_some_and(|until| Instant::now() < until)
        {
            injection.waiting_commands.push(cdb[0]);
            if cdb[0] == 0 {
                return Err(check_condition(2, 4, 1));
            }
        }
        if cdb[0] == 0xa5 && injection.move_full {
            injection.move_failures += 1;
            return Err(check_condition(5, 0x3b, 0x0e));
        }
        Ok(())
    }
}
fn check_condition(key: u8, asc: u8, ascq: u8) -> ScsiError {
    let mut sense = vec![0; 18];
    sense[0] = 0x70;
    sense[2] = key;
    sense[7] = 10;
    sense[12] = asc;
    sense[13] = ascq;
    ScsiError::CheckCondition {
        sense,
        bytes_transferred: 0,
    }
}
impl SgTransport for Transport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.before(cdb)?;
        let result = self.inner.execute_in(cdb, buf);
        if let Err(ScsiError::CheckCondition { sense, .. }) = &result {
            let mut injection = self.injection.lock().unwrap();
            if injection.kernel_omits_residual
                && cdb[0] == 8
                && sense.get(2).is_some_and(|key| key & 15 == 8)
            {
                // Recorded variable READ EOD: information=262144, SG_IO resid=0.
                let sense = vec![
                    0xf0, 0, 8, 0, 4, 0, 0, 0x58, 0, 0, 0, 0, 0, 5, 0x30, 0, 0x64, 2, 0x20, 0, 6,
                    1, 0x41, 0x4f, 0x58, 0x30, 0x33, 0x32, 0x4c, 0, 0, 0,
                ];
                assert_eq!(cdb[1] & 1, 0, "recorded outcome is a variable READ");
                assert_eq!(buf.len(), BLOCK as usize);
                injection.corrected_eod += 1;
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: BLOCK,
                });
            }
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.before(cdb)?;
        let outcome = self.inner.execute_out(cdb, buf)?;
        let mut injection = self.injection.lock().unwrap();
        if cdb[0] == 0x0a && injection.lose_write_reply {
            injection.lose_write_reply = false;
            injection.lost_write_replies += 1;
            return Err(ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: vec![],
            });
        }
        Ok(outcome)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.before(cdb)?;
        self.inner.execute_none(cdb)?;
        if (cdb[0] == 0x1b && cdb[4] & 1 != 0)
            || (cdb[0] == 0xa5 && u16::from_be_bytes([cdb[6], cdb[7]]) == BAY)
        {
            let mut injection = self.injection.lock().unwrap();
            injection.waiting_until = Some(Instant::now() + injection.load_delay);
        }
        Ok(())
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[tokio::test]
async fn first_append_after_load_accepts_recorded_eod_without_kernel_residual() {
    let fixture = Harness::new(false);
    {
        let mut world = fixture.world.lock().unwrap();
        world.drive_bays.insert(BAY, None);
        let tape = world.tapes[BARCODE].clone();
        world.put_tape_in_slot(0x400, BARCODE, tape);
    }
    fixture.injection.lock().unwrap().kernel_omits_residual = true;
    let client = fixture.start().await;
    let session = client.open().await;
    let object = client
        .append(&session, "eod", b"first append after load")
        .await;
    client.close(&session).await;
    assert!(
        fixture.injection.lock().unwrap().corrected_eod > 0,
        "recorded sense reaches the production reader"
    );
    client
        .assert_object(&object, b"first append after load")
        .await;
    client.state.shutdown_drive_pool().await.unwrap();
}

/// The production wait uses a wall clock. This test spends two minutes in 02/04/01.
#[tokio::test]
async fn first_load_optimisation_and_subsequent_load_wait_without_data_commands() {
    let fixture = Harness::new(false);
    {
        let mut world = fixture.world.lock().unwrap();
        world.drive_bays.insert(BAY, None);
        let tape = world.tapes[BARCODE].clone();
        world.put_tape_in_slot(0x400, BARCODE, tape);
    }
    for delay in [Duration::from_secs(120), Duration::from_secs(2)] {
        {
            let mut injection = fixture.injection.lock().unwrap();
            injection.load_delay = delay;
            injection.waiting_commands.clear();
        }
        let client = fixture.start().await;
        let started = Instant::now();
        let session = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                match client
                    .writer()
                    .open_write_session(open_request("archive"))
                    .await
                {
                    Ok(session) => break session.into_inner(),
                    Err(error) => {
                        assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}")
                    }
                }
                let health = pb::daemon_client::DaemonClient::new(client.channel.clone())
                    .health(())
                    .await
                    .unwrap()
                    .into_inner();
                assert_ne!(
                    health.status,
                    pb::health_response::Status::Unspecified as i32
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .expect("loading eventually admits a write without operator action");
        assert!(started.elapsed() >= delay);
        {
            let injection = fixture.injection.lock().unwrap();
            assert!(injection.waiting_commands.contains(&0));
            assert!(
                injection
                    .waiting_commands
                    .iter()
                    .all(|opcode| matches!(opcode, 0 | 0x12)),
                "only TUR and INQUIRY during loading: {:?}",
                injection.waiting_commands
            );
        }
        client.close(&session).await;
        client.state.shutdown_drive_pool().await.unwrap();
    }
}

#[tokio::test]
async fn interrupted_unload_is_rethreaded_before_reading() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let object = client
        .append(&session, "kept", b"survives interrupted unload")
        .await;
    client.close(&session).await;
    fixture
        .world
        .lock()
        .unwrap()
        .leave_cartridge_unthreaded(BAY);
    client
        .assert_object(&object, b"survives interrupted unload")
        .await;
    assert!(!fixture.world.lock().unwrap().drive_states[&BAY].initializing_command_required);
    client.state.shutdown_drive_pool().await.unwrap();
}

impl Harness {
    fn library_handle(&self) -> remanence_library::LibraryHandle {
        let library = self.world.lock().unwrap().library_snapshot();
        let world = Arc::clone(&self.world);
        let injection = Arc::clone(&self.injection);
        library
            .open_with(&StaticAllowlist::new([LIBRARY]), move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(Transport {
                    inner: ModelTransport::new(Arc::clone(&world), role),
                    role,
                    injection: Arc::clone(&injection),
                }) as Box<dyn SgTransport>)
            })
            .unwrap()
    }
}

/// Explicit robotics RPCs reopen hardware, so this case uses the public library boundary.
#[test]
fn move_check_condition_reports_failure_without_moving_cartridges() {
    let fixture = Harness::new(false);
    {
        let mut world = fixture.world.lock().unwrap();
        world.drive_bays.insert(BAY, None);
        let tape = world.tapes[BARCODE].clone();
        world.put_tape_in_slot(0x400, BARCODE, tape);
    }
    let mut library = fixture.library_handle();
    fixture.injection.lock().unwrap().move_full = true;
    let error = library
        .move_medium(0x400, 0x401, &StaticAllowlist::new([LIBRARY]))
        .unwrap_err();
    assert!(
        matches!(
            error,
            remanence_library::MoveError::DestinationFull { addr: 0x401 }
        ),
        "{error}"
    );
    assert_eq!(fixture.injection.lock().unwrap().move_failures, 1);
    assert!(
        !library.is_dirty(),
        "definite command rejection has a known outcome"
    );
    library.refresh().unwrap();
    assert_eq!(
        library
            .library()
            .slots
            .iter()
            .find(|slot| slot.element_address == 0x400)
            .unwrap()
            .cartridge
            .as_deref(),
        Some(BARCODE)
    );
    assert!(
        !library
            .library()
            .slots
            .iter()
            .find(|slot| slot.element_address == 0x401)
            .unwrap()
            .full
    );
}

/// A full slot is a definite refusal even when its barcode cannot be read.
#[test]
fn move_to_full_slot_preserves_both_cartridges() {
    for labelled in [false, true] {
        let fixture = Harness::new(false);
        {
            let mut world = fixture.world.lock().unwrap();
            world.drive_bays.insert(BAY, None);
            let tape = world.tapes[BARCODE].clone();
            world.put_tape_in_slot(0x400, BARCODE, tape);
            if labelled {
                world.put_tape_in_slot(
                    0x401,
                    "OTHERL9",
                    VirtualTape::empty(18_000_000_000_000, BLOCK),
                );
            } else {
                assert!(world.set_slot_full_without_voltag(0x401));
            }
        }
        let mut library = fixture.library_handle();
        let error = library
            .move_medium(0x400, 0x401, &StaticAllowlist::new([LIBRARY]))
            .unwrap_err();
        assert!(
            matches!(error, remanence_library::MoveError::DestinationFull { .. }),
            "{error}"
        );
        let world = fixture.world.lock().unwrap();
        assert!(world.slots.iter().all(|slot| slot.full));
        assert!(world
            .command_log
            .iter()
            .all(|command| command.opcode != 0xa5));
    }
}

#[test]
fn full_destination_refusal_leaves_source_cartridge_threaded() {
    let fixture = Harness::new(false);
    {
        let mut world = fixture.world.lock().unwrap();
        world.put_tape_in_slot(
            0x401,
            "OTHERL9",
            VirtualTape::empty(18_000_000_000_000, BLOCK),
        );
        world
            .drive_states
            .entry(BAY)
            .or_default()
            .unload_leaves_unthreaded = true;
    }
    let mut library = fixture.library_handle();
    let error = library
        .unload(BAY, Some(0x401), &StaticAllowlist::new([LIBRARY]))
        .unwrap_err();
    assert!(
        matches!(
            error,
            remanence_library::UnloadError::Move(
                remanence_library::MoveError::DestinationFull { .. }
            )
        ),
        "{error}"
    );
    let world = fixture.world.lock().unwrap();
    assert!(
        !world.drive_states[&BAY].initializing_command_required,
        "a full destination is refused before UNLOAD"
    );
    assert!(
        world
            .command_log
            .iter()
            .all(|command| command.opcode != 0x1b && command.opcode != 0xa5),
        "refusal sends neither UNLOAD nor MOVE"
    );
}

/// The child exits after the checkpoint response, without close or daemon shutdown.
#[tokio::test]
async fn acknowledged_object_survives_process_exit_without_close() {
    const NAME: &str = "acknowledged_object_survives_process_exit_without_close";
    const BYTES: &[u8] = b"acknowledged before process exit";
    if let Some(parent) = std::env::var_os("CHARACTERISATION_CRASH_PARENT") {
        let mut fixture = Harness::new(true);
        let client = fixture.start().await;
        let session = client.open().await;
        let object = client.append(&session, "crash", BYTES).await;
        let checkpoint = client.checkpoint(&session).await;
        assert_eq!(checkpoint.committed_objects[0].object_id, object.object_id);
        let world = fixture.world.lock().unwrap();
        let tape = &world.tapes[BARCODE];
        let records: Vec<(u8, Vec<u8>, u32)> = tape
            .records
            .iter()
            .map(|record| match record {
                Record::Block(bytes) => (0, bytes.clone(), 0),
                Record::ZeroBlock(size) => (1, vec![], *size),
                Record::Filemark => (2, vec![], 0),
            })
            .collect();
        let snapshot = serde_json::json!({
            "root": fixture.root, "config": fixture.config, "object_id": object.object_id,
            "records": records, "written_bytes": tape.written_bytes,
        });
        std::fs::write(
            std::path::Path::new(&parent).join("snapshot.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let _ = fixture._temp.take().unwrap().keep();
        // All tape bytes and state are durable; no actor gets a close request.
        std::process::exit(0);
    }
    let parent = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env("CHARACTERISATION_CRASH_PARENT", parent.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot: serde_json::Value =
        serde_json::from_slice(&std::fs::read(parent.path().join("snapshot.json")).unwrap())
            .unwrap();
    let mut fixture = Harness::new(true);
    fixture.root = serde_json::from_value(snapshot["root"].clone()).unwrap();
    fixture.config = serde_json::from_value(snapshot["config"].clone()).unwrap();
    let records: Vec<(u8, Vec<u8>, u32)> =
        serde_json::from_value(snapshot["records"].clone()).unwrap();
    {
        let mut world = fixture.world.lock().unwrap();
        let tape = world.tapes.get_mut(BARCODE).unwrap();
        tape.records = records
            .into_iter()
            .map(|(kind, bytes, size)| match kind {
                0 => Record::Block(bytes),
                1 => Record::ZeroBlock(size),
                2 => Record::Filemark,
                _ => panic!("invalid snapshot record"),
            })
            .collect();
        tape.written_bytes = snapshot["written_bytes"].as_u64().unwrap();
    }
    let object = pb::ObjectRecord {
        object_id: serde_json::from_value(snapshot["object_id"].clone()).unwrap(),
        ..Default::default()
    };
    let client = fixture.start().await;
    client.assert_object(&object, BYTES).await;
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn automatic_commits_remain_deliverable_at_checkpoint() {
    let mut fixture = Harness::new(false);
    fixture.config.daemon.checkpoint_max_objects = 1;
    let client = fixture.start().await;
    let session = client.open().await;
    let first = client
        .append(&session, "automatic-first", b"first automatic commit")
        .await;
    let second = client
        .append(&session, "automatic-second", b"second automatic commit")
        .await;
    let checkpoint = client.checkpoint(&session).await;
    assert_eq!(
        checkpoint
            .committed_objects
            .iter()
            .map(|object| &object.object_id)
            .collect::<Vec<_>>(),
        vec![&first.object_id, &second.object_id]
    );
    assert_eq!(checkpoint.committed_copies.len(), 2);
    assert!(client
        .checkpoint(&session)
        .await
        .committed_objects
        .is_empty());
    client.close(&session).await;
    client
        .assert_object(&first, b"first automatic commit")
        .await;
    client
        .assert_object(&second, b"second automatic commit")
        .await;
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn retrying_same_object_returns_same_identity_without_another_copy() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let first = client.append(&session, "retry", b"retry bytes").await;
    let repeated = client.append(&session, "retry", b"retry bytes").await;
    assert_eq!(first.object_id, repeated.object_id);
    assert_eq!(client.checkpoint(&session).await.committed_objects.len(), 1);
    client.close(&session).await;
    let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let session = client.open().await;
    let repeated = client.append(&session, "retry", b"retry bytes").await;
    assert_eq!(first.object_id, repeated.object_id);
    client.close(&session).await;
    assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
    client.assert_object(&first, b"retry bytes").await;
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn mismatched_loaded_identity_refuses_writes_without_changing_media() {
    let fixture = Harness::new(false);
    let bootstrap = remanence_api::build_tape_bootstrap(
        [0x36; 16],
        BLOCK,
        ParityConfig::None,
        "2026-10-29T00:00:00Z",
        "characterisation",
    );
    let mut block = vec![0; BLOCK as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    fixture
        .world
        .lock()
        .unwrap()
        .tapes
        .get_mut(BARCODE)
        .unwrap()
        .records[0] = Record::Block(block);
    let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let client = fixture.start().await;
    let error = client
        .writer()
        .open_write_session(open_request("archive"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
    assert!(fixture
        .world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .all(|command| !matches!(command.opcode, 0x0a | 0x8a | 0x10 | 0x80)));
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn unexpected_complete_tail_refuses_append_without_overwriting_data() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let object = client
        .append(&session, "safe", b"committed before extra tail")
        .await;
    client.close(&session).await;
    {
        let mut world = fixture.world.lock().unwrap();
        let tape = world.tapes.get_mut(BARCODE).unwrap();
        let extra = tape.records[2..].to_vec();
        assert!(extra
            .iter()
            .any(|record| matches!(record, Record::Block(_))));
        tape.written_bytes += extra
            .iter()
            .map(|record| match record {
                Record::Block(bytes) => bytes.len() as u64,
                Record::ZeroBlock(size) => u64::from(*size),
                Record::Filemark => 0,
            })
            .sum::<u64>();
        tape.records.extend(extra);
    }
    let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let error = client
        .writer()
        .open_write_session(open_request("archive"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
    // The catalog still names the earlier acknowledged object.
    let found = pb::catalog_client::CatalogClient::new(client.channel.clone())
        .get_object(pb::GetObjectRequest {
            pool_id: String::new(),
            key: Some(pb::get_object_request::Key::ObjectId(
                object.object_id.clone(),
            )),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.object_id, object.object_id);
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn finalized_tape_remains_readable_after_restart_and_refuses_more_writes() {
    check_finalized_access(true).await;
}

#[tokio::test]
async fn completed_finalization_releases_tape_for_reading_without_restart() {
    check_finalized_access(false).await;
}

async fn check_finalized_access(restart: bool) {
    let fixture = Harness::new(true);
    let client = fixture.start().await;
    let session = client.open().await;
    let object = client.append(&session, "final", b"finalized bytes").await;
    client.close(&session).await;
    let mut catalog = pb::catalog_client::CatalogClient::new(client.channel.clone());
    let request = pb::FinalizeTapeRequest {
        tape_uuid: UUID.to_vec(),
        expected_pool_id: Some("archive".into()),
        reason: "archive complete".into(),
        idempotency_key: Some(pb::IdempotencyKey {
            value: vec![0x37; 16],
        }),
    };
    let accepted = catalog
        .finalize_tape(request.clone())
        .await
        .unwrap()
        .into_inner();
    let finalized = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = catalog
                .get_tape_finalization(pb::GetTapeFinalizationRequest {
                    tape_uuid: UUID.to_vec(),
                })
                .await
                .unwrap()
                .into_inner();
            if status.outcome != pb::TapeFinalizationOutcome::Finalizing as i32 {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("finalization completes");
    assert_eq!(finalized.operation_id, accepted.operation_id);
    let mut daemon = pb::daemon_client::DaemonClient::new(client.channel.clone());
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let operation = daemon
                .get_operation(pb::GetOperationRequest {
                    operation_id: finalized.operation_id.clone().unwrap(),
                })
                .await
                .unwrap()
                .into_inner();
            if operation.state != pb::OperationState::Queued as i32
                && operation.state != pb::OperationState::Running as i32
            {
                assert_eq!(
                    operation.state,
                    pb::OperationState::Succeeded as i32,
                    "{operation:?}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("finalization operation completes");
    assert_eq!(
        finalized.outcome,
        pb::TapeFinalizationOutcome::Finalized as i32,
        "{finalized:?}"
    );
    if !restart {
        client.assert_object(&object, b"finalized bytes").await;
    }
    let repeated = catalog.finalize_tape(request).await.unwrap().into_inner();
    assert_eq!(repeated.operation_id, finalized.operation_id);
    assert_eq!(repeated.edition_digest, finalized.edition_digest);
    let error = client
        .writer()
        .open_write_session(open_request("archive"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    client.state.shutdown_drive_pool().await.unwrap();
    drop(client);
    if restart {
        let client = fixture.start().await;
        client.assert_object(&object, b"finalized bytes").await;
        client.state.shutdown_drive_pool().await.unwrap();
    }
}

#[tokio::test]
async fn lost_write_reply_never_reports_an_uncommitted_object_as_successful() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let committed = client.append(&session, "before-error", b"safe bytes").await;
    client.checkpoint(&session).await;
    fixture.injection.lock().unwrap().lose_write_reply = true;
    let error = client
        .try_append(&session, "lost-reply", b"unknown write bytes")
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal, "{error}");
    assert_eq!(fixture.injection.lock().unwrap().lost_write_replies, 1);
    let mut catalog = pb::catalog_client::CatalogClient::new(client.channel.clone());
    let error = catalog
        .get_object(pb::GetObjectRequest {
            pool_id: String::new(),
            key: Some(pb::get_object_request::Key::CallerObjectId(
                "lost-reply".into(),
            )),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::NotFound);
    let found = catalog
        .get_object(pb::GetObjectRequest {
            pool_id: String::new(),
            key: Some(pb::get_object_request::Key::ObjectId(
                committed.object_id.clone(),
            )),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(found.object_id, committed.object_id);
    client.state.shutdown_drive_pool().await.unwrap();
}

#[tokio::test]
async fn oversized_append_is_refused_without_damaging_committed_data() {
    let fixture = Harness::new(false);
    let client = fixture.start().await;
    let session = client.open().await;
    let object = client.append(&session, "safe-size", b"safe bytes").await;
    client.checkpoint(&session).await;
    let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let error = client
        .writer()
        .append_object(tokio_stream::iter([pb::AppendObjectMessage {
            payload: Some(pb::append_object_message::Payload::Start(
                pb::AppendObjectStart {
                    session_id: session.session_id.clone(),
                    caller_object_id: "too-large".into(),
                    declared_size_bytes: Some(65 * 1024 * 1024),
                    ..Default::default()
                },
            )),
        }]))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::ResourceExhausted, "{error}");
    assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
    client.close(&session).await;
    client.assert_object(&object, b"safe bytes").await;
    client.state.shutdown_drive_pool().await.unwrap();
}
