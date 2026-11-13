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
/// Where the model cartridge starts: already in the drive, or in a storage slot.
#[derive(Clone, Copy, PartialEq)]
enum Seat {
    Drive,
    Slot,
}
/// One seated model cartridge and real state, shared by archive CLI regressions.
fn fixture() -> Fixture {
    fixture_seated(Seat::Drive)
}
fn fixture_seated(seat: Seat) -> Fixture {
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
[[libraries]]
serial = "POOL-WRITE-LIBRARY"
"#,
        root.path().display()
    );
    std::fs::write(&config_path, text).unwrap();
    let mut state = StateHandle::open_from_config_file(&config_path).unwrap();
    // The state's own funnel records the tape's Reprovision identity in the library log, which
    // the runtime's session and fence records require; the catalog handle's call writes SQL only.
    state
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: TAPE,
            voltag: "POOL001L9".into(),
            block_size: BLOCK,
            parity: remanence_parity::ParityConfig::None,
            force: false,
        })
        .unwrap();
    state
        .write_handle()
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
    let slots = u16::from(seat == Seat::Slot);
    let mut world = VirtualWorld::single_drive(LIBRARY, BAY, "POOL-DRIVE", 0x400, slots);
    match seat {
        Seat::Drive => world.put_tape_in_drive(BAY, "POOL001L9", None, tape),
        Seat::Slot => world.put_tape_in_slot(0x400, "POOL001L9", tape),
    }
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

/// The selected library must belong to the configuration before the writer opens hardware.
#[test]
fn archive_write_refuses_unlisted_library_and_releases_state_lock() {
    let fx = fixture_seated(Seat::Slot);
    let text = std::fs::read_to_string(&fx.config_path).unwrap();
    std::fs::write(&fx.config_path, text.replace(LIBRARY, "OTHER-LIB")).unwrap();

    let (code, out, err) = run_with_factory(&fx, model_factory(&fx));
    let text = String::from_utf8_lossy(&err);
    assert_eq!(code, ExitCode::from(2), "{text}");
    assert!(
        text.contains("library POOL-WRITE-LIBRARY is not listed in"),
        "{text}"
    );
    assert!(out.is_empty());
    assert!(opcodes(&fx).is_empty(), "no SCSI command before refusal");
    let _state = StateHandle::open_from_config_file(&fx.config_path).unwrap();
}

/// D-C3a-2d-1: read native records as characterisation_sessions' ruling24_native_records
/// does, without acquiring another tape owner or changing the log.
fn native_write_records(config_path: &Path) -> Vec<remanence_state::tape_log::Record> {
    let config = remanence_state::load_config(config_path).unwrap();
    let path = config
        .journal
        .dir
        .join("tapes")
        .join(format!("{}.remtape", uuid::Uuid::from_bytes(TAPE).simple()));
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("D-C3a-2d-1: read tape source: {error}"),
    };
    remanence_state::tape_log::store::Header::decode(&mut file, TAPE).unwrap();
    let offset = std::io::Seek::stream_position(&mut file).unwrap();
    let mut decoder = remanence_state::tape_log::codec::Decoder::at(file, 1, offset);
    let mut records = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        records.push(frame.record.clone());
    }
    records
}

