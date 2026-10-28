//! Model-actor coverage for detached finalization admission and readiness.
use super::*;
use remanence_library::BlockSink;
use remanence_parity::{
    ParityError, PhysicalPositionHint, RawTapeSink, RawWriteOutcome, TapeFileJournal,
};

/// Count physical records while the real parity sink constructs a valid plan.
#[derive(Default)]
struct CounterSink(u64);
impl RawTapeSink for CounterSink {
    fn write_fixed_block(&mut self, bytes: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.0 += 1;
        Ok(RawWriteOutcome::WroteBlock {
            bytes_written: bytes.len() as u32,
            position_after: PhysicalPositionHint::new(self.0),
            early_warning: false,
            end_of_medium: false,
        })
    }
    fn write_filemarks(&mut self, count: u32, _: bool) -> Result<RawWriteOutcome, ParityError> {
        self.0 += u64::from(count);
        Ok(RawWriteOutcome::WroteFilemark {
            position_after: PhysicalPositionHint::new(self.0),
            early_warning: false,
            end_of_medium: false,
        })
    }
    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        Ok(PhysicalPositionHint::new(self.0))
    }
}

/// Keep sink bundles in memory so checkpoint annotations precede durable journal writes.
struct Bundles(Vec<CommittedBundle>);
impl TapeFileJournal for Bundles {
    fn load_committed(
        &self,
    ) -> Result<remanence_parity::CommittedState, remanence_parity::JournalError> {
        panic!("fixture only records new writes")
    }
    fn tape_uuid(&self) -> TapeUuid {
        [0x91; 16]
    }
    fn commit_bundle(
        &mut self,
        bundle: &CommittedBundle,
    ) -> Result<(), remanence_parity::JournalError> {
        self.0.push(bundle.clone());
        Ok(())
    }
}

