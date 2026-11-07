//! Decisions R1c-2 and R1c-3: a LOAD is not a readiness operation, and a write waits for
//! unresolved motion on its own endpoint, and no other. The motion fold is the authority, so
//! every case here runs over a typed `LibraryRuntime`, the production arm; the unit-test
//! fixture authority that has no runtime is not what these exercise.

use super::*;
use remanence_library::motion::{
    MotionAuthority, MotionAuthorityFailure, MotionCommand, MotionContext, MotionTicket,
    PendingMotion,
};
use remanence_library::outcome::CompletionEvidence;
use remanence_state::library_runtime::LibraryRuntime;
use std::sync::atomic::AtomicUsize;

const SERIAL: &str = "LIB-R1C2";
const BAY_A: u16 = 0x100;
const BAY_B: u16 = 0x101;

fn key(bay: u16) -> remanence_state::DriveKey {
    remanence_state::DriveKey::new(SERIAL, bay)
}

/// A one-bay model library with a cartridge already seated in bay A.
fn seated_world() -> Arc<Mutex<VirtualWorld>> {
    let mut world = VirtualWorld::single_drive(SERIAL, BAY_A, "DRV-R1C2", 0x400, 2);
    world.put_tape_in_drive(BAY_A, "R1C2A01L9", Some(0x400), VirtualTape::default());
    Arc::new(Mutex::new(world))
}

fn pending(command: &MotionCommand, ticket: MotionTicket) -> PendingMotion {
    PendingMotion {
        command: command.clone(),
        context: MotionContext::default(),
        ticket,
        evidence: CompletionEvidence::success(),
    }
}

fn intent(runtime: &LibraryRuntime, command: &MotionCommand) -> MotionTicket {
    MotionAuthority::intent(runtime, command, &MotionContext::default()).unwrap()
}

/// A CHECK CONDITION the drive reported: the command did not run, and the authority knows it.
fn known_failure() -> CompletionEvidence {
    CompletionEvidence {
        failed: true,
        scsi_status: Some(2),
        ..CompletionEvidence::success()
    }
}

struct Typed {
    root: crate::tests::TypedRoot,
    runtime: LibraryRuntime,
}
fn typed() -> Typed {
    let root = crate::tests::TypedRoot::open();
    let runtime = root
        .catalog()
        .library_runtime()
        .expect("a typed root has a runtime");
    Typed { root, runtime }
}

