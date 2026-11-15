//! Crash exploration through the same in-process write-session engine as the daemon.
//! Each command, file append, sync, and client receipt captures a restartable cut.

#[path = "acceptance_finalization.rs"]
mod acceptance_finalization;

#[path = "ingress_deadline_media_tests.rs"]
mod ingress_deadline_media_tests;

#[path = "b9_fix_tests.rs"]
mod b9_fix_tests;

#[path = "b9_tests.rs"]
mod b9_tests;

#[path = "b10_fix_tests.rs"]
mod b10_fix_tests;
#[path = "b7_tests.rs"]
mod b7_tests;

#[path = "b6c_tests.rs"]
mod b6c_tests;

#[path = "b6b_fix_tests.rs"]
mod b6b_fix_tests;

#[path = "host_record_tests.rs"]
mod host_record_tests;

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
    setup_with_barcode(root, BARCODE)
}

fn setup_with_barcode(root: &Path, barcode: &str) -> Arc<Mutex<VirtualWorld>> {
    let mut index = remanence_state::projector::open(root.join("state.sqlite"))
        .map(|catalog| catalog.write_handle())
        .unwrap();
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
            voltag: barcode.into(),
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
    world.put_tape_in_drive(BAY, barcode, Some(0x400), tape);
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
    cfg.checkpoint_journal_dir = root.join("journals/tapes");
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.calibration_store = CalibrationControlStore::open(root.join("calibration")).unwrap();
    cfg.checkpoint_max_objects = 100;
    cfg.checkpoint_max_age_seconds = 3600;
    cfg.fixture_audit_mut().audit_fsync = true;
    cfg
}

/// Provision the durability tape before opening its typed runtime, then attach its pool.
/// The returned directory also hosts setup's unused legacy catalog and virtual-media inputs.
fn typed_root() -> (crate::tests::TypedRoot, PathBuf) {
    let typed = crate::tests::TypedRoot::open_with_tape_inputs([(
        ProvisionTapeInput {
            tape_uuid: TAPE,
            voltag: BARCODE.into(),
            block_size: BLOCK,
            parity: ParityConfig::Scheme(scheme()),
            force: false,
        },
        remanence_state::LabelProvenance::Operator,
        Some(MediaObservation {
            generation: LtoGen::Lto9,
            source: GenerationSource::Operator,
        }),
    )]);
    let root = typed.paths().sqlite_path.parent().unwrap().to_owned();
    let mut index = typed.catalog().write_handle();
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: POOL.into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .unwrap();
    index.project_tape_pool_membership(TAPE, POOL).unwrap();
    drop(index);
    (typed, root)
}

/// Match the durability config's overrides while retaining the typed authority and native paths.
fn typed_config(
    typed: &crate::tests::TypedRoot,
    library: &remanence_library::LibraryHandle,
) -> WriteOwnerConfig {
    let mut cfg = test_typed_write_owner_config(
        typed,
        library,
        library_snapshot_cell(library.library().clone()),
    );
    std::fs::create_dir_all(&cfg.checkpoint_journal_dir).unwrap();
    cfg.calibration_store = CalibrationControlStore::open(&typed.paths().calibration_dir).unwrap();
    cfg.checkpoint_max_objects = 100;
    cfg.checkpoint_max_age_seconds = 3600;
    cfg
}

/// Configure a typed actor with its catalog and forward the caller's tracing subscriber.
async fn open_typed_writer_configured(
    typed: &crate::tests::TypedRoot,
    mut library: remanence_library::LibraryHandle,
    selection: SelectedTape,
    configure: impl FnOnce(&mut WriteOwnerConfig, &mut TapePoolConfig),
) -> (
    mpsc::Sender<DriveCommand>,
    Result<Uuid, Status>,
    std::thread::JoinHandle<()>,
) {
    let mut cfg = typed_config(typed, &library);
    let mut index = typed.catalog().write_handle();
    let mut pool_cfg = pool();
    configure(&mut cfg, &mut pool_cfg);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let actor_tx = tx.clone();
    let (reply, received) = oneshot::channel();
    let request = super::super::actor_runtime::OpenWriteActorRequest {
        actor: AuditActor::System,
        pool_cfg,
        selected: selection,
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: Some(LIBRARY.into()),
        barcode: index.get_tape(&TAPE).unwrap().unwrap().voltag,
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        reply,
    };
    // The actor's log lines reach the caller's subscriber, when it has one.
    let dispatch = tracing::dispatcher::get_default(|current| current.clone());
    let thread = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            handle_drive_open_write(
                BAY, &mut index, &cfg, actor_tx, &mut rx, &mut drive, &mut 0, None, request,
            );
        });
    });
    let opened = received
        .await
        .expect("writer replies without panic")
        .map(|session| Uuid::from_slice(&session.session_id).unwrap());
    (tx, opened, thread)
}

/// Leave real typed pool-session debt by checkpointing a file and aborting at the shutdown deadline.
async fn typed_due_tape(
    typed: &crate::tests::TypedRoot,
    root: &Path,
    world: Arc<Mutex<VirtualWorld>>,
) {
    let lifecycle = DrivePoolLifecycle::default();
    let (tx, opened, thread) =
        open_typed_writer_configured(typed, open_model_library(world), selected(), |cfg, _| {
            cfg.lifecycle = Some(lifecycle.clone())
        })
        .await;
    let id = opened.unwrap();
    append_actor_test_file(&tx, id, root.join("saved"), "saved", "saved", b"committed").await;
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Checkpoint {
        session_id: id,
        trigger: CheckpointTrigger::Explicit,
        expected_batch_id: None,
        reply: Some(reply),
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    let mut state = crate::ApiState::new_with_pool_configs(typed.catalog(), vec![pool()]);
    state.checkpoint_journal_dir = Arc::new(typed.paths().journal_dir.join("tapes"));
    state.drive_idle_unload_seconds = 0;
    crate::closeout_obligation::scan(&state).unwrap();
    assert!(
        state.closeout_work.contains((TAPE, 1)),
        "live owner debt is observable without stealing its lease"
    );
    assert!(crate::closeout_obligation::summary(&state, TAPE)
        .unwrap()
        .open_session
        .is_some());
    lifecycle
        .shutdown_deadline
        .set(std::time::Instant::now())
        .unwrap();
    lifecycle.shutting_down.store(true, Ordering::SeqCst);
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: Some("deadline test".into()),
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().unwrap();
    assert!(crate::closeout_obligation::summary(&state, TAPE)
        .unwrap()
        .closeout_due());
}

/// A lost open reply still has a durable owner and must end it before releasing
/// the drive; it cannot leave an anonymous epoch for the next session.
#[test]
fn lost_open_reply_ends_the_durable_session() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let mut library = open_model_library(world);
    let cfg = config(root.path(), &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let (actor_tx, mut rx) = mpsc::channel(16);
    let (reply, received) = oneshot::channel();
    drop(received);
    let request = super::super::actor_runtime::OpenWriteActorRequest {
        actor: AuditActor::System,
        pool_cfg: pool(),
        selected: selected(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: Some(LIBRARY.into()),
        barcode: Some(BARCODE.into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        reply,
    };
    let mut index = remanence_state::projector::open(&cfg.index_path)
        .map(|catalog| catalog.write_handle())
        .unwrap();
    handle_drive_open_write(
        BAY, &mut index, &cfg, actor_tx, &mut rx, &mut drive, &mut 0, None, request,
    );
    let authority = TapeAuthority::open(&cfg.checkpoint_journal_dir, TAPE, index.path()).unwrap();
    let snapshot = authority.freeze().unwrap();
    assert!(snapshot.tape_summary().open_session.is_none());
    let end = snapshot.tape_summary().last_session_end.as_ref().unwrap();
    assert_eq!(
        end.reason,
        remanence_state::tape_log::SessionEndReason::ClientDisconnected
    );
    assert_eq!(end.tail_file_count, 0);
    assert!(!snapshot.tape_summary().closeout_due());
    assert!(index.list_active_tape_io_fences().unwrap().is_empty());
}

/// The detached RPC task may receive an actor reply after its caller vanished.
/// Its explicit disconnect command ends the already-open owner without a mark.
#[tokio::test]
async fn disconnected_open_task_ends_the_session_and_releases_authority() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let (tx, opened, thread) = open_writer(root.path(), open_model_library(world.clone())).await;
    let id = opened.unwrap();
    let before = world.lock().unwrap().tapes[BARCODE].records.clone();
    let (reply, ended) = oneshot::channel();
    tx.send(DriveCommand::DisconnectWrite {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    ended.await.unwrap().unwrap();
    thread.join().unwrap();
    let index = remanence_state::projector::open(root.path().join("state.sqlite"))
        .map(|catalog| catalog.write_handle())
        .unwrap();
    let authority =
        TapeAuthority::open(root.path().join("journals/tapes"), TAPE, index.path()).unwrap();
    let lease = authority.acquire_exclusive().unwrap();
    let snapshot = lease.freeze().unwrap();
    let end = snapshot.tape_summary().last_session_end.as_ref().unwrap();
    assert_eq!(
        end.reason,
        remanence_state::tape_log::SessionEndReason::ClientDisconnected
    );
    assert_eq!(end.tail_file_count, 0);
    assert!(snapshot.tape_summary().open_session.is_none());
    assert_eq!(world.lock().unwrap().tapes[BARCODE].records, before);
    assert!(index.list_active_tape_io_fences().unwrap().is_empty());
}

/// A write session's close leaves the release to the actor, which removes the
/// session from the pool's session map once the session has ended. That map is
/// where live status reads a drive's `session_id`, so after a real write close a
/// status names no session. The API records the session in the map when the open
/// returns, so the test does the same on a pool that shares the actor's lifecycle.
#[tokio::test]
async fn write_close_release_empties_the_pools_session_map() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let lifecycle = DrivePoolLifecycle::default();
    let drive_key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(drive_key.clone(), AtomicBool::new(true))]));
    let pool = DrivePool::new_with_lifecycle(
        HashMap::new(),
        HashMap::new(),
        Arc::clone(&reservations),
        lifecycle.clone(),
    );
    let (tx, opened, thread) = open_writer_configured(
        root.path(),
        open_model_library(world),
        selected(),
        |cfg, _| {
            cfg.lifecycle = Some(lifecycle);
            cfg.reservations = Arc::clone(&reservations);
        },
    )
    .await;
    let id = opened.unwrap();
    pool.record_session(
        id,
        MountedSession {
            bay: BAY,
            library_serial: LIBRARY.to_string(),
            barcode: Some(BARCODE.to_string()),
            home_slot: None,
            tape_uuid: TAPE,
            drive_uuid: None,
        },
    );
    assert_eq!(
        pool.session_ids_by_bay(LIBRARY),
        HashMap::from([(BAY, id.as_bytes().to_vec())]),
        "the open session is in the pool's map"
    );
    close_writer(&tx, id, thread).await;
    assert!(
        pool.session_ids_by_bay(LIBRARY).is_empty(),
        "the actor's release emptied the pool's session map"
    );
    assert!(pool.sessions_by_drive().is_empty());
    assert!(!reservations[&drive_key].load(Ordering::SeqCst));
}

