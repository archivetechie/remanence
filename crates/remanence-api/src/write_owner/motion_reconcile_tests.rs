//! MR-B: abandoned readiness LOADs close Unknown on both service wait ownership paths.

use super::*;
use remanence_library::motion::{MotionAuthority, MotionCommand, MotionContext, PendingMotion};
use remanence_library::outcome::CompletionEvidence;
use remanence_library::{transport::TransferOutcome, ScsiError};
use remanence_state::library_log as lib;
use std::sync::atomic::AtomicUsize;

/// Preserve the model's initializing-command request, then hold its loaded drive not ready.
struct InitializingDrive {
    inner: ModelTransport,
    polled: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

impl SgTransport for InitializingDrive {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }

    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let result = self.inner.execute_none(cdb);
        if self.inner.role() != remanence_chaos::model::DeviceRole::Changer
            && cdb.first() == Some(&0)
            && result.is_ok()
        {
            if let Some(polled) = self.polled.lock().unwrap().take() {
                let _ = polled.send(());
            }
            return Err(ScsiError::CheckCondition {
                sense: readiness_fixed_sense(2, 4, 1),
                bytes_transferred: 0,
            });
        }
        result
    }
}

/// The same LOAD must close exactly once, and any resolving read must follow its Unknown.
fn assert_abandoned_load(records: &[lib::Record], operation: Uuid, reconciled: bool) {
    let loads: Vec<_> = records
        .iter()
        .filter(|record| {
            record.motion_intent().is_some_and(|intent| {
                intent.parent_operation == Some(*operation.as_bytes())
                    && matches!(intent.motion, lib::Motion::Load(_))
            })
        })
        .collect();
    assert_eq!(loads.len(), 1, "one readiness LOAD was dispatched");
    let load = loads[0].envelope.record_uuid;
    let outcomes: Vec<_> = records
        .iter()
        .enumerate()
        .filter_map(|(position, record)| {
            record
                .motion_outcome()
                .filter(|outcome| outcome.intent.record_uuid == load)
                .map(|outcome| (position, outcome))
        })
        .collect();
    assert_eq!(outcomes.len(), 1, "the LOAD has exactly one outcome");
    assert_eq!(outcomes[0].1.result, lib::MotionResult::Unknown);
    if reconciled {
        let resolution = records
            .iter()
            .position(|record| {
                record.inventory_observation().is_some_and(|value| {
                    value
                        .resolves
                        .0
                        .iter()
                        .any(|reference| reference.record_uuid == load)
                })
            })
            .expect("InventoryReconciled resolves the abandoned LOAD");
        assert!(
            outcomes[0].0 < resolution,
            "Unknown precedes reconciliation"
        );
    }
}