/// The accessor beside `requires_reconciliation`: an open or Unknown intent on the endpoint
/// answers yes; a known `Failed` or a confirmed `Done` answers no; a `Done` after `Unknown` is
/// refused by the fold and the answer stays yes until a reconcile; and nothing on another
/// endpoint, nor a library-scope MOVE, answers yes for a bay, while the library's motion gate
/// still refuses a new LOAD in either case.
#[test]
fn endpoint_motion_unresolved_is_per_endpoint_and_ends_with_a_known_result_or_a_reconcile() {
    let t = typed();
    let runtime = &t.runtime;
    let world = seated_world();
    let library = world.lock().unwrap().library_snapshot();
    let (a, b) = (key(BAY_A), key(BAY_B));
    let load_a = MotionCommand::Load(a.clone());
    let load_b = MotionCommand::Load(b.clone());
    assert!(!runtime.endpoint_motion_unresolved(&a));

    // Same endpoint: an open LOAD intent refuses.
    let ticket = intent(runtime, &load_a);
    assert!(runtime.endpoint_motion_unresolved(&a), "open intent");
    // Another endpoint is not named. The library's motion gate still refuses its new LOAD.
    assert!(!runtime.endpoint_motion_unresolved(&b));
    assert!(runtime.requires_reconciliation(&load_b));
    let refused = MotionAuthority::intent(runtime, &load_b, &MotionContext::default()).unwrap_err();
    assert_eq!(refused.kind, MotionAuthorityFailure::UnresolvedMotion);

    // Unknown refuses until a reconcile, and a Done after Unknown is refused by the fold.
    runtime.unconfirmed(&pending(&load_a, ticket)).unwrap();
    assert!(runtime.endpoint_motion_unresolved(&a), "unknown outcome");
    assert!(runtime
        .confirm_inventory(&pending(&load_a, ticket), &library)
        .is_err());
    assert!(
        runtime.endpoint_motion_unresolved(&a),
        "a Done after Unknown does not resolve it"
    );
    runtime.reconcile_inventory(&library).unwrap();
    assert!(!runtime.endpoint_motion_unresolved(&a), "reconciled");
    assert!(!runtime.requires_reconciliation(&load_b));

    // A known Failed admits.
    let ticket = intent(runtime, &load_a);
    assert!(runtime.endpoint_motion_unresolved(&a));
    runtime.failed(ticket, &known_failure()).unwrap();
    assert!(!runtime.endpoint_motion_unresolved(&a), "Failed is known");

    // A Done confirmed by inventory admits.
    let ticket = intent(runtime, &load_a);
    runtime
        .confirm_inventory(&pending(&load_a, ticket), &library)
        .unwrap();
    assert!(!runtime.endpoint_motion_unresolved(&a), "Done");

    // A library-scope MOVE names no endpoint, yet refuses every new LOAD in the library.
    let mv = MotionCommand::Move {
        library_serial: SERIAL.into(),
        source: 0x400,
        destination: BAY_B,
    };
    intent(runtime, &mv);
    assert!(!runtime.endpoint_motion_unresolved(&a));
    assert!(!runtime.endpoint_motion_unresolved(&b));
    assert!(runtime.requires_reconciliation(&load_a));
    let refused = MotionAuthority::intent(runtime, &load_a, &MotionContext::default()).unwrap_err();
    assert_eq!(refused.kind, MotionAuthorityFailure::UnresolvedMotion);
    drop(t.root);
}

/// The funnel every daemon open and dismount passes (`ensure_media_readiness_admitted`) and the
/// readiness half the session close uses refuse the endpoint's own unresolved motion with a text
/// that names the endpoint and the remedy, and admit every other case in the decision.
#[test]
fn admission_funnel_refuses_only_the_endpoints_own_unresolved_motion() {
    use crate::startup_media_readiness::{
        ensure_media_readiness_admitted, ensure_media_readiness_unconflicted,
    };
    let t = typed();
    let runtime = &t.runtime;
    let index = t.root.catalog().write_handle();
    let world = seated_world();
    let library = world.lock().unwrap().library_snapshot();
    let admit = |bay: Option<u16>, robotics: bool| {
        ensure_media_readiness_admitted(
            &index,
            Some(runtime),
            "open session",
            SERIAL,
            bay,
            None,
            robotics,
        )
    };
    admit(Some(BAY_A), false).unwrap();

    // An open LOAD on bay A refuses a write on bay A, naming the endpoint and the remedy.
    let load_a = MotionCommand::Load(key(BAY_A));
    let ticket = intent(runtime, &load_a);
    let refused = admit(Some(BAY_A), false).unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    let text = refused.message();
    assert!(text.contains("open session blocked"), "{text}");
    assert!(
        text.contains(&format!("{SERIAL} drive bay 0x{BAY_A:04x}")),
        "{text}"
    );
    assert!(text.contains("unresolved"), "{text}");
    assert!(text.contains("restart the daemon"), "{text}");
    assert!(
        text.ends_with("restart the daemon to reconcile library motion"),
        "{text}"
    );
    assert!(
        !text.contains("rem ") && !text.contains("CLI"),
        "no CLI command is named as a remedy: {text}"
    );
    // The close path shares the check; with no bay named, nothing is checked.
    assert!(ensure_media_readiness_unconflicted(
        &index,
        Some(runtime),
        "close session",
        SERIAL,
        Some(BAY_A),
        None,
        false
    )
    .is_err());
    admit(None, true).unwrap();
    // Bay B is another endpoint: admitted while bay A's LOAD is open, and while it is Unknown.
    admit(Some(BAY_B), false).unwrap();
    runtime.unconfirmed(&pending(&load_a, ticket)).unwrap();
    assert!(admit(Some(BAY_A), false).is_err(), "Unknown still refuses");
    admit(Some(BAY_B), false).unwrap();
    // A reconcile ends it; so does a known Failed.
    runtime.reconcile_inventory(&library).unwrap();
    admit(Some(BAY_A), false).unwrap();
    let ticket = intent(runtime, &load_a);
    runtime.failed(ticket, &known_failure()).unwrap();
    admit(Some(BAY_A), false).unwrap();

    // A library-scope MOVE unresolved: a write on an already-loaded bay is admitted, and a new
    // LOAD is refused by the motion gate.
    intent(
        runtime,
        &MotionCommand::Move {
            library_serial: SERIAL.into(),
            source: 0x400,
            destination: BAY_B,
        },
    );
    admit(Some(BAY_A), false).unwrap();
    admit(Some(BAY_B), false).unwrap();
    assert!(MotionAuthority::intent(runtime, &load_a, &MotionContext::default()).is_err());
    // Without a typed runtime (unit-test fixtures only) there is nothing to ask.
    let ticket_free = typed();
    let free_index = ticket_free.root.catalog().write_handle();
    intent(&ticket_free.runtime, &load_a);
    ensure_media_readiness_admitted(&free_index, None, "open", SERIAL, Some(BAY_A), None, false)
        .unwrap();
}

