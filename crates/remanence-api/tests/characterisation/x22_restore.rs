//! Restore a live state copy through the daemon's gRPC services over one simulated drive.
//! The cartridges retain the writes made after the copy; reads, pool re-selection, motion
//! and explicit tail discard must respect the restored journal's committed prefix.
use super::*;

const X22_UUID2: [u8; 16] = [0x36; 16];
const X22_BARCODE2: &str = "CHAR02L9";
const X22_CATALOG: [&str; 3] = ["index.sqlite", "index.sqlite-wal", "index.sqlite-shm"];

fn x22_copy_tree(src: &std::path::Path, dst: &std::path::Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "spool" {
            continue;
        }
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            x22_copy_tree(&entry.path(), &dst.join(&name));
        } else if kind.is_file() {
            // A live sidecar may disappear between listing and copying, as with rsync.
            let _ = std::fs::copy(entry.path(), dst.join(&name));
        }
    }
}

fn x22_clear(root: &std::path::Path) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            std::fs::remove_dir_all(path).unwrap();
        } else {
            std::fs::remove_file(path).unwrap();
        }
    }
}

/// Put back one copy of all authority records, leaving the derived catalogue out.
fn x22_restore(root: &std::path::Path, snapshot: &std::path::Path) {
    x22_clear(root);
    x22_copy_tree(snapshot, root);
    for name in X22_CATALOG {
        let _ = std::fs::remove_file(root.join(name));
    }
}

/// The fixture's configuration as a file, for the offline rebuild command's entry point.
fn x22_write_config(root: &std::path::Path) {
    let path = root.display();
    std::fs::write(
        root.join("config.toml"),
        format!(
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
        ),
    )
    .unwrap();
}

/// The offline CLI's entry point, retaining its own error type.
fn x22_rebuild(
    root: &std::path::Path,
) -> Result<remanence_state::RebuildReport, remanence_state::StateError> {
    x22_write_config(root);
    StateHandle::rebuild_projection_from_config_file(root.join("config.toml"))
}