/// D-C3a-2d-1: selected writes keep locators and encryption; committed replay opens no session.
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
        let state = StateHandle::open_from_config_file(&config_path).unwrap();
        let native = state
            .catalog()
            .get_native_object_by_pool_and_caller_object_id("default", name)
            .unwrap()
            .unwrap();
        assert_eq!(native.instances[0].representation, name);
        if encrypted {
            assert_eq!(
                native.instances[0]
                    .recipient_epoch_ids
                    .as_ref()
                    .unwrap()
                    .len(),
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
    // D-C3a-2d-1: the native session does not record the drive today; the owner's
    // drive-identity order W-DRV will add it. Committed replay opens no session.
    let records = native_write_records(&config_path);
    let opened = records
        .iter()
        .filter(|record| matches!(record, remanence_state::tape_log::Record::SessionOpened(_)))
        .count();
    assert_eq!(
        opened, 2,
        "D-C3a-2d-1: one native session per archive write"
    );
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

/// A daemon's open bay session, published through the library runtime, blocks an archive write.
#[test]
fn archive_write_leaves_open_daemon_session_for_daemon_recovery_without_fencing() {
    use remanence_state::library_log as lib;
    let fixture = fixture();
    let state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    let session_id = uuid::Uuid::new_v4();
    state
        .library_runtime()
        .observe_session(
            *session_id.as_bytes(),
            lib::SessionKind::Operation,
            Some(&remanence_library::DriveKey::new(LIBRARY, BAY)),
            Some(TAPE),
            lib::SessionEvent::Opened,
            None,
            remanence_state::AuditActor::System,
            (None, None),
        )
        .unwrap();
    let before = state.catalog().non_terminal_sessions().unwrap();
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
    let state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    assert_eq!(state.catalog().non_terminal_sessions().unwrap(), before);
    assert!(!state
        .catalog()
        .tape_io_is_fenced(
            Some(&TAPE),
            None,
            Some(&remanence_library::DriveKey::new(LIBRARY, BAY))
        )
        .unwrap());
    let paths = state.paths().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    drop(state);
    let view = lib::native_view::NativeTapeView::probe(
        &paths.journal_dir.join("tapes"),
        &paths.sqlite_path,
    )
    .unwrap();
    let mut fences = 0;
    let mut sessions = 0;
    let mut seeded_sessions = 0;
    lib::store::replay_observed(&paths.audit_dir, library_uuid, &view, &mut |record| {
        match record.record().tag() {
            lib::Tag::FenceRaised => fences += 1,
            lib::Tag::SessionObserved => {
                sessions += 1;
                if record.record().envelope.session_id == Some(*session_id.as_bytes()) {
                    seeded_sessions += 1;
                }
            }
            _ => {}
        }
        Ok(())
    })
    .unwrap();
    // The seeded `Opened` is the log's only session record: nothing orphaned or closed it.
    assert_eq!((fences, sessions), (0, 1));
    assert_eq!(
        seeded_sessions, 1,
        "the one session record is the seeded session's"
    );
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
    let state = StateHandle::open_from_config_file(&fixture.config_path).unwrap();
    let object = state
        .catalog()
        .get_native_object(locator["object_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(object.instances[0].status, "committed");
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
    let expected =
        format!("error: write object: {cause}; actor context differs from original error\n");
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
        "legacy PoolWriteError leading text followed by actor context"
    );
}

/// Local diagnostics retain both the legacy cause and the actor's recovery advice.
#[test]
fn typed_write_error_keeps_appended_fence_abandonment_and_checkpoint_context() {
    use remanence_api::engine::drive_adapter::ActorFailure;
    let cause = Arc::new(remanence_api::PoolWriteError::InvalidInput(
        "write failed".into(),
    ));
    let leading = cause.to_string();
    for suffix in [
        "additionally failed to persist standalone fence: disk full",
        "abandonment marker failed: disk full",
    ] {
        let message = format!("{leading}; {suffix}");
        assert_eq!(
            local_write_error(ActorFailure {
                status: tonic::Status::internal(message.clone()),
                pool_write_error: Some(cause.clone())
            }),
            message
        );
    }
    let retry =
        format!("checkpoint batch batch-one failed; re-send all 2 WRITTEN objects: {leading}");
    assert_eq!(
        local_write_error(ActorFailure {
            status: tonic::Status::internal(retry.clone()),
            pool_write_error: Some(cause.clone())
        }),
        format!("{leading}; {retry}")
    );
    assert_eq!(
        local_write_error(ActorFailure {
            status: tonic::Status::internal(leading.clone()),
            pool_write_error: Some(cause)
        }),
        leading
    );
}

// ---------------------------------------------------------------------------
//  Decisions R1c-2 and R1c-3 on the production path
// ---------------------------------------------------------------------------

fn bay_key() -> remanence_library::DriveKey {
    remanence_library::DriveKey::new(LIBRARY, BAY)
}

/// What an interrupted earlier process leaves in the library log: a LOAD intent on the bay
/// with no outcome, or with an `Unknown` one.
fn leave_load(fx: &Fixture, unknown: bool) {
    use remanence_library::motion::{MotionAuthority, MotionCommand, MotionContext, PendingMotion};
    let state = StateHandle::open_from_config_file(&fx.config_path).unwrap();
    let runtime = state.library_runtime();
    let command = MotionCommand::Load(bay_key());
    let ticket = MotionAuthority::intent(&runtime, &command, &MotionContext::default()).unwrap();
    if unknown {
        runtime
            .unconfirmed(&PendingMotion {
                command,
                context: MotionContext::default(),
                ticket,
                evidence: remanence_library::outcome::CompletionEvidence::success(),
            })
            .unwrap();
    }
}

fn load_unresolved(fx: &Fixture) -> bool {
    let state = StateHandle::open_from_config_file(&fx.config_path).unwrap();
    state
        .library_runtime()
        .endpoint_motion_unresolved(&bay_key())
}

fn opcodes(fx: &Fixture) -> Vec<u8> {
    fx.world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .map(|command| command.opcode)
        .collect()
}

/// Run the writer with a transport factory of the test's own, and clear whatever the writer
/// left unconsumed (a run refused before any library open never takes it).
fn run_with_factory(
    fx: &Fixture,
    factory: crate::CliTransportFactory,
) -> (ExitCode, Vec<u8>, Vec<u8>) {
    crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| {
        assert!(slot.borrow().is_none());
        *slot.borrow_mut() = Some(factory);
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_archive_write_with_lock_root(
        &fx.report,
        &fx.args(),
        &[LIBRARY.into()],
        &[],
        &mut out,
        &mut err,
        &fx.root.path().join("locks"),
    );
    crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| slot.borrow_mut().take());
    (code, out, err)
}

fn model_factory(fx: &Fixture) -> crate::CliTransportFactory {
    let shared = fx.world.clone();
    Box::new(move |path| {
        let role = shared.lock().unwrap().role_for_path(path).unwrap();
        Ok(super::mounted_tests::library_transport(
            ModelTransport::new(shared.clone(), role),
        ))
    })
}

/// Start the daemon's real constructor over the writer's state, as the rehearsal's restart did.
/// The returned state handle owns the state lease for as long as the daemon lives, and the
/// runtime carries the daemon's background tasks.
fn start_daemon(
    fx: &Fixture,
) -> Result<
    (
        StateHandle,
        remanence_api::ApiState,
        tokio::runtime::Runtime,
    ),
    String,
> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let state = StateHandle::open_from_config_file(&fx.config_path).map_err(|e| e.to_string())?;
    let mut config = state.config().clone();
    config.libraries = vec![remanence_state::LibraryConfig {
        serial: LIBRARY.into(),
        allow_derived_drive_identity: false,
    }];
    let spool = fx.root.path().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let world = fx.world.clone();
    let report = remanence_library::DiscoveryReport {
        libraries: vec![world.lock().unwrap().library_snapshot()],
        unattached_drives: vec![],
        complete: true,
        incomplete_evidence: vec![],
        warnings: vec![],
    };
    let api = remanence_api::ApiState::with_drive_pool_opener(
        state.projection_client(),
        &config,
        report,
        StaticAllowlist::new([LIBRARY]),
        spool,
        16 * 1024 * 1024,
        move |library, policy| {
            let world = world.clone();
            library.open_with(policy, move |path| {
                let role = world
                    .lock()
                    .unwrap()
                    .role_for_path(path)
                    .expect("model device");
                Ok(Box::new(ModelTransport::new(world.clone(), role)))
            })
        },
    )
    .map_err(|status| status.message().to_string())?;
    Ok((state, api, runtime))
}

/// The rehearsal's sequence on the production path. The archive writer loads the cartridge from
/// a slot with a LOAD that has no readiness owner; that must not leave a readiness row, the
/// write must succeed, and the daemon's constructor must start over the same state.
#[test]
fn archive_write_from_a_slot_leaves_no_readiness_row_and_the_daemon_starts() {
    let fx = fixture_seated(Seat::Slot);
    let (code, out, err) = fx.run(&fx.args(), false);
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&err));
    assert!(!out.is_empty());
    let log = opcodes(&fx);
    assert!(
        log.contains(&0xa5) && log.contains(&0x1b),
        "MOVE and LOAD: {log:?}"
    );
    {
        let state = StateHandle::open_from_config_file(&fx.config_path).unwrap();
        assert!(state
            .catalog()
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty());
        assert!(!state
            .library_runtime()
            .endpoint_motion_unresolved(&bay_key()));
    }
    let (state, _api, _runtime) =
        start_daemon(&fx).unwrap_or_else(|error| panic!("daemon start: {error}"));
    assert!(state
        .catalog()
        .list_active_media_readiness_operations(None)
        .unwrap()
        .is_empty());
}

