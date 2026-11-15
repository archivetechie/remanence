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
use remanence_library::{transport::TransferOutcome, ScsiError, TimeoutClass};
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
    let ticket = intent(&t.runtime, &load);
    let refused = cfg.mounted_admission(&index, &drive).unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(
        refused.message().contains("mounted write blocked")
            && refused.message().contains("unresolved")
            && refused.message().contains("restart the daemon"),
        "{refused}"
    );
    t.runtime.reconcile_inventory(&library_state).unwrap();
    assert_eq!(
        cfg.mounted_admission(&index, &drive).unwrap_err().code(),
        tonic::Code::FailedPrecondition,
        "inventory reconciliation leaves an owned LOAD unresolved"
    );
    t.runtime.unconfirmed(&pending(&load, ticket)).unwrap();
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
    let snapshot = world.lock().unwrap().library_snapshot();
    start_daemon_with_snapshot(root, world, snapshot, wrap)
}

/// Start with the caller's discovery snapshot so host-link changes reach the
/// real constructor while model transports remain available.
fn start_daemon_with_snapshot(
    root: &Root,
    world: &Arc<Mutex<VirtualWorld>>,
    snapshot: Library,
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
        libraries: vec![snapshot],
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

// ---------------------------------------------------------------------------
//  Decision R1c-4: a waited LoadDrive closes its own LOAD intent
// ---------------------------------------------------------------------------

/// Run the daemon's `LoadDrive` with `wait_ready = true` over the typed runtime and the model
/// world, to Ready. The library and the config stay alive for the caller.
fn waited_load(
    root: &Root,
    world: &Arc<Mutex<VirtualWorld>>,
) -> (
    remanence_state::StateHandle,
    WriteOwnerConfig,
    remanence_library::LibraryHandle,
) {
    let state = root.open();
    let mut index = state.write_handle();
    let mut library = open_model_library(world.clone());
    // The daemon observes its drives before it serves; the readiness row names the drive
    // serial this records.
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from([SERIAL.to_string()]),
    )
    .unwrap();
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
            [0x33; 32],
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation, "load_drive");
    cfg.configure_operation_library(&mut library, &handle);
    super::robotics::run_load_sequence(&mut index, &cfg, &handle, &mut library, 0x400, BAY_A, true)
        .unwrap();
    (state, cfg, library)
}

fn waited_world() -> Arc<Mutex<VirtualWorld>> {
    let mut model = VirtualWorld::single_drive(SERIAL, BAY_A, "DRV-R1C2", 0x400, 1);
    model.put_tape_in_slot(0x400, "R1C2L01L9", VirtualTape::default());
    Arc::new(Mutex::new(model))
}

/// A cancelled service LOAD is reconciled before terminal publication, then resume frees MOVE.
#[test]
fn mr_b_service_cancel_resume_and_move_reconciles_load() {
    use crate::pb::daemon_server::Daemon;
    use crate::pb::library_service_server::LibraryService;
    use remanence_state::library_log as lib;

    /// Watch the real registry until the service publishes a terminal state.
    async fn terminal(api: &crate::ApiState, operation: Uuid) -> pb::OperationStatus {
        let mut watch = api.operations.watch(&operation).unwrap();
        tokio::time::timeout(StdDuration::from_secs(5), async {
            while let Some(status) = watch.next().await {
                let status = status.unwrap();
                if crate::operations::is_terminal(&status) {
                    return status;
                }
            }
            panic!("operation stream ended before terminal publication");
        })
        .await
        .expect("service operation became terminal")
    }

    let root = root();
    let mut model = VirtualWorld::single_drive(SERIAL, BAY_A, "DRV-R1C2", 0x400, 3);
    model.put_tape_in_slot(0x400, "R1C2L01L8", VirtualTape::default());
    model.put_tape_in_slot(0x401, "MRBMOVE1L8", VirtualTape::default());
    let world = Arc::new(Mutex::new(model));
    let snapshot = world.lock().unwrap().library_snapshot();
    let sense = Arc::new(Mutex::new(Some((2, 4, 1))));
    let (state, api, runtime) = start_daemon_with_snapshot(&root, &world, snapshot, {
        let sense = sense.clone();
        move |inner| {
            Box::new(LifecycleDrive {
                inner,
                sense: sense.clone(),
                fail_load: false,
            })
        }
    })
    .unwrap();
    let chain_uuid = state.library_runtime().manifest().library_uuid();
    let operation = runtime.block_on(async {
        let library = api.library_service();
        let response = library
            .load_drive(tonic::Request::new(pb::LoadDriveRequest {
                library_uuid: crate::library::library_uuid(SERIAL).to_vec(),
                slot_element_address: 0x400,
                drive_element_address: u32::from(BAY_A),
                no_wait: false,
                idempotency_key: None,
            }))
            .await
            .unwrap()
            .into_inner();
        let operation = Uuid::from_slice(&response.operation_id).unwrap();
        let mut watch = api.operations.watch(&operation).unwrap();
        tokio::time::timeout(StdDuration::from_secs(5), async {
            while let Some(status) = watch.next().await {
                let status = status.unwrap();
                assert!(
                    !crate::operations::is_terminal(&status),
                    "LOAD ended before readiness_poll: {status:?}"
                );
                if status.progress.get("phase").map(String::as_str) == Some("readiness_poll")
                    && status.progress.get("sense").map(String::as_str) == Some("02/04/01")
                {
                    return;
                }
            }
            panic!("LOAD never published readiness_poll");
        })
        .await
        .expect("waited LOAD reached its held not-ready poll");
        api.daemon_service()
            .cancel_operation(tonic::Request::new(pb::CancelOperationRequest {
                operation_id: operation.as_bytes().to_vec(),
                idempotency_key: None,
                force: false,
            }))
            .await
            .unwrap();
        terminal(&api, operation).await;
        assert!(
            !state
                .library_runtime()
                .endpoint_motion_unresolved(&key(BAY_A)),
            "terminal waited LOAD has no unresolved motion on its bay"
        );

        *sense.lock().unwrap() = None;
        library
            .resume_media_readiness(tonic::Request::new(pb::ResumeMediaReadinessRequest {
                operation_id: operation.as_bytes().to_vec(),
                timeout_seconds: 5,
                poll_interval_seconds: 1,
            }))
            .await
            .unwrap();
        let ready = terminal(&api, operation).await;
        assert_eq!(
            ready.state,
            pb::OperationState::Succeeded as i32,
            "{ready:?}"
        );
        assert_eq!(
            ready.progress.get("state").map(String::as_str),
            Some("ready")
        );
        assert_eq!(
            state
                .catalog()
                .media_readiness_operation(operation)
                .unwrap()
                .unwrap()
                .state,
            "ready"
        );
        let moved = library
            .move_medium(tonic::Request::new(pb::MoveMediumRequest {
                library_uuid: crate::library::library_uuid(SERIAL).to_vec(),
                source_element_address: 0x401,
                destination_element_address: 0x402,
                idempotency_key: None,
            }))
            .await
            .unwrap()
            .into_inner();
        let moved = terminal(&api, Uuid::from_slice(&moved.operation_id).unwrap()).await;
        assert_eq!(
            moved.state,
            pb::OperationState::Succeeded as i32,
            "{moved:?}"
        );
        operation
    });
    assert_eq!(drive_load_commands_on(&world, BAY_A), 1);
    runtime.block_on(api.shutdown_drive_pool()).unwrap();
    drop(api);
    drop(runtime);
    drop(state);
    wait_for_lifecycle_close(&root);
    let records =
        crate::tests::read_library_records(&root.paths, chain_uuid, &mut |r| Some(r.clone()))
            .unwrap_or_else(|_| panic!("library log must be readable after daemon shutdown"));
    let loads: Vec<_> = records
        .iter()
        .filter(|record| {
            record.motion_intent().is_some_and(|intent| {
                intent.parent_operation == Some(*operation.as_bytes())
                    && matches!(intent.motion, lib::Motion::Load(_))
            })
        })
        .collect();
    assert_eq!(loads.len(), 1);
    let load = loads[0].envelope.record_uuid;
    let outcomes: Vec<_> = records
        .iter()
        .enumerate()
        .filter_map(|(position, record)| {
            record
                .motion_outcome()
                .filter(|outcome| outcome.intent.record_uuid == load)
                .map(|outcome| (position, outcome))
        })
        .collect();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1.result, lib::MotionResult::Unknown);
    let reconciled = records
        .iter()
        .position(|record| {
            record.inventory_observation().is_some_and(|observation| {
                observation
                    .resolves
                    .0
                    .iter()
                    .any(|reference| reference.record_uuid == load)
            })
        })
        .expect("InventoryReconciled resolves the service LOAD");
    assert!(
        outcomes[0].0 < reconciled,
        "Unknown precedes InventoryReconciled"
    );
}

