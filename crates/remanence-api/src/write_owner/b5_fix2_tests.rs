//! Model-backed regressions for cleaning dismounts and cancelled read opens.

use super::*;
use crate::drive_pool::{DriveKey, DrivePoolLifecycle};

/// Inject shutdown before admission, or fail the first dispatched UNLOAD and
/// open the magazine after its retry, exercising the complete manual clean.
struct CleaningStopTransport {
    inner: ModelTransport,
    world: Arc<Mutex<VirtualWorld>>,
    lifecycle: DrivePoolLifecycle,
    before_unload: bool,
    unloads: Arc<AtomicU64>,
}

impl SgTransport for CleaningStopTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let result = self.inner.execute_in(cdb, buf);
        if cdb[0] == 0x4d && self.before_unload {
            // TapeAlert verification is the last drive read before UNLOAD.
            self.lifecycle.shutting_down.store(true, Ordering::SeqCst);
            self.lifecycle
                .cut_changer_wait
                .store(true, Ordering::SeqCst);
        }
        result
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        if cdb[0] == 0x1b && cdb[4] & 1 == 0 {
            let attempt = self.unloads.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                // Admission already succeeded. Its guard must survive this
                // failure and permit the retry even after shutdown begins.
                self.lifecycle.shutting_down.store(true, Ordering::SeqCst);
                self.lifecycle
                    .cut_changer_wait
                    .store(true, Ordering::SeqCst);
                return Err(remanence_library::ScsiError::Io(std::io::Error::other(
                    "first cleaning UNLOAD failed",
                )));
            }
            self.inner.execute_none(cdb)?;
            self.world.lock().unwrap().remove_magazine([0x400]);
            return Ok(());
        }
        self.inner.execute_none(cdb)
    }

    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_out(cdb, buf)
    }

    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

