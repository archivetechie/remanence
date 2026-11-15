//! G4: typed drive fences select a usable bay or refuse before mount commands.
use super::*;
use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualTape, VirtualWorld};
use remanence_state::library_log as lib;
use std::sync::Mutex;

const SERIAL: &str = "LIB-G4";
const TAPE: TapeUuid = [0x74; 16];
const LABEL: &str = "G40001L9";
const BAY: u16 = 0x100;
const OTHER: u16 = 0x101;
const SLOT: u16 = 0x400;

/// Build model inventory with the fenced bay first, making the exclusion observable.
fn world(seated: bool, two_bays: bool) -> Arc<Mutex<VirtualWorld>> {
    let mut world = VirtualWorld::single_drive(SERIAL, BAY, "G4-DRIVE", SLOT, 1);
    if two_bays {
        world.element_layout.drive_count = 2;
        world.drive_serials.insert(OTHER, "G4-OTHER".into());
        world.drive_bays.insert(OTHER, None);
        world.drive_source_slots.insert(OTHER, None);
        world.drive_states.insert(OTHER, Default::default());
        world.drive_alert_flags.insert(OTHER, Default::default());
    }
    if seated {
        world.put_tape_in_drive(BAY, LABEL, Some(SLOT), VirtualTape::default());
    } else {
        world.put_tape_in_slot(SLOT, LABEL, VirtualTape::default());
    }
    Arc::new(Mutex::new(world))
}

/// Use the real library handle with model SCSI only; opening the changer is setup.
fn open_library(world: &Arc<Mutex<VirtualWorld>>) -> LibraryHandle {
    let mut snapshot = world.lock().unwrap().library_snapshot();
    snapshot.drive_bays.sort_by_key(|bay| bay.element_address);
    assert_eq!(snapshot.drive_bays[0].element_address, BAY);
    let world = world.clone();
    snapshot
        .open_with(
            &remanence_library::StaticAllowlist::new([SERIAL]),
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(ModelTransport::new(world.clone(), role)))
            },
        )
        .unwrap()
}

