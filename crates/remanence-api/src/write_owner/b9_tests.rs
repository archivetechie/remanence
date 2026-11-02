//! B9 receipt and accepted-tail regressions through the production actor and model transport.
use super::b6b_fix_tests::{native_records, selection};
use super::*;
use remanence_state::tape_log::{Record as NativeRecord, SessionEndReason};
use std::sync::atomic::Ordering;

#[tokio::test]
async fn b9_durable_checkpoint_receipts_survive_projection_failure() {
    for trigger in [
        CheckpointTrigger::Explicit,
        CheckpointTrigger::Append,
        CheckpointTrigger::Timer,
        CheckpointTrigger::Shutdown,
    ] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let lifecycle = DrivePoolLifecycle::default();
        let (tx, opened, thread) = open_writer_configured(
            root.path(),
            open_model_library(world),
            selected(),
            |cfg, _| {
                cfg.lifecycle = Some(lifecycle.clone());
                // Hook 10: a competing SQLite writer holds the database across the
                // projection, after the native Committed has been fsynced.
                cfg.io_memory.epoch_test_action.store(10, Ordering::Relaxed);
            },
        )
        .await;
        let id = opened.unwrap();
        let append = append_actor_test_file(
            &tx,
            id,
            root.path().join("object"),
            "object",
            "object",
            b"durable but unprojected",
        )
        .await;
        let (reply, received) = oneshot::channel();
        tx.send(DriveCommand::Checkpoint {
            session_id: id,
            trigger,
            expected_batch_id: None,
            reply: Some(reply),
        })
        .await
        .unwrap();
        let outcome = received.await.unwrap();
        // The native log already holds the Committed group naming the object.
        let committed = native_records(root.path())
            .iter()
            .filter(|r| matches!(r, NativeRecord::Committed(c) if c.object_count == 1))
            .count();
        assert_eq!(committed, 1, "the commit is durable in the tape log");
        thread.join().unwrap();
        let ended = lifecycle.ended_writes.lock().unwrap()[&id].0.clone();
        // Design write-API rule 2: a durable commit is never reported as an error,
        // and the ended session carries the receipts of every commit record.
        let checkpoint = outcome.unwrap_or_else(|status| {
        panic!(
            "durable commit reported as an error: {status}; ended session carries {} receipts; end reason {:?}",
            ended.checkpointed_objects.len(),
            native_records(root.path()).iter().find_map(|r| match r {
                NativeRecord::SessionEnded(e) => Some(e.reason),
                _ => None,
            })
        )
    });
        assert_eq!(checkpoint.committed_objects.len(), 1);
        assert_eq!(
            ended.checkpointed_objects.len(),
            usize::from(matches!(
                trigger,
                CheckpointTrigger::Append | CheckpointTrigger::Shutdown
            ))
        );
        assert_eq!(ended.objects_committed, 1);
        assert!(append.durable_receipt.lock().unwrap().is_some());
        assert!(native_records(root.path()).iter().any(|r| matches!(r, NativeRecord::SessionEnded(end) if end.reason == SessionEndReason::FailedAppend)));
    }
}