/// A waited `LoadDrive` that reaches Ready closes its readiness-owned LOAD: the endpoint has
/// nothing unresolved, every MotionIntent in the library log has a MotionOutcome, and the UNLOAD
/// the robotics executor runs next (the same call and context as its Unload arm) is admitted
/// and succeeds.
#[test]
fn waited_load_drive_closes_its_load_intent_and_the_unload_is_admitted() {
    let root = root();
    let world = waited_world();
    let library_uuid;
    {
        let (state, cfg, mut library) = waited_load(&root, &world);
        let runtime = state.library_runtime();
        library_uuid = runtime.manifest().library_uuid();
        assert!(
            !runtime.endpoint_motion_unresolved(&key(BAY_A)),
            "the waited LOAD is closed once the drive is Ready"
        );
        assert_eq!(drive_load_commands_on(&world, BAY_A), 1);

        let unload_operation = uuid::Uuid::from_bytes([0x34; 16]);
        let outcome = library
            .with_motion_context(
                remanence_library::motion::MotionContext {
                    parent_operation: Some(*unload_operation.as_bytes()),
                    ..Default::default()
                },
                |library| {
                    crate::mount_support::unload_library_bay_with_admission(
                        library,
                        BAY_A,
                        None,
                        &cfg.policy,
                        None,
                        std::path::Path::new(crate::mount_support::DEVICE_LOCK_ROOT),
                        &mut || Ok(()),
                    )
                },
            )
            .unwrap_or_else(|error| panic!("the UNLOAD after a waited LOAD was refused: {error}"));
        assert!(matches!(
            outcome,
            crate::mount_support::UnloadOutcome::Unloaded
        ));
        assert!(!runtime.endpoint_motion_unresolved(&key(BAY_A)));
    }
    // Every MotionIntent has a MotionOutcome. The log is locked while the runtime lives, so
    // read it back once the state is gone.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (intents, outcomes) = loop {
        match crate::tests::read_library_records(&root.paths, library_uuid, &mut |record| {
            if let Some(intent) = record.motion_intent() {
                return Some((
                    Some((record.envelope.record_uuid, intent.motion.clone())),
                    None,
                ));
            }
            record
                .motion_outcome()
                .map(|outcome| (None, Some(outcome.intent.record_uuid)))
        }) {
            Ok(records) => {
                let intents: Vec<_> = records.iter().filter_map(|r| r.0.clone()).collect();
                let outcomes: Vec<_> = records.iter().filter_map(|r| r.1).collect();
                break (intents, outcomes);
            }
            Err(crate::tests::ChainRead::Busy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(crate::tests::ChainRead::Busy(errno)) => {
                panic!("library log still locked: {errno}")
            }
            Err(crate::tests::ChainRead::Failed(message)) => panic!("{message}"),
        }
    };
    assert!(
        intents.len() >= 4,
        "MOVE and LOAD, then UNLOAD and MOVE: {intents:?}"
    );
    for (uuid, motion) in &intents {
        assert!(
            outcomes.contains(uuid),
            "MotionIntent {motion:?} has no MotionOutcome"
        );
    }
}