/// Start as `rem-daemon`'s entry does (preflight, state lock, admit, library open, index open,
/// drive pool), returning the refusal text where the entry would print it and exit.
async fn x22_try_start(fixture: &Harness) -> Result<Client, String> {
    let paths = state_paths(&fixture.config);
    remanence_state::authority_manifest::AuthorityManifest::preflight(&paths)
        .map_err(|e| format!("authority layout: {e}"))?;
    let lock = Arc::new(
        StateLockGuard::acquire(&paths.state_dir).map_err(|e| format!("state lock: {e}"))?,
    );
    remanence_state::authority_layout::admit_owned_config(&fixture.config)
        .map_err(|e| format!("authority layout (admit): {e}"))?;
    let runtime = LibraryRuntime::open(&paths, fixture.config.audit.fsync, lock)
        .map_err(|e| format!("library open: {e}"))?;
    let index = remanence_state::projector::open(&paths.sqlite_path)
        .map_err(|e| format!("open index: {e}"))?
        .with_library_runtime(runtime);
    let report = DiscoveryReport {
        libraries: vec![fixture.world.lock().unwrap().library_snapshot()],
        unattached_drives: vec![],
        complete: true,
        incomplete_evidence: vec![],
        warnings: vec![],
    };
    let world = Arc::clone(&fixture.world);
    let injection = Arc::clone(&fixture.injection);
    let spool = fixture.root.join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let state = ApiState::with_drive_pool_opener(
        index,
        &fixture.config,
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
    .map_err(|e| format!("start drive pool: {e}"))?;
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
        .map_err(|e| format!("connect: {e}"))?;
    Ok(Client {
        channel,
        state,
        task,
    })
}

/// Stop all owners and fail if the state lock has not actually been released.
async fn x22_stop(fixture: &Harness, client: Client) {
    tokio::time::timeout(Duration::from_secs(20), client.state.shutdown_drive_pool())
        .await
        .expect("drive pool shutdown deadline")
        .expect("drive pool shutdown");
    client.shutdown().await;
    fixture.wait_for_teardown().await;
}

/// Preserve RPC codes and the entire message, including on deadline expiry.
async fn x22_rpc<T>(
    call: impl std::future::Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> Result<T, tonic::Status> {
    tokio::time::timeout(Duration::from_secs(20), call)
        .await
        .map_err(|_| tonic::Status::deadline_exceeded("X22 RPC deadline"))?
        .map(tonic::Response::into_inner)
}

async fn x22_fences(client: &Client) -> Result<Vec<pb::TapeIoFence>, tonic::Status> {
    let mut daemon = pb::daemon_client::DaemonClient::new(client.channel.clone());
    Ok(x22_rpc(daemon.health(())).await?.active_tape_io_fences)
}

async fn x22_catalog(
    client: &Client,
    object: &pb::ObjectRecord,
) -> Result<pb::ObjectRecord, tonic::Status> {
    x22_rpc(
        pb::catalog_client::CatalogClient::new(client.channel.clone()).get_object(
            pb::GetObjectRequest {
                pool_id: String::new(),
                key: Some(pb::get_object_request::Key::ObjectId(
                    object.object_id.clone(),
                )),
            },
        ),
    )
    .await
}

/// Provision and bootstrap the next cartridge with the harness's parity scheme.
/// Only slot 0x401 is populated: every transition after setup uses the daemon.
fn x22_second_tape_in_slot(fixture: &Harness, parity: bool) {
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
    provision_in_pool(
        &fixture.config,
        X22_UUID2,
        X22_BARCODE2,
        parity.clone(),
        "archive",
    );
    let bootstrap = remanence_api::build_tape_bootstrap(
        X22_UUID2,
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
    fixture
        .world
        .lock()
        .unwrap()
        .put_tape_in_slot(0x401, X22_BARCODE2, tape);
}

fn x22_pool_request() -> pb::OpenWriteSessionRequest {
    pb::OpenWriteSessionRequest {
        target: Some(pb::open_write_session_request::Target::PoolTarget(
            pb::TapePoolTarget {
                pool_id: "archive".into(),
                library_uuid: Vec::new(),
                mount_if_needed: true,
            },
        )),
        ..Default::default()
    }
}

fn x22_which(tape_uuid: &[u8]) -> &'static str {
    if tape_uuid == UUID {
        "tape 1"
    } else if tape_uuid == X22_UUID2 {
        "tape 2"
    } else {
        "another tape"
    }
}

fn x22_tape_lens(fixture: &Harness) -> String {
    let world = fixture.world.lock().unwrap();
    format!(
        "tape 1 holds {} records, tape 2 holds {}",
        world.tapes[BARCODE].records.len(),
        world.tapes[X22_BARCODE2].records.len()
    )
}

/// The first non-busy open is returned verbatim: an admission refusal cannot be retried here.
async fn x22_pool_open(client: &Client, parity: bool) -> Result<pb::WriteSession, tonic::Status> {
    let deadline = Instant::now()
        + if parity {
            Duration::from_secs(10)
        } else {
            Duration::from_millis(1500)
        };
    loop {
        match x22_rpc(client.writer().open_write_session(x22_pool_request())).await {
            Err(error)
                if Instant::now() < deadline
                    && (error.code() == tonic::Code::Aborted
                        || (parity
                            && error.code() == tonic::Code::FailedPrecondition
                            && (error.message().contains("busy")
                                || error.message().contains("no free drive")))) =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => return result,
        }
    }
}

async fn x22_pool_write(
    client: &Client,
    name: &str,
    parity: bool,
) -> Result<(pb::ObjectRecord, pb::WriteSession), tonic::Status> {
    let session = x22_pool_open(client, parity).await?;
    let object = tokio::time::timeout(
        Duration::from_secs(20),
        client.try_append(&session, name, &vec![0x5a; 300_000]),
    )
    .await
    .map_err(|_| tonic::Status::deadline_exceeded("X22 append deadline"))??;
    let closed = x22_rpc(
        client
            .writer()
            .close_write_session(pb::CloseWriteSessionRequest {
                session_id: session.session_id,
                ..Default::default()
            }),
    )
    .await?;
    Ok((object, closed))
}

async fn x22_read_on(
    client: &Client,
    tape: [u8; 16],
    object: &pb::ObjectRecord,
    range: Option<std::ops::Range<usize>>,
) -> Result<Vec<u8>, tonic::Status> {
    let mut reader = client.reader();
    let request = pb::OpenReadSessionRequest {
        target: Some(pb::open_read_session_request::Target::TapeTarget(
            pb::TapeTarget {
                tape_uuid: tape.to_vec(),
                mount_if_needed: true,
                required_pool_id: "archive".into(),
                allow_unpooled: false,
            },
        )),
        ..Default::default()
    };
    let deadline = Instant::now() + Duration::from_millis(1500);
    let session = loop {
        match x22_rpc(reader.open_read_session(request.clone())).await {
            Ok(session) => break session,
            Err(error)
                if Instant::now() < deadline
                    && matches!(
                        error.code(),
                        tonic::Code::Aborted | tonic::Code::FailedPrecondition
                    ) =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let range = range.unwrap_or(0..0);
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = reader
            .read_object_range(pb::ReadObjectRangeRequest {
                session_id: session.session_id.clone(),
                object_id: object.object_id.clone(),
                start_byte: range.start as u64,
                end_byte: range.end as u64,
                ..Default::default()
            })
            .await?
            .into_inner();
        let mut actual = Vec::new();
        while let Some(chunk) = stream.message().await? {
            actual.extend(chunk.data);
        }
        Ok::<_, tonic::Status>(actual)
    })
    .await
    .unwrap_or_else(|_| Err(tonic::Status::deadline_exceeded("X22 read deadline")));
    // Close even after a stream refusal so a negative read cannot leave a drive held.
    let closed = x22_rpc(reader.close_read_session(pb::CloseReadSessionRequest {
        session_id: session.session_id,
        ..Default::default()
    }))
    .await;
    let actual = result?;
    closed?;
    Ok(actual)
}

async fn x22_pinned_write(
    client: &Client,
    tape: [u8; 16],
    name: &str,
    bytes: &[u8],
) -> Result<pb::ObjectRecord, tonic::Status> {
    let session = x22_rpc(
        client
            .writer()
            .open_write_session(pb::OpenWriteSessionRequest {
                target: Some(pb::open_write_session_request::Target::TapeTarget(
                    pb::TapeTarget {
                        tape_uuid: tape.to_vec(),
                        mount_if_needed: true,
                        required_pool_id: "archive".into(),
                        allow_unpooled: false,
                    },
                )),
                ..Default::default()
            }),
    )
    .await?;
    let object = tokio::time::timeout(
        Duration::from_secs(20),
        client.try_append(&session, name, bytes),
    )
    .await
    .map_err(|_| tonic::Status::deadline_exceeded("X22 pinned append deadline"))??;
    x22_rpc(
        client
            .writer()
            .close_write_session(pb::CloseWriteSessionRequest {
                session_id: session.session_id,
                ..Default::default()
            }),
    )
    .await?;
    Ok(object)
}

async fn x22_supersede(
    client: &Client,
    tape: [u8; 16],
) -> Result<(pb::SupersedeTailResponse, pb::SupersedeTailResponse), tonic::Status> {
    let mut catalog = pb::catalog_client::CatalogClient::new(client.channel.clone());
    let mut request = pb::SupersedeTailRequest {
        observed_tape_log_sequence: None,
        tape_uuid: tape.to_vec(),
        ack_tape_uuid: tape.to_vec(),
        confirm_supersede: None,
        expected_found: Vec::new(),
    };
    let preview = x22_rpc(catalog.supersede_tail(request.clone())).await?;
    request.confirm_supersede = Some(preview.required_confirmation.clone());
    request.observed_tape_log_sequence = Some(preview.tape_log_sequence);
    request.expected_found = preview.found.clone();
    let done = x22_rpc(catalog.supersede_tail(request)).await?;
    Ok((preview, done))
}

struct RestoredInsideSession {
    fixture: Harness,
    a: (pb::ObjectRecord, Vec<u8>),
    z: (pb::ObjectRecord, Vec<u8>),
    b: (pb::ObjectRecord, Vec<u8>),
    copied_records: Vec<Record>,
}

/// Copy with A checkpointed in an open session, then retain B only on the cartridge.
async fn x22_take_live_copy(parity: bool) -> (RestoredInsideSession, tempfile::TempDir) {
    let fixture = Harness::new(parity);
    x22_second_tape_in_slot(&fixture, parity);
    let snapshot = tempfile::tempdir().unwrap();
    let bytes_a: Vec<u8> = (0..175_000_u32).flat_map(u32::to_le_bytes).collect();
    let bytes_z = vec![0xc7; 700_000];
    let bytes_b = vec![0xb6; 700_000];
    let client = fixture.start().await;
    let z = x22_pinned_write(&client, X22_UUID2, "Z", &bytes_z)
        .await
        .unwrap();
    let session = client.open().await;
    let a = client.append(&session, "A", &bytes_a).await;
    client.checkpoint(&session).await;
    x22_copy_tree(&fixture.root, &snapshot.path().join("copy"));
    let copied_records = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let b = client.append(&session, "B", &bytes_b).await;
    client.close(&session).await;
    x22_stop(&fixture, client).await;
    (
        RestoredInsideSession {
            fixture,
            a: (a, bytes_a),
            z: (z, bytes_z),
            b: (b, bytes_b),
            copied_records,
        },
        snapshot,
    )
}

fn x22_save_tapes(fixture: &Harness, path: &std::path::Path) {
    let world = fixture.world.lock().unwrap();
    let mut saved = serde_json::Map::new();
    for barcode in [BARCODE, X22_BARCODE2] {
        let tape = &world.tapes[barcode];
        let records: Vec<(u8, Vec<u8>, u32)> = tape
            .records
            .iter()
            .map(|record| match record {
                Record::Block(bytes) => (0, bytes.clone(), 0),
                Record::ZeroBlock(size) => (1, vec![], *size),
                Record::Filemark => (2, vec![], 0),
            })
            .collect();
        saved.insert(
            barcode.to_owned(),
            serde_json::json!({"records": records, "written_bytes": tape.written_bytes}),
        );
    }
    saved.insert(
        "in_drive".to_owned(),
        serde_json::json!(world.loaded_barcode(BAY)),
    );
    std::fs::write(path, serde_json::to_vec(&saved).unwrap()).unwrap();
}

fn x22_load_tapes(fixture: &Harness, path: &std::path::Path) -> String {
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut world = fixture.world.lock().unwrap();
    for barcode in [BARCODE, X22_BARCODE2] {
        let records: Vec<(u8, Vec<u8>, u32)> =
            serde_json::from_value(saved[barcode]["records"].clone()).unwrap();
        let tape = world.tapes.get_mut(barcode).unwrap();
        tape.records = records
            .into_iter()
            .map(|(kind, bytes, size)| match kind {
                0 => Record::Block(bytes),
                1 => Record::ZeroBlock(size),
                2 => Record::Filemark,
                _ => panic!("invalid saved record"),
            })
            .collect();
        tape.written_bytes = saved[barcode]["written_bytes"].as_u64().unwrap();
    }
    format!("{}", saved["in_drive"])
}

/// Recreate the saved cartridges, then restore and rebuild in a fresh process.
async fn restored_inside_session(parity: bool) -> RestoredInsideSession {
    let spec: serde_json::Value = serde_json::from_str(
        &std::env::var("X22_RESTORE_CHILD_SPEC").expect("restore child specification"),
    )
    .unwrap();
    let dir = std::path::PathBuf::from(spec["dir"].as_str().unwrap());
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    let mut fixture = Harness::new(parity);
    x22_second_tape_in_slot(&fixture, parity);
    fixture.root = serde_json::from_value(meta["root"].clone()).unwrap();
    fixture.config = serde_json::from_value(meta["config"].clone()).unwrap();
    let in_drive = x22_load_tapes(&fixture, &dir.join("tapes.json"));
    assert_eq!(
        in_drive, "\"CHAR01L9\"",
        "saved drive must match the child's seated cartridge"
    );
    let saved_records: Vec<(u8, Vec<u8>, u32)> =
        serde_json::from_value(meta["copied_records"].clone()).unwrap();
    let copied_records = saved_records
        .into_iter()
        .map(|(kind, bytes, size)| match kind {
            0 => Record::Block(bytes),
            1 => Record::ZeroBlock(size),
            2 => Record::Filemark,
            _ => panic!("invalid copied record"),
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut attempts = 0;
    loop {
        attempts += 1;
        x22_restore(&fixture.root, &dir.join("copy"));
        let rebuilt = x22_rebuild(&fixture.root).expect("restore catalogue rebuild");
        if fixture.root.join("index.sqlite").exists() {
            println!("X22 child setup parity={parity}: rebuild Ok, index.sqlite present, attempts={attempts}, tapes={}, copies={}",
                rebuilt.tapes_rebuilt, rebuilt.object_instances_rebuilt);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restored catalogue still missing after retries (M0 difference)"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let object = |key: &str| pb::ObjectRecord {
        object_id: serde_json::from_value(meta[key].clone()).unwrap(),
        ..Default::default()
    };
    RestoredInsideSession {
        fixture,
        a: (
            object("a"),
            (0..175_000_u32).flat_map(u32::to_le_bytes).collect(),
        ),
        z: (object("z"), vec![0xc7; 700_000]),
        b: (object("b"), vec![0xb6; 700_000]),
        copied_records,
    }
}

/// The child owns a fresh projector registry. Require both its exit status and its final marker:
/// an incorrect --exact selector otherwise runs zero tests and still exits successfully.
async fn x22_run_restored_test<F, Fut>(name: &str, parity: bool, steps: F)
where
    F: FnOnce(RestoredInsideSession) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    const SPEC: &str = "X22_RESTORE_CHILD_SPEC";
    let closing = format!("X22 child completed {name} parity={parity}");
    if let Some(spec) = std::env::var_os(SPEC) {
        let spec: serde_json::Value = serde_json::from_str(spec.to_str().unwrap()).unwrap();
        // M0 measures both parity values, each in a separate child invocation.
        if spec["parity"].as_bool().unwrap() != parity {
            return;
        }
        let restored = restored_inside_session(parity).await;
        steps(restored).await;
        println!("{closing}");
        return;
    }
    let (saved, snapshot) = x22_take_live_copy(parity).await;
    x22_save_tapes(&saved.fixture, &snapshot.path().join("tapes.json"));
    let copied_records: Vec<(u8, Vec<u8>, u32)> = saved
        .copied_records
        .iter()
        .map(|record| match record {
            Record::Block(bytes) => (0, bytes.clone(), 0),
            Record::ZeroBlock(size) => (1, vec![], *size),
            Record::Filemark => (2, vec![], 0),
        })
        .collect();
    std::fs::write(
        snapshot.path().join("meta.json"),
        serde_json::to_vec(&serde_json::json!({
            "root": saved.fixture.root, "config": saved.fixture.config,
            "a": saved.a.0.object_id, "z": saved.z.0.object_id, "b": saved.b.0.object_id,
            "copied_records": copied_records,
        }))
        .unwrap(),
    )
    .unwrap();
    let spec = serde_json::json!({"dir": snapshot.path(), "parity": parity});
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &format!("x22_restore::{name}"), "--nocapture"])
        .env(SPEC, spec.to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    assert!(
        output.status.success(),
        "restore child {}: stdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert!(
        stdout.lines().any(|line| line == closing),
        "restore child missing closing line {closing:?}:\n{stdout}\n{stderr}"
    );
}

#[tokio::test]
async fn m0_child_process_restore_measurement() {
    for parity in [false, true] {
        x22_run_restored_test(
            "m0_child_process_restore_measurement",
            parity,
            move |restored| async move {
                if parity {
                    let client = x22_try_start(&restored.fixture)
                        .await
                        .expect("M0 parity start Ok");
                    println!(
                        "X22 M0 parity start Ok, fences (measurement): {:?}",
                        x22_fences(&client).await.unwrap()
                    );
                    x22_stop(&restored.fixture, client).await;
                    return;
                }
                let fixture = &restored.fixture;
                let client = x22_try_start(fixture).await.expect("M0 start Ok");
                println!("X22 M0: child-process start Ok");
                let fences = x22_fences(&client).await.unwrap();
                println!("X22 M0 initial fences: {fences:?}");
                assert!(fences.is_empty(), "M0 initial fences");
                assert_eq!(
                    x22_read_on(&client, UUID, &restored.a.0, None)
                        .await
                        .unwrap(),
                    restored.a.1
                );
                assert_eq!(
                    x22_read_on(&client, X22_UUID2, &restored.z.0, None)
                        .await
                        .unwrap(),
                    restored.z.1
                );
                println!("X22 M0: A and Z read with correct bytes");
                let (c, closed) = x22_pool_write(&client, "M0-C", false)
                    .await
                    .expect("M0 pool write");
                assert_eq!(closed.tape_uuid.as_deref(), Some(X22_UUID2.as_slice()));
                assert_eq!(
                    x22_read_on(&client, X22_UUID2, &c, None).await.unwrap(),
                    vec![0x5a; 300_000]
                );
                let fences = x22_fences(&client).await.unwrap();
                println!(
                    "X22 M0 after pool write: {fences:?}; close names {}",
                    x22_which(closed.tape_uuid.as_ref().unwrap())
                );
                assert_eq!(fences.len(), 1, "M0 fence count");
                assert_eq!(fences[0].tape_uuid, UUID);
                assert_eq!(fences[0].reason, "append_admission_refused");
                {
                    let world = fixture.world.lock().unwrap();
                    println!(
                        "X22 M0 placement: drive={:?}, home={:?}",
                        world.loaded_barcode(BAY),
                        world.slots.iter().find(|slot| slot.address == 0x400)
                    );
                    assert_eq!(world.loaded_barcode(BAY), Some(X22_BARCODE2));
                    assert_eq!(
                        world
                            .slots
                            .iter()
                            .find(|slot| slot.address == 0x400)
                            .and_then(|s| s.barcode.as_deref()),
                        Some(BARCODE)
                    );
                }
                x22_stop(fixture, client).await;
                println!("X22 M0 stopped; {}", x22_tape_lens(fixture));
            },
        )
        .await;
    }
}

/// Test 2: discard only the written-since suffix, then append over it and read the result.
#[tokio::test]
async fn inside_session_discard_restores_writing() {
    x22_run_restored_test(
        "inside_session_discard_restores_writing",
        false,
        |restored| async move {
            let fixture = &restored.fixture;
            let client = x22_try_start(fixture).await.expect("restored daemon start");
            assert_eq!(
                x22_read_on(&client, UUID, &restored.a.0, None)
                    .await
                    .unwrap(),
                restored.a.1
            );
            assert_eq!(
                x22_read_on(&client, X22_UUID2, &restored.z.0, None)
                    .await
                    .unwrap(),
                restored.z.1
            );
            let (c, closed) = x22_pool_write(&client, "C", false)
                .await
                .expect("first pool write succeeds");
            assert_eq!(closed.tape_uuid.as_deref(), Some(X22_UUID2.as_slice()));
            let fences = x22_fences(&client).await.unwrap();
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].tape_uuid, UUID);
            assert_eq!(
                fences[0].reason, "append_admission_refused",
                "inside-session fence reason"
            );
            assert_eq!(
                x22_read_on(&client, UUID, &restored.a.0, None)
                    .await
                    .unwrap(),
                restored.a.1
            );
            assert_eq!(
                x22_read_on(&client, X22_UUID2, &restored.z.0, None)
                    .await
                    .unwrap(),
                restored.z.1
            );
            assert_eq!(
                x22_read_on(&client, X22_UUID2, &c, None).await.unwrap(),
                vec![0x5a; 300_000]
            );
            let (preview, done) = x22_supersede(&client, UUID)
                .await
                .expect("discard preview and confirmation");
            assert_eq!(preview.found.len(), 1);
            assert_eq!(preview.required_confirmation, "SUPERSEDE CHAR01L9");
            assert!(!preview.completed);
            assert!(done.completed);
            assert!(x22_fences(&client).await.unwrap().is_empty());
            assert_eq!(
                x22_read_on(&client, UUID, &restored.a.0, None)
                    .await
                    .unwrap(),
                restored.a.1
            );
            assert_eq!(
                x22_read_on(&client, X22_UUID2, &restored.z.0, None)
                    .await
                    .unwrap(),
                restored.z.1
            );
            match x22_catalog(&client, &restored.b.0).await {
                Ok(found) => {
                    assert!(found.copies.is_empty(), "B must have no catalogue copy");
                    println!("X22 discard: B catalogue returns no copy");
                }
                Err(error) => {
                    assert_eq!(error.code(), tonic::Code::NotFound, "B catalogue: {error}");
                    println!(
                        "X22 discard: B catalogue returns NotFound: {}",
                        error.message()
                    );
                }
            }
            let error = x22_read_on(&client, UUID, &restored.b.0, None)
                .await
                .expect_err("discarded B cannot read");
            assert_eq!(
                error.code(),
                tonic::Code::NotFound,
                "discarded B read: {error}"
            );
            println!(
                "X22 discard: B read {:?}: {} ({} bytes discarded)",
                error.code(),
                error.message(),
                restored.b.1.len()
            );
            let bytes_d = vec![0xd4; 700_000];
            let d = x22_pinned_write(&client, UUID, "D", &bytes_d)
                .await
                .expect("tape 1 writable after discard");
            {
                let world = fixture.world.lock().unwrap();
                let records = &world.tapes[BARCODE].records;
                assert_eq!(
                    &records[..restored.copied_records.len()],
                    restored.copied_records.as_slice(),
                    "discard and D preserve every record present at the copy"
                );
                assert!(
                    !records[restored.copied_records.len()..]
                        .iter()
                        .any(|record| match record {
                            Record::Block(bytes) => bytes
                                .windows(1024)
                                .any(|run| run.iter().all(|b| *b == 0xb6)),
                            _ => false,
                        }),
                    "B payload must be physically gone past the known prefix"
                );
            }
            assert_eq!(x22_read_on(&client, UUID, &d, None).await.unwrap(), bytes_d);
            x22_stop(fixture, client).await;
        },
    )
    .await;
}

/// Test 4: one non-busy open must evict tape 1 and select tape 2 inside that RPC.
#[tokio::test]
async fn pool_write_reselects_with_one_drive() {
    x22_run_restored_test(
        "pool_write_reselects_with_one_drive",
        false,
        |restored| async move {
            check_pool_placement(restored, false).await;
        },
    )
    .await;
}

async fn check_pool_placement(restored: RestoredInsideSession, parity: bool) {
    let fixture = &restored.fixture;
    let before = {
        let world = fixture.world.lock().unwrap();
        assert_eq!(world.drive_bays.len(), 1);
        assert_eq!(
            world.loaded_barcode(BAY),
            Some(BARCODE),
            "tape 1 seated at loss"
        );
        (
            world.tapes[BARCODE].records.clone(),
            world.tapes[X22_BARCODE2].records.len(),
        )
    };
    let client = x22_try_start(fixture).await.expect("restored daemon start");
    let session = x22_pool_open(&client, parity)
        .await
        .expect("first non-busy pool open succeeds");
    let bytes_c = vec![0x5a; 300_000];
    let c = client
        .try_append(&session, "C", &bytes_c)
        .await
        .expect("C append");
    let closed = x22_rpc(
        client
            .writer()
            .close_write_session(pb::CloseWriteSessionRequest {
                session_id: session.session_id,
                ..Default::default()
            }),
    )
    .await
    .expect("C close");
    assert_eq!(closed.tape_uuid.as_deref(), Some(X22_UUID2.as_slice()));
    {
        let world = fixture.world.lock().unwrap();
        assert_eq!(world.loaded_barcode(BAY), Some(X22_BARCODE2));
        let home = world
            .slots
            .iter()
            .find(|slot| slot.address == 0x400)
            .unwrap();
        assert!(home.full);
        assert_eq!(home.barcode.as_deref(), Some(BARCODE));
        assert_eq!(
            world.tapes[BARCODE].records, before.0,
            "evicted tape 1 records unchanged"
        );
        assert!(
            world.tapes[X22_BARCODE2].records.len() > before.1,
            "tape 2 records grew"
        );
    }
    assert_eq!(
        x22_read_on(&client, X22_UUID2, &c, None).await.unwrap(),
        bytes_c
    );
    x22_stop(fixture, client).await;
    if parity {
        // OperationRecorded has no tape field. Only a Failed summary naming tape 1's
        // UUID supports attribution; successful Z/C chains remain unattributed.
        let closeouts = recorded_closeouts(fixture);
        let tape1 = uuid::Uuid::from_bytes(UUID).to_string();
        let ids: std::collections::BTreeSet<_> = closeouts.iter().map(|r| r.operation).collect();
        println!(
            "X22 parity: {} TapeCloseout operation(s), {} events (measurement only)",
            ids.len(),
            closeouts.len()
        );
        for id in ids {
            let chain: Vec<_> = closeouts.iter().filter(|r| r.operation == id).collect();
            let tape1_failure = chain.iter().any(|r| {
                matches!(&r.event,
                remanence_state::library_log::OperationEvent::Failed(failed)
                    if failed.summary.as_str().contains(&tape1))
            });
            println!(
                "X22 parity TapeCloseout {}: {}",
                uuid::Uuid::from_bytes(id),
                if tape1_failure {
                    "tape 1, by Failed summary UUID"
                } else {
                    "cannot attribute: no tape-1 UUID in a Failed summary (includes tape-2 Z/C closeouts)"
                }
            );
            for record in chain {
                println!(
                    "X22 parity TapeCloseout {} event {:?}",
                    uuid::Uuid::from_bytes(id),
                    record.event
                );
                if let remanence_state::library_log::OperationEvent::Failed(failed) = &record.event
                {
                    println!("X22 parity Failed summary: {}", failed.summary.as_str());
                }
            }
        }
        println!("X22 parity: the worker's own fence check returns before closeout_worker_attempt and records no TapeCloseout there");
    }
}

/// Cross the first 262144-byte block boundary under the append-only fence.
#[tokio::test]
async fn fenced_range_read_crosses_block_boundary() {
    x22_run_restored_test(
        "fenced_range_read_crosses_block_boundary",
        false,
        |restored| async move {
            let fixture = &restored.fixture;
            let client = x22_try_start(fixture).await.expect("restored daemon start");
            x22_pool_write(&client, "C", false)
                .await
                .expect("pool write before range read");
            let fences = x22_fences(&client).await.unwrap();
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].tape_uuid, UUID);
            assert_eq!(fences[0].reason, "append_admission_refused");
            let actual = x22_read_on(&client, UUID, &restored.a.0, Some(100_000..300_000))
                .await
                .expect("fenced range read open and stream succeed");
            assert_eq!(
                actual,
                restored.a.1[100_000..300_000],
                "position-dependent range bytes"
            );
            x22_stop(fixture, client).await;
        },
    )
    .await;
}