/// A drive-actor cancellation relinquishes its initializing LOAD before terminal publication.
#[tokio::test]
async fn mr_b_cancelled_drive_wait_abandons_load_once() {
    const SERIAL: &str = "LIB-MRB-DRIVE";
    let mut typed = crate::tests::TypedRoot::open();
    let catalog = typed.catalog();
    let runtime = catalog.library_runtime().unwrap();
    let mut index = catalog.write_handle();
    let (world, _) = unthreaded_cartridge_world(SERIAL);
    let model = world.lock().unwrap().library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([SERIAL]);
    let (polled, reached) = oneshot::channel();
    let polled = Arc::new(Mutex::new(Some(polled)));
    let mut library = model
        .open_with(&policy, {
            let world = world.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(InitializingDrive {
                    inner: ModelTransport::new(world.clone(), role),
                    polled: polled.clone(),
                }))
            }
        })
        .unwrap();
    library.set_motion_authority(Arc::new(runtime.clone()));
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from([SERIAL.to_string()]),
    )
    .unwrap();
    let operation = Uuid::from_bytes(
        runtime
            .start_owned_operation(lib::OperationKind::MediaReadinessWait, AuditActor::System)
            .unwrap(),
    );
    index
        .record_media_readiness_operation(MediaReadinessOperationInput {
            operation_id: operation,
            run_id: None,
            endpoint: DriveKey::new(SERIAL, 0x100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("DRV-UNTHREADED".into()),
            barcode: Some("UNT001L9".into()),
            source_slot: Some(0x400),
            media_generation: Some(9),
            media_family: Some(LtoGen::Lto9),
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    runtime.set_motion_reconcile_notifier({
        let notifications = notifications.clone();
        move |serial| notifications.lock().unwrap().push(serial.to_string())
    });
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation, "media_readiness_wait");
    let mut drive = library.open_drive(0x100, &policy).unwrap();
    let wait = tokio::task::spawn_blocking(move || {
        handle_drive_wait_ready_and_release(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: 0x100,
                operation_id: operation,
                family: MediaFamily::Lto9OrLater,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::from_secs(5),
                    poll_interval: StdDuration::from_millis(10),
                },
                cartridge_in_drive: true,
                phase: "readiness_poll",
            },
            &handle,
            None,
        );
        (index, drive)
    });
    tokio::time::timeout(StdDuration::from_secs(5), reached)
        .await
        .expect("drive reached 02/04/01 after its initializing LOAD")
        .unwrap();
    registry.request_cancel(&operation).unwrap();
    let (index, mut drive) = tokio::time::timeout(StdDuration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        registry.latest(&operation).unwrap().state,
        pb::OperationState::Cancelled as i32
    );
    assert_eq!(drive_load_commands(&world), 1);
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![SERIAL],
        "saw the library once"
    );
    drive.abandon_readiness_loads().unwrap();
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![SERIAL],
        "the second drain is empty"
    );
    let mut changer = library.into_changer();
    assert_eq!(changer.reconcile_unowned_motion(), Ok(true));
    drop(changer);
    drop(drive);
    drop(index);
    drop(catalog);
    drop(runtime);
    let records = typed.records(|record| Some(record.clone())).await;
    assert_abandoned_load(&records, operation, true);
}

/// The failed polling-drive open drains its handed-off LOAD to Unknown exactly once.
#[tokio::test]
async fn mr_b_failed_polling_drive_open_abandons_handoff_once() {
    const SERIAL: &str = "LIB-MRB-HANDOFF";
    let mut typed = crate::tests::TypedRoot::open();
    let catalog = typed.catalog();
    let runtime = catalog.library_runtime().unwrap();
    let mut index = catalog.write_handle();
    let mut model = VirtualWorld::single_drive(SERIAL, 0x100, "DRV-MRB-HANDOFF", 0x400, 1);
    model.put_tape_in_slot(0x400, "MRBH01L9", VirtualTape::default());
    let world = Arc::new(Mutex::new(model));
    let model = world.lock().unwrap().library_snapshot();
    let opens = Arc::new(AtomicUsize::new(0));
    let mut library = model
        .open_with(&remanence_library::StaticAllowlist::new([SERIAL]), {
            let opens = opens.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                if role != remanence_chaos::model::DeviceRole::Changer
                    && opens.fetch_add(1, Ordering::SeqCst) == 1
                {
                    return Err(remanence_library::IoErrorKind {
                        kind: "PermissionDenied",
                        message: "injected second drive open failure".into(),
                        raw_os_error: None,
                    });
                }
                Ok(Box::new(ModelTransport::new(world.clone(), role)))
            }
        })
        .unwrap();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from([SERIAL.to_string()]),
    )
    .unwrap();
    let cfg = test_typed_write_owner_config(
        &typed,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let api = typed.api_state();
    let operation = api
        .accept_owner_request(
            AuditActor::System,
            "load_drive",
            "library",
            SERIAL.into(),
            [0xb4; 32],
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation, "load_drive");
    cfg.configure_operation_library(&mut library, &handle);
    let notifications = Arc::new(Mutex::new(Vec::new()));
    runtime.set_motion_reconcile_notifier({
        let notifications = notifications.clone();
        move |serial| notifications.lock().unwrap().push(serial.to_string())
    });
    let result = run_load_sequence(&mut index, &cfg, &handle, &mut library, 0x400, 0x100, true);
    assert!(result
        .unwrap_err()
        .contains("injected second drive open failure"));
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![SERIAL],
        "saw the library once"
    );
    library.abandon_handed_off_loads(0x100).unwrap();
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![SERIAL],
        "the second drain is empty"
    );
    drop(library);
    drop(cfg);
    drop(api);
    drop(index);
    drop(catalog);
    drop(runtime);
    let records = typed.records(|record| Some(record.clone())).await;
    assert_abandoned_load(&records, operation, false);
}

