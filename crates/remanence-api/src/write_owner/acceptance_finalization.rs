//! Finalization crash acceptance through public RPCs and the daemon startup path.
//! This child of the durability tests reuses their restart images and committed
//! session fixtures, which need crate-private session-engine access.
//!
//! Full matrix (including the acceptance failures):
//! `CARGO_TARGET_DIR=$PWD/target-0-fincuts cargo test -p remanence-api --release
//! --lib acceptance_finalization -- --ignored --nocapture`
//! Maximum loss discards buffered tape records, unsynced bytes, and unsynced
//! names. Torn-append cases keep 1, half, penultimate, or all dirty bytes.
//! The default samples the buffered WFM boundaries of A and C.
//! Identical finished record images share a successful physical verification;
//! each restart independently checks placement and reads every committed Object.

use super::*;
use crate::pb::catalog_server::Catalog;
use crate::pb::library_service_server::LibraryService;
use crate::pb::read_session_service_server::ReadSessionService;
use crate::ApiState;
use remanence_state::authority::AuthorityFinalization;
use remanence_state::authority::TapeAuthorityView;
use std::time::Duration;
use tokio_stream::StreamExt;
use tonic::Request;

const KEY: [u8; 16] = [0xf1; 16];

// The tape is append-only during the reference trace. Store its records once;
// thousands of separation-record cuts share the same immutable host image.
#[derive(Clone)]
struct FinalCut {
    number: usize,
    boundary: String,
    before_sync: Option<PathBuf>,
    world: VirtualWorld,
    end: usize,
    disk: Arc<BTreeMap<PathBuf, Vec<u8>>>,
    files: Arc<FileSnapshot>,
}

#[derive(Default)]
struct FinalTrace {
    cuts: Vec<FinalCut>,
    records: Vec<Record>,
    disk: Arc<BTreeMap<PathBuf, Vec<u8>>>,
    files: Arc<FileSnapshot>,
    active: bool,
    lose_filemark: Option<(u64, bool)>,
    lost_cut: Option<usize>,
    allowed_files: Vec<(u64, u64)>,
}

fn capture_final(
    trace: &Arc<Mutex<FinalTrace>>,
    world: &Arc<Mutex<VirtualWorld>>,
    files: &MemoryDirectory,
    root: &Path,
    boundary: String,
    before_sync: Option<PathBuf>,
) -> Option<usize> {
    let mut trace = trace.lock().unwrap();
    if !trace.active {
        return None;
    }
    let mut guard = world.lock().unwrap();
    let records = std::mem::take(&mut guard.tapes.get_mut(BARCODE).unwrap().records);
    let commands = std::mem::take(&mut guard.command_log);
    let snapshot = guard.clone();
    guard.command_log = commands;
    assert!(
        records.len() >= trace.records.len(),
        "reference trace only appends"
    );
    let previous_end = trace.records.len();
    trace.records.extend_from_slice(&records[previous_end..]);
    let end = records.len();
    guard.tapes.get_mut(BARCODE).unwrap().records = records;
    drop(guard);
    let disk = disk_image(root);
    if *trace.disk != disk {
        trace.disk = Arc::new(disk);
    }
    if !boundary.starts_with("command") {
        trace.files = Arc::new(files.snapshot());
    }
    let cut = FinalCut {
        number: trace.cuts.len() + 1,
        boundary: boundary.replace(root.to_str().unwrap(), "<state>"),
        before_sync,
        world: snapshot,
        end,
        disk: trace.disk.clone(),
        files: trace.files.clone(),
    };
    trace.cuts.push(cut);
    Some(trace.cuts.len() - 1)
}