/// Session open: a drive whose own LOAD or UNLOAD is unresolved refuses the open before the
/// readiness probe, on the typed index the daemon passes; another bay opens.
#[test]
fn session_open_refuses_the_endpoints_own_unresolved_motion() {
    let t = typed();
    let mut index = t.root.catalog().write_handle();
    let ctx = |bay| SessionOpenReadinessContext {
        action: "open write session",
        bay,
        library_serial: SERIAL,
        barcode: Some("R1C2A01L9"),
        source_slot: None,
        drive_serial: None,
        needs_drive_load: false,
        load_continues_move: false,
        lifecycle: None,
    };
    session_open_reject_admission_conflicts(&mut index, &ctx(BAY_A)).unwrap();
    intent(&t.runtime, &MotionCommand::Unload(key(BAY_A)));
    let refused = session_open_reject_admission_conflicts(&mut index, &ctx(BAY_A)).unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(
        refused.message().contains("open write session blocked")
            && refused
                .message()
                .contains(&format!("{SERIAL} drive bay 0x{BAY_A:04x}"))
            && refused.message().contains("restart the daemon"),
        "{refused}"
    );
    session_open_reject_admission_conflicts(&mut index, &ctx(BAY_B)).unwrap();
}

/// Mounted placement (the CLI writer and an operator mount) re-checks the endpoint's motion at
/// every admission: refused while the LOAD is open, admitted after the reconcile.
#[test]
fn mounted_admission_refuses_the_endpoints_own_unresolved_motion() {
    let t = typed();
    let world = seated_world();
    let mut library = open_model_library(world.clone());
    let library_state = library.library().clone();
    let policy = remanence_library::StaticAllowlist::new([SERIAL]);
    let drive = library.open_drive(BAY_A, &policy).unwrap();
    let index = t.root.catalog().write_handle();
    let mut cfg = test_write_owner_config(
        t.root.paths().sqlite_path.clone(),
        t.root.paths().audit_dir.clone(),
        &library,
        library_snapshot_cell(library_state.clone()),
    );
    cfg.authority = crate::write_owner::TerminalAuthority::Typed(t.runtime.clone());
    cfg.catalog = t.root.catalog();
    cfg.placement =
        crate::write_owner::ActorPlacement::Mounted(crate::write_owner::MountedDriveContext {
            endpoint: drive.endpoint().durable_key().unwrap().clone(),
            tape_uuid: [0x5c; 16],
            barcode: None,
            cartridge_generation: None,
            identity_generation: drive.identity_generation(),
            actor: remanence_state::AuditActor::System,
            audit_error: Arc::new(Mutex::new(None)),
            audit_session: Arc::new(Mutex::new(None)),
            direct_replay_fault: None,
        });
    cfg.mounted_admission(&index, &drive).unwrap();
    let load = MotionCommand::Load(key(BAY_A));
    intent(&t.runtime, &load);
    let refused = cfg.mounted_admission(&index, &drive).unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(
        refused.message().contains("mounted write blocked")
            && refused.message().contains("unresolved")
            && refused.message().contains("restart the daemon"),
        "{refused}"
    );
    t.runtime.reconcile_inventory(&library_state).unwrap();
    cfg.mounted_admission(&index, &drive).unwrap();
}

