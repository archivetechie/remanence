//! Publication regressions use real admission/recovery and durable host-log
//! completion, with a model actor reply and a barrier at operation publication.

use super::*;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

struct Fixture {
    _temp: tempfile::TempDir,
    state: ApiState,
    pool: crate::write_owner::DrivePool,
    admission: ManualFinalizeTapeAdmission,
    drive_rx: tokio::sync::mpsc::Receiver<crate::write_owner::DriveCommand>,
    _changer_rx: tokio::sync::mpsc::Receiver<crate::write_owner::ChangerCommand>,
}

fn fixture(automatic: bool) -> Fixture {
    const BLOCK_SIZE: u32 = 256 * 1024;
    let temp = tempfile::Builder::new()
        .prefix("remanence-startup-terminal-owner")
        .tempdir()
        .expect("tempdir");
    let index_path = temp.path().join("state.sqlite");
    let checkpoint_dir = temp.path().join("tapes");
    let tape_uuid = *Uuid::new_v4().as_bytes();
    let pool_id = "startup-owner-finalize";
    let (index, pool_config) = if automatic {
        let config =
            seed_automatic_after_replica_c(&index_path, tape_uuid, "OWN001L9", pool_id, BLOCK_SIZE);
        (CatalogIndex::open(&index_path).unwrap(), config)
    } else {
        let pool_config = TapePoolConfig {
            scheme: None,
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.92,
            watermark_high: 0.97,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK_SIZE),
            min_object_size_bytes: 0,
        };
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "OWN001L9".to_string(),
                block_size: BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign pool");
        let checkpoint = manual_finalize_checkpoint(tape_uuid, BLOCK_SIZE);
        remanence_state::authority::fixtures::AuthorityBuilder::new(&checkpoint_dir, tape_uuid)
            .build()
            .expect("open checkpoint journal")
            .append(&checkpoint)
            .expect("append checkpoint");
        index
            .project_checkpoint_record(&checkpoint)
            .expect("project checkpoint");

        (index, pool_config)
    };

    let mut state = ApiState::new_with_pool_configs(index, [pool_config]);
    state.default_library_serial = Some(Arc::new("LIB001".to_string()));
    let mut loaded = drive_bay(0x0101, true, Some("OWN001L9"));
    loaded.source_slot = None;
    state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
        crate::live_status::LibrarySnapshot {
            report: remanence_library::DiscoveryReport {
                unattached_drives: Vec::new(),
                complete: true,
                incomplete_evidence: Vec::new(),
                libraries: vec![test_library(vec![loaded], vec![])],
                warnings: Vec::new(),
            },
            captured_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    ))));
    let (drive_tx, drive_rx) = tokio::sync::mpsc::channel(2);
    let (changer_tx, changer_rx) = tokio::sync::mpsc::channel(1);
    let pool = crate::write_owner::DrivePool::new(
        changer_tx,
        std::collections::HashMap::from([(0x0101, drive_tx)]),
        Arc::new(std::collections::HashMap::from([(
            0x0101,
            AtomicBool::new(false),
        )])),
    );
    state.drive_pool = Some(pool.clone());
    let admission = ManualFinalizeTapeAdmission {
        candidate_operation_id: Uuid::new_v4(),
        actor: remanence_state::AuditActor::User("owner@example.invalid".to_string()),
        actor_fingerprint: "sha256:startup-owner-finalize".to_string(),
        idempotency_key: Uuid::from_u128(0xA904),
        request_fingerprint: [0xA9; 32],
        tape_uuid,
        expected_pool_id: Some(pool_id.to_string()),
        reason: "finalize across a restart".to_string(),
    };

    Fixture {
        _temp: temp,
        state,
        pool,
        admission,
        drive_rx,
        _changer_rx: changer_rx,
    }
}

