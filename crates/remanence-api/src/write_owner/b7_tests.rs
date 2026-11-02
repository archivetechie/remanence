//! Obligation regressions using real native commits and model-only drive I/O.
use super::*;
use crate::closeout_obligation::{scan, summary};

/// Commit a short epoch, then exercise the B6c cutoff to leave durable debt.
async fn due_tape(root: &Path) -> Arc<Mutex<VirtualWorld>> {
    let world = setup(root);
    let lifecycle = DrivePoolLifecycle::default();
    let (tx, opened, thread) = open_writer_configured(
        root,
        open_model_library(world.clone()),
        selected(),
        |cfg, _| cfg.lifecycle = Some(lifecycle.clone()),
    )
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
    let state = state(root);
    scan(&state).unwrap();
    assert!(
        state.closeout_work.contains((TAPE, 1)),
        "live owner debt is observable without stealing its lease"
    );
    assert!(summary(&state, TAPE).unwrap().open_session.is_some());
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
    assert!(summary(&state, TAPE).unwrap().closeout_due());
    world
}
fn state(root: &Path) -> crate::ApiState {
    let mut s = crate::ApiState::new_with_pool_configs(
        CatalogIndex::open(root.join("state.sqlite")).unwrap(),
        vec![pool()],
    );
    s.checkpoint_journal_dir = Arc::new(root.join("journals/tapes"));
    s.audit_dir = Arc::new(root.join("audit"));
    s.drive_idle_unload_seconds = 0;
    s
}

