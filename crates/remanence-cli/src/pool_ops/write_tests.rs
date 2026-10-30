//! The archive CLI's selected-library write and host-only replay use production routing.
use super::*;
use remanence_chaos::model::{ModelTransport, Record, VirtualTape, VirtualWorld};
use std::sync::{Arc, Mutex};
const TAPE: [u8; 16] = [0x51; 16];
const BAY: u16 = 0x100;
const BLOCK: u32 = 262144;
const LIBRARY: &str = "POOL-WRITE-LIBRARY";

struct Fixture {
    root: tempfile::TempDir,
    config_path: std::path::PathBuf,
    world: remanence_chaos::model::SharedVirtualWorld,
    report: remanence_library::DiscoveryReport,
}
/// One seated model cartridge and real state, shared by archive CLI regressions.
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let config_path = root.path().join("config.toml");
    let text = format!(
        r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
io_memory_ceiling = "64 MiB"
append_ring_bytes = "16 MiB"
[tape_io]
read_reservoir_bytes = "16 MiB"
[[tape_pools]]
id = "default"
block_size = 262144
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/audit"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
        root.path().display()
    );
    std::fs::write(&config_path, text).unwrap();
    let mut state = StateHandle::open_from_config_file(&config_path).unwrap();
    state
        .catalog_index()
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: TAPE,
            voltag: "POOL001L9".into(),
            block_size: BLOCK,
            parity: remanence_parity::ParityConfig::None,
            force: false,
        })
        .unwrap();
    state
        .catalog_index()
        .project_tape_pool_membership(TAPE, "default")
        .unwrap();
    drop(state);
    let bootstrap = remanence_api::build_tape_bootstrap(
        TAPE,
        BLOCK,
        remanence_parity::ParityConfig::None,
        "2026-10-30T00:00:00Z",
        "pool-write-test",
    );
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK);
    let mut world = VirtualWorld::single_drive(LIBRARY, BAY, "POOL-DRIVE", 0x400, 0);
    world.put_tape_in_drive(BAY, "POOL001L9", None, tape);
    let report = remanence_library::DiscoveryReport {
        libraries: vec![world.library_snapshot()],
        unattached_drives: vec![],
        complete: true,
        incomplete_evidence: vec![],
        warnings: vec![],
    };
    let world = Arc::new(Mutex::new(world));
    Fixture {
        root,
        config_path,
        world,
        report,
    }
}

#[test]
fn selected_library_actor_preserves_encryption_locators_and_replay_before_load() {
    let Fixture {
        root,
        config_path,
        world,
        report,
    } = fixture();
    let keys = [
        RecipientPrivateKey::new([1; 16], "primary", [3; 32]).unwrap(),
        RecipientPrivateKey::new([2; 16], "backup", [4; 32]).unwrap(),
    ];
    let recipients: Vec<_> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let path = root.path().join(format!("key-{i}.remr"));
            std::fs::write(&path, key.public_key(i as u8).unwrap().serialize().unwrap()).unwrap();
            path
        })
        .collect();
    for encrypted in [false, true] {
        let name = if encrypted { "encrypted" } else { "plaintext" };
        let path = root.path().join(name);
        std::fs::write(&path, name).unwrap();
        let args = ArchiveWriteArgs {
            library: LIBRARY.into(),
            file: path.clone(),
            pool_id: "default".into(),
            archive_path: None,
            caller_object_id: Some(name.into()),
            recipients: if encrypted {
                recipients.clone()
            } else {
                vec![]
            },
            json_output: true,
            config: config_path.clone(),
        };
        let shared = world.clone();
        crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |path| {
                let role = shared
                    .lock()
                    .unwrap()
                    .role_for_path(path)
                    .expect("model device");
                Ok(super::mounted_tests::library_transport(
                    ModelTransport::new(shared.clone(), role),
                ))
            }));
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_archive_write_with_lock_root(
            &report,
            &args,
            &[LIBRARY.into()],
            &[],
            &mut out,
            &mut err,
            &root.path().join("locks"),
        );
        assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&err));
        crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| assert!(slot.borrow().is_none()));
        assert!(path.exists(), "caller owns source after success");
        let locator: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(locator["representation"], name);
        assert_eq!(locator["pool_id"], "default");
        assert_eq!(locator["caller_object_id"], name);
        assert_eq!(locator["tape_uuid"], bytes_to_hex(&TAPE));
        assert!(locator["tape_file_number"].is_string());
        assert!(locator["first_body_lba"].is_string());
        let mut state = StateHandle::open_from_config_file(&config_path).unwrap();
        let native = state
            .catalog_index()
            .get_native_object_by_pool_and_caller_object_id("default", name)
            .unwrap()
            .unwrap();
        assert_eq!(native.copies[0].representation, name);
        if encrypted {
            assert_eq!(
                native.copies[0].recipient_epoch_ids.as_ref().unwrap().len(),
                2
            );
            assert_eq!(locator["recipient_epochs"][0]["label"], "primary");
        }
        drop(state);
        // Even a missing library must not be consulted on committed replay.
        let mut replay_report = report.clone();
        replay_report.libraries.clear();
        crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|_| {
                panic!("committed replay must precede any library open or load")
            }))
        });
        let before = world.lock().unwrap().command_log.len();
        let (mut replay, mut errors) = (Vec::new(), Vec::new());
        assert_eq!(
            run_archive_write_with_lock_root(
                &replay_report,
                &args,
                &[LIBRARY.into()],
                &[],
                &mut replay,
                &mut errors,
                &root.path().join("locks")
            ),
            ExitCode::SUCCESS,
            "{}",
            String::from_utf8_lossy(&errors)
        );
        crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| assert!(slot.borrow_mut().take().is_some()));
        assert_eq!(world.lock().unwrap().command_log.len(), before);
        let replay: serde_json::Value = serde_json::from_slice(&replay).unwrap();
        for field in [
            "object_id",
            "tape_uuid",
            "tape_file_number",
            "first_body_lba",
            "content_sha256",
            "representation",
            "metadata_frame_len",
            "format_version",
        ] {
            assert_eq!(replay[field], locator[field], "{field}");
        }
        if encrypted {
            assert!(replay["recipient_epochs"][0]["label"].is_null());
        }
    }
    let records = remanence_state::FileAuditLog::replay(root.path().join("audit")).unwrap();
    let opened: Vec<_> = records
        .iter()
        .filter(|r| r.event == remanence_state::AuditEvent::SessionOpened)
        .collect();
    assert_eq!(opened.len(), 2);
    for record in opened {
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
    assert!(
        !world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .any(|c| matches!(c.opcode, 0x1b | 0xa5)),
        "already placed writer never loads, unloads or moves"
    );
    let mut missing = ArchiveWriteArgs {
        library: "missing".into(),
        file: root.path().join("plaintext"),
        pool_id: "default".into(),
        archive_path: None,
        caller_object_id: Some("new".into()),
        recipients: vec![],
        json_output: true,
        config: config_path,
    };
    assert_eq!(
        run_archive_write(
            &report,
            &missing,
            &[],
            &[],
            &mut Vec::new(),
            &mut Vec::new()
        ),
        ExitCode::from(2)
    );
    missing.pool_id = "unknown".into();
    assert_eq!(
        run_archive_write(
            &report,
            &missing,
            &[],
            &[],
            &mut Vec::new(),
            &mut Vec::new()
        ),
        ExitCode::from(1)
    );
}