/// Shutdown clears queued flags without reading or resolving the unowned LOAD.
#[tokio::test]
async fn mr_b_fr1_shutdown_recheck_reads_nothing() {
    const SERIAL: &str = "LIB-MRB-FR1-SHUTDOWN";
    let mut typed = crate::tests::TypedRoot::open();
    let runtime = typed.catalog().library_runtime().unwrap();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        SERIAL,
        0x100,
        "DRV-MRB-FR1",
        0x400,
        3,
    )));
    let library = open_model_library(world.clone());
    let mut cfg = test_typed_write_owner_config(
        &typed,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let lifecycle = crate::drive_pool::DrivePoolLifecycle::default();
    lifecycle.shutting_down.store(true, Ordering::SeqCst);
    cfg.lifecycle = Some(lifecycle);
    let command = MotionCommand::Load(DriveKey::new(SERIAL, 0x100));
    let ticket = MotionAuthority::intent(&runtime, &command, &MotionContext::default()).unwrap();
    runtime
        .unconfirmed(&PendingMotion {
            command: command.clone(),
            context: MotionContext::default(),
            ticket,
            evidence: CompletionEvidence::success(),
        })
        .unwrap();
    let reads = world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .filter(|c| c.opcode == 0xb8)
        .count();
    let actor = spawn_changer_actor(library.into_changer(), cfg);
    let a = Arc::new(AtomicBool::new(true));
    let b = Arc::new(AtomicBool::new(true));
    for pending in [&a, &b] {
        assert!(actor
            .send(ChangerCommand::ReconcileMotion {
                pending: pending.clone()
            })
            .await
            .is_ok());
    }
    tokio::time::timeout(StdDuration::from_secs(5), async {
        while b.load(Ordering::SeqCst) {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
    })
    .await
    .expect("the second queued request was handled");
    assert!(!a.load(Ordering::SeqCst));
    assert_eq!(
        world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter(|c| c.opcode == 0xb8)
            .count(),
        reads,
        "no 0xB8 added during shutdown"
    );
    assert!(runtime.requires_reconciliation(&command));
    drop(actor);
    drop(runtime);
    let _records = typed.records(|record| Some(record.clone())).await;
}

/// Shutdown leaves the drive-owned LOAD parked until an explicit abandonment.
#[tokio::test]
async fn mr_b_fr1_shutdown_drive_wait_keeps_load_parked() {
    const SERIAL: &str = "LIB-MRB-FR1-DRIVE";
    let mut typed = crate::tests::TypedRoot::open();
    let catalog = typed.catalog();
    let runtime = catalog.library_runtime().unwrap();
    let mut index = catalog.write_handle();
    let (world, _) = unthreaded_cartridge_world(SERIAL);
    let model = world.lock().unwrap().library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([SERIAL]);
    let (polled, reached) = oneshot::channel();
    let polled = Arc::new(Mutex::new(Some(polled)));
    let mut library = model
        .open_with(&policy, {
            let world = world.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(InitializingDrive {
                    inner: ModelTransport::new(world.clone(), role),
                    polled: polled.clone(),
                }))
            }
        })
        .unwrap();
    library.set_motion_authority(Arc::new(runtime.clone()));
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from([SERIAL.to_string()]),
    )
    .unwrap();
    let operation = Uuid::from_bytes(
        runtime
            .start_owned_operation(lib::OperationKind::MediaReadinessWait, AuditActor::System)
            .unwrap(),
    );
    index
        .record_media_readiness_operation(MediaReadinessOperationInput {
            operation_id: operation,
            run_id: None,
            endpoint: DriveKey::new(SERIAL, 0x100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("DRV-UNTHREADED".into()),
            barcode: Some("UNT001L9".into()),
            source_slot: Some(0x400),
            media_generation: Some(9),
            media_family: Some(LtoGen::Lto9),
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    let notifications = Arc::new(Mutex::new(Vec::new()));
    runtime.set_motion_reconcile_notifier({
        let notifications = notifications.clone();
        move |serial| notifications.lock().unwrap().push(serial.to_string())
    });
    let registry = crate::operations::OperationRegistry::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let wake = Arc::new(crate::drive_pool::ReadinessWake::default());
    let handle = registry
        .register(operation, "media_readiness_wait")
        .with_readiness_lifetime(shutdown.clone(), wake.clone());
    let mut drive = library.open_drive(0x100, &policy).unwrap();
    let wait = tokio::task::spawn_blocking(move || {
        handle_drive_wait_ready_and_release(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: 0x100,
                operation_id: operation,
                family: MediaFamily::Lto9OrLater,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::from_secs(5),
                    poll_interval: StdDuration::from_millis(10),
                },
                cartridge_in_drive: true,
                phase: "readiness_poll",
            },
            &handle,
            None,
        );
        (index, drive)
    });
    tokio::time::timeout(StdDuration::from_secs(5), reached)
        .await
        .expect("drive reached 02/04/01 after its initializing LOAD")
        .unwrap();
    shutdown.store(true, Ordering::SeqCst);
    wake.notify.notify_waiters();
    let (index, mut drive) = tokio::time::timeout(StdDuration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        registry.latest(&operation).unwrap().state,
        pb::OperationState::Cancelled as i32
    );
    assert_eq!(
        registry
            .latest(&operation)
            .unwrap()
            .progress
            .get("phase")
            .map(String::as_str),
        Some("shutdown")
    );
    assert_eq!(drive_load_commands(&world), 1);
    assert!(
        notifications.lock().unwrap().is_empty(),
        "saw nothing during shutdown"
    );
    assert!(runtime.endpoint_motion_owned(&DriveKey::new(SERIAL, 0x100)));
    drive.abandon_readiness_loads().unwrap();
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![SERIAL],
        "the explicit drain notifies once"
    );
    drop(library);
    drop(drive);
    drop(index);
    drop(catalog);
    drop(runtime);
    let records = typed.records(|record| Some(record.clone())).await;
    assert_abandoned_load(&records, operation, false);
}