// ---------------------------------------------------------------------------
//  The daemon's unowned LOADs and the restart that follows them
// ---------------------------------------------------------------------------

/// A state root with the library configured, the way the daemon owns it.
struct Root {
    _dir: tempfile::TempDir,
    paths: remanence_state::StatePaths,
    config: remanence_state::RemConfig,
}
fn root() -> Root {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    let config = remanence_state::parse_config_toml(&format!(
        r#"
[daemon]
state_dir = "{0}/state"
default_idle_timeout_seconds = 1800
drive_idle_unload_seconds = 1800
io_memory_ceiling = "64 MiB"
append_ring_bytes = "16 MiB"
[tape_io]
read_reservoir_bytes = "16 MiB"
[journal]
dir = "{0}/native"
require_trusted_volume = false
[audit]
dir = "{0}/library"
fsync = true
[index]
sqlite_path = "{0}/catalog.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
[[libraries]]
serial = "{SERIAL}"
[[tape_pools]]
id = "r1c2"
block_size = 262144
"#,
        base.display()
    ))
    .unwrap();
    let paths = remanence_state::StatePaths::from_config(base.join("config.toml"), &config);
    Root {
        _dir: dir,
        paths,
        config,
    }
}
impl Root {
    fn open(&self) -> remanence_state::StateHandle {
        remanence_state::StateHandle::open_with_config(self.paths.clone(), self.config.clone())
            .unwrap()
    }
}

/// Start the daemon's real constructor over the root, as a restart does. The library is the
/// model world, optionally behind a transport of the test's own.
fn start_daemon(
    root: &Root,
    world: &Arc<Mutex<VirtualWorld>>,
    wrap: impl Fn(ModelTransport) -> Box<dyn SgTransport> + Clone + Send + 'static,
) -> Result<
    (
        remanence_state::StateHandle,
        crate::ApiState,
        tokio::runtime::Runtime,
    ),
    Status,
> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let state = root.open();
    let spool = root.paths.state_dir.join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    let report = DiscoveryReport {
        libraries: vec![world.lock().unwrap().library_snapshot()],
        unattached_drives: vec![],
        complete: true,
        incomplete_evidence: vec![],
        warnings: vec![],
    };
    let opener_world = world.clone();
    let api = crate::ApiState::with_drive_pool_opener(
        state.projection_client(),
        state.config(),
        report,
        remanence_library::StaticAllowlist::new([SERIAL]),
        spool,
        16 * 1024 * 1024,
        move |library, policy| {
            let world = opener_world.clone();
            let wrap = wrap.clone();
            library.open_with(policy, move |path| {
                let role = world
                    .lock()
                    .unwrap()
                    .role_for_path(path)
                    .expect("model device");
                Ok(wrap(ModelTransport::new(world.clone(), role)))
            })
        },
    )?;
    Ok((state, api, runtime))
}

fn plain(transport: ModelTransport) -> Box<dyn SgTransport> {
    Box::new(transport)
}

fn no_active_readiness_rows(state: &remanence_state::StateHandle) {
    assert!(state
        .catalog()
        .list_active_media_readiness_operations(None)
        .unwrap()
        .is_empty());
}

