//! B6b ending regressions at the real actor and native authority boundaries.
use super::*;
use remanence_state::tape_log::{
    codec::Decoder, store::Header, Record as NativeRecord, SessionEndReason,
};

/// Read every native record, including endings later superseded by ClosedOut.
fn native_records(root: &Path) -> Vec<NativeRecord> {
    let bytes = std::fs::read(
        root.join("journals/tapes")
            .join(format!("{}.remtape", Uuid::from_bytes(TAPE).simple())),
    )
    .unwrap();
    let mut input = std::io::Cursor::new(&bytes);
    Header::decode(&mut input, TAPE).unwrap();
    let mut decoder = Decoder::at(&bytes[input.position() as usize..], 1, input.position());
    let mut records = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        records.push(frame.record);
    }
    records
}

/// Keep the media bootstrap and selected geometry identical for both policies.
fn selection(root: &Path, world: &Arc<Mutex<VirtualWorld>>, parity: bool) -> SelectedTape {
    if parity {
        return selected();
    }
    let mut index = CatalogIndex::open(root.join("state.sqlite")).unwrap();
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: TAPE,
            voltag: BARCODE.into(),
            block_size: BLOCK,
            parity: ParityConfig::None,
            force: false,
        })
        .unwrap();
    index.project_tape_pool_membership(TAPE, POOL).unwrap();
    let mut model = world.lock().unwrap();
    let Record::Block(block) = &mut model.tapes.get_mut(BARCODE).unwrap().records[0] else {
        panic!("bootstrap")
    };
    let mut bootstrap = parse_bootstrap_block(block).unwrap();
    bootstrap.scheme = None;
    bootstrap.no_parity_flag = true;
    write_bootstrap_block(&bootstrap, block).unwrap();
    SelectedTape {
        parity_config: ParityConfig::None,
        ..selected()
    }
}

