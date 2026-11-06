//! Single-connection regression with real actors and temporary model transports.
use super::*;
/// Count writable main-file descriptors, excluding WAL and SHM.
fn b4a_writable_connections(path: &Path) -> usize {
    let path = path.canonicalize().unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            if std::fs::read_link(entry.path()).ok().as_ref() != Some(&path) {
                return false;
            }
            let info =
                std::fs::read_to_string(Path::new("/proc/self/fdinfo").join(entry.file_name()))
                    .unwrap();
            let flags = info
                .lines()
                .find_map(|line| line.strip_prefix("flags:\t"))
                .unwrap();
            u32::from_str_radix(flags, 8).unwrap() & 3 == 2
        })
        .count()
}
/// Two real drive actors retain receipts and readiness wakes while sharing one catalog connection.
#[tokio::test]
async fn b4a_two_drive_actors_share_one_catalog_connection() {
    const BLOCK: u32 = 256 * 1024;
    let temp = tempfile::tempdir().unwrap();
    let index_path = temp.path().join("state.sqlite");
    let mut index = remanence_state::projector::open(&index_path)
        .map(|catalog| catalog.write_handle())
        .unwrap();
    let audit_dir = temp.path().join("audit");
    std::fs::create_dir_all(&audit_dir).unwrap();
    let audit_lock = Arc::new(Mutex::new(()));
    let admissions = WriteAdmissionCoordinator::default();
    let mut actors = Vec::new();
    let api = crate::ApiState::new(remanence_state::projector::open(&index_path).unwrap());
    let lifecycle = crate::drive_pool::DrivePoolLifecycle::default();
    let started = std::time::Instant::now();
    let mut world = VirtualWorld::single_drive("LIB-B4A", 0x0100, "DRV1", 0x0400, 2);
    world.drive_serials.insert(0x0101, "DRV2".into());
    world.drive_bays.insert(0x0101, None);
    world.drive_source_slots.insert(0x0101, None);
    world.drive_alert_flags.insert(0x0101, Default::default());
    world.drive_states.insert(0x0101, Default::default());
    world.element_layout.drive_count = 2;
    let world = Arc::new(Mutex::new(world));
    let mut samples = Vec::new();
    for n in [1u8, 2] {
        let pool_id = format!("copy-{n}");
        let tape_uuid = [n; 16];
        let barcode = format!("TP000{n}L9");
        let serial = "LIB-B4A".to_string();
        let bay = 0x00ff + u16::from(n);
        let drive_serial = format!("DRV{n}");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.clone(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .unwrap();
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: barcode.clone(),
                block_size: BLOCK,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap();
        index
            .project_tape_pool_membership(tape_uuid, &pool_id)
            .unwrap();
        let drive_uuid = index
            .observe_drive(DriveObservationInput {
                serial: drive_serial.clone(),
                identity_source: "DvcidAndInquiry".into(),
                vendor: None,
                product: None,
                firmware_rev: None,
                managed: "rem".into(),
                library_serial: Some(serial.clone()),
                element_address: Some(i64::from(bay)),
                observed_at_utc: None,
            })
            .unwrap()
            .drive_uuid;
        let bootstrap = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".into(),
            written_at: "2026-10-29T00:00:00Z".into(),
            sequence: 0,
            block_size_bytes: BLOCK,
            drive_compression: false,
        };
        let mut block = vec![0; BLOCK as usize];
        write_bootstrap_block(&bootstrap, &mut block).unwrap();
        let mut tape = VirtualTape::empty(64 * 1024 * 1024, BLOCK);
        tape.records = vec![Record::Block(block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK);
        world
            .lock()
            .unwrap()
            .put_tape_in_drive(bay, &barcode, Some(0x03ff + u16::from(n)), tape);
        world.lock().unwrap().leave_cartridge_unthreaded(bay);
        let mut library = open_model_library(world.clone());
        let snapshot = library_snapshot_cell(library.library().clone());
        let mut cfg =
            test_write_owner_config(index_path.clone(), audit_dir.clone(), &library, snapshot);
        cfg.fixture_audit_mut().audit_append_lock = audit_lock.clone();
        cfg.write_admissions = admissions.clone();
        cfg.checkpoint_max_objects = 1;
        cfg.lifecycle = Some(lifecycle.clone());
        let drive = library.open_drive(bay, &cfg.policy).unwrap();
        let tx = spawn_drive_actor(bay, drive, cfg);
        let pool_cfg = TapePoolConfig {
            id: pool_id,
            scheme: None,
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.9,
            watermark_high: 0.95,
            capacity_cap_bytes: None,
            block_size_bytes: u64::from(BLOCK),
            min_object_size_bytes: 0,
        };
        let session = b4a_open_actor_test_write_session(
            &tx,
            &pool_cfg,
            tape_uuid,
            &serial,
            &barcode,
            &drive_uuid,
            &drive_serial,
        )
        .await;
        tokio::time::timeout(
            StdDuration::from_secs(1),
            lifecycle.readiness_wake.notify.notified(),
        )
        .await
        .expect("each actor readiness write wakes the pool");
        actors.push((tx, session));
    }
    samples.push(b4a_writable_connections(&index_path));
    {
        let mut index = api.catalog.write_handle();
        if let Some(pool) = &api.drive_pool {
            index.set_readiness_observer(pool.readiness_wake.clone());
        }
        index
    }
    .upsert_tape_pool_projection(TapePoolProjectionInput {
        pool_id: "api-pool".into(),
        display_name: None,
        copy_class: None,
        content_class: None,
        created_at_utc: None,
    })
    .unwrap();
    api.record_request_received(AuditActor::System, Uuid::new_v4(), "b4a", &[1; 16], None)
        .unwrap();
    samples.push(b4a_writable_connections(&index_path));
    let append = |i: usize| {
        append_actor_test_file_result(
            &actors[i].0,
            actors[i].1,
            temp.path().join(format!("source-{i}")),
            "payload",
            "shared-caller",
            b"payload",
        )
    };
    let (a, b) = tokio::join!(append(0), append(1));
    for result in [a, b] {
        assert_eq!(
            result
                .unwrap()
                .record
                .append_commit_info
                .unwrap()
                .durability,
            pb::AppendDurability::Checkpointed as i32
        );
    }
    samples.push(b4a_writable_connections(&index_path));
    for (tx, session_id) in actors {
        let (reply, response) = oneshot::channel();
        tx.send(DriveCommand::Close { session_id, reply })
            .await
            .unwrap();
        response.await.unwrap().unwrap();
    }
    assert!(index
        .get_native_object_by_pool_and_caller_object_id("copy-1", "shared-caller")
        .unwrap()
        .is_some());
    assert!(index
        .get_native_object_by_pool_and_caller_object_id("copy-2", "shared-caller")
        .unwrap()
        .is_some());
    samples.push(b4a_writable_connections(&index_path));
    eprintln!(
        "B4a two-drive elapsed={:?}, writable connections={samples:?}",
        started.elapsed()
    );
    assert_eq!(samples, vec![1, 1, 1, 1]);
}

async fn b4a_open_actor_test_write_session(
    drive_tx: &mpsc::Sender<DriveCommand>,
    pool_cfg: &TapePoolConfig,
    tape_uuid: TapeUuid,
    library_serial: &str,
    barcode: &str,
    drive_uuid: &[u8],
    drive_serial: &str,
) -> Uuid {
    let (open_tx, open_rx) = oneshot::channel();
    drive_tx
        .send(DriveCommand::OpenWrite {
            actor: remanence_state::AuditActor::System,
            pool_cfg: pool_cfg.clone(),
            selected: SelectedTape {
                standalone_endpoint: None,
                pool_id: pool_cfg.id.clone(),
                tape_uuid,
                block_size: u32::try_from(pool_cfg.block_size_bytes)
                    .expect("actor test pool block size fits u32"),
                parity_config: ParityConfig::None,
            },
            target_kind: pb::write_session::TargetKind::WriteSessionTargetKindPool,
            needs_drive_load: true,
            library_serial: library_serial.to_string(),
            barcode: Some(barcode.to_string()),
            source_slot: None,
            drive_uuid: Some(drive_uuid.to_vec()),
            drive_serial: Some(drive_serial.to_string()),
            reply: open_tx,
        })
        .await
        .expect("send actor test write open");
    let session = open_rx
        .await
        .expect("actor test write open reply")
        .expect("open actor test write session");
    Uuid::from_slice(&session.session_id).expect("actor test write session UUID")
}