struct FinalTransport {
    inner: ModelTransport,
    world: Arc<Mutex<VirtualWorld>>,
    files: MemoryDirectory,
    trace: Arc<Mutex<FinalTrace>>,
    root: PathBuf,
    position: Option<u64>,
}
impl FinalTransport {
    fn guard_write(&self, records: u64, filemark: bool) {
        let trace = self.trace.lock().unwrap();
        if trace.allowed_files.is_empty() {
            if trace.active {
                assert_eq!(
                    self.position,
                    Some(self.world.lock().unwrap().tapes[BARCODE].records.len() as u64),
                    "reference writes only append at a proved position"
                );
            }
            return;
        }
        let position = self
            .position
            .expect("every recovery write has command-proved position");
        assert!(
            trace.allowed_files.iter().any(|(start, end)| {
                if filemark {
                    position == *end && records == 1
                } else {
                    position >= *start && position + records <= *end
                }
            }),
            "write at {position}, count={records}, filemark={filemark} follows the immutable plan"
        );
    }
    fn cut(&self, cdb: &[u8]) -> Option<usize> {
        capture_final(
            &self.trace,
            &self.world,
            &self.files,
            &self.root,
            format!("command {:02x?}", cdb),
            None,
        )
    }
}
impl SgTransport for FinalTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let result = self.inner.execute_in(cdb, buf);
        if cdb[0] == 0x34 {
            self.position = (result.is_ok() && buf[0] & 0x0c == 0)
                .then(|| u64::from_be_bytes(buf[8..16].try_into().unwrap()));
        } else if cdb[0] == 0x08 {
            self.position = None;
        }
        let _ = self.cut(cdb);
        result
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        let marks = if cdb[0] == 0x10 {
            u64::from(u32::from_be_bytes([0, cdb[2], cdb[3], cdb[4]]))
        } else {
            0
        };
        if marks != 0 {
            self.guard_write(marks, true);
        }
        let result = self.inner.execute_none(cdb);
        if result.is_ok() {
            match cdb[0] {
                0x92 => self.position = Some(u64::from_be_bytes(cdb[4..12].try_into().unwrap())),
                0x01 => self.position = Some(0),
                0x10 => self.position = self.position.map(|p| p + marks),
                0x11 | 0x91 | 0x1b => self.position = None,
                _ => {}
            }
        } else {
            self.position = None;
        }
        let cut = self.cut(cdb);
        if result.is_ok() && cdb.first() == Some(&0x10) {
            let mut trace = self.trace.lock().unwrap();
            let file_count = self.world.lock().unwrap().tapes[BARCODE]
                .records
                .iter()
                .filter(|r| matches!(r, Record::Filemark))
                .count() as u64;
            if trace.active && trace.lose_filemark == Some((file_count, cdb[1] & 1 == 0)) {
                trace.lose_filemark = None;
                trace.lost_cut = Some(cut.expect("active trace captures the lost response"));
                // Match the model's lost-response fault: effects reach tape,
                // but the caller receives a transport timeout, not success.
                return Err(remanence_library::ScsiError::TransportError {
                    status: 0,
                    host_status: 3,
                    driver_status: 0,
                    info: 1,
                    sense: vec![],
                });
            }
        }
        result
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if cdb[0] == 0x0a {
            self.guard_write((buf.len() / BLOCK as usize) as u64, false);
        }
        let result = self.inner.execute_out(cdb, buf);
        if cdb[0] == 0x0a {
            self.position = if result.is_ok() {
                self.position
                    .map(|p| p + (buf.len() / BLOCK as usize) as u64)
            } else {
                None
            };
        }
        let _ = self.cut(cdb);
        result
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

fn rpc_config(root: &Path) -> remanence_state::RemConfig {
    remanence_state::parse_config_toml(&format!(
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
sqlite_path = "{0}/state.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
[[libraries]]
serial = "{LIBRARY}"
[[tape_pools]]
id = "{POOL}"
block_size = {BLOCK}
"#,
        root.display()
    ))
    .unwrap()
}