/// A serial ingress deadline drops a pending batch but must retain its dispatched identity.
#[tokio::test]
async fn pending_serial_deadline_and_shutdown_abort_keep_named_tail() {
    use super::ingress_deadline_media_tests::StalledBody;
    use crate::write_session_ingress::WriteSessionApi;
    for parity in [false, true] {
        for abort_shutdown in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let world = setup(root.path());
            let selected = selection(root.path(), &world, parity);
            let lifecycle = DrivePoolLifecycle::default();
            let (tx, opened, thread) = open_writer_configured(
                root.path(),
                open_model_library(world.clone()),
                selected.clone(),
                |cfg, _| {
                    cfg.lifecycle = Some(lifecycle.clone());
                },
            )
            .await;
            let id = opened.unwrap();
            // A committed short prefix makes the parity rebuild observable.
            append_actor_test_file(
                &tx,
                id,
                root.path().join("committed"),
                "committed",
                "committed",
                b"saved",
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
            received.await.unwrap().unwrap();
            let pending = append_actor_test_file(
                &tx,
                id,
                root.path().join("pending"),
                "pending",
                "pending",
                b"uncommitted",
            )
            .await;
            assert_eq!(
                pending.record.append_commit_info.unwrap().durability,
                pb::AppendDurability::Written as i32
            );
            if abort_shutdown {
                lifecycle.shutting_down.store(true, Ordering::SeqCst);
                let (reply, received) = oneshot::channel();
                tx.send(DriveCommand::Abort {
                    session_id: id,
                    reason: Some("operator abort racing shutdown".into()),
                    reply,
                })
                .await
                .unwrap();
                received.await.unwrap().unwrap();
            } else {
                let (changer, _rx) = mpsc::channel(1);
                let pool = DrivePool::new_for_library(
                    LIBRARY,
                    changer,
                    HashMap::from([(BAY, tx.clone())]),
                    Arc::new(HashMap::from([(BAY, AtomicBool::new(true))])),
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
                let mut state = crate::ApiState::new(
                    CatalogIndex::open(root.path().join("state.sqlite")).unwrap(),
                );
                let spool = root.path().join("spool");
                std::fs::create_dir(&spool).unwrap();
                state.spool_dir = Some(Arc::new(spool));
                state.spool_budget_bytes = Some(8 * 1024 * 1024);
                state.drive_pool = Some(pool);
                state.append_staging_mode = AppendStagingMode::Serial;
                state.append_ingress_idle_secs = 1;
                let api = WriteSessionApi { state };
                let (waiting, ready) = oneshot::channel();
                let append = tokio::spawn(async move {
                    api.append_object_stream(StalledBody {
                        messages: std::collections::VecDeque::from([Ok(pb::AppendObjectMessage {
                            payload: Some(pb::append_object_message::Payload::Start(
                                pb::AppendObjectStart {
                                    session_id: id.as_bytes().to_vec(),
                                    caller_object_id: "stalled".into(),
                                    declared_size_bytes: Some(100),
                                    source_replay_capability:
                                        pb::SourceReplayCapability::ReplayFromStart as i32,
                                    ..Default::default()
                                },
                            )),
                        })]),
                        waiting: Some(waiting),
                    })
                    .await
                });
                ready.await.unwrap();
                let error = tokio::time::timeout(StdDuration::from_secs(5), append)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err();
                assert_eq!(error.code(), tonic::Code::DeadlineExceeded);
            }
            thread.join().unwrap();
            let records = native_records(root.path());
            let end_pos = records
                .iter()
                .position(|r| matches!(r, NativeRecord::SessionEnded(_)))
                .unwrap();
            let NativeRecord::SessionEnded(end) = &records[end_pos] else {
                unreachable!()
            };
            assert_eq!(
                end.reason,
                if abort_shutdown {
                    SessionEndReason::Abort
                } else {
                    SessionEndReason::ClientStall
                }
            );
            assert!(end.tail_file_count >= 1);
            assert!(records.iter().any(|r| matches!(r, NativeRecord::PayloadContinuation(c) if matches!(&c.chunk, remanence_state::tape_log::Chunk::TailFile(file) if matches!(&file.identity, remanence_state::tape_log::TailIdentity::Object(object) if *object == Uuid::from_slice(&pending.record.object_id).unwrap().to_string())))));
            assert_eq!(
                records[end_pos + 1..]
                    .iter()
                    .any(|r| matches!(r, NativeRecord::ClosedOut(_))),
                parity
            );
            let index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
            assert!(index.list_active_tape_io_fences().unwrap().is_empty());
            let (tx, opened, thread) = open_writer_with_selection(
                root.path(),
                open_model_library(world.clone()),
                selected,
            )
            .await;
            close_writer(
                &tx,
                opened.expect("named tail re-proves without supersede"),
                thread,
            )
            .await;
            assert!(index.list_active_tape_io_fences().unwrap().is_empty());
        }
    }
}

/// Neither refused command can leak its reason into a later actual close.
#[tokio::test]
async fn stale_idle_and_wrong_abort_do_not_change_end_reason() {
    for stale_idle in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let (tx, opened, thread) = open_writer(root.path(), open_model_library(world)).await;
        let id = opened.unwrap();
        if stale_idle {
            tx.send(DriveCommand::TimerIdleClose {
                session_id: id,
                checkpoint_batch_id: Uuid::new_v4(),
            })
            .await
            .unwrap();
        } else {
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::Abort {
                session_id: Uuid::new_v4(),
                reason: None,
                reply,
            })
            .await
            .unwrap();
            assert_eq!(
                received.await.unwrap().unwrap_err().code(),
                tonic::Code::NotFound
            );
        }
        close_writer(&tx, id, thread).await;
        assert!(native_records(root.path()).iter().any(|r| matches!(r, NativeRecord::SessionEnded(end) if end.reason == SessionEndReason::CleanClose)));
    }
}