/// After a waited LOAD, the endpoint's own R1c-3 predicate sees nothing unresolved, so a write
/// admission on the bay is admitted.
#[test]
fn waited_load_drive_leaves_the_bay_admissible_for_a_write() {
    let root = root();
    let world = waited_world();
    let (state, _cfg, _library) = waited_load(&root, &world);
    let runtime = state.library_runtime();
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
    .unwrap_or_else(|error| panic!("write admission refused: {}", error.message()));
}

/// A1: an unowned successor fence survives the daemon's real startup twice
/// without another readiness observation, and remains explicitly releasable.
#[test]
fn readiness_successor_fence_survives_two_daemon_starts_and_release() {
    use remanence_state::{MediaReadinessOperationInput, MediaReadinessTransitionInput};
    let root = root();
    let world = seated_world();
    let transition = |id, phase: &str, state: &str| MediaReadinessTransitionInput {
        operation_id: id,
        phase: Some(phase.into()),
        state: state.into(),
        dirty_scope: Some("drive+tape".into()),
        last_cdb_opcode: None,
        completion_evidence: None,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: None,
        cancel_source: None,
        signal: None,
        evidence_path: None,
        last_error_json: None,
        quarantine_id: None,
    };
    let (library_uuid, predecessor, fenced, row) = {
        let state = root.open();
        let mut catalog = state.write_handle();
        let library = world.lock().unwrap().library_snapshot();
        crate::drive_collection::observe_drive_catalog_from_libraries(
            &mut catalog,
            [&library],
            &HashSet::from([SERIAL.to_string()]),
        )
        .unwrap();
        let owner = catalog
            .begin_typed_media_readiness_operation(MediaReadinessOperationInput {
                operation_id: uuid::Uuid::new_v4(),
                run_id: None,
                endpoint: key(BAY_A),
                changer_sg: None,
                drive_sg: None,
                drive_serial: Some("DRV-R1C2".into()),
                barcode: Some("R1C2A01L9".into()),
                source_slot: Some(0x400),
                media_generation: Some(9),
                media_family: Some(remanence_state::LtoGen::Lto9),
                phase: "readiness_poll".into(),
                state: "planned".into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        let predecessor = uuid::Uuid::parse_str(&owner.operation_id).unwrap();
        let mut ready = transition(predecessor, "wait_ready_tur", "ready");
        ready.dirty_scope = Some("none".into());
        ready.last_cdb_opcode = Some(0);
        ready.completion_evidence = Some((&CompletionEvidence::success()).into());
        catalog.record_media_readiness_transition(ready).unwrap();
        let mut fence = transition(predecessor, "clobber_confirmation", "aborted_unknown");
        fence.quarantine_id = Some(format!("mrq-{predecessor}"));
        fence.signal = Some("SIGINT".into());
        fence.cancel_source = Some("signal".into());
        let fenced = catalog.record_media_readiness_fence(fence).unwrap();
        assert_ne!(fenced, predecessor);
        let row = catalog.media_readiness_operation(fenced).unwrap().unwrap();
        assert_eq!(row.state, "aborted_unknown");
        assert_eq!(row.phase, "clobber_confirmation");
        assert_eq!(row.dirty_scope.as_deref(), Some("drive+tape"));
        assert_eq!(row.quarantine_id, Some(format!("mrq-{fenced}")));
        (
            state.library_runtime().manifest().library_uuid(),
            predecessor,
            fenced,
            row,
        )
    };
    // Read authenticated source records only after the state owner is closed.
    let observations = || {
        // The projection worker can retain the authority briefly after StateHandle drops,
        // as in the adjacent waited-load source-replay test. Bound the teardown wait.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match crate::tests::read_library_records(&root.paths, library_uuid, &mut |record| {
                record.readiness_observation().cloned()
            }) {
                Ok(records) => break records,
                Err(crate::tests::ChainRead::Busy(_)) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(crate::tests::ChainRead::Busy(errno)) => {
                    panic!("closed readiness source remains locked: {errno}")
                }
                Err(crate::tests::ChainRead::Failed(message)) => panic!("{message}"),
            }
        }
    };
    let before = observations();
    assert_eq!(before.len(), 3, "owner, Ready, successor fence");
    for start in 1..=2 {
        let (state, api, runtime) = start_daemon(&root, &world, plain)
            .unwrap_or_else(|error| panic!("startup {start} refused the successor fence: {error}"));
        assert_eq!(
            state
                .catalog()
                .media_readiness_operation(fenced)
                .unwrap()
                .unwrap(),
            row
        );
        assert_eq!(
            state
                .catalog()
                .media_readiness_operation(predecessor)
                .unwrap()
                .unwrap()
                .state,
            "ready"
        );
        runtime.block_on(api.shutdown_drive_pool()).unwrap();
        drop(api);
        drop(runtime);
        drop(state);
        assert_eq!(observations(), before, "startup {start} appended readiness");
    }
    {
        let state = root.open();
        let mut catalog = state.write_handle();
        let mut release = transition(fenced, "quarantine_release", "released");
        release.dirty_scope = Some("none".into());
        release.cancel_source = Some("operator".into());
        release.quarantine_id = Some(format!("mrq-{fenced}"));
        catalog.record_media_readiness_transition(release).unwrap();
        assert_eq!(
            catalog
                .media_readiness_operation(fenced)
                .unwrap()
                .unwrap()
                .state,
            "released"
        );
        assert!(catalog
            .list_active_media_readiness_operations(None)
            .unwrap()
            .is_empty());
    }
    let released = observations();
    assert_eq!(released.len(), before.len() + 1);
    assert_eq!(
        released.last().unwrap().result,
        remanence_state::library_log::ReadinessResult::Released
    );
}

// ---------------------------------------------------------------------------
// R1c §7.1: a failed LoadDrive retains its independent readiness chain.
// ---------------------------------------------------------------------------

