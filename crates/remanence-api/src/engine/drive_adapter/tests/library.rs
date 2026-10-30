//! Already-placed library ownership and the confined WOR checkpoint crash seam.
use super::*;
use std::os::unix::process::ExitStatusExt;
const LIBRARY: &str = "NOT-A-LIBRARY-ENDPOINT";
const FAULT_ROOT: &str = "/tmp/system-harness/scenario-wor/direct-replay-faults";
const ENABLE: &str = "REM_WOR_DIRECT_REPLAY_FAULT_ENABLE";
const PLAN: &str = "REM_WOR_DIRECT_REPLAY_FAULT_PLAN";
const ENABLE_VALUE: &str = "direct-replay-after-checkpoint-v1-abort";

pub(super) fn library_parts(
    fixture: &Fixture,
) -> (
    remanence_library::LibraryHandle,
    remanence_library::DriveHandle,
    StaticAllowlist,
) {
    let snapshot = fixture.world.lock().unwrap().library_snapshot();
    let world = fixture.world.clone();
    let commands = fixture.commands.clone();
    let fault = fixture.fault.clone();
    let policy = StaticAllowlist::new([LIBRARY]);
    let mut library = snapshot
        .open_with(&policy, move |path| {
            let role = world
                .lock()
                .unwrap()
                .role_for_path(path)
                .expect("virtual path");
            Ok(Box::new(Transport {
                inner: ModelTransport::new(world.clone(), role),
                commands: commands.clone(),
                fault: fault.clone(),
                evidence: None,
                last_opcode: None,
            }))
        })
        .unwrap();
    let drive = library.open_drive(BAY, &policy).unwrap();
    (library, drive, policy)
}
pub(super) fn library_owner(
    fixture: &Fixture,
    state: StateHandle,
    caller: &str,
) -> Result<MountedDrive> {
    let (library, drive, policy) = library_parts(fixture);
    MountedDrive::from_placed_library(
        state,
        library,
        drive,
        policy,
        UUID,
        caller,
        fixture.root.path(),
    )
}

#[test]
fn placed_library_keeps_actual_endpoint_guards_and_source() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = library_owner(&fixture, state, "library-object").unwrap();
    assert_eq!(
        owner.client.endpoint,
        remanence_library::DriveKey::new(LIBRARY, BAY)
    );
    assert!(owner.owner.as_ref().unwrap()._library_guard.is_some());
    assert!(fixture.reopen().is_err());
    // The serial lock is shared with standalone ownership, not a fabricated bay lock.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.root.path().join(format!("{SERIAL}.lock")))
        .unwrap();
    use std::os::fd::AsRawFd;
    // SAFETY: the owned File provides a live descriptor throughout the flock calls.
    assert_ne!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let session = owner.open_session("default").unwrap();
    assert_eq!(session.drive_element_address, Some(u32::from(BAY)));
    owner.append(fixture.request("library-object")).unwrap();
    owner.finish().unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    assert!(fixture.reopen().is_ok());
    assert!(fixture.root.path().join("library-object").exists());
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| matches!(c[0], 0x1b | 0xa5)));
    let records = FileAuditLog::replay(&fixture.config.audit.dir).unwrap();
    for record in records.iter().filter(|r| {
        matches!(
            r.event,
            AuditEvent::SessionOpened | AuditEvent::SessionClosed
        )
    }) {
        assert_eq!(
            record.detail["endpoint_key"],
            ciborium::Value::Text(remanence_library::DriveKey::new(LIBRARY, BAY).encode())
        );
        assert_eq!(
            record.detail["library_serial"],
            ciborium::Value::Text(LIBRARY.into())
        );
        assert_eq!(
            record.detail["drive_bay"],
            ciborium::Value::Integer(u64::from(BAY).into())
        );
    }
}