/// Exercise the real shutdown waiter while the manual cleaning worker owns MOVE.
async fn cleaning_shutdown(before_unload: bool) {
    let temp = tempfile::tempdir().unwrap();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "LIB-CLEAN-STOP",
        0x100,
        "DRV-CLEAN-STOP",
        0x400,
        1,
    )));
    {
        let mut world = world.lock().unwrap();
        world.put_tape_in_slot(
            0x400,
            "CLN001L9",
            VirtualTape {
                cleaning_cart: true,
                ..VirtualTape::default()
            },
        );
        world
            .drive_states
            .get_mut(&0x100)
            .unwrap()
            .unload_leaves_unthreaded = true;
    }
    let lifecycle = DrivePoolLifecycle::default();
    let unloads = Arc::new(AtomicU64::new(0));
    let snapshot = world.lock().unwrap().library_snapshot();
    let library = open_model_library(world.clone());
    let index_path = temp.path().join("state.sqlite");
    let mut index = CatalogIndex::open(&index_path).unwrap();
    let drive_uuid = index
        .observe_drive(DriveObservationInput {
            serial: "DRV-CLEAN-STOP".into(),
            identity_source: "DvcidAndInquiry".into(),
            vendor: None,
            product: None,
            firmware_rev: None,
            managed: "rem".into(),
            library_serial: Some("LIB-CLEAN-STOP".into()),
            element_address: Some(0x100),
            observed_at_utc: None,
        })
        .unwrap()
        .drive_uuid;
    let mut cfg = test_write_owner_config(
        index_path.clone(),
        temp.path().join("audit"),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    std::fs::create_dir_all(&cfg.audit_dir).unwrap();
    cfg.cleaning.min_cycle_duration = "0s".into();
    cfg.lifecycle = Some(lifecycle.clone());
    let board = crate::live_status::ChangerWaitBoard::default();
    cfg.changer_ready_wait =
        remanence_library::ChangerReadyWait::bounded(StdDuration::from_secs(60))
            .with_poll_interval(StdDuration::from_millis(10))
            .with_observer(board.observer())
            .with_stop_signal("daemon shutdown", lifecycle.cut_changer_wait.clone());
    let pool = DrivePool::new_with_lifecycle(
        HashMap::new(),
        HashMap::new(),
        Arc::new(HashMap::new()),
        lifecycle.clone(),
    );
    let mut api = crate::ApiState::new(CatalogIndex::open(&index_path).unwrap());
    api.drive_pool = Some(pool.clone());
    let cleaning_world = world.clone();
    let cleaning_unloads = unloads.clone();
    let cleaning = tokio::task::spawn_blocking(move || {
        // LibraryHandle's opener is thread-local; construct it in its worker.
        let mut library = snapshot
            .open_with(&cfg.policy, move |path| {
                let role = cleaning_world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(CleaningStopTransport {
                    inner: ModelTransport::new(cleaning_world.clone(), role),
                    world: cleaning_world.clone(),
                    lifecycle: lifecycle.clone(),
                    before_unload,
                    unloads: cleaning_unloads.clone(),
                }))
            })
            .unwrap();
        library.set_changer_ready_wait(cfg.changer_ready_wait.clone());
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive_uuid, "now")
    });
    if before_unload {
        let error = cleaning.await.unwrap().unwrap_err();
        assert!(
            error.message().contains("shutdown before UNLOAD"),
            "{error}"
        );
        assert_eq!(unloads.load(Ordering::SeqCst), 0);
        let world = world.lock().unwrap();
        assert_eq!(world.loaded_barcode(0x100), Some("CLN001L9"));
        assert!(!world.drive_states[&0x100].initializing_command_required);
    } else {
        tokio::time::timeout(StdDuration::from_secs(10), async {
            while board.get("LIB-CLEAN-STOP").is_none() {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("cleaning MOVE waits for open magazine");
        api.begin_shutdown();
        let closing = tokio::spawn(async move { api.shutdown_drive_pool().await });
        tokio::time::sleep(StdDuration::from_millis(200)).await;
        assert!(
            !closing.is_finished(),
            "shutdown must wait for cleaning MOVE"
        );
        assert!(pool.dismount_in_progress(&DriveKey::new("LIB-CLEAN-STOP", 0x100)));
        world.lock().unwrap().reinsert_magazine();
        tokio::time::timeout(StdDuration::from_secs(10), cleaning)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        tokio::time::timeout(StdDuration::from_secs(10), closing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(unloads.load(Ordering::SeqCst), 2);
        let world = world.lock().unwrap();
        assert_eq!(world.loaded_barcode(0x100), None);
        assert_eq!(world.slots[0].barcode.as_deref(), Some("CLN001L9"));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_clean_shutdown_before_unload_keeps_cartridge_seated() {
    cleaning_shutdown(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_clean_shutdown_after_unload_waits_for_retry_and_magazine_move() {
    cleaning_shutdown(false).await;
}

/// A lost open reply closes its audit lifetime and arms the real idle return.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_read_open_parks_audits_close_and_idle_returns_home() {
    let temp = tempfile::tempdir().unwrap();
    let (park_tx, park_rx) = mpsc::unbounded_channel();
    let lifecycle = DrivePoolLifecycle::with_timer_park_sender(park_tx);
    let tape_uuid = [0x79; 16];
    let (index_path, _, tx, serial, barcode, world) =
        read_harvest_world_with_lifecycle(&temp, tape_uuid, "RDH001L9", Some(lifecycle.clone()));
    let library = open_model_library(world.clone());
    let snapshot = library_snapshot_cell(library.library().clone());
    let mut cfg = test_write_owner_config(
        index_path.clone(),
        temp.path().join("audit"),
        &library,
        snapshot.clone(),
    );
    cfg.lifecycle = Some(lifecycle.clone());
    let changer = spawn_changer_actor(library.into_changer(), cfg);
    let key = DriveKey::new(&serial, 0x100);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(serial.clone(), changer)]),
        HashMap::from([(key.clone(), tx.clone())]),
        Arc::new(HashMap::from([(key.clone(), AtomicBool::new(false))])),
        lifecycle,
    );
    let mut api = crate::ApiState::new(CatalogIndex::open(&index_path).unwrap());
    api.drive_pool = Some(pool.clone());
    api.library_snapshot = Some(snapshot);
    api.audit_dir = Arc::new(temp.path().join("audit"));
    api.drive_idle_unload_seconds = 1;
    crate::mount::spawn_timer_idle_dismount_listener(api.clone(), park_rx);
    let (reply, received) = oneshot::channel();
    drop(received);
    tx.send(DriveCommand::OpenRead {
        tape_uuid,
        needs_drive_load: true,
        library_serial: serial,
        barcode: Some(barcode),
        source_slot: Some(0x400),
        drive_uuid: None,
        drive_serial: Some("DRV-READ-HARVEST".into()),
        resume_target: None,
        daemon_epoch: 1,
        reply,
    })
    .await
    .unwrap();
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::PrepareRemoval { reply })
        .await
        .unwrap();
    received.await.unwrap().unwrap();
    let parked = pool.parked_at(&key).expect("cancelled open parked");
    assert_eq!(parked.seated.tape_uuid, Some(tape_uuid));
    assert!(world.lock().unwrap().drive_bays[&0x100].is_some());
    let audit = FileAuditLog::replay(api.audit_dir.as_ref()).unwrap();
    let session_id = parked.seated.prior_session_id.unwrap();
    let events: Vec<_> = audit
        .iter()
        .filter(|record| record.session_id == Some(session_id))
        .map(|record| record.event.clone())
        .collect();
    assert!(events.contains(&AuditEvent::SessionOpened));
    assert!(events.contains(&AuditEvent::SessionClosed));
    tokio::time::timeout(StdDuration::from_secs(10), async {
        while world.lock().unwrap().loaded_barcode(0x100).is_some() {
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
    })
    .await
    .expect("idle timer returns cancelled read cartridge home");
    assert_eq!(
        world.lock().unwrap().slots[0].barcode.as_deref(),
        Some("RDH001L9")
    );
    api.shutdown_drive_pool().await.unwrap();
}
