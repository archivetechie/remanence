//! B9 review regressions through real actors, admission, and model-only media.
use super::b6b_fix_tests::{native_records, selection};
use super::*;
use remanence_state::tape_log::Record as NativeRecord;

/// Open through the dispatch loop so actor ownership participates in every assertion.
async fn live_actor(root: &Path) -> (crate::ApiState, DrivePool, Uuid) {
    let world = setup(root);
    let mut library = open_model_library(world);
    let lifecycle = DrivePoolLifecycle::default();
    let key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let mut cfg = config(root, &library);
    cfg.lifecycle = Some(lifecycle.clone());
    cfg.reservations = reservations.clone();
    let drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let tx = spawn_drive_actor(BAY, drive, cfg);
    let (changer, _rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(key, tx.clone())]),
        reservations,
        lifecycle,
    );
    let mut state = super::b7_tests::state(root);
    state.library_snapshot = Some(library_snapshot_cell(library.library().clone()));
    state.drive_pool = Some(pool.clone());
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::OpenWrite {
        actor: AuditActor::System,
        pool_cfg: super::pool(),
        selected: selected(),
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPinnedTape,
        needs_drive_load: false,
        library_serial: LIBRARY.into(),
        barcode: Some(BARCODE.into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: Some(DRIVE.into()),
        reply,
    })
    .await
    .unwrap();
    let opened = received.await.unwrap().unwrap();
    let id = Uuid::from_slice(&opened.session_id).unwrap();
    // A Get round trip establishes that open admission has handed off to run().
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Get {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    (state, pool, id)
}

fn mounted() -> MountedSession {
    MountedSession {
        bay: BAY,
        library_serial: LIBRARY.into(),
        barcode: Some(BARCODE.into()),
        home_slot: None,
        tape_uuid: TAPE,
        drive_uuid: None,
    }
}

/// A late registration must reach Checkpoint/Close instead of waiting on the live actor.
#[tokio::test]
async fn b9_fix_shutdown_hands_off_session_after_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let (state, pool, id) = live_actor(root.path()).await;
    assert!(pool.sessions_by_drive().is_empty());
    let shutdown = tokio::spawn(async move { state.shutdown_drive_pool().await });
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while !pool.is_shutting_down() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // On this current-thread runtime shutdown has reached its first reservation wait.
    assert!(!shutdown.is_finished());
    pool.record_session(id, mounted());
    tokio::time::timeout(StdDuration::from_secs(3), shutdown)
        .await
        .expect("late session must close within the shutdown bound")
        .unwrap()
        .unwrap();
    assert!(pool.sessions_by_drive().is_empty());
    assert!(native_records(root.path()).iter().any(|r| matches!(r,
        NativeRecord::SessionEnded(e) if e.reason == remanence_state::tape_log::SessionEndReason::GracefulShutdown)));
}

/// Busy robotics and health calls must reach their immediate live-session refusals.
#[tokio::test]
async fn b9_fix_live_session_refuses_robotics_and_health_promptly() {
    use crate::pb::library_service_server::LibraryService;
    let root = tempfile::tempdir().unwrap();
    let (state, pool, id) = live_actor(root.path()).await;
    pool.record_session(id, mounted());
    let error = tokio::time::timeout(
        StdDuration::from_secs(1),
        state
            .library_service()
            .refresh_inventory(tonic::Request::new(pb::RefreshInventoryRequest {
                library_uuid: crate::library::library_uuid(LIBRARY).to_vec(),
                ..Default::default()
            })),
    )
    .await
    .expect("robotics must refuse while session remains open")
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains("drives are busy"), "{error}");
    let key = DriveKey::new(LIBRARY, BAY);
    let error = tokio::time::timeout(
        StdDuration::from_secs(1),
        pool.poll_drive_health(&key, vec![1; 16]),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error.message(), "write session already active");
    let heartbeat_pool = pool.clone();
    let error =
        tokio::task::spawn_blocking(move || heartbeat_pool.heartbeat_drive(&key, vec![1; 16]))
            .await
            .unwrap()
            .unwrap_err();
    assert_eq!(error.message(), "write session already active");
    state.shutdown_drive_pool().await.unwrap();
}