fn start(
    root: &Path,
    world: Arc<Mutex<VirtualWorld>>,
    trace: Arc<Mutex<FinalTrace>>,
    files: MemoryDirectory,
) -> Result<ApiState, Status> {
    let cfg = rpc_config(root);
    // Crash images are relocated into an isolated temporary root. Rebind the
    // fixture marker to that root, exactly as its config paths are rebased.
    std::fs::create_dir_all(&cfg.journal.dir).unwrap();
    std::fs::write(
        root.join("authority-format"),
        format!(
            "{}\n{}\n",
            remanence_state::authority_layout::FORMAT,
            serde_json::to_string(&cfg.journal.dir.canonicalize().unwrap()).unwrap()
        ),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("spool")).unwrap();
    let report = remanence_library::DiscoveryReport {
        libraries: vec![world.lock().unwrap().library_snapshot()],
        unattached_drives: vec![],
        complete: true,
        incomplete_evidence: vec![],
        warnings: vec![],
    };
    ApiState::with_drive_pool_opener(
        CatalogIndex::open(root.join("state.sqlite")).unwrap(),
        &cfg,
        report,
        remanence_library::StaticAllowlist::new([LIBRARY]),
        root.join("spool"),
        64 * 1024 * 1024,
        |library, policy| {
            library.open_with(policy, {
                let world = world.clone();
                let trace = trace.clone();
                let files = files.clone();
                let root = root.to_owned();
                move |path| {
                    let role = world.lock().unwrap().role_for_path(path).unwrap();
                    Ok(Box::new(FinalTransport {
                        inner: ModelTransport::new(world.clone(), role),
                        world: world.clone(),
                        files: files.clone(),
                        trace: trace.clone(),
                        root: root.clone(),
                        position: None,
                    }) as Box<dyn SgTransport>)
                }
            })
        },
    )
}

async fn request(api: &ApiState) -> Result<pb::TapeFinalization, Status> {
    api.catalog_service()
        .finalize_tape(Request::new(pb::FinalizeTapeRequest {
            tape_uuid: TAPE.to_vec(),
            expected_pool_id: Some(POOL.into()),
            reason: "acceptance finalization".into(),
            idempotency_key: Some(pb::IdempotencyKey {
                value: KEY.to_vec(),
            }),
        }))
        .await
        .map(tonic::Response::into_inner)
}

async fn status(api: &ApiState) -> Result<pb::TapeFinalization, Status> {
    api.catalog_service()
        .get_tape_finalization(Request::new(pb::GetTapeFinalizationRequest {
            tape_uuid: TAPE.to_vec(),
        }))
        .await
        .map(tonic::Response::into_inner)
}

