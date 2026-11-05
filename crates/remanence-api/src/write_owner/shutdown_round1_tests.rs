//! Shutdown dispatch barriers exercise real model CDBs and durable projections.
use super::*;
use crate::drive_pool::{DaemonShuttingDown, DriveKey, DrivePoolLifecycle};

/// Share shutdown admission with an actual reserved drive and actor channels.
fn motion_pool(lifecycle: DrivePoolLifecycle) -> DrivePool {
    let (changer, _) = mpsc::channel(1);
    let (drive, _) = mpsc::channel(1);
    DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(DriveKey::new(LIBRARY, BAY), drive)]),
        Arc::new(HashMap::from([(
            DriveKey::new(LIBRARY, BAY),
            AtomicBool::new(false),
        )])),
        lifecycle,
    )
}

/// Put the catalogued cartridge back into its home slot without sending motion.
fn slot_tape(world: &Arc<Mutex<VirtualWorld>>) {
    let mut world = world.lock().unwrap();
    let barcode = world.drive_bays.get_mut(&BAY).unwrap().take().unwrap();
    world.drive_source_slots.insert(BAY, None);
    world.slots[0].full = true;
    world.slots[0].barcode = Some(barcode);
}

/// Stop at a real MOVE audit barrier after reservation and inventory preparation.
/// Also prove that a dispatched MOVE keeps ownership and completes SSC LOAD.
#[test]
fn shutdown_round1_reconcile_reserved_load_and_admitted_completion() {
    for dispatched in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        slot_tape(&world);
        let lifecycle = DrivePoolLifecycle::default();
        let pool = motion_pool(lifecycle.clone());
        pool.reserve_library_exclusive(LIBRARY).unwrap();
        let (entered, reached) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let worker_world = world.clone();
        let root_path = root.path().to_owned();
        let active = lifecycle.dismounts.clone();
        let worker = std::thread::spawn(move || {
            let mut library = open_model_library(worker_world);
            let mut cfg = config(&root_path, &library);
            cfg.lifecycle = Some(lifecycle);
            let mut fired = false;
            library.set_audit_hook(move |event| {
                let boundary = if dispatched {
                    matches!(event, remanence_library::AuditEvent::Finished { operation: remanence_library::AuditOp::Load { .. }, .. })
                } else {
                    matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0xa5)
                };
                if boundary && !fired {
                    fired = true;
                    entered.send(()).unwrap();
                    released.recv_timeout(StdDuration::from_secs(10)).unwrap();
                }
            });
            let mut index = CatalogIndex::open(root_path.join("state.sqlite")).unwrap();
            let registry = crate::operations::OperationRegistry::default();
            let handle = registry.register(Uuid::new_v4(), "reconcile");
            super::super::super::reconcile::reconcile_with_library(
                &mut index,
                &cfg,
                TAPE,
                handle,
                &mut library,
            );
        });
        reached.recv_timeout(StdDuration::from_secs(10)).unwrap();
        let owns_motion = !active.lock().unwrap().is_empty();
        pool.begin_shutdown();
        release.send(()).unwrap();
        worker.join().unwrap();
        let world = world.lock().unwrap();
        let commands: Vec<_> = world
            .command_log
            .iter()
            .filter(|c| matches!(c.opcode, 0xa5 | 0x1b))
            .map(|c| c.opcode)
            .collect();
        if dispatched {
            assert!(owns_motion, "MOVE owns shutdown tracking through LOAD");
            assert_eq!(commands, [0xa5, 0x1b]);
            assert_eq!(world.loaded_barcode(BAY), Some(BARCODE));
        } else {
            assert!(
                commands.is_empty(),
                "reconcile dispatched after shutdown: {commands:?}"
            );
            assert!(world.loaded_barcode(BAY).is_none());
        }
        assert!(active.lock().unwrap().is_empty());
    }
}