/// An earlier process left a LOAD with no outcome (or an Unknown one) on the bay where the
/// cartridge now sits. The writer reconciles the library first, sends no motion of its own,
/// then admits and writes; the daemon then starts over the same state.
#[test]
fn archive_write_reconciles_an_earlier_processs_unresolved_load_before_admitting() {
    for unknown in [false, true] {
        let fx = fixture();
        leave_load(&fx, unknown);
        assert!(load_unresolved(&fx), "setup: the bay's LOAD is unresolved");
        let (code, out, err) = fx.run(&fx.args(), false);
        assert_eq!(
            code,
            ExitCode::SUCCESS,
            "unknown={unknown}: {}",
            String::from_utf8_lossy(&err)
        );
        assert!(!out.is_empty());
        let log = opcodes(&fx);
        assert!(
            log.contains(&0xb8),
            "the reconcile read the changer: {log:?}"
        );
        assert!(
            !log.iter().any(|op| matches!(op, 0x1b | 0xa5)),
            "a seated cartridge sends no motion: {log:?}"
        );
        assert!(
            !load_unresolved(&fx),
            "the reconcile resolved the bay's intent"
        );
        let (state, _api, _runtime) =
            start_daemon(&fx).unwrap_or_else(|error| panic!("daemon start: {error}"));
        assert!(state
            .catalog()
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty());
    }
}