/// A durable fence survives restart while reads and the ensuing idle unload still pass.
#[tokio::test]
async fn fenced_restart_read_idle_unloads() {
    x22_run_restored_test("fenced_restart_read_idle_unloads", false, |mut restored| async move {
        let fixture = &mut restored.fixture;
        let client = x22_try_start(fixture).await.expect("restored daemon start");
        x22_pool_write(&client, "C", false).await.expect("pool write before restart");
        let fences = x22_fences(&client).await.unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].tape_uuid, UUID);
        assert_eq!(fences[0].reason, "append_admission_refused");
        let quarantine_id = fences[0].quarantine_id.clone();
        assert_eq!(x22_read_on(&client, UUID, &restored.a.0, None).await.unwrap(), restored.a.1);
        assert_eq!(fixture.world.lock().unwrap().loaded_barcode(BAY), Some(BARCODE));
        x22_stop(fixture, client).await;
        fixture.config.daemon.drive_idle_unload_seconds = 1;
        let client = x22_try_start(fixture).await.expect("restart with active fence");
        let fences = x22_fences(&client).await.unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].tape_uuid, UUID);
        assert_eq!(fences[0].reason, "append_admission_refused");
        assert_eq!(fences[0].quarantine_id, quarantine_id);
        assert_eq!(x22_read_on(&client, UUID, &restored.a.0, None).await.unwrap(), restored.a.1);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let unloaded = {
                let world = fixture.world.lock().unwrap();
                world.loaded_barcode(BAY) != Some(BARCODE) && world.slots.iter().any(|slot|
                    slot.address == 0x400 && slot.full && slot.barcode.as_deref() == Some(BARCODE))
            };
            if unloaded { break; }
            assert!(Instant::now() < deadline, "fenced tape 1 idle unload deadline: drive must release CHAR01L9 to slot 0x400");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        println!("X22 restart: same quarantine {quarantine_id}; A read without release; tape 1 idle unloaded to slot 0x400");
        x22_stop(fixture, client).await;
    }).await;
}

