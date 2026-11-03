//! B10/arc regressions through model drive actors and the real mount funnel.
use super::*;
use crate::pb::catalog_server::Catalog;

const OTHER: [u8; 16] = [0x12; 16];
const THIRD: [u8; 16] = [0x13; 16];

/// Seat another cartridge with the same geometry; no physical device is opened.
fn add_tape(root: &Path, world: &Arc<Mutex<VirtualWorld>>, id: [u8; 16], label: &str, bay: u16) {
    let mut index = CatalogIndex::open(root.join("state.sqlite")).unwrap();
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: id,
            voltag: label.into(),
            block_size: BLOCK,
            parity: ParityConfig::Scheme(scheme()),
            force: false,
        })
        .unwrap();
    index.project_tape_pool_membership(id, POOL).unwrap();
    let bootstrap = crate::build_tape_bootstrap(
        id,
        BLOCK,
        ParityConfig::Scheme(scheme()),
        "2026-11-03T00:00:00Z",
        "b10-fix",
    );
    let mut block = vec![0; BLOCK as usize];
    write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(18_000_000_000_000, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = BLOCK.into();
    tape.durable_frontier = 2;
    let mut world = world.lock().unwrap();
    world.drive_serials.insert(bay, format!("B10-{bay}"));
    world.drive_states.insert(bay, Default::default());
    world.drive_alert_flags.insert(bay, Default::default());
    world.element_layout.drive_count += 1;
    world.put_tape_in_drive(bay, label, Some(0x400 + bay - BAY), tape);
}

/// Start real actors with shared lifecycle maps, leaving worker startup explicit.
fn actor_state(
    root: &Path,
    world: Arc<Mutex<VirtualWorld>>,
    policy: PoolSelectionPolicyName,
) -> crate::ApiState {
    let mut library = open_model_library(world.clone());
    let mut state = super::b7_tests::state(root);
    let lifecycle = DrivePoolLifecycle::default();
    let bays: Vec<_> = world
        .lock()
        .unwrap()
        .drive_serials
        .keys()
        .copied()
        .collect();
    let reservations: Arc<HashMap<DriveKey, AtomicBool>> = Arc::new(
        bays.iter()
            .map(|b| (DriveKey::new(LIBRARY, *b), AtomicBool::new(false)))
            .collect(),
    );
    let mut cfg = config(root, &library);
    cfg.lifecycle = Some(lifecycle.clone());
    cfg.reservations = reservations.clone();
    let mut policy_cfg = pool();
    policy_cfg.selection_policy = policy;
    cfg.tape_pools = vec![policy_cfg.clone()];
    state.pool_configs = Arc::new(HashMap::from([(POOL.into(), policy_cfg)]));
    state.library_snapshot = Some(library_snapshot_cell(library.library().clone()));
    let mut drives = HashMap::new();
    for bay in bays {
        let drive = library.open_drive(bay, &cfg.policy).unwrap();
        drives.insert(
            DriveKey::new(LIBRARY, bay),
            spawn_drive_actor(bay, drive, cfg.clone()),
        );
    }
    let (changer, _rx) = mpsc::channel(2);
    state.drive_pool = Some(DrivePool::new_with_lifecycle(
        HashMap::from([(LIBRARY.into(), changer)]),
        drives,
        reservations,
        lifecycle,
    ));
    state
}

