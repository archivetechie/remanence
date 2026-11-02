//! Deadline regressions at the actual dispatch and native ending boundaries.
use super::*;
use remanence_state::tape_log::{SessionEndReason, TailIdentity};

/// A transport call owns the gate until released, including on transport loss.
pub(super) struct CommandGate {
    pub(super) armed: AtomicBool,
    pub(super) entered: Mutex<Option<oneshot::Sender<()>>>,
    pub(super) release: (Mutex<bool>, std::sync::Condvar),
    pub(super) sidecar: bool,
    pub(super) opcode: Option<u8>,
    pub(super) lost: bool,
}
impl CommandGate {
    fn wait(&self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        let target = if let Some(opcode) = self.opcode {
            cdb[0] == opcode
        } else if self.sidecar {
            cdb[0] == 0x0a
        } else {
            cdb[0] == 0x10 && cdb[2..5] == [0, 0, 0]
        };
        if target && self.armed.swap(false, Ordering::SeqCst) {
            self.entered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            let (lock, wake) = &self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            if self.lost {
                return Err(remanence_library::ScsiError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "injected transport settle bound exhausted",
                )));
            }
        }
        Ok(())
    }
    pub(super) fn release(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
pub(super) struct GatedTransport {
    pub(super) inner: ModelTransport,
    pub(super) gate: Arc<CommandGate>,
}
impl SgTransport for GatedTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.gate.wait(cdb)?;
        self.inner.execute_in(cdb, buf)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.gate.wait(cdb)?;
        self.inner.execute_out(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.gate.wait(cdb)?;
        self.inner.execute_none(cdb)
    }
}

/// Expire while a command is issued, then settle it without dispatching another.
async fn interrupted_session(sidecar: bool, lost: bool) {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let (entered, reached) = oneshot::channel();
    let gate = Arc::new(CommandGate {
        armed: AtomicBool::new(false),
        entered: Mutex::new(Some(entered)),
        release: (Mutex::new(false), std::sync::Condvar::new()),
        sidecar,
        opcode: None,
        lost,
    });
    let model = world.lock().unwrap().library_snapshot();
    let w = world.clone();
    let g = gate.clone();
    let library = model
        .open_with(
            &remanence_library::StaticAllowlist::new([LIBRARY]),
            move |path| {
                let role = w.lock().unwrap().role_for_path(path).unwrap();
                Ok::<Box<dyn SgTransport>, remanence_library::IoErrorKind>(Box::new(
                    GatedTransport {
                        inner: ModelTransport::new(w.clone(), role),
                        gate: g.clone(),
                    },
                ))
            },
        )
        .unwrap();
    let lifecycle = DrivePoolLifecycle::default();
    let key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let (tx, opened, thread) =
        open_writer_configured(root.path(), library, selected(), |cfg, _| {
            cfg.lifecycle = Some(lifecycle.clone());
            cfg.reservations = reservations.clone();
        })
        .await;
    let id = opened.unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.path().join("pending"),
        "pending",
        "pending",
        b"deadline object",
    )
    .await;
    gate.armed.store(true, Ordering::SeqCst);
    let (reply, mut received) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    if sidecar {
        assert_eq!(
            received
                .try_recv()
                .expect("reply precedes closeout")
                .unwrap()
                .session
                .checkpointed_objects
                .len(),
            1
        );
    }
    let before = world.lock().unwrap().command_log.len();
    // Graceful shutdown begins while the physical call is still in flight.
    lifecycle
        .shutdown_deadline
        .set(std::time::Instant::now() + StdDuration::from_millis(30))
        .unwrap();
    let (changer, _rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(key, tx)]),
        reservations,
        lifecycle,
    );
    pool.record_session(
        id,
        MountedSession {
            bay: BAY,
            library_serial: LIBRARY.into(),
            barcode: Some(BARCODE.into()),
            home_slot: None,
            tape_uuid: TAPE,
            drive_uuid: None,
        },
    );
    let mut state =
        crate::ApiState::new(CatalogIndex::open(root.path().join("state.sqlite")).unwrap());
    state.drive_pool = Some(pool);
    let shutdown = tokio::spawn(async move { state.shutdown_drive_pool().await });
    tokio::time::sleep(StdDuration::from_millis(60)).await;
    assert!(
        !shutdown.is_finished(),
        "issued command must settle before shutdown returns"
    );
    assert_eq!(world.lock().unwrap().command_log.len(), before);
    let settled_at = std::time::Instant::now();
    gate.release();
    let _ = tokio::time::timeout(StdDuration::from_secs(3), shutdown)
        .await
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    assert!(settled_at.elapsed() < StdDuration::from_secs(3));
    assert_eq!(
        world.lock().unwrap().command_log.len(),
        before + usize::from(!lost),
        "only the already issued command may reach the model"
    );
    let authority = TapeAuthority::open(
        root.path().join("journals/tapes"),
        TAPE,
        &root.path().join("state.sqlite"),
    )
    .unwrap();
    let snapshot = authority.freeze().unwrap();
    let summary = snapshot.tape_summary();
    let mut index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
    if lost {
        assert!(
            summary.open_session.is_some(),
            "unknown completion keeps the crash row"
        );
        assert!(summary.last_session_end.is_none());
        for fence in index.list_active_tape_io_fences().unwrap() {
            index
                .release_tape_io_fence(&fence.quarantine_id, "test transport restored")
                .unwrap();
        }
        let (_tx, opened, thread) = open_writer(root.path(), open_model_library(world)).await;
        let error = opened.unwrap_err();
        assert!(error.message().contains("TailBeyondKnownState"), "{error}");
        thread.join().unwrap();
    } else {
        let end = summary.last_session_end.as_ref().unwrap();
        assert_eq!(
            end.reason,
            if sidecar {
                SessionEndReason::CloseoutFailed
            } else {
                SessionEndReason::FailedAppend
            }
        );
        assert_eq!(end.tail_file_count, 1);
        assert!(summary.open_session.is_none());
        assert!(index.list_active_tape_io_fences().unwrap().is_empty());
        let mut tail = Vec::new();
        snapshot
            .visit_session_tail(&mut |file| {
                tail.push(file.clone());
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            (&tail[0].identity, sidecar),
            (TailIdentity::Sidecar { .. }, true) | (TailIdentity::Object(_), false)
        ));
        if sidecar {
            assert_eq!(tail[0].known_blocks, 1);
            assert_eq!(tail[0].unknown_blocks, 0);
        }
        if sidecar {
            assert!(summary.closeout_due());
        }
        drop(snapshot);
        let (tx, opened, thread) = open_writer(root.path(), open_model_library(world)).await;
        let id = opened.expect("recorded live tail must reopen without acknowledgement");
        if sidecar {
            assert!(
                super::b6b_fix_tests::native_records(root.path())
                    .iter()
                    .any(|record| matches!(
                        record,
                        remanence_state::tape_log::Record::ClosedOut(_)
                    )),
                "next access closes the interrupted sidecar"
            );
        }
        close_writer(&tx, id, thread).await;
    }
}