/// The reconcile does not vouch for the cartridge: with the unresolved LOAD reconciled, the
/// writer still resolves the tape against the live inventory and refuses when it is nowhere.
#[test]
fn archive_write_refuses_after_the_reconcile_when_the_inventory_does_not_hold_the_tape() {
    let fx = fixture();
    leave_load(&fx, true);
    fx.world.lock().unwrap().drive_bays.insert(BAY, None);
    let (code, out, err) = fx.run(&fx.args(), false);
    let text = String::from_utf8_lossy(&err).to_string();
    assert_eq!(code, ExitCode::from(1), "{text}");
    assert!(out.is_empty());
    assert!(text.contains("error: load tape"), "{text}");
    assert!(!opcodes(&fx)
        .iter()
        .any(|op| matches!(op, 0x0a | 0x1b | 0xa5)));
    assert!(!load_unresolved(&fx), "the reconcile still ran first");
}

/// A changer that fails the reconcile's fresh READ ELEMENT STATUS: the writer reports that
/// error as it is, behind one prefix, promises no remedy, and sends nothing to the tape.
#[test]
fn archive_write_shows_a_failing_reconcile_error_as_it_is() {
    use remanence_library::{transport::TransferOutcome, ScsiError, SgTransport, TimeoutClass};
    struct FailSecondElementStatus {
        inner: Box<dyn SgTransport>,
        seen: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl SgTransport for FailSecondElementStatus {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            if cdb[0] == 0xb8 && self.seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                return Err(ScsiError::TransportError {
                    status: 0,
                    host_status: 3,
                    driver_status: 0,
                    info: 1,
                    sense: vec![],
                });
            }
            self.inner.execute_in(cdb, buf)
        }
        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            self.inner.execute_none(cdb)
        }
        fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            self.inner.execute_out(cdb, buf)
        }
        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }
    let fx = fixture();
    leave_load(&fx, true);
    let shared = fx.world.clone();
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (code, out, err) = run_with_factory(
        &fx,
        Box::new(move |path| {
            let role = shared.lock().unwrap().role_for_path(path).unwrap();
            Ok(Box::new(FailSecondElementStatus {
                inner: super::mounted_tests::library_transport(ModelTransport::new(
                    shared.clone(),
                    role,
                )),
                seen: seen.clone(),
            }))
        }),
    );
    let text = String::from_utf8_lossy(&err).to_string();
    assert_eq!(code, ExitCode::from(1), "{text}");
    assert!(out.is_empty());
    let (prefix, cause) = text
        .trim_end()
        .split_once("startup motion reconciliation: ")
        .unwrap_or_else(|| panic!("the refusal names the reconcile: {text}"));
    assert_eq!(prefix, "error: ", "{text}");
    assert!(!cause.is_empty() && !cause.contains('\n'), "{text}");
    assert!(
        cause.contains("SG_IO transport error") && cause.contains("host_status=0x0003"),
        "the injected failure, as the changer reported it: {text}"
    );
    assert!(
        !text.contains("restart the daemon") && !text.contains("journal replay failed"),
        "{text}"
    );
    assert!(!opcodes(&fx)
        .iter()
        .any(|op| matches!(op, 0x0a | 0x1b | 0xa5)));
    assert!(load_unresolved(&fx), "a failed reconcile resolves nothing");
}