async fn open(state: &crate::ApiState, tape: Option<[u8; 16]>) -> Result<pb::WriteSession, Status> {
    let target = match tape {
        Some(tape_uuid) => crate::mount::WriteSessionTarget::PinnedTape {
            tape_uuid,
            required_pool_id: POOL.into(),
        },
        None => crate::mount::WriteSessionTarget::Pool {
            pool_id: POOL.into(),
        },
    };
    tokio::time::timeout(
        StdDuration::from_secs(10),
        crate::mount::open_write_session(AuditActor::System, state, target, None),
    )
    .await
    .unwrap()
}
async fn close(state: &crate::ApiState, session: &pb::WriteSession) {
    let id = Uuid::from_slice(&session.session_id).unwrap();
    let pool = state.drive_pool().unwrap();
    let mounted = pool.session(id).unwrap();
    let (reply, received) = oneshot::channel();
    pool.drive_tx(&mounted.drive_key())
        .unwrap()
        .send(DriveCommand::Close {
            session_id: id,
            reply,
        })
        .await
        .unwrap();
    received.await.unwrap().unwrap();
    pool.wait_tape_closeout(mounted.tape_uuid).await;
}
async fn write(state: &crate::ApiState, session: &pb::WriteSession, root: &Path, name: &str) {
    let id = Uuid::from_slice(&session.session_id).unwrap();
    let pool = state.drive_pool().unwrap();
    let tx = pool
        .drive_tx(&pool.session(id).unwrap().drive_key())
        .unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.join(name),
        name,
        name,
        b"b10 committed object",
    )
    .await;
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Checkpoint {
        session_id: id,
        trigger: CheckpointTrigger::Explicit,
        expected_batch_id: None,
        reply: Some(reply),
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
}

/// B was last written, C has an unrelated worker hold: FillOldest still picks A.
#[tokio::test]
async fn b10_fix_fill_oldest_ignores_unrelated_worker_recency() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    add_tape(root.path(), &world, OTHER, "AAA001L9", BAY + 1);
    add_tape(root.path(), &world, THIRD, "ZZZ001L9", BAY + 2);
    let state = actor_state(root.path(), world, PoolSelectionPolicyName::FillOldest);
    let c = open(&state, Some(THIRD)).await.unwrap();
    close(&state, &c).await;
    let b = open(&state, Some(TAPE)).await.unwrap();
    write(&state, &b, root.path(), "last-written-b").await;
    close(&state, &b).await;
    let pool = state.drive_pool().unwrap();
    let (hold, reservation) = pool.reserve_worker_tape(THIRD).unwrap();
    let a = open(&state, None).await.unwrap();
    assert_eq!(a.tape_uuid, Some(OTHER.to_vec()));
    close(&state, &a).await;
    drop(reservation);
    drop(hold);
    state.shutdown_drive_pool().await.unwrap();
}