/// Post-reply fsync ambiguity never revokes the receipt or invents a durable end.
#[tokio::test]
async fn post_reply_closeout_and_end_fsync_failures_keep_receipts_and_crash_row() {
    for fail_end in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let files = MemoryDirectory::default();
        let _registration = register(root.path().join("journals/tapes"), Arc::new(files.clone()));
        let (tx, opened, thread) = open_writer(root.path(), open_model_library(world)).await;
        let id = opened.unwrap();
        let object = append_actor_test_file(
            &tx,
            id,
            root.path().join("object"),
            "object",
            "object",
            b"saved",
        )
        .await;
        let authority = TapeAuthority::open(
            root.path().join("journals/tapes"),
            TAPE,
            &root.path().join("state.sqlite"),
        )
        .unwrap();
        let target = remanence_state::authority::fixtures::publication_target(&authority);
        let backend = files.clone();
        let syncs = Arc::new(AtomicU64::new(0));
        let observed = syncs.clone();
        // Commit and ClosedOut each stage + sync; the empty SessionEnded has one sync.
        files.observe(Some(Arc::new(move |event| {
            if let FileEvent::BeforeSync(path) = event {
                if target.matches(&path) {
                    let n = observed.fetch_add(1, Ordering::SeqCst);
                    if n == if fail_end { 4 } else { 3 } {
                        backend.fail_next_sync(&path).unwrap();
                    }
                }
            }
        })));
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        let closed = received
            .await
            .unwrap()
            .expect("durable close receipts precede ending errors");
        assert_eq!(
            closed.session.checkpointed_objects[0].object_id,
            object.record.object_id
        );
        thread.join().unwrap();
        files.observe(None);
        let index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
        let fences = index.list_active_tape_io_fences().unwrap();
        assert_eq!(fences.len(), 1, "syncs={}", syncs.load(Ordering::SeqCst));
        assert_eq!(fences[0].reason, "AuthorityAmbiguous");
        // A lost fsync is modelled as losing its dirty bytes on restart.
        let mut snapshot = files.snapshot();
        snapshot.crash(&BTreeMap::new(), false).unwrap();
        snapshot.materialize(root.path(), root.path()).unwrap();
        let bytes = std::fs::read(
            root.path()
                .join("journals/tapes")
                .join(format!("{}.remtape", Uuid::from_bytes(TAPE).simple())),
        )
        .unwrap();
        let mut input = std::io::Cursor::new(&bytes);
        Header::decode(&mut input, TAPE).unwrap();
        let mut decoder = Decoder::at(&bytes[input.position() as usize..], 1, input.position());
        let mut fold = remanence_state::tape_log::fold::Fold::new();
        while let Some(frame) = decoder.next_frame().unwrap() {
            fold.push(&frame).unwrap();
        }
        let summary = fold.summary();
        assert!(summary.open_session.is_some());
    }
}

/// The API's automatic mid-session seal publishes its decision before the short sidecar.
#[tokio::test]
async fn automatic_mid_session_terminal_prefix_has_final_partial_epoch() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let (tx, opened, thread) = open_writer_configured(
        root.path(),
        open_model_library(world),
        selected(),
        |cfg, pool| {
            cfg.checkpoint_max_objects = 1;
            pool.watermark_low = 0.000_000_000_001;
        },
    )
    .await;
    let id = opened.unwrap();
    let result = append_actor_test_file(
        &tx,
        id,
        root.path().join("seal"),
        "seal",
        "seal",
        b"sealed mid-session",
    )
    .await;
    assert_eq!(
        result.record.append_commit_info.unwrap().sealed_after_write,
        Some(true)
    );
    thread.join().unwrap();
    let records = native_records(root.path());
    let decision = records
        .iter()
        .position(|r| matches!(r, NativeRecord::FinalizationDecided(_)))
        .unwrap();
    let prefix = records
        .iter()
        .position(|r| matches!(r, NativeRecord::TerminalPrefix { .. }))
        .unwrap();
    assert!(decision < prefix);
    let authority = TapeAuthority::open(
        root.path().join("journals/tapes"),
        TAPE,
        &root.path().join("state.sqlite"),
    )
    .unwrap();
    let plan = authority
        .latest_commit()
        .unwrap()
        .unwrap()
        .finalization
        .unwrap();
    assert!(plan.manual.is_none());
    assert_ne!(
        plan.terminal_prefix
            .unwrap()
            .sidecar_directory_entries
            .last()
            .unwrap()
            .flags
            & remanence_parity::SIDECAR_DIRECTORY_FLAG_FINAL_PARTIAL_EPOCH,
        0
    );
}

