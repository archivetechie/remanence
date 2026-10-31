//! Crash recovery uses the real composed unload and serial flock with model SCSI.
use super::*;
use remanence_chaos::model::{ModelTransport, SharedVirtualWorld, VirtualTape, VirtualWorld};
use remanence_library::transport::{RecordingLog, RecordingTransport};

const BAY: u16 = 0x100;
const SLOT: u16 = 0x400;
const SERIAL: &str = "UNLOAD-DRIVE";

/// A config file without opening state, catalogs, or reset fences.
fn config(root: &Path) -> PathBuf {
    let path = root.join("config.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/audit"
fsync = false
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
            root.display()
        ),
    )
    .unwrap();
    path
}

/// Reopening a model transport retains the initiator's medium-removal prevent.
fn fixture() -> (
    LibraryHandle,
    StaticAllowlist,
    SharedVirtualWorld,
    RecordingLog,
) {
    let mut world = VirtualWorld::single_drive("UNLOAD-LIB", BAY, SERIAL, SLOT, 1);
    world.put_tape_in_drive(BAY, "TAPE", Some(SLOT), VirtualTape::empty(1024, 512));
    let world = Arc::new(Mutex::new(world));
    let snapshot = world.lock().unwrap().library_snapshot();
    let policy = StaticAllowlist::new(["UNLOAD-LIB"]);
    let log = RecordingLog::new();
    let shared = world.clone();
    let trace = log.clone();
    let library = snapshot
        .open_with(&policy, move |path| {
            let role = shared.lock().unwrap().role_for_path(path).unwrap();
            Ok(Box::new(RecordingTransport::with_log(
                ModelTransport::new(shared.clone(), role),
                trace.clone(),
            )))
        })
        .unwrap();
    (library, policy, world, log)
}

/// Model an abort: PREVENT reaches the drive and the owning handle never drops.
fn strand_prevent(library: &mut LibraryHandle, policy: &StaticAllowlist) {
    let mut writer = library.open_drive(BAY, policy).unwrap();
    writer.prevent_medium_removal().unwrap();
    std::mem::forget(writer);
}

/// Only mutating control CDBs, ignoring inquiry/readiness/inventory traffic.
fn controls(log: &RecordingLog) -> Vec<u8> {
    log.borrow()
        .iter()
        .filter(|cdb| matches!(cdb[0], 0x1e | 0x1b | 0xa5))
        .map(|cdb| cdb[0])
        .collect()
}

#[test]
fn plain_unload_needs_no_config_or_recovery_locks() {
    for explicit in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing-config.toml");
        let (mut library, policy, _, log) = fixture();
        let outcome = unload_library_bay(
            &mut library,
            BAY,
            Some(SLOT),
            &policy,
            explicit.then_some(missing.as_path()),
            &root.path().join("locks"),
        )
        .unwrap();
        assert_eq!(outcome, super::UnloadOutcome::Unloaded);
        assert_eq!(controls(&log), [0x1b, 0xa5]);
        assert!(!root.path().join("locks").exists());
        assert!(!missing.exists());
    }
    // Even explicit valid config must not consult the live daemon's state lock.
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let _owner = remanence_state::StateLockGuard::acquire(root.path()).unwrap();
    let (mut library, policy, _, log) = fixture();
    unload_library_bay(
        &mut library,
        BAY,
        Some(SLOT),
        &policy,
        Some(&config),
        &root.path().join("locks"),
    )
    .unwrap();
    assert_eq!(controls(&log), [0x1b, 0xa5]);
    assert!(!root.path().join("locks").exists());
}

#[test]
fn stranded_prevent_unload_allows_before_retry_and_move() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let (mut library, policy, world, log) = fixture();
    strand_prevent(&mut library, &policy);
    log.borrow_mut().clear();
    let outcome = unload_library_bay(
        &mut library,
        BAY,
        Some(SLOT),
        &policy,
        Some(&config),
        root.path(),
    )
    .unwrap();
    assert_eq!(outcome, super::UnloadOutcome::ClearedStrandedPrevent);
    assert_eq!(controls(&log), [0x1b, 0x1e, 0x1b, 0xa5]);
    assert!(log
        .borrow()
        .iter()
        .filter(|cdb| cdb[0] == 0x1e)
        .all(|cdb| cdb[4] == 0));
    assert!(!world.lock().unwrap().drive_states[&BAY].removal_prevented);
    assert!(library.library().slots[0].full);
    let records = remanence_state::FileAuditLog::replay(root.path().join("audit")).unwrap();
    let audited_cdbs: Vec<_> = records
        .iter()
        .filter_map(|record| match record.detail.get("cdb") {
            Some(ciborium::Value::Bytes(cdb)) => cdb.first().copied(),
            _ => None,
        })
        .collect();
    assert_eq!(audited_cdbs, [0x1e, 0x1b, 0xa5]);
    assert!(records.iter().any(|record| record
        .detail
        .values()
        .any(|value| value == &ciborium::Value::Text("allow_removal".into()))));
}