/// Complete the model actor's physical tail using the native progress
/// records, then run the production host-only suffix to persist the seal.
fn complete_model_tail(
    state: &ApiState,
    request: &mut crate::write_owner::ManualFinalizeTapeActorRequest,
) -> crate::write_owner::ManualFinalizeTapeActorReply {
    use remanence_state::TerminalFinalizationProgress::*;
    let authority = remanence_state::TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        request.tape_uuid,
        state.index_path.as_ref(),
    )
    .unwrap();
    let mut lease = authority.acquire_exclusive_for_terminal_recovery().unwrap();
    for (before, after) in [
        (BeforeReplicaA, AfterReplicaA),
        (AfterReplicaA, AfterSeparationAb),
        (AfterSeparationAb, AfterReplicaB),
        (AfterReplicaB, AfterSeparationBc),
        (AfterSeparationBc, AfterReplicaC),
    ] {
        let current = lease.advance_terminal_finalization(before, after).unwrap();
        state
            .index_write()
            .unwrap()
            .project_terminal_finalization(remanence_state::TerminalFinalizationProjectionInput {
                tape_uuid: request.tape_uuid,
                trigger: current.trigger,
                operation_id: Some(request.candidate_operation_id),
                progress: current.progress,
                edition_digest: current.edition_digest,
                layout_digest: current.layout.layout_digest,
                outcome: remanence_state::TerminalFinalizationOutcome::InProgress,
                updated_at_utc: None,
            })
            .unwrap();
    }
    drop(lease);
    crate::write_owner::preflight_manual_finalize_tape(
        &mut state.index_write().unwrap(),
        crate::write_owner::ManualFinalizePreflightConfig {
            mounted_endpoint: None,
            checkpoint_journal_dir: state.checkpoint_journal_dir.as_ref(),
            audit_dir: state.audit_dir.as_ref(),
            audit_fsync: true,
            audit_append_lock: &state.audit_append_lock,
        },
        Some("OWN001L9"),
        request,
    )
    .unwrap()
    .expect("post-C host completion")
}