#[test]
#[ignore = "subprocess helper for WOR crash/gate tests; aborts only with the exact confined plan"]
fn wor_checkpoint_child() {
    // SAFETY: setrlimit changes only this dedicated subprocess; suppress core files.
    assert_eq!(
        unsafe {
            libc::setrlimit(
                libc::RLIMIT_CORE,
                &libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                },
            )
        },
        0
    );
    let (fixture, state) = Fixture::new(false);
    let request = fixture.request("wor-object");
    std::fs::write(
        std::env::var_os("REM_STEP5_MANIFEST").unwrap(),
        serde_json::to_vec(&fixture.config).unwrap(),
    )
    .unwrap();
    match std::env::var("REM_STEP5_MODE").unwrap().as_str() {
        "refuse" => {
            let error = library_owner(&fixture, state, "wor-object")
                .err()
                .expect("invalid WOR plan refused");
            assert!(
                matches!(
                    error.pool_write_error.as_deref(),
                    Some(crate::PoolWriteError::InvalidInput(_))
                ),
                "constructor retains direct-writer error text"
            );
            assert_eq!(
                error.pool_write_error.as_ref().unwrap().to_string(),
                error.status.message()
            );
            assert!(
                error.status.message().contains("REM_WOR")
                    || error.status.message().contains("direct-replay")
            );
            assert!(!fixture
                .commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
        }
        "standalone" => {
            let mut owner = fixture.owner(state);
            owner.write_checkpointed(request).unwrap();
            owner.finish().unwrap();
        }
        mode @ ("mismatch" | "publish-failure") => {
            let mut owner = library_owner(&fixture, state, "wor-object").unwrap();
            let mut request = request;
            if mode == "mismatch" {
                request.caller_object_id = "different-object".into();
            } else {
                // Collision after constructor validation, at evidence publication.
                let plan: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(std::env::var_os(PLAN).unwrap()).unwrap(),
                )
                .unwrap();
                std::fs::write(
                    plan["evidence_path"].as_str().unwrap(),
                    b"existing evidence",
                )
                .unwrap();
            }
            let error = owner.write_checkpointed(request).unwrap_err();
            let crate::MountError::Actor(error) = error else {
                panic!("expected typed actor error: {error}")
            };
            assert!(matches!(
                error.pool_write_error.as_deref(),
                Some(crate::PoolWriteError::InvalidInput(_))
            ));
            assert_eq!(
                error.status.message(),
                error.pool_write_error.as_ref().unwrap().to_string()
            );
            if mode == "mismatch" {
                assert!(!fixture
                    .commands
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
            }
            assert!(fixture.root.path().join("wor-object").exists());
            let _ = owner.finish();
        }
        "abort" => {
            let mut owner = library_owner(&fixture, state, "wor-object").unwrap();
            owner.write_checkpointed(request).unwrap();
            panic!("the durable checkpoint cut must abort before returning a receipt");
        }
        other => panic!("unexpected child mode {other}"),
    }
}

fn child(manifest: &Path, mode: &str) -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "engine::drive_adapter::tests::library::wor_checkpoint_child",
            "--ignored",
            "--nocapture",
        ])
        .env_remove(ENABLE)
        .env_remove(PLAN)
        .env("REM_STEP5_MANIFEST", manifest)
        .env("REM_STEP5_MODE", mode);
    command
}

