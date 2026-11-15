//! MR-B: abandoned readiness LOADs close Unknown on both service wait ownership paths.

use super::*;
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

/// The library retains a successful waited LOAD if opening its polling drive fails.
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
