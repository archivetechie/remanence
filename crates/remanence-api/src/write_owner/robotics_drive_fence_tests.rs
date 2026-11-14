//! A7 executes the real robotics body with a model device opener.
use super::*;
use crate::write_owner::WriteAdmissionCoordinator;
use remanence_chaos::model::{ModelTransport, VirtualTape, VirtualWorld};
use remanence_library::{DiscoveryReport, Library};
use remanence_state::{library_runtime::LibraryRuntime, StateLockGuard, StatePaths};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Mutex,
};
use uuid::Uuid;

fn open_model_library(
    world: std::sync::Arc<std::sync::Mutex<VirtualWorld>>,
) -> remanence_library::LibraryHandle {
    let library = world.lock().expect("world lock").library_snapshot();
    let policy = remanence_library::StaticAllowlist::new([library.serial.as_str()]);
    library
        .open_with(&policy, move |path| {
            let role = world
                .lock()
                .expect("world lock")
                .role_for_path(path)
                .expect("known model path");
            Ok(Box::new(ModelTransport::new(
                std::sync::Arc::clone(&world),
                role,
            )))
        })
        .expect("open model library")
}

fn test_write_owner_config(
    index_path: PathBuf,
    audit_dir: PathBuf,
    library: &remanence_library::LibraryHandle,
    library_snapshot: Arc<RwLock<Arc<crate::LibrarySnapshot>>>,
) -> WriteOwnerConfig {
    let serial = library.library().serial.clone();
    let checkpoint_journal_dir = index_path.parent().unwrap().join("tapes");
    std::fs::create_dir_all(&checkpoint_journal_dir).unwrap();
    WriteOwnerConfig {
        authority: crate::write_owner::TerminalAuthority::fixture(
            None,
            &audit_dir,
            false,
            &Arc::new(std::sync::Mutex::new(())),
        ),
        catalog: remanence_state::projector::open(&(index_path)).unwrap(),
        index_path,
        report: DiscoveryReport {
            unattached_drives: Vec::new(),
            complete: true,
            incomplete_evidence: Vec::new(),
            libraries: vec![library.library().clone()],
            warnings: Vec::new(),
        },
        policy: remanence_library::StaticAllowlist::new([serial.as_str()]),
        reservations: Arc::new(HashMap::new()),
        placement: crate::write_owner::ActorPlacement::Library(
            crate::write_owner::LibraryActorContext {
                serial: serial.clone(),
                snapshot: library_snapshot,
            },
        ),
        snapshot_miss_alarm: 1,
        managed_library_serials: Arc::new(HashSet::from([serial])),
        cleaning: remanence_state::CleaningConfig::default(),
        tape_io: remanence_state::TapeIoConfig::default(),
        tape_pools: Vec::new(),
        verification_route: remanence_parity::VerificationRoute::Walk,
        io_memory: crate::io_memory::IoMemoryReservation::new(
            remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
        )
        .expect("test I/O memory manager"),
        write_admissions: WriteAdmissionCoordinator::default(),
        checkpoint_journal_dir,
        checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
        checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
        checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
        session_idle_seconds: 1800,
        lifecycle: Some(crate::drive_pool::DrivePoolLifecycle::default()),
        changer_ready_wait: remanence_library::ChangerReadyWait::default(),
        calibration_store: remanence_state::CalibrationControlStore::open(
            std::env::temp_dir().join(format!("rem-calibration-tests-{}", Uuid::new_v4())),
        )
        .expect("open test calibration store"),
    }
}

fn library_snapshot_cell(library: Library) -> Arc<RwLock<Arc<crate::LibrarySnapshot>>> {
    Arc::new(RwLock::new(Arc::new(crate::LibrarySnapshot {
        report: DiscoveryReport {
            unattached_drives: Vec::new(),
            complete: true,
            incomplete_evidence: Vec::new(),
            libraries: vec![library],
            warnings: Vec::new(),
        },
        captured_at: OffsetDateTime::UNIX_EPOCH,
    })))
}