#[test]
fn wor_cut_publishes_durable_single_object_evidence_before_projection_and_abort() {
    std::fs::create_dir_all(FAULT_ROOT).unwrap();
    let root = tempfile::tempdir_in(FAULT_ROOT).unwrap();
    let evidence = root.path().join("evidence.json");
    let plan = root.path().join("plan.json");
    let nonce = Uuid::new_v4();
    std::fs::write(&plan, serde_json::to_vec(&serde_json::json!({
        "schema":"rem.wor.direct-replay-fault-plan.v1", "tape_uuid":Uuid::from_bytes(UUID).to_string(),
        "caller_object_id":"wor-object", "nonce":nonce.to_string(), "evidence_path":evidence,
    })).unwrap()).unwrap();
    let manifest = root.path().join("manifest.json");
    let output = child(&manifest, "abort")
        .env(ENABLE, ENABLE_VALUE)
        .env(PLAN, &plan)
        .output()
        .unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config: remanence_state::RemConfig =
        serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let index = CatalogIndex::open(&config.index.sqlite_path).unwrap();
    assert!(
        index
            .get_native_object_by_pool_and_caller_object_id("default", "wor-object")
            .unwrap()
            .is_none(),
        "cut is before SQLite projection"
    );
    drop(index);
    let journal =
        remanence_state::FileCheckpointJournal::open(config.journal.dir.join("checkpoints"), UUID)
            .unwrap();
    let record = journal
        .last()
        .unwrap()
        .expect("fsynced checkpoint survives abort");
    assert_eq!(record.objects.len(), 1);
    let evidence: serde_json::Value =
        serde_json::from_slice(&std::fs::read(evidence).unwrap()).unwrap();
    assert_eq!(evidence.as_object().unwrap().len(), 8);
    assert_eq!(
        evidence["schema"],
        "rem.wor.direct-replay-fault-evidence.v1"
    );
    assert_eq!(evidence["nonce"], nonce.to_string());
    assert_eq!(evidence["object_id"], record.objects[0].object.object_id);
    assert_eq!(evidence["caller_object_id"], "wor-object");
    assert_eq!(evidence["checkpoint_ordinal"], record.ordinal.to_string());
    assert_eq!(evidence["checkpoint_eod_lba"], record.eod_lba.to_string());
    let mut state = StateHandle::open_with_config(
        remanence_state::StatePaths::from_config(
            config.daemon.state_dir.join("config.toml"),
            &config,
        ),
        config.clone(),
    )
    .unwrap();
    let request = crate::WriteObjectToPoolRequest {
        pool_id: "default".into(),
        source: crate::WriteObjectSource::Path(config.daemon.state_dir.join("wor-object")),
        archive_path: "wor-object".into(),
        caller_object_id: "wor-object".into(),
        expected_content_sha256: None,
        expected_object_id: None,
        representation: crate::PoolWriteRepresentation::Plaintext,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
    };
    let replay = crate::replay_committed_pool_write_from_state(&mut state, &request)
        .unwrap()
        .unwrap();
    assert_eq!(
        Uuid::from_bytes(replay.object.object_id).to_string(),
        evidence["object_id"]
    );
    assert!(config.daemon.state_dir.join("wor-object").exists());
    drop(state);
    drop(journal);
    std::fs::remove_dir_all(&config.daemon.state_dir).unwrap();
}