#[tokio::test]
async fn b7_restart_replays_deadline_and_crash_debt_without_ending_or_mounting() {
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    let before = world.lock().unwrap().command_log.len();
    let state = state(root.path());
    scan(&state).unwrap();
    scan(&state).unwrap();
    assert!(state.closeout_work.contains((TAPE, 1)));
    let s = summary(&state, TAPE).unwrap();
    assert_eq!(
        s.last_session_end.unwrap().reason,
        remanence_state::tape_log::SessionEndReason::GracefulShutdown
    );
    // Remove the last metadata group to recreate the crash cut after Committed.
    let path = remanence_state::TapeAuthority::list_entries(&*state.checkpoint_journal_dir)
        .unwrap()
        .pop()
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut cursor = std::io::Cursor::new(&bytes);
    remanence_state::tape_log::store::Header::decode(&mut cursor, TAPE).unwrap();
    let mut decoder = remanence_state::tape_log::codec::Decoder::at(
        &bytes[cursor.position() as usize..],
        1,
        cursor.position(),
    );
    let mut end = None;
    while let Some(frame) = decoder.next_frame().unwrap() {
        if matches!(
            frame.record,
            remanence_state::tape_log::Record::SessionEnded(_)
        ) {
            end = Some(frame.offset);
        }
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(end.unwrap())
        .unwrap();
    let restarted = state_for_crash(root.path());
    scan(&restarted).unwrap();
    assert!(restarted.closeout_work.contains((TAPE, 1)));
    assert!(
        summary(&restarted, TAPE).unwrap().open_session.is_some(),
        "scan cannot synthesize crash endings"
    );
    assert_eq!(world.lock().unwrap().command_log.len(), before);
}
fn state_for_crash(root: &Path) -> crate::ApiState {
    state(root)
}

#[tokio::test]
async fn b7_out_of_scope_debt_is_visible_and_never_worked() {
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    for foreign_library in [true, false] {
        let mut state = state(root.path());
        let library = open_model_library(world.clone());
        state.library_snapshot = Some(library_snapshot_cell(library.library().clone()));
        let (changer, mut changer_rx) = mpsc::channel(2);
        let (drive, mut drive_rx) = mpsc::channel(2);
        let serial = if foreign_library {
            "UNCONFIGURED"
        } else {
            LIBRARY
        };
        let bay = if foreign_library { BAY } else { BAY + 1 };
        let pool = DrivePool::new_for_library(
            serial,
            changer,
            HashMap::from([(bay, drive)]),
            Arc::new(HashMap::from([(bay, AtomicBool::new(false))])),
        );
        state.drive_pool = Some(pool);
        let before = summary(&state, TAPE).unwrap().last_closed_sequence;
        scan(&state).unwrap();
        let mut live = pb::GetLiveStatusResponse::default();
        state.refresh_closeout_surfaces(&mut live).unwrap();
        assert_eq!(live.closeout_debt.len(), 1);
        assert_eq!(live.closeout_debt[0].tape_uuid, TAPE);
        assert!(live.closeout_debt[0].closeout_due);
        crate::mount::service_closeout_obligation(&state, TAPE, 1)
            .await
            .unwrap_err();
        assert_eq!(summary(&state, TAPE).unwrap().last_closed_sequence, before);
        assert!(drive_rx.try_recv().is_err());
        assert!(changer_rx.try_recv().is_err());
        assert!(state.closeout_work.contains((TAPE, 1)));
    }
}

#[tokio::test]
async fn b7_projection_failure_preserves_startup_queue_and_read_errors() {
    let root = tempfile::tempdir().unwrap();
    due_tape(root.path()).await;
    let state = state(root.path());
    let db = rusqlite::Connection::open(&*state.index_path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_b7_projection BEFORE UPDATE ON tapes BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END;").unwrap();
    crate::startup_checkpoint::replay_checkpoint_journal_projections_with_audit(
        &mut state.index_write().unwrap(),
        &state.checkpoint_journal_dir,
        &state.audit_dir,
        &state.audit_append_lock,
    )
    .unwrap();
    scan(&state).unwrap();
    assert!(state.closeout_work.contains((TAPE, 1)));
    let tape = state.index().unwrap().get_tape(&TAPE).unwrap().unwrap();
    let pb = state
        .tape_with_debt(crate::catalog_conversion::tape_to_proto(tape))
        .unwrap();
    assert!(pb.unprotected_bytes > 0);
    assert!(pb.unprotected_since.is_some());
    let path = TapeAuthority::list_entries(&*state.checkpoint_journal_dir)
        .unwrap()
        .pop()
        .unwrap();
    std::fs::write(path, b"broken authority").unwrap();
    scan(&state).unwrap();
    assert!(state.closeout_work.contains((TAPE, 1)));
    assert!(state
        .refresh_closeout_surfaces(&mut pb::GetLiveStatusResponse::default())
        .is_err());
}

#[tokio::test]
async fn b7_selection_prefers_debt_but_keeps_pinned_identity_and_capacity() {
    let root = tempfile::tempdir().unwrap();
    due_tape(root.path()).await;
    let state = state(root.path());
    let mut index = state.index_write().unwrap();
    let other = [0x11; 16];
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: other,
            voltag: "AAA001L9".into(),
            block_size: BLOCK,
            parity: ParityConfig::Scheme(scheme()),
            force: false,
        })
        .unwrap();
    index.project_tape_pool_membership(other, POOL).unwrap();
    let picked = crate::pool_write::select_tape_in_pool_for_write_session(
        &index,
        &pool(),
        1,
        &HashSet::new(),
        &state.checkpoint_journal_dir,
    )
    .unwrap();
    assert_eq!(picked.tape_uuid, TAPE);
    let pinned = crate::pool_write::admit_pinned_tape_for_write_session(
        &index,
        other,
        POOL,
        &pool(),
        &state.checkpoint_journal_dir,
    )
    .unwrap();
    assert!(
        matches!(pinned, crate::pool_write::PinnedWriteDisposition::Writable(t) if t.tape_uuid == other)
    );
    let db = rusqlite::Connection::open(&*state.index_path).unwrap();
    // Leave only an Object-sized gap; the due sidecar must disqualify this tape.
    db.execute(
        "UPDATE tapes SET written_extent_lba = ? WHERE tape_uuid = ?",
        rusqlite::params![
            (18_000_000_000_000u64 / u64::from(BLOCK) - 1)
                .to_be_bytes()
                .as_slice(),
            TAPE.as_slice()
        ],
    )
    .unwrap();
    let picked = crate::pool_write::select_tape_in_pool_for_write_session(
        &index,
        &pool(),
        1,
        &HashSet::new(),
        &state.checkpoint_journal_dir,
    )
    .unwrap();
    assert_eq!(picked.tape_uuid, other);
}