async fn settled(api: &ApiState) -> Result<pb::TapeFinalization, String> {
    let started = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let value = status(api).await.map_err(|e| e.to_string())?;
            if let Some(id) = value
                .operation_id
                .as_ref()
                .and_then(|id| Uuid::from_slice(id).ok())
            {
                if let Some(operation) = api.operations.latest(&id) {
                    if !crate::operations::is_terminal(&operation) {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        continue;
                    }
                    return Ok(value);
                } else if started.elapsed() >= Duration::from_millis(100) {
                    // A failed startup admission can leave the durable intent
                    // without a live worker. The same-key RPC supplies the
                    // operator retry and its concrete refusal.
                    return Ok(value);
                }
            }
            if value.outcome != pb::TapeFinalizationOutcome::Finalizing as i32 {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .map_err(|_| "finalization remains in progress".to_string())?
}

async fn stop(api: &ApiState) {
    api.begin_shutdown();
    if let Err(error) = api.shutdown_drive_pool().await {
        eprintln!("cleanup: {error}");
    }
}

struct Session {
    root: tempfile::TempDir,
    cuts: Vec<FinalCut>,
    records: Vec<Record>,
    receipts: Vec<Receipt>,
    verified: Mutex<Vec<Vec<Record>>>,
    intent: AuthorityFinalization,
    prefix: Vec<Record>,
    lost_cut: Option<usize>,
}

async fn trace_finalization(lose_filemark: Option<(u64, bool)>) -> Session {
    // The write engine supplies three acknowledged Objects and a short epoch.
    let seed = tempfile::tempdir().unwrap();
    let baseline = trace_session(seed.path(), false).await.pop().unwrap();
    let root = tempfile::tempdir().unwrap();
    restore_disk(root.path(), &baseline.disk);
    baseline
        .files
        .materialize(seed.path(), root.path())
        .unwrap();
    let files = MemoryDirectory::default();
    for (path, bytes) in baseline.files.files() {
        files.seed(
            root.path().join(path.strip_prefix(seed.path()).unwrap()),
            bytes.written.clone(),
        );
    }
    let mut world = baseline.world;
    world.tapes.get_mut(BARCODE).unwrap().records = baseline.records.as_ref().clone();
    world.tapes.get_mut(BARCODE).unwrap().capacity_bytes = 8 * 1024 * 1024 * 1024;
    let prefix = world.tapes[BARCODE].records.clone();
    let world = Arc::new(Mutex::new(world));
    let trace = Arc::new(Mutex::new(FinalTrace {
        lose_filemark,
        ..FinalTrace::default()
    }));
    let registration = register(root.path().to_owned(), Arc::new(files.clone()));
    let api = start(root.path(), world.clone(), trace.clone(), files.clone()).unwrap();
    files.observe(Some(Arc::new({
        let world = world.clone();
        let trace = trace.clone();
        let files = files.clone();
        let root = root.path().to_owned();
        move |event: FileEvent| {
            let before_sync = match &event {
                FileEvent::BeforeSync(path) => Some(path.clone()),
                _ => None,
            };
            let _ = capture_final(
                &trace,
                &world,
                &files,
                &root,
                format!("{event:?}"),
                before_sync,
            );
        }
    })));
    trace.lock().unwrap().active = true;
    let _ = capture_final(
        &trace,
        &world,
        &files,
        root.path(),
        "before FinalizeTape".into(),
        None,
    );
    request(&api).await.unwrap();
    let result = settled(&api).await.unwrap();
    if lose_filemark.is_none() {
        assert_eq!(
            result.outcome,
            pb::TapeFinalizationOutcome::Finalized as i32,
            "{result:?}"
        );
    } else {
        assert!(
            trace.lock().unwrap().lost_cut.is_some(),
            "fault reaches the requested component"
        );
    }
    let _ = capture_final(
        &trace,
        &world,
        &files,
        root.path(),
        "client finalized".into(),
        None,
    );
    trace.lock().unwrap().active = false;
    files.observe(None);
    stop(&api).await;
    drop(api);
    drop(registration);
    let cuts = std::mem::take(&mut trace.lock().unwrap().cuts);
    // Read the immutable decision from a snapshot before completion retires it.
    let intent = cuts
        .iter()
        .rev()
        .find_map(|cut| {
            let temp = tempfile::tempdir().unwrap();
            restore_disk(temp.path(), &cut.disk);
            cut.files.materialize(root.path(), temp.path()).unwrap();
            remanence_state::authority::fixtures::AuthorityBuilder::new(
                temp.path().join("journals/tapes"),
                TAPE,
            )
            .build()
            .ok()?
            .pending_finalization()
            .ok()
            .flatten()
        })
        .expect("trace contains the terminal plan");
    let prefix_plan = intent.terminal_prefix.as_ref().unwrap();
    assert!(prefix_plan.parity_map_tape_file_number.is_some());
    assert_eq!(
        prefix_plan.tail_start_tape_file_number - prefix_plan.start_tape_file_number,
        2,
        "a short open epoch emits its closing sidecar and final ParityMap"
    );
    eprintln!(
        "finalization trace: {} cuts; prefix files {}..{}; layout {:?}",
        cuts.len(),
        intent
            .terminal_prefix
            .as_ref()
            .unwrap()
            .start_tape_file_number,
        intent
            .terminal_prefix
            .as_ref()
            .unwrap()
            .tail_start_tape_file_number,
        intent.layout
    );
    let records = std::mem::take(&mut trace.lock().unwrap().records);
    let lost_cut = trace.lock().unwrap().lost_cut;
    Session {
        root,
        cuts,
        intent,
        prefix,
        records,
        receipts: baseline.receipts,
        verified: Mutex::default(),
        lost_cut,
    }
}

/// Read and close through the daemon so all audit appends share its lock.
async fn read_objects(api: &ApiState, receipts: &[Receipt]) -> Result<(), String> {
    let service = api.read_session_service();
    let session = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = service
                .open_read_session(Request::new(pb::OpenReadSessionRequest {
                    target: Some(pb::open_read_session_request::Target::TapeTarget(
                        pb::TapeTarget {
                            tape_uuid: TAPE.to_vec(),
                            mount_if_needed: true,
                            required_pool_id: POOL.into(),
                            allow_unpooled: false,
                        },
                    )),
                    ..Default::default()
                }))
                .await;
            match response {
                Ok(response) => break Ok(response.into_inner()),
                Err(error)
                    if error.code() == tonic::Code::FailedPrecondition
                        && error.message() == "tape is already mounted" =>
                {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(error) => break Err(format!("read open: {error}")),
            }
        }
    })
    .await
    .map_err(|_| "read open remains busy".to_string())??;
    let read = async {
        for receipt in receipts {
            let mut stream = service
                .read_object_range(Request::new(pb::ReadObjectRangeRequest {
                    session_id: session.session_id.clone(),
                    object_id: Uuid::parse_str(&receipt.id).unwrap().as_bytes().to_vec(),
                    stream_chunk_bytes: BLOCK,
                    ..Default::default()
                }))
                .await
                .map_err(|e| format!("read request: {e}"))?
                .into_inner();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                bytes.extend(chunk.map_err(|e| format!("read stream: {e}"))?.data);
            }
            if bytes != receipt.payload {
                return Err(format!(
                    "committed Object {} differs byte for byte",
                    receipt.id
                ));
            }
        }
        Ok(())
    }
    .await;
    let close = service
        .close_read_session(Request::new(pb::CloseReadSessionRequest {
            session_id: session.session_id,
            ..Default::default()
        }))
        .await
        .map(|_| ())
        .map_err(|e| format!("read close: {e}"));
    read.and(close)
}