impl Fixture {
    fn args(&self) -> ArchiveWriteArgs {
        let file = self.root.path().join("object");
        std::fs::write(&file, b"committed content").unwrap();
        ArchiveWriteArgs {
            library: LIBRARY.into(),
            file,
            pool_id: "default".into(),
            archive_path: None,
            // Exercise the default fresh caller identity: the printed locator is essential.
            caller_object_id: None,
            recipients: vec![],
            json_output: true,
            config: self.config_path.clone(),
        }
    }
    fn run(&self, args: &ArchiveWriteArgs, fail_allow: bool) -> (ExitCode, Vec<u8>, Vec<u8>) {
        let shared = self.world.clone();
        crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |path| {
                let role = shared.lock().unwrap().role_for_path(path).unwrap();
                Ok(super::mounted_tests::library_transport_with_allow_failure(
                    ModelTransport::new(shared.clone(), role),
                    fail_allow,
                ))
            }));
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run_archive_write_with_lock_root(
            &self.report,
            args,
            &[LIBRARY.into()],
            &[],
            &mut out,
            &mut err,
            &self.root.path().join("locks"),
        );
        crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| assert!(slot.borrow().is_none()));
        (code, out, err)
    }
}

/// A daemon's legacy bay columns derive endpoint_key in the schema trigger.
#[test]
fn archive_write_leaves_open_daemon_session_for_daemon_recovery_without_fencing() {
    let fixture = fixture();
    let mut state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    let session_id = uuid::Uuid::new_v4();
    state
        .audit()
        .append(remanence_state::AuditEventRecord {
            actor: remanence_state::AuditActor::System,
            source_layer: remanence_state::SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(session_id),
            idempotency_key: None,
            event: remanence_state::AuditEvent::SessionOpened,
            subject: remanence_state::AuditSubject {
                kind: "session".into(),
                id: Some(session_id.to_string()),
            },
            detail: [
                ("session_kind".into(), ciborium::Value::Text("write".into())),
                ("tape_uuid".into(), ciborium::Value::Bytes(TAPE.to_vec())),
                (
                    "library_serial".into(),
                    ciborium::Value::Text(LIBRARY.into()),
                ),
                (
                    "drive_bay".into(),
                    ciborium::Value::Integer(u64::from(BAY).into()),
                ),
            ]
            .into(),
        })
        .unwrap();
    state.replay_audit_projection().unwrap();
    let before = state.catalog_index().non_terminal_sessions().unwrap();
    assert_eq!(
        before[0].drive_key,
        Some(remanence_library::DriveKey::new(LIBRARY, BAY))
    );
    drop(state);
    let (code, out, err) = fixture.run(&fixture.args(), false);
    assert_eq!(code, ExitCode::from(1));
    assert!(out.is_empty());
    let text = String::from_utf8(err).unwrap();
    assert!(
        text.contains(&format!(
            "has open session {session_id}; run daemon recovery before archive write"
        )),
        "{text}"
    );
    let mut state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    assert_eq!(
        state.catalog_index().non_terminal_sessions().unwrap(),
        before
    );
    assert!(!state
        .catalog_index()
        .tape_io_is_fenced(
            Some(&TAPE),
            None,
            Some(&remanence_library::DriveKey::new(LIBRARY, BAY))
        )
        .unwrap());
    let records = remanence_state::FileAuditLog::replay(fixture.root.path().join("audit")).unwrap();
    assert!(!records.iter().any(|r| matches!(
        r.event,
        remanence_state::AuditEvent::SessionOrphaned
            | remanence_state::AuditEvent::TapeIoFenceRaised
    )));
    assert!(!fixture
        .world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .any(|c| matches!(c.opcode, 0x0a | 0x10 | 0x19)));
}