/// Run one session handler, joining its thread after close or refusal.
async fn open_writer(
    root: &Path,
    library: remanence_library::LibraryHandle,
) -> (
    mpsc::Sender<DriveCommand>,
    Result<Uuid, Status>,
    std::thread::JoinHandle<()>,
) {
    open_writer_with_selection(root, library, selected()).await
}

/// Select fixture geometry without changing the normal durability scenario.
async fn open_writer_with_selection(
    root: &Path,
    library: remanence_library::LibraryHandle,
    selection: SelectedTape,
) -> (
    mpsc::Sender<DriveCommand>,
    Result<Uuid, Status>,
    std::thread::JoinHandle<()>,
) {
    open_writer_configured(root, library, selection, |_, _| {}).await
}

/// Configure one isolated actor before its native session is opened.
async fn open_writer_configured(
    root: &Path,
    mut library: remanence_library::LibraryHandle,
    selection: SelectedTape,
    configure: impl FnOnce(&mut WriteOwnerConfig, &mut TapePoolConfig),
) -> (
    mpsc::Sender<DriveCommand>,
    Result<Uuid, Status>,
    std::thread::JoinHandle<()>,
) {
    let mut cfg = config(root, &library);
    let mut pool_cfg = pool();
    configure(&mut cfg, &mut pool_cfg);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let actor_tx = tx.clone();
    let (reply, received) = oneshot::channel();
    let request = super::super::actor_runtime::OpenWriteActorRequest {
        actor: AuditActor::System,
        pool_cfg,
        selected: selection,
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: Some(LIBRARY.into()),
        barcode: remanence_state::projector::open(&cfg.index_path)
            .map(|catalog| catalog.write_handle())
            .unwrap()
            .get_tape(&TAPE)
            .unwrap()
            .unwrap()
            .voltag,
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        reply,
    };
    // The actor's log lines reach the caller's subscriber, when it has one.
    let dispatch = tracing::dispatcher::get_default(|current| current.clone());
    let thread = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            let mut index = remanence_state::projector::open(&cfg.index_path)
                .map(|catalog| catalog.write_handle())
                .unwrap();
            handle_drive_open_write(
                BAY, &mut index, &cfg, actor_tx, &mut rx, &mut drive, &mut 0, None, request,
            );
        });
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
        let mut index = remanence_state::projector::open(&cfg.index_path)
            .map(|catalog| catalog.write_handle())
            .unwrap();
        handle_drive_open_read(
            BAY, &mut index, &cfg, &mut rx, &mut drive, &mut 0, None, request,
        );
    });
    let opened = received.await.expect("read engine replies without panic");
    let session = match opened {
        Ok(session) => Uuid::from_slice(&session.session_id).unwrap(),
        Err(status) => {
            thread.join().unwrap();
            return Err(format!("read open refuses acknowledged object: {status}"));
        }
    };
    let index = remanence_state::projector::open(root.join("state.sqlite"))
        .map(|catalog| catalog.write_handle())
        .unwrap();
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
    let mut index = remanence_state::projector::open(root.join("state.sqlite"))
        .map(|catalog| catalog.write_handle())
        .unwrap();
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
    let refusal = crate::write_owner::supersede_mounted_tail(
        &mut index,
        &cfg.terminal_config(),
        &mut drive,
        TAPE,
        generation,
        BARCODE,
        AuditActor::System,
        |required, found, sequence| {
            preview = Some((required.to_owned(), found.to_vec(), sequence));
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
    let Some((confirmation, found, sequence)) = preview else {
        return Err(refusal);
    };
    crate::write_owner::supersede_mounted_tail(
        &mut index,
        &cfg.terminal_config(),
        &mut drive,
        TAPE,
        generation,
        BARCODE,
        AuditActor::System,
        |required, current, current_sequence| {
            assert_eq!(current_sequence, sequence);
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
        || cut.receipts.is_empty() && message.contains("locate parity checkpoint EOD:")
    {
        "missing bootstrap"
    } else if message.contains("read audit segment header") {
        "short audit header"
    } else {
        return Err(format!(
            "unexpected recovery refusal at {} keep={keep}: {status}",
            cut.boundary
        ));
    };
    assert_eq!(
        status.code(),
        if message.contains("locate parity checkpoint EOD:") {
            tonic::Code::Unavailable
        } else if category == "short audit header" {
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
    lifted_fences: BTreeSet<(String, String, String)>,
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
    let mut index = remanence_state::projector::open(temp.path().join("state.sqlite"))
        .map(|catalog| catalog.write_handle())
        .unwrap();
    let replay = crate::startup_checkpoint::replay_checkpoint_journal_projections(
        &mut index,
        &temp.path().join("journals/tapes"),
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
    let authority = remanence_state::authority::fixtures::AuthorityBuilder::new(
        temp.path().join("journals/tapes"),
        TAPE,
    )
    .build()
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
                let protected_start = authority.last().map_or(2, |record| record.eod_lba);
                // Admission closes any prior session's debt before opening the next epoch.
                let expected_start =
                    remanence_state::projector::open(temp.path().join("state.sqlite"))
                        .map(|catalog| catalog.write_handle())
                        .unwrap()
                        .get_tape(&TAPE)
                        .unwrap()
                        .unwrap()
                        .written_extent_lba
                        .unwrap();
                let before = world.lock().unwrap().tapes[BARCODE]
                    .records
                    .get(..protected_start as usize)
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
                let index = remanence_state::projector::open(temp.path().join("state.sqlite"))
                    .map(|catalog| catalog.write_handle())
                    .unwrap();
                let next = index.find_native_object_instances(&next_id).unwrap();
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
    let mut index = remanence_state::projector::open(temp.path().join("state.sqlite"))
        .map(|catalog| catalog.write_handle())
        .unwrap();
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

#[allow(dead_code)] // Retained legacy entry point; property now uses the typed engine.
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
        self.lifted_fences.extend(other.lifted_fences);
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

/// Build a session config over a reopened typed catalog without a new fixture constructor.
fn crash_typed_config(
    paths: &remanence_state::StatePaths,
    catalog: &remanence_state::ProjectionClient,
    library: &remanence_library::LibraryHandle,
) -> WriteOwnerConfig {
    let mut cfg = test_write_owner_config(
        paths.sqlite_path.clone(),
        paths.audit_dir.clone(),
        library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.authority = crate::write_owner::TerminalAuthority::Typed(
        catalog
            .library_runtime()
            .expect("crash catalog has a runtime"),
    );
    cfg.catalog = catalog.clone();
    cfg.checkpoint_journal_dir = paths.journal_dir.join("tapes");
    cfg.calibration_store = CalibrationControlStore::open(&paths.calibration_dir).unwrap();
    cfg.checkpoint_max_objects = 100;
    cfg.checkpoint_max_age_seconds = 3600;
    cfg
}

/// Seed all pre-registration files, including locks and the two closed SQLite catalogs.
fn seed_typed_crash_files(root: &Path, files: &MemoryDirectory) {
    for entry in std::fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            seed_typed_crash_files(&path, files);
        } else {
            files.seed(path.clone(), std::fs::read(path).unwrap());
        }
    }
}

/// Open a writer using the reopened runtime and its typed catalog.
async fn open_crash_typed_writer(
    paths: &remanence_state::StatePaths,
    catalog: &remanence_state::ProjectionClient,
    mut library: remanence_library::LibraryHandle,
) -> (
    mpsc::Sender<DriveCommand>,
    Result<Uuid, Status>,
    std::thread::JoinHandle<()>,
) {
    let cfg = crash_typed_config(paths, catalog, &library);
    let mut index = catalog.write_handle();
    let pool_cfg = pool();
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let actor_tx = tx.clone();
    let (reply, received) = oneshot::channel();
    let request = super::super::actor_runtime::OpenWriteActorRequest {
        actor: AuditActor::System,
        pool_cfg,
        selected: selected(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: Some(LIBRARY.into()),
        barcode: index.get_tape(&TAPE).unwrap().unwrap().voltag,
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        reply,
    };
    // The actor's log lines reach the caller's subscriber, when it has one.
    let dispatch = tracing::dispatcher::get_default(|current| current.clone());
    let thread = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            handle_drive_open_write(
                BAY, &mut index, &cfg, actor_tx, &mut rx, &mut drive, &mut 0, None, request,
            );
        });
    });
    let opened = received
        .await
        .expect("writer replies without panic")
        .map(|session| Uuid::from_slice(&session.session_id).unwrap());
    (tx, opened, thread)
}

/// Trace a typed session with every authority handle opened inside the registration.
async fn trace_typed_session(skip_barrier: bool) -> (crate::tests::TypedRoot, PathBuf, Vec<Cut>) {
    let (typed, root) = typed_root();
    trace_typed_session_from_root(typed, root, skip_barrier).await
}

/// Finalization needs a source-backed pool assignment, made before any crash capture.
async fn trace_pooled_typed_session() -> (crate::tests::TypedRoot, PathBuf, Vec<Cut>) {
    let mut typed = crate::tests::TypedRoot::open_with_tape_inputs([(
        ProvisionTapeInput {
            tape_uuid: TAPE,
            voltag: BARCODE.into(),
            block_size: BLOCK,
            parity: ParityConfig::Scheme(scheme()),
            force: false,
        },
        remanence_state::LabelProvenance::Operator,
        Some(MediaObservation {
            generation: LtoGen::Lto9,
            source: GenerationSource::Operator,
        }),
    )]);
    let paths = typed.paths().clone();
    let root = paths.sqlite_path.parent().unwrap().to_owned();
    let mut config = typed.config();
    config.tape_pools.push(
        serde_json::from_value(serde_json::json!({"id": POOL, "block_size": BLOCK})).unwrap(),
    );
    typed.release();
    let mut state = remanence_state::StateHandle::open_with_config(paths, config).unwrap();
    let generation = state
        .library_runtime()
        .tape_state(TAPE)
        .unwrap()
        .assignment_generation;
    state
        .assign_tape_pool(TAPE, generation, Some(POOL))
        .unwrap();
    drop(state);
    trace_typed_session_from_root(typed, root, false).await
}

/// Preserve the matrix's write trace while allowing finalization to supply its pooled root.
async fn trace_typed_session_from_root(
    mut typed: crate::tests::TypedRoot,
    root: PathBuf,
    skip_barrier: bool,
) -> (crate::tests::TypedRoot, PathBuf, Vec<Cut>) {
    let paths = typed.paths().clone();
    let world = setup(&root);
    typed.release();
    let files = MemoryDirectory::default();
    seed_typed_crash_files(&root, &files);
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
    let runtime = remanence_state::library_runtime::LibraryRuntime::open(
        &paths,
        true,
        Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap()),
    )
    .unwrap();
    let catalog = remanence_state::projector::open(&paths.sqlite_path)
        .unwrap()
        .with_library_runtime(runtime.clone());
    // Discovery authenticates the drive before sessions use it; this append is modelled too.
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut catalog.write_handle(),
        [library.library()],
        &HashSet::from([LIBRARY.to_string()]),
    )
    .unwrap();
    let (tx, opened, thread) = open_crash_typed_writer(&paths, &catalog, library).await;
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
        let index = catalog.write_handle();
        let object_id = Uuid::from_slice(&object.object_id).unwrap().to_string();
        let copy = &index.find_native_object_instances(&object_id).unwrap()[0];
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
            &root,
            format!("client commit {number}"),
            None,
        );
    }
    close_writer(&tx, id, thread).await;
    trace.lock().unwrap().active = false;
    files.observe(None);
    let cuts = std::mem::take(&mut trace.lock().unwrap().cuts);
    drop(tx);
    drop(catalog);
    drop(runtime);
    (typed, root, cuts)
}