/// Hold the initializing-LOAD callback in the actual operation registry lock.
/// Shutdown must close admission while publication is blocked, then refuse LOAD.
#[test]
fn shutdown_round1_readiness_registry_barrier_refuses_load() {
    let root = tempfile::tempdir().unwrap();
    let lifecycle = DrivePoolLifecycle::default();
    let pool = motion_pool(lifecycle);
    let registry = crate::operations::OperationRegistry::default();
    let (entered, reached) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    registry.before_publish_for_test(move |status| {
        if status
            .progress
            .get("state")
            .is_some_and(|s| s == "pre_ready_loading")
        {
            entered.send(()).unwrap();
            released.recv_timeout(StdDuration::from_secs(10)).unwrap();
        }
    });
    let id = Uuid::new_v4();
    let handle = registry
        .register(id, "readiness")
        .with_readiness_motion(pool.clone(), DriveKey::new(LIBRARY, BAY));
    let index_path = root.path().join("state.sqlite");
    let worker = std::thread::spawn(move || {
        let (world, mut library) = unthreaded_cartridge_world(LIBRARY);
        let mut index = CatalogIndex::open(&index_path).unwrap();
        record_unthreaded_readiness_operation(&mut index, LIBRARY, id);
        let mut drive = library
            .open_drive(BAY, &remanence_library::StaticAllowlist::new([LIBRARY]))
            .unwrap();
        let result = poll_drive_media_readiness(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: BAY,
                operation_id: id,
                family: MediaFamily::Lto9OrLater,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::from_secs(2),
                    poll_interval: StdDuration::from_millis(1),
                },
                cartridge_in_drive: true,
                phase: "test",
            },
            &handle,
        );
        (result, drive_load_commands(&world))
    });
    reached.recv_timeout(StdDuration::from_secs(10)).unwrap();
    pool.begin_shutdown();
    release.send(()).unwrap();
    let (result, loads) = worker.join().unwrap();
    assert_eq!(
        loads, 0,
        "callback resumed after shutdown and dispatched LOAD"
    );
    assert!(matches!(result, Err(DriveReadinessError::Shutdown)));
    readiness_dispatched_load_finishes();
}

/// Cleaning refuses before any MOVE, cancels its durable run and leaves only
/// alarms/fences that existed before this attempt. Check both initial fence states.
#[test]
fn shutdown_round1_cleaning_refusal_restores_durable_state() {
    for (pre_fenced, late_cutoff) in [(false, false), (true, false), (false, true)] {
        let root = tempfile::tempdir().unwrap();
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            LIBRARY, BAY, DRIVE, 0x400, 1,
        )));
        world.lock().unwrap().put_tape_in_slot(
            0x400,
            "CLN001L9",
            VirtualTape {
                cleaning_cart: true,
                ..VirtualTape::default()
            },
        );
        let mut library = open_model_library(world.clone());
        let mut cfg = config(root.path(), &library);
        let lifecycle = DrivePoolLifecycle::default();
        let pool = motion_pool(lifecycle.clone());
        cfg.lifecycle = Some(lifecycle);
        let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
        let uuid = index
            .observe_drive(DriveObservationInput {
                serial: DRIVE.into(),
                identity_source: "DvcidAndInquiry".into(),
                vendor: None,
                product: None,
                firmware_rev: None,
                managed: "rem".into(),
                library_serial: Some(LIBRARY.into()),
                element_address: Some(i64::from(BAY)),
                observed_at_utc: None,
            })
            .unwrap()
            .drive_uuid;
        index.set_drive_fenced(&uuid, pre_fenced).unwrap();
        index
            .raise_alarm(
                "existing-cleaning-alarm",
                "cleaning-needs-operator",
                "warning",
                Some("existing evidence"),
            )
            .unwrap();
        if !late_cutoff {
            pool.begin_shutdown();
        }
        let stopping = pool.clone();
        library.set_audit_hook(move |event| {
            if matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0xa5)
            {
                // Real final library preparation, after this attempt raised its fence.
                stopping.begin_shutdown();
            }
        });
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), "cleaning");
        let result = run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &uuid, "now");
        assert!(result.is_err());
        assert!(DaemonShuttingDown::is_status(&result.unwrap_err()));
        assert_eq!(
            index.get_drive_by_uuid(&uuid).unwrap().unwrap().fenced,
            pre_fenced
        );
        assert!(index
            .get_active_clean_run_by_drive(&uuid)
            .unwrap()
            .is_none());
        assert_eq!(
            index
                .get_alarm("existing-cleaning-alarm")
                .unwrap()
                .unwrap()
                .state,
            "open"
        );
        for run in index.list_clean_runs(true).unwrap() {
            assert!(run.detail.unwrap().contains("cancelled"));
            assert_eq!(
                index
                    .get_alarm(&format!("cleaning-needs-operator:{}", run.run_id))
                    .unwrap()
                    .unwrap()
                    .state,
                "cleared"
            );
        }
        assert!(world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .all(|c| !matches!(c.opcode, 0xa5 | 0x1b)));
    }
}