/// Keep the model's command log, while reporting the drive's controlled SSC outcome.
struct LifecycleDrive {
    inner: ModelTransport,
    sense: Arc<Mutex<Option<(u8, u8, u8)>>>,
    fail_load: bool,
}
impl SgTransport for LifecycleDrive {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let result = self.inner.execute_none(cdb);
        if self.inner.role() == remanence_chaos::model::DeviceRole::Changer {
            return result;
        }
        if cdb.first() == Some(&0x1b)
            && cdb.get(4).is_some_and(|flags| flags & 1 != 0)
            && self.fail_load
        {
            return Err(ScsiError::CheckCondition {
                sense: readiness_fixed_sense(5, 0x20, 0),
                bytes_transferred: 0,
            });
        }
        if cdb.first() == Some(&0) && result.is_ok() {
            if let Some((key, asc, ascq)) = *self.sense.lock().unwrap() {
                return Err(ScsiError::CheckCondition {
                    sense: readiness_fixed_sense(key, asc, ascq),
                    bytes_transferred: 0,
                });
            }
        }
        result
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

/// LTO-8 keeps 02/04/01 in BecomingReady; LTO-9 labels it MediaInitializing.
fn lifecycle_world() -> Arc<Mutex<VirtualWorld>> {
    let mut model = VirtualWorld::single_drive(SERIAL, BAY_A, "DRV-R1C2", 0x400, 1);
    model.put_tape_in_slot(0x400, "R1C2L01L8", VirtualTape::default());
    Arc::new(Mutex::new(model))
}

/// Exercise the production owned begin and terminal event, interrupted after a real poll.
fn failed_waited_load(
    root: &Root,
    world: &Arc<Mutex<VirtualWorld>>,
    fail_load: bool,
    cancel: bool,
) -> (Uuid, [u8; 16], u32) {
    let state = root.open();
    let mut index = state.write_handle();
    let model = world.lock().unwrap().library_snapshot();
    let open_world = world.clone();
    let mut library = model
        .open_with(
            &remanence_library::StaticAllowlist::new([SERIAL]),
            move |path| {
                let role = open_world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(LifecycleDrive {
                    inner: ModelTransport::new(open_world.clone(), role),
                    sense: Arc::new(Mutex::new(Some((2, 4, 1)))),
                    fail_load,
                }) as Box<dyn SgTransport>)
            },
        )
        .unwrap();
    crate::drive_collection::observe_drive_catalog_from_libraries(
        &mut index,
        [library.library()],
        &HashSet::from([SERIAL.to_string()]),
    )
    .unwrap();
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
            [0x35; 32],
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    let handle = registry
        .register(operation, "load_drive")
        .with_readiness_lifetime(
            shutdown.clone(),
            Arc::new(crate::drive_pool::ReadinessWake::default()),
        );
    cfg.configure_operation_library(&mut library, &handle);
    let before_shutdown = std::thread::scope(|scope| {
        let interrupt = (!fail_load).then(|| {
            scope.spawn(|| {
                let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
                while !registry
                    .latest(&operation)
                    .is_some_and(|s| s.progress.contains_key("sense"))
                {
                    if std::time::Instant::now() >= deadline {
                        shutdown.store(true, Ordering::Release);
                        panic!("LoadDrive never reached its first readiness poll");
                    }
                    std::thread::sleep(StdDuration::from_millis(5));
                }
                let row = state
                    .catalog()
                    .media_readiness_operation(operation)
                    .unwrap()
                    .unwrap();
                if cancel {
                    registry.request_cancel(&operation).unwrap();
                } else {
                    shutdown.store(true, Ordering::Release);
                }
                row
            })
        });
        let result = super::robotics::run_load_sequence(
            &mut index,
            &cfg,
            &handle,
            &mut library,
            0x400,
            BAY_A,
            true,
        );
        assert!(result.is_err(), "the waited LOAD must end not ready");
        super::robotics::finish_library_operation(
            &mut index,
            &cfg,
            &handle,
            SERIAL,
            result,
            Ok(()),
        );
        interrupt.map(|thread| thread.join().unwrap())
    });
    let terminal = registry.latest(&operation).unwrap();
    assert_eq!(terminal.state, pb::OperationState::Failed as i32);
    if !fail_load && !cancel {
        assert_eq!(
            terminal.progress.get("phase").map(String::as_str),
            Some("shutdown")
        );
        assert_eq!(
            index.media_readiness_operation(operation).unwrap(),
            before_shutdown,
            "shutdown and owner failure append no readiness observation"
        );
    }
    let row = index.media_readiness_operation(operation).unwrap().unwrap();
    if !fail_load {
        assert_eq!(row.state, "becoming_ready");
    }
    if cancel {
        assert_eq!(row.cancel_source.as_deref(), Some("operator"));
    }
    assert_eq!(
        index
            .get_operation(&operation.to_string())
            .unwrap()
            .unwrap()
            .state,
        "failed"
    );
    let runtime = state.library_runtime();
    (
        operation,
        runtime.manifest().library_uuid(),
        runtime
            .readiness_load_attempts(*operation.as_bytes())
            .unwrap(),
    )
}

/// Wait for background owners to release the state root before reopening it in this test.
fn wait_for_lifecycle_close(root: &Root) {
    let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    loop {
        match remanence_state::StateLockGuard::acquire(&root.paths.state_dir) {
            Ok(guard) => {
                drop(guard);
                return;
            }
            Err(error) if error.is_state_lock_held() && std::time::Instant::now() < deadline => {
                std::thread::sleep(StdDuration::from_millis(10));
            }
            Err(error) => panic!("state root remains locked after shutdown: {error}"),
        }
    }
}