#[tokio::test]
async fn closeout_deadline_settles_sidecar_and_next_open_closes_due_range() {
    interrupted_session(true, false).await;
}
#[tokio::test]
async fn final_barrier_deadline_settles_command_and_names_object_tail() {
    interrupted_session(false, false).await;
}
#[tokio::test]
async fn lost_transport_at_settle_bound_leaves_crash_row() {
    interrupted_session(false, true).await;
}

/// Both actor requests must arrive before either responds: sequential shutdown
/// would deadlock this test. Both workers observe the very same absolute cutoff.
#[tokio::test]
async fn shutdown_sessions_share_one_deadline_and_end_in_parallel() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    let lifecycle = DrivePoolLifecycle::default();
    let rendezvous = Arc::new(tokio::sync::Barrier::new(2));
    let mut drives = HashMap::new();
    let mut reservations = HashMap::new();
    let mut sessions = Vec::new();
    let mut actors = Vec::new();
    for bay in [BAY, BAY + 1] {
        let key = DriveKey::new(LIBRARY, bay);
        let (tx, mut rx) = mpsc::channel(4);
        drives.insert(key.clone(), tx);
        reservations.insert(key, AtomicBool::new(false));
        let id = Uuid::new_v4();
        let mounted = MountedSession {
            bay,
            library_serial: LIBRARY.into(),
            barcode: None,
            home_slot: None,
            tape_uuid: [bay as u8; 16],
            drive_uuid: None,
        };
        sessions.push((id, mounted));
        let shared = lifecycle.clone();
        let rendezvous = rendezvous.clone();
        actors.push(tokio::spawn(async move {
            let Some(DriveCommand::Checkpoint {
                reply: Some(reply), ..
            }) = rx.recv().await
            else {
                panic!("shutdown checkpoint");
            };
            rendezvous.wait().await;
            let deadline = *shared.shutdown_deadline.get().unwrap();
            tokio::time::sleep_until(deadline.into()).await;
            shared.sessions.lock().unwrap().remove(&id);
            reply
                .send(Err(Status::not_found("write session not found")))
                .unwrap();
            deadline
        }));
    }
    let pool =
        DrivePool::new_with_lifecycle(HashMap::new(), drives, Arc::new(reservations), lifecycle);
    for (id, mounted) in sessions {
        pool.record_session(id, mounted);
    }
    let mut state =
        crate::ApiState::new(CatalogIndex::open(root.path().join("state.sqlite")).unwrap());
    state.shutdown_close_deadline_secs = 1;
    state.drive_pool = Some(pool);
    let started = std::time::Instant::now();
    tokio::time::timeout(StdDuration::from_millis(1800), state.shutdown_drive_pool())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < StdDuration::from_millis(1800));
    let a = actors.remove(0).await.unwrap();
    let b = actors.remove(0).await.unwrap();
    assert_eq!(a, b);
    state.begin_shutdown();
    assert_eq!(
        state.shutdown_close_deadline(),
        Some(a),
        "re-entry cannot extend the budget"
    );
}