/// Cut admission during the last inventory read before physical UNLOAD, after
/// the worker and actor have crossed their early checks.
struct StopAtUnloadInventory {
    inner: ModelTransport,
    pool: DrivePool,
    armed: Arc<AtomicBool>,
}
impl SgTransport for StopAtUnloadInventory {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        let result = self.inner.execute_in(cdb, buf);
        if cdb[0] == 0xb8 && self.armed.swap(false, Ordering::SeqCst) {
            self.pool.begin_shutdown();
        }
        result
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
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

/// Debt needs another cartridge evicted. Refusal at the physical UNLOAD must
/// cross the actor callback as typed shutdown and preserve the previous retry.
#[tokio::test]
async fn shutdown_round1_debt_eviction_preserves_retry_evidence() {
    let root = tempfile::tempdir().unwrap();
    let world = super::b7_tests::due_tape(root.path()).await;
    slot_tape(&world);
    {
        let mut world = world.lock().unwrap();
        world.slots.push(remanence_chaos::model::SlotState {
            address: 0x401,
            full: false,
            barcode: None,
            access: true,
            except: false,
        });
        world.put_tape_in_drive(BAY, "PARK001L9", Some(0x401), VirtualTape::default());
    }
    let lifecycle = DrivePoolLifecycle::default();
    let (changer, _changer_rx) = mpsc::channel(1);
    let (drive, mut drive_rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(DriveKey::new(LIBRARY, BAY), drive)]),
        Arc::new(HashMap::from([(
            DriveKey::new(LIBRARY, BAY),
            AtomicBool::new(false),
        )])),
        lifecycle.clone(),
    );
    let mut state = super::b7_tests::state(root.path());
    let library = open_model_library(world.clone());
    state.library_snapshot = Some(library_snapshot_cell(library.library().clone()));
    state.drive_pool = Some(pool.clone());
    crate::closeout_obligation::scan(&state).unwrap();
    let prior = "prior readiness evidence";
    state
        .closeout_work
        .seed_retry_reason_for_test((TAPE, 1), prior);
    let worker_state = state.clone();
    let worker = tokio::spawn(async move {
        crate::closeout_obligation::service_unheld_for_test(&worker_state).await
    });
    let command = tokio::time::timeout(StdDuration::from_secs(10), drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let DriveCommand::Unload { reply } = command else {
        panic!("expected debt eviction UNLOAD");
    };
    let armed = Arc::new(AtomicBool::new(false));
    let snapshot = world.lock().unwrap().library_snapshot();
    let factory_world = world.clone();
    let factory_armed = armed.clone();
    let factory_pool = pool.clone();
    let policy = remanence_library::StaticAllowlist::new([LIBRARY]);
    let mut library = snapshot
        .open_with(&policy, move |path| {
            let role = factory_world.lock().unwrap().role_for_path(path).unwrap();
            Ok(Box::new(StopAtUnloadInventory {
                inner: ModelTransport::new(factory_world.clone(), role),
                pool: factory_pool.clone(),
                armed: factory_armed.clone(),
            }))
        })
        .unwrap();
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    let before = world.lock().unwrap().command_log.len();
    armed.store(true, Ordering::SeqCst);
    let result = super::super::super::actor_runtime::unload_actor_drive(
        &mut drive,
        root.path(),
        Some(&lifecycle),
    );
    let typed = result.as_ref().is_err_and(DaemonShuttingDown::is_status);
    reply.send(result).unwrap();
    tokio::time::timeout(StdDuration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(pool.is_shutting_down(), "physical admission barrier ran");
    assert_eq!(
        state.closeout_work.retry_evidence_for_test((TAPE, 1)),
        (0, Some(prior.into()), false),
        "shutdown must not call failed or finish"
    );
    assert!(typed, "actor dropped remanence-shutdown metadata");
    assert!(crate::closeout_obligation::summary(&state, TAPE)
        .unwrap()
        .closeout_due());
    let world = world.lock().unwrap();
    assert!(world.command_log[before..]
        .iter()
        .all(|c| !matches!(c.opcode, 0xa5 | 0x1b)));
    assert_eq!(world.loaded_barcode(BAY), Some("PARK001L9"));
}

/// Operator LOAD has its own inventory and audit waits beyond actor admission.
#[test]
fn shutdown_round1_operator_load_rechecks_after_library_preparation() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    slot_tape(&world);
    let mut library = open_model_library(world.clone());
    let mut cfg = config(root.path(), &library);
    let lifecycle = DrivePoolLifecycle::default();
    let pool = motion_pool(lifecycle.clone());
    cfg.lifecycle = Some(lifecycle);
    pool.reserve_library_exclusive(LIBRARY).unwrap();
    library.set_audit_hook(move |event| {
        if matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0xa5) {
            pool.begin_shutdown();
        }
    });
    let mut index = CatalogIndex::open(&cfg.index_path).unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(Uuid::new_v4(), "load");
    let result = run_load_sequence(&mut index, &cfg, &handle, &mut library, 0x400, BAY, false);
    assert!(
        result.is_err(),
        "operator LOAD must refuse after final preparation"
    );
    assert!(world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .all(|c| !matches!(c.opcode, 0xa5 | 0x1b)));
}