#[tokio::test]
async fn b9_buffered_tail_loss_reopens_without_supersede() {
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
    let pending = append_actor_test_file(
        &tx,
        id,
        root.path().join("pending"),
        "pending",
        "pending",
        b"acknowledged by the drive, still in its buffer",
    )
    .await;
    assert_eq!(
        pending.record.append_commit_info.unwrap().durability,
        pb::AppendDurability::Written as i32
    );
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: Some("client abort".into()),
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().unwrap();
    let records = native_records(root.path());
    let end = records
        .iter()
        .find_map(|r| match r {
            NativeRecord::SessionEnded(e) => Some(e.clone()),
            _ => None,
        })
        .expect("abort ends the session durably");
    assert_eq!(end.reason, SessionEndReason::Abort);
    assert!(
        end.tail_file_count >= 1,
        "the abort names the dispatched object"
    );
    // Parity-off: nothing is due, so no closeout and no synchronizing command
    // followed the abort. The object's records are acknowledged but buffered.
    let (frontier, total) = {
        let w = world.lock().unwrap();
        let tape = &w.tapes[BARCODE];
        (tape.durable_frontier, tape.records.len())
    };
    assert!(
        frontier < total,
        "the model keeps the aborted object buffered (frontier {frontier}, records {total})"
    );
    world.lock().unwrap().power_cut(BAY, frontier).unwrap();

    let (tx, opened, thread) =
        open_writer_with_selection(root.path(), open_model_library(world.clone()), selected).await;
    close_writer(&tx, opened.expect("lost buffered tail must reopen"), thread).await;
    assert!(CatalogIndex::open(root.path().join("state.sqlite"))
        .unwrap()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    assert!(native_records(root.path()).iter().any(
        |r| matches!(r, NativeRecord::SessionEnded(e) if e.proved_eod == Some(frontier as u64))
    ));
}

/// A threshold checkpoint is the append reply, so durable failure must return that object's receipt.
#[tokio::test]
async fn b9_threshold_append_returns_durable_receipt() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let lifecycle = DrivePoolLifecycle::default();
    let (tx, opened, thread) = open_writer_configured(
        root.path(),
        open_model_library(world),
        selected(),
        |cfg, _| {
            cfg.lifecycle = Some(lifecycle.clone());
            cfg.checkpoint_max_objects = 1;
            cfg.io_memory.epoch_test_action.store(10, Ordering::Relaxed);
        },
    )
    .await;
    let id = opened.unwrap();
    let receipt = append_actor_test_file(
        &tx,
        id,
        root.path().join("threshold"),
        "threshold",
        "threshold",
        b"durable threshold",
    )
    .await;
    assert_eq!(
        receipt.record.append_commit_info.unwrap().durability,
        pb::AppendDurability::Checkpointed as i32
    );
    thread.join().unwrap();
    let ended = &lifecycle.ended_writes.lock().unwrap()[&id].0;
    assert_eq!(ended.objects_committed, 1);
    assert_eq!(ended.checkpointed_objects.len(), 1);
    assert!(native_records(root.path()).iter().any(
        |r| matches!(r, NativeRecord::SessionEnded(e) if e.reason == SessionEndReason::FailedAppend)
    ));
}

/// Actual authorization precedes validation; every OperationControl role reaches it.
#[tokio::test]
async fn b9_closeout_authorizes_operation_control_roles() {
    use crate::pb::catalog_server::Catalog;
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    let state = super::b7_tests::state(root.path());
    for (role, allowed) in [
        ("operator", true),
        ("admin", true),
        ("orchestrator", true),
        ("system", true),
        ("readonly", false),
    ] {
        let mut request = tonic::Request::new(pb::CloseoutTapeRequest::default());
        request
            .metadata_mut()
            .insert("x-remanence-role", role.parse().unwrap());
        let error = state
            .catalog_service()
            .closeout_tape(request)
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            if allowed {
                tonic::Code::InvalidArgument
            } else {
                tonic::Code::PermissionDenied
            },
            "{role}"
        );
    }
}

