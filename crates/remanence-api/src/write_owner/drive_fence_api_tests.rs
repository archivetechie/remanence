use super::*;
use remanence_state::{library_log as lib, library_runtime::LibraryRuntime};

pub(super) fn raise(runtime: &LibraryRuntime, endpoint: &remanence_state::DriveKey) -> u64 {
    let endpoint = match endpoint {
        remanence_state::DriveKey::Bay {
            library_serial,
            bay,
        } => lib::DriveKey::Bay(lib::Bay {
            library_serial: library_serial.as_str().into(),
            bay: *bay,
        }),
        remanence_state::DriveKey::Standalone { drive_serial } => {
            lib::DriveKey::Standalone(lib::Standalone {
                drive_serial: drive_serial.as_str().into(),
            })
        }
    };
    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(endpoint),
                    cause: lib::FenceCause::Cleaning,
                    evidence: None,
                    at: None,
                    operation: None,
                    detail: lib::Bounded(vec![]),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap()
        .sequence
}

#[test]
fn l1r3e_session_open_drive_fence_precedes_probe_and_load() {
    for needs_drive_load in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (runtime, catalog) = runtime_backed_catalog(root.path());
        let mut index = catalog.write_handle();
        let (mut drive, log) = open_test_drive_with_tur_script("DEC418146K_LL02", vec![None]);
        raise(
            &runtime,
            &remanence_state::DriveKey::new("DEC418146K_LL02", 0x100),
        );
        log.borrow_mut().clear();
        let error = session_open_short_probe_or_load(
            &mut index,
            &mut drive,
            SessionOpenReadinessContext {
                action: "open write session",
                bay: 0x100,
                library_serial: "DEC418146K_LL02",
                barcode: Some("AOX030L9"),
                source_slot: Some(0x3eb),
                drive_serial: None,
                needs_drive_load,
                load_continues_move: false,
                lifecycle: None,
            },
        )
        .unwrap_err();
        assert!(error.message().starts_with("DriveFenced: "), "{error}");
        assert!(log.borrow().is_empty(), "no TUR or LOAD before refusal");
        assert!(index
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty());
    }
}

#[test]
fn l1r3e_mounted_drive_fence_and_motion_precedence() {
    use crate::tape_admission::test_support::{Fixture, T};
    for case in ["plain", "tape", "latch", "recovery"] {
        let mut fixture = Fixture::new();
        let mut drive = crate::tape_admission::test_support::drive();
        let cfg = fixture.config(&drive);
        let runtime = fixture.state.as_ref().unwrap().library_runtime();
        raise(&runtime, drive.endpoint().durable_key().unwrap());
        if case == "tape" {
            fixture.raise(T, lib::FenceCause::TransferError);
        }
        if case == "latch" {
            runtime.fence_motion_after_evidence_failure();
        }
        let index = fixture.index();
        let result = if case == "recovery" {
            cfg.mounted_recovery_ready(&index, &mut drive)
        } else {
            cfg.mounted_admission(&index, &drive)
        };
        let error = result.unwrap_err();
        if case == "latch" {
            assert!(error.message().contains("motion is fenced"), "{error}");
        } else {
            assert!(error.message().starts_with("DriveFenced: "), "{error}");
        }
        drop(index);
        drop(cfg);
        drop(runtime);
        drop(drive);
        let records = fixture.refusals();
        assert_eq!(records.len(), usize::from(case != "latch"));
        for d in records {
            assert!(d
                .detail
                .0
                .iter()
                .any(|e| e.0.as_str() == "code" && e.1 == lib::DiagnosticValue::Unsigned(12)));
        }
    }
}

#[tokio::test]
async fn l1r3e_finalize_drive_fence_precedes_prepare() {
    use crate::tape_admission::test_support::{Fixture, T};
    let mut fixture = Fixture::new();
    fixture.pool.watermark_high = 0.99;
    let mut world = VirtualWorld::single_drive("LIB-FINAL", 0x100, "DRV-FINAL", 0x400, 1);
    let bootstrap = crate::build_tape_bootstrap(
        T,
        262144,
        ParityConfig::None,
        "2026-11-11T00:00:00Z",
        "test",
    );
    let mut block = vec![0; 262144];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(18_000_000_000_000, 262144);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = 262144;
    world.put_tape_in_drive(0x100, "FINAL001L9", Some(0x400), tape);
    let world = Arc::new(Mutex::new(world));
    let mut library = open_model_library(world.clone());
    let state = fixture.state.as_ref().unwrap();
    let runtime = state.library_runtime();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut state.write_handle(),
        [library.library()],
        &HashSet::from([library.library().serial.clone()]),
    )
    .unwrap();
    let mut cfg = test_state_write_owner_config(
        state,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.checkpoint_max_objects = 1;
    let actor_drive = library.open_drive(0x100, &cfg.policy).unwrap();
    let tx = spawn_drive_actor(0x100, actor_drive, cfg.clone());
    let id = open_actor_test_write_session(
        &tx,
        &fixture.pool,
        T,
        "LIB-FINAL",
        "FINAL001L9",
        &[],
        "DRV-FINAL",
    )
    .await;
    append_actor_test_file_result(
        &tx,
        id,
        fixture.root.paths().state_dir.join("data"),
        "data",
        "data",
        b"finalize control",
    )
    .await
    .unwrap();
    let (reply, closed) = oneshot::channel();
    tx.send(DriveCommand::Close {
        session_id: id,
        reply,
    })
    .await
    .unwrap();
    closed.await.unwrap().unwrap();
    settle_write_actor(&tx, id).await;
    drop(tx);
    let mut drive = library.open_drive(0x100, &cfg.policy).unwrap();
    let mut index = fixture.index();
    let mut request = ManualFinalizeTapeActorRequest {
        candidate_operation_id: Uuid::new_v4(),
        actor: AuditActor::System,
        actor_fingerprint: "drive-fence-finalize".into(),
        idempotency_key: Uuid::new_v4(),
        request_fingerprint: [5; 32],
        tape_uuid: T,
        expected_pool_id: Some(fixture.pool.id.clone()),
        assignment_generation: index
            .get_tape_assignment_snapshot(&T)
            .unwrap()
            .unwrap()
            .assignment_generation,
        reason: "test".into(),
        block_size: 262144,
        parity_config: ParityConfig::None,
        pool_config: Some(fixture.pool.clone()),
    };
    assert!(preflight_manual_finalize_tape(
        &mut index,
        ManualFinalizePreflightConfig {
            authority: cfg.authority.clone(),
            mounted_endpoint: Some(drive.endpoint().durable_key().unwrap()),
            checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
        },
        None,
        &mut request
    )
    .unwrap()
    .is_none());
    raise(&runtime, drive.endpoint().durable_key().unwrap());
    let commands = world.lock().unwrap().command_log.len();
    let error = super::super::terminal_finalize::finalize_mounted_tape(
        &mut index,
        &super::super::terminal_types::TerminalConfig::terminal_config(&cfg),
        &mut drive,
        &request,
        None,
        |_, _| panic!("A3 prepare reached"),
        |_, _| panic!("A3 finish reached"),
    )
    .unwrap_err();
    assert_eq!(world.lock().unwrap().command_log.len(), commands);
    assert!(error.message().starts_with("DriveFenced: "), "{error}");
}