/// A client stall inside the synchronous overlap reader cannot outlive shutdown.
#[tokio::test]
async fn shutdown_deadline_wakes_stalled_overlap_ingress_and_ends_actor() {
    use super::ingress_deadline_media_tests::StalledBody;
    use crate::write_session_ingress::WriteSessionApi;
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let lifecycle = DrivePoolLifecycle::default();
    let key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let (tx, opened, thread) = open_writer_configured(
        root.path(),
        open_model_library(world.clone()),
        selected(),
        |cfg, _| {
            cfg.lifecycle = Some(lifecycle.clone());
            cfg.reservations = reservations.clone();
        },
    )
    .await;
    let id = opened.unwrap();
    let before = world.lock().unwrap().tapes[BARCODE].records.len();
    let (changer, _rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(key, tx)]),
        reservations,
        lifecycle,
    );
    pool.record_session(
        id,
        MountedSession {
            bay: BAY,
            library_serial: LIBRARY.into(),
            barcode: Some(BARCODE.into()),
            home_slot: None,
            tape_uuid: TAPE,
            drive_uuid: None,
        },
    );
    let mut state =
        crate::ApiState::new(CatalogIndex::open(root.path().join("state.sqlite")).unwrap());
    let spool = root.path().join("spool");
    std::fs::create_dir(&spool).unwrap();
    state.spool_dir = Some(Arc::new(spool));
    state.spool_budget_bytes = Some(8 * 1024 * 1024);
    state.drive_pool = Some(pool);
    state.shutdown_close_deadline_secs = 1;
    state.append_ingress_idle_secs = 300;
    state.append_staging_mode = AppendStagingMode::Overlap;
    let slab = crate::append_ring::APPEND_RING_SLAB_BYTES;
    state.append_ring_bytes = (6 * slab) as u64;
    state.append_overlap_min_bytes = slab as u64;
    let api = WriteSessionApi {
        state: state.clone(),
    };
    let (waiting, ready) = oneshot::channel();
    let append = tokio::spawn(async move {
        api.append_object_stream(StalledBody {
            messages: std::collections::VecDeque::from([
                Ok(pb::AppendObjectMessage {
                    payload: Some(pb::append_object_message::Payload::Start(
                        pb::AppendObjectStart {
                            session_id: id.as_bytes().to_vec(),
                            caller_object_id: "stalled".into(),
                            declared_size_bytes: Some((8 * slab) as u64),
                            expected_content_sha256: Some(vec![0; 32]),
                            source_replay_capability: pb::SourceReplayCapability::ReplayFromStart
                                as i32,
                            ..Default::default()
                        },
                    )),
                }),
                Ok(pb::AppendObjectMessage {
                    payload: Some(pb::append_object_message::Payload::Chunk(
                        pb::AppendObjectChunk {
                            session_id: id.as_bytes().to_vec(),
                            data: vec![0x61; 6 * slab],
                        },
                    )),
                }),
            ]),
            waiting: Some(waiting),
        })
        .await
    });
    tokio::time::timeout(StdDuration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), async {
        while world.lock().unwrap().tapes[BARCODE].records.len() <= before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let _ = tokio::time::timeout(StdDuration::from_secs(3), state.shutdown_drive_pool())
        .await
        .expect("shutdown releases stalled overlap reader");
    let error = tokio::time::timeout(StdDuration::from_secs(1), append)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::DeadlineExceeded);
    assert!(error.message().contains("ShutdownCloseDeadline"));
    thread.join().unwrap();
    let authority = TapeAuthority::open(
        root.path().join("journals/tapes"),
        TAPE,
        &root.path().join("state.sqlite"),
    )
    .unwrap();
    let snapshot = authority.freeze().unwrap();
    let end = snapshot.tape_summary().last_session_end.as_ref().unwrap();
    assert_eq!(end.reason, SessionEndReason::FailedAppend);
    assert!(end.tail_file_count > 0);
}