/// A cold observer waiting on the native lock must leave a single runtime worker free.
#[tokio::test(flavor = "current_thread")]
async fn b9_cold_observation_lock_does_not_block_runtime() {
    let root = tempfile::tempdir().unwrap();
    super::b7_tests::due_tape(root.path()).await;
    let mut state = super::b7_tests::state(root.path());
    let cold = root.path().join("cold");
    std::fs::create_dir(&cold).unwrap();
    let name = format!("{}.remtape", Uuid::from_bytes(TAPE).simple());
    std::fs::copy(
        root.path().join("journals/tapes").join(&name),
        cold.join(&name),
    )
    .unwrap();
    state.checkpoint_journal_dir = Arc::new(cold);
    let authority = TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        TAPE,
        state.index_path.as_ref(),
    )
    .unwrap();
    authority.before_next_observation_replay(|| std::thread::sleep(StdDuration::from_millis(400)));
    let started = std::time::Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let state = state.clone();
        tasks.push(tokio::spawn(async move {
            use crate::pb::catalog_server::Catalog;
            state
                .catalog_service()
                .get_tape(tonic::Request::new(pb::GetTapeRequest {
                    tape_uuid: TAPE.to_vec(),
                }))
                .await
        }));
    }
    tokio::time::sleep(StdDuration::from_millis(40)).await;
    assert!(
        started.elapsed() < StdDuration::from_millis(200),
        "native lock blocked async runtime"
    );
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(
        authority.observation_replay_count(),
        1,
        "concurrent cold callers must coalesce on the existing per-tape guard"
    );
}

/// Shutdown refuses both worker reservation and host operation publication.
#[tokio::test]
async fn b9_shutdown_refuses_maintenance_before_publication() {
    let root = tempfile::tempdir().unwrap();
    super::b7_tests::due_tape(root.path()).await;
    let mut state = super::b7_tests::state(root.path());
    let (changer, _rx) = mpsc::channel(1);
    let pool =
        DrivePool::new_for_library(LIBRARY, changer, HashMap::new(), Arc::new(HashMap::new()));
    state.drive_pool = Some(pool.clone());
    state.begin_shutdown();
    assert!(pool.reserve_worker_tape(TAPE).is_err());
    let before = state.index().unwrap().list_operations().unwrap().len();
    assert!(crate::mount::service_closeout_obligation(&state, TAPE, 1)
        .await
        .is_err());
    assert!(crate::mount::closeout_worker_attempt(&state, TAPE, None)
        .await
        .is_err());
    assert!(crate::tape_closeout::start(
        &state,
        AuditActor::System,
        pb::CloseoutTapeRequest {
            tape_uuid: TAPE.to_vec(),
            expected_pool_id: None,
            idempotency_key: Some(pb::IdempotencyKey {
                value: Uuid::new_v4().as_bytes().to_vec()
            }),
        }
    )
    .is_err());
    use crate::pb::catalog_server::Catalog;
    assert!(state
        .catalog_service()
        .finalize_tape(tonic::Request::new(pb::FinalizeTapeRequest {
            tape_uuid: TAPE.to_vec(),
            expected_pool_id: Some(POOL.into()),
            reason: "after shutdown".into(),
            idempotency_key: Some(pb::IdempotencyKey {
                value: Uuid::new_v4().as_bytes().to_vec()
            }),
        }))
        .await
        .is_err());
    assert_eq!(
        state.index().unwrap().list_operations().unwrap().len(),
        before
    );
}

/// A never-written actor waits for an issued maintenance command and refuses the next one.
#[tokio::test]
async fn b9_fresh_actor_shutdown_waits_for_maintenance_settlement() {
    for kind in ["heartbeat", "closeout", "inventory", "finalize"] {
        fresh_maintenance_settlement(kind).await;
    }
}