/// Raise through the typed runtime so planning and the motion backstop share authority.
fn raise_drive(runtime: &remanence_state::library_runtime::LibraryRuntime) -> u64 {
    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    kind: lib::FenceKind::DriveFault,
                    scope: lib::FenceScope::Drive(lib::DriveKey::Bay(lib::Bay {
                        library_serial: SERIAL.into(),
                        bay: BAY,
                    })),
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

/// One typed root and live actor channels exercise the reserve loop without hardware.
fn actor_state(
    root: &crate::tests::TypedRoot,
    world: &Arc<Mutex<VirtualWorld>>,
) -> (
    ApiState,
    mpsc::Receiver<crate::write_owner::ChangerCommand>,
    Vec<mpsc::Receiver<crate::write_owner::DriveCommand>>,
) {
    let mut state = root.api_state();
    let mut library = world.lock().unwrap().library_snapshot();
    library.drive_bays.sort_by_key(|bay| bay.element_address);
    let mut drives = HashMap::new();
    let mut reservations = HashMap::new();
    let mut receivers = Vec::new();
    for bay in &library.drive_bays {
        let key = bay.element_address;
        let (tx, rx) = mpsc::channel(4);
        drives.insert(key, tx);
        reservations.insert(key, AtomicBool::new(false));
        receivers.push(rx);
    }
    let (tx, rx) = mpsc::channel(4);
    state.drive_pool = Some(crate::write_owner::DrivePool::new_for_library(
        SERIAL,
        tx,
        drives,
        Arc::new(reservations),
    ));
    state.library_snapshot = Some(Arc::new(std::sync::RwLock::new(Arc::new(
        crate::LibrarySnapshot {
            report: remanence_library::DiscoveryReport {
                libraries: vec![library],
                unattached_drives: vec![],
                complete: true,
                incomplete_evidence: vec![],
                warnings: vec![],
            },
            captured_at: time::OffsetDateTime::UNIX_EPOCH,
        },
    ))));
    (state, rx, receivers)
}

/// Read durable refusal diagnostics only after all runtime clones have dropped.
async fn assert_refusals(root: &mut crate::tests::TypedRoot, count: usize) {
    let records = root
        .records(|record| {
            record
                .diagnostic()
                .filter(|d| d.event == lib::DiagnosticEvent::RefusalRecorded)
                .cloned()
        })
        .await;
    assert_eq!(records.len(), count);
    for record in records {
        assert_eq!(record.subject_kind.as_str(), "tape");
        assert_eq!(
            record.subject_id.as_ref().unwrap().as_str(),
            Uuid::from_bytes(TAPE).to_string()
        );
        assert_eq!(
            record.detail,
            lib::Bounded(vec![
                lib::DiagnosticEntry("code".into(), lib::DiagnosticValue::Unsigned(12)),
                lib::DiagnosticEntry(
                    "name".into(),
                    lib::DiagnosticValue::Text("DriveFenced".into())
                ),
            ])
        );
    }
}

/// M1: the production reserve/move route selects the other bay and issues no fenced command.
#[tokio::test]
async fn l1r3eg4_actor_slot_uses_unfenced_bay() {
    let mut root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let world = world(false, true);
    let (state, mut changer, mut drives) = actor_state(&root, &world);
    raise_drive(&state.catalog.library_runtime().unwrap());
    let state_for_task = state.clone();
    let task =
        tokio::spawn(async move { test_mount_for_write(&state_for_task, SERIAL, &TAPE).await });
    let command = tokio::time::timeout(Duration::from_secs(5), changer.recv())
        .await
        .unwrap()
        .unwrap();
    let crate::write_owner::ChangerCommand::Move {
        src, dst, reply, ..
    } = command
    else {
        panic!("slot load must move the cartridge");
    };
    // Always finish the tracked task, including in the red proof.
    reply.send(Ok(())).unwrap();
    let result = task.await.unwrap().unwrap();
    assert_eq!((src, dst), (SLOT, OTHER));
    assert_eq!(result, (OTHER, Some(SLOT)));
    assert!(changer.try_recv().is_err());
    assert!(drives.iter_mut().all(|rx| rx.try_recv().is_err()));
    drop(state);
    assert_refusals(&mut root, 0).await;
}

/// M2/M3: seated and only-bay cases return one typed refusal without waiting or dispatch.
async fn refused_actor(seated: bool) {
    refused_actor_with_busy_bay(seated, false).await;
}

/// Keep closeout guards alive so a fenced seated bay must refuse before any busy wait.
async fn refused_actor_with_busy_bay(seated: bool, seated_busy: bool) {
    let mut root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let world = world(seated, seated);
    let (state, mut changer, mut drives) = actor_state(&root, &world);
    raise_drive(&state.catalog.library_runtime().unwrap());
    // A held closeout on the other bay would make a mistaken busy wait hang.
    let _closing = state
        .drive_pool()
        .unwrap()
        .test_begin_actor_work(crate::drive_pool::DriveKey::new(SERIAL, OTHER));
    // Planning reads reservations; actor work keeps a mistaken busy wait pending.
    let _seated_reservation = seated_busy.then(|| {
        state
            .drive_pool()
            .unwrap()
            .reserve_drive(&crate::drive_pool::DriveKey::new(SERIAL, BAY))
            .unwrap()
    });
    let _seated_closing = seated_busy.then(|| {
        state
            .drive_pool()
            .unwrap()
            .test_begin_actor_work(crate::drive_pool::DriveKey::new(SERIAL, BAY))
    });
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        resolve_and_reserve_actor_mount(&state, state.drive_pool().unwrap(), SERIAL, &TAPE),
    )
    .await
    .expect("DriveFenced must not enter the busy wait");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("fenced mount admitted"),
    };
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert!(error.message().starts_with("DriveFenced: "), "{error}");
    assert!(!error.message().contains("busy") && !error.message().contains("no free drive"));
    assert!(changer.try_recv().is_err());
    assert!(drives.iter_mut().all(|rx| rx.try_recv().is_err()));
    drop(state);
    assert_refusals(&mut root, 1).await;
}

#[tokio::test]
async fn l1r3eg4_actor_seated_refuses() {
    refused_actor(true).await;
}