/// Closeout RPC validation binds the key to the whole request, even with debt due.
#[tokio::test]
async fn closeout_rpc_rejects_nil_reused_key_and_wrong_pool_with_debt() {
    use crate::pb::catalog_server::Catalog;
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let mut state =
        crate::ApiState::new(CatalogIndex::open(root.path().join("state.sqlite")).unwrap());
    state.checkpoint_journal_dir = Arc::new(root.path().join("journals/tapes"));
    std::fs::create_dir_all(state.checkpoint_journal_dir.as_ref()).unwrap();
    let service = state.catalog_service();
    let request = |key: Uuid, pool: Option<&str>| pb::CloseoutTapeRequest {
        tape_uuid: TAPE.to_vec(),
        expected_pool_id: pool.map(str::to_owned),
        idempotency_key: Some(pb::IdempotencyKey {
            value: key.as_bytes().to_vec(),
        }),
    };
    let error = service
        .closeout_tape(tonic::Request::new(request(Uuid::nil(), Some(POOL))))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    assert!(error.message().contains("nil UUID"));
    let key = Uuid::new_v4();
    service
        .closeout_tape(tonic::Request::new(request(key, Some(POOL))))
        .await
        .unwrap();
    let error = service
        .closeout_tape(tonic::Request::new(request(key, None)))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::AlreadyExists);
    assert!(error.message().contains("different request"));
    // Wait for the no-op operation before starting the writer's native lease.
    tokio::task::yield_now().await;
    let (tx, opened, thread) = open_writer(root.path(), open_model_library(world.clone())).await;
    let id = opened.unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.path().join("object"),
        "object",
        "object",
        b"committed debt",
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
    received.await.unwrap().unwrap();
    let before = world.lock().unwrap().command_log.len();
    let error = service
        .closeout_tape(tonic::Request::new(request(Uuid::new_v4(), Some("wrong"))))
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error.message(),
        "CloseoutTape expected_pool_id does not match the tape's assigned pool"
    );
    assert_eq!(world.lock().unwrap().command_log.len(), before);
    close_writer(&tx, id, thread).await;
}

/// Fail a settled final barrier without altering the preceding complete Objects.
struct FinalBarrierFailure {
    inner: ModelTransport,
    armed: Arc<AtomicBool>,
}
impl SgTransport for FinalBarrierFailure {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        bytes: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, bytes)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        bytes: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, bytes)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        if cdb[0] == 0x10 && cdb[2..5] == [0, 0, 0] && self.armed.swap(false, Ordering::SeqCst) {
            return Err(remanence_library::ScsiError::Io(std::io::Error::other(
                "injected final barrier failure",
            )));
        }
        self.inner.execute_none(cdb)
    }
}

/// The shutdown error reply is not completion: the named end must finish fsync first.
#[tokio::test]
async fn failed_shutdown_barrier_waits_for_durable_named_end() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let armed = Arc::new(AtomicBool::new(false));
    let model = world.lock().unwrap().library_snapshot();
    let w = world.clone();
    let fail = armed.clone();
    let library = model
        .open_with(
            &remanence_library::StaticAllowlist::new([LIBRARY]),
            move |path| {
                let role = w.lock().unwrap().role_for_path(path).unwrap();
                Ok::<Box<dyn SgTransport>, remanence_library::IoErrorKind>(Box::new(
                    FinalBarrierFailure {
                        inner: ModelTransport::new(w.clone(), role),
                        armed: fail.clone(),
                    },
                ))
            },
        )
        .unwrap();
    let lifecycle = DrivePoolLifecycle::default();
    let key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let files = MemoryDirectory::default();
    let _registration = register(root.path().join("journals/tapes"), Arc::new(files.clone()));
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
        b"pending shutdown",
    )
    .await;
    let (changer, _rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(key.clone(), tx)]),
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
    state.drive_pool = Some(pool.clone());
    let authority = TapeAuthority::open(
        root.path().join("journals/tapes"),
        TAPE,
        &root.path().join("state.sqlite"),
    )
    .unwrap();
    let target = remanence_state::authority::fixtures::publication_target(&authority);
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let wait_gate = gate.clone();
    let (entered, mut reached) = oneshot::channel();
    let entered = Mutex::new(Some(entered));
    let fail = armed.clone();
    files.observe(Some(Arc::new(move |event| {
        if let FileEvent::BeforeSync(path) = event {
            if target.matches(&path) && !fail.load(Ordering::SeqCst) {
                if let Some(entered) = entered.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    let (lock, wake) = &*wait_gate;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = wake.wait(released).unwrap();
                    }
                }
            }
        }
    })));
    armed.store(true, Ordering::SeqCst);
    let shutdown = tokio::spawn(async move { state.shutdown_drive_pool().await });
    tokio::time::timeout(StdDuration::from_secs(5), &mut reached)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !shutdown.is_finished(),
        "checkpoint error must still wait for ending fsync"
    );
    assert!(pool.drive_is_closing(&key));
    {
        let (lock, wake) = &*gate;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }
    let error = tokio::time::timeout(StdDuration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.message().contains("shutdown checkpoint failed"),
        "{error}"
    );
    thread.join().unwrap();
    files.observe(None);
    let snapshot = authority.freeze().unwrap();
    let summary = snapshot.tape_summary();
    assert!(summary.open_session.is_none());
    let end = summary.last_session_end.as_ref().unwrap();
    assert_eq!(end.reason, SessionEndReason::DriveError);
    assert_eq!(end.tail_file_count, 1);
    assert!(!pool.drive_is_closing(&key));
    let (tx, opened, thread) = open_writer(root.path(), open_model_library(world)).await;
    close_writer(
        &tx,
        opened.expect("ended tail requires no crash acknowledgement"),
        thread,
    )
    .await;
}