#[test]
fn wor_environment_is_fail_closed_and_inert_for_standalone_and_daemon_sessions() {
    let root = tempfile::tempdir().unwrap();
    let manifest = root.path().join("manifest.json");
    for (enabled, plan) in [
        (Some("wrong"), None),
        (None, Some("/tmp/outside.json")),
        (Some(ENABLE_VALUE), None),
        (Some(ENABLE_VALUE), Some("/tmp/outside.json")),
    ] {
        let mut command = child(&manifest, "refuse");
        if let Some(value) = enabled {
            command.env(ENABLE, value);
        }
        if let Some(value) = plan {
            command.env(PLAN, value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::create_dir_all(FAULT_ROOT).unwrap();
    let confined = tempfile::tempdir_in(FAULT_ROOT).unwrap();
    let plan_path = confined.path().join("plan.json");
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), confined.path().join("escape")).unwrap();
    let valid = serde_json::json!({
        "schema": "rem.wor.direct-replay-fault-plan.v1",
        "tape_uuid": Uuid::from_bytes(UUID).to_string(), "caller_object_id": "wor-object",
        "nonce": Uuid::new_v4().to_string(), "evidence_path": confined.path().join("evidence.json"),
    });
    for (field, value) in [
        ("caller_object_id", serde_json::json!("another-object")),
        ("tape_uuid", serde_json::json!(Uuid::new_v4().to_string())),
        ("nonce", serde_json::json!(Uuid::nil().to_string())),
        ("unexpected_field", serde_json::json!(true)),
        (
            "evidence_path",
            serde_json::json!(confined.path().join("escape/evidence.json")),
        ),
        (
            "evidence_path",
            serde_json::json!("/tmp/outside-evidence.json"),
        ),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        std::fs::write(&plan_path, serde_json::to_vec(&invalid).unwrap()).unwrap();
        let output = child(&manifest, "refuse")
            .env(ENABLE, ENABLE_VALUE)
            .env(PLAN, &plan_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{field}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!confined.path().join("evidence.json").exists());
        assert!(!outside.path().join("evidence.json").exists());
    }
    for mode in ["mismatch", "publish-failure"] {
        std::fs::write(&plan_path, serde_json::to_vec(&valid).unwrap()).unwrap();
        let output = child(&manifest, mode)
            .env(ENABLE, ENABLE_VALUE)
            .env(PLAN, &plan_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(
        std::fs::read(confined.path().join("evidence.json")).unwrap(),
        b"existing evidence"
    );
    let output = child(&manifest, "standalone")
        .env(ENABLE, "wrong")
        .env(PLAN, "/tmp/outside.json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "write_owner::tests::checkpoint_actor_deduplicates_in_batch_and_holds_until_checkpoint",
            "--nocapture",
        ])
        .env(ENABLE, "wrong")
        .env(PLAN, "/tmp/outside.json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "must execute the daemon checkpoint test"
    );
}

/// Retrying an accepted terminal suffix under a lowered cap keeps a local typed cause.
#[test]
fn mounted_library_terminal_capacity_refusal_keeps_typed_cause() {
    let (mut fixture, state) = Fixture::new(false);
    let mut owner = library_owner(&fixture, state, "terminal-capacity").unwrap();
    owner.open_session("default").unwrap();
    owner.append(fixture.request("terminal-capacity")).unwrap();
    owner.close_session().unwrap();
    let key = Uuid::new_v4();
    let request = || crate::StandaloneFinalizeRequest {
        tape_uuid: UUID,
        acknowledge_tape: UUID,
        expected_pool_id: Some("default".into()),
        reason: "operator requested".into(),
        idempotency_key: key,
    };
    *fixture.fault.lock().unwrap() = Some(0x0a);
    owner
        .finalize(request())
        .expect_err("injected terminal write failure");
    let _ = owner.finish();
    let mut state = fixture.reopen().unwrap();
    let fences = state.catalog_index().list_active_tape_io_fences().unwrap();
    for fence in fences {
        state
            .release_tape_io_fence(
                &fence.quarantine_id,
                "test operator inspected library and tape",
            )
            .unwrap();
    }
    drop(state);
    fixture.config.tape_pools[0].capacity_cap_bytes = Some(32 * 1024 * 1024);
    let mut owner =
        library_owner(&fixture, fixture.reopen().unwrap(), "terminal-capacity").unwrap();
    let before = fixture.commands.lock().unwrap().len();
    let error = owner.finalize(request()).unwrap_err();
    assert!(
        matches!(
            error.pool_write_error.as_deref(),
            Some(crate::PoolWriteError::Parity(remanence_parity::ParityError::InvalidScheme(message)))
                if message.contains("unsafe terminal close capacity profile")
        ),
        "{error:?}"
    );
    assert!(!fixture.commands.lock().unwrap()[before..]
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
    let _ = owner.finish();

    // Exercise the media recovery funnel too: preflight has already accepted this
    // intent, so a later policy change must retain the same cause at the drive.
    let mut state = fixture.reopen().unwrap();
    let actor = AuditActor::local_user();
    let fingerprint = crate::catalog_conversion::audit_actor_fingerprint(&actor);
    let (_, native) = crate::mount::prepare_manual_finalize_request(
        state.catalog_index(),
        crate::mount::ManualFinalizeTapeAdmission {
            candidate_operation_id: Uuid::new_v4(),
            actor,
            request_fingerprint: crate::catalog_conversion::manual_finalize_request_fingerprint(
                UUID,
                Some("default"),
                &fingerprint,
                b"operator requested",
            ),
            actor_fingerprint: fingerprint,
            idempotency_key: key,
            tape_uuid: UUID,
            expected_pool_id: Some("default".into()),
            reason: "operator requested".into(),
        },
        |_| Ok(fixture.config.tape_pools[0].clone()),
    )
    .unwrap();
    let (_library, mut drive, _policy) = library_parts(&fixture);
    for mounted in [true, false] {
        let cfg = write_owner::TerminalWriteConfig {
            mounted_endpoint: mounted.then(|| remanence_library::DriveKey::new(LIBRARY, BAY)),
            checkpoint_journal_dir: fixture.config.journal.dir.join("checkpoints"),
            audit_dir: fixture.config.audit.dir.clone(),
            audit_fsync: true,
            audit_append_lock: Arc::new(Mutex::new(())),
        };
        let status = write_owner::finalize_mounted_tape(
            state.catalog_index(),
            &cfg,
            &mut drive,
            &native,
            Some("HAND"),
            None,
            |_, drive| {
                drive
                    .test_unit_ready()
                    .map_err(|e| Status::unavailable(e.to_string()))?;
                drive
                    .rewind()
                    .map_err(|e| Status::unavailable(e.to_string()))?;
                crate::verify_tape_identity(&mut remanence_library::DriveHandleSource(drive), &UUID)
                    .map_err(|e| Status::failed_precondition(e.to_string()))
            },
            |_, _| {},
        )
        .unwrap_err();
        let failure = ActorFailure::from(status);
        assert_eq!(failure.status.code(), error.status.code());
        assert_eq!(failure.status.message(), error.status.message());
        assert_eq!(
            failure.pool_write_error.is_some(),
            mounted,
            "daemon retains legacy source-free Status"
        );
    }
}

/// Borrowing a seated library drive must not perform standalone calibration transitions.
#[test]
fn placed_library_preserves_calibration_and_wrap_map() {
    let (fixture, mut state) = Fixture::new(false);
    state
        .calibration_control()
        .record_harvest_success(UUID, 0)
        .unwrap();
    let row = state.calibration_control().row(UUID);
    assert_eq!(
        row.state,
        remanence_state::VolumeCalibrationState::Calibrated
    );
    let map = remanence_state::WrapMapCacheRecord {
        tape_uuid: UUID,
        descriptors: vec![],
        mapped_extent_lba: 512,
        write_epoch: row.write_epoch,
        calibration_generation: row.calibration_generation,
        harvested_at_utc: "2026-10-30T00:00:00Z".into(),
    };
    state.catalog_index().upsert_wrap_map(&map).unwrap();
    let control = state.calibration_control().clone();
    let mut owner = library_owner(&fixture, state, "calibration-object").unwrap();
    assert_eq!(
        control.row(UUID),
        row,
        "mount makes no calibration transition"
    );
    owner
        .write_checkpointed(fixture.request("calibration-object"))
        .unwrap();
    owner.finish().unwrap();
    assert_eq!(
        control.row(UUID),
        row,
        "same calibration behavior as the old library writer"
    );
    let mut state = fixture.reopen().unwrap();
    assert_eq!(
        state.catalog_index().get_wrap_map(&UUID).unwrap(),
        Some(map)
    );
}

#[test]
fn placed_library_returns_committed_receipt_after_checkpoint_publication_failure() {
    let (fixture, mut state) = Fixture::new(false);
    let (library, drive, policy) = library_parts(&fixture);
    let path = Path::new("library drive");
    let mut mount =
        OperatorMount::from_placed_library(path, drive, &mut state, SERIAL, fixture.root.path())
            .unwrap();
    let mounted = mount
        .mount(MountTarget::Device {
            path: path.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    let audit = mount.context.audit_error.clone();
    mount.finish().unwrap();
    let cfg = mounted_config(&state, &mounted, policy, audit).unwrap();
    cfg.io_memory
        .epoch_test_action
        .store(9, std::sync::atomic::Ordering::Relaxed);
    let mut owner =
        MountedDrive::start_configured(state, mounted, cfg, Some(library.into_changer())).unwrap();
    let error = owner
        .write_checkpointed(fixture.request("committed-before-failure"))
        .unwrap_err();
    let crate::MountError::Committed {
        receipt, failure, ..
    } = error
    else {
        panic!("missing committed receipt: {error}");
    };
    assert_eq!(receipt.object.caller_object_id, "committed-before-failure");
    assert!(failure
        .to_string()
        .contains("injected post-projection failure"));
    assert_eq!(owner.receipts().len(), 1);
    assert_eq!(owner.receipts()[0].object_id, receipt.object.object_id);
    let _ = owner.finish();
    assert!(fixture.reopen().is_ok());
}

#[test]
fn placed_library_identity_refusal_preserves_direct_writer_error_text() {
    let (fixture, state) = Fixture::new(false);
    let (library, drive, policy) = library_parts(&fixture);
    let error = MountedDrive::from_placed_library(
        state,
        library,
        drive,
        policy,
        [0x99; 16],
        "identity",
        fixture.root.path(),
    )
    .err()
    .expect("wrong cartridge refused");
    assert!(matches!(
        error.pool_write_error.as_deref(),
        Some(crate::PoolWriteError::TapeIdentity(_))
    ));
    assert_eq!(
        error.status.message(),
        error.pool_write_error.as_ref().unwrap().to_string()
    );
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
    assert!(fixture.reopen().is_ok());
}