/// Read typed owner/readiness chains after their process releases the log lock.
fn lifecycle_history(
    root: &Root,
    library: [u8; 16],
    id: Uuid,
) -> (
    Vec<remanence_state::library_log::OperationRecorded>,
    Vec<remanence_state::library_log::ReadinessObserved>,
) {
    wait_for_lifecycle_close(root);
    let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    loop {
        match crate::tests::read_library_records(&root.paths, library, &mut |record| {
            let owner = record
                .operation_recorded()
                .filter(|v| v.operation == *id.as_bytes())
                .cloned();
            let ready = record
                .readiness_observation()
                .filter(|v| v.operation == *id.as_bytes())
                .cloned();
            (owner.is_some() || ready.is_some()).then_some((owner, ready))
        }) {
            Ok(records) => {
                return (
                    records.iter().filter_map(|r| r.0.clone()).collect(),
                    records.into_iter().filter_map(|r| r.1).collect(),
                )
            }
            Err(crate::tests::ChainRead::Busy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(StdDuration::from_millis(10));
            }
            Err(crate::tests::ChainRead::Busy(errno)) => {
                panic!("library log remains locked: {errno}")
            }
            Err(crate::tests::ChainRead::Failed(message)) => panic!("{message}"),
        }
    }
}

/// Release through the catalog transition used by quarantine release, refunding the count.
fn release_lifecycle_owner(root: &Root, id: Uuid) {
    wait_for_lifecycle_close(root);
    let state = root.open();
    let mut index = state.write_handle();
    let release = remanence_state::MediaReadinessTransitionInput {
        operation_id: id,
        phase: Some("quarantine_release".into()),
        state: "released".into(),
        dirty_scope: Some("none".into()),
        cancel_source: Some("operator".into()),
        last_cdb_opcode: None,
        completion_evidence: None,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: None,
        signal: None,
        evidence_path: None,
        last_error_json: None,
        quarantine_id: None,
    };
    index.record_media_readiness_transition(release).unwrap();
    assert_eq!(
        state
            .library_runtime()
            .readiness_load_attempts(*id.as_bytes())
            .unwrap(),
        0
    );
    assert_eq!(
        index.media_readiness_operation(id).unwrap().unwrap().state,
        "released"
    );
}

/// A failed SSC LOAD leaves a typed Failed owner that can restart twice and release.
#[test]
fn readiness_owner_failed_load_restarts_twice_and_releases() {
    let root = root();
    let world = lifecycle_world();
    let (id, library, attempts) = failed_waited_load(&root, &world, true, false);
    let (owner, _) = lifecycle_history(&root, library, id);
    assert!(matches!(
        owner.last().unwrap().event,
        remanence_state::library_log::OperationEvent::Failed(_)
    ));
    for start in 1..=2 {
        let (state, api, runtime) = start_daemon(&root, &world, |inner| {
            Box::new(LifecycleDrive {
                inner,
                sense: Arc::new(Mutex::new(Some((2, 4, 1)))),
                fail_load: false,
            })
        })
        .unwrap_or_else(|error| panic!("startup {start} refused failed LOAD readiness: {error}"));
        assert_eq!(attempts, 1);
        assert_eq!(
            state
                .library_runtime()
                .readiness_load_attempts(*id.as_bytes())
                .unwrap(),
            1
        );
        runtime.block_on(api.shutdown_drive_pool()).unwrap();
        drop(api);
        drop(runtime);
        drop(state);
        wait_for_lifecycle_close(&root);
    }
    release_lifecycle_owner(&root, id);
}

/// Operator cancellation survives two reconciles and remains excluded from auto-resume.
#[test]
fn readiness_owner_operator_cancel_restarts_twice_and_releases() {
    let root = root();
    let world = lifecycle_world();
    let (id, library, attempts) = failed_waited_load(&root, &world, false, true);
    let (owner, observations) = lifecycle_history(&root, library, id);
    assert!(matches!(
        owner.last().unwrap().event,
        remanence_state::library_log::OperationEvent::Failed(_)
    ));
    assert_eq!(
        observations.last().unwrap().cancel_source,
        Some(remanence_state::library_log::CancelSource::Operator)
    );
    for start in 1..=2 {
        let (state, api, runtime) = start_daemon(&root, &world, |inner| {
            Box::new(LifecycleDrive {
                inner,
                sense: Arc::new(Mutex::new(Some((2, 4, 1)))),
                fail_load: false,
            })
        })
        .unwrap_or_else(|error| panic!("startup {start} refused cancelled readiness: {error}"));
        assert_eq!(attempts, 1);
        assert_eq!(
            state
                .library_runtime()
                .readiness_load_attempts(*id.as_bytes())
                .unwrap(),
            1
        );
        let readiness_wake = &api.drive_pool().unwrap().readiness_wake;
        let scans = readiness_wake.scans.load(Ordering::Relaxed);
        let start_attempts = readiness_wake.start_attempts.load(Ordering::Relaxed);
        readiness_wake.notify.notify_one();
        runtime.block_on(async {
            tokio::time::timeout(StdDuration::from_secs(5), async {
                // Scans count entry, so two more entries prove a full sweep completed.
                while readiness_wake.scans.load(Ordering::Relaxed) < scans + 2 {
                    tokio::time::sleep(StdDuration::from_millis(10)).await;
                }
            })
            .await
            .expect("auto-resume must scan the cancelled row");
        });
        assert!(
            api.operations.latest(&id).is_none(),
            "auto-resume must skip operator cancellation"
        );
        assert_eq!(
            readiness_wake.start_attempts.load(Ordering::Relaxed),
            start_attempts,
            "auto-resume must not attempt to start an operator-cancelled row"
        );
        assert_eq!(
            state
                .catalog()
                .media_readiness_operation(id)
                .unwrap()
                .unwrap()
                .cancel_source
                .as_deref(),
            Some("operator")
        );
        runtime.block_on(api.shutdown_drive_pool()).unwrap();
        drop(api);
        drop(runtime);
        drop(state);
        wait_for_lifecycle_close(&root);
    }
    release_lifecycle_owner(&root, id);
}