async fn fresh_maintenance_settlement(kind: &str) {
    use super::b6c_tests::{CommandGate, GatedTransport};
    let root = tempfile::tempdir().unwrap();
    let world = super::b7_tests::due_tape(root.path()).await;
    if kind == "finalize" {
        let (tx, opened, thread) =
            open_writer(root.path(), open_model_library(world.clone())).await;
        close_writer(&tx, opened.unwrap(), thread).await;
    }
    let (entered, reached) = oneshot::channel();
    let gate = Arc::new(CommandGate {
        armed: AtomicBool::new(false),
        entered: Mutex::new(Some(entered)),
        release: (Mutex::new(false), std::sync::Condvar::new()),
        sidecar: false,
        opcode: Some(0),
        lost: false,
    });
    let model = world.lock().unwrap().library_snapshot();
    let w = world.clone();
    let g = gate.clone();
    let mut library = model
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
    let mut cfg = config(root.path(), &library);
    cfg.lifecycle = Some(lifecycle.clone());
    let memory = cfg.io_memory.clone();
    let drive = library
        .open_drive(BAY, &remanence_library::StaticAllowlist::new([LIBRARY]))
        .unwrap();
    let tx = spawn_drive_actor(BAY, drive, cfg);
    let (changer, _rx) = mpsc::channel(1);
    let key = DriveKey::new(LIBRARY, BAY);
    let reservations = Arc::new(HashMap::from([(key.clone(), AtomicBool::new(true))]));
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(key.clone(), tx.clone())]),
        reservations.clone(),
        lifecycle.clone(),
    );
    let mut state = super::b7_tests::state(root.path());
    state.drive_pool = Some(pool);
    gate.armed.store(true, Ordering::SeqCst);
    let (reply, mut received) = oneshot::channel();
    let (stream_tx, _stream_rx) = mpsc::channel(1);
    let command = match kind {
        "heartbeat" => DriveCommand::Heartbeat {
            ownership: None,
            drive_uuid: vec![1; 16],
            reply,
        },
        "closeout" => DriveCommand::CloseoutTape {
            permit: memory.try_reserve(64 * 1024 * 1024).unwrap(),
            progress: crate::closeout_progress::CloseoutProgress::new(Uuid::new_v4(), TAPE, None)
                .unwrap(),
            tape_uuid: TAPE,
            expected_pool: Some(POOL.into()),
            needs_drive_load: false,
            library_serial: LIBRARY.into(),
            barcode: Some(BARCODE.into()),
            source_slot: None,
            drive_serial: None,
            reply,
        },
        "inventory" => DriveCommand::TapeInventory {
            tape_uuid: TAPE,
            needs_drive_load: false,
            library_serial: LIBRARY.into(),
            barcode: Some(BARCODE.into()),
            source_slot: None,
            drive_serial: None,
            stream_tx,
            reply,
        },
        "finalize" => {
            let actor_fingerprint =
                crate::catalog_conversion::audit_actor_fingerprint(&AuditActor::System);
            let request_fingerprint =
                crate::catalog_conversion::manual_finalize_request_fingerprint(
                    TAPE,
                    Some(POOL),
                    &actor_fingerprint,
                    b"b9 settlement",
                );
            let (_, mut request) = crate::mount::prepare_manual_finalize_request(
                &state.index().unwrap(),
                crate::mount::ManualFinalizeTapeAdmission {
                    candidate_operation_id: Uuid::new_v4(),
                    actor: AuditActor::System,
                    actor_fingerprint,
                    idempotency_key: Uuid::new_v4(),
                    request_fingerprint,
                    tape_uuid: TAPE,
                    expected_pool_id: Some(POOL.into()),
                    reason: "b9 settlement".into(),
                },
                |id| state.pool_config(id),
            )
            .unwrap();
            assert!(super::super::preflight_manual_finalize_cancellable(
                &mut state.index_write().unwrap(),
                super::super::ManualFinalizePreflightConfig {
                    mounted_endpoint: None,
                    checkpoint_journal_dir: state.checkpoint_journal_dir.as_path(),
                    audit_dir: state.audit_dir.as_path(),
                    audit_fsync: state.audit_fsync,
                    audit_append_lock: &state.audit_append_lock,
                },
                Some(BARCODE),
                &mut request,
                None,
                || false,
            )
            .unwrap()
            .is_none());
            let (final_reply, final_result) =
                oneshot::channel::<Result<ManualFinalizeTapeActorReply, tonic::Status>>();
            tokio::spawn(async move {
                let _ = reply.send(final_result.await.unwrap().map(|_| ()));
            });
            DriveCommand::FinalizeTape {
                request,
                needs_drive_load: false,
                library_serial: LIBRARY.into(),
                barcode: Some(BARCODE.into()),
                source_slot: None,
                drive_uuid: None,
                drive_serial: None,
                reply: final_reply,
            }
        }
        _ => unreachable!(),
    };
    tx.send(command).await.unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), async {
        tokio::select! {
            reached = reached => reached.unwrap(),
            early = &mut received => panic!("{kind} failed before dispatch: {early:?}"),
        }
    })
    .await
    .expect(kind);
    lifecycle
        .shutdown_deadline
        .set(std::time::Instant::now())
        .unwrap();
    tokio::time::pause();
    let shutdown = tokio::spawn(async move { state.shutdown_drive_pool().await });
    tokio::task::yield_now().await;
    tokio::time::advance(StdDuration::from_secs(601)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    assert!(
        !shutdown.is_finished(),
        "shutdown abandoned issued {kind} after the old 600-second wait"
    );
    reservations[&key].store(false, Ordering::Release);
    gate.release();
    let _ = received.await.unwrap();
    tokio::time::timeout(StdDuration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let before = world.lock().unwrap().command_log.len();
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Heartbeat {
        ownership: None,
        drive_uuid: vec![1; 16],
        reply,
    })
    .await
    .unwrap();
    assert!(received.await.unwrap().is_err());
    assert_eq!(world.lock().unwrap().command_log.len(), before);
}