/// A polling-drive open failing at shutdown leaves the handed-off LOAD parked.
#[tokio::test]
async fn mr_b_fr1_shutdown_robotics_keeps_handoff_parked() {
    const SERIAL: &str = "LIB-MRB-FR1-HANDOFF";
    let mut typed = crate::tests::TypedRoot::open();
    let catalog = typed.catalog();
    let runtime = catalog.library_runtime().unwrap();
    let mut index = catalog.write_handle();
    let mut model = VirtualWorld::single_drive(SERIAL, 0x100, "DRV-MRB-HANDOFF", 0x400, 1);
    model.put_tape_in_slot(0x400, "MRBH01L9", VirtualTape::default());
    let world = Arc::new(Mutex::new(model));
    let model = world.lock().unwrap().library_snapshot();
    let shutdown = Arc::new(AtomicBool::new(false));
    let opens = Arc::new(AtomicUsize::new(0));
    let mut library = model
        .open_with(&remanence_library::StaticAllowlist::new([SERIAL]), {
            let opens = opens.clone();
            let shutdown = shutdown.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                if role != remanence_chaos::model::DeviceRole::Changer
                    && opens.fetch_add(1, Ordering::SeqCst) == 1
                {
                    shutdown.store(true, Ordering::SeqCst);
                    return Err(remanence_library::IoErrorKind {
                        kind: "PermissionDenied",
                        message: "injected second drive open failure".into(),
                        raw_os_error: None,
                    });
                }
                Ok(Box::new(ModelTransport::new(world.clone(), role)))
            }
        })
        .unwrap();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from([SERIAL.to_string()]),
    )
    .unwrap();
    let cfg = test_typed_write_owner_config(
        &typed,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let api = typed.api_state();
    let operation = api
        .accept_owner_request(
            AuditActor::System,
            "load_drive",
            "library",
            SERIAL.into(),
            [0xb4; 32],
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry
        .register(operation, "load_drive")
        .with_readiness_lifetime(
            shutdown.clone(),
            Arc::new(crate::drive_pool::ReadinessWake::default()),
        );
    cfg.configure_operation_library(&mut library, &handle);
    let notifications = Arc::new(Mutex::new(Vec::new()));
    runtime.set_motion_reconcile_notifier({
        let notifications = notifications.clone();
        move |serial| notifications.lock().unwrap().push(serial.to_string())
    });
    let result = run_load_sequence(&mut index, &cfg, &handle, &mut library, 0x400, 0x100, true);
    assert!(result
        .unwrap_err()
        .contains("injected second drive open failure"));
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    assert!(shutdown.load(Ordering::SeqCst));
    assert!(
        notifications.lock().unwrap().is_empty(),
        "saw nothing during shutdown"
    );
    library.abandon_handed_off_loads(0x100).unwrap();
    assert_eq!(
        *notifications.lock().unwrap(),
        vec![SERIAL],
        "the explicit drain notifies once"
    );
    drop(library);
    drop(cfg);
    drop(api);
    drop(index);
    drop(catalog);
    drop(runtime);
    let records = typed.records(|record| Some(record.clone())).await;
    assert_abandoned_load(&records, operation, false);
}

/// Forward only to the model, failing changer reads after a successful physical MOVE.
struct FailReadsAfterMove {
    inner: ModelTransport,
    moved: bool,
}

impl SgTransport for FailReadsAfterMove {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        if self.moved && cdb.first() == Some(&0xb8) {
            return Err(ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: vec![],
            });
        }
        self.inner.execute_in(cdb, buf)
    }

    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let result = self.inner.execute_none(cdb);
        if cdb.first() == Some(&0xa5) && result.is_ok() {
            self.moved = true;
        }
        result
    }
}