/// Read acknowledged bytes through a read actor attached to the reopened runtime.
async fn read_typed_receipts(
    paths: &remanence_state::StatePaths,
    catalog: &remanence_state::ProjectionClient,
    world: Arc<Mutex<VirtualWorld>>,
    receipts: &[Receipt],
) -> Result<(), serde_json::Value> {
    if receipts.is_empty() {
        return Ok(());
    }
    let mut library = open_model_library(world);
    let cfg = crash_typed_config(paths, catalog, &library);
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
    let mut index = catalog.write_handle();
    let thread = std::thread::spawn(move || {
        handle_drive_open_read(
            BAY, &mut index, &cfg, &mut rx, &mut drive, &mut 0, None, request,
        );
    });
    let opened = received.await.expect("read engine replies without panic");
    let session = match opened {
        Ok(session) => Uuid::from_slice(&session.session_id).unwrap(),
        Err(status) => {
            thread.join().unwrap();
            return Err(typed_record_refusal(
                "read open",
                &status,
                receipts.is_empty(),
            ));
        }
    };
    let index = catalog.write_handle();
    let mut outcome = Ok(());
    for receipt in receipts {
        let members = match index.list_native_object_files(&receipt.id) {
            Ok(members) if !members.is_empty() => members,
            other => {
                outcome = Err(typed_violation(
                    "read catalog",
                    format!(
                        "acknowledged object {} has no readable member: {other:?}",
                        receipt.id
                    ),
                ));
                break;
            }
        };
        let member = &members[0];
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
                outcome = Err(typed_violation(
                    "read bytes",
                    format!(
                        "acknowledged object {} fails byte-for-byte read: {other:?}",
                        receipt.id
                    ),
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
    let closed = received.await.unwrap();
    thread.join().expect("read-session engine does not panic");
    if outcome.is_ok() {
        closed.map_err(|status| typed_violation("read close", status.to_string()))?;
    }
    outcome
}

/// Match the production preview to immutable receipts before confirmation can discard data.
/// Object entries carry IDs; foreign entries carry an LBA, bounded by the next filemark.
fn typed_tail_preview(
    required: &str,
    found: &[String],
    sequence: u64,
    receipts: &[Receipt],
    records: &[Record],
) -> serde_json::Value {
    let mut matches = BTreeSet::new();
    let mut unlocated = Vec::new();
    for entry in found {
        if let Ok(id) = Uuid::parse_str(entry) {
            matches.extend(
                receipts
                    .iter()
                    .filter(|r| Uuid::parse_str(&r.id).ok() == Some(id))
                    .map(|r| r.id.clone()),
            );
        } else if let Some(start) = entry
            .split_once(" at LBA ")
            .and_then(|(_, rest)| {
                rest.split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse::<usize>()
                    .ok()
            })
            .filter(|&start| start < records.len())
        {
            let end = records[start..]
                .iter()
                .position(|r| matches!(r, Record::Filemark))
                .map_or(records.len(), |offset| start + offset + 1);
            matches.extend(
                receipts
                    .iter()
                    .filter(|r| r.start < end && start < r.start + r.records.len())
                    .map(|r| r.id.clone()),
            );
        } else {
            unlocated.push(entry.clone());
        }
    }
    serde_json::json!({
        "required_confirmation": required, "found": found, "sequence": sequence,
        "acknowledged_record_found": if unlocated.is_empty() || !matches.is_empty() { Some(!matches.is_empty()) } else { None },
        "acknowledged_object_ids": matches, "unlocated_found": unlocated,
    })
}

/// A barcode supplements the case's UUID; it cannot stand in for that UUID.
fn typed_fence_names_case_tape(fence: &remanence_state::TapeIoFenceRecord) -> bool {
    fence.tape_uuid == TAPE
        && (fence.barcode.is_none() || fence.barcode.as_deref() == Some(BARCODE))
}

/// Exercise positive, adjacent and unplaceable preview entries without a crash census.
#[test]
fn typed_tail_preview_matches_receipts() {
    let id = "abcdef01-2345-6789-abcd-ef0123456789";
    let records = vec![
        Record::Block(vec![1]),
        Record::Filemark,
        Record::Block(vec![2]),
        Record::Filemark,
    ];
    let receipts = [Receipt {
        id: id.into(),
        payload: vec![2],
        start: 2,
        records: records[2..].to_vec(),
    }];
    for (entry, expected, unlocated) in [
        (id.to_owned(), Some(true), false),
        ("foreign record at LBA 3".into(), Some(true), false),
        ("foreign record at LBA 0".into(), Some(false), false),
        ("foreign record at LBA 4".into(), None, true),
        ("unparsable entry".into(), None, true),
        (
            Uuid::parse_str(id)
                .unwrap()
                .simple()
                .to_string()
                .to_uppercase(),
            Some(true),
            false,
        ),
    ] {
        let found = [entry.clone()];
        let preview = typed_tail_preview("SUPERSEDE fixture", &found, 7, &receipts, &records);
        assert_eq!(
            preview["acknowledged_record_found"].as_bool(),
            expected,
            "{entry}: {preview}"
        );
        assert_eq!(
            preview["acknowledged_object_ids"],
            if expected == Some(true) {
                serde_json::json!([id])
            } else {
                serde_json::json!([])
            },
            "{entry}: {preview}"
        );
        assert_eq!(
            preview["unlocated_found"],
            if unlocated {
                serde_json::json!([entry])
            } else {
                serde_json::json!([])
            },
            "{preview}"
        );
    }
}

/// Reject the right barcode on the wrong UUID, and accept only an absent or matching barcode.
#[test]
fn typed_fence_tape_identity() {
    let mut fence = remanence_state::TapeIoFenceRecord {
        fence_id: 1,
        tape_uuid: TAPE.to_vec(),
        barcode: Some(BARCODE.into()),
        state: "active".into(),
        reason: "session_suffix_contradiction".into(),
        evidence_json: None,
        quarantine_id: "library:1".into(),
        created_at_utc: String::new(),
        updated_at_utc: String::new(),
        release_ack: None,
        endpoint: None,
        release_forced: None,
    };
    assert!(typed_fence_names_case_tape(&fence));
    fence.tape_uuid = vec![0xe0; 16];
    assert!(!typed_fence_names_case_tape(&fence));
    fence.tape_uuid = TAPE.to_vec();
    fence.barcode = None;
    assert!(typed_fence_names_case_tape(&fence));
    fence.barcode = Some("OTHER".into());
    assert!(!typed_fence_names_case_tape(&fence));
}

/// Require the same tape identity and fenced read admission as the legacy crash check.
fn check_typed_fence_before_lift(
    index: &remanence_state::CatalogIndex,
    fence: &remanence_state::TapeIoFenceRecord,
) -> Result<serde_json::Value, serde_json::Value> {
    if !typed_fence_names_case_tape(fence) {
        return Err(typed_violation(
            "fence tape",
            format!(
                "fence {} does not name the case's tape: {fence:?}",
                fence.quarantine_id
            ),
        ));
    }
    let read = session_open_reject_tape_io_fences(index, &TAPE, Some(BARCODE), "open read session");
    match read {
        Err(status)
            if status.code() == tonic::Code::FailedPrecondition
                && status
                    .message()
                    .contains(&Uuid::from_bytes(TAPE).to_string()) =>
        {
            Ok(serde_json::json!({
                "quarantine_id": fence.quarantine_id, "tape_uuid": fence.tape_uuid,
                "barcode": fence.barcode, "read_refused": true,
                "read_refusal": typed_record_refusal("pre-lift read admission", &status, false),
            }))
        }
        other => Err(typed_violation(
            "pre-lift read admission",
            format!(
                "fence {}: expected FailedPrecondition naming the tape, got {other:?}",
                fence.quarantine_id
            ),
        )),
    }
}

/// Preview and confirm tail recovery using the typed terminal authority.
fn acknowledge_typed_tail(
    paths: &remanence_state::StatePaths,
    catalog: &remanence_state::ProjectionClient,
    world: Arc<Mutex<VirtualWorld>>,
    receipts: &[Receipt],
    recovery: &mut serde_json::Value,
) -> Result<(), serde_json::Value> {
    use super::super::terminal_types::TerminalConfig;
    *recovery = serde_json::json!({"preview": null, "confirmed": false, "fence_checks": []});
    let mut library = open_model_library(world.clone());
    let cfg = crash_typed_config(paths, catalog, &library);
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let mut index = catalog.write_handle();
    for fence in index
        .list_active_tape_io_fences()
        .map_err(|error| typed_violation("supersede fences", error.to_string()))?
    {
        let check = check_typed_fence_before_lift(&index, &fence);
        recovery["fence_checks"]
            .as_array_mut()
            .unwrap()
            .push(match &check {
                Ok(observation) => observation.clone(),
                Err(failure) => failure.clone(),
            });
        check?;
    }
    prepare_drive_for_write(
        &mut drive,
        &TAPE,
        BLOCK,
        Uuid::new_v4(),
        WriteMediaPolicy::RewritableObject,
    )
    .map_err(|error| typed_violation("supersede prepare", error.to_string()))?;
    let generation = drive.identity_generation();
    let before = world.lock().unwrap().tapes[BARCODE].records.clone();
    let mut preview = None;
    let preview_result = crate::write_owner::supersede_mounted_tail(
        &mut index,
        &cfg.terminal_config(),
        &mut drive,
        TAPE,
        generation,
        BARCODE,
        AuditActor::System,
        |required, found, sequence| {
            recovery["preview"] = typed_tail_preview(required, found, sequence, receipts, &before);
            preview = Some((required.to_owned(), found.to_vec(), sequence));
            Err("confirmation required".into())
        },
    );
    let refusal = preview_result.map_or_else(Ok, |_| {
        Err(typed_violation(
            "supersede preview",
            "preview accepted without confirmation",
        ))
    })?;
    if world.lock().unwrap().tapes[BARCODE].records != before {
        return Err(typed_violation("supersede preview", "preview changed tape"));
    }
    let Some((confirmation, found, sequence)) = preview else {
        return Err(typed_record_refusal(
            "supersede",
            &refusal,
            receipts.is_empty(),
        ));
    };
    if recovery["preview"]["acknowledged_record_found"].as_bool() != Some(false) {
        return Err(typed_violation(
            "supersede preview",
            format!(
                "preview may discard an acknowledged record or contains an unlocated entry: {}",
                recovery["preview"]
            ),
        ));
    }
    crate::write_owner::supersede_mounted_tail(
        &mut index,
        &cfg.terminal_config(),
        &mut drive,
        TAPE,
        generation,
        BARCODE,
        AuditActor::System,
        |required, current, current_sequence| {
            if current_sequence != sequence || required != confirmation || current != found {
                return Err("supersede preview changed before confirmation".into());
            }
            Ok(())
        },
    )
    .map(|_| {
        recovery["confirmed"] = true.into();
    })
    .map_err(|status| typed_record_refusal("supersede", &status, receipts.is_empty()))
}

/// Describe a refusal without deciding whether its category or code is acceptable.
fn typed_record_refusal(step: &str, status: &Status, no_receipts: bool) -> serde_json::Value {
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
        || no_receipts && message.contains("locate parity checkpoint EOD:")
    {
        "missing bootstrap"
    } else if message.contains("read audit segment header") {
        "short audit header"
    } else {
        "new"
    };
    let expected_code = if message.contains("locate parity checkpoint EOD:") {
        tonic::Code::Unavailable
    } else if category == "short audit header" {
        tonic::Code::Internal
    } else if category == "torn parity frame" && !message.contains("open parity tape journal") {
        tonic::Code::Unavailable
    } else {
        tonic::Code::FailedPrecondition
    };
    let end = (0..=message.len().min(200))
        .rev()
        .find(|&end| message.is_char_boundary(end))
        .unwrap();
    serde_json::json!({
        "outcome": "refused", "step": step,
        "status_code": format!("{:?}", status.code()),
        "status_number": status.code() as i32,
        "legacy_status_matches": category != "new" && status.code() == expected_code,
        "message": &message[..end], "full_message": message,
        "category": category,
        "names_tape": message.contains(&Uuid::from_bytes(TAPE).to_string()) || message.contains(BARCODE),
    })
}

/// Keep a first failing step and its full diagnostic, without asserting a case outcome.
fn typed_violation(step: &str, message: impl Into<String>) -> serde_json::Value {
    serde_json::json!({"outcome": "property_violation", "step": step, "message": message.into()})
}

/// Relocate only the manifest binding; empty fixture directories are not represented by disk_image.
fn rebind_typed_crash_root(root: &Path) -> Result<remanence_state::StatePaths, serde_json::Value> {
    let paths = remanence_state::StatePaths {
        config_path: root.join("config"),
        state_dir: root.join("state"),
        audit_dir: root.join("library"),
        journal_dir: root.join("native"),
        sqlite_path: root.join("catalog.sqlite"),
        tape_cache_dir: root.join("cache"),
        calibration_dir: root.join("calibration"),
    };
    let rebind = || -> Result<(), Box<dyn std::error::Error>> {
        let path = paths.state_dir.join("authority-manifest.json");
        let mut manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
        for (key, directory) in [
            ("state_root", paths.state_dir.clone()),
            ("tape_root", paths.journal_dir.join("tapes")),
            ("library_root", paths.audit_dir.clone()),
        ] {
            // These directories existed before every source cut, including an empty tapes/.
            std::fs::create_dir_all(&directory)?;
            manifest[key] = serde_json::to_value(directory.canonicalize()?)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
        Ok(())
    };
    rebind().map_err(|error| typed_violation("manifest rebind", error.to_string()))?;
    Ok(paths)
}

/// Native runtime errors do not carry a tonic status; retain that distinction in the census.
fn typed_runtime_refusal(
    step: &str,
    error: impl std::fmt::Display,
    no_receipts: bool,
) -> serde_json::Value {
    let mut refusal = typed_record_refusal(
        step,
        &Status::failed_precondition(error.to_string()),
        no_receipts,
    );
    refusal["status_code"] = serde_json::Value::Null;
    refusal["status_number"] = serde_json::Value::Null;
    refusal["status_source"] = "RuntimeError (no RPC status)".into();
    refusal["legacy_status_matches"] = false.into();
    refusal
}

/// Retain all refusals and violations even when a later recovery step succeeds.
fn note_typed_failure(evidence: &mut serde_json::Value, failure: serde_json::Value) {
    let key = if failure["outcome"] == "refused" {
        "refusals"
    } else {
        "violations"
    };
    evidence[key].as_array_mut().unwrap().push(failure);
}

/// Resolve each active fence once, before recovery can remove its typed source.
fn record_typed_fence(
    runtime: &remanence_state::library_runtime::LibraryRuntime,
    evidence: &mut serde_json::Value,
    fence: &remanence_state::TapeIoFenceRecord,
    initial_ids: &BTreeSet<String>,
) -> usize {
    let rows = evidence["fences"].as_array_mut().unwrap();
    if let Some(position) = rows
        .iter()
        .position(|row| row["quarantine_id"] == fence.quarantine_id)
    {
        return position;
    }
    let typed = runtime.fence_for_quarantine_id(&fence.quarantine_id);
    rows.push(serde_json::json!({
        "quarantine_id": fence.quarantine_id, "at_reopen": initial_ids.contains(&fence.quarantine_id),
        "cause": typed.as_ref().map(|f| format!("{:?}", f.cause)),
        "reason": fence.reason, "evidence": fence.evidence_json,
        "tape_uuid": fence.tape_uuid, "barcode": fence.barcode,
        "released": false, "force": false, "release_error": null, "receipts_read": null,
        "lift_method": null, "lift_path": null,
    }));
    rows.len() - 1
}

/// Capture admission and active source identities before any post-open append or recovery.
fn record_typed_writer_open(
    catalog: &remanence_state::ProjectionClient,
    runtime: &remanence_state::library_runtime::LibraryRuntime,
    evidence: &mut serde_json::Value,
    refusal: Option<&serde_json::Value>,
    step: &str,
    initial_ids: &BTreeSet<String>,
) {
    match catalog.write_handle().list_active_tape_io_fences() {
        Ok(fences) => {
            let ids: Vec<_> = fences
                .iter()
                .map(|fence| fence.quarantine_id.clone())
                .collect();
            for fence in &fences {
                record_typed_fence(runtime, evidence, fence, initial_ids);
            }
            evidence["writer_opens"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "step": step, "admitted": refusal.is_none(), "active_fence_ids": ids,
                }));
            if let Some(refusal) = refusal {
                let count_ok = match refusal["category"].as_str() {
                    Some("uncommitted tape tail") => fences.len() == 1,
                    Some("missing bootstrap") => fences.len() <= 1,
                    _ => false,
                };
                let identities_ok = fences.iter().all(|fence| {
                    fence.tape_uuid == TAPE && runtime.fence_for_quarantine_id(&fence.quarantine_id)
                        .is_some_and(|f| f.cause == remanence_state::library_log::FenceCause::AppendAdmissionRefused)
                });
                if !count_ok || !identities_ok {
                    note_typed_failure(evidence, typed_violation("writer refusal fences", format!(
                        "{} refusal must leave {} AppendAdmissionRefused fence(s) on the case's tape; got {fences:?}",
                        refusal["category"], if refusal["category"] == "uncommitted tape tail" { "exactly one" } else { "zero or one" }
                    )));
                }
            }
        }
        Err(error) => note_typed_failure(
            evidence,
            typed_violation("writer open fences", error.to_string()),
        ),
    }
}

/// Snapshot the prefix before the append, or before reopening after a suffix rewrite.
fn typed_protected_prefix(
    world: &Arc<Mutex<VirtualWorld>>,
    protected_start: u64,
) -> Result<Vec<Record>, serde_json::Value> {
    world.lock().unwrap().tapes[BARCODE]
        .records
        .get(..protected_start as usize)
        .map(<[Record]>::to_vec)
        .ok_or_else(|| {
            typed_violation(
                "append cursor",
                "recovery admits an append beyond surviving tape",
            )
        })
}

/// Append, checkpoint and check the protected prefix after either successful writer admission.
/// Close and join the actor even when the append or checkpoint reports a violation.
async fn exercise_typed_admitted_writer(
    catalog: &remanence_state::ProjectionClient,
    world: &Arc<Mutex<VirtualWorld>>,
    input_root: &Path,
    protected_prefix: Result<Vec<Record>, serde_json::Value>,
    writer: (
        mpsc::Sender<DriveCommand>,
        Uuid,
        std::thread::JoinHandle<()>,
    ),
    evidence: &mut serde_json::Value,
) -> Result<(), serde_json::Value> {
    let (tx, id, thread) = writer;
    evidence["writer_reopened"] = true.into();
    evidence["step"] = "next append".into();
    // Always close the actor after admission, including a failing append/checkpoint.
    let appended = async {
        let index = catalog.write_handle();
        let expected_start = index
            .get_tape(&TAPE)
            .map_err(|e| typed_violation("append cursor", e.to_string()))?
            .and_then(|tape| tape.written_extent_lba)
            .ok_or_else(|| {
                typed_violation("append cursor", "admitted tape has no written extent")
            })?;
        let before = protected_prefix?;
        append_actor_test_file_result(
            &tx,
            id,
            input_root.join("next-input"),
            "next-file",
            "next-caller",
            b"next append",
        )
        .await
        .map_err(|s| typed_violation("next append", s.to_string()))?;
        evidence["step"] = "next checkpoint".into();
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Checkpoint {
            session_id: id,
            trigger: CheckpointTrigger::Explicit,
            expected_batch_id: None,
            reply: Some(reply),
        })
        .await
        .map_err(|e| typed_violation("next checkpoint", e.to_string()))?;
        let response = received
            .await
            .map_err(|e| typed_violation("next checkpoint", e.to_string()))?
            .map_err(|s| typed_violation("next checkpoint", s.to_string()))?;
        let object = response
            .committed_objects
            .first()
            .ok_or_else(|| typed_violation("next checkpoint", "no committed object"))?;
        let next_id = Uuid::from_slice(&object.object_id)
            .map_err(|e| typed_violation("next checkpoint", e.to_string()))?
            .to_string();
        let next = index
            .find_native_object_instances(&next_id)
            .map_err(|e| typed_violation("next append position", e.to_string()))?;
        if next.first().and_then(|copy| copy.global_start_block) != Some(expected_start) {
            return Err(typed_violation(
                "next append position",
                "next append does not immediately follow committed prefix",
            ));
        }
        if world.lock().unwrap().tapes[BARCODE]
            .records
            .get(..before.len())
            != Some(before.as_slice())
        {
            return Err(typed_violation(
                "protected prefix",
                "next append changed committed tape prefix",
            ));
        }
        Ok(())
    }
    .await;
    if let Err(failure) = appended {
        note_typed_failure(evidence, failure);
    }
    evidence["step"] = "writer close".into();
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .map_err(|e| typed_violation("writer close", e.to_string()))?;
    let closed = received
        .await
        .map_err(|e| typed_violation("writer close", e.to_string()))?;
    thread.join().expect("typed recovery writer never panics");
    if let Err(status) = closed {
        note_typed_failure(
            evidence,
            typed_violation("writer close", status.to_string()),
        );
    }
    Ok(())
}

/// Exercise restored authority, admission, recovery and exact-byte reads through production funnels.
async fn check_typed_crash(
    source: &Path,
    cut: &Cut,
    case: &CrashCase,
    evidence: &mut serde_json::Value,
) -> Result<(), serde_json::Value> {
    evidence["step"] = "restore".into();
    let temp = tempfile::tempdir().unwrap();
    restore_disk(temp.path(), &cut.disk);
    let mut files = cut.files.clone();
    files
        .crash(&case.tails, case.keep_entries)
        .map_err(|e| typed_violation("file crash", e.to_string()))?;
    files
        .materialize(source, temp.path())
        .map_err(|e| typed_violation("materialize", e.to_string()))?;
    let paths = rebind_typed_crash_root(temp.path())?;
    let mut world = cut.world.clone();
    world.tapes.get_mut(BARCODE).unwrap().records = cut.records.as_ref().clone();
    world
        .power_cut(BAY, case.keep)
        .map_err(|e| typed_violation("power cut", e.to_string()))?;
    for receipt in &cut.receipts {
        if world.tapes[BARCODE]
            .records
            .get(receipt.start..receipt.start + receipt.records.len())
            != Some(receipt.records.as_slice())
        {
            return Err(typed_violation(
                "receipt frontier",
                format!(
                    "{}: acknowledged object {} loses tape bytes at keep={}",
                    cut.boundary, receipt.id, case.keep
                ),
            ));
        }
    }
    let world = Arc::new(Mutex::new(world));
    evidence["step"] = "runtime open".into();
    let lease = remanence_state::StateLockGuard::acquire(&paths.state_dir)
        .map_err(|e| typed_runtime_refusal("runtime open", e, cut.receipts.is_empty()))?;
    let runtime =
        remanence_state::library_runtime::LibraryRuntime::open(&paths, true, Arc::new(lease))
            .map_err(|e| typed_runtime_refusal("runtime open", e, cut.receipts.is_empty()))?;
    let catalog = remanence_state::projector::open(&paths.sqlite_path)
        .map_err(|e| typed_violation("catalog open", e.to_string()))?
        .with_library_runtime(runtime.clone());
    evidence["runtime_reopened"] = true.into();
    // Restart discovery refreshes the same managed endpoint before writer or read admission.
    evidence["step"] = "drive observation".into();
    let library = world.lock().unwrap().library_snapshot();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut catalog.write_handle(),
        [&library],
        &HashSet::from([LIBRARY.to_string()]),
    )
    .map_err(|status| {
        typed_record_refusal("drive observation", &status, cut.receipts.is_empty())
    })?;
    let index = catalog.write_handle();
    let initial_fences = index
        .list_active_tape_io_fences()
        .map_err(|e| typed_violation("reopen fences", e.to_string()))?;
    let initial_ids: BTreeSet<_> = initial_fences
        .iter()
        .map(|f| f.quarantine_id.clone())
        .collect();
    let mut fences = initial_fences;
    evidence["step"] = "catalog prefix".into();
    // The summary comes from the production native authority, never an AuthorityBuilder.
    let summary = remanence_state::TapeAuthority::open(
        paths.journal_dir.join("tapes"),
        TAPE,
        &paths.sqlite_path,
    )
    .and_then(|authority| authority.observe_summary());
    let listed = index
        .list_native_objects()
        .map_err(|e| typed_violation("catalog prefix", e.to_string()))?;
    if let Ok(summary) = &summary {
        if listed.len() as u64 != summary.object_count {
            note_typed_failure(
                evidence,
                typed_violation(
                    "catalog prefix",
                    format!(
                        "catalog has {} objects; durable native prefix has {}",
                        listed.len(),
                        summary.object_count
                    ),
                ),
            );
        }
    }
    // A missing log before any receipt is observable; after a receipt it is a violation.
    evidence["native_summary_error"] = summary.as_ref().err().map(ToString::to_string).into();
    if let Err(error) = &summary {
        if !cut.receipts.is_empty() {
            note_typed_failure(
                evidence,
                typed_violation(
                    "native summary",
                    format!("native summary failed at a cut with acknowledged receipts: {error}"),
                ),
            );
        }
    }
    for receipt in &cut.receipts {
        if !listed.iter().any(|object| object.object_id == receipt.id) {
            note_typed_failure(
                evidence,
                typed_violation(
                    "catalog receipt",
                    format!("acknowledged object {} is not listed", receipt.id),
                ),
            );
        }
        let members = index
            .list_native_object_files(&receipt.id)
            .map_err(|e| typed_violation("catalog receipt", e.to_string()))?;
        if members.first().map(|member| member.size_bytes) != Some(receipt.payload.len() as u64) {
            note_typed_failure(
                evidence,
                typed_violation(
                    "catalog receipt",
                    format!(
                        "acknowledged object {} has missing or wrong-sized member",
                        receipt.id
                    ),
                ),
            );
        }
    }
    drop(index);
    for attempt in 0..2 {
        evidence["step"] = "writer open".into();
        let (tx, opened, thread) =
            open_crash_typed_writer(&paths, &catalog, open_model_library(world.clone())).await;
        let refusal = opened
            .as_ref()
            .err()
            .map(|status| typed_record_refusal("writer open", status, cut.receipts.is_empty()));
        record_typed_writer_open(
            &catalog,
            &runtime,
            evidence,
            refusal.as_ref(),
            "writer open",
            &initial_ids,
        );
        match opened {
            Err(_) => {
                thread.join().expect("typed recovery writer never panics");
                let refusal = refusal.unwrap();
                let needs_ack = refusal["category"] == "uncommitted tape tail";
                note_typed_failure(evidence, refusal);
                if needs_ack && attempt == 0 {
                    evidence["step"] = "supersede".into();
                    let mut recovery = serde_json::Value::Null;
                    let result = acknowledge_typed_tail(
                        &paths,
                        &catalog,
                        world.clone(),
                        &cut.receipts,
                        &mut recovery,
                    );
                    // All checked fences were recorded before the ordinary supersede.
                    for check in recovery["fence_checks"].as_array().into_iter().flatten() {
                        if let Some(row) = evidence["fences"]
                            .as_array_mut()
                            .unwrap()
                            .iter_mut()
                            .find(|row| row["quarantine_id"] == check["quarantine_id"])
                        {
                            if runtime
                                .fence_for_quarantine_id(row["quarantine_id"].as_str().unwrap())
                                .is_none()
                            {
                                row["released"] = true.into();
                                row["lift_method"] = "supersede".into();
                                row["lift_path"] = "ordinary".into();
                                row["supersede"] = recovery.clone();
                            }
                        }
                    }
                    evidence["tail_recoveries"]
                        .as_array_mut()
                        .unwrap()
                        .push(recovery);
                    match result {
                        Ok(()) => evidence["tail_acknowledged"] = true.into(),
                        Err(failure) => {
                            note_typed_failure(evidence, failure);
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
            Ok(id) => {
                exercise_typed_admitted_writer(
                    &catalog,
                    &world,
                    temp.path(),
                    typed_protected_prefix(&world, summary.as_ref().map_or(2, |s| s.cursor.lba)),
                    (tx, id, thread),
                    evidence,
                )
                .await?;
                break;
            }
        }
    }
    evidence["step"] = "fence release".into();
    let index = catalog.write_handle();
    for fence in index
        .list_active_tape_io_fences()
        .map_err(|e| typed_violation("list fences", e.to_string()))?
    {
        if !fences
            .iter()
            .any(|old| old.quarantine_id == fence.quarantine_id)
        {
            fences.push(fence);
        }
    }
    drop(index);
    // Resolve every remaining source before one supersede can release several fences.
    let fences: Vec<_> = fences
        .into_iter()
        .map(|fence| {
            let position = record_typed_fence(&runtime, evidence, &fence, &initial_ids);
            (fence, position)
        })
        .collect();
    for (fence, position) in fences {
        let mut row = evidence["fences"][position].clone();
        if row["released"] == true {
            continue;
        }
        let suffix = row["cause"] == "SessionSuffixContradiction"
            || row["cause"] == "AppendAdmissionRefused";
        if suffix {
            let opens = evidence["writer_opens"].as_array().unwrap();
            let under_fence = |open: &&serde_json::Value| {
                open["active_fence_ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|id| id == &fence.quarantine_id)
            };
            row["writer_refused_while_active"] = opens
                .iter()
                .filter(under_fence)
                .any(|o| o["admitted"] == false)
                .into();
            let admitted = opens
                .iter()
                .filter(under_fence)
                .any(|o| o["admitted"] == true);
            row["writer_admitted_while_active"] = admitted.into();
            if admitted {
                note_typed_failure(
                    evidence,
                    typed_violation(
                        "writer open",
                        format!(
                            "writer admitted under active suffix fence {}",
                            fence.quarantine_id
                        ),
                    ),
                );
            }
            if runtime
                .fence_for_quarantine_id(&fence.quarantine_id)
                .is_some()
            {
                evidence["step"] = "suffix supersede".into();
                let mut recovery = serde_json::Value::Null;
                let result = acknowledge_typed_tail(
                    &paths,
                    &catalog,
                    world.clone(),
                    &cut.receipts,
                    &mut recovery,
                );
                let confirmation_failed = result.is_err() || recovery["confirmed"] != true;
                row["supersede"] = recovery;
                if let Err(failure) = result {
                    row["release_error"] = failure.clone();
                    note_typed_failure(evidence, failure);
                }
                row["released"] = runtime
                    .fence_for_quarantine_id(&fence.quarantine_id)
                    .is_none()
                    .into();
                if confirmation_failed || row["released"] != true {
                    note_typed_failure(evidence, typed_violation("suffix supersede", format!(
                        "suffix confirmation failed or fence {} is still active: confirmed={}, released={}",
                        fence.quarantine_id, row["supersede"]["confirmed"], row["released"])));
                }
                if row["released"] == true {
                    row["lift_method"] = "supersede".into();
                    row["lift_path"] = "suffix".into();
                    // Retain the completed lift even if a later writer retry fails.
                    evidence["fences"][position] = row.clone();
                    evidence["tail_acknowledged"] = true.into();
                    let protected_prefix = typed_protected_prefix(
                        &world,
                        summary.as_ref().map_or(2, |s| s.cursor.lba),
                    );
                    evidence["step"] = "writer retry after suffix supersede".into();
                    let (tx, opened, thread) = open_crash_typed_writer(
                        &paths,
                        &catalog,
                        open_model_library(world.clone()),
                    )
                    .await;
                    let retry_refusal = opened.as_ref().err().map(|status| {
                        typed_record_refusal(
                            "writer retry after suffix supersede",
                            status,
                            cut.receipts.is_empty(),
                        )
                    });
                    record_typed_writer_open(
                        &catalog,
                        &runtime,
                        evidence,
                        retry_refusal.as_ref(),
                        "writer retry after suffix supersede",
                        &initial_ids,
                    );
                    row["writer_retry_admitted"] = opened.is_ok().into();
                    match opened {
                        Err(status) => {
                            thread.join().expect("typed retry writer never panics");
                            note_typed_failure(
                                evidence,
                                typed_violation(
                                    "writer retry after suffix supersede",
                                    format!("writer retry refused: {status}"),
                                ),
                            );
                            note_typed_failure(
                                evidence,
                                typed_record_refusal(
                                    "writer retry after suffix supersede",
                                    &status,
                                    cut.receipts.is_empty(),
                                ),
                            );
                        }
                        Ok(id) => {
                            exercise_typed_admitted_writer(
                                &catalog,
                                &world,
                                temp.path(),
                                protected_prefix,
                                (tx, id, thread),
                                evidence,
                            )
                            .await?;
                        }
                    }
                }
            } else {
                // A previous confirmed recovery released this fence in the same operator action.
                let previous = evidence["fences"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|old| old["lift_method"] == "supersede" && old["released"] == true);
                if let Some(previous) = previous {
                    row["released"] = true.into();
                    row["lift_method"] = "supersede".into();
                    row["lift_path"] = previous["lift_path"].clone();
                    row["supersede"] = previous["supersede"].clone();
                    row["released_with"] = previous["quarantine_id"].clone();
                } else {
                    let failure = typed_violation(
                        "suffix supersede",
                        format!(
                            "fence {} disappeared without an observed supersede",
                            fence.quarantine_id
                        ),
                    );
                    row["release_error"] = failure.clone();
                    note_typed_failure(evidence, failure);
                }
            }
        } else {
            evidence["step"] = "pre-lift read admission".into();
            match check_typed_fence_before_lift(&catalog.write_handle(), &fence) {
                Ok(check) => row["pre_lift"] = check,
                Err(failure) => {
                    row["pre_lift"] = failure.clone();
                    row["release_error"] = failure.clone();
                    note_typed_failure(evidence, failure);
                    evidence["fences"][position] = row;
                    continue;
                }
            }
            evidence["step"] = "fence release".into();
            match runtime.release_tape_fence(
                &fence.quarantine_id,
                "crash census: inspect acknowledged records after recovery",
                false,
                AuditActor::System,
            ) {
                Ok(released) => {
                    row["released"] = released.is_some().into();
                    if released.is_some() {
                        row["lift_method"] = "RPC".into();
                        row["lift_path"] = "RPC".into();
                    }
                }
                Err(error) => {
                    let refusal =
                        typed_runtime_refusal("fence release", error, cut.receipts.is_empty());
                    row["release_error"] = refusal.clone();
                    note_typed_failure(evidence, refusal);
                }
            }
        }
        evidence["fences"][position] = row;
    }
    evidence["step"] = "read open".into();
    let read = read_typed_receipts(&paths, &catalog, world, &cut.receipts).await;
    evidence["receipts_read"] = read.is_ok().into();
    for fence in evidence["fences"].as_array_mut().unwrap() {
        fence["receipts_read"] = read.is_ok().into();
        fence["read_attempted"] = (!cut.receipts.is_empty()).into();
    }
    read?;
    Ok(())
}

/// Execute the same failed-sync choice as the legacy runner, retaining a complete observation.
async fn observe_typed_case(
    source: &Path,
    cuts: &[Cut],
    case: &CrashCase,
    evidence: &mut serde_json::Value,
) {
    let cut = &cuts[case.cut];
    let failed_cut;
    let cut = if case.fail_sync {
        use remanence_library::durable_file::DurableDirectory;
        let path = cut.before_sync.as_ref().unwrap();
        let failed = MemoryDirectory::from_snapshot(cut.files.clone());
        failed.fail_next_sync(path).unwrap();
        let mut file = failed.open(path).unwrap();
        assert!(file.sync().is_err());
        file.sync().unwrap();
        failed_cut = Cut {
            files: failed.snapshot(),
            ..cut.clone()
        };
        &failed_cut
    } else {
        cut
    };
    if let Err(failure) = check_typed_crash(source, cut, case, evidence).await {
        note_typed_failure(evidence, failure);
    }
}

/// Case identity includes every crash choice and the affected file family, not just its cut.
fn typed_case_evidence(
    source: &Path,
    cuts: &[Cut],
    case: &CrashCase,
    index: usize,
) -> serde_json::Value {
    let cut = &cuts[case.cut];
    let file_kind = |path: &Path| {
        let relative = path.strip_prefix(source).unwrap();
        if relative.starts_with("library") {
            "library log"
        } else if relative.starts_with("native/tapes") {
            "tape log"
        } else {
            "other"
        }
    };
    let tails: Vec<_> = case.tails.iter().map(|(path, bytes)| serde_json::json!({
        "file": path.strip_prefix(source).unwrap(), "file_kind": file_kind(path), "bytes": bytes,
    })).collect();
    serde_json::json!({
        "case_index": index, "cut_index": case.cut, "cut_number": cut.number, "boundary": cut.boundary,
        "kind": if case.fail_sync { "fail_sync" } else if !tails.is_empty() { "torn_tail" } else { "frontier" },
        "fail_sync": case.fail_sync, "frontier": cut.world.tapes[BARCODE].durable_frontier,
        "keep_records": case.keep, "written_records": cut.records.len(), "keep_entries": case.keep_entries,
        "sync_file": cut.before_sync.as_ref().map(|p| p.strip_prefix(source).unwrap()),
        "sync_file_kind": cut.before_sync.as_deref().map(file_kind), "tails": tails,
        "receipt_count": cut.receipts.len(), "runtime_reopened": false, "writer_reopened": false,
        "tail_acknowledged": false, "receipts_read": null, "writer_opens": [], "tail_recoveries": [],
        "step": "failed sync", "refusals": [], "fences": [], "violations": [],
    })
}

/// Classify observed results only; this deliberately defines no accepted outcome set.
fn finish_typed_case(evidence: &mut serde_json::Value) {
    let outcome = if !evidence["violations"].as_array().unwrap().is_empty() {
        "property_violation"
    } else if !evidence["fences"].as_array().unwrap().is_empty() {
        "fence"
    } else if evidence["writer_reopened"] == true && evidence["receipts_read"] == true {
        "reopened"
    } else {
        "refused"
    };
    evidence["outcome"] = outcome.into();
}

// Both matrices permit only the refusal categories observed by their censuses.
const TYPED_REFUSAL_CATEGORIES: &[&str] = &["missing bootstrap", "uncommitted tape tail"];

/// Both matrices must observe the same production supersede protocol.
fn decided_typed_fences(_full: bool) -> BTreeSet<(String, String, String)> {
    BTreeSet::from([(
        "AppendAdmissionRefused".into(),
        "append_admission_refused".into(),
        "supersede".into(),
    )])
}

/// Enforce typed refusal/violation rules; return evidence for focused assertions, never census pins.
async fn run_typed_case(
    source: &Path,
    cuts: &[Cut],
    case: &CrashCase,
) -> Result<(Coverage, serde_json::Value), String> {
    let mut evidence = typed_case_evidence(source, cuts, case, 0);
    observe_typed_case(source, cuts, case, &mut evidence).await;
    finish_typed_case(&mut evidence);
    let cut = &cuts[case.cut];
    let context = format!(
        "C{:03} {}; keep={}; entries={}; tails={:?}",
        cut.number, cut.boundary, case.keep, case.keep_entries, case.tails
    );
    if let Some(failure) = evidence["violations"].as_array().unwrap().first() {
        return Err(format!(
            "{context}: {}: {}",
            failure["step"], failure["message"]
        ));
    }
    let mut coverage = Coverage {
        crashes: 1,
        failed_syncs: usize::from(case.fail_sync),
        torn_tails: usize::from(!case.tails.is_empty()),
        completed: usize::from(evidence["writer_reopened"] == true),
        acknowledged_tails: usize::from(evidence["tail_acknowledged"] == true),
        ..Coverage::default()
    };
    for refusal in evidence["refusals"].as_array().unwrap() {
        if refusal["step"] != "writer open" {
            return Err(format!(
                "{context}: unexpected refusal step {}: {}",
                refusal["step"], refusal["full_message"]
            ));
        }
        if !TYPED_REFUSAL_CATEGORIES.contains(&refusal["category"].as_str().unwrap()) {
            return Err(format!(
                "{context}: unexpected refusal category {} at {}",
                refusal["category"], refusal["step"]
            ));
        }
        let Some(code) = refusal["status_number"].as_i64() else {
            return Err(format!(
                "{context}: {}: {}",
                refusal["step"], refusal["full_message"]
            ));
        };
        // Call the unchanged classifier for its original status assertions and accepted set.
        let status = Status::new(
            tonic::Code::from_i32(code as i32),
            refusal["full_message"].as_str().unwrap(),
        );
        record_refusal(&mut coverage, &status, cut, case.keep)?;
    }
    for fence in evidence["fences"].as_array().unwrap() {
        if fence["released"] != true {
            return Err(format!(
                "{context}: fence {}: {}",
                fence["cause"], fence["release_error"]
            ));
        }
        coverage.released_fences += 1;
        coverage.lifted_fences.insert((
            fence["cause"]
                .as_str()
                .ok_or_else(|| format!("{context}: lifted fence has no typed cause"))?
                .into(),
            fence["reason"].as_str().unwrap().into(),
            fence["lift_method"].as_str().unwrap().into(),
        ));
    }
    Ok((coverage, evidence))
}

/// Census both matrices through the property engine, retaining every case outcome.
async fn census_typed_crashes(full: bool) {
    let (_typed, root, cuts) = trace_typed_session(false).await;
    assert!(cuts.iter().any(|cut| cut.boundary.starts_with("Append")));
    for directory in ["library", "native/tapes"] {
        assert!(
            cuts.iter().any(|cut| cut
                .before_sync
                .as_ref()
                .is_some_and(|path| path.starts_with(root.join(directory)))),
            "BeforeSync on {directory}"
        );
    }
    assert_eq!(cuts.last().unwrap().receipts.len(), 3);
    let cases = crash_cases(&cuts, full);
    let worker_count = if full { 1 } else { 4 };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut observations = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..worker_count)
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
                        let mut evidence = typed_case_evidence(&root, &cuts, case, index);
                        // A case panic is itself a finding; it must not erase later cases from the census.
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            runtime.block_on(observe_typed_case(&root, &cuts, case, &mut evidence));
                        }));
                        if let Err(panic) = result {
                            let message = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                                .unwrap_or_else(|| "non-string panic".into());
                            let step = evidence["step"].as_str().unwrap().to_owned();
                            note_typed_failure(
                                &mut evidence,
                                typed_violation(&step, format!("panic: {message}")),
                            );
                        }
                        finish_typed_case(&mut evidence);
                        results.push(evidence);
                    }
                    results
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().expect("census worker never panics"))
            .collect::<Vec<_>>()
    });
    observations.sort_by_key(|row| row["case_index"].as_u64().unwrap());
    assert_eq!(
        observations.len(),
        cases.len(),
        "every case has an observation"
    );
    let mut summary = BTreeMap::<(String, String), usize>::new();
    for row in &observations {
        *summary
            .entry((
                row["outcome"].as_str().unwrap().into(),
                row["kind"].as_str().unwrap().into(),
            ))
            .or_default() += 1;
    }
    let report_dir = std::env::var_os("REM_CRASH_CENSUS_DIR").map_or_else(
        || {
            tempfile::Builder::new()
                .prefix("remanence-crash-census-")
                .tempdir()
                .unwrap()
                .keep()
        },
        PathBuf::from,
    );
    std::fs::create_dir_all(&report_dir).unwrap();
    let census = serde_json::json!({
        "schema": 2, "default_matrix": !full,
        "cut_count": cuts.len(), "case_count": cases.len(),
        "status_note": "RPC codes are captured unchanged; RuntimeError has no RPC status and is null.",
        "seed_note": "Every file, including SQLite, is seeded; materialize overlays modelled log writes.",
        "cases": observations,
    });
    std::fs::write(
        report_dir.join("census.json"),
        serde_json::to_vec_pretty(&census).unwrap(),
    )
    .unwrap();
    let mut table = format!("Census: {} cuts, {} cases, three receipts; {worker_count} workers; full={full}.\n\n| Outcome | Case kind | Count |\n|---|---|---:|\n", cuts.len(), cases.len());
    for ((outcome, kind), count) in summary {
        table.push_str(&format!("| {outcome} | {kind} | {count} |\n"));
    }
    std::fs::write(report_dir.join("summary.md"), &table).unwrap();
    eprintln!(
        "{table}census written to {}",
        report_dir.join("census.json").display()
    );
}