/// Timer delivery retains unclaimed receipts; Close returns and retains the same durable result.
#[tokio::test]
async fn b9_no_reply_timer_and_close_preserve_durable_projection_failure() {
    for close in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let lifecycle = DrivePoolLifecycle::default();
        let (tx, opened, thread) = open_writer_configured(
            root.path(),
            open_model_library(world),
            selected(),
            |cfg, _| {
                cfg.lifecycle = Some(lifecycle.clone());
                cfg.io_memory.epoch_test_action.store(10, Ordering::Relaxed);
            },
        )
        .await;
        let id = opened.unwrap();
        append_actor_test_file(
            &tx,
            id,
            root.path().join("pending"),
            "pending",
            "pending",
            b"durable",
        )
        .await;
        if close {
            let (reply, received) = oneshot::channel();
            tx.send(DriveCommand::Close {
                session_id: id,
                reply,
            })
            .await
            .unwrap();
            let closed = received.await.unwrap().unwrap();
            assert_eq!(closed.session.checkpointed_objects.len(), 1);
            assert_eq!(closed.session.objects_committed, 1);
        } else {
            tx.send(DriveCommand::Checkpoint {
                session_id: id,
                trigger: CheckpointTrigger::Timer,
                expected_batch_id: None,
                reply: None,
            })
            .await
            .unwrap();
        }
        thread.join().unwrap();
        let ended = &lifecycle.ended_writes.lock().unwrap()[&id].0;
        assert_eq!(ended.checkpointed_objects.len(), 1);
        assert_eq!(ended.objects_committed, 1);
        assert!(native_records(root.path()).iter().any(|r| matches!(r, NativeRecord::SessionEnded(e) if e.reason == SessionEndReason::FailedAppend)));
    }
}

/// Cancelling a health RPC cannot release ownership of its already queued command.
#[tokio::test]
async fn b9_queued_health_work_outlives_cancelled_requester() {
    let (changer, _rx) = mpsc::channel(1);
    let (drive, mut commands) = mpsc::channel(1);
    let pool = DrivePool::new_for_library(
        LIBRARY,
        changer,
        HashMap::from([(BAY, drive)]),
        Arc::new(HashMap::new()),
    );
    let caller = {
        let pool = pool.clone();
        tokio::spawn(async move {
            pool.poll_drive_health(&DriveKey::new(LIBRARY, BAY), vec![1; 16])
                .await
        })
    };
    let command = commands.recv().await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    pool.begin_shutdown();
    let waiting = tokio::spawn(async move { pool.wait_closeouts().await });
    tokio::time::sleep(StdDuration::from_millis(30)).await;
    assert!(
        !waiting.is_finished(),
        "queued health ownership was lost with its requester"
    );
    drop(command);
    tokio::time::timeout(StdDuration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
}