/// Check (a) must refuse even when both the fenced seated bay and the other bay are busy.
#[tokio::test]
async fn l1r3eg4_actor_seated_busy_refuses_without_wait() {
    refused_actor_with_busy_bay(true, true).await;
}

#[tokio::test]
async fn l1r3eg4_actor_only_bay_refuses_without_wait() {
    refused_actor(false).await;
}

/// The pinned resolver checks the proven bay, and still gives barcode errors first.
#[tokio::test]
async fn l1r3eg4_pinned_proven_bay_refuses() {
    let mut root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let world = world(true, true);
    let (state, _, _drives) = actor_state(&root, &world);
    raise_drive(&state.catalog.library_runtime().unwrap());
    let error = resolve_pinned_actor_mount(&state, SERIAL, BAY, &TAPE).unwrap_err();
    assert!(error.message().starts_with("DriveFenced: "), "{error}");
    let error = resolve_pinned_actor_mount(&state, SERIAL, OTHER, &TAPE).unwrap_err();
    assert!(error.message().contains("is empty"));
    drop(state);
    assert_refusals(&mut root, 1).await;
}

/// The CLI/shared loader must perform the same planning before its motion backstop.
async fn load_case(seated: bool, two_bays: bool) {
    let mut root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let world = world(seated, two_bays);
    let mut library = open_library(&world);
    let index = root.catalog().write_handle();
    raise_drive(&index.library_runtime().unwrap());
    world.lock().unwrap().command_log.clear();
    let result = load_tape_by_uuid(
        &index,
        &mut library,
        &remanence_library::StaticAllowlist::new([SERIAL]),
        &TAPE,
    );
    let admitted = !seated && two_bays;
    if admitted {
        let drive = result.unwrap();
        assert_eq!(
            drive.endpoint().durable_key(),
            Some(&remanence_state::DriveKey::new(SERIAL, OTHER))
        );
        let world = world.lock().unwrap();
        assert_eq!(world.loaded_barcode(OTHER), Some(LABEL));
        assert_eq!(world.loaded_barcode(BAY), None);
        assert!(world.command_log.iter().any(|c| c.opcode == 0xa5));
        assert!(world
            .command_log
            .iter()
            .any(|c| c.role == DeviceRole::Drive { bay: OTHER } && c.opcode == 0x1b));
        assert!(world
            .command_log
            .iter()
            .all(|c| c.role != DeviceRole::Drive { bay: BAY }));
    } else {
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("fenced load admitted"),
        };
        assert!(matches!(error, LoadByUuidError::DriveFenced(_)), "{error}");
        assert!(error.to_string().starts_with("DriveFenced: "), "{error}");
        assert!(!error.to_string().contains("no free drive"));
        assert!(world.lock().unwrap().command_log.is_empty());
    }
    drop(library);
    drop(index);
    assert_refusals(&mut root, usize::from(!admitted)).await;
}

#[tokio::test]
async fn l1r3eg4_load_slot_uses_unfenced_bay() {
    load_case(false, true).await;
}

#[tokio::test]
async fn l1r3eg4_load_seated_refuses() {
    load_case(true, true).await;
}

#[tokio::test]
async fn l1r3eg4_load_only_bay_refuses() {
    load_case(false, false).await;
}