/// Durable final-close receipts survive both pre-end host failures from S5.
#[tokio::test]
async fn deferred_close_receipts_survive_lease_and_tail_cleanup_failures() {
    for action in [12, 13] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let (tx, opened, thread) = open_writer_configured(
            root.path(),
            open_model_library(world),
            selected(),
            |cfg, _| {
                cfg.io_memory
                    .epoch_test_action
                    .store(action, Ordering::Relaxed);
            },
        )
        .await;
        let id = opened.unwrap();
        let object = append_actor_test_file(
            &tx,
            id,
            root.path().join("object"),
            "object",
            "object",
            b"durable before host failure",
        )
        .await;
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        let closed = received
            .await
            .unwrap()
            .expect("durable commit must retain its successful reply");
        assert_eq!(closed.session.checkpointed_objects.len(), 1);
        assert_eq!(
            closed.session.checkpointed_objects[0].object_id,
            object.record.object_id
        );
        thread.join().unwrap();
        let records = native_records(root.path());
        assert!(records
            .iter()
            .any(|r| matches!(r, NativeRecord::Committed(_))));
        assert!(!records
            .iter()
            .any(|r| matches!(r, NativeRecord::SessionEnded(_))));
        let index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
        assert_eq!(
            index.list_active_tape_io_fences().unwrap()[0].reason,
            "AuthorityAmbiguous"
        );
    }
}

/// Refused commands cannot poison the reason of a subsequent host-side close failure.
#[tokio::test]
async fn failed_close_after_stale_timer_or_stray_abort_ends_failed_append_during_shutdown() {
    for stale_idle in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let lifecycle = DrivePoolLifecycle::default();
        let (tx, opened, thread) = open_writer_configured(
            root.path(),
            open_model_library(world),
            selected(),
            |cfg, _| {
                cfg.lifecycle = Some(lifecycle.clone());
                cfg.io_memory.epoch_test_action.store(9, Ordering::Relaxed);
            },
        )
        .await;
        let id = opened.unwrap();
        if stale_idle {
            tx.send(DriveCommand::TimerIdleClose {
                session_id: id,
                checkpoint_batch_id: Uuid::new_v4(),
            })
            .await
            .unwrap();
        } else {
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::Abort {
                session_id: Uuid::new_v4(),
                reason: None,
                reply,
            })
            .await
            .unwrap();
            assert_eq!(
                received.await.unwrap().unwrap_err().code(),
                tonic::Code::NotFound
            );
        }
        append_actor_test_file(
            &tx,
            id,
            root.path().join("object"),
            "object",
            "object",
            b"host failure",
        )
        .await;
        lifecycle.shutting_down.store(true, Ordering::SeqCst);
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
        assert!(received
            .await
            .unwrap()
            .unwrap_err()
            .message()
            .contains("injected post-projection failure"));
        thread.join().unwrap();
        assert!(native_records(root.path()).iter().any(|r| matches!(r, NativeRecord::SessionEnded(end) if end.reason == SessionEndReason::FailedAppend)));
    }
}