/// A complete uncommitted Object remains after a simulated process crash.
async fn crash_tail(root: &Path) -> Arc<Mutex<VirtualWorld>> {
    let world = setup(root);
    let lifecycle = DrivePoolLifecycle::default();
    let (tx, opened, thread) = open_writer_configured(
        root,
        open_model_library(world.clone()),
        selected(),
        |cfg, _| cfg.lifecycle = Some(lifecycle.clone()),
    )
    .await;
    let id = opened.unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.join("committed"),
        "committed",
        "committed",
        b"committed",
    )
    .await;
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Checkpoint {
        session_id: id,
        trigger: CheckpointTrigger::Explicit,
        expected_batch_id: None,
        reply: Some(reply),
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    append_actor_test_file(
        &tx,
        id,
        root.join("foreign"),
        "foreign",
        "foreign",
        b"complete crash tail",
    )
    .await;
    lifecycle
        .shutdown_deadline
        .set(std::time::Instant::now())
        .unwrap();
    lifecycle.shutting_down.store(true, Ordering::SeqCst);
    let (reply, received) = oneshot::channel();
    tx.send(DriveCommand::Abort {
        session_id: id,
        reason: None,
        reply,
    })
    .await
    .unwrap();
    received.await.unwrap().unwrap();
    thread.join().unwrap();
    // Remove the ending group to reproduce exactly the crash before publication.
    let path = TapeAuthority::list_entries(root.join("journals/tapes"))
        .unwrap()
        .pop()
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let mut cursor = std::io::Cursor::new(&bytes);
    remanence_state::tape_log::store::Header::decode(&mut cursor, TAPE).unwrap();
    let mut decoder = remanence_state::tape_log::codec::Decoder::at(
        &bytes[cursor.position() as usize..],
        1,
        cursor.position(),
    );
    let mut cut = None;
    while let Some(frame) = decoder.next_frame().unwrap() {
        if matches!(
            frame.record,
            remanence_state::tape_log::Record::SessionEnded(_)
        ) {
            cut = Some(frame.offset);
        }
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_len(cut.unwrap())
        .unwrap();
    world
}
async fn wait_parked(state: &crate::ApiState) {
    tokio::time::timeout(StdDuration::from_secs(10), async {
        loop {
            if state.closeout_work.parked_tapes().contains(&TAPE)
                && !state
                    .drive_pool()
                    .unwrap()
                    .mounted_tape_uuids()
                    .contains(&TAPE)
            {
                break;
            }
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

/// Parked crash debt cannot monopolize a pool; alone it retains its real refusal.
#[tokio::test]
async fn b10_fix_parked_crash_tail_uses_other_tape_and_preserves_refusal() {
    for alternative in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let world = crash_tail(root.path()).await;
        if alternative {
            add_tape(root.path(), &world, OTHER, "ZZZ002L9", BAY + 1);
        }
        let state = actor_state(root.path(), world, PoolSelectionPolicyName::CompleteOrFill);
        crate::closeout_obligation::spawn(state.clone());
        wait_parked(&state).await;
        if alternative {
            for n in 0..3 {
                let session = open(&state, None).await.unwrap();
                assert_eq!(session.tape_uuid, Some(OTHER.to_vec()));
                write(&state, &session, root.path(), &format!("other-{n}")).await;
                close(&state, &session).await;
            }
        } else {
            let error = open(&state, None).await.unwrap_err();
            assert!(error.message().contains("TailBeyondKnownState"), "{error}");
        }
        state.shutdown_drive_pool().await.unwrap();
    }
}

/// An undiscovered operator refusal also falls through to the other seated tape.
#[tokio::test]
async fn b10_fix_pool_reselects_on_operator_admission_refusal() {
    let root = tempfile::tempdir().unwrap();
    let world = crash_tail(root.path()).await;
    add_tape(root.path(), &world, OTHER, "ZZZ002L9", BAY + 1);
    let state = actor_state(root.path(), world, PoolSelectionPolicyName::CompleteOrFill);
    let session = open(&state, None).await.unwrap();
    assert_eq!(session.tape_uuid, Some(OTHER.to_vec()));
    close(&state, &session).await;
    state.shutdown_drive_pool().await.unwrap();
}

/// Confirmed tail recovery wakes parked debt and the existing worker closes it.
#[tokio::test]
async fn b10_fix_supersede_tail_wakes_parked_worker() {
    let root = tempfile::tempdir().unwrap();
    let world = crash_tail(root.path()).await;
    let state = actor_state(root.path(), world, PoolSelectionPolicyName::CompleteOrFill);
    crate::closeout_obligation::spawn(state.clone());
    wait_parked(&state).await;
    let mut request = pb::SupersedeTailRequest {
        tape_uuid: TAPE.to_vec(),
        ack_tape_uuid: TAPE.to_vec(),
        ..Default::default()
    };
    let preview = state
        .catalog_service()
        .supersede_tail(tonic::Request::new(request.clone()))
        .await
        .unwrap()
        .into_inner();
    assert!(!preview.completed);
    request.observed_tape_log_sequence = Some(preview.tape_log_sequence);
    request.confirm_supersede = Some(preview.required_confirmation);
    request.expected_found = preview.found;
    let result = state
        .catalog_service()
        .supersede_tail(tonic::Request::new(request))
        .await
        .unwrap()
        .into_inner();
    assert!(result.completed);
    tokio::time::timeout(StdDuration::from_secs(10), async {
        while crate::closeout_obligation::summary(&state, TAPE)
            .unwrap()
            .closeout_due()
        {
            tokio::time::sleep(StdDuration::from_millis(5)).await;
        }
    })
    .await
    .expect("worker services repaired debt without restart");
    state.shutdown_drive_pool().await.unwrap();
}

/// Live debt is visible across retry ticks without claiming or recording failures.
#[tokio::test]
async fn b10_fix_live_writer_debt_is_not_attempted() {
    let root = tempfile::tempdir().unwrap();
    let world = setup(root.path());
    let state = actor_state(root.path(), world, PoolSelectionPolicyName::CompleteOrFill);
    let session = open(&state, None).await.unwrap();
    write(&state, &session, root.path(), "live").await;
    crate::closeout_obligation::spawn(state.clone());
    tokio::time::sleep(StdDuration::from_secs(7)).await;
    assert!(state.closeout_work.contains((TAPE, 1)));
    state.closeout_work.assert_unattempted_for_test(TAPE);
    close(&state, &session).await;
    state.shutdown_drive_pool().await.unwrap();
}

/// Finalization can win after recheck but before reserve_tape: reselect B.
#[tokio::test]
async fn b10_fix_post_wait_reservation_busy_reselects() {
    let root = tempfile::tempdir().unwrap();
    let world = super::b7_tests::due_tape(root.path()).await;
    add_tape(root.path(), &world, OTHER, "ZZZ002L9", BAY + 1);
    let state = actor_state(root.path(), world, PoolSelectionPolicyName::CompleteOrFill);
    let pool = state.drive_pool().unwrap().clone();
    let guards = Arc::new(Mutex::new(Vec::new()));
    let held = guards.clone();
    *pool.post_wait_reservation_hook.lock().unwrap() = Some(Box::new(move |pool| {
        let (hold, reservation) = pool.reserve_worker_tape(TAPE).unwrap();
        hold.finalizing(Uuid::new_v4());
        held.lock().unwrap().push((hold, reservation));
    }));
    let (closing, reserved) = pool.reserve_worker_tape(TAPE).unwrap();
    let caller = state.clone();
    let task = tokio::spawn(async move { open(&caller, None).await });
    tokio::time::timeout(StdDuration::from_secs(5), async {
        while !closing.test_has_waiter() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(reserved);
    drop(closing);
    let session = task.await.unwrap().unwrap();
    assert_eq!(
        guards.lock().unwrap().len(),
        1,
        "finalization won after recheck"
    );
    assert_eq!(session.tape_uuid, Some(OTHER.to_vec()));
    close(&state, &session).await;
    guards.lock().unwrap().clear();
    state.shutdown_drive_pool().await.unwrap();
}

/// Parked debt is last among fitting candidates, but remains eligible alone.
#[tokio::test]
async fn b10_fix_parked_debt_ranking_preserves_single_candidate() {
    use crate::pool_write::{SelectionSkips, TapeSelectionHold, TapeSelectionHolds};
    let root = tempfile::tempdir().unwrap();
    let world = super::b7_tests::due_tape(root.path()).await;
    add_tape(root.path(), &world, OTHER, "ZZZ002L9", BAY + 1);
    let state = super::b7_tests::state(root.path());
    let holds = TapeSelectionHolds(HashMap::from([(TAPE, TapeSelectionHold::ParkedDebt)]));
    for (scope, expected) in [
        (HashSet::from([TAPE, OTHER]), OTHER),
        (HashSet::from([TAPE]), TAPE),
    ] {
        let selected = crate::pool_write::select_tape_in_pool_for_write_session_scoped(
            &state.index().unwrap(),
            &pool(),
            0,
            &holds,
            &state.checkpoint_journal_dir,
            &scope,
            &mut SelectionSkips::default(),
        )
        .unwrap();
        assert_eq!(selected.tape_uuid, expected);
    }
}