/// A drive fence refuses at the typed motion gate, before the executor can send MOVE MEDIUM.
#[test]
fn robotics_move_to_drive_fenced_bay_records_one_refusal_without_move() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let paths = StatePaths {
        config_path: root.join("config"),
        state_dir: root.join("state"),
        audit_dir: root.join("library"),
        journal_dir: root.join("native"),
        sqlite_path: root.join("catalog.sqlite"),
        tape_cache_dir: root.join("cache"),
        calibration_dir: root.join("calibration"),
    };
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
    let catalog = remanence_state::projector::open(&paths.sqlite_path)
        .unwrap()
        .with_library_runtime(runtime.clone());
    let mut index = catalog.write_handle();
    let mut model = VirtualWorld::single_drive("LIB-CLEAN", 0x100, "DRV-CLEAN", 0x400, 1);
    model.put_tape_in_slot(
        0x400,
        "CLNU01L9",
        VirtualTape {
            cleaning_cart: false,
            cleaning_cart_expired: false,
            ..VirtualTape::default()
        },
    );
    let world = Arc::new(Mutex::new(model));
    let library = open_model_library(world.clone());
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from(["LIB-CLEAN".into()]),
    )
    .unwrap();
    let mut cfg = test_write_owner_config(
        paths.sqlite_path.clone(),
        paths.audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.catalog = catalog.clone();
    cfg.authority = crate::write_owner::TerminalAuthority::Typed(runtime.clone());
    cfg.cleaning.min_cycle_duration = "0s".into();
    let api = crate::ApiState::new(catalog.clone());
    let action = RoboticsAction::Move {
        src: 0x400,
        dst: 0x100,
    };
    let fingerprint = crate::operation_source::robotics_fingerprint("LIB-CLEAN", &action);
    let operation = api
        .accept_owner_request(
            AuditActor::User("real-cleaner".into()),
            "move_medium",
            "library",
            "LIB-CLEAN".into(),
            fingerprint,
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation, "move_medium");

    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    detail: lib::Bounded(vec![]),
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(lib::DriveKey::Bay(lib::Bay {
                        library_serial: "LIB-CLEAN".into(),
                        bay: 0x100,
                    })),
                    cause: lib::FenceCause::Cleaning,
                    operation: None,
                    evidence: None,
                    at: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    let outcome = handle_robotics_with_opener(&mut index, &cfg, "LIB-CLEAN", action, &handle, {
        let world = world.clone();
        move |lib, policy| {
            lib.open_with(policy, move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(ModelTransport::new(world.clone(), role)))
            })
        }
    });
    let moved = world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .any(|c| c.opcode == 0xa5);
    println!("A7 MOVE logged: {moved}");
    let message = match outcome {
        RoboticsOutcome::Completed { action, .. } => action.err().unwrap_or_default(),
        RoboticsOutcome::Failed { message, .. } => message,
        RoboticsOutcome::Cancelled => "cancelled".into(),
    };
    let library_uuid = runtime.manifest().library_uuid();
    drop(api);
    drop(library);
    drop(cfg);
    drop(index);
    drop(catalog);
    drop(runtime);
    let view = lib::native_view::NativeTapeView::probe(
        &paths.journal_dir.join("tapes"),
        &paths.sqlite_path,
    )
    .unwrap();
    let mut refusals = 0;
    lib::store::replay_observed(&paths.audit_dir, library_uuid, &view, &mut |record| {
        if let Some(d) = record.record().diagnostic() {
            if d.event == lib::DiagnosticEvent::RefusalRecorded
                && d.detail
                    .0
                    .iter()
                    .any(|e| e.0.as_str() == "code" && e.1 == lib::DiagnosticValue::Unsigned(12))
            {
                refusals += 1;
            }
        }
        Ok(())
    })
    .unwrap();
    assert!(
        !moved,
        "robotics executor issued MOVE through drive fence; message={message}; refusals={refusals}"
    );
    assert!(
        message.starts_with("DriveFenced:") || message.contains("DriveFenced:"),
        "{message}"
    );
    assert_eq!(refusals, 1);
}