/// A queued MOVE must recheck after actor-side inventory and audit preparation,
/// even when the queue dispatcher already acquired its motion guard.
#[tokio::test]
async fn shutdown_round1_actor_move_rechecks_after_audit_wait() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    slot_tape(&world);
    let mut library = open_model_library(world.clone());
    let mut cfg = config(root.path(), &library);
    let lifecycle = DrivePoolLifecycle::default();
    let pool = motion_pool(lifecycle.clone());
    cfg.lifecycle = Some(lifecycle);
    let admission = pool.begin_motion(&DriveKey::new(LIBRARY, BAY)).unwrap();
    library.set_audit_hook(move |event| {
        if matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0xa5) {
            pool.begin_shutdown();
        }
    });
    let actor =
        super::super::super::actor_runtime::spawn_changer_actor(library.into_changer(), cfg);
    let (reply, received) = oneshot::channel();
    actor
        .send(ChangerCommand::Move {
            src: 0x400,
            dst: BAY,
            after_unload: false,
            dismount_guard: Some(admission),
            recovery_guard: None,
            caller_gone: None,
            reply,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(StdDuration::from_secs(10), received)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.as_ref().is_err_and(DaemonShuttingDown::is_status),
        "actor dispatched MOVE after audit cutoff: {result:?}"
    );
    assert!(world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .all(|c| c.opcode != 0xa5));
}

/// Physical UNLOAD also has a blocking audit callback after inventory checks.
#[test]
fn shutdown_round1_unload_rechecks_after_audit_and_verification_waits() {
    for verification_wait in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = setup(root.path());
        let mut library = open_model_library(world.clone());
        let lifecycle = DrivePoolLifecycle::default();
        let pool = motion_pool(lifecycle.clone());
        let audit_pool = pool.clone();
        library.set_audit_hook(move |event| {
            if !verification_wait && matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0x1b) {
                audit_pool.begin_shutdown();
            }
        });
        let mut drive = library
            .open_drive(BAY, &remanence_library::StaticAllowlist::new([LIBRARY]))
            .unwrap();
        let armed = Arc::new(AtomicBool::new(false));
        let callback_armed = armed.clone();
        let _control = drive.verification_context().control(
            Arc::new(AtomicBool::new(false)),
            Arc::new(move |_, _, _| {
                if verification_wait && callback_armed.swap(false, Ordering::SeqCst) {
                    pool.begin_shutdown();
                }
            }),
        );
        armed.store(true, Ordering::SeqCst);
        let result = super::super::super::actor_runtime::unload_actor_drive(
            &mut drive,
            root.path(),
            Some(&lifecycle),
        );
        assert!(
            world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .all(|c| c.opcode != 0x1b),
            "UNLOAD dispatched after preparation cutoff"
        );
        assert!(
            result.as_ref().is_err_and(DaemonShuttingDown::is_status),
            "lost typed refusal: {result:?}"
        );
    }
}