fn planning_harness(
    in_slot: bool,
) -> (
    DismountHarness,
    ManualFinalizeTapeAdmission,
    remanence_parity::TerminalPrefixPlan,
) {
    let mut h = dismount_harness(3600);
    let uuid = [0x91; 16];
    let block_size = 262144;
    let scheme = remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_static("rs-cauchy-gf256-v1"),
        data_blocks_per_stripe: 128,
        parity_blocks_per_stripe: 4,
        stripes_per_neighborhood: 1,
    };
    let mut raw = CounterSink::default();
    let mut bundles = Bundles(Vec::new());
    let identity = remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"));
    let mut sink = remanence_parity::ParitySink::new_with_journal(
        &mut raw,
        &mut bundles,
        scheme.clone(),
        uuid,
        block_size,
        identity.clone(),
    )
    .unwrap();
    sink.write_bootstrap().unwrap();
    let runtime = sink.terminal_triple_capacity_runtime_state().unwrap();
    let input = remanence_parity::TerminalTripleCloseInput {
        projected_object_present: true,
        projected_object_blocks: 3,
        block_size_bytes: block_size,
        current_epoch_fill_blocks: 0,
        data_shards_per_epoch: 128,
        parity_shards_per_epoch: 4,
        pending_completed_sidecars: 0,
        sidecar_entries_before_object: 0,
        structural_entries_before_object: runtime.structural_entries_before_object,
        object_rows_before_object: 0,
        object_filemark_blocks: 1,
        sidecar_filemark_blocks: 1,
        parity_map_filemark_blocks: 1,
        replica_filemark_blocks: 1,
        gap_filemark_blocks: 1,
        gap_nominal_bytes: remanence_parity::DEFAULT_INDEX_SEPARATION_BYTES,
        safety_margin_blocks: 3,
        remaining_tape_blocks: 1_000_000,
        capacity_basis_blocks: 1_000_000 + runtime.used_tape_blocks,
        low_watermark_blocks: 0,
        high_watermark_blocks: 1,
        pending_completed_epoch_parity_bytes: 0,
        remaining_spool_bytes: u64::MAX,
    };
    sink.begin_object_with_terminal_triple_reservation(input.reserve_object().unwrap())
        .unwrap();
    for _ in 0..3 {
        sink.write_block(&vec![0x5a; block_size as usize]).unwrap();
    }
    let summary = sink.finish_object().unwrap();
    let plan = sink
        .plan_terminal_index_close(identity.capture().unwrap())
        .unwrap();
    drop(sink);
    let mut checkpoint = manual_finalize_checkpoint(uuid, block_size);
    checkpoint.scheme = Some(scheme.clone());
    checkpoint.eod_lba = raw.0;
    checkpoint.objects[0].copy.first_body_lba = summary.physical_start_lba.unwrap();
    checkpoint.objects[0].copy.first_parity_data_ordinal = Some(0);
    if let CheckpointObjectRecoveryRepresentation::Plaintext {
        manifest_first_chunk_lba,
        ..
    } = &mut checkpoint.objects[0].object_recovery_row.representation
    {
        *manifest_first_chunk_lba = summary.physical_start_lba.unwrap();
    }
    let mut object = summary.committed_bundle().unwrap();
    object.entries[0].object_id = Some(checkpoint.objects[0].object.object_id.clone());
    object.entries[0].object_recovery_row =
        Some(checkpoint.objects[0].object_recovery_row.to_parity_row());
    checkpoint.object_tape_file_bundles = vec![object.clone()];
    let mut journal = remanence_parity::FileTapeFileJournal::open(
        h.state
            .checkpoint_journal_dir
            .parent()
            .unwrap()
            .join(format!("{}.remjournal", crate::bytes_to_hex(&uuid))),
        uuid,
        block_size,
        scheme.clone(),
    )
    .unwrap();
    journal.commit_bundle(&bundles.0[0]).unwrap();
    journal.commit_bundle(&object).unwrap();
    journal
        .commit_bundle(&CommittedBundle {
            kind: CommittedBundleKind::CheckpointedThrough,
            entries: vec![],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 3,
        })
        .unwrap();
    drop(journal);
    remanence_state::FileCheckpointJournal::open(h.state.checkpoint_journal_dir.as_ref(), uuid)
        .unwrap()
        .append(&checkpoint)
        .unwrap();
    let mut index = h.state.index_write().unwrap();
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: uuid,
            voltag: "OLD001L9".into(),
            block_size,
            parity: ParityConfig::Scheme(scheme.clone()),
            force: false,
        })
        .unwrap();
    index
        .project_committed_tape_file_bundle(
            remanence_state::TapeJournalIndexInput {
                tape_uuid: uuid,
                block_size,
                scheme: Some(scheme),
                journal_offset_bytes: 0,
            },
            &bundles.0[0],
        )
        .unwrap();
    checkpoint.objects[0].fresh_tape = false;
    index.project_checkpoint_record(&checkpoint).unwrap();
    drop(index);
    h.seated.tape_uuid = Some(uuid);
    if in_slot {
        let mut snapshot = h.state.library_snapshot.as_ref().unwrap().write().unwrap();
        let mut report = snapshot.report.clone();
        report.libraries[0] = test_library(
            vec![drive_bay(0x0101, false, None)],
            vec![slot(0x0401, "OLD001L9")],
        );
        *snapshot = std::sync::Arc::new(crate::live_status::LibrarySnapshot {
            report,
            captured_at: snapshot.captured_at,
        });
    }
    let admission = ManualFinalizeTapeAdmission {
        candidate_operation_id: Uuid::new_v4(),
        actor: remanence_state::AuditActor::System,
        actor_fingerprint: "planning-test".into(),
        idempotency_key: Uuid::new_v4(),
        request_fingerprint: [0x91; 32],
        tape_uuid: uuid,
        expected_pool_id: None,
        reason: "close tape".into(),
    };
    (h, admission, plan)
}

fn spawn_plan(
    h: &DismountHarness,
    admission: ManualFinalizeTapeAdmission,
    budget: Duration,
) -> tokio::task::JoinHandle<Result<crate::write_owner::ManualFinalizeTapeResult, Status>> {
    let state = h.state.clone();
    tokio::spawn(async move { manual_finalize_tape_with_budget(&state, admission, budget).await })
}