/// T1: a waited LOAD refused by a drive fence leaves no readiness row or robotics gate.
#[test]
fn fx1_waited_load_drive_fence_leaves_no_readiness_row_or_robotics_gate() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let paths = StatePaths {
        config_path: root.join("config"),
        state_dir: root.join("state"),
        audit_dir: root.join("library"),
        journal_dir: root.join("native"),
        sqlite_path: root.join("catalog.sqlite"),
        tape_cache_dir: root.join("cache"),
        calibration_dir: root.join("calibration"),
    };
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
    let catalog = remanence_state::projector::open(&paths.sqlite_path)
        .unwrap()
        .with_library_runtime(runtime.clone());
    let mut index = catalog.write_handle();
    let mut model = VirtualWorld::single_drive("LIB-CLEAN", 0x100, "DRV-CLEAN", 0x400, 1);
    model.put_tape_in_slot(
        0x400,
        "CLNU01L9",
        VirtualTape {
            cleaning_cart: false,
            cleaning_cart_expired: false,
            ..VirtualTape::default()
        },
    );
    let world = Arc::new(Mutex::new(model));
    let library = open_model_library(world.clone());
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from(["LIB-CLEAN".into()]),
    )
    .unwrap();
    let mut cfg = test_write_owner_config(
        paths.sqlite_path.clone(),
        paths.audit_dir.clone(),
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    cfg.catalog = catalog.clone();
    cfg.authority = crate::write_owner::TerminalAuthority::Typed(runtime.clone());
    cfg.cleaning.min_cycle_duration = "0s".into();
    let api = crate::ApiState::new(catalog.clone());
    let action = RoboticsAction::Load {
        slot: 0x400,
        bay: 0x100,
        wait_ready: true,
    };
    let fingerprint = crate::operation_source::robotics_fingerprint("LIB-CLEAN", &action);
    let operation = api
        .accept_owner_request(
            AuditActor::User("real-cleaner".into()),
            "load_drive",
            "library",
            "LIB-CLEAN".into(),
            fingerprint,
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation, "load_drive");

    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    detail: lib::Bounded(vec![]),
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(lib::DriveKey::Bay(lib::Bay {
                        library_serial: "LIB-CLEAN".into(),
                        bay: 0x100,
                    })),
                    cause: lib::FenceCause::Cleaning,
                    operation: None,
                    evidence: None,
                    at: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    let outcome = handle_robotics_with_opener(&mut index, &cfg, "LIB-CLEAN", action, &handle, {
        let world = world.clone();
        move |lib, policy| {
            lib.open_with(policy, move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(ModelTransport::new(world.clone(), role)))
            })
        }
    });
    let moved = world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .any(|c| c.opcode == 0xa5);
    println!("A7 MOVE logged: {moved}");
    let message = match &outcome {
        RoboticsOutcome::Completed { action, .. } => action.clone().err().unwrap_or_default(),
        RoboticsOutcome::Failed { message, .. } => message.clone(),
        RoboticsOutcome::Cancelled => "cancelled".into(),
    };
    outcome.publish(&mut index, &cfg, &handle, "LIB-CLEAN");
    let active = index.list_active_media_readiness_operations(None).unwrap();
    let admission = crate::startup_media_readiness::ensure_media_readiness_admitted(
        &index,
        Some(&runtime),
        "move_medium",
        "LIB-CLEAN",
        None,
        None,
        true,
    );
    let library_uuid = runtime.manifest().library_uuid();
    drop(api);
    drop(library);
    drop(cfg);
    drop(index);
    drop(catalog);
    drop(runtime);
    let view = lib::native_view::NativeTapeView::probe(
        &paths.journal_dir.join("tapes"),
        &paths.sqlite_path,
    )
    .unwrap();
    let mut refusals = 0;
    lib::store::replay_observed(&paths.audit_dir, library_uuid, &view, &mut |record| {
        if let Some(d) = record.record().diagnostic() {
            if d.event == lib::DiagnosticEvent::RefusalRecorded
                && d.detail
                    .0
                    .iter()
                    .any(|e| e.0.as_str() == "code" && e.1 == lib::DiagnosticValue::Unsigned(12))
            {
                refusals += 1;
            }
        }
        Ok(())
    })
    .unwrap();
    assert!(
        !moved,
        "robotics executor issued MOVE through drive fence; message={message}; refusals={refusals}"
    );
    assert!(
        message.starts_with("DriveFenced:") || message.contains("DriveFenced:"),
        "{message}"
    );
    assert_eq!(refusals, 1);
    assert!(
        active.is_empty() && admission.is_ok(),
        "no readiness row and robotics admitted: active={active:?}; admission={admission:?}"
    );
}