/// The transport has accepted LOAD when it closes admission; the command must
/// still thread the cartridge and release its tracked ownership normally.
struct ShutdownOnLoad {
    inner: ModelTransport,
    pool: DrivePool,
}
impl SgTransport for ShutdownOnLoad {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        if cdb[0] == 0x1b && cdb[4] & 1 != 0 {
            self.pool.begin_shutdown();
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

fn readiness_dispatched_load_finishes() {
    let root = tempfile::tempdir().unwrap();
    let lifecycle = DrivePoolLifecycle::default();
    let pool = motion_pool(lifecycle.clone());
    let (world, _) = unthreaded_cartridge_world(LIBRARY);
    let snapshot = world.lock().unwrap().library_snapshot();
    let factory_world = world.clone();
    let factory_pool = pool.clone();
    let policy = remanence_library::StaticAllowlist::new([LIBRARY]);
    let mut library = snapshot
        .open_with(&policy, move |path| {
            let role = factory_world.lock().unwrap().role_for_path(path).unwrap();
            Ok(Box::new(ShutdownOnLoad {
                inner: ModelTransport::new(factory_world.clone(), role),
                pool: factory_pool.clone(),
            }))
        })
        .unwrap();
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    let mut index = CatalogIndex::open(root.path().join("state.sqlite")).unwrap();
    let id = Uuid::new_v4();
    record_unthreaded_readiness_operation(&mut index, LIBRARY, id);
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry
        .register(id, "readiness")
        .with_readiness_motion(pool.clone(), DriveKey::new(LIBRARY, BAY));
    let result = poll_drive_media_readiness(
        &mut index,
        &mut drive,
        DriveReadinessPoll {
            bay: BAY,
            operation_id: id,
            family: MediaFamily::Lto9OrLater,
            options: remanence_library::MediaReadinessWaitOptions {
                wait: true,
                timeout: StdDuration::from_secs(2),
                poll_interval: StdDuration::from_millis(1),
            },
            cartridge_in_drive: true,
            phase: "test",
        },
        &handle,
    );
    assert!(result.unwrap().readiness.is_ready());
    assert!(pool.is_shutting_down());
    assert_eq!(drive_load_commands(&world), 1);
    assert!(!world.lock().unwrap().drive_states[&BAY].initializing_command_required);
    assert!(lifecycle.dismounts.lock().unwrap().is_empty());
}

/// Real seated-cartridge debt must refuse its first physical command after cutoff.
#[tokio::test]
async fn regate1_debt_seated_conditional_load_after_cutoff() {
    debt_conditional_load_cutoff(false).await;
}

/// A real debt MOVE admitted before cutoff must complete its initial SSC LOAD.
#[tokio::test]
async fn regate1_dispatched_debt_move_completes_initial_load() {
    debt_conditional_load_cutoff(true).await;
}

/// Exercise the debt worker, real changer, and real drive actor with audit barriers.
async fn debt_conditional_load_cutoff(move_first: bool) {
    let root = tempfile::tempdir().unwrap();
    let world = super::b7_tests::due_tape(root.path()).await;
    if move_first {
        slot_tape(&world);
    } else {
        world.lock().unwrap().leave_cartridge_unthreaded(BAY);
    }
    let mut library = open_model_library(world.clone());
    let lifecycle = DrivePoolLifecycle::default();
    let cutoff_pool = motion_pool(lifecycle.clone());
    let barrier_world = world.clone();
    library.set_audit_hook(move |event| {
        if move_first
            && matches!(
                event,
                remanence_library::AuditEvent::Finished {
                    operation: remanence_library::AuditOp::Move { .. },
                    ..
                }
            )
        {
            // The model MOVE normally auto-threads; require the actor's LOAD.
            barrier_world
                .lock()
                .unwrap()
                .leave_cartridge_unthreaded(BAY);
            cutoff_pool.begin_shutdown();
        } else if !move_first
            && matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0x1b)
        {
            cutoff_pool.begin_shutdown();
        }
    });
    let mut cfg = config(root.path(), &library);
    cfg.lifecycle = Some(lifecycle.clone());
    cfg.reservations = Arc::new(HashMap::from([(
        DriveKey::new(LIBRARY, BAY),
        AtomicBool::new(false),
    )]));
    let mut drive = library.open_drive(BAY, &cfg.policy).unwrap();
    let changer = super::super::super::actor_runtime::spawn_changer_actor(
        library.into_changer(),
        cfg.clone(),
    );
    let (drive_tx, mut drive_rx) = mpsc::channel(1);
    let pool = DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        HashMap::from([(DriveKey::new(LIBRARY, BAY), drive_tx)]),
        cfg.reservations.clone(),
        lifecycle.clone(),
    );
    let mut state = super::b7_tests::state(root.path());
    state.library_snapshot = Some(library_snapshot_cell(
        world.lock().unwrap().library_snapshot(),
    ));
    state.drive_pool = Some(pool.clone());
    crate::closeout_obligation::scan(&state).unwrap();
    let before = world.lock().unwrap().command_log.len();
    let worker_state = state.clone();
    let worker = tokio::spawn(async move {
        crate::closeout_obligation::service_unheld_for_test(&worker_state).await
    });
    let mut command = tokio::time::timeout(StdDuration::from_secs(10), drive_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let DriveCommand::CloseoutTape {
        needs_drive_load,
        reply,
        ..
    } = &mut command
    else {
        panic!("expected native debt closeout");
    };
    assert_eq!(*needs_drive_load, move_first);
    let (actor_reply, actor_result) = oneshot::channel();
    let worker_reply = std::mem::replace(reply, actor_reply);
    let (tx, rx) = mpsc::channel(1);
    tx.send(command).await.unwrap();
    drop(tx);
    let (unused_actor_tx, _) = mpsc::channel(1);
    let actor = std::thread::spawn(move || {
        super::super::super::actor_runtime::drive_loop(BAY, &mut drive, cfg, unused_actor_tx, rx);
    });
    let result = tokio::time::timeout(StdDuration::from_secs(10), actor_result)
        .await
        .unwrap()
        .unwrap();
    let typed_refusal = result.as_ref().is_err_and(DaemonShuttingDown::is_status);
    worker_reply.send(result).unwrap();
    tokio::time::timeout(StdDuration::from_secs(10), worker)
        .await
        .unwrap()
        .unwrap();
    actor.join().unwrap();
    assert!(pool.is_shutting_down(), "audit barrier closed admission");
    assert!(lifecycle.dismounts.lock().unwrap().is_empty());
    let world = world.lock().unwrap();
    let motion: Vec<_> = world.command_log[before..]
        .iter()
        .filter(|c| matches!(c.opcode, 0xa5 | 0x1b))
        .map(|c| c.opcode)
        .collect();
    if move_first {
        assert_eq!(motion, [0xa5, 0x1b]);
        assert!(!world.drive_states[&BAY].initializing_command_required);
    } else {
        assert!(motion.is_empty(), "fresh LOAD after cutoff: {motion:?}");
        assert!(
            typed_refusal,
            "conditional LOAD must preserve typed shutdown"
        );
        assert!(crate::closeout_obligation::summary(&state, TAPE)
            .unwrap()
            .closeout_due());
        assert_eq!(
            state.closeout_work.retry_evidence_for_test((TAPE, 1)),
            (0, None, false)
        );
    }
    assert_eq!(world.loaded_barcode(BAY), Some(BARCODE));
}

/// Reopening the durable catalog restores only a conclusively refused allowance.
#[test]
fn regate1_refused_readiness_load_can_resume_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.sqlite");
    let (world, mut library) = unthreaded_cartridge_world(LIBRARY);
    let pool = motion_pool(DrivePoolLifecycle::default());
    let cutoff = pool.clone();
    library.set_audit_hook(move |event| {
        if matches!(event, remanence_library::AuditEvent::Started { cdb, .. } if cdb[0] == 0x1b) {
            cutoff.begin_shutdown();
        }
    });
    let mut drive = library
        .open_drive(BAY, &remanence_library::StaticAllowlist::new([LIBRARY]))
        .unwrap();
    let id = Uuid::new_v4();
    let mut index = CatalogIndex::open(&path).unwrap();
    record_unthreaded_readiness_operation(&mut index, LIBRARY, id);
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry
        .register(id, "readiness")
        .with_readiness_motion(pool, DriveKey::new(LIBRARY, BAY));
    let request = || DriveReadinessPoll {
        bay: BAY,
        operation_id: id,
        family: MediaFamily::Lto9OrLater,
        options: remanence_library::MediaReadinessWaitOptions {
            wait: true,
            timeout: StdDuration::from_secs(2),
            poll_interval: StdDuration::from_millis(1),
        },
        cartridge_in_drive: true,
        phase: "restart_test",
    };
    let result = poll_drive_media_readiness(&mut index, &mut drive, request(), &handle);
    assert!(matches!(result, Err(DriveReadinessError::Shutdown)));
    assert_eq!(drive_load_commands(&world), 0);
    drop(index);
    drop(drive);
    drop(library);
    let mut index = CatalogIndex::open(&path).unwrap();
    let rows = readiness_transition_rows(&path, id);
    assert!(
        rows.iter()
            .any(|r| r.1 == "pre_ready_loading" && r.2 == Some(0x1b)),
        "write-ahead evidence retained"
    );
    let resumed_budget =
        crate::startup_media_readiness::media_readiness_initializing_load_budget(&index, id)
            .unwrap();
    let mut library = open_model_library(world.clone());
    let mut drive = library
        .open_drive(BAY, &remanence_library::StaticAllowlist::new([LIBRARY]))
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(id, "readiness").with_readiness_motion(
        motion_pool(DrivePoolLifecycle::default()),
        DriveKey::new(LIBRARY, BAY),
    );
    let result = poll_drive_media_readiness(&mut index, &mut drive, request(), &handle).unwrap();
    assert!(
        result.readiness.is_ready(),
        "budget={resumed_budget}, loads={}, result={result:?}",
        drive_load_commands(&world)
    );
    assert_eq!(resumed_budget, 1);
    assert!(rows
        .iter()
        .any(|r| r.0.as_deref() == Some("initializing_load_refused")));
    assert_eq!(drive_load_commands(&world), 1);
    assert_eq!(
        index.media_readiness_operation(id).unwrap().unwrap().state,
        "ready"
    );
}