/// Shutdown leaves the last poll intact; auto-resume can finish without rewriting the owner.
#[test]
fn readiness_owner_shutdown_restarts_twice_and_auto_resumes() {
    let root = root();
    let world = lifecycle_world();
    let (id, library, attempts) = failed_waited_load(&root, &world, false, false);
    let (owner, observations) = lifecycle_history(&root, library, id);
    assert!(matches!(
        owner.last().unwrap().event,
        remanence_state::library_log::OperationEvent::Failed(_)
    ));
    assert_eq!(
        observations.len(),
        2,
        "planned and first poll, no shutdown observation"
    );
    assert_eq!(
        observations.last().unwrap().state,
        remanence_state::library_log::ReadinessState::BecomingReady
    );
    assert_eq!(observations.last().unwrap().cancel_source, None);
    let loads = drive_load_commands_on(&world, BAY_A);
    assert_eq!(loads, 1);
    for start in 1..=2 {
        let sense = Arc::new(Mutex::new(Some((2, 4, 1))));
        let drive_sense = sense.clone();
        let (state, api, runtime) = start_daemon(&root, &world, move |inner| {
            Box::new(LifecycleDrive {
                inner,
                sense: drive_sense.clone(),
                fail_load: false,
            })
        })
        .unwrap_or_else(|error| panic!("startup {start} refused shutdown readiness: {error}"));
        assert_eq!(attempts, 1);
        assert_eq!(
            state
                .library_runtime()
                .readiness_load_attempts(*id.as_bytes())
                .unwrap(),
            1
        );
        runtime.block_on(async {
            tokio::time::timeout(StdDuration::from_secs(5), async {
                while !api.operations.latest(&id).is_some_and(|status| {
                    status.operation_kind == "media_readiness_wait"
                        && status.progress.contains_key("sense")
                }) {
                    tokio::time::sleep(StdDuration::from_millis(10)).await;
                }
            })
            .await
            .expect("auto-resume must admit the stranded LoadDrive id");
            if start == 2 {
                *sense.lock().unwrap() = None;
                tokio::time::timeout(StdDuration::from_secs(45), async {
                    while state
                        .catalog()
                        .media_readiness_operation(id)
                        .unwrap()
                        .unwrap()
                        .state
                        != "ready"
                    {
                        tokio::time::sleep(StdDuration::from_millis(10)).await;
                    }
                })
                .await
                .expect("auto-resume must record Ready");
                let status = pb::daemon_server::Daemon::get_operation(
                    &api.daemon_service(),
                    tonic::Request::new(pb::GetOperationRequest {
                        operation_id: id.as_bytes().to_vec(),
                    }),
                )
                .await
                .unwrap()
                .into_inner();
                assert_eq!(status.operation_kind, "load_drive");
                assert_eq!(
                    status.state,
                    pb::OperationState::Failed as i32,
                    "registry reuse must preserve durable LoadDrive failure"
                );
            }
            api.shutdown_drive_pool().await.unwrap();
        });
        assert_eq!(
            drive_load_commands_on(&world, BAY_A),
            loads,
            "auto-resume sends no SSC LOAD"
        );
        drop(api);
        drop(runtime);
        drop(state);
        wait_for_lifecycle_close(&root);
    }
    let (after_owner, after_readiness) = lifecycle_history(&root, library, id);
    assert_eq!(
        after_owner, owner,
        "auto-resume cannot overwrite the typed owner chain"
    );
    assert_eq!(
        after_readiness.last().unwrap().result,
        remanence_state::library_log::ReadinessResult::Ready
    );
}

/// A stranded owner's spent budget applies to both startup's probe and the resumed daemon poll.
#[test]
fn readiness_owner_shutdown_keeps_initializing_load_budget_spent() {
    let root = root();
    let world = lifecycle_world();
    let (id, library, _) = failed_waited_load(&root, &world, false, false);
    let loads = drive_load_commands_on(&world, BAY_A);
    assert_eq!(loads, 1);
    let (state, api, runtime) = start_daemon(&root, &world, |inner| {
        Box::new(LifecycleDrive {
            inner,
            sense: Arc::new(Mutex::new(Some((2, 4, 2)))),
            fail_load: false,
        })
    })
    .expect("startup must probe the stranded row");
    assert_eq!(
        drive_load_commands_on(&world, BAY_A),
        loads,
        "startup must not refund the LOAD budget"
    );
    runtime.block_on(async {
        let initializing_attempts = tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                if let Some(status) = api.operations.latest(&id) {
                    if status.operation_kind == "media_readiness_wait"
                        && status
                            .progress
                            .get("sense")
                            .is_some_and(|v| v == "02/04/02")
                    {
                        break status
                            .progress
                            .get("attempts")
                            .expect("readiness poll must publish attempts")
                            .parse::<u64>()
                            .expect("readiness attempts must be a count");
                    }
                }
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("auto-resume must poll initializing-command-required");
        tokio::time::timeout(StdDuration::from_secs(5), async {
            // A later poll follows the initializing LOAD settle and possible dispatch.
            while !api.operations.latest(&id).is_some_and(|status| {
                status.operation_kind == "media_readiness_wait"
                    && status
                        .progress
                        .get("attempts")
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_some_and(|attempts| attempts > initializing_attempts)
            }) {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("auto-resume must poll again after the initializing LOAD settle");
        assert_eq!(
            drive_load_commands_on(&world, BAY_A),
            loads,
            "daemon poll must not refund the LOAD budget"
        );
        api.shutdown_drive_pool().await.unwrap();
    });
    assert_eq!(drive_load_commands_on(&world, BAY_A), loads);
    drop(api);
    drop(runtime);
    drop(state);
    wait_for_lifecycle_close(&root);
    let (_, observations) = lifecycle_history(&root, library, id);
    assert!(
        observations.iter().any(|value| value.phase
            == remanence_state::library_log::ReadinessPhase::StartupReconcileTur),
        "the startup probe is durable"
    );
}

// WBR-K16-FX1: refusal before the drive LOAD's admission must leave no readiness chain.

/// Exercise a refused waited load through the typed owner and persist its Failed outcome.
fn fx1_refused_waited_load(
    root: &Root,
    state: &remanence_state::StateHandle,
    library: &mut remanence_library::LibraryHandle,
    observe_drive: bool,
) -> Uuid {
    let mut index = state.write_handle();
    if observe_drive {
        crate::drive_collection::observe_drive_catalog_from_libraries(
            &mut index,
            [library.library()],
            &HashSet::from([SERIAL.to_string()]),
        )
        .unwrap();
    }
    let mut cfg = test_write_owner_config(
        root.paths.sqlite_path.clone(),
        root.paths.audit_dir.clone(),
        library,
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
            [0x36; 32],
        )
        .unwrap();
    let registry = crate::operations::OperationRegistry::default();
    let handle = registry.register(operation, "load_drive");
    cfg.configure_operation_library(library, &handle);
    let result =
        super::robotics::run_load_sequence(&mut index, &cfg, &handle, library, 0x400, BAY_A, true);
    assert!(result.is_err(), "the fixture must refuse the waited load");
    super::robotics::finish_library_operation(&mut index, &cfg, &handle, SERIAL, result, Ok(()));
    assert_eq!(
        registry.latest(&operation).unwrap().state,
        pb::OperationState::Failed as i32
    );
    assert_eq!(
        index
            .get_operation(&operation.to_string())
            .unwrap()
            .unwrap()
            .state,
        "failed"
    );
    operation
}

/// T2: an empty-slot refusal leaves nothing for startup to quarantine.
#[test]
fn fx1_waited_empty_slot_leaves_no_readiness_row_across_restart() {
    let root = root();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        SERIAL, BAY_A, "DRV-R1C2", 0x400, 1,
    )));
    {
        let state = root.open();
        let mut library = open_model_library(world.clone());
        fx1_refused_waited_load(&root, &state, &mut library, true);
        no_active_readiness_rows(&state);
    }
    let (state, api, runtime) = start_daemon(&root, &world, plain)
        .unwrap_or_else(|status| panic!("restart refused: {}", status.message()));
    no_active_readiness_rows(&state);
    runtime.block_on(api.shutdown_drive_pool()).unwrap();
}