struct HeldPlan {
    reply: oneshot::Sender<PlanningResult<remanence_parity::TerminalPrefixPlan>>,
    _ownership: std::sync::Arc<PlanningOwnership>,
}
impl HeldPlan {
    fn send(self, result: Result<remanence_parity::TerminalPrefixPlan, Status>) -> Result<(), ()> {
        self.reply
            .send(self._ownership.complete(result))
            .map_err(|_| ())
    }
}
async fn receive_plan(h: &mut DismountHarness) -> HeldPlan {
    match tokio::time::timeout(Duration::from_secs(3), h.drive_rx.recv())
        .await
        .unwrap()
        .unwrap()
    {
        crate::write_owner::DriveCommand::PlanFinalization {
            reply, ownership, ..
        } => HeldPlan {
            reply,
            _ownership: ownership,
        },
        _ => panic!("expected planning command"),
    }
}

async fn released(h: &DismountHarness, uuid: TapeUuid) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(guard) = h.pool.reserve_tape(uuid) {
                drop(guard);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn assert_unaccepted(h: &DismountHarness, admission: &ManualFinalizeTapeAdmission) {
    assert!(remanence_state::FileCheckpointJournal::open(
        h.state.checkpoint_journal_dir.as_ref(),
        admission.tape_uuid
    )
    .unwrap()
    .terminal_finalization_intent()
    .unwrap()
    .is_none());
    assert!(h
        .state
        .operations
        .latest(&admission.candidate_operation_id)
        .is_none());
    assert!(h
        .state
        .index()
        .unwrap()
        .idempotency_scope_record(
            &admission.actor_fingerprint,
            "finalize_tape",
            admission.idempotency_key
        )
        .unwrap()
        .is_none());
    assert_eq!(h.pool.parked.lock().unwrap().idle_timers.len(), 1);
}

/// A dropped unary request cannot release physical ownership during either actor await.
#[tokio::test]
async fn cancelled_planning_retains_owners_and_retry_is_accepted() {
    for abort_task in [false, true] {
        for during_move in [true, false] {
            let (mut h, admission, plan) = planning_harness(during_move);
            let (reply, _received) = oneshot::channel();
            let caller = if abort_task {
                let state = h.state.clone();
                let admission = admission.clone();
                tokio::spawn(async move {
                    manual_finalize_tape_critical(
                        &state,
                        admission,
                        &reply,
                        Duration::from_secs(900),
                    )
                    .await
                })
            } else {
                spawn_plan(&h, admission.clone(), Duration::from_secs(900))
            };
            if during_move {
                let crate::write_owner::ChangerCommand::PlanningMove {
                    reply, ownership, ..
                } = h.changer_rx.recv().await.unwrap()
                else {
                    panic!("expected changer move")
                };
                caller.abort();
                let _ = caller.await;
                assert!(h.pool.reserve_tape(admission.tape_uuid).is_err());
                assert!(h.pool.reserve_drive(&h.seated.drive_key()).is_err());
                assert!(h
                    .pool
                    .reserve_library_exclusive(&h.seated.library_serial)
                    .is_err());
                // The model changer publishes the new loaded location before replying.
                let mut snapshot = h.state.library_snapshot.as_ref().unwrap().write().unwrap();
                let mut loaded = drive_bay(0x0101, true, Some("OLD001L9"));
                loaded.source_slot = Some(0x0401);
                let mut report = snapshot.report.clone();
                report.libraries[0] = test_library(vec![loaded], vec![empty_slot(0x0401)]);
                *snapshot = std::sync::Arc::new(crate::live_status::LibrarySnapshot {
                    report,
                    captured_at: snapshot.captured_at,
                });
                drop(snapshot);
                ownership.moved();
                let _ = reply.send(ownership.complete(Ok(())));
            } else {
                let reply = receive_plan(&mut h).await;
                caller.abort();
                let _ = caller.await;
                assert!(h.pool.reserve_tape(admission.tape_uuid).is_err());
                assert!(h.pool.reserve_drive(&h.seated.drive_key()).is_err());
                assert!(h
                    .pool
                    .reserve_library_exclusive(&h.seated.library_serial)
                    .is_err());
                let _ = reply.send(Ok(plan.clone()));
            }
            released(&h, admission.tape_uuid).await;
            assert_unaccepted(&h, &admission);
            let retry = spawn_plan(&h, admission.clone(), Duration::from_secs(900));
            receive_plan(&mut h).await.send(Ok(plan)).unwrap();
            assert!(matches!(
                retry.await.unwrap().unwrap(),
                crate::write_owner::ManualFinalizeTapeResult::Accepted(_)
            ));
        }
    }
}

fn record_readiness(h: &DismountHarness, state: &str) -> Uuid {
    let id = Uuid::new_v4();
    h.state
        .index_write()
        .unwrap()
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id: id,
            run_id: None,
            endpoint: h.seated.drive_key(),
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: h.seated.barcode.clone(),
            source_slot: Some(h.seated.home_slot),
            media_generation: Some(9),
            phase: "session_open_short_probe".into(),
            state: state.into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    id
}

/// Every resumable readiness state retries planning and reaches durable acceptance.
#[tokio::test]
async fn planning_waits_for_each_resumable_readiness_state() {
    for state in [
        "planned",
        "pre_ready_loading",
        "media_initializing",
        "becoming_ready",
        "target_busy",
        "unit_attention",
    ] {
        let (mut h, admission, plan) = planning_harness(false);
        let caller = spawn_plan(&h, admission.clone(), Duration::from_secs(10));
        let reply = receive_plan(&mut h).await;
        let id = record_readiness(&h, state);
        reply
            .send(Err(
                crate::readiness_wait::ReadinessFence(id).status("planning drive is loading")
            ))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!caller.is_finished(), "{state}");
        assert!(h.pool.reserve_tape(admission.tape_uuid).is_err());
        assert_unaccepted(&h, &admission);
        h.state
            .index_write()
            .unwrap()
            .record_media_readiness_transition(
                crate::startup_media_readiness::startup_media_readiness_probe_transition(
                    id,
                    &remanence_library::MediaReadiness::Ready,
                ),
            )
            .unwrap();
        receive_plan(&mut h).await.send(Ok(plan)).unwrap();
        assert!(
            matches!(
                caller.await.unwrap().unwrap(),
                crate::write_owner::ManualFinalizeTapeResult::Accepted(_)
            ),
            "{state}"
        );
    }
}