/// A native commit fsync already in progress settles as success after the cutoff;
/// neither the deadline nor skipped closeout can revoke its receipt.
#[tokio::test]
async fn commit_sync_crossing_deadline_returns_receipts_and_leaves_closeout_due() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let files = MemoryDirectory::default();
    let _registration = register(root.path().join("journals/tapes"), Arc::new(files.clone()));
    let lifecycle = DrivePoolLifecycle::default();
    let (tx, opened, thread) = open_writer_configured(
        root.path(),
        open_model_library(world.clone()),
        selected(),
        |cfg, _| {
            cfg.lifecycle = Some(lifecycle.clone());
        },
    )
    .await;
    let id = opened.unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.path().join("object"),
        "object",
        "object",
        b"durable despite deadline",
    )
    .await;
    let authority = TapeAuthority::open(
        root.path().join("journals/tapes"),
        TAPE,
        &root.path().join("state.sqlite"),
    )
    .unwrap();
    let target = remanence_state::authority::fixtures::publication_target(&authority);
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let wait_gate = gate.clone();
    let (entered, reached) = oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let syncs = std::sync::atomic::AtomicUsize::new(0);
    files.observe(Some(Arc::new(move |event| {
        if let FileEvent::BeforeSync(path) = event {
            // stage_report syncs the provisional group; the next sync is Committed.
            if target.matches(&path) && syncs.fetch_add(1, Ordering::SeqCst) == 1 {
                if let Some(entered) = entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    let mut released = wait_gate.0.lock().unwrap();
                    while !*released {
                        released = wait_gate.1.wait(released).unwrap();
                    }
                }
            }
        }
    })));
    let (reply, mut received) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), reached)
        .await
        .unwrap()
        .unwrap();
    let commands = world.lock().unwrap().command_log.len();
    lifecycle
        .shutdown_deadline
        .set(std::time::Instant::now())
        .unwrap();
    lifecycle.shutting_down.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            received.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ),
        "no receipt before fsync settles"
    );
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    let closed = tokio::time::timeout(StdDuration::from_secs(5), received)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    files.observe(None);
    assert_eq!(closed.session.checkpointed_objects.len(), 1);
    assert_eq!(
        lifecycle.ended_writes.lock().unwrap()[&id]
            .0
            .checkpointed_objects,
        closed.session.checkpointed_objects
    );
    assert_eq!(world.lock().unwrap().command_log.len(), commands);
    let snapshot = authority.freeze().unwrap();
    let summary = snapshot.tape_summary();
    let end = summary.last_session_end.as_ref().unwrap();
    assert_eq!(end.reason, SessionEndReason::FailedAppend);
    assert_eq!(end.tail_file_count, 0);
    assert!(summary.closeout_due());
}

/// An actor-open reply racing the pool's initial snapshot must still be ended.
#[tokio::test]
async fn shutdown_ends_session_registered_after_initial_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let lifecycle = DrivePoolLifecycle::default();
    let key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let (tx, opened, thread) = open_writer_configured(
        root.path(),
        open_model_library(world),
        selected(),
        |cfg, _| {
            cfg.lifecycle = Some(lifecycle.clone());
            cfg.reservations = reservations.clone();
        },
    )
    .await;
    let id = opened.unwrap();
    let (changer, _rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(key, tx)]),
        reservations,
        lifecycle,
    );
    let mut state =
        crate::ApiState::new(CatalogIndex::open(root.path().join("state.sqlite")).unwrap());
    state.drive_pool = Some(pool.clone());
    let shutdown = tokio::spawn(async move { state.shutdown_drive_pool().await });
    tokio::time::timeout(StdDuration::from_secs(2), async {
        while !pool.is_shutting_down() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // The spawned shutdown runs until its reservation wait before yielding.
    assert!(!shutdown.is_finished());
    pool.record_session(
        id,
        MountedSession {
            bay: BAY,
            library_serial: LIBRARY.into(),
            barcode: Some(BARCODE.into()),
            home_slot: None,
            tape_uuid: TAPE,
            drive_uuid: None,
        },
    );
    tokio::time::timeout(StdDuration::from_secs(3), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    thread.join().unwrap();
    let authority = TapeAuthority::open(
        root.path().join("journals/tapes"),
        TAPE,
        &root.path().join("state.sqlite"),
    )
    .unwrap();
    let snapshot = authority.freeze().unwrap();
    assert!(snapshot.tape_summary().open_session.is_none());
    assert_eq!(
        snapshot
            .tape_summary()
            .last_session_end
            .as_ref()
            .unwrap()
            .reason,
        SessionEndReason::GracefulShutdown
    );
    assert!(pool.sessions_by_drive().is_empty());
}