/// T3: the single begin lies strictly between this operation's MOVE Done and LOAD intent.
#[test]
fn fx1_waited_load_begin_follows_move_done_and_precedes_load_intent() {
    use remanence_state::library_log as lib;
    let root = root();
    let world = waited_world();
    let uuid = {
        let (state, _cfg, _library) = waited_load(&root, &world);
        state.library_runtime().manifest().library_uuid()
    };
    wait_for_lifecycle_close(&root);
    let records = crate::tests::read_library_records(&root.paths, uuid, &mut |r| Some(r.clone()))
        .unwrap_or_else(|_| panic!("library log must be readable after owner closes"));
    let (first, readiness) = records
        .iter()
        .enumerate()
        .find_map(|(i, r)| r.readiness_observation().map(|v| (i, v)))
        .expect("waited load has a readiness observation");
    let operation = readiness.operation;
    let begins: Vec<_> = records
        .iter()
        .filter_map(|r| r.readiness_observation())
        .filter(|v| v.operation == operation && v.state == lib::ReadinessState::Planned)
        .collect();
    assert_eq!(begins.len(), 1, "exactly one readiness begin");
    let move_id = records
        .iter()
        .find_map(|r| {
            r.motion_intent()
                .filter(|v| {
                    v.parent_operation == Some(operation)
                        && matches!(v.motion, lib::Motion::Move(_))
                })
                .map(|_| r.envelope.record_uuid)
        })
        .expect("owned MOVE intent");
    let done = records
        .iter()
        .position(|r| {
            r.motion_outcome().is_some_and(|v| {
                v.intent.record_uuid == move_id && v.result == lib::MotionResult::Done
            })
        })
        .expect("MOVE completed Done");
    let load = records
        .iter()
        .position(|r| {
            r.motion_intent().is_some_and(|v| {
                v.parent_operation == Some(operation) && matches!(v.motion, lib::Motion::Load(_))
            })
        })
        .expect("owned LOAD intent");
    assert!(done < first && first < load,
        "readiness begin must follow MOVE Done and precede LOAD intent: done={done}, begin={first}, load={load}");
}

/// Fail the actual MOVE dispatch with a definite SCSI source-empty completion.
struct Fx1FailMove {
    inner: ModelTransport,
    attempts: Arc<AtomicUsize>,
}
impl SgTransport for Fx1FailMove {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb.first() == Some(&0xa5) {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            return Err(ScsiError::CheckCondition {
                sense: readiness_fixed_sense(5, 0x3b, 0x0e),
                bytes_transferred: 0,
            });
        }
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

/// T4: a definite failure of the MOVE CDB cannot strand a readiness begin.
#[test]
fn fx1_waited_move_check_condition_leaves_no_readiness_row() {
    let root = root();
    let world = waited_world();
    let state = root.open();
    let attempts = Arc::new(AtomicUsize::new(0));
    let snapshot = world.lock().unwrap().library_snapshot();
    let mut library = snapshot
        .open_with(&remanence_library::StaticAllowlist::new([SERIAL]), {
            let world = world.clone();
            let attempts = attempts.clone();
            move |path| {
                let role = world.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(Fx1FailMove {
                    inner: ModelTransport::new(world.clone(), role),
                    attempts: attempts.clone(),
                }) as Box<dyn SgTransport>)
            }
        })
        .unwrap();
    fx1_refused_waited_load(&root, &state, &mut library, true);
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "MOVE CDB attempted");
    no_active_readiness_rows(&state);
}

/// T5: an installed drive without a device path refuses after MOVE, before the begin.
#[test]
fn fx1_waited_unopenable_drive_leaves_no_readiness_row_across_restart() {
    let root = root();
    let world = waited_world();
    {
        let state = root.open();
        let mut snapshot = world.lock().unwrap().library_snapshot();
        snapshot.drive_bays[0].installed.as_mut().unwrap().sg_path = None;
        let mut library = snapshot
            .open_with(&remanence_library::StaticAllowlist::new([SERIAL]), {
                let world = world.clone();
                move |path| {
                    let role = world.lock().unwrap().role_for_path(path).unwrap();
                    Ok(Box::new(ModelTransport::new(world.clone(), role)) as Box<dyn SgTransport>)
                }
            })
            .unwrap();
        fx1_refused_waited_load(&root, &state, &mut library, true);
        assert_eq!(drive_load_commands_on(&world, BAY_A), 0, "no LOAD CDB");
        assert_eq!(
            world.lock().unwrap().loaded_barcode(BAY_A),
            Some("R1C2L01L9")
        );
        no_active_readiness_rows(&state);
    }
    let (state, api, runtime) = start_daemon(&root, &world, plain)
        .unwrap_or_else(|status| panic!("restart refused: {}", status.message()));
    no_active_readiness_rows(&state);
    let index = state.write_handle();
    crate::startup_media_readiness::ensure_media_readiness_admitted(
        &index,
        Some(&state.library_runtime()),
        "move_medium",
        SERIAL,
        None,
        None,
        true,
    )
    .unwrap();
    runtime.block_on(api.shutdown_drive_pool()).unwrap();
}