#[test]
fn stranded_prevent_unload_refuses_live_writer_device_lock_without_allow() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let mut state = StateHandle::open_from_config_file(&config).unwrap();
    let (mut library, policy, world, log) = fixture();
    // Derive the serial from the installed bay exactly as MountedDrive does.
    let serial = library
        .library()
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == BAY)
        .unwrap()
        .installed
        .as_ref()
        .unwrap()
        .serial
        .clone();
    let drive = library.open_drive(BAY, &policy).unwrap();
    let mut writer = OperatorMount::from_placed_library(
        Path::new("library drive"),
        drive,
        &mut state,
        &serial,
        root.path(),
    )
    .unwrap();
    writer
        .context
        .drive
        .as_mut()
        .unwrap()
        .prevent_medium_removal()
        .unwrap();
    log.borrow_mut().clear();
    let error =
        unload_library_bay(&mut library, BAY, Some(SLOT), &policy, None, root.path()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&format!("drive {serial} is locked")),
        "{error}"
    );
    assert!(world.lock().unwrap().drive_states[&BAY].removal_prevented);
    assert_eq!(controls(&log), [0x1b]);
}

#[test]
fn stranded_prevent_actor_drive_unload_audits_allow_before_retry_and_keeps_lock() {
    let root = tempfile::tempdir().unwrap();
    let (mut library, policy, world, log) = fixture();
    strand_prevent(&mut library, &policy);
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    let audits = Arc::new(Mutex::new(Vec::new()));
    let seen = audits.clone();
    let trace = log.clone();
    drive.set_audit_hook(move |event| {
        if let remanence_library::AuditEvent::Started { operation, .. } = event {
            seen.lock().unwrap().push(*operation);
            if *operation == remanence_library::AuditOp::AllowRemoval {
                assert_eq!(controls(&trace), [0x1b], "audit precedes ALLOW CDB");
            }
        }
    });
    log.borrow_mut().clear();
    let outcome = crate::write_owner::unload_actor_drive(&mut drive, root.path()).unwrap();
    assert_eq!(controls(&log), [0x1b, 0x1e, 0x1b]);
    assert_eq!(
        *audits.lock().unwrap(),
        [
            remanence_library::AuditOp::DriveUnload { bay: Some(BAY) },
            remanence_library::AuditOp::AllowRemoval,
            remanence_library::AuditOp::DriveUnload { bay: Some(BAY) },
        ]
    );
    assert!(!world.lock().unwrap().drive_states[&BAY].removal_prevented);
    assert!(
        UnloadRecoveryGuard::acquire(&mut drive, None, root.path()).is_err(),
        "actor reply retains exclusion for MOVE"
    );
    drop(outcome);
    assert!(UnloadRecoveryGuard::acquire(&mut drive, None, root.path()).is_ok());
}

#[test]
fn stranded_prevent_actor_unload_refuses_standalone_writer_without_allow() {
    let root = tempfile::tempdir().unwrap();
    let (mut library, policy, world, log) = fixture();
    strand_prevent(&mut library, &policy);
    let mut daemon_drive = library.open_drive(BAY, &policy).unwrap();
    let mut holder = WriterChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mount_support::unload_tests::standalone_writer_lock_child",
                "--nocapture",
            ])
            .env("REM_UNLOAD_TEST_LOCK_ROOT", root.path())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    use std::io::BufRead;
    let mut output = std::io::BufReader::new(holder.0.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert!(
            output.read_line(&mut line).unwrap() > 0,
            "writer child must acquire its lock"
        );
        if line.contains("WRITER_LOCK_READY") {
            break;
        }
    }
    log.borrow_mut().clear();
    let error = crate::write_owner::unload_actor_drive(&mut daemon_drive, root.path()).unwrap_err();
    assert!(
        error
            .message()
            .contains("locked by a live writer or another process"),
        "{error}"
    );
    assert_eq!(controls(&log), [0x1b]);
    assert!(world.lock().unwrap().drive_states[&BAY].removal_prevented);
}