/// The state lock is what keeps the CLI from reconciling beside a running daemon: while another
/// owner holds it, the writer is refused before it opens the library, and so is `rem load`'s
/// motion-authority bind. Nothing reaches the changer.
#[test]
fn the_cli_cannot_reconcile_while_another_process_owns_the_state() {
    let fx = fixture();
    leave_load(&fx, true);
    let owner = StateHandle::open_from_config_file(&fx.config_path).unwrap();
    let (code, out, err) = run_with_factory(&fx, model_factory(&fx));
    let text = String::from_utf8_lossy(&err).to_string();
    assert_eq!(code, ExitCode::from(1), "{text}");
    assert!(out.is_empty());
    assert!(
        text.contains("error: open state: state lock is already held"),
        "{text}"
    );
    assert!(
        opcodes(&fx).is_empty(),
        "no command reached the model: {:?}",
        opcodes(&fx)
    );

    let policy = StaticAllowlist::new([LIBRARY]);
    crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| *slot.borrow_mut() = Some(model_factory(&fx)));
    let mut handle =
        crate::open_library_handle(fx.report.library(LIBRARY).unwrap(), &policy).unwrap();
    let opened = opcodes(&fx).len();
    let bind = remanence_api::mount_support::bind_library_motion_authority(
        &mut handle,
        Some(&fx.config_path),
    );
    let bind_error = bind.expect_err("the bind needs the state lock").to_string();
    assert!(
        bind_error.contains("state lock is already held"),
        "{bind_error}"
    );
    assert_eq!(opcodes(&fx).len(), opened, "the refused bind sent nothing");
    drop(owner);
    assert!(load_unresolved(&fx), "nothing reconciled behind the owner");
}

/// `rem load` reconciles before it moves: an earlier process left a library-scope MOVE open,
/// which refuses every new MOVE, and the load still succeeds because the bind reconciled first.
#[test]
fn rem_load_reconciles_the_library_before_its_first_motion() {
    use remanence_library::motion::{MotionAuthority, MotionCommand, MotionContext};
    let fx = fixture_seated(Seat::Slot);
    {
        let state = StateHandle::open_from_config_file(&fx.config_path).unwrap();
        MotionAuthority::intent(
            &state.library_runtime(),
            &MotionCommand::Move {
                library_serial: LIBRARY.into(),
                source: 0x400,
                destination: BAY,
            },
            &MotionContext::default(),
        )
        .unwrap();
        assert!(state
            .library_runtime()
            .requires_reconciliation(&MotionCommand::Load(bay_key())));
    }
    crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| *slot.borrow_mut() = Some(model_factory(&fx)));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = crate::run_state_change_with_config(
        &fx.report,
        LIBRARY,
        &fx.config_path,
        &[LIBRARY.into()],
        &[],
        &mut out,
        &mut err,
        |handle, policy| {
            handle
                .load(0x400, BAY, policy)
                .map(|()| "loaded".to_string())
                .map_err(|error| error.to_string())
        },
    );
    crate::TEST_CLI_TRANSPORT_FACTORY.with(|slot| slot.borrow_mut().take());
    assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&err));
    let log = opcodes(&fx);
    assert!(log.contains(&0xa5) && log.contains(&0x1b), "{log:?}");
    assert!(!load_unresolved(&fx));
    // `rem load` is another LOAD with no readiness owner; it leaves no readiness row either.
    let state = StateHandle::open_from_config_file(&fx.config_path).unwrap();
    assert!(state
        .catalog()
        .list_active_media_readiness_operations(None)
        .unwrap()
        .is_empty());
}

/// P6 of decision R1c-3, on the production path: the cartridge in the drive is read at BOT and
/// must name the selected tape before the session's first write. A different cartridge in the
/// bay is refused, and no WRITE or WRITE FILEMARKS reaches the drive.
#[test]
fn archive_write_verifies_the_tape_identity_before_its_first_write() {
    let fx = fixture();
    let other = [0x52; 16];
    let bootstrap = remanence_api::build_tape_bootstrap(
        other,
        BLOCK,
        remanence_parity::ParityConfig::None,
        "2026-10-30T00:00:00Z",
        "pool-write-test",
    );
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    fx.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("POOL001L9")
        .unwrap()
        .records[0] = Record::Block(block);
    let (code, out, err) = fx.run(&fx.args(), false);
    let text = String::from_utf8_lossy(&err).to_string();
    assert_eq!(code, ExitCode::from(1), "{text}");
    assert!(out.is_empty());
    assert!(text.contains("tape identity"), "{text}");
    assert!(
        !opcodes(&fx).iter().any(|op| matches!(op, 0x0a | 0x10)),
        "nothing was written: {:?}",
        opcodes(&fx)
    );
}