/// Dispatch or a crash after write-ahead must still exhaust the durable allowance.
#[test]
fn shutdown_round2_dispatched_and_unknown_loads_still_count_after_restart() {
    for dispatched in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.sqlite");
        let (world, mut library) = unthreaded_cartridge_world(LIBRARY);
        let mut drive = library
            .open_drive(BAY, &remanence_library::StaticAllowlist::new([LIBRARY]))
            .unwrap();
        let id = Uuid::new_v4();
        let mut index = CatalogIndex::open(&path).unwrap();
        record_unthreaded_readiness_operation(&mut index, LIBRARY, id);
        index
            .record_media_readiness_transition(
                crate::startup_media_readiness::media_readiness_initializing_load_transition(
                    id,
                    &MediaReadiness::BecomingReady {
                        ascq: 2,
                        media_initializing: true,
                    },
                ),
            )
            .unwrap();
        if dispatched {
            drive.load_immediate().unwrap();
            // Drive again needs initialization without any durable ready observation.
            world.lock().unwrap().leave_cartridge_unthreaded(BAY);
        }
        drop(index);
        let mut index = CatalogIndex::open(&path).unwrap();
        assert_eq!(
            crate::startup_media_readiness::media_readiness_initializing_load_budget(&index, id)
                .unwrap(),
            0
        );
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(id, "readiness").with_readiness_motion(
            motion_pool(DrivePoolLifecycle::default()),
            DriveKey::new(LIBRARY, BAY),
        );
        let result = poll_drive_media_readiness(
            &mut index,
            &mut drive,
            DriveReadinessPoll {
                bay: BAY,
                operation_id: id,
                family: MediaFamily::Lto9OrLater,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: true,
                    timeout: StdDuration::from_millis(20),
                    poll_interval: StdDuration::from_millis(1),
                },
                cartridge_in_drive: true,
                phase: "unknown_test",
            },
            &handle,
        )
        .unwrap();
        assert!(result.timed_out);
        assert_eq!(drive_load_commands(&world), usize::from(dispatched));
    }
}