#[tokio::test]
async fn b7_worker_waits_for_owner_retries_dispatch_and_merges_seal_debt() {
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    let mut state = state(root.path());
    let library = open_model_library(world);
    state.library_snapshot = Some(library_snapshot_cell(library.library().clone()));
    let (changer, mut changer_rx) = mpsc::channel(2);
    let (drive, mut drive_rx) = mpsc::channel(2);
    let pool = DrivePool::new_for_library(
        LIBRARY,
        changer,
        HashMap::from([(BAY, drive)]),
        Arc::new(HashMap::from([(BAY, AtomicBool::new(false))])),
    );
    state.drive_pool = Some(pool.clone());
    let owner = pool.reserve_tape(TAPE).unwrap();
    scan(&state).unwrap();
    crate::mount::service_closeout_obligation(&state, TAPE, 1)
        .await
        .unwrap_err();
    assert!(drive_rx.try_recv().is_err());
    drop(owner);
    let work = tokio::spawn({
        let state = state.clone();
        async move { crate::mount::service_closeout_obligation(&state, TAPE, 1).await }
    });
    let command = tokio::time::timeout(StdDuration::from_secs(5), drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match command {
        DriveCommand::CloseoutTape { reply, .. } => {
            reply
                .send(Err(Status::unavailable("injected unavailable drive")))
                .unwrap();
        }
        _ => panic!("closeout command expected"),
    }
    work.await.unwrap().unwrap_err();
    scan(&state).unwrap();
    assert!(state.closeout_work.contains((TAPE, 1)));
    // A subsequent barrier records a seal trigger after a settled drive error.
    // Startup must merge both predicates into the existing finalization route.
    let sealed_root = tempfile::tempdir().unwrap();
    setup(sealed_root.path());
    let directory = sealed_root.path().join("journals/tapes");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::create_dir_all(sealed_root.path().join("audit")).unwrap();
    let path = TapeAuthority::list_entries(&*state.checkpoint_journal_dir)
        .unwrap()
        .pop()
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut cursor = std::io::Cursor::new(&bytes);
    remanence_state::tape_log::store::Header::decode(&mut cursor, TAPE).unwrap();
    let mut decoder = remanence_state::tape_log::codec::Decoder::at(
        &bytes[cursor.position() as usize..],
        1,
        cursor.position(),
    );
    let mut cut = None;
    while let Some(frame) = decoder.next_frame().unwrap() {
        if matches!(
            frame.record,
            remanence_state::tape_log::Record::SessionEnded(_)
        ) {
            cut = Some(frame.offset);
        }
    }
    std::fs::write(
        directory.join(path.file_name().unwrap()),
        &bytes[..cut.unwrap() as usize],
    )
    .unwrap();
    state.index_path = Arc::new(sealed_root.path().join("state.sqlite"));
    state.checkpoint_journal_dir = Arc::new(directory);
    state.audit_dir = Arc::new(sealed_root.path().join("audit"));
    let authority =
        TapeAuthority::open(&*state.checkpoint_journal_dir, TAPE, &state.index_path).unwrap();
    let mut lease = authority.acquire_exclusive().unwrap();
    let session = lease
        .freeze()
        .unwrap()
        .tape_summary()
        .open_session
        .as_ref()
        .unwrap()
        .session_id;
    let mut commit = lease
        .freeze()
        .unwrap()
        .tape_summary()
        .last_ordinary_record
        .clone()
        .unwrap();
    commit.session_id = Some(session);
    commit.seal_due =
        Some(remanence_parity::authority::TerminalFinalizationTrigger::HardwareEarlyWarning);
    lease
        .with_store(|core| {
            let group = core.prepare()?;
            commit.group = group.reference(0);
            core.append(group.close(&remanence_state::tape_log::Record::Committed(commit))?)?;
            Ok(())
        })
        .unwrap();
    lease
        .end_session(
            session,
            "2026-11-02T00:00:01Z".into(),
            remanence_state::tape_log::SessionEndReason::DriveError,
            None,
            std::iter::empty(),
        )
        .unwrap();
    drop(lease);
    scan(&state).unwrap();
    assert!(summary(&state, TAPE).unwrap().closeout_due());
    assert!(
        crate::pool_write::select_tape_in_pool_for_write_session(
            &state.index().unwrap(),
            &super::pool(),
            1,
            &HashSet::new(),
            &state.checkpoint_journal_dir
        )
        .is_err(),
        "seal-due tape cannot receive another Object"
    );
    let work = tokio::spawn({
        let state = state.clone();
        async move { crate::mount::service_closeout_obligation(&state, TAPE, 1).await }
    });
    let command = tokio::time::timeout(StdDuration::from_secs(5), drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    match command {
        DriveCommand::OpenWrite { reply, .. } => {
            reply
                .send(Err(Status::unavailable("injected terminal drive refusal")))
                .unwrap();
        }
        _ => panic!("one merged automatic finalization dispatch expected"),
    }
    work.await.unwrap().unwrap_err();
    assert!(drive_rx.try_recv().is_err());
    assert!(changer_rx.try_recv().is_err());
    assert!(state.closeout_work.contains((TAPE, 1)));
}