/// A release fence, elapsed budget, shutdown, or dropped request ends planning without admission.
#[tokio::test]
async fn planning_wait_stops_without_acceptance() {
    for stop in [
        "release",
        "expired",
        "dropped",
        "task_aborted",
        "shutdown",
        "operator",
    ] {
        let (mut h, admission, _) = planning_harness(false);
        let budget = if stop == "expired" {
            Duration::from_millis(10)
        } else {
            Duration::from_secs(10)
        };
        let (response, _received) = oneshot::channel();
        let caller = if stop == "task_aborted" {
            let state = h.state.clone();
            let request = admission.clone();
            tokio::spawn(async move {
                manual_finalize_tape_critical(&state, request, &response, budget).await
            })
        } else {
            spawn_plan(&h, admission.clone(), budget)
        };
        let reply = receive_plan(&mut h).await;
        let id = record_readiness(
            &h,
            if stop == "release" {
                "timeout_unknown"
            } else {
                "becoming_ready"
            },
        );
        let refusal = crate::readiness_wait::ReadinessFence(id).status("original planning refusal");
        reply.send(Err(refusal.clone())).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        if !matches!(stop, "release" | "expired") {
            assert!(
                !caller.is_finished(),
                "resumable planning waits until {stop}"
            );
        }
        if matches!(stop, "dropped" | "task_aborted") {
            caller.abort();
            let _ = caller.await;
        } else {
            if stop == "shutdown" {
                h.state
                    .readiness_shutdown
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            if stop == "operator" {
                let mut transition =
                    crate::startup_media_readiness::startup_media_readiness_probe_transition(
                        id,
                        &remanence_library::MediaReadiness::BecomingReady {
                            ascq: 1,
                            media_initializing: false,
                        },
                    );
                transition.cancel_source = Some("operator".into());
                h.state
                    .index_write()
                    .unwrap()
                    .record_media_readiness_transition(transition)
                    .unwrap();
            }
            let error = tokio::time::timeout(Duration::from_secs(2), caller)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.code(), refusal.code(), "{stop}");
            assert_eq!(error.message(), refusal.message(), "{stop}");
            assert_eq!(
                crate::readiness_wait::ReadinessFence::from_status(&error)
                    .unwrap()
                    .0,
                id
            );
        }
        released(&h, admission.tape_uuid).await;
        assert_unaccepted(&h, &admission);
        assert!(h.drive_rx.try_recv().is_err());
    }
}