async fn check(
    session: &Session,
    cut: &FinalCut,
    keep: usize,
    tails: BTreeMap<PathBuf, usize>,
    keep_entries: bool,
) -> Result<&'static str, String> {
    let temp = tempfile::tempdir().unwrap();
    restore_disk(temp.path(), &cut.disk);
    let mut files = cut.files.as_ref().clone();
    files.crash(&tails, keep_entries).unwrap();
    files.materialize(session.root.path(), temp.path()).unwrap();
    let mut world = cut.world.clone();
    world.tapes.get_mut(BARCODE).unwrap().records = session.records[..cut.end].to_vec();
    world.power_cut(BAY, keep).unwrap();
    assert_eq!(
        &world.tapes[BARCODE].records[..session.prefix.len()],
        session.prefix.as_slice(),
        "committed prefix survives"
    );
    let world = Arc::new(Mutex::new(world));
    let prefix = session.intent.terminal_prefix.as_ref().unwrap();
    let mut allowed_files: Vec<_> = prefix
        .publication
        .files
        .iter()
        .map(|entry| {
            let start = entry.physical_start_hint.unwrap();
            (start, start + entry.block_count)
        })
        .collect();
    allowed_files.extend(
        session
            .intent
            .layout
            .components
            .iter()
            .map(|c| (c.start_lba, c.start_lba + c.record_count)),
    );
    let recovery_trace = Arc::new(Mutex::new(FinalTrace {
        allowed_files,
        ..FinalTrace::default()
    }));
    let api = start(
        temp.path(),
        world.clone(),
        recovery_trace,
        MemoryDirectory::default(),
    )
    .map_err(|e| format!("startup: {e}"))?;
    let outcome = async {
        let mut route = "startup";
        // Startup projects and schedules accepted intents. Before acceptance,
        // the operator repeats the same public request and idempotency key.
        let mut result = match status(&api).await {
            Err(error) if error.code() == tonic::Code::NotFound => {
                route = "operator retry";
                request(&api).await.map_err(|e| format!("operator retry: {e}"))?;
                settled(&api).await?
            }
            _ => settled(&api).await?,
        };
        if result.outcome != pb::TapeFinalizationOutcome::Finalized as i32 {
            route = "operator retry";
            let fences = api.index().map_err(|e| e.to_string())?.list_active_tape_io_fences().map_err(|e| e.to_string())?;
            for fence in fences {
                assert_eq!(fence.tape_uuid, TAPE);
                api.library_service().release_tape_io_fence(Request::new(pb::ReleaseTapeIoFenceRequest {
                    quarantine_id: fence.quarantine_id,
                    acknowledgment: "checked committed prefix and fixed finalization plan".into(),
                })).await.map_err(|e| format!("operator release: {e}"))?;
                route = "operator release and retry";
            }
            request(&api).await.map_err(|e| format!("operator retry: {e}"))?;
            result = settled(&api).await?;
        }
        if result.outcome != pb::TapeFinalizationOutcome::Finalized as i32 {
            return Err(format!("unfinished: {result:?}"));
        }
        assert_eq!(result.completed_replicas, Some(3), "finalization reports three replicas");
        {
            let guard = world.lock().unwrap();
            let records = &guard.tapes[BARCODE].records;
            let layout = &session.intent.layout;
            assert_eq!(records.len() as u64, layout.expected_eod_lba, "one terminal suffix");
            assert_eq!(&records[..session.prefix.len()], session.prefix.as_slice(), "no Object changes after decision");
            let prefix_plan = session.intent.terminal_prefix.as_ref().unwrap();
            let mut file_starts = vec![0];
            file_starts.extend(records.iter().enumerate().filter_map(|(i, r)| {
                (matches!(r, Record::Filemark) && i + 1 < records.len()).then_some(i + 1)
            }));
            assert_eq!(file_starts.len() as u64, layout.components[4].tape_file_number + 1);
            let map_magic = remanence_parity::derive_parity_map_magic(&TAPE);
            let maps: Vec<_> = file_starts.iter().enumerate().filter_map(|(file, start)| {
                matches!(&records[*start], Record::Block(bytes) if bytes.starts_with(&map_magic)).then_some(file as u64)
            }).collect();
            assert_eq!(maps, vec![prefix_plan.parity_map_tape_file_number.unwrap()], "exactly one final ParityMap");
            assert_eq!(file_starts[prefix_plan.start_tape_file_number as usize] as u64, prefix_plan.start_lba);
            assert_eq!(prefix_plan.start_lba as usize, session.prefix.len());
            for entry in &prefix_plan.publication.files {
                let magic = match entry.kind {
                    remanence_parity::TapeFileKind::ParitySidecar => remanence_parity::derive_sidecar_magic(&TAPE),
                    remanence_parity::TapeFileKind::ParityMap => map_magic,
                    _ => panic!("only finalization control files follow the decision"),
                };
                let start = file_starts[entry.tape_file_number as usize];
                assert_eq!(Some(start as u64), entry.physical_start_hint);
                assert!(matches!(&records[start], Record::Block(bytes) if bytes.starts_with(&magic)));
                assert_eq!(records[start + entry.block_count as usize], Record::Filemark);
            }
            for component in &layout.components {
                let start = component.start_lba as usize;
                let end = start + component.record_count as usize;
                assert!(records[start..end].iter().all(|r| matches!(r, Record::Block(_) | Record::ZeroBlock(_))));
                assert_eq!(records[end], Record::Filemark);
                let Record::Block(header) = &records[start] else { panic!("component has a typed header") };
                let parsed = match component.kind {
                    remanence_state::TerminalFinalizationComponentKind::TapeIndexReplica => {
                        let header = remanence_parity::parse_tape_index_replica_header(header, &TAPE).unwrap();
                        assert_eq!(header.plan.replica_ordinal, component.ordinal);
                        header.plan.component
                    }
                    remanence_state::TerminalFinalizationComponentKind::IndexSeparationExtent => {
                        let header = remanence_parity::parse_index_separation_header(header, &TAPE).unwrap();
                        assert_eq!(header.plan.descriptor.gap_ordinal, component.ordinal);
                        header.plan.component
                    }
                };
                assert_eq!(parsed.planned_start_lba, component.start_lba);
                assert_eq!(parsed.record_count, component.record_count);
                assert_eq!(parsed.planned_tape_file_number, component.tape_file_number);
                assert_eq!(records[..start].iter().filter(|r| matches!(r, Record::Filemark)).count() as u64, component.tape_file_number);
            }
        }
        let image = world.lock().unwrap().tapes[BARCODE].records.clone();
        let cached = session.verified.lock().unwrap().contains(&image);
        if !cached {
        let verified = api.catalog_service().verify_tape_index(Request::new(pb::VerifyTapeIndexRequest { tape_uuid: TAPE.to_vec() })).await.map_err(|e| format!("verification: {e}"))?.into_inner();
        if verified.state != pb::TapeIndexVerificationState::VerifiedComplete as i32 {
            return Err(format!("verification: {verified:?}"));
        }
        session.verified.lock().unwrap().push(image);
        }
        Ok(route)
    }.await;
    let readable = read_objects(&api, &session.receipts).await;
    stop(&api).await;
    match (outcome, readable) {
        (Ok(route), Ok(())) => Ok(route),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(read)) => Err(format!("{error}; {read}")),
    }
}