/// The daemon's LoadDrive with `no_wait` publishes a LOAD parented to its robotics operation
/// and no readiness begin. It must leave no readiness row, and the restart that follows must
/// start and serve.
#[test]
fn load_drive_no_wait_leaves_no_readiness_row_and_the_restart_starts() {
    let root = root();
    let mut model = VirtualWorld::single_drive(SERIAL, BAY_A, "DRV-R1C2", 0x400, 1);
    model.put_tape_in_slot(0x400, "R1C2L01L9", VirtualTape::default());
    let world = Arc::new(Mutex::new(model));
    {
        let state = root.open();
        let mut index = state.write_handle();
        let mut library = open_model_library(world.clone());
        let mut cfg = test_write_owner_config(
            root.paths.sqlite_path.clone(),
            root.paths.audit_dir.clone(),
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        cfg.catalog = state.projection_client();
        cfg.authority = crate::write_owner::TerminalAuthority::Typed(state.library_runtime());
        let api = crate::ApiState::new(state.projection_client());
        let operation = api
            .accept_owner_request(
                AuditActor::User("loader".into()),
                "load_drive",
                "library",
                SERIAL.into(),
                [0x32; 32],
            )
            .unwrap();
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(operation, "load_drive");
        cfg.configure_operation_library(&mut library, &handle);
        super::robotics::run_load_sequence(
            &mut index,
            &cfg,
            &handle,
            &mut library,
            0x400,
            BAY_A,
            false,
        )
        .unwrap();
        no_active_readiness_rows(&state);
        assert!(!state
            .library_runtime()
            .endpoint_motion_unresolved(&key(BAY_A)));
        assert_eq!(drive_load_commands_on(&world, BAY_A), 1);
    }
    let (state, _api, _runtime) = start_daemon(&root, &world, plain)
        .unwrap_or_else(|status| panic!("restart refused: {}", status.message()));
    no_active_readiness_rows(&state);
}

/// The cleaning-cartridge LOAD (parented to the clean operation, no readiness begin) leaves no
/// readiness row either, and the restart starts.
#[test]
fn cleaning_load_leaves_no_readiness_row_and_the_restart_starts() {
    let root = root();
    let mut model = VirtualWorld::single_drive(SERIAL, BAY_A, "DRV-R1C2", 0x400, 1);
    model.put_tape_in_slot(
        0x400,
        "CLNR01L9",
        VirtualTape {
            cleaning_cart: true,
            ..VirtualTape::default()
        },
    );
    let world = Arc::new(Mutex::new(model));
    {
        let state = root.open();
        let mut index = state.write_handle();
        let mut library = open_model_library(world.clone());
        crate::drive_collection::observe_drive_catalog_from_libraries(
            &mut index,
            [library.library()],
            &HashSet::from([SERIAL.to_string()]),
        )
        .unwrap();
        let drive = index.list_drives(true, true).unwrap()[0].drive_uuid.clone();
        let mut cfg = test_write_owner_config(
            root.paths.sqlite_path.clone(),
            root.paths.audit_dir.clone(),
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        cfg.catalog = state.projection_client();
        cfg.authority = crate::write_owner::TerminalAuthority::Typed(state.library_runtime());
        cfg.cleaning.min_cycle_duration = "0s".into();
        let api = crate::ApiState::new(state.projection_client());
        let action = crate::write_owner::RoboticsAction::Clean {
            drive_uuid: drive.clone(),
            trigger: "now".into(),
        };
        let fingerprint = crate::operation_source::robotics_fingerprint(SERIAL, &action);
        let operation = api
            .accept_owner_request(
                AuditActor::User("cleaner".into()),
                "clean_drive",
                "library",
                SERIAL.into(),
                fingerprint,
            )
            .unwrap();
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(operation, "clean_drive");
        super::robotics::record_library_operation(
            &mut index,
            &cfg,
            &handle,
            SERIAL,
            remanence_state::library_log::OperationEvent::Started(
                remanence_state::library_log::Unit,
            ),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        cfg.configure_operation_library(&mut library, &handle);
        run_cleaning_sequence(&mut index, &cfg, &handle, &mut library, &drive, "now").unwrap();
        super::robotics::record_library_operation(
            &mut index,
            &cfg,
            &handle,
            SERIAL,
            remanence_state::library_log::OperationEvent::Finished(
                remanence_state::library_log::Response {
                    response_fingerprint: None,
                },
            ),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        assert!(world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .any(|c| c.opcode == 0x1b));
        no_active_readiness_rows(&state);
        assert!(!state
            .library_runtime()
            .endpoint_motion_unresolved(&key(BAY_A)));
    }
    let (state, _api, _runtime) = start_daemon(&root, &world, plain)
        .unwrap_or_else(|status| panic!("restart refused: {}", status.message()));
    no_active_readiness_rows(&state);
}

fn drive_load_commands_on(world: &Arc<Mutex<VirtualWorld>>, bay: u16) -> usize {
    world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .filter(|c| {
            c.opcode == 0x1b
                && matches!(c.role, remanence_chaos::model::DeviceRole::Drive { bay: b } if b == bay)
        })
        .count()
}

/// P5 of decision R1c-2, on the daemon's own constructor. An earlier process left a LOAD on the
/// bay with no outcome, or with an Unknown one, and the daemon restarts over it. Startup
/// reconciles the library from a fresh read of the changer before any drive is opened, so by the
/// time a write could be admitted the bay's intent is resolved, and no motion command was sent
/// to do it. If the changer cannot be read, the daemon refuses to start instead.
#[test]
fn restart_reconciles_an_open_or_unknown_load_before_it_can_admit_a_write() {
    use remanence_library::{transport::TransferOutcome, ScsiError, TimeoutClass};
    for unknown in [false, true] {
        let root = root();
        let world = seated_world();
        let load = MotionCommand::Load(key(BAY_A));
        {
            let state = root.open();
            let runtime = state.library_runtime();
            let ticket = intent(&runtime, &load);
            if unknown {
                runtime.unconfirmed(&pending(&load, ticket)).unwrap();
            }
            assert!(runtime.endpoint_motion_unresolved(&key(BAY_A)));
        }
        let (state, _api, _runtime) = start_daemon(&root, &world, plain)
            .unwrap_or_else(|status| panic!("unknown={unknown}: {}", status.message()));
        let runtime = state.library_runtime();
        assert!(
            !runtime.endpoint_motion_unresolved(&key(BAY_A)),
            "unknown={unknown}: startup resolved the bay before serving"
        );
        let index = state.write_handle();
        crate::startup_media_readiness::ensure_media_readiness_admitted(
            &index,
            Some(&runtime),
            "open session",
            SERIAL,
            Some(BAY_A),
            None,
            false,
        )
        .unwrap();
        let log: Vec<u8> = world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .map(|c| c.opcode)
            .collect();
        assert!(log.contains(&0xb8), "a fresh read of the changer: {log:?}");
        assert!(
            !log.iter().any(|op| matches!(op, 0x1b | 0xa5)),
            "resolving sends no motion: {log:?}"
        );
        no_active_readiness_rows(&state);
    }

    // The changer cannot be read at the reconcile: the daemon does not start, so nothing is
    // admitted, and the error is the changer's, as it is.
    struct FailSecondElementStatus {
        inner: ModelTransport,
        seen: Arc<AtomicUsize>,
    }
    impl SgTransport for FailSecondElementStatus {
        fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
            if cdb[0] == 0xb8 && self.seen.fetch_add(1, Ordering::SeqCst) == 1 {
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
    let root = root();
    let world = seated_world();
    {
        let state = root.open();
        intent(&state.library_runtime(), &MotionCommand::Load(key(BAY_A)));
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let started = start_daemon(&root, &world, move |inner| {
        Box::new(FailSecondElementStatus {
            inner,
            seen: seen.clone(),
        })
    });
    let refused = match started {
        Err(refused) => refused,
        Ok(_) => panic!("a daemon that cannot reconcile does not start"),
    };
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(
        refused
            .message()
            .starts_with("startup motion reconciliation: ")
            && refused.message().contains("SG_IO transport error"),
        "{refused}"
    );
}