/// 31L(h): the mounted actor reaches drive admission before cause-14 tape admission.
#[tokio::test]
async fn l1r3eg4_mounted_drive_precedes_cause14_once() {
    use crate::tape_admission::test_support::{Fixture, T};
    use crate::MountBackend;
    use remanence_chaos::model::Record;
    let mut fixture = Fixture::new();
    let payload = crate::build_tape_bootstrap(
        T,
        262144,
        ParityConfig::None,
        "2026-11-12T00:00:00Z",
        "G4 precedence",
    );
    let mut block = vec![0; 262144];
    remanence_parity::bootstrap::write_bootstrap_block(&payload, &mut block).unwrap();
    let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, 262144);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = 262144;
    let mut model = VirtualWorld::single_drive(SERIAL, BAY, "G4-DRIVE", SLOT, 1);
    model.put_tape_in_drive(BAY, "<no-voltag>", Some(SLOT), tape);
    let world = Arc::new(Mutex::new(model));
    let mut library = open_library(&world);
    let index = fixture.index();
    let policy = remanence_library::StaticAllowlist::new([SERIAL]);
    let mounted = crate::ChangerMount {
        library: &mut library,
        policy: &policy,
        index: &index,
    }
    .mount(crate::MountTarget::Tape { tape_uuid: T })
    .unwrap();
    let cfg = fixture.config(&mounted.drive);
    let (commands, thread) = crate::write_owner::spawn_mounted_actor(mounted, cfg).unwrap();
    let runtime = index.library_runtime().unwrap();
    let fence = raise_drive(&runtime);
    fixture.raise(T, lib::FenceCause::NativeLogBehindProjection);
    world.lock().unwrap().command_log.clear();
    let mut results = Vec::new();
    for released in [false, true] {
        if released {
            runtime
                .append(|a, v| {
                    a.fence_released(
                        lib::FenceReleased {
                            fence,
                            evidence: "G4 precedence release".into(),
                            forced: false,
                            detail: lib::Bounded(vec![]),
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap();
        }
        let (reply, received) = oneshot::channel();
        commands
            .send(crate::write_owner::DriveCommand::FinalizeMounted {
                request: crate::StandaloneFinalizeRequest {
                    tape_uuid: T,
                    acknowledge_tape: T,
                    expected_pool_id: None,
                    reason: "G4 precedence".into(),
                    idempotency_key: Uuid::new_v4(),
                },
                reply,
            })
            .await
            .unwrap();
        results.push(received.await.unwrap());
    }
    let (reply, received) = oneshot::channel();
    commands
        .send(crate::write_owner::DriveCommand::FinishMountedInvocation { reply })
        .await
        .unwrap();
    received.await.unwrap().unwrap();
    thread.join().unwrap();
    drop(commands);
    drop(runtime);
    drop(index);
    drop(library);
    let records = fixture.refusals();
    let first = results.remove(0).unwrap_err();
    assert!(first.message().starts_with("DriveFenced: "), "{first}");
    let second = results.remove(0).unwrap_err();
    assert!(
        second.message().starts_with("TapeLogBehindProjection: "),
        "{second}"
    );
    assert_eq!(records.len(), 2, "exactly one refusal per mounted request");
    assert_eq!(records[0].subject_kind.as_str(), "tape");
    assert_eq!(
        records[0].detail,
        lib::Bounded(vec![
            lib::DiagnosticEntry("code".into(), lib::DiagnosticValue::Unsigned(12)),
            lib::DiagnosticEntry(
                "name".into(),
                lib::DiagnosticValue::Text("DriveFenced".into())
            ),
        ])
    );
    assert_eq!(
        records[0].subject_id.as_ref().unwrap().as_str(),
        Uuid::from_bytes(T).to_string()
    );
    crate::tape_admission::test_support::assert_code(&records[1], T, 1);
    assert!(world.lock().unwrap().command_log.is_empty());
}

/// G1 admits the seated supersede's exact suffix fence, never an ordinary open.
#[tokio::test]
async fn c0b3c_supersede_mount_admits_only_own_suffix_fences() {
    let root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
    let world = world(true, false);
    let (state, mut changer, mut drives) = actor_state(&root, &world);
    let runtime = state.catalog.library_runtime().unwrap();
    let raise = |cause| {
        runtime
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        kind: lib::FenceKind::Write,
                        scope: lib::FenceScope::Tape(runtime.tape_state(TAPE).unwrap().tape),
                        cause,
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
    };
    let suffix = raise(lib::FenceCause::SessionSuffixContradiction).sequence;
    let (mount, reservation) = resolve_and_reserve_actor_mount_inner(
        &state,
        state.drive_pool().unwrap(),
        SERIAL,
        &TAPE,
        Some(TAPE),
    )
    .await
    .expect("supersede mount admits its suffix fence");
    assert_eq!(mount.bay, BAY);
    assert_eq!(mount.source_slot, None, "seated recovery does not reach G2");
    drop(reservation);
    let (mount, reservation) =
        resolve_and_reserve_actor_mount(&state, state.drive_pool().unwrap(), SERIAL, &TAPE)
            .await
            .expect("ordinary motion admits the suffix fence");
    assert_eq!(mount.bay, BAY);
    assert_eq!(mount.source_slot, None);
    drop(reservation);
    raise(lib::FenceCause::TransferError);
    let error = resolve_and_reserve_actor_mount_inner(
        &state,
        state.drive_pool().unwrap(),
        SERIAL,
        &TAPE,
        Some(TAPE),
    )
    .await
    .expect_err("supersede mount must refuse an unrelated tape fence");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        error.message(),
        "open session blocked by active tape-I/O fence; inspect rem tape quarantine list"
    );
    assert!(
        runtime.active_fence(suffix).is_some(),
        "resolution never releases the fence"
    );
    assert!(changer.try_recv().is_err());
    assert!(drives.iter_mut().all(|rx| rx.try_recv().is_err()));
}

/// A suffix-fenced slot cartridge admits mounting, including eviction of a healthy tape.
#[tokio::test]
async fn c0b3c_fr1_supersede_slot_admits_motion() {
    for occupied in [true, false] {
        let root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
        let mut inventory = VirtualWorld::single_drive(SERIAL, BAY, "G4-DRIVE", SLOT, 2);
        inventory.put_tape_in_slot(SLOT, LABEL, VirtualTape::default());
        let world = Arc::new(Mutex::new(inventory));
        if occupied {
            world.lock().unwrap().put_tape_in_drive(
                BAY,
                "HEALTHYL9",
                Some(SLOT + 1),
                VirtualTape::default(),
            );
        }
        let plan = resolve_actor_mount_plan_from_library(
            &world.lock().unwrap().library_snapshot(),
            LABEL,
            &HashSet::new(),
        )
        .expect("slot fixture has a usable mount plan");
        assert_eq!(matches!(plan, ActorMountPlan::Evict { .. }), occupied);
        let (state, mut changer, mut drives) = actor_state(&root, &world);
        let runtime = state.catalog.library_runtime().unwrap();
        runtime
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        kind: lib::FenceKind::Write,
                        scope: lib::FenceScope::Tape(runtime.tape_state(TAPE).unwrap().tape),
                        cause: lib::FenceCause::SessionSuffixContradiction,
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
            .unwrap();
        let mount = resolve_and_reserve_actor_mount_inner(
            &state,
            state.drive_pool().unwrap(),
            SERIAL,
            &TAPE,
            Some(TAPE),
        );
        let answer = async {
            if occupied {
                let crate::write_owner::DriveCommand::Unload { reply, .. } =
                    drives[0].recv().await.unwrap()
                else {
                    panic!("eviction must unload first");
                };
                reply.send(Ok(Duration::from_millis(7).into())).unwrap();
                let crate::write_owner::ChangerCommand::Move {
                    src, dst, reply, ..
                } = changer.recv().await.unwrap()
                else {
                    panic!("eviction must move home");
                };
                assert_eq!((src, dst), (BAY, SLOT + 1));
                reply.send(Ok(())).unwrap();
            }
        };
        let ((mount, reservation), ()) = tokio::time::timeout(Duration::from_secs(1), async {
            let (result, ()) = tokio::join!(mount, answer);
            (result.expect("slot recovery admits its suffix fence"), ())
        })
        .await
        .expect("slot recovery completes motion");
        assert_eq!(mount.source_slot, Some(SLOT));
        assert_eq!(mount.bay, BAY);
        assert!(changer.try_recv().is_err());
        assert!(drives.iter_mut().all(|rx| rx.try_recv().is_err()));
        drop(reservation);
        state.begin_shutdown();
    }
}

/// Raise a typed tape fence so mount and startup gates use the live authority.
fn raise_tape(runtime: &remanence_state::library_runtime::LibraryRuntime, cause: lib::FenceCause) {
    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    kind: lib::FenceKind::Write,
                    scope: lib::FenceScope::Tape(runtime.tape_state(TAPE).unwrap().tape),
                    cause,
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
        .unwrap();
}

/// Startup registers a cause-5 cartridge and its idle timer unloads and moves it home.
#[tokio::test]
async fn x22_startup_read_pass_parks_and_idle_unloads() {
    for (cause, parks) in [
        (lib::FenceCause::AppendAdmissionRefused, true),
        (lib::FenceCause::SessionSuffixContradiction, false),
    ] {
        let root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
        let world = world(true, false);
        let (mut state, mut changer, mut drives) = actor_state(&root, &world);
        state.drive_idle_unload_seconds = 1;
        raise_tape(&state.catalog.library_runtime().unwrap(), cause);
        let snapshot = state.current_library_snapshot().unwrap();
        register_startup_seated_cartridges(&state, &snapshot.report);
        let pool = state.drive_pool().unwrap();
        let key = crate::drive_pool::DriveKey::new(SERIAL, BAY);
        assert_eq!(pool.parked_at(&key).is_some(), parks, "startup {cause:?}");
        if parks {
            tokio::time::timeout(Duration::from_secs(5), async {
                let crate::write_owner::DriveCommand::Unload { reply, .. } =
                    drives[0].recv().await.unwrap()
                else {
                    panic!("idle dismount must unload first");
                };
                reply.send(Ok(Duration::from_millis(7).into())).unwrap();
                let crate::write_owner::ChangerCommand::Move {
                    src, dst, reply, ..
                } = changer.recv().await.unwrap()
                else {
                    panic!("idle dismount must move home");
                };
                assert_eq!((src, dst), (BAY, SLOT));
                reply.send(Ok(())).unwrap();
                while pool.parked_at(&key).is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cause-5 idle unload and move");
        } else {
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        assert!(drives.iter_mut().all(|rx| rx.try_recv().is_err()));
        assert!(changer.try_recv().is_err());
        state.begin_shutdown();
    }
}

/// A failed inventory after a slot move advances the epoch before parking; success does not.
#[tokio::test]
async fn x22_inventory_slot_error_invalidates_calibration() {
    for fails in [true, false] {
        let root = crate::tests::TypedRoot::open_with_tapes(&[(TAPE, LABEL)]);
        let world = world(false, false);
        let (mut state, mut changer, mut drives) = actor_state(&root, &world);
        state.drive_idle_unload_seconds = 0;
        raise_tape(
            &state.catalog.library_runtime().unwrap(),
            lib::FenceCause::AppendAdmissionRefused,
        );
        let before = state.calibration_store().row(TAPE);
        let (stream, _stream_rx) = mpsc::channel(4);
        let inventory = tape_inventory_critical(state.clone(), TAPE, stream);
        let answer = async {
            let crate::write_owner::ChangerCommand::Move {
                src, dst, reply, ..
            } = tokio::time::timeout(Duration::from_secs(5), changer.recv())
                .await
                .expect("inventory moves the cause-5 tape")
                .unwrap()
            else {
                panic!("inventory must move from slot");
            };
            assert_eq!((src, dst), (SLOT, BAY));
            reply.send(Ok(())).unwrap();
            let crate::write_owner::DriveCommand::TapeInventory {
                tape_uuid,
                needs_drive_load,
                reply,
                ..
            } = drives[0].recv().await.unwrap()
            else {
                panic!("inventory actor command");
            };
            assert_eq!(tape_uuid, TAPE);
            assert!(needs_drive_load);
            reply
                .send(if fails {
                    Err(Status::failed_precondition("fixture inventory refusal"))
                } else {
                    Ok(())
                })
                .unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(inventory, answer)
        })
        .await
        .expect("inventory completes after replies");
        if fails {
            let error = result.unwrap_err();
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
            assert_eq!(error.message(), "fixture inventory refusal");
        } else {
            result.unwrap();
        }
        let row = state.calibration_store().row(TAPE);
        assert_eq!(
            row.state,
            remanence_state::VolumeCalibrationState::Uncalibrated
        );
        assert_eq!(
            row.write_epoch,
            before.write_epoch + u64::from(fails),
            "slot inventory error invalidates calibration"
        );
        if !fails {
            assert_eq!(
                row, before,
                "successful inventory does not invalidate calibration"
            );
        }
        assert!(state
            .drive_pool()
            .unwrap()
            .parked_at(&crate::drive_pool::DriveKey::new(SERIAL, BAY))
            .is_some());
        assert!(changer.try_recv().is_err());
        assert!(drives.iter_mut().all(|rx| rx.try_recv().is_err()));
        state.begin_shutdown();
    }
}