/// Slow cold replay on one tape cannot hold global admission, including shutdown publication.
#[tokio::test]
async fn b9_fix_cold_preparation_leaves_other_rpc_and_shutdown_admissible() {
    use crate::pb::catalog_server::Catalog;
    let root = tempfile::tempdir().unwrap();
    let world = super::b7_tests::due_tape(root.path()).await;
    let mut state = super::b7_tests::state(root.path());
    state.library_snapshot = Some(library_snapshot_cell(
        world.lock().unwrap().library_snapshot(),
    ));
    let (changer, _changer_rx) = mpsc::channel(1);
    let (drive, mut drive_rx) = mpsc::channel(1);
    let pool = DrivePool::new_for_library(
        LIBRARY,
        changer,
        HashMap::from([(BAY, drive)]),
        Arc::new(HashMap::from([(BAY, AtomicBool::new(false))])),
    );
    state.drive_pool = Some(pool.clone());
    let other = [0xe1; 16];
    state
        .index_write()
        .unwrap()
        .provision_tape(ProvisionTapeInput {
            tape_uuid: other,
            voltag: "OTHER".into(),
            block_size: BLOCK,
            parity: ParityConfig::None,
            force: false,
        })
        .unwrap();
    // A distinct log path has no warm observer cache from fixture construction.
    let cold = root.path().join("cold");
    std::fs::create_dir(&cold).unwrap();
    let name = format!("{}.remtape", Uuid::from_bytes(TAPE).simple());
    std::fs::copy(state.checkpoint_journal_dir.join(&name), cold.join(&name)).unwrap();
    state.checkpoint_journal_dir = Arc::new(cold);
    let authority = TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        TAPE,
        state.index_path.as_ref(),
    )
    .unwrap();
    let (entered, reached) = oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    authority.before_next_observation_replay(move || {
        entered.send(()).unwrap();
        // A watchdog also lets the old global-lock implementation fail without hanging the suite.
        let _ = released.recv_timeout(StdDuration::from_secs(5));
    });
    let observer = tokio::task::spawn_blocking(move || authority.observe_summary());
    tokio::time::timeout(StdDuration::from_secs(2), reached)
        .await
        .unwrap()
        .unwrap();
    let preparing = state.clone();
    let preparation = tokio::spawn(async move {
        crate::mount::service_closeout_obligation(&preparing, TAPE, 1).await
    });
    tokio::time::timeout(StdDuration::from_secs(2), async {
        loop {
            match pool.reserve_tape(TAPE) {
                Ok(probe) => drop(probe),
                Err(_) => break,
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let before = state.index().unwrap().list_operations().unwrap().len();
    let started = std::time::Instant::now();
    let error = state
        .catalog_service()
        .closeout_tape(tonic::Request::new(pb::CloseoutTapeRequest {
            tape_uuid: other.to_vec(),
            expected_pool_id: Some("different-pool".into()),
            idempotency_key: Some(pb::IdempotencyKey {
                value: Uuid::new_v4().as_bytes().to_vec(),
            }),
        }))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().contains("expected_pool_id"));
    assert!(
        started.elapsed() < StdDuration::from_secs(1),
        "another tape's cold replay blocked the RPC"
    );
    state.begin_shutdown();
    release.send(()).unwrap();
    observer.await.unwrap().unwrap();
    let error = preparation.await.unwrap().unwrap_err();
    assert_eq!(
        error.message(),
        "daemon shutdown; maintenance admission closed"
    );
    assert_eq!(
        state.index().unwrap().list_operations().unwrap().len(),
        before
    );
    assert!(drive_rx.try_recv().is_err());
    pool.wait_closeouts().await;
}

/// Reopening an exact clean ending adds only SessionOpened, never a redundant refinement.
#[tokio::test]
async fn b9_fix_clean_reopen_does_not_refine_session_ended() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let selected = selection(root.path(), &world, false);
    let (tx, opened, thread) = open_writer_with_selection(
        root.path(),
        open_model_library(world.clone()),
        selected.clone(),
    )
    .await;
    let id = opened.unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.path().join("committed"),
        "committed",
        "committed",
        b"exact cursor",
    )
    .await;
    close_writer(&tx, id, thread).await;
    // The sequence used by supersede preview must survive the exact-position proof.
    // A later SessionOpened is itself a legitimate sequence change.
    {
        let mut library = open_model_library(world.clone());
        let cfg = config(root.path(), &library);
        let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
        prepare_drive_for_write(
            &mut drive,
            &TAPE,
            BLOCK,
            Uuid::new_v4(),
            WriteMediaPolicy::RewritableObject,
        )
        .unwrap();
        let authority =
            TapeAuthority::open(&cfg.checkpoint_journal_dir, TAPE, &cfg.index_path).unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        let snapshot = lease.freeze().unwrap();
        let preview_sequence = snapshot.log_reference().0;
        let end = snapshot.tape_summary().last_session_end.as_ref().unwrap();
        assert_eq!(end.possible_tail_blocks, end.synchronized_tail_blocks);
        let admission = crate::append_admission::admit_append_boundary(
            &mut drive,
            crate::append_admission::AppendAuthority::Session {
                snapshot: &snapshot,
                closeout: None,
            },
            crate::append_admission::context(TAPE, BLOCK, end.cursor.partition, end.cursor.lba),
        )
        .unwrap();
        if let Some(eod) = admission.ended_tail_eod {
            lease.record_session_tail_eod(eod).unwrap();
        }
        assert_eq!(
            lease.freeze().unwrap().log_reference().0,
            preview_sequence,
            "exact-position admission must not stale the pending preview sequence"
        );
        assert!(admission.ended_tail_eod.is_none());
    }
    let before = native_records(root.path());
    let (tx, opened, thread) =
        open_writer_with_selection(root.path(), open_model_library(world), selected).await;
    let after = native_records(root.path());
    assert_eq!(
        after.len(),
        before.len() + 1,
        "open appends only its new session identity"
    );
    assert!(matches!(after.last(), Some(NativeRecord::SessionOpened(_))));
    assert_eq!(
        after
            .iter()
            .filter(|r| matches!(r, NativeRecord::SessionEnded(_)))
            .count(),
        before
            .iter()
            .filter(|r| matches!(r, NativeRecord::SessionEnded(_)))
            .count()
    );
    close_writer(&tx, opened.unwrap(), thread).await;
}

