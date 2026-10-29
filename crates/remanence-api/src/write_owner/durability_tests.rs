//! Crash exploration through the same in-process write-session engine as the daemon.
//! Each command, file append, sync, and client receipt captures a restartable cut.

#[path = "acceptance_finalization.rs"]
mod acceptance_finalization;

use super::*;
use remanence_chaos::file_model::{FileEvent, FileSnapshot, MemoryDirectory};
use remanence_library::durable_file::test_support::register;
use std::collections::{BTreeMap, BTreeSet};

const BAY: u16 = 0x100;
const BARCODE: &str = "DUR001L9";
const TAPE: [u8; 16] = [0xd0; 16];
const BLOCK: u32 = 256 * 1024;
const POOL: &str = "durability";
const LIBRARY: &str = "LIB-DURABILITY";
const DRIVE: &str = "DRV-DURABILITY";

#[derive(Clone)]
struct Receipt {
    id: String,
    payload: Vec<u8>,
    start: usize,
    records: Vec<Record>,
}

#[derive(Clone)]
struct Cut {
    number: usize,
    boundary: String,
    before_sync: Option<PathBuf>,
    world: VirtualWorld,
    records: Arc<Vec<Record>>,
    disk: BTreeMap<PathBuf, Vec<u8>>,
    files: FileSnapshot,
    receipts: Vec<Receipt>,
}

#[derive(Default)]
struct Trace {
    cuts: Vec<Cut>,
    receipts: Vec<Receipt>,
    records: Arc<Vec<Record>>,
    active: bool,
}

