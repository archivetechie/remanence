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
        assert_eq!(live.closeout_debt[0].closeout_due, Some(true));
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
    assert!(pb.unprotected_bytes.is_some_and(|b| b > 0));
    assert!(pb.unprotected_since.is_some());
    let path = TapeAuthority::list_entries(&*state.checkpoint_journal_dir)
        .unwrap()
        .pop()
        .unwrap();
    std::fs::write(path, b"broken authority").unwrap();
    scan(&state).unwrap();
    assert!(state.closeout_work.contains((TAPE, 1)));
    let mut live = pb::GetLiveStatusResponse::default();
    state.refresh_closeout_surfaces(&mut live).unwrap();
    assert!(live.closeout_debt[0].closeout_authority_error.is_some());
    assert_eq!(live.closeout_debt[0].unprotected_bytes, None);
}

#[tokio::test]
async fn b7_selection_prefers_debt_but_keeps_pinned_identity_and_capacity() {
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    let mut config = pool();
    config.selection_policy = remanence_state::PoolSelectionPolicyName::FillOldest;
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
        &config,
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
        &config,
        &state.checkpoint_journal_dir,
    )
    .unwrap();
    assert!(
        matches!(pinned, crate::pool_write::PinnedWriteDisposition::Writable(t) if t.tape_uuid == other)
    );
    let debt = summary(&state, TAPE).unwrap();
    let scheme = debt.scheme.as_ref().unwrap();
    let encoded = remanence_parity::sidecar::encode_sidecar_tape_file(
        &remanence_parity::sidecar::SidecarDescriptor {
            tape_uuid: TAPE,
            epoch_id: 0,
            k: scheme.data_blocks_per_stripe,
            m: scheme.parity_blocks_per_stripe,
            stripes_per_epoch: scheme.stripes_per_neighborhood,
            block_size: BLOCK,
            protected_ordinal_start: debt.protected,
            protected_ordinal_end_exclusive: debt.total,
        },
        &vec![
            vec![0u8; BLOCK as usize];
            (scheme.parity_blocks_per_stripe as usize)
                * (scheme.stripes_per_neighborhood as usize)
        ],
        vec![0; (debt.total - debt.protected) as usize],
    )
    .unwrap();
    assert_eq!(
        crate::pool_write::closeout_charge(&debt).unwrap(),
        (encoded.blocks.len() as u64 + 1) * u64::from(BLOCK)
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
        &config,
        1,
        &HashSet::new(),
        &state.checkpoint_journal_dir,
    )
    .unwrap();
    assert_eq!(picked.tape_uuid, other);
    db.execute(
        "UPDATE tapes SET written_extent_lba = ? WHERE tape_uuid = ?",
        rusqlite::params![debt.cursor.lba.to_be_bytes().as_slice(), TAPE.as_slice()],
    )
    .unwrap();
    drop(index);
    let (tx, opened, thread) = open_writer(root.path(), open_model_library(world)).await;
    let id = opened.unwrap(); // real admission closes the prior debt
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: None,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().unwrap();
    assert!(!summary(&state, TAPE).unwrap().closeout_due());
    let control = crate::pool_write::select_tape_in_pool_for_write_session(
        &state.index().unwrap(),
        &config,
        1,
        &HashSet::new(),
        &state.checkpoint_journal_dir,
    )
    .unwrap();
    assert_eq!(
        control.tape_uuid, other,
        "without debt, ordinary barcode ranking picks the other tape"
    );
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
    let operations = state.index().unwrap().list_operations().unwrap();
    let closeouts = operations
        .iter()
        .filter(|o| o.operation_kind == "tape_closeout")
        .collect::<Vec<_>>();
    assert!(!closeouts.is_empty());
    assert!(
        closeouts
            .iter()
            .all(|o| o.state == "failed" || o.state == "finished"),
        "every worker operation is terminal: {closeouts:?}"
    );
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
        .unwrap_err()
        .to_string()
        .contains("seal-due or finalizing"),
        "seal-due tape refusal explains the maintenance restriction"
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

/// Pause the actual synchronous media barrier after stage_report has returned.
struct HeldBarrier {
    inner: ModelTransport,
    armed: Arc<AtomicBool>,
    entered: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    release: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}