/// The existing actor/ModelTransport harness holds a heartbeat's TUR while OpenWrite
/// is already in that actor's mailbox. Publication happens before the held command ends.
#[tokio::test]
async fn l1r3e_queued_open_rechecks_drive_fence() {
    type TurHold = Arc<Mutex<Option<(oneshot::Sender<()>, std::sync::mpsc::Receiver<()>)>>>;
    struct HeldTur {
        inner: ModelTransport,
        hold: TurHold,
    }
    impl SgTransport for HeldTur {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_in(cdb, buf)
        }
        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError>
        {
            self.inner.execute_out(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
            if cdb[0] == 0 {
                if let Some((entered, release)) = self.hold.lock().unwrap().take() {
                    let _ = entered.send(());
                    release.recv_timeout(StdDuration::from_secs(10)).unwrap();
                }
            }
            self.inner.execute_none(cdb)
        }
        fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }
    let fixture = crate::tape_admission::test_support::Fixture::new();
    let mut world = VirtualWorld::single_drive("LIB-QUEUE", 0x100, "DRV-QUEUE", 0x400, 1);
    world.put_tape_in_drive(
        0x100,
        "QUEUE001L9",
        Some(0x400),
        VirtualTape::empty(1 << 30, 262144),
    );
    let world = Arc::new(Mutex::new(world));
    let model = world.lock().unwrap().library_snapshot();
    let hold = Arc::new(Mutex::new(None));
    let mut library = model
        .open_with(&remanence_library::StaticAllowlist::new(["LIB-QUEUE"]), {
            let world = world.clone();
            let hold = hold.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(HeldTur {
                    inner: ModelTransport::new(world.clone(), role),
                    hold: hold.clone(),
                }) as Box<dyn SgTransport>)
            }
        })
        .unwrap();
    let state = fixture.state.as_ref().unwrap();
    let runtime = state.library_runtime();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut state.write_handle(),
        [library.library()],
        &HashSet::from([library.library().serial.clone()]),
    )
    .unwrap();
    let cfg = test_state_write_owner_config(state, &library, library_snapshot_cell(model));
    let drive = library.open_drive(0x100, &cfg.policy).unwrap();
    let tx = spawn_drive_actor(0x100, drive, cfg);
    let (entered, waiting) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    *hold.lock().unwrap() = Some((entered, gate));
    let (reply, heartbeat) = oneshot::channel();
    tx.send(DriveCommand::Heartbeat {
        ownership: None,
        reply,
    })
    .await
    .unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let (reply, mut opened) = oneshot::channel();
    tx.send(DriveCommand::OpenWrite {
        actor: AuditActor::System,
        pool_cfg: fixture.pool.clone(),
        selected: SelectedTape {
            standalone_endpoint: None,
            pool_id: fixture.pool.id.clone(),
            tape_uuid: crate::tape_admission::test_support::T,
            block_size: 262144,
            parity_config: ParityConfig::None,
        },
        target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
        needs_drive_load: false,
        library_serial: "LIB-QUEUE".into(),
        barcode: Some("QUEUE001L9".into()),
        source_slot: None,
        drive_uuid: None,
        drive_serial: None,
        reply,
    })
    .await
    .unwrap();
    assert!(matches!(
        opened.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    let commands = world.lock().unwrap().command_log.len();
    raise(
        &runtime,
        &remanence_state::DriveKey::new("LIB-QUEUE", 0x100),
    );
    release.send(()).unwrap();
    heartbeat.await.unwrap().unwrap();
    let error = tokio::time::timeout(StdDuration::from_secs(5), opened)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.message().starts_with("DriveFenced: "), "{error}");
    assert_eq!(
        world.lock().unwrap().command_log.len(),
        commands + 1,
        "only the held heartbeat finishes"
    );
}