/// Bound both sides of the barrier; dropping the release sender also
/// unblocks a failed test so the runtime cannot hang during teardown.
fn pause_publication(
    state: &ApiState,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (published, observed) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    state.operations.after_terminal_for_test(move || {
        let _ = published.send(());
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    (observed, release)
}

async fn assert_sealed_refusals(f: &Fixture) {
    for target in [
        WriteSessionTarget::PinnedTape {
            tape_uuid: f.admission.tape_uuid,
            required_pool_id: f.admission.expected_pool_id.clone().unwrap(),
        },
        WriteSessionTarget::Pool {
            pool_id: f.admission.expected_pool_id.clone().unwrap(),
        },
    ] {
        let pool_target = matches!(target, WriteSessionTarget::Pool { .. });
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            open_write_session_reserved(
                remanence_state::AuditActor::System,
                &f.state,
                &f.pool,
                target,
                None,
            ),
        )
        .await
        .expect("sealed open refuses while publisher is paused")
        .unwrap_err();
        // A pinned open names the sealed tape; a pool whose only tape is sealed keeps its
        // long-standing "no writable tapes" code. Neither may answer Busy.
        let expected = if pool_target {
            tonic::Code::ResourceExhausted
        } else {
            tonic::Code::FailedPrecondition
        };
        assert_eq!(error.code(), expected, "{error}");
        assert_ne!(error.code(), tonic::Code::Aborted, "{error}");
        assert!(error.message().contains("sealed"), "{error}");
    }
    let error = crate::tape_closeout::start(
        &f.state,
        remanence_state::AuditActor::System,
        pb::CloseoutTapeRequest {
            tape_uuid: f.admission.tape_uuid.to_vec(),
            expected_pool_id: f.admission.expected_pool_id.clone(),
            idempotency_key: Some(pb::IdempotencyKey {
                value: Uuid::new_v4().as_bytes().to_vec(),
            }),
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{error}");
    assert!(error.message().contains("sealed"), "{error}");
}

async fn check_publication(worker_owned: bool, succeeds: bool) {
    let mut f = fixture(false);
    let mut recovery = None;
    manual_finalize_tape(&f.state, f.admission.clone())
        .await
        .unwrap();
    if worker_owned {
        let crate::write_owner::DriveCommand::FinalizeTape { reply, .. } =
            tokio::time::timeout(Duration::from_secs(5), f.drive_rx.recv())
                .await
                .unwrap()
                .unwrap()
        else {
            panic!("finalize command")
        };
        drop(reply);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(probe) = f.pool.reserve_tape(f.admission.tape_uuid) {
                    drop(probe);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let state = f.state.clone();
        let tape = f.admission.tape_uuid;
        let incarnation = remanence_state::TapeAuthority::open(
            state.checkpoint_journal_dir.as_ref(),
            tape,
            state.index_path.as_ref(),
        )
        .unwrap()
        .observe_summary()
        .unwrap()
        .incarnation;
        recovery = Some(tokio::spawn(async move {
            service_closeout_obligation(&state, tape, incarnation).await
        }));
    }
    let crate::write_owner::DriveCommand::FinalizeTape {
        mut request, reply, ..
    } = tokio::time::timeout(Duration::from_secs(5), f.drive_rx.recv())
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("finalize command")
    };
    assert!(crate::drive_pool::TapeBusy::from_status(
        &f.pool.check_finalizing(f.admission.tape_uuid).unwrap_err()
    )
    .is_some());
    let (published, release) = pause_publication(&f.state);
    if succeeds {
        let completed = complete_model_tail(&f.state, &mut request);
        reply.send(Ok(completed)).unwrap();
    } else {
        reply
            .send(Err(Status::internal("injected finalization failure")))
            .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), published)
        .await
        .unwrap()
        .unwrap();
    let status = f
        .state
        .operations
        .latest(&f.admission.candidate_operation_id)
        .unwrap();
    assert_eq!(
        status.state,
        if succeeds {
            pb::OperationState::Succeeded
        } else {
            pb::OperationState::Failed
        } as i32
    );
    // Check admissions first so the unfixed tree demonstrates the user-visible Busy.
    if succeeds {
        assert_sealed_refusals(&f).await;
    }
    f.pool
        .check_finalizing(f.admission.tape_uuid)
        .expect("terminal operation must have no finalizing marker");
    assert!(
        f.pool.mounted_tape_uuids().contains(&f.admission.tape_uuid),
        "worker still owns cleanup"
    );
    if succeeds {
        // Admission must also survive an independently stale marker.
        let stale = f
            .pool
            .begin_finalizing(f.admission.tape_uuid, Uuid::new_v4());
        assert_sealed_refusals(&f).await;
        drop(stale);
    }
    release.send(()).unwrap();
    if let Some(recovery) = recovery {
        assert_eq!(recovery.await.unwrap().is_ok(), succeeds);
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.pool.mounted_tape_uuids().contains(&f.admission.tape_uuid) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

/// The automatic recovery tail publishes in the outer worker rather
/// than in the nested manual worker; cover its success and failure too.
async fn check_automatic_publication(succeeds: bool) {
    let f = fixture(true);
    if !succeeds {
        rusqlite::Connection::open(f.state.index_path.as_ref()).unwrap().execute_batch(
            "create trigger fail_sealed_projection before update on tapes when new.state = 'sealed' begin select raise(abort, 'injected sealed projection failure'); end;"
        ).unwrap();
    }
    let (published, release) = pause_publication(&f.state);
    let state = f.state.clone();
    let tape = f.admission.tape_uuid;
    let incarnation = remanence_state::TapeAuthority::open(
        state.checkpoint_journal_dir.as_ref(),
        tape,
        state.index_path.as_ref(),
    )
    .unwrap()
    .observe_summary()
    .unwrap()
    .incarnation;
    let worker =
        tokio::spawn(async move { service_closeout_obligation(&state, tape, incarnation).await });
    tokio::time::timeout(Duration::from_secs(5), published)
        .await
        .unwrap()
        .unwrap();
    if succeeds {
        assert_sealed_refusals(&f).await;
    }
    f.pool
        .check_finalizing(tape)
        .expect("automatic terminal publication clears finalizing");
    release.send(()).unwrap();
    assert_eq!(worker.await.unwrap().is_ok(), succeeds);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_success_releases_finalizing_before_publication() {
    check_automatic_publication(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_failure_releases_finalizing_before_publication() {
    check_automatic_publication(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_owned_success_releases_finalizing_before_publication() {
    check_publication(false, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_owned_failure_releases_finalizing_before_publication() {
    check_publication(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_owned_success_releases_finalizing_before_publication() {
    check_publication(true, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_owned_failure_releases_finalizing_before_publication() {
    check_publication(true, false).await;
}

/// Poll the real read admission once so the test knows it reached the cleanup
/// hold, without timing assumptions or a detached request task.
async fn assert_read_waits_for_cleanup(
    read: std::pin::Pin<&mut impl std::future::Future<Output = Result<pb::ReadSession, Status>>>,
) {
    let mut read = read;
    let polled = std::future::poll_fn(|cx| std::task::Poll::Ready(read.as_mut().poll(cx))).await;
    assert!(
        polled.is_pending(),
        "read must wait for cleanup, not return Busy: {polled:?}"
    );
}

/// A model actor refusal proves read admission reached the drive. Physical read
/// correctness is covered by the characterisation suite's in-memory tape actor.
async fn finish_read_probe(
    f: &mut Fixture,
    read: impl std::future::Future<Output = Result<pb::ReadSession, Status>> + Send + 'static,
) {
    let read = tokio::spawn(read);
    let command = tokio::time::timeout(Duration::from_secs(5), f.drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::write_owner::DriveCommand::OpenRead { reply, .. } = command else {
        panic!("expected admitted read command");
    };
    reply
        .send(Err(Status::internal("model read reached actor")))
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.message(), "model read reached actor");
    assert_ne!(error.code(), tonic::Code::Aborted);
}

/// Verify uses its real reservation path and must also reach the model actor.
async fn assert_verify_admitted(f: &mut Fixture) {
    let state = f.state.clone();
    let tape = f.admission.tape_uuid;
    let handle = state
        .operations
        .register(Uuid::new_v4(), "verify_tape_index");
    let progress = crate::tape_verification::Progress::new(handle.clone(), tape);
    let verify =
        tokio::spawn(async move { verify_tape_index(&state, tape, handle, progress).await });
    let command = tokio::time::timeout(Duration::from_secs(5), f.drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::write_owner::DriveCommand::VerifyTapeIndex { reply, .. } = command else {
        panic!("expected admitted verify command");
    };
    reply
        .send(Ok(pb::TapeIndexVerification::default()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), verify)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

/// Scan while the request owns finalization, seal and release it, then dispatch
/// the stale entry through the production claim/service/debt-refresh path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sealed_request_finalization_is_not_readmitted_by_closeout_worker() {
    let mut f = fixture(false);
    let tape = f.admission.tape_uuid;
    let id = f.admission.candidate_operation_id;
    manual_finalize_tape(&f.state, f.admission.clone())
        .await
        .unwrap();
    let command = tokio::time::timeout(Duration::from_secs(5), f.drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::write_owner::DriveCommand::FinalizeTape {
        mut request, reply, ..
    } = command
    else {
        panic!("expected request-owned finalization");
    };
    crate::closeout_obligation::scan(&f.state).unwrap();
    f.state.closeout_work.assert_unattempted_for_test(tape);
    assert!(
        crate::closeout_obligation::service_unheld_for_test(&f.state)
            .await
            .is_empty()
    );
    let authority = remanence_state::TapeAuthority::open(
        f.state.checkpoint_journal_dir.as_ref(),
        tape,
        f.state.index_path.as_ref(),
    )
    .unwrap();
    let key = (tape, authority.observe_summary().unwrap().incarnation);
    let (published, release) = pause_publication(&f.state);
    reply
        .send(Ok(complete_model_tail(&f.state, &mut request)))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), published)
        .await
        .unwrap()
        .unwrap();
    assert!(authority.observe_summary().unwrap().sealed);
    assert!(f.state.closeout_work.contains(key));
    assert_operation_succeeded(&f).await;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.pool.mounted_tape_uuids().contains(&tape) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.state.operations.running_publications_for_test(&id), 1);
    let claimed = tokio::time::timeout(
        Duration::from_secs(5),
        crate::closeout_obligation::service_unheld_for_test(&f.state),
    )
    .await
    .unwrap();
    assert_eq!(claimed, vec![key]);
    assert_eq!(
        f.state.operations.running_publications_for_test(&id),
        1,
        "a sealed tape must never publish Running again"
    );
    f.pool
        .check_finalizing(tape)
        .expect("sealed tape has no finalizing marker");
    authority.check_finalizing().unwrap();
    assert_operation_succeeded(&f).await;
    assert!(
        !f.state.closeout_work.contains(key),
        "stale work-list entry was removed"
    );
    assert!(
        f.drive_rx.try_recv().is_err(),
        "no second actor finalization"
    );
    let read = open_read_session_critical(
        f.state.clone(),
        ReadSessionTarget::Tape { tape_uuid: tape },
        None,
    );
    finish_read_probe(&mut f, read).await;
    assert_verify_admitted(&mut f).await;
    assert_operation_succeeded(&f).await;
}

/// Use the RPC implementation, including its durable-history/live-state overlay.
async fn assert_operation_succeeded(f: &Fixture) {
    let operation = <crate::DaemonService as pb::daemon_server::Daemon>::get_operation(
        &f.state.daemon_service(),
        tonic::Request::new(pb::GetOperationRequest {
            operation_id: f.admission.candidate_operation_id.as_bytes().to_vec(),
        }),
    )
    .await
    .unwrap()
    .into_inner();
    assert_eq!(operation.state, pb::OperationState::Succeeded as i32);
}

/// Open a read exactly while terminal publication is paused and the request
/// still owns cleanup; it must wait and then reach the actor without Busy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_during_terminal_publication_waits_without_busy() {
    let mut f = fixture(false);
    manual_finalize_tape(&f.state, f.admission.clone())
        .await
        .unwrap();
    let command = tokio::time::timeout(Duration::from_secs(5), f.drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::write_owner::DriveCommand::FinalizeTape {
        mut request, reply, ..
    } = command
    else {
        panic!("expected finalization");
    };
    let (published, release) = pause_publication(&f.state);
    reply
        .send(Ok(complete_model_tail(&f.state, &mut request)))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), published)
        .await
        .unwrap()
        .unwrap();
    let mut read = Box::pin(open_read_session_critical(
        f.state.clone(),
        ReadSessionTarget::Tape {
            tape_uuid: f.admission.tape_uuid,
        },
        None,
    ));
    assert_read_waits_for_cleanup(read.as_mut()).await;
    assert_operation_succeeded(&f).await;
    release.send(()).unwrap();
    finish_read_probe(&mut f, read).await;
}