/// T6: identity refusal at the LOAD admission leaves the moved cartridge and PartialFailure.
#[test]
fn fx1_waited_begin_refusal_leaves_moved_cartridge_without_readiness_row() {
    let root = root();
    let world = waited_world();
    let state = root.open();
    let mut library = open_model_library(world.clone());
    fx1_refused_waited_load(&root, &state, &mut library, false);
    assert!(
        world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .any(|c| c.opcode == 0xa5),
        "MOVE CDB completed before begin refusal"
    );
    assert_eq!(
        world.lock().unwrap().loaded_barcode(BAY_A),
        Some("R1C2L01L9")
    );
    assert_eq!(drive_load_commands_on(&world, BAY_A), 0, "no LOAD CDB");
    assert_eq!(
        library.dirty_cause(),
        Some(remanence_library::DirtyCause::PartialFailure)
    );
    no_active_readiness_rows(&state);
}

/// T7 measures startup's W3 behavior with MOVE Done, an owned begin and no LOAD intent.
#[test]
fn fx1_measure_startup_after_move_done_and_begin_without_load_intent() {
    let root = root();
    let world = waited_world();
    let operation;
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
        let mut cfg = test_write_owner_config(
            root.paths.sqlite_path.clone(),
            root.paths.audit_dir.clone(),
            &library,
            library_snapshot_cell(library.library().clone()),
        );
        cfg.catalog = state.projection_client();
        cfg.authority = crate::write_owner::TerminalAuthority::Typed(state.library_runtime());
        let api = crate::ApiState::new(state.projection_client());
        operation = api
            .accept_owner_request(
                AuditActor::User("loader".into()),
                "load_drive",
                "library",
                SERIAL.into(),
                [0x37; 32],
            )
            .unwrap();
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(operation, "load_drive");
        cfg.configure_operation_library(&mut library, &handle);
        library
            .with_motion_context(
                MotionContext {
                    parent_operation: Some(*operation.as_bytes()),
                    ..Default::default()
                },
                |library| library.move_medium(0x400, BAY_A, &cfg.policy),
            )
            .unwrap();
        world.lock().unwrap().leave_cartridge_unthreaded(BAY_A);
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id: operation,
                run_id: None,
                endpoint: key(BAY_A),
                changer_sg: Some(library.library().changer_sg.display().to_string()),
                drive_sg: library.library().drive_bays[0]
                    .installed
                    .as_ref()
                    .unwrap()
                    .sg_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
                drive_serial: Some("DRV-R1C2".into()),
                barcode: Some("R1C2L01L9".into()),
                source_slot: Some(0x400),
                media_generation: None,
                media_family: None,
                phase: "load_drive_readiness".into(),
                state: "planned".into(),
                dirty_scope: Some("drive+tape".into()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .unwrap();
        assert_eq!(drive_load_commands_on(&world, BAY_A), 0);
    }
    match start_daemon(&root, &world, plain) {
        Ok((state, api, runtime)) => {
            let row = state
                .catalog()
                .media_readiness_operation(operation)
                .unwrap()
                .unwrap();
            println!(
                "T7 W3 startup: LOAD CDBs={}, readiness={}",
                drive_load_commands_on(&world, BAY_A),
                row.state
            );
            runtime.block_on(api.shutdown_drive_pool()).unwrap();
        }
        Err(status) => println!(
            "T7 W3 startup refused: {status}; LOAD CDBs={}",
            drive_load_commands_on(&world, BAY_A)
        ),
    }
}

/// T13: three real daemon constructors retain identity across a missing host
/// connection, using the established pool shutdown and state-lock teardown.
#[test]
fn k18_daemon_restarts_across_missing_host_details() {
    let root = root();
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        SERIAL, BAY_A, "DRV-K18", 0x400, 1,
    )));
    let mut identity = None;
    for (start, present) in [true, false, true].into_iter().enumerate() {
        let mut snapshot = world.lock().unwrap().library_snapshot();
        let installed = snapshot.drive_bays[0].installed.as_mut().unwrap();
        installed.identity_source = if present {
            IdentitySource::DvcidAndInquiry
        } else {
            IdentitySource::DvcidInline
        };
        installed.vendor = present.then(|| "HPE".to_owned());
        installed.product = present.then(|| "Ultrium 9-SCSI".to_owned());
        installed.revision = present.then(|| "R1.0".to_owned());
        let result = start_daemon_with_snapshot(&root, &world, snapshot, plain);
        let (state, api, runtime) = match result {
            Ok(value) => value,
            Err(error) => {
                wait_for_lifecycle_close(&root);
                panic!(
                    "T13: daemon start {} must preserve identity: {error}",
                    start + 1
                );
            }
        };
        let rows = state.write_handle().list_drives(true, true).unwrap();
        assert_eq!(rows.len(), 1);
        if let Some(identity) = &identity {
            assert_eq!(&rows[0].drive_uuid, identity);
        } else {
            identity = Some(rows[0].drive_uuid.clone());
        }
        assert!(!state.library_runtime().motion_fenced());
        runtime.block_on(api.shutdown_drive_pool()).unwrap();
        drop(api);
        drop(runtime);
        drop(state);
        wait_for_lifecycle_close(&root);
    }
}