#[tokio::test]
#[ignore = "default crash census; set REM_CRASH_CENSUS_DIR or inspect the printed temporary directory"]
async fn typed_crash_census() {
    census_typed_crashes(false).await;
}

#[tokio::test]
#[ignore = "full crash census; set REM_CRASH_CENSUS_DIR or inspect the printed temporary directory"]
async fn typed_crash_full_census() {
    census_typed_crashes(true).await;
}

async fn property(skip_barrier: bool, full: bool) -> Result<Coverage, String> {
    let (_typed, root, cuts) = trace_typed_session(skip_barrier).await;
    assert!(cuts.iter().any(|cut| cut.boundary.starts_with("Append")));
    assert!(cuts
        .iter()
        .any(|cut| cut.boundary.starts_with("BeforeSync")));
    assert!(cuts.iter().any(|cut| cut.receipts.len() == 3));
    let cases = crash_cases(&cuts, full);
    // Check the immutable receipt/frontier property across every cut before
    // recovery refusals can obscure a lost acknowledged block in another cut.
    for case in &cases {
        let cut = &cuts[case.cut];
        let kept = &cut.records[..case.keep];
        for receipt in &cut.receipts {
            if kept.get(receipt.start..receipt.start + receipt.records.len())
                != Some(receipt.records.as_slice())
            {
                return Err(format!(
                    "{}: acknowledged object {} loses tape bytes at keep={}",
                    cut.boundary, receipt.id, case.keep
                ));
            }
        }
    }
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
            coverage.merge(run_typed_case(&root, &cuts, case).await?.0);
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
                                runtime
                                    .block_on(run_typed_case(&root, &cuts, case))
                                    .map(|(coverage, _)| coverage),
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
    let refused: BTreeSet<_> = coverage.refused.keys().map(String::as_str).collect();
    let allowed = TYPED_REFUSAL_CATEGORIES.iter().copied().collect();
    assert!(
        refused.is_subset(&allowed),
        "unexpected refusal categories: {refused:?}"
    );
    assert_eq!(
        coverage.lifted_fences,
        decided_typed_fences(full),
        "lifted fence set differs from the decided matrix set"
    );
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
        BTreeSet::from(["missing bootstrap"]),
        "existing refusal identity defects change; review the findings"
    );
    Ok(coverage)
}