fn disk_image(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !dir.exists() {
            return;
        }
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else if path.extension().is_none_or(|ext| ext != "lock")
                && !path.to_string_lossy().ends_with("-shm")
            {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

fn capture(
    trace: &Arc<Mutex<Trace>>,
    world: &Arc<Mutex<VirtualWorld>>,
    files: &MemoryDirectory,
    root: &Path,
    boundary: String,
    before_sync: Option<PathBuf>,
) {
    let mut trace = trace.lock().unwrap();
    if !trace.active {
        return;
    }
    let mut world = world.lock().unwrap().clone();
    let tape = world.tapes.get_mut(BARCODE).unwrap();
    if *trace.records != tape.records {
        trace.records = Arc::new(std::mem::take(&mut tape.records));
    } else {
        tape.records.clear();
    }
    let cut = Cut {
        number: trace.cuts.len() + 1,
        boundary: boundary.replace(root.to_str().unwrap(), "<state>"),
        before_sync,
        world,
        records: trace.records.clone(),
        disk: disk_image(root),
        files: files.snapshot(),
        receipts: trace.receipts.clone(),
    };
    trace.cuts.push(cut);
}

/// The negative control changes only WFM's IMMED bit, retaining its filemarks
/// and successful response while withholding the synchronization guarantee.
struct CrashTransport {
    inner: ModelTransport,
    world: Arc<Mutex<VirtualWorld>>,
    files: MemoryDirectory,
    trace: Arc<Mutex<Trace>>,
    root: PathBuf,
    skip_barrier: bool,
}
impl CrashTransport {
    fn cut(&self, cdb: &[u8]) {
        capture(
            &self.trace,
            &self.world,
            &self.files,
            &self.root,
            format!("command {:02x?}", cdb),
            None,
        );
    }
}
impl SgTransport for CrashTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let result = self.inner.execute_in(cdb, buf);
        self.cut(cdb);
        result
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        let mut dispatched = cdb.to_vec();
        if self.skip_barrier && cdb.first() == Some(&0x10) {
            dispatched[1] |= 1;
        }
        let result = self.inner.execute_none(&dispatched);
        self.cut(cdb);
        result
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let result = self.inner.execute_out(cdb, buf);
        self.cut(cdb);
        result
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

fn scheme() -> ParityScheme {
    ParityScheme {
        id: SchemeId::new_static("durability-test"),
        data_blocks_per_stripe: 4,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    }
}
fn pool() -> TapePoolConfig {
    TapePoolConfig {
        scheme: None,
        id: POOL.into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        selection_policy: PoolSelectionPolicyName::CompleteOrFill,
        watermark_low: 0.9,
        watermark_high: 0.95,
        capacity_cap_bytes: None,
        block_size_bytes: BLOCK.into(),
        min_object_size_bytes: 0,
    }
}
fn selected() -> SelectedTape {
    SelectedTape {
        standalone_endpoint: None,
        pool_id: POOL.into(),
        tape_uuid: TAPE,
        block_size: BLOCK,
        parity_config: ParityConfig::Scheme(scheme()),
    }
}

fn setup(root: &Path) -> Arc<Mutex<VirtualWorld>> {
    let mut index = CatalogIndex::open(root.join("state.sqlite")).unwrap();
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: POOL.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .unwrap();
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: TAPE,
            voltag: BARCODE.into(),
            block_size: BLOCK,
            parity: ParityConfig::Scheme(scheme()),
            force: false,
        })
        .unwrap();
    index.project_tape_pool_membership(TAPE, POOL).unwrap();
    index
        .set_tape_generation(
            TAPE,
            MediaObservation {
                generation: LtoGen::Lto9,
                source: GenerationSource::Operator,
            },
        )
        .unwrap();
    let bootstrap = BootstrapPayload {
        scheme: Some(ParitySchemeRecord {
            id: scheme().id.as_str().into(),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
            no_parity_flag: false,
        }),
        no_parity_flag: false,
        filemark_map_digest: Some(sole_bot_filemark_map_digest().unwrap()),
        tape_uuid: TAPE,
        written_by_version: "test".into(),
        written_at: "2026-10-29T00:00:00Z".into(),
        sequence: 0,
        block_size_bytes: BLOCK,
        drive_compression: false,
    };
    let mut block = vec![0; BLOCK as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(1024 * 1024 * 1024, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = BLOCK.into();
    tape.durable_frontier = 2;
    let mut world = VirtualWorld::single_drive(LIBRARY, BAY, DRIVE, 0x400, 1);
    world.put_tape_in_drive(BAY, BARCODE, Some(0x400), tape);
    Arc::new(Mutex::new(world))
}

fn config(root: &Path, library: &remanence_library::LibraryHandle) -> WriteOwnerConfig {
    std::fs::create_dir_all(root.join("audit")).unwrap();
    let mut cfg = test_write_owner_config(
        root.join("state.sqlite"),
        root.join("audit"),
        library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_journal_dir = root.join("journals/checkpoints");
    cfg.calibration_store = CalibrationControlStore::open(root.join("calibration")).unwrap();
    cfg.checkpoint_max_objects = 100;
    cfg.checkpoint_max_age_seconds = 3600;
    cfg.audit_fsync = true;
    cfg
}

/// Run one session handler, joining its thread after close or refusal.
async fn open_writer(
    root: &Path,
    mut library: remanence_library::LibraryHandle,
) -> (
    mpsc::Sender<DriveCommand>,
    Result<Uuid, Status>,
    std::thread::JoinHandle<()>,
) {
    let cfg = config(root, &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let actor_tx = tx.clone();
    let (reply, received) = oneshot::channel();
    let request = super::super::actor_runtime::OpenWriteActorRequest {
        actor: AuditActor::System,
        pool_cfg: pool(),
        selected: selected(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: LIBRARY.into(),
        barcode: Some(BARCODE.into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        reply,
    };
    let thread = std::thread::spawn(move || {
        let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
        handle_drive_open_write(
            BAY, &mut index, &cfg, actor_tx, &mut rx, &mut drive, &mut 0, request, &mut None,
        );
    });
    let opened = received
        .await
        .expect("writer replies without panic")
        .map(|session| Uuid::from_slice(&session.session_id).unwrap());
    (tx, opened, thread)
}

async fn close_writer(
    tx: &mpsc::Sender<DriveCommand>,
    id: Uuid,
    thread: std::thread::JoinHandle<()>,
) {
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().expect("write-session engine does not panic");
}

async fn trace_session(root: &Path, skip_barrier: bool) -> Vec<Cut> {
    let world = setup(root);
    let files = MemoryDirectory::default();
    let trace = Arc::new(Mutex::new(Trace {
        active: true,
        ..Trace::default()
    }));
    let _registration = register(root.to_owned(), Arc::new(files.clone()));
    files.observe(Some(Arc::new({
        let trace = trace.clone();
        let world = world.clone();
        let files = files.clone();
        let root = root.to_owned();
        move |event: FileEvent| {
            let before_sync = if let FileEvent::BeforeSync(path) = &event {
                Some(path.clone())
            } else {
                None
            };
            capture(
                &trace,
                &world,
                &files,
                &root,
                format!("{event:?}"),
                before_sync,
            )
        }
    })));
    let model = world.lock().unwrap().library_snapshot();
    let library = model
        .open_with(&remanence_library::StaticAllowlist::new([LIBRARY]), {
            let world = world.clone();
            let files = files.clone();
            let trace = trace.clone();
            let root = root.to_owned();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(CrashTransport {
                    inner: ModelTransport::new(world.clone(), role),
                    world: world.clone(),
                    files: files.clone(),
                    trace: trace.clone(),
                    root: root.clone(),
                    skip_barrier,
                }))
            }
        })
        .unwrap();
    let (tx, opened, thread) = open_writer(root, library).await;
    let id = opened.unwrap();
    for number in 0..3 {
        let payload = format!("durable object {number}: exact bytes\0\u{ff}").into_bytes();
        let outcome = append_actor_test_file(
            &tx,
            id,
            root.join(format!("input-{number}")),
            &format!("file-{number}"),
            &format!("caller-{number}"),
            &payload,
        )
        .await;
        assert_eq!(
            outcome
                .record
                .append_commit_info
                .as_ref()
                .unwrap()
                .durability,
            pb::AppendDurability::Written as i32
        );
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Checkpoint {
            session_id: id,
            trigger: CheckpointTrigger::Explicit,
            expected_batch_id: None,
            reply: Some(reply),
        })
        .await
        .unwrap();
        let checkpoint = received.await.unwrap().unwrap();
        assert_eq!(checkpoint.committed_objects.len(), 1);
        let object = &checkpoint.committed_objects[0];
        assert_eq!(
            object.append_commit_info.as_ref().unwrap().durability,
            pb::AppendDurability::Checkpointed as i32
        );
        let index = CatalogIndex::open(root.join("state.sqlite")).unwrap();
        let object_id = Uuid::from_slice(&object.object_id).unwrap().to_string();
        let copy = &index.find_native_object_copies(&object_id).unwrap()[0];
        let start = copy.global_start_block.unwrap() as usize;
        let end = copy.global_end_block.unwrap() as usize + 1;
        let records = world.lock().unwrap().tapes[BARCODE].records[start..end].to_vec();
        trace.lock().unwrap().receipts.push(Receipt {
            id: object_id,
            payload,
            start,
            records,
        });
        capture(
            &trace,
            &world,
            &files,
            root,
            format!("client commit {number}"),
            None,
        );
    }
    close_writer(&tx, id, thread).await;
    trace.lock().unwrap().active = false;
    files.observe(None);
    let cuts = std::mem::take(&mut trace.lock().unwrap().cuts);
    cuts
}

/// Read exact member bytes through the normal read-session engine after restart.
async fn read_receipts(
    root: &Path,
    world: Arc<Mutex<VirtualWorld>>,
    receipts: &[Receipt],
) -> Result<(), String> {
    if receipts.is_empty() {
        return Ok(());
    }
    let mut library = open_model_library(world);
    let cfg = config(root, &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let (reply, received) = oneshot::channel();
    let request = super::super::actor_runtime::OpenReadActorRequest {
        tape_uuid: TAPE,
        needs_drive_load: false,
        library_serial: LIBRARY.into(),
        barcode: Some(BARCODE.into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        resume_target: None,
        daemon_epoch: 2,
        reply,
    };
    let thread = std::thread::spawn(move || {
        let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
        handle_drive_open_read(BAY, &mut index, &cfg, &mut rx, &mut drive, &mut 0, request);
    });
    let opened = received.await.expect("read engine replies without panic");
    let session = match opened {
        Ok(session) => Uuid::from_slice(&session.session_id).unwrap(),
        Err(status) => {
            thread.join().unwrap();
            return Err(format!("read open refuses acknowledged object: {status}"));
        }
    };
    let index = CatalogIndex::open(root.join("state.sqlite")).unwrap();
    let mut outcome = Ok(());
    for receipt in receipts {
        let member = &index.list_native_object_files(&receipt.id).unwrap()[0];
        let (chunk_tx, chunk_rx) = crate::read_core::read_stream_channel(BLOCK as usize);
        tx.send(DriveCommand::ReadObjectRange {
            session_id: session,
            object_id: receipt.id.clone(),
            file_id: member.file_id.clone(),
            start_byte: 0,
            end_byte: member.size_bytes,
            stream_chunk_bytes: BLOCK,
            chunk_tx,
        })
        .await
        .unwrap();
        match collect_stream_chunks(chunk_rx).await {
            Ok(bytes) if bytes == receipt.payload => {}
            other => {
                outcome = Err(format!(
                    "acknowledged object {} fails byte-for-byte read: {other:?}",
                    receipt.id
                ));
                break;
            }
        }
    }
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::CloseRead {
        session_id: session,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().expect("read-session engine does not panic");
    outcome
}

/// Use the operator's preview/confirmation route to discard only an uncommitted tail.
fn acknowledge_tail(root: &Path, world: Arc<Mutex<VirtualWorld>>) -> Result<(), Status> {
    use super::super::terminal_types::TerminalConfig;
    let mut library = open_model_library(world.clone());
    let cfg = config(root, &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let mut index = CatalogIndex::open(root.join("state.sqlite")).unwrap();
    prepare_drive_for_write(
        &mut drive,
        &TAPE,
        BLOCK,
        Uuid::new_v4(),
        WriteMediaPolicy::RewritableObject,
    )
    .unwrap();
    let generation = drive.identity_generation();
    let before = world.lock().unwrap().tapes[BARCODE].records.clone();
    let mut preview = None;
    let refusal = crate::mount_backend::supersede_mounted_tail(
        &mut index,
        &cfg.terminal_config(),
        &mut drive,
        TAPE,
        generation,
        BARCODE,
        AuditActor::System,
        |required, found| {
            preview = Some((required.to_owned(), found.to_vec()));
            Err("confirmation required".into())
        },
    )
    .unwrap_err();
    assert_eq!(refusal.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        world.lock().unwrap().tapes[BARCODE].records,
        before,
        "preview does not change tape"
    );
    let Some((confirmation, found)) = preview else {
        return Err(refusal);
    };
    crate::mount_backend::supersede_mounted_tail(
        &mut index,
        &cfg.terminal_config(),
        &mut drive,
        TAPE,
        generation,
        BARCODE,
        AuditActor::System,
        |required, current| {
            assert_eq!(required, confirmation);
            assert_eq!(current, found);
            Ok(())
        },
    )
    .map(|_| ())
}

fn record_refusal(
    coverage: &mut Coverage,
    status: &Status,
    cut: &Cut,
    keep: usize,
) -> Result<bool, String> {
    let message = status.message();
    let category = if message.contains("TailBeyondKnownState") {
        "uncommitted tape tail"
    } else if message.contains("valid bundle(s) beyond its last checkpoint marker") {
        "orphan parity bundle"
    } else if message.contains("open parity tape journal")
        && message.contains("failed to fill whole buffer")
    {
        "short parity header"
    } else if message.contains("journal recovery required: torn record") {
        "torn parity frame"
    } else if message.contains("tape identity: bootstrap I/O")
        || message.contains("bootstrap is not followed by a filemark")
    {
        "missing bootstrap"
    } else if message.contains("read audit segment header") {
        "short audit header"
    } else {
        return Err(format!("unexpected recovery refusal: {status}"));
    };
    assert_eq!(
        status.code(),
        if category == "short audit header" {
            tonic::Code::Internal
        } else if category == "torn parity frame" && !message.contains("open parity tape journal") {
            tonic::Code::Unavailable
        } else {
            tonic::Code::FailedPrecondition
        }
    );
    *coverage.refused.entry(category.into()).or_default() += 1;
    coverage
        .refusal_examples
        .entry(category.into())
        .or_insert_with(|| {
            format!(
                "{}; keep={keep}; receipts={}",
                cut.boundary,
                cut.receipts.len()
            )
        });
    if !message.contains(&Uuid::from_bytes(TAPE).to_string()) && !message.contains(BARCODE) {
        *coverage
            .unnamed_refusals
            .entry(category.into())
            .or_default() += 1;
    }
    Ok(category == "uncommitted tape tail")
}

fn restore_disk(root: &Path, disk: &BTreeMap<PathBuf, Vec<u8>>) {
    for (relative, bytes) in disk {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
}

#[derive(Default, Debug)]
struct Coverage {
    cuts: usize,
    failed_syncs: usize,
    torn_tails: usize,
    crashes: usize,
    completed: usize,
    acknowledged_tails: usize,
    released_fences: usize,
    unnamed_refusals: BTreeMap<String, usize>,
    refused: BTreeMap<String, usize>,
    refusal_examples: BTreeMap<String, String>,
}

async fn check_crash(
    source: &Path,
    cut: &Cut,
    keep: usize,
    tails: BTreeMap<PathBuf, usize>,
    keep_entries: bool,
    coverage: &mut Coverage,
) -> Result<(), String> {
    coverage.crashes += 1;
    // Include the crash choice, not only its boundary, in citable diagnostics.
    let cut = &Cut {
        boundary: format!(
            "C{:03} {}; keep_entries={keep_entries}; tails={:?}",
            cut.number,
            cut.boundary,
            tails
                .iter()
                .map(|(path, bytes)| (path.strip_prefix(source).unwrap(), bytes))
                .collect::<Vec<_>>(),
        ),
        ..cut.clone()
    };
    let temp = tempfile::tempdir().unwrap();
    restore_disk(temp.path(), &cut.disk);
    let mut files = cut.files.clone();
    files.crash(&tails, keep_entries).unwrap();
    files.materialize(source, temp.path()).unwrap();
    let mut world = cut.world.clone();
    world.tapes.get_mut(BARCODE).unwrap().records = cut.records.as_ref().clone();
    world.power_cut(BAY, keep).unwrap();
    // Even an operator refusal cannot excuse losing an acknowledged record.
    for receipt in &cut.receipts {
        if world.tapes[BARCODE]
            .records
            .get(receipt.start..receipt.start + receipt.records.len())
            != Some(receipt.records.as_slice())
        {
            return Err(format!(
                "{}: acknowledged object {} loses tape bytes at keep={keep}",
                cut.boundary, receipt.id
            ));
        }
    }
    let world = Arc::new(Mutex::new(world));
    let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
    let replay = crate::startup_checkpoint::replay_checkpoint_journal_projections(
        &mut index,
        &temp.path().join("journals/checkpoints"),
    );
    if let Err(status) = replay {
        assert_eq!(status.code(), tonic::Code::FailedPrecondition, "{status}");
        assert!(
            status
                .message()
                .contains(&Uuid::from_bytes(TAPE).to_string()),
            "refusal names the tape: {status}"
        );
        assert!(
            status.message().contains("torn trailing frame"),
            "unexpected startup refusal: {status}"
        );
        for receipt in &cut.receipts {
            assert!(
                index.get_native_object(&receipt.id).unwrap().is_some(),
                "startup refusal preserves acknowledged object"
            );
        }
        *coverage
            .refused
            .entry("startup: torn checkpoint frame".into())
            .or_default() += 1;
        coverage
            .refusal_examples
            .entry("startup: torn checkpoint frame".into())
            .or_insert_with(|| {
                format!(
                    "{}; keep={keep}; receipts={}",
                    cut.boundary,
                    cut.receipts.len()
                )
            });
        return read_receipts(temp.path(), world, &cut.receipts).await;
    }
    let authority = FileCheckpointJournal::open(temp.path().join("journals/checkpoints"), TAPE)
        .unwrap()
        .replay()
        .unwrap();
    let committed = authority
        .last()
        .map_or(0, |record| record.committed_object_count);
    let listed = index.list_native_objects().unwrap();
    assert_eq!(
        listed.len() as u64,
        committed,
        "catalog agrees with durable prefix at {}",
        cut.boundary
    );
    for receipt in &cut.receipts {
        assert!(
            listed.iter().any(|object| object.object_id == receipt.id),
            "acknowledged object is listed at {}",
            cut.boundary
        );
        let members = index.list_native_object_files(&receipt.id).unwrap();
        assert_eq!(members[0].size_bytes, receipt.payload.len() as u64);
    }
    drop(index);
    for attempt in 0..2 {
        let (tx, opened, thread) =
            open_writer(temp.path(), open_model_library(world.clone())).await;
        match opened {
            Ok(id) => {
                coverage.completed += 1;
                let expected_start = authority.last().map_or(2, |record| record.eod_lba);
                let before = world.lock().unwrap().tapes[BARCODE]
                    .records
                    .get(..expected_start as usize)
                    .map(<[Record]>::to_vec);
                let Some(before) = before else {
                    close_writer(&tx, id, thread).await;
                    return Err(format!(
                        "{}: recovery admits an append beyond surviving tape",
                        cut.boundary
                    ));
                };
                append_actor_test_file(
                    &tx,
                    id,
                    temp.path().join("next-input"),
                    "next-file",
                    "next-caller",
                    b"next append",
                )
                .await;
                let (reply, received) = oneshot::channel();
                tx.send(DriveCommand::Checkpoint {
                    session_id: id,
                    trigger: CheckpointTrigger::Explicit,
                    expected_batch_id: None,
                    reply: Some(reply),
                })
                .await
                .unwrap();
                let response = received.await.unwrap().unwrap();
                let next_id = Uuid::from_slice(&response.committed_objects[0].object_id)
                    .unwrap()
                    .to_string();
                let index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
                let next = index.find_native_object_copies(&next_id).unwrap();
                assert_eq!(
                    next[0].global_start_block,
                    Some(expected_start),
                    "next append immediately follows the committed prefix at {}",
                    cut.boundary
                );
                assert_eq!(
                    &world.lock().unwrap().tapes[BARCODE].records[..before.len()],
                    before.as_slice()
                );
                close_writer(&tx, id, thread).await;
                break;
            }
            Err(status) => {
                thread.join().expect("recovery never panics");
                let needs_ack = record_refusal(coverage, &status, cut, keep)?;
                if needs_ack && attempt == 0 {
                    match acknowledge_tail(temp.path(), world.clone()) {
                        Ok(()) => {
                            coverage.acknowledged_tails += 1;
                        }
                        Err(status) => {
                            record_refusal(coverage, &status, cut, keep)?;
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }
    let mut index = CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
    for fence in index.list_active_tape_io_fences().unwrap() {
        assert_eq!(fence.tape_uuid, TAPE);
        assert_eq!(fence.reason, "supersede_tail_failed");
        let status =
            session_open_reject_tape_io_fences(&index, &TAPE, Some(BARCODE), "open read session")
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status
            .message()
            .contains(&Uuid::from_bytes(TAPE).to_string()));
        index
            .release_tape_io_fence(
                &fence.quarantine_id,
                "verified acknowledged records after failed tail discard",
            )
            .unwrap()
            .unwrap();
        coverage.released_fences += 1;
    }
    drop(index);
    read_receipts(temp.path(), world, &cut.receipts).await?;
    Ok(())
}

/// One independent restart. The default always includes the minimum-survival
/// choice at every cut; the full suite adds the original boundary matrix.
struct CrashCase {
    cut: usize,
    keep: usize,
    tails: BTreeMap<PathBuf, usize>,
    keep_entries: bool,
    fail_sync: bool,
}

fn crash_cases(cuts: &[Cut], full: bool) -> Vec<CrashCase> {
    // Sample the first dirty header and last dirty frame sync of each file.
    // This includes both short-header and torn-record recovery paths.
    let mut samples = BTreeMap::new();
    for (index, cut) in cuts.iter().enumerate() {
        if let Some(path) = &cut.before_sync {
            let (_, bytes) = cut.files.files().find(|(name, _)| *name == path).unwrap();
            if bytes.written.len() > bytes.durable.len() {
                let key = (path.clone(), bytes.durable.is_empty());
                if bytes.durable.is_empty() {
                    samples.entry(key).or_insert(index);
                } else {
                    // Late frames retain acknowledged objects, so recovery must
                    // handle a tear without silently dropping their catalog rows.
                    samples.insert(key, index);
                }
            }
        }
    }
    let samples: BTreeSet<_> = samples.into_values().collect();
    let mut cases = Vec::new();
    for (index, cut) in cuts.iter().enumerate().rev() {
        let frontier = cut.world.tapes[BARCODE].durable_frontier;
        let end = cut.records.len();
        let sampled = full || samples.contains(&index);
        if cut.before_sync.is_some() && sampled {
            cases.push(CrashCase {
                cut: index,
                keep: frontier,
                tails: BTreeMap::new(),
                keep_entries: false,
                fail_sync: true,
            });
        }
        // Losing everything not proved durable is the strongest missing-barrier
        // challenge. This case remains mandatory at every default cut.
        let keeps: Vec<_> = if !full {
            vec![frontier]
        } else if end - frontier <= 8 {
            (frontier..=end).collect()
        } else {
            vec![frontier, frontier + (end - frontier) / 2, end]
        };
        for keep in keeps {
            cases.push(CrashCase {
                cut: index,
                keep,
                tails: BTreeMap::new(),
                keep_entries: false,
                fail_sync: false,
            });
        }
        if full {
            cases.push(CrashCase {
                cut: index,
                keep: frontier,
                tails: BTreeMap::new(),
                keep_entries: true,
                fail_sync: false,
            });
        }
        if sampled {
            for (path, bytes) in cut.files.files() {
                let dirty = bytes.written.len().saturating_sub(bytes.durable.len());
                if dirty == 0 || (!full && cut.before_sync.as_deref() != Some(path)) {
                    continue;
                }
                let choices = if full {
                    BTreeSet::from([1, dirty / 2, dirty.saturating_sub(1), dirty])
                } else {
                    BTreeSet::from([1, dirty.saturating_sub(1)])
                };
                for tail in choices {
                    cases.push(CrashCase {
                        cut: index,
                        keep: frontier,
                        tails: BTreeMap::from([(path.to_owned(), tail)]),
                        keep_entries: true,
                        fail_sync: false,
                    });
                }
            }
        }
    }
    assert_eq!(
        cases
            .iter()
            .filter(|case| {
                !case.fail_sync
                    && !case.keep_entries
                    && case.tails.is_empty()
                    && case.keep == cuts[case.cut].world.tapes[BARCODE].durable_frontier
            })
            .map(|case| case.cut)
            .collect::<BTreeSet<_>>()
            .len(),
        cuts.len(),
        "every cut has the minimum-survival crash",
    );
    cases
}

async fn run_case(source: &Path, cuts: &[Cut], case: &CrashCase) -> Result<Coverage, String> {
    let cut = &cuts[case.cut];
    let mut coverage = Coverage::default();
    let failed_cut;
    let cut = if case.fail_sync {
        use remanence_library::durable_file::DurableDirectory;
        let path = cut.before_sync.as_ref().unwrap();
        let failed = MemoryDirectory::from_snapshot(cut.files.clone());
        failed.fail_next_sync(path).unwrap();
        let mut file = failed.open(path).unwrap();
        assert!(file.sync().is_err());
        // A later successful sync does not restore bytes lost by the error.
        file.sync().unwrap();
        coverage.failed_syncs = 1;
        failed_cut = Cut {
            boundary: format!(
                "failed sync {}",
                path.strip_prefix(source).unwrap().display()
            ),
            before_sync: None,
            files: failed.snapshot(),
            ..cut.clone()
        };
        &failed_cut
    } else {
        cut
    };
    coverage.torn_tails = usize::from(!case.tails.is_empty());
    check_crash(
        source,
        cut,
        case.keep,
        case.tails.clone(),
        case.keep_entries,
        &mut coverage,
    )
    .await?;
    Ok(coverage)
}

impl Coverage {
    fn merge(&mut self, other: Self) {
        self.failed_syncs += other.failed_syncs;
        self.torn_tails += other.torn_tails;
        self.crashes += other.crashes;
        self.completed += other.completed;
        self.acknowledged_tails += other.acknowledged_tails;
        self.released_fences += other.released_fences;
        for (category, count) in other.unnamed_refusals {
            *self.unnamed_refusals.entry(category).or_default() += count;
        }
        for (category, count) in other.refused {
            *self.refused.entry(category).or_default() += count;
        }
        for (category, example) in other.refusal_examples {
            self.refusal_examples.entry(category).or_insert(example);
        }
    }
}

async fn property(skip_barrier: bool, full: bool) -> Result<Coverage, String> {
    let root = tempfile::tempdir().unwrap();
    let cuts = trace_session(root.path(), skip_barrier).await;
    assert!(cuts.iter().any(|cut| cut.boundary.starts_with("Append")));
    assert!(cuts
        .iter()
        .any(|cut| cut.boundary.starts_with("BeforeSync")));
    assert!(cuts.iter().any(|cut| cut.receipts.len() == 3));
    let cases = crash_cases(&cuts, full);
    eprintln!(
        "durability trace: {} cut points, {} cases, skip_barrier={skip_barrier}, full={full}",
        cuts.len(),
        cases.len()
    );
    let mut coverage = Coverage {
        cuts: cuts.len(),
        ..Coverage::default()
    };
    if full || skip_barrier {
        // Keep the full matrix sequential and stop the negative control at its
        // first actual data-loss finding. Both use the identical restart checks.
        for case in &cases {
            coverage.merge(run_case(root.path(), &cuts, case).await?);
        }
    } else {
        // Four independent state directories and worlds bound concurrent engine
        // work. Merge in case order so diagnostic examples remain deterministic.
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        let mut results = Vec::new();
                        loop {
                            let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            let Some(case) = cases.get(index) else {
                                break;
                            };
                            results.push((
                                index,
                                runtime.block_on(run_case(root.path(), &cuts, case)),
                            ));
                        }
                        results
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("crash worker never panics"))
                .collect::<Vec<_>>()
        });
        let mut results = results;
        results.sort_by_key(|(index, _)| *index);
        for (_, result) in results {
            coverage.merge(result?);
        }
    }
    assert_eq!(coverage.cuts, cuts.len());
    assert_eq!(coverage.crashes, cases.len());
    assert!(coverage.failed_syncs > 0);
    assert!(coverage.torn_tails > 0);
    assert!(coverage.acknowledged_tails > 0);
    assert_eq!(
        coverage
            .unnamed_refusals
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            "missing bootstrap",
            "orphan parity bundle",
            "short audit header",
            "short parity header",
            "torn parity frame",
        ]),
        "existing refusal identity defects change; review the findings"
    );
    Ok(coverage)
}

#[tokio::test]
async fn acknowledged_prefix_survives_crashes() {
    let start = std::time::Instant::now();
    let coverage = property(false, false)
        .await
        .expect("acknowledged prefix survives every default crash");
    eprintln!(
        "default durability coverage: {coverage:?}; elapsed {:?}",
        start.elapsed()
    );
}

#[tokio::test]
#[ignore = "full durability matrix, about 12 minutes: cargo test -p remanence-api --lib durability -- --ignored"]
async fn acknowledged_prefix_survives_full_durability_matrix() {
    let start = std::time::Instant::now();
    let coverage = property(false, true)
        .await
        .expect("acknowledged prefix survives every full-matrix crash");
    eprintln!(
        "full durability coverage: {coverage:?}; elapsed {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn acknowledged_prefix_property_detects_missing_barriers() {
    let start = std::time::Instant::now();
    let failure = property(true, false)
        .await
        .expect_err("skipping barriers must violate the same property");
    assert!(
        failure.contains("loses tape bytes"),
        "unexpected failure: {failure}"
    );
    eprintln!("negative control: {failure}; elapsed {:?}", start.elapsed());
}