/// Always reap the separate holder, including when the parent assertion fails.
struct WriterChild(std::process::Child);
impl Drop for WriterChild {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Subprocess entrypoint: production standalone VPD identity and device flock.
#[test]
fn standalone_writer_lock_child() {
    let Some(root) = std::env::var_os("REM_UNLOAD_TEST_LOCK_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let config = config(&root);
    let state = StateHandle::open_from_config_file(&config).unwrap();
    let (mut library, policy, world, _) = fixture();
    let drive = library.open_drive(BAY, &policy).unwrap();
    let standalone = DriveHandle::open_standalone_with_transport(
        Path::new("fake-drive"),
        Box::new(ModelTransport::new(
            world,
            remanence_chaos::model::DeviceRole::Drive { bay: BAY },
        )),
        &StaticAllowlist::new([drive.drive().serial.clone()]),
    )
    .unwrap();
    let mut writer = LockedAuditedDevice::open(standalone, &state, &mut move |_, logical| {
        Ok(root.join(logical.strip_prefix("/run/lock/rem").unwrap()))
    })
    .unwrap();
    writer
        .drive
        .as_mut()
        .unwrap()
        .prevent_medium_removal()
        .unwrap();
    println!("WRITER_LOCK_READY");
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_line(&mut String::new()).unwrap();
}

#[test]
fn stranded_prevent_explicit_config_refuses_state_owner_without_allow() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let _owner = remanence_state::StateLockGuard::acquire(root.path()).unwrap();
    let (mut library, policy, _, log) = fixture();
    strand_prevent(&mut library, &policy);
    log.borrow_mut().clear();
    let error = unload_library_bay(
        &mut library,
        BAY,
        Some(SLOT),
        &policy,
        Some(&config),
        root.path(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("state lock"), "{error}");
    assert_eq!(controls(&log), [0x1b]);
}

/// No-config recovery ignores an unrelated daemon state lock and holds the
/// device flock across both the recovery commands and the changer MOVE.
#[test]
fn stranded_prevent_without_config_holds_device_lock_through_move() {
    let root = tempfile::tempdir().unwrap();
    let _state_owner = remanence_state::StateLockGuard::acquire(root.path()).unwrap();
    let (mut library, policy, _, log) = fixture();
    strand_prevent(&mut library, &policy);
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    let serial = drive.drive().serial.clone();
    let base = root.path().to_owned();
    let operations = Arc::new(Mutex::new(Vec::new()));
    let seen = operations.clone();
    drive.set_audit_hook(move |event| {
        if let remanence_library::AuditEvent::Started { cdb, .. } = event {
            if cdb[0] == 0x1e || cdb[0] == 0xa5 {
                let base = base.clone();
                assert!(lock_device(
                    &mut move |_, logical| Ok(
                        base.join(logical.strip_prefix("/run/lock/rem").unwrap())
                    ),
                    &serial
                )
                .is_err());
                seen.lock().unwrap().push(cdb[0]);
            }
        }
    });
    log.borrow_mut().clear();
    unload_library_bay(&mut library, BAY, Some(SLOT), &policy, None, root.path()).unwrap();
    assert_eq!(controls(&log), [0x1b, 0x1e, 0x1b, 0xa5]);
    assert_eq!(*operations.lock().unwrap(), [0x1e, 0xa5]);
}

#[test]
fn stranded_prevent_reset_fence_does_not_block_bare_state_lock_recovery() {
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path());
    let fence = root.path().join("catalog-reset.in-progress");
    std::fs::write(&fence, b"interrupted reset").unwrap();
    assert!(matches!(
        StateHandle::open_from_config_file(&config),
        Err(remanence_state::StateError::CatalogResetInProgress(_))
    ));
    let (mut library, policy, _, log) = fixture();
    strand_prevent(&mut library, &policy);
    log.borrow_mut().clear();
    let outcome = unload_library_bay(
        &mut library,
        BAY,
        Some(SLOT),
        &policy,
        Some(&config),
        root.path(),
    )
    .unwrap();
    assert_eq!(outcome, super::UnloadOutcome::ClearedStrandedPrevent);
    assert_eq!(controls(&log), [0x1b, 0x1e, 0x1b, 0xa5]);
    assert_eq!(std::fs::read(&fence).unwrap(), b"interrupted reset");
    assert!(!root.path().join("index.sqlite").exists());
}

#[test]
fn stranded_prevent_allow_failure_never_retries_or_moves() {
    let root = tempfile::tempdir().unwrap();
    let (mut library, policy, world, log) = fixture();
    strand_prevent(&mut library, &policy);
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    let fault = world.clone();
    drive.set_audit_hook(move |event| {
        if matches!(
            event,
            remanence_library::AuditEvent::Started {
                operation: remanence_library::AuditOp::AllowRemoval,
                ..
            }
        ) {
            fault
                .lock()
                .unwrap()
                .drive_states
                .get_mut(&BAY)
                .unwrap()
                .reset_unit_attention = true;
        }
    });
    log.borrow_mut().clear();
    let error =
        unload_library_bay(&mut library, BAY, Some(SLOT), &policy, None, root.path()).unwrap_err();
    assert!(
        error.to_string().contains("ALLOW MEDIUM REMOVAL failed"),
        "{error}"
    );
    assert_eq!(controls(&log), [0x1b, 0x1e]);
    assert!(world.lock().unwrap().drive_states[&BAY].removal_prevented);
}

#[test]
fn unload_other_sense_never_allows_or_acquires_recovery_locks() {
    let root = tempfile::tempdir().unwrap();
    let (mut library, policy, world, log) = fixture();
    world
        .lock()
        .unwrap()
        .drive_states
        .get_mut(&BAY)
        .unwrap()
        .reset_unit_attention = true;
    assert!(unload_library_bay(
        &mut library,
        BAY,
        Some(SLOT),
        &policy,
        Some(&root.path().join("missing-config")),
        &root.path().join("locks")
    )
    .is_err());
    assert_eq!(controls(&log), [0x1b]);
    assert!(!root.path().join("locks").exists());
}

#[test]
fn unload_recovery_preserves_original_failure_when_audit_also_fails() {
    let root = tempfile::tempdir().unwrap();
    let (mut library, policy, _, _) = fixture();
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    let guard = UnloadRecoveryGuard::acquire(&mut drive, None, root.path()).unwrap();
    *guard.audit_error.lock().unwrap() = Some("audit fsync failed".into());
    for original in ["ALLOW MEDIUM REMOVAL failed", "drive UNLOAD failed"] {
        assert_eq!(
            guard
                .finish(Err(refusal(original)))
                .unwrap_err()
                .to_string(),
            original
        );
    }
    assert!(guard
        .finish(Ok(()))
        .unwrap_err()
        .to_string()
        .contains("audit fsync failed"));
}

/// A caller may disappear after enqueueing MOVE. The command must own exclusion
/// until the changer completes, rather than borrowing it from that caller.
#[tokio::test]
async fn stranded_prevent_cancelled_move_keeps_recovery_lock_in_command() {
    let root = tempfile::tempdir().unwrap();
    let (mut library, policy, _, log) = fixture();
    strand_prevent(&mut library, &policy);
    let mut drive = library.open_drive(BAY, &policy).unwrap();
    log.borrow_mut().clear();
    let unloaded = crate::write_owner::unload_actor_drive(&mut drive, root.path()).unwrap();
    let (changer, mut commands) = tokio::sync::mpsc::channel(1);
    let pool = crate::write_owner::DrivePool::new_for_library(
        "UNLOAD-LIB",
        changer,
        std::collections::HashMap::new(),
        Arc::new(std::collections::HashMap::new()),
    );
    let caller = tokio::spawn(async move {
        crate::mount::changer_move_after_unload(&pool, "UNLOAD-LIB", BAY, SLOT, unloaded).await
    });
    let command = commands.recv().await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(
        UnloadRecoveryGuard::acquire(&mut drive, None, root.path()).is_err(),
        "cancelled caller must not release a queued MOVE's flock"
    );
    let crate::write_owner::ChangerCommand::Move {
        src,
        dst,
        recovery_guard,
        reply,
        ..
    } = command
    else {
        panic!("MOVE command")
    };
    assert!(recovery_guard.is_some());
    library.move_medium(src, dst, &policy).unwrap();
    assert_eq!(controls(&log), [0x1b, 0x1e, 0x1b, 0xa5]);
    drop(recovery_guard);
    assert!(reply.send(Ok(())).is_err(), "requester is gone");
    assert!(UnloadRecoveryGuard::acquire(&mut drive, None, root.path()).is_ok());
}