#[test]
fn archive_write_prints_committed_locator_when_allow_fails() {
    let fixture = fixture();
    let args = fixture.args();
    let (code, out, err) = fixture.run(&args, true);
    assert_eq!(code, ExitCode::from(1), "{}", String::from_utf8_lossy(&err));
    let locator: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let mut state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    let object = state
        .catalog_index()
        .get_native_object(locator["object_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(object.copies[0].status, "committed");
    assert_eq!(
        locator["caller_object_id"],
        object.caller_object_id.unwrap()
    );
    let error = String::from_utf8(err).unwrap();
    assert!(error.contains("transport error"), "{error}");
    assert!(args.file.exists());
}

/// Check the CLI error envelope separately from the adapter's injected checkpoint test.
#[test]
fn archive_write_prints_committed_locator_before_post_checkpoint_error() {
    let fixture = fixture();
    let args = fixture.args();
    let world = fixture.world.clone();
    let policy = StaticAllowlist::new([LIBRARY]);
    let mut library = fixture.report.libraries[0]
        .open_with(&policy, move |path| {
            let role = world.lock().unwrap().role_for_path(path).unwrap();
            Ok(super::mounted_tests::library_transport(
                ModelTransport::new(world.clone(), role),
            ))
        })
        .unwrap();
    let drive = library.open_drive(BAY, &policy).unwrap();
    let state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    let mut owner = remanence_api::engine::drive_adapter::MountedDrive::from_placed_library(
        state,
        library,
        drive,
        policy,
        TAPE,
        "published",
        &fixture.root.path().join("locks"),
    )
    .unwrap();
    let receipt = owner
        .write_checkpointed(WriteObjectToPoolRequest {
            pool_id: "default".into(),
            source: remanence_api::WriteObjectSource::Path(args.file.clone()),
            archive_path: "object".into(),
            caller_object_id: "published".into(),
            expected_content_sha256: None,
            expected_object_id: None,
            representation: PoolWriteRepresentation::Plaintext,
            input_kind: remanence_api::WriteObjectInputKind::LogicalFile,
        })
        .unwrap();
    let object_id = Uuid::from_bytes(receipt.object.object_id).to_string();
    let cause = remanence_api::PoolWriteError::InvalidInput("post-checkpoint failure".into());
    let expected = format!("error: write object: {cause}\n");
    let error = remanence_api::MountError::Committed {
        receipt: Box::new(receipt),
        reason: "new",
        failure: Box::new(remanence_api::MountError::Actor(
            remanence_api::engine::drive_adapter::ActorFailure {
                status: tonic::Status::internal("actor context differs from original error"),
                pool_write_error: Some(Arc::new(cause)),
            },
        )),
    };
    // Both sinks share an ordered event log to prove stdout precedes stderr.
    type Events = Arc<Mutex<Vec<(bool, Vec<u8>)>>>;
    struct Sink {
        events: Events,
        stdout: bool,
    }
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.events
                .lock()
                .unwrap()
                .push((self.stdout, bytes.to_vec()));
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let (mut out, mut err) = (
        Sink {
            events: events.clone(),
            stdout: true,
        },
        Sink {
            events: events.clone(),
            stdout: false,
        },
    );
    assert_eq!(
        print_archive_write_outcome(
            Err(error),
            owner.finish(),
            "default",
            None,
            &args,
            &mut out,
            &mut err
        ),
        ExitCode::from(1)
    );
    let events = events.lock().unwrap();
    let first_error = events.iter().position(|(stdout, _)| !stdout).unwrap();
    assert!(events[first_error..].iter().all(|(stdout, _)| !stdout));
    let output: Vec<u8> = events[..first_error]
        .iter()
        .flat_map(|(_, bytes)| bytes.clone())
        .collect();
    let errors: Vec<u8> = events[first_error..]
        .iter()
        .flat_map(|(_, bytes)| bytes.clone())
        .collect();
    let locator: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(locator["object_id"], object_id);
    assert_eq!(
        String::from_utf8(errors).unwrap(),
        expected,
        "original PoolWriteError text, not Status context"
    );
}