/// Dropping the reply during immutable planning cannot publish an operation.
#[tokio::test]
async fn dropped_during_acceptance_leaves_no_intent_and_same_key_replans() {
    for after_intent in [false, true] {
        let (mut h, admission, plan) = planning_harness(false);
        let (reply, received) = oneshot::channel();
        let state = h.state.clone();
        let request = admission.clone();
        let task = tokio::spawn(async move {
            crate::write_owner::set_manual_preflight_hook(after_intent, move || drop(received));
            manual_finalize_tape_critical(&state, request, &reply, Duration::from_secs(900)).await
        });
        receive_plan(&mut h).await.send(Ok(plan.clone())).unwrap();
        assert_eq!(
            task.await.unwrap().unwrap_err().code(),
            tonic::Code::Cancelled
        );
        released(&h, admission.tape_uuid).await;
        assert_unaccepted(&h, &admission);
        let retry = spawn_plan(&h, admission.clone(), Duration::from_secs(900));
        receive_plan(&mut h).await.send(Ok(plan)).unwrap();
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(accepted) =
            retry.await.unwrap().unwrap()
        else {
            panic!("accepted retry")
        };
        let rejoined = manual_finalize_tape(&h.state, admission).await.unwrap();
        let crate::write_owner::ManualFinalizeTapeResult::Accepted(rejoined) = rejoined else {
            panic!("accepted rejoin")
        };
        assert_eq!(rejoined.operation_id, accepted.operation_id);
    }
}

/// The completed planning reply still owns its lease until admission consumes it.
#[tokio::test]
async fn cancelled_after_plan_reply_before_acceptance_releases_with_cleanup() {
    for abort_task in [false, true] {
        let (mut h, admission, plan) = planning_harness(false);
        let (reply, received) = oneshot::channel();
        let state = h.state.clone();
        let request = admission.clone();
        let caller = tokio::spawn(async move {
            manual_finalize_tape_critical(&state, request, &reply, Duration::from_secs(900)).await
        });
        receive_plan(&mut h).await.send(Ok(plan.clone())).unwrap();
        // This current-thread test does not yield between reply and cancellation.
        assert!(h.pool.reserve_drive(&h.seated.drive_key()).is_err());
        assert!(h.pool.reserve_tape(admission.tape_uuid).is_err());
        if abort_task {
            caller.abort();
        } else {
            drop(received);
        }
        let outcome = caller.await;
        if abort_task {
            assert!(outcome.unwrap_err().is_cancelled());
        } else {
            assert_eq!(outcome.unwrap().unwrap_err().code(), tonic::Code::Cancelled);
        }
        released(&h, admission.tape_uuid).await;
        assert_unaccepted(&h, &admission);
        let retry = spawn_plan(&h, admission, Duration::from_secs(900));
        receive_plan(&mut h).await.send(Ok(plan)).unwrap();
        assert!(matches!(
            retry.await.unwrap().unwrap(),
            crate::write_owner::ManualFinalizeTapeResult::Accepted(_)
        ));
    }
}