/// Collapse cuts only when power loss leaves byte-identical host and tape
/// images. Drive counters have no fault triggers in this fixture; reset removes
/// position knowledge, and the source slot and loaded cartridge stay fixed.
fn maximum_loss_groups(session: &Session) -> Vec<Vec<usize>> {
    use sha2::{Digest, Sha256};
    let mut groups: BTreeMap<[u8; 32], Vec<usize>> = BTreeMap::new();
    let mut snapshots = BTreeMap::new();
    for (i, cut) in session.cuts.iter().enumerate() {
        let key = (
            Arc::as_ptr(&cut.disk) as usize,
            Arc::as_ptr(&cut.files) as usize,
        );
        let host_digest = *snapshots.entry(key).or_insert_with(|| {
            let temp = tempfile::tempdir().unwrap();
            restore_disk(temp.path(), &cut.disk);
            let mut files = cut.files.as_ref().clone();
            files.crash(&BTreeMap::new(), false).unwrap();
            files.materialize(session.root.path(), temp.path()).unwrap();
            let mut hash = Sha256::new();
            for (path, bytes) in disk_image(temp.path()) {
                let name = path.to_str().unwrap().as_bytes();
                hash.update(name.len().to_le_bytes());
                hash.update(name);
                hash.update(bytes.len().to_le_bytes());
                hash.update(bytes);
            }
            <[u8; 32]>::from(hash.finalize())
        });
        let state = &cut.world.drive_states[&BAY];
        assert!(state.dirty_after_ops.is_none() && state.cleaning_cycle_ops_remaining.is_none());
        assert_eq!(cut.world.drive_bays[&BAY].as_deref(), Some(BARCODE));
        let mut hash = Sha256::new();
        hash.update(host_digest);
        let keep = cut.world.tapes[BARCODE].durable_frontier;
        let mut reset = cut.world.clone();
        let tape = reset.tapes.get_mut(BARCODE).unwrap();
        tape.written_bytes = session.records[..keep]
            .iter()
            .map(|record| match record {
                Record::Block(bytes) => bytes.len() as u64,
                Record::ZeroBlock(bytes) => u64::from(*bytes),
                Record::Filemark => 0,
            })
            .sum();
        let state = reset.drive_states.get_mut(&BAY).unwrap();
        state.op_count = 0;
        state.reset_unit_attention = true;
        state.position_unknown = true;
        // Tape bytes are the common reference prefix of length `keep`.
        hash.update(keep.to_le_bytes());
        hash.update(format!("{reset:?}").as_bytes());
        groups.entry(hash.finalize().into()).or_default().push(i);
    }
    let mut groups: Vec<_> = groups.into_values().collect();
    groups.sort_by_key(|group| group[0]);
    assert_eq!(
        groups.iter().map(Vec::len).sum::<usize>(),
        session.cuts.len()
    );
    groups
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "acceptance: fails on main; expected from step 6"]
async fn finalization_maximum_loss_matrix() {
    let session = trace_finalization(None).await;
    let groups = maximum_loss_groups(&session);
    eprintln!("{} distinct maximum-loss restart images", groups.len());
    let mut failures = Vec::new();
    for group in groups {
        let cut = &session.cuts[group[0]];
        let result = check(
            &session,
            cut,
            cut.world.tapes[BARCODE].durable_frontier,
            BTreeMap::new(),
            false,
        )
        .await;
        eprintln!(
            "C{:03}..C{:03} ({} cuts) {} => {result:?}",
            cut.number,
            session.cuts[*group.last().unwrap()].number,
            group.len(),
            cut.boundary
        );
        if let Err(error) = result {
            failures.push((cut.number, cut.boundary.clone(), error));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A lost reply leaves the command effects intact and precedes host commit.
async fn lost_responses(barrier: bool) {
    let mut failures = Vec::new();
    // The closing sidecar and final ParityMap share one blocking barrier.
    let first = if barrier { 8 } else { 7 };
    for filemark in first..=13 {
        let session = trace_finalization(Some((filemark, barrier))).await;
        let cut = &session.cuts[session.lost_cut.unwrap()];
        let outcome = check(
            &session,
            cut,
            cut.world.tapes[BARCODE].durable_frontier,
            BTreeMap::new(),
            false,
        )
        .await;
        let label = format!(
            "lost response file {} barrier={barrier} C{} {}",
            filemark - 1,
            cut.number,
            cut.boundary
        );
        eprintln!("{label} => {outcome:?}");
        if let Err(error) = outcome {
            failures.push(format!("{label}: {error}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "full finalization matrix; run with --release --ignored --nocapture"]
async fn finalization_lost_buffered_responses_complete() {
    lost_responses(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "acceptance: fails on main; expected from step 6"]
async fn finalization_lost_barrier_responses_complete() {
    lost_responses(true).await;
}

/// Exercise already durable terminal progress in the ordinary workspace run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalization_cuts_complete() {
    let session = trace_finalization(None).await;
    // Start with C so subsequent identical finished images reuse its physical
    // verification proof. Every restart still checks all objects and positions.
    for component in [4, 0] {
        let plan = session.intent.layout.components[component];
        let end = (plan.start_lba + plan.record_count + 1) as usize;
        let cut = session
            .cuts
            .iter()
            .find(|c| c.boundary.starts_with("command [10, 01,") && c.end == end)
            .unwrap();
        let result = check(
            &session,
            cut,
            cut.world.tapes[BARCODE].durable_frontier,
            BTreeMap::new(),
            false,
        )
        .await;
        eprintln!("default C{} {} => {result:?}", cut.number, cut.boundary);
        assert!(
            result.is_ok(),
            "C{} {}: {result:?}",
            cut.number,
            cut.boundary
        );
    }
}

/// Byte tears retain a prefix of an unsynced append independently of tape.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "acceptance: fails on main; expected from step 6"]
async fn finalization_torn_journals_complete() {
    let session = trace_finalization(None).await;
    let mut failures = Vec::new();
    for cut in &session.cuts {
        let Some(path) = &cut.before_sync else {
            continue;
        };
        let Some((_, bytes)) = cut.files.files().find(|(name, _)| *name == path) else {
            continue;
        };
        let dirty = bytes.written.len().saturating_sub(bytes.durable.len());
        if dirty == 0 || !bytes.written.starts_with(&bytes.durable) {
            continue;
        }
        for tail in BTreeSet::from([1, dirty / 2, dirty.saturating_sub(1), dirty]) {
            let result = check(
                &session,
                cut,
                cut.world.tapes[BARCODE].durable_frontier,
                BTreeMap::from([(path.to_owned(), tail)]),
                true,
            )
            .await;
            eprintln!(
                "C{} {} keep_tail={tail}/{dirty} => {result:?}",
                cut.number, cut.boundary
            );
            if let Err(error) = result {
                failures.push(format!(
                    "C{} {} tail={tail}: {error}",
                    cut.number, cut.boundary
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} cuts do not finalize:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