/// A slow consumer remains valid beyond the removed deadline; shutdown interrupts both sends.
#[test]
fn b9_fix_read_and_inventory_backpressure_wait_only_for_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let library = open_model_library(world);
    let lifecycle = DrivePoolLifecycle::default();
    let mut cfg = config(root.path(), &library);
    cfg.lifecycle = Some(lifecycle.clone());
    let (read_tx, _read_rx) =
        crate::read_core::read_stream_channel(crate::read_core::READ_STREAM_CHANNEL_BYTE_BUDGET);
    read_tx.set_shutdown_signal(Some(lifecycle.shutting_down.clone()));
    read_tx
        .blocking_send(Ok(pb::BytesChunk::default()))
        .unwrap();
    let (inventory_tx, _inventory_rx) = mpsc::channel(1);
    inventory_tx
        .try_send(Ok(pb::TapeInventoryStreamItem::default()))
        .unwrap();
    let (started, starts) = std::sync::mpsc::channel();
    let (done, completions) = std::sync::mpsc::channel();
    let read_done = done.clone();
    let read_started = started.clone();
    let read = std::thread::spawn(move || {
        read_started.send(()).unwrap();
        let failed = read_tx
            .blocking_send(Ok(pb::BytesChunk::default()))
            .is_err();
        read_done.send(("read", failed)).unwrap();
    });
    let inventory = std::thread::spawn(move || {
        started.send(()).unwrap();
        let error = crate::write_owner::terminal_inventory::send_inventory_stream_item(
            &cfg,
            &inventory_tx,
            pb::TapeInventoryStreamItem::default(),
        )
        .unwrap_err();
        done.send((
            "inventory",
            error.code() == tonic::Code::Cancelled
                && error.message() == "terminal inventory interrupted by shutdown",
        ))
        .unwrap();
    });
    for _ in 0..2 {
        starts.recv_timeout(StdDuration::from_secs(2)).unwrap();
    }
    assert!(
        matches!(
            completions.recv_timeout(StdDuration::from_secs(31)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ),
        "normal slow consumer was timed out"
    );
    lifecycle.shutting_down.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        let (kind, interrupted) = completions.recv_timeout(StdDuration::from_secs(2)).unwrap();
        assert!(interrupted, "{kind} send must stop on shutdown");
    }
    read.join().unwrap();
    inventory.join().unwrap();
}