impl SgTransport for HeldBarrier {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        if cdb.first() == Some(&0x10)
            && cdb.get(2..5) == Some(&[0, 0, 0])
            && self.armed.swap(false, Ordering::SeqCst)
        {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let _ = self.release.lock().unwrap().recv();
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[tokio::test]
async fn b7_fix_catalog_and_live_read_last_commit_during_held_checkpoint_barrier() {
    use pb::{catalog_server::Catalog, library_service_server::LibraryService};
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let armed = Arc::new(AtomicBool::new(false));
    let (entered, arrived) = oneshot::channel();
    let entered = Arc::new(Mutex::new(Some(entered)));
    let (release, wait) = std::sync::mpsc::channel();
    let wait = Arc::new(Mutex::new(wait));
    let model = world.lock().unwrap().library_snapshot();
    let library = model
        .open_with(&remanence_library::StaticAllowlist::new([LIBRARY]), {
            let armed = armed.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(HeldBarrier {
                    inner: ModelTransport::new(world.clone(), role),
                    armed: armed.clone(),
                    entered: entered.clone(),
                    release: wait.clone(),
                }))
            }
        })
        .unwrap();
    let (tx, opened, thread) = open_writer(root.path(), library).await;
    let id = opened.unwrap();
    let mut state = state(root.path());
    state.library_snapshot = Some(library_snapshot_cell(model));
    let before = summary(&state, TAPE).unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.path().join("held"),
        "held",
        "held",
        b"pending",
    )
    .await;
    armed.store(true, Ordering::SeqCst);
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Checkpoint {
        session_id: id,
        trigger: CheckpointTrigger::Explicit,
        expected_batch_id: None,
        reply: Some(reply),
    })
    .await
    .unwrap();
    tokio::time::timeout(StdDuration::from_secs(10), arrived)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        format!("{:?}", summary(&state, TAPE).unwrap()),
        format!("{before:?}")
    );
    let catalog = state.catalog_service();
    let listed = catalog
        .list_tapes(tonic::Request::new(pb::ListTapesRequest::default()))
        .await
        .unwrap()
        .into_inner();
    let tape = catalog
        .get_tape(tonic::Request::new(pb::GetTapeRequest {
            tape_uuid: TAPE.to_vec(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.tapes, vec![tape.clone()]);
    assert_eq!(tape.unprotected_bytes, Some(0));
    assert_eq!(tape.closeout_due, Some(false));
    for _ in 0..2 {
        // fresh and cached paths
        let live = state
            .library_service()
            .get_live_status(tonic::Request::new(pb::GetLiveStatusRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert!(live.closeout_debt.is_empty());
        assert_eq!(live.libraries[0].drives[0].unprotected_bytes, Some(0));
    }
    release.send(()).unwrap();
    received.await.unwrap().unwrap();
    assert!(summary(&state, TAPE).unwrap().total > before.total);
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: None,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().unwrap();
}

#[tokio::test]
async fn b7_fix_partial_catalog_live_and_positive_native_mapping() {
    use pb::{catalog_server::Catalog, library_service_server::LibraryService};
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    let mut state = state(root.path());
    state.library_snapshot = Some(library_snapshot_cell(
        world.lock().unwrap().library_snapshot(),
    ));
    let bad = [0xb9; 16];
    let fresh = [0xba; 16];
    for (uuid, barcode) in [(bad, "BAD001L9"), (fresh, "NEW001L9")] {
        state
            .index_write()
            .unwrap()
            .provision_tape(ProvisionTapeInput {
                tape_uuid: uuid,
                voltag: barcode.into(),
                block_size: BLOCK,
                parity: ParityConfig::Scheme(scheme()),
                force: false,
            })
            .unwrap();
    }
    std::fs::write(
        state
            .checkpoint_journal_dir
            .join(format!("{}.remtape", Uuid::from_bytes(bad).simple())),
        b"bad native log",
    )
    .unwrap();
    let catalog = state.catalog_service();
    let list = catalog
        .list_tapes(tonic::Request::new(pb::ListTapesRequest::default()))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(list.tapes.len(), 3);
    let bad_tape = list.tapes.iter().find(|t| t.tape_uuid == bad).unwrap();
    assert!(bad_tape.closeout_authority_error.is_some());
    assert_eq!(
        (
            bad_tape.unprotected_bytes,
            bad_tape.closeout_due,
            bad_tape.closeout_overdue
        ),
        (None, None, None)
    );
    assert!(bad_tape.unprotected_since.is_none());
    assert_eq!(
        catalog
            .get_tape(tonic::Request::new(pb::GetTapeRequest {
                tape_uuid: bad.to_vec()
            }))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    let tape = catalog
        .get_tape(tonic::Request::new(pb::GetTapeRequest {
            tape_uuid: TAPE.to_vec(),
        }))
        .await
        .unwrap()
        .into_inner();
    let projected = crate::catalog_conversion::tape_to_proto(
        state.index().unwrap().get_tape(&TAPE).unwrap().unwrap(),
    );
    assert_eq!(tape.tape_uuid, TAPE);
    assert_eq!(tape.body_format, projected.body_format);
    assert_eq!(tape.block_size_bytes, Some(u64::from(BLOCK)));
    assert_eq!(
        tape.last_committed_tape_file,
        projected.last_committed_tape_file
    );
    assert_eq!(tape.state, projected.state);
    assert_eq!(tape.pool_id.as_deref(), Some(POOL));
    assert!(tape.unprotected_bytes.unwrap() > 0);
    let filtered = catalog
        .list_tapes(tonic::Request::new(pb::ListTapesRequest {
            pool_id: POOL.into(),
            ..Default::default()
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(filtered.tapes, vec![tape]);
    let live = state
        .library_service()
        .get_live_status(tonic::Request::new(pb::GetLiveStatusRequest {}))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(live.closeout_debt.len(), 2);
    assert!(live
        .closeout_debt
        .iter()
        .any(|t| t.tape_uuid == bad && t.closeout_authority_error.is_some()));
}

/// Worker ownership is visible before it reserves a drive, so all exact-tape
/// client entry points wait and then make their normal admission decision.
#[tokio::test]
async fn b7_fix_pinned_closeout_and_finalize_wait_for_worker() {
    for caller in ["pinned", "closeout", "finalize"] {
        let root = tempfile::tempdir().unwrap();
        let world = due_tape(root.path()).await;
        let mut state = state(root.path());
        state.library_snapshot = Some(library_snapshot_cell(
            world.lock().unwrap().library_snapshot(),
        ));
        let (changer, _changer_rx) = mpsc::channel(2);
        let (drive, mut drive_rx) = mpsc::channel(2);
        state.drive_pool = Some(DrivePool::new_for_library(
            LIBRARY,
            changer,
            HashMap::from([(BAY, drive)]),
            Arc::new(HashMap::from([(BAY, AtomicBool::new(false))])),
        ));
        let worker = tokio::spawn({
            let state = state.clone();
            async move { crate::mount::service_closeout_obligation(&state, TAPE, 1).await }
        });
        let command = tokio::time::timeout(StdDuration::from_secs(5), drive_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let DriveCommand::CloseoutTape {
            reply: worker_reply,
            ..
        } = command
        else {
            panic!("worker must dispatch closeout")
        };
        let mut client = tokio::spawn({
            let state = state.clone();
            async move {
                match caller {
                    "pinned" => crate::mount::test_open_write_for_caller(
                        &state,
                        TAPE,
                        POOL,
                        Arc::new(AtomicBool::new(false)),
                    )
                    .await
                    .map(|_| ()),
                    "closeout" => {
                        crate::mount::closeout_tape(
                            &state,
                            TAPE,
                            None,
                            crate::closeout_progress::CloseoutProgress::new(
                                Uuid::new_v4(),
                                TAPE,
                                None,
                            )
                            .unwrap(),
                        )
                        .await
                    }
                    _ => crate::mount::manual_finalize_tape(
                        &state,
                        crate::mount::ManualFinalizeTapeAdmission {
                            candidate_operation_id: Uuid::new_v4(),
                            actor: remanence_state::AuditActor::System,
                            actor_fingerprint: "test".into(),
                            idempotency_key: Uuid::new_v4(),
                            request_fingerprint: [7; 32],
                            tape_uuid: TAPE,
                            expected_pool_id: Some(POOL.into()),
                            reason: "operator".into(),
                        },
                    )
                    .await
                    .and_then(|result| match result {
                        ManualFinalizeTapeResult::Busy => {
                            Err(Status::failed_precondition("unexpected Busy"))
                        }
                        _ => Ok(()),
                    }),
                }
            }
        });
        assert!(
            tokio::time::timeout(StdDuration::from_millis(30), &mut client)
                .await
                .is_err(),
            "{caller} must wait while worker owns tape"
        );
        worker_reply
            .send(Err(Status::unavailable("worker test settled")))
            .unwrap();
        worker.await.unwrap().unwrap_err();
        if caller != "finalize" {
            match tokio::time::timeout(StdDuration::from_secs(5), drive_rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                DriveCommand::OpenWrite { reply, .. } => {
                    reply
                        .send(Err(Status::unavailable("client reached actor")))
                        .unwrap();
                }
                DriveCommand::CloseoutTape { reply, .. } => {
                    reply
                        .send(Err(Status::unavailable("client reached actor")))
                        .unwrap();
                }
                _ => panic!("unexpected client command"),
            }
        }
        let result = tokio::time::timeout(StdDuration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap();
        if let Err(error) = result {
            assert!(
                !error.message().contains("already mounted") && !error.message().contains("Busy"),
                "{caller}: {error}"
            );
        }
    }
}

#[tokio::test]
async fn b7_fix_worker_waits_for_drive_without_reserving_memory() {
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    let mut state = state(root.path());
    state.library_snapshot = Some(library_snapshot_cell(
        world.lock().unwrap().library_snapshot(),
    ));
    let (changer, _changer_rx) = mpsc::channel(2);
    let (drive, mut drive_rx) = mpsc::channel(2);
    let pool = DrivePool::new_for_library(
        LIBRARY,
        changer,
        HashMap::from([(BAY, drive)]),
        Arc::new(HashMap::from([(BAY, AtomicBool::new(false))])),
    );
    state.drive_pool = Some(pool.clone());
    let key = crate::drive_pool::DriveKey::new(LIBRARY, BAY);
    let reserved = pool.reserve_drive(&key).unwrap();
    let closing = pool.test_begin_closing([0xaa; 16], key);
    let mut work = tokio::spawn({
        let state = state.clone();
        async move { crate::mount::service_closeout_obligation(&state, TAPE, 1).await }
    });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(50), &mut work)
            .await
            .is_err()
    );
    assert_eq!(
        state.io_memory.granted(),
        0,
        "no parity charge before drive reservation"
    );
    drop(reserved);
    drop(closing);
    match tokio::time::timeout(StdDuration::from_secs(5), drive_rx.recv())
        .await
        .unwrap()
        .unwrap()
    {
        DriveCommand::CloseoutTape { reply, permit, .. } => {
            assert!(permit.bytes() > 0);
            assert_eq!(state.io_memory.granted(), permit.bytes());
            drop(permit);
            reply
                .send(Err(Status::unavailable("settled test attempt")))
                .unwrap();
        }
        _ => panic!("closeout expected"),
    }
    work.await.unwrap().unwrap_err();
    assert_eq!(state.io_memory.granted(), 0);
}

#[tokio::test]
async fn b7_fix_scan_skips_bad_entries_prunes_keys_and_reprojects_out_of_scope() {
    let root = tempfile::tempdir().unwrap();
    let world = due_tape(root.path()).await;
    let state = state(root.path()); // no operated library/drive at all
    let before = world.lock().unwrap().command_log.len();
    std::fs::write(
        state.checkpoint_journal_dir.join("000-malformed.remtape"),
        b"bad",
    )
    .unwrap();
    let db = rusqlite::Connection::open(&*state.index_path).unwrap();
    db.execute_batch("DELETE FROM tape_log_projection; DELETE FROM tape_log_object_commits; DELETE FROM tape_files;").unwrap();
    scan(&state).unwrap();
    assert!(
        state.closeout_work.contains((TAPE, 1)),
        "malformed entry cannot stop later discovery"
    );
    let native = summary(&state, TAPE).unwrap();
    assert_eq!(
        state
            .index()
            .unwrap()
            .projected_through_sequence(TAPE)
            .unwrap(),
        native.last_closed_sequence
    );
    let files: i64 = db
        .query_row("SELECT count(*) FROM tape_files", [], |r| r.get(0))
        .unwrap();
    assert!(
        files > 0,
        "host-only projection restores file rows outside mount scope"
    );
    assert_eq!(world.lock().unwrap().command_log.len(), before);
    db.execute(
        "UPDATE tapes SET state = 'retired' WHERE tape_uuid = ?",
        [TAPE.as_slice()],
    )
    .unwrap();
    scan(&state).unwrap();
    assert!(!state.closeout_work.contains((TAPE, 1)));
    db.execute(
        "UPDATE tapes SET state = 'ready' WHERE tape_uuid = ?",
        [TAPE.as_slice()],
    )
    .unwrap();
    scan(&state).unwrap();
    assert!(state.closeout_work.contains((TAPE, 1)));
    db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    db.execute("DELETE FROM tapes WHERE tape_uuid = ?", [TAPE.as_slice()])
        .unwrap();
    scan(&state).unwrap();
    assert!(!state.closeout_work.contains((TAPE, 1)));
}

#[tokio::test]
async fn b7_fix_pool_skips_excluded_or_failed_preferred_drive_and_never_evicts_excluded_bay() {
    for excluded in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let world = due_tape(root.path()).await;
        let mut state = state(root.path());
        let other = [0xbc; 16];
        state
            .index_write()
            .unwrap()
            .provision_tape(ProvisionTapeInput {
                tape_uuid: other,
                voltag: "OTHER1L9".into(),
                block_size: BLOCK,
                parity: ParityConfig::Scheme(scheme()),
                force: false,
            })
            .unwrap();
        state
            .index_write()
            .unwrap()
            .project_tape_pool_membership(other, POOL)
            .unwrap();
        let mut inventory = world.lock().unwrap().library_snapshot();
        let mut healthy = inventory.drive_bays[0].clone();
        healthy.element_address = BAY + 1;
        healthy.loaded_tape = Some("OTHER1L9".into());
        healthy.source_slot = None;
        inventory.drive_bays.push(healthy);
        state.library_snapshot = Some(library_snapshot_cell(inventory.clone()));
        let (changer, mut changer_rx) = mpsc::channel(2);
        let (drive, mut drive_rx) = mpsc::channel(2);
        let mut drives = HashMap::from([(BAY + 1, drive)]);
        let mut reservations = HashMap::from([(BAY + 1, AtomicBool::new(false))]);
        if !excluded {
            let (dead, receiver) = mpsc::channel(1);
            drop(receiver);
            drives.insert(BAY, dead);
            reservations.insert(BAY, AtomicBool::new(false));
        }
        let pool = DrivePool::new_for_library(LIBRARY, changer, drives, Arc::new(reservations));
        state.drive_pool = Some(pool.clone());
        let located =
            crate::mount::located_tapes_in_operated_libraries(&state, &pool, None).unwrap();
        assert_eq!(located.contains_key(&TAPE), !excluded);
        let refusal = crate::mount::test_mount_for_write(&state, LIBRARY, &TAPE)
            .await
            .unwrap_err();
        assert!(refusal.message().contains("0x0100"));
        assert!(refusal.message().contains(if excluded {
            "out-of-scope"
        } else {
            "unavailable"
        }));
        let open = tokio::spawn({
            let state = state.clone();
            async move {
                crate::mount::open_write_session(
                    remanence_state::AuditActor::System,
                    &state,
                    crate::mount::WriteSessionTarget::Pool {
                        pool_id: POOL.into(),
                    },
                    None,
                )
                .await
            }
        });
        match tokio::time::timeout(StdDuration::from_secs(5), drive_rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            DriveCommand::OpenWrite {
                selected, reply, ..
            } => {
                assert_eq!(
                    selected.tape_uuid, other,
                    "unreachable due tape cannot monopolize pool opens"
                );
                reply
                    .send(Err(Status::unavailable("test reached healthy actor")))
                    .unwrap();
            }
            _ => panic!("open expected"),
        }
        open.await.unwrap().unwrap_err();
        assert!(changer_rx.try_recv().is_err());
        if excluded {
            // Move the target to a slot and leave the excluded bay as the only
            // physically evictable drive. It must not become an eviction target.
            inventory.drive_bays.pop();
            inventory.slots[0].full = true;
            inventory.slots[0].cartridge = Some("OTHER1L9".into());
            state.library_snapshot = Some(library_snapshot_cell(inventory));
            crate::mount::test_mount_for_write(&state, LIBRARY, &other)
                .await
                .unwrap_err();
            assert!(changer_rx.try_recv().is_err());
            assert!(drive_rx.try_recv().is_err());
        }
    }
}