/// Exercise loss of the initial Bootstrap delimiter at a WRITE(6)/WRITE FILEMARKS boundary.
/// Select by physical shape and crash choices, independently of cut numbers and case indices.
#[tokio::test]
async fn typed_crash_operator_recovers_bootstrap_suffix() {
    let start = std::time::Instant::now();
    let (_typed, root, cuts) = trace_typed_session(false).await;
    let cases = crash_cases(&cuts, true);
    let selected: Vec<_> = cases
        .iter()
        .filter(|case| {
            let cut = &cuts[case.cut];
            case.keep == 1
                && !case.keep_entries
                && case.tails.is_empty()
                && !case.fail_sync
                && cut.receipts.is_empty()
                && cut.world.tapes[BARCODE].durable_frontier == 0
                && (cut.boundary.starts_with("command [0a,")
                    || cut.boundary.starts_with("command [10,"))
                && matches!(
                    cut.records.as_slice(),
                    [Record::Block(_)] | [Record::Block(_), Record::Filemark]
                )
                && matches!(&cut.records[0], Record::Block(block)
                if remanence_parity::bootstrap::parse_bootstrap_block(block).is_ok())
        })
        .collect();
    assert!(
        !selected.is_empty(),
        "Bootstrap delimiter crash shape must remain covered"
    );
    for case in &selected {
        let (coverage, evidence) = run_typed_case(&root, &cuts, case)
            .await
            .expect("operator recovers the Bootstrap suffix fence");
        assert_eq!(coverage.released_fences, 1, "{evidence}");
        assert_eq!(
            coverage.lifted_fences,
            decided_typed_fences(true),
            "{evidence}"
        );
        let fences = evidence["fences"].as_array().unwrap();
        assert_eq!(fences.len(), 1, "{evidence}");
        let fence = &fences[0];
        assert_eq!(fence["writer_refused_while_active"], true, "{evidence}");
        assert_eq!(fence["writer_admitted_while_active"], false, "{evidence}");
        assert_eq!(
            fence["supersede"]["preview"]["required_confirmation"],
            format!("SUPERSEDE {BARCODE}"),
            "{evidence}"
        );
        assert_eq!(
            fence["supersede"]["preview"]["acknowledged_record_found"], false,
            "{evidence}"
        );
        assert_eq!(fence["supersede"]["confirmed"], true, "{evidence}");
        assert_eq!(fence["writer_retry_admitted"], true, "{evidence}");
        eprintln!(
            "operator case C{:03} {}; keep={}",
            cuts[case.cut].number, cuts[case.cut].boundary, case.keep
        );
    }
    eprintln!(
        "focused operator coverage: {} cases; elapsed {:?}",
        selected.len(),
        start.elapsed()
    );
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

#[path = "b3b_admission_caller_tests.rs"]
mod b3b_admission_caller_tests;
#[path = "shutdown_round1_tests.rs"]
mod shutdown_round1_tests;