/// Four production layers enforce this refusal; this integration pin checks its public payload.
#[tokio::test]
async fn fenced_pinned_write_names_quarantine() {
    x22_run_restored_test(
        "fenced_pinned_write_names_quarantine",
        false,
        |restored| async move {
            let fixture = &restored.fixture;
            let client = x22_try_start(fixture).await.expect("restored daemon start");
            x22_pool_write(&client, "C", false)
                .await
                .expect("pool write before pinned refusal");
            let fences = x22_fences(&client).await.unwrap();
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].tape_uuid, UUID);
            assert_eq!(fences[0].reason, "append_admission_refused");
            let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
            let bytes = vec![0xd4; 300_000];
            let error = x22_pinned_write(&client, UUID, "D-refused", &bytes)
                .await
                .expect_err("write pinned to fenced tape 1 must refuse");
            assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}");
            assert!(
                error
                    .message()
                    .contains(&format!("quarantine {}", fences[0].quarantine_id)),
                "whole refusal must name Health's quarantine_id: {error}"
            );
            assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
            println!(
                "X22 pinned write (pins the refusal only): {:?}: {}",
                error.code(),
                error.message()
            );
            x22_stop(fixture, client).await;
        },
    )
    .await;
}

/// P11 can let the parity closeout worker take the drive before the pool open.
#[tokio::test]
async fn parity_pool_write_reselects_with_one_drive() {
    x22_run_restored_test(
        "parity_pool_write_reselects_with_one_drive",
        true,
        |restored| async move {
            check_pool_placement(restored, true).await;
        },
    )
    .await;
}