/// An unreadable confirmation raises the snapshot alarm, and the next MOVE refuses before dispatch.
#[tokio::test]
async fn mr_b_fr1_move_read_failures_raise_alarm_and_refuse_next_move() {
    const SERIAL: &str = "LIB-MRB-FR1-MOVE";
    let mut typed = crate::tests::TypedRoot::open();
    let catalog = typed.catalog();
    let mut model = VirtualWorld::single_drive(SERIAL, 0x100, "DRV-MRB-FR1", 0x400, 3);
    model.put_tape_in_slot(0x400, "MRBFR1A01L9", VirtualTape::default());
    model.put_tape_in_slot(0x401, "MRBFR1B01L9", VirtualTape::default());
    let world = Arc::new(Mutex::new(model));
    let snapshot = world.lock().unwrap().library_snapshot();
    let library = snapshot
        .open_with(&remanence_library::StaticAllowlist::new([SERIAL]), {
            let world = world.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(FailReadsAfterMove {
                    inner: ModelTransport::new(world.clone(), role),
                    moved: false,
                }))
            }
        })
        .unwrap();
    let cfg = test_typed_write_owner_config(
        &typed,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let actor = spawn_changer_actor(library.into_changer(), cfg);
    let (reply, result) = oneshot::channel();
    assert!(actor
        .send(ChangerCommand::Move {
            src: 0x400,
            dst: 0x402,
            after_unload: false,
            dismount_guard: None,
            recovery_guard: None,
            caller_gone: None,
            reply,
        })
        .await
        .is_ok());
    let error = tokio::time::timeout(StdDuration::from_secs(5), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.message().contains("physical confirmation failed"),
        "{error:?}"
    );
    let alarm = catalog
        .write_handle()
        .get_alarm(&library_snapshot_persist_alarm_key(SERIAL))
        .unwrap()
        .expect("failed after-read raises the library snapshot alarm");
    assert_eq!(alarm.kind, "snapshot-persist-failing");
    assert_eq!(alarm.state, "open");
    assert_eq!(
        world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter(|c| c.opcode == 0xa5)
            .count(),
        1
    );
    let (reply, result) = oneshot::channel();
    assert!(actor
        .send(ChangerCommand::Move {
            src: 0x401,
            dst: 0x400,
            after_unload: false,
            dismount_guard: None,
            recovery_guard: None,
            caller_gone: None,
            reply,
        })
        .await
        .is_ok());
    let error = tokio::time::timeout(StdDuration::from_secs(5), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(
        error.message().starts_with("reconcile library motion:"),
        "before-read refuses the second MOVE: {error:?}"
    );
    assert_eq!(
        world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter(|c| c.opcode == 0xa5)
            .count(),
        1,
        "the second MOVE sends no CDB"
    );
    drop(actor);
    drop(catalog);
    let _records = typed.records(|record| Some(record.clone())).await;
}
