//! Model coverage for invocation boundaries, durable crash cuts, and the shared
//! finalization/import funnels. Transport injection never substitutes a writer.
use super::*;
use crate::{StandaloneFinalizeRequest, WriteObjectToPoolRequest};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{transport::TransferOutcome, ScsiError, TimeoutClass};
use remanence_state::AuditSink as _;
use remanence_state::{AuditActor, AuditEvent, AuditEventRecord, AuditSubject, SourceLayer};
use std::collections::BTreeMap;
const UUID: TapeUuid = [
    0x73, 0x73, 0x73, 0x73, 0x73, 0x73, 0x43, 0x73, 0x83, 0x73, 0x73, 0x73, 0x73, 0x73, 0x73, 0x73,
];
const BLOCK: u32 = 262144;
const BAY: u16 = 0x100;
const DEVICE: &str = "/dev/sg-model-p4";
const SERIAL: &str = "P4-MODEL";
// Checkpoint publication triggers either a readiness fault or a media swap.
type PublicationBoundary = (
    Arc<remanence_state::TapeAuthority>,
    SharedVirtualWorld,
    bool,
    Arc<Mutex<bool>>,
);
struct Transport {
    inner: ModelTransport,
    fault: Arc<Mutex<Option<u8>>>,
    audit_cut: Option<PathBuf>,
    hide_serial: bool,
    commands: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
    cut_after_prevent: bool,
    boundary: Option<PublicationBoundary>,
}
impl Transport {
    fn fail(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if let Some(commands) = &self.commands {
            commands.lock().unwrap().push(cdb.to_vec());
        }
        if self.boundary.as_ref().is_some_and(|(authority, _, _, _)| {
            remanence_state::authority::fixtures::has_publication(authority)
        }) {
            let (_, world, ua, seen) = self.boundary.take().unwrap();
            *seen.lock().unwrap() = true;
            assert_eq!(
                cdb[0], 0x00,
                "checkpoint must be followed by fresh readiness proof"
            );
            if ua {
                let mut sense = vec![0; 18];
                sense[0] = 0x70;
                sense[2] = 6;
                sense[7] = 10;
                sense[12] = 0x28;
                return Err(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                });
            }
            world
                .lock()
                .unwrap()
                .put_tape_in_drive(BAY, "SWAPPED", None, media([0x82; 16]));
        }
        let mut fault = self.fault.lock().unwrap();
        // R6: the transport reports a definite WRITE rejection and zero transferred bytes.
        if *fault == Some(0xfe) && cdb[0] == 0x0a {
            *fault = None;
            return Err(ScsiError::CheckCondition {
                sense: vec![0x72, 5, 0x24, 0],
                bytes_transferred: 0,
            });
        }

        // 0xff is a test-only arm for a validated partial fixed WRITE, with
        // no modeled media mutation so later positioning has an independent oracle.
        if *fault == Some(0xff) && cdb[0] == 0x0a {
            *fault = None;
            assert_eq!(cdb[1] & 1, 1);
            let mut sense = vec![0; 18];
            sense[0] = 0xf0;
            sense[2] = 3;
            sense[3..7].copy_from_slice(&2u32.to_be_bytes());
            sense[7] = 10;
            sense[12] = 0x0c;
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 4 * BLOCK,
            });
        }
        if *fault == Some(cdb[0]) {
            *fault = None;
            if let Some(path) = self.audit_cut.take() {
                std::fs::rename(&path, path.with_extension("saved")).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            return Err(ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: vec![],
            });
        }
        Ok(())
    }
}
impl SgTransport for Transport {
    fn execute_in(&mut self, cdb: &[u8], bytes: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.fail(cdb)?;
        if self.hide_serial && cdb[0] == 0x12 && cdb[1] & 1 != 0 && cdb[2] == 0x80 {
            return Ok(TransferOutcome::clean(0));
        }
        self.inner.execute_in(cdb, bytes)
    }
    fn execute_out(&mut self, cdb: &[u8], bytes: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.fail(cdb)?;
        self.inner.execute_out(cdb, bytes)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.fail(cdb)?;
        if cdb[0] == 0x1e && cdb[4] == 1 && self.cut_after_prevent {
            *self.fault.lock().unwrap() = Some(0x01);
        }
        if matches!(cdb[0], 0x1e | 0x16 | 0x17) {
            return Ok(());
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}
fn media(uuid: TapeUuid) -> VirtualTape {
    let payload = crate::build_tape_bootstrap(
        uuid,
        BLOCK,
        crate::ParityConfig::None,
        "2026-10-15T00:00:00Z",
        "P4-model",
    );
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&payload, &mut block).unwrap();
    let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK);
    tape
}
fn world() -> SharedVirtualWorld {
    let mut w = VirtualWorld::single_drive("UNUSED", BAY, SERIAL, 0x400, 0);
    w.put_tape_in_drive(BAY, "HAND", None, media(UUID));
    Arc::new(Mutex::new(w))
}
fn state(root: &Path) -> StateHandle {
    state_with_audit(root, true)
}
fn state_with_audit(root: &Path, fsync: bool) -> StateHandle {
    state_with_parity(root, fsync, crate::ParityConfig::None)
}
fn configured_state(root: &Path, fsync: bool) -> StateHandle {
    let mut config = remanence_state::parse_config_toml(&format!(
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
[[tape_pool_rules]]
prefix = "HAND"
pool_id = "default"
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
        root.display()
    ))
    .unwrap();
    config.audit.fsync = fsync;
    StateHandle::open_with_config(
        remanence_state::StatePaths::from_config(root.join("config.toml"), &config),
        config,
    )
    .unwrap()
}
fn state_with_parity(root: &Path, fsync: bool, parity: crate::ParityConfig) -> StateHandle {
    let mut state = configured_state(root, fsync);
    state
        .provision_tape_with_media(
            remanence_state::ProvisionTapeInput {
                tape_uuid: UUID,
                voltag: "HAND".into(),
                block_size: BLOCK,
                parity,
                force: false,
            },
            remanence_state::LabelProvenance::Operator,
            Some(remanence_state::MediaObservation {
                generation: LtoGen::Lto9,
                source: GenerationSource::Operator,
            }),
        )
        .unwrap();
    state
}
fn adopted_state(root: &Path, parity: crate::ParityConfig) -> StateHandle {
    let mut state = configured_state(root, true);
    state
        .adopt_bootstrap_identity(
            remanence_state::AdoptBootstrapIdentityInput {
                operation_id: uuid::Uuid::new_v4(),
                tape_uuid: UUID,
                voltag: Some("HAND".into()),
                label_provenance: LabelProvenance::Operator,
                observation: Some(remanence_state::MediaObservation {
                    generation: LtoGen::Lto9,
                    source: GenerationSource::Operator,
                }),
                pool_id: Some("default".into()),
                block_size: BLOCK,
                parity,
            },
            remanence_state::BootstrapAdoptionEvidence {
                endpoint: remanence_library::DriveKey::Standalone {
                    drive_serial: SERIAL.into(),
                },
                library_revision: None,
                home_slot: None,
                drive_serial: SERIAL.into(),
                bootstrap_drive_compression: false,
                configured_drive_compression: false,
                physical_tail: remanence_state::BootstrapAdoptionTailEvidence::DataAfterBootstrap,
            },
        )
        .unwrap();
    assert_eq!(
        state.catalog().get_tape(&UUID).unwrap().unwrap().state,
        "recovery_required"
    );
    state
}
fn backend<'a>(
    state: &'a mut StateHandle,
    world: &SharedVirtualWorld,
    root: &Path,
    fault: Arc<Mutex<Option<u8>>>,
) -> Result<OperatorMount<'a>, MountError> {
    OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(world), DeviceRole::Drive { bay: BAY }),
            fault,
            audit_cut: None,
            hide_serial: false,
            commands: None,
            cut_after_prevent: false,
            boundary: None,
        }),
        state,
        root,
    )
}
/// Transfer the original state and proven mount into the production owned actor.
macro_rules! own_mount {
    ($backend:ident, $mounted:ident, $state:ident) => {{
        let paths = $backend.state.paths().clone();
        let config = $backend.state.config().clone();
        let audit = $backend.context.audit_error.clone();
        $backend.finish().unwrap();
        let owner = crate::engine::drive_adapter::MountedDrive::start(
            $state,
            $mounted,
            StaticAllowlist::new([SERIAL]),
            audit,
        )
        .unwrap();
        (owner, move || {
            StateHandle::open_with_config(paths.clone(), config.clone()).unwrap()
        })
    }};
}
fn target() -> MountTarget {
    MountTarget::Device {
        path: DEVICE.into(),
        expected_uuid: Some(UUID),
    }
}
fn request(root: &Path, name: &str) -> WriteObjectToPoolRequest {
    let file = root.join(name);
    std::fs::write(&file, name.as_bytes()).unwrap();
    WriteObjectToPoolRequest {
        pool_id: "default".into(),
        source: crate::WriteObjectSource::Path(file),
        archive_path: name.into(),
        caller_object_id: name.into(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: crate::WriteObjectInputKind::LogicalFile,
        representation: crate::PoolWriteRepresentation::Plaintext,
    }
}
fn fault() -> Arc<Mutex<Option<u8>>> {
    Arc::new(Mutex::new(None))
}
#[test]
fn insertion_removal_and_swap_require_a_new_proof() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let lock = root.path().join("locks");
    world.lock().unwrap().drive_bays.insert(BAY, None);
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    assert!(mount.mount(target()).is_err());
    drop(mount);
    world
        .lock()
        .unwrap()
        .put_tape_in_drive(BAY, "HAND", None, media(UUID));
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    world.lock().unwrap().drive_bays.insert(BAY, None);
    assert!(actor
        .write_checkpointed(request(root.path(), "removed"))
        .is_err());
    assert_eq!(
        world.lock().unwrap().tapes["HAND"].written_bytes,
        u64::from(BLOCK)
    );
    let _ = actor.finish();
    let state = reopen_state();
    assert!(
        state
            .catalog()
            .list_active_tape_io_fences()
            .unwrap()
            .is_empty(),
        "readiness failure before media dispatch must not quarantine the tape"
    );
    // A distinct invocation proves the swapped BOT against the original UUID.
    let second = tempfile::tempdir().unwrap();
    let mut second_state = self::state(second.path());
    world
        .lock()
        .unwrap()
        .put_tape_in_drive(BAY, "OTHER", None, media([0x74; 16]));
    let mut mount = backend(&mut second_state, &world, &lock, fault()).unwrap();
    assert!(mount.mount(target()).is_err());
    assert_eq!(
        world.lock().unwrap().tapes["OTHER"].written_bytes,
        u64::from(BLOCK)
    );
}
#[test]
#[ignore = "ruling 24: a pre-commit session is process-local, so no durable session exists to recover; the post-commit restart fence waits on ruling 29 (L2)"]
fn crash_after_session_open_allows_read_mount_but_refuses_writes() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let lock = root.path().join("locks");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    drop(mounted);
    drop(mount);
    let paths = state.paths().clone();
    let config = state.config().clone();
    drop(state);
    let mut state = StateHandle::open_with_config(paths, config).unwrap();
    let mut recovered = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = recovered.mount(target()).unwrap();
    assert!(
        matches!(recovered.mutate_catalog(&mounted, |_| Ok(())), Err(MountError::Refused(ref message)) if message.contains("fence"))
    );
    assert_eq!(
        world.lock().unwrap().tapes["HAND"].written_bytes,
        u64::from(BLOCK)
    );
}
#[test]
fn uncertain_write_cut_records_a_live_end_and_preserves_prior_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let lock = root.path().join("locks");
    let fault = fault();
    let mut mount = backend(&mut state, &world, &lock, Arc::clone(&fault)).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "first"))
        .unwrap();
    *fault.lock().unwrap() = Some(0x0a);
    assert!(actor
        .write_checkpointed(request(root.path(), "second"))
        .is_err());
    assert!(fault.lock().unwrap().is_none(), "the write cut was reached");
    let _ = actor.finish();
    let mut state = reopen_state();
    assert!(state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    let mut reopened = backend(&mut state, &world, &lock, self::fault()).unwrap();
    let mounted = reopened.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(reopened, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "third"))
        .unwrap();
    actor.finish().unwrap();
    let state = reopen_state();
    assert_eq!(state.catalog().list_native_objects().unwrap().len(), 2);
}
/// A terminal component whose log record is durable but whose SQLite
/// projection fails reports the replay retry, and the same request resumes.
#[test]
fn durable_terminal_component_projection_failure_asks_for_replay_and_resumes() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let sqlite = state.paths().sqlite_path.clone();
    let world = world();
    let lock = root.path().join("locks");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "component"))
        .unwrap();
    actor.close_session().unwrap();
    let key = uuid::Uuid::new_v4();
    let finalize = || StandaloneFinalizeRequest {
        tape_uuid: UUID,
        acknowledge_tape: UUID,
        expected_pool_id: Some("default".into()),
        reason: "operator finished".into(),
        idempotency_key: key,
    };
    let conn = rusqlite::Connection::open(&sqlite).unwrap();
    conn.execute_batch("create trigger fail_component_projection before update of finalization_progress on tapes when new.finalization_progress = 'after_replica_a' begin select raise(abort, 'injected component projection failure'); end;").unwrap();
    let error = actor
        .finalize_standalone(finalize())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("terminal component is durable; retry after tape-log replay"),
        "{error}"
    );
    assert!(
        error.contains("injected component projection failure"),
        "{error}"
    );
    conn.execute_batch("drop trigger fail_component_projection")
        .unwrap();
    drop(conn);
    let _ = actor.finish();
    let mut state = reopen_state();
    // The standalone owner fences the tape on the failed request; after the
    // operator's release the same request resumes from the durable log.
    let fences = state.catalog().list_active_tape_io_fences().unwrap();
    assert!(!fences.is_empty());
    for fence in fences {
        state
            .release_tape_io_fence(&fence.quarantine_id, "projection fault removed", false)
            .unwrap();
    }
    let holds = state.library_runtime().standalone_drive_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].drive_serial, SERIAL);
    assert!(state
        .library_runtime()
        .release_standalone_drive_hold(
            &format!("library:{}", holds[0].fence),
            "projection fault removed",
            AuditActor::local_user(),
        )
        .unwrap()
        .is_some());
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor.finalize_standalone(finalize()).unwrap();
    let _ = actor.finish();
    let state = reopen_state();
    assert_eq!(
        state.catalog().get_tape(&UUID).unwrap().unwrap().state,
        "sealed"
    );
}

/// Typed catch-up restores a lost SQLite seal before supersede-tail can dispatch a write.
#[test]
fn supersede_tail_refuses_a_sealed_log_that_sqlite_reports_ready() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let sqlite = state.paths().sqlite_path.clone();
    let world = world();
    let lock = root.path().join("locks");
    let runtime = state.library_runtime();
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "sealed"))
        .unwrap();
    actor.close_session().unwrap();
    actor
        .finalize_standalone(StandaloneFinalizeRequest {
            tape_uuid: UUID,
            acknowledge_tape: UUID,
            expected_pool_id: Some("default".into()),
            reason: "operator finished".into(),
            idempotency_key: uuid::Uuid::new_v4(),
        })
        .unwrap();
    rusqlite::Connection::open(&sqlite)
        .unwrap()
        .execute(
            "update tapes set state = 'ready', finalization_progress = null, finalization_trigger = null, finalization_operation_id = null, finalization_edition_digest = null, finalization_layout_digest = null, finalization_outcome = null, completed_replicas = null where tape_uuid = ?1",
            [UUID.as_slice()],
        )
        .unwrap();
    runtime.fixture_mark_projection_dirty();
    let before = dispatched_writes(&world);
    // On typed state, catch-up repairs a lost SQLite seal from the logs.
    let refusal = actor
        .supersede_tail_confirmed("HAND", |_, _| Ok(()))
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("terminal finalization prevents tail discard"),
        "{refusal}"
    );
    assert_eq!(dispatched_writes(&world), before);
    let _ = actor.finish();
    drop(runtime);
    let state = reopen_state();
    assert_eq!(
        state.catalog().get_tape(&UUID).unwrap().unwrap().state,
        "sealed"
    );
}
#[test]
fn checkpoint_finalize_and_terminal_restore_share_the_production_funnels() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let world = world();
    let lock = root.path().join("locks");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "first"))
        .unwrap();
    actor
        .write_checkpointed(request(root.path(), "second"))
        .unwrap();
    let id = uuid::Uuid::new_v4();
    let finalize = || StandaloneFinalizeRequest {
        tape_uuid: UUID,
        acknowledge_tape: UUID,
        expected_pool_id: Some("default".into()),
        reason: "operator finished".into(),
        idempotency_key: id,
    };
    actor.close_session().unwrap();
    actor.finalize_standalone(finalize()).unwrap();
    actor.finalize_standalone(finalize()).unwrap();
    let before_replay = dispatched_writes(&world);
    assert!(actor
        .write_checkpointed(request(root.path(), "first"))
        .unwrap()
        .is_replay());
    assert_eq!(dispatched_writes(&world), before_replay);
    let output = root.path().join("restored");
    actor.finish().unwrap();
    let mut state = reopen_state();
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let report = crate::restore_catalogless(&mut mounted.drive, &output, None).unwrap();
    assert_eq!(report["terminal_index"], true, "{report}");
    assert_eq!(
        std::fs::read(output.join("candidate-1/first")).unwrap(),
        b"first"
    );
    assert_eq!(
        std::fs::read(output.join("candidate-2/second")).unwrap(),
        b"second"
    );
    mount.release(mounted);
}
/// Every file and directory below `path`, as relative paths, sorted.
fn journal_tree(path: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, path: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap().path();
            out.push(entry.strip_prefix(root).unwrap().to_path_buf());
            if entry.is_dir() {
                visit(root, &entry, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(path, path, &mut out);
    out.sort();
    out
}
/// A source journal directory as a previous host leaves it: one initialized native log for the
/// model tape. The import is refused before it is read; this is what a non-refusing import would
/// have staged and published.
fn source_journal(root: &Path, parity: crate::ParityConfig) -> PathBuf {
    let source = state_with_parity(root, true, parity.clone());
    let scheme = match parity {
        crate::ParityConfig::Scheme(scheme) => Some(scheme),
        _ => None,
    };
    let authority = remanence_state::TapeAuthority::open(
        source.paths().journal_dir.join("tapes"),
        UUID,
        &source.paths().sqlite_path,
    )
    .unwrap();
    let mut lease = authority.acquire_exclusive().unwrap();
    lease
        .publish_initialized(
            BLOCK,
            scheme,
            remanence_parity::CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    drop(lease);
    source.paths().journal_dir.clone()
}
/// Ruling 32-L1: the import refuses before it checks, stages, publishes or writes anything,
/// whatever the source or the tape's restrictions are.
fn assert_import_refused(
    mount: &mut OperatorMount<'_>,
    mounted: &mut MountedTape,
    from: &Path,
    world: &SharedVirtualWorld,
) {
    let journal_dir = mount.state.paths().journal_dir.clone();
    let sqlite = mount.state.paths().sqlite_path.clone();
    let commands = world.lock().unwrap().command_log.len();
    let tree = journal_tree(&journal_dir);
    let before = crate::tests::refusal_tests::mark(&sqlite);
    let tape_state = mount
        .state
        .catalog()
        .get_tape(&UUID)
        .unwrap()
        .unwrap()
        .state;
    let error = mount
        .import_journals(mounted, from)
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("JournalImportDeferred"), "{error}");
    assert_eq!(
        world.lock().unwrap().command_log.len(),
        commands,
        "the refusal sends no SCSI command"
    );
    assert_eq!(
        journal_tree(&journal_dir),
        tree,
        "no staging directory and no publication"
    );
    assert!(!remanence_state::TapeAuthority::exists(journal_dir.join("tapes"), UUID).unwrap());
    // The refusal is counted in the library log: one record, which the projection applies and
    // projects nothing from. Every row outside the projection's own books is unchanged.
    crate::tests::refusal_tests::assert_grew_by(&sqlite, &before, 1);
    assert_eq!(
        mount
            .state
            .catalog()
            .get_tape(&UUID)
            .unwrap()
            .unwrap()
            .state,
        tape_state
    );
}

/// Idle removal leaves the adopted tape without a journal, and ruling 32-L1 then refuses the
/// import of any journal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_idle_unload_leaves_the_journal_absent_and_import_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let from = source_journal(&root.path().join("original"), crate::ParityConfig::None);
    let world = world();
    let lock = root.path().join("locks");
    let mut adopted = adopted_state(&root.path().join("new-host"), crate::ParityConfig::None);
    crate::write_owner::tests::unload_adopted_cartridge(&adopted, world.clone(), UUID).await;
    {
        let mut world = world.lock().unwrap();
        world.drive_bays.insert(BAY, Some("HAND".into()));
        world.slots[0].full = false;
        world.slots[0].barcode = None;
    }
    let mut mount = backend(&mut adopted, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    mount.release(mounted);
}

/// Ruling 32-L1: a journal import, repeated or not, is refused before any effect and grants no
/// append authority, so the adopted tape stays `recovery_required`.
#[test]
fn journal_import_is_refused_before_any_effect_and_authorizes_no_append() {
    let root = tempfile::tempdir().unwrap();
    let from = source_journal(&root.path().join("original"), crate::ParityConfig::None);
    let world = world();
    let lock = root.path().join("locks");
    let mut state = adopted_state(&root.path().join("new-host"), crate::ParityConfig::None);
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    mount.release(mounted);
    assert_eq!(
        state.catalog().get_tape(&UUID).unwrap().unwrap().state,
        "recovery_required"
    );
}

/// Ruling 32-L1, counted. Each refused import is one `RefusalRecorded` naming the mounted tape,
/// in the library log of the state that refused it, and the log holds nothing else of them.
#[test]
fn journal_import_refusal_is_recorded_each_time_with_its_tape() {
    use crate::tests::refusal_tests::{read_log, refusals, RecordedRefusal};
    let root = tempfile::tempdir().unwrap();
    let from = source_journal(&root.path().join("original"), crate::ParityConfig::None);
    let world = world();
    let lock = root.path().join("locks");
    let mut state = adopted_state(&root.path().join("new-host"), crate::ParityConfig::None);
    let paths = state.paths().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    mount.release(mounted);
    drop(state);

    let log = read_log(&paths, library_uuid);
    let expected = RecordedRefusal::expect(
        14,
        "JournalImportDeferred",
        "tape",
        Some(UUID),
        remanence_state::AuditActor::local_user(),
    );
    assert_eq!(refusals(&log), [expected.clone(), expected]);
    assert!(
        log[log.len() - 2..]
            .iter()
            .all(|(tag, _)| *tag == remanence_state::library_log::Tag::Diagnostic),
        "the two refusals are the last two records: {log:?}"
    );
}

#[test]
fn encrypted_scan_uses_the_same_contained_restore_sink() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    let key = remanence_aead::RecipientPrivateKey::new([9; 16], "model", [7; 32]).unwrap();
    let mut input = request(root.path(), "secret");
    input.representation = crate::PoolWriteRepresentation::Encrypted {
        recipients: vec![
            key.public_key(0).unwrap(),
            remanence_aead::RecipientPrivateKey::new([10; 16], "backup", [8; 32])
                .unwrap()
                .public_key(1)
                .unwrap(),
        ],
    };
    actor.write_checkpointed(input).unwrap();
    let _ = actor.finish();
    let mut state = reopen_state();
    let mut mount = self::backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        self::fault(),
    )
    .unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let refused =
        crate::scan_and_restore(&mut mounted.drive, &root.path().join("no-key"), None).unwrap();
    assert!(refused.to_string().contains("--private-key"));
    let output = root.path().join("with-key");
    let report = crate::scan_and_restore(&mut mounted.drive, &output, Some(&key)).unwrap();
    assert_eq!(report["candidates"][0]["status"], "restored", "{report}");
    assert_eq!(
        std::fs::read(output.join("candidate-1/secret")).unwrap(),
        b"secret"
    );
    mount.release(mounted);
}

#[test]
fn position_overflow_refuses_without_motion_and_reservation_is_opt_in() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut mount = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
            fault: fault(),
            audit_cut: None,
            hide_serial: false,
            boundary: None,
            commands: Some(Arc::clone(&commands)),
            cut_after_prevent: false,
        }),
        &mut state,
        &root.path().join("locks"),
    )
    .unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let count = world.lock().unwrap().command_log.len();
    assert!(
        crate::position_to_object(&mut mounted.drive, crate::BotRelativeTapeFile(u64::MAX))
            .is_err()
    );
    assert_eq!(world.lock().unwrap().command_log.len(), count);
    assert!(!mounted.reserved);
    let reservations = || {
        commands
            .lock()
            .unwrap()
            .iter()
            .map(|cdb| cdb[0])
            .filter(|opcode| matches!(opcode, 0x16 | 0x17))
            .collect::<Vec<_>>()
    };
    assert!(reservations().is_empty());
    mount.reserve(&mut mounted).unwrap();
    assert_eq!(reservations(), [0x16]);
    assert!(mounted.reserved);
    mount.release(mounted);
    assert_eq!(reservations(), [0x16, 0x17]);
}

/// Ruling 32-L1: the refusal does not look at the source, so a missing log and a complete one are
/// refused alike and nothing is published to the destination.
#[test]
fn parity_import_is_refused_whether_or_not_the_source_log_is_present() {
    let root = tempfile::tempdir().unwrap();
    let parity = crate::ParityConfig::Scheme(remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_static("p4-import"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    });
    let world = world();
    let bootstrap =
        crate::build_tape_bootstrap(UUID, BLOCK, parity.clone(), "2026-10-15T00:00:00Z", "P4");
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    world.lock().unwrap().tapes.get_mut("HAND").unwrap().records[0] = Record::Block(block);
    let lock = root.path().join("locks");
    let from = source_journal(&root.path().join("original"), parity.clone());
    let partner = from
        .join("tapes")
        .join(format!("{}.remtape", uuid::Uuid::from_bytes(UUID).simple()));
    let mut state = adopted_state(&root.path().join("new-host"), parity);
    let destination = state.paths().journal_dir.join("tapes");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let hidden = remanence_state::authority::row_fixtures::hide_authority(&partner).unwrap();
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    hidden.restore().unwrap();
    assert_import_refused(&mut mount, &mut mounted, &from, &world);
    assert!(!remanence_state::TapeAuthority::exists(&destination, UUID).unwrap());
    mount.release(mounted);
}

/// DEC-1 and D-N-1-1: the coupled failure latches until recovery, but a durable live ending owes no fence.
#[test]
fn combined_transport_and_audit_failure_latches_until_recovery_then_owes_no_fence() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let recovery_paths = state.paths().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    let world = world();
    // This fault starts after the fixture's real complete Bootstrap+delimiter.
    // Publish that verified native fact; fresh-media refusal has its own test.
    {
        use remanence_state::authority::TapeAuthorityView;
        let records = world.lock().unwrap().tapes["HAND"].records.clone();
        assert_eq!(records.len(), 2);
        let Record::Block(bytes) = &records[0] else {
            panic!("Bootstrap")
        };
        let bootstrap = remanence_parity::bootstrap::parse_bootstrap_block(bytes).unwrap();
        assert_eq!(bootstrap.tape_uuid, UUID);
        assert_eq!(bootstrap.block_size_bytes, BLOCK);
        assert!(bootstrap.scheme.is_none());
        assert!(matches!(records[1], Record::Filemark));
        let authority = remanence_state::TapeAuthority::open(
            state.paths().journal_dir.join("tapes"),
            UUID,
            &state.paths().sqlite_path,
        )
        .unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        lease
            .publish_initialized(
                BLOCK,
                None,
                remanence_parity::CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        state
            .write_handle()
            .project_tape_log(&lease.freeze().unwrap())
            .unwrap();
    }
    let audit_path = std::fs::read_dir(state.paths().audit_dir.clone())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|e| e == "remlib"))
        .unwrap();
    let fault = fault();
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
            fault: Arc::clone(&fault),
            audit_cut: Some(audit_path.clone()),
            hide_serial: false,
            commands: None,
            cut_after_prevent: false,
            boundary: None,
        }),
        &mut state,
        &root.path().join("locks"),
    )
    .unwrap();
    let mounted = backend.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(backend, mounted, state);
    actor.open_session("default").unwrap(); // Commit BOT before injecting the Object failure.
    *fault.lock().unwrap() = Some(0x0a);
    let failed_write = actor
        .write_checkpointed(request(root.path(), "cut"))
        .unwrap_err();
    eprintln!("coupled failure: {failed_write}");
    assert!(actor
        .write_checkpointed(request(root.path(), "latched"))
        .is_err());
    assert!(actor
        .catalog_index()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    std::fs::remove_dir(&audit_path).unwrap();
    std::fs::rename(audit_path.with_extension("saved"), &audit_path).unwrap();
    let count = dispatched_writes(&world);
    for _ in 0..2 {
        let error = actor
            .write_checkpointed(request(root.path(), "retry"))
            .unwrap_err();
        assert!(!error.to_string().is_empty(), "retry must remain refused");
    }
    assert_eq!(dispatched_writes(&world), count);
    let _ = actor.finish();
    // Exercise the existing explicit publication recovery algorithm, retaining
    // every complete frame and syncing it before clearing the ambiguity marker.
    // This is a reproduction of the remaining startup integration boundary.
    {
        use remanence_state::library_log::{
            native_view::NativeTapeView,
            store::{Clock, Origin, UuidSource},
            LibraryLog,
        };
        struct ClockNow;
        impl Clock for ClockNow {
            fn now(&self) -> u64 {
                1
            }
        }
        struct Fresh;
        impl UuidSource for Fresh {
            fn next(&self, _: u64) -> [u8; 16] {
                *uuid::Uuid::new_v4().as_bytes()
            }
        }
        let _lease = remanence_state::StateLockGuard::acquire(&recovery_paths.state_dir).unwrap();
        let native = remanence_state::tape_log::store::Store::open(
            recovery_paths.journal_dir.join("tapes"),
            UUID,
            &recovery_paths.sqlite_path,
        )
        .unwrap();
        let native = native.snapshot().unwrap();
        eprintln!(
            "durable native ending after failed fence: {:?}",
            native
                .summary()
                .last_session_end
                .as_ref()
                .map(|end| end.reason)
        );
        assert!(
            native.summary().last_session_end.is_some(),
            "exercise durable native ending"
        );
        let view = NativeTapeView::probe(
            &recovery_paths.journal_dir.join("tapes"),
            &recovery_paths.sqlite_path,
        )
        .unwrap();
        let mut log = LibraryLog::open(
            &recovery_paths.audit_dir,
            library_uuid,
            Origin {
                host: "r11-reproduction".into(),
                pid: std::process::id(),
                build: "test".into(),
            },
            Box::new(ClockNow),
            Box::new(Fresh),
            true,
            &view,
        )
        .unwrap();
        log.clear_ambiguity(&view).unwrap();
    }
    let mut state = reopen_state();
    // DEC-1 and D-N-1-1: recovery preserves the durable ending without a quarantine.
    let recovered = self::backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        self::fault(),
    )
    .unwrap();
    drop(recovered);
    let expected = state.catalog().list_active_tape_io_fences().unwrap();
    assert!(
        expected.is_empty(),
        "a durable live ending owes no quarantine (DEC-1, D-N-1-1)"
    );
    let paths = state.paths().clone();
    let config = state.config().clone();
    drop(state);
    for suffix in ["", "-wal", "-shm"] {
        let p = PathBuf::from(format!("{}{suffix}", paths.sqlite_path.display()));
        if p.exists() {
            std::fs::remove_file(p).unwrap();
        }
    }
    for _ in 0..2 {
        let state = StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
        let actual = state.catalog().list_active_tape_io_fences().unwrap();
        assert!(actual.is_empty(), "rebuild preserves the no-fence ending");
        drop(state);
    }
}

#[test]
fn automatic_terminal_close_reproves_after_checkpoint_publication() {
    for ua in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let paths = state.paths().clone();
        let mut config = state.config().clone();
        drop(state);
        config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
        config.tape_pools[0].watermark_low = 0.000001;
        config.tape_pools[0].watermark_high = 0.25;
        let mut state = StateHandle::open_with_config(paths, config).unwrap();
        let checkpoint = remanence_state::authority::fixtures::AuthorityBuilder::new(
            state.paths().journal_dir.join("tapes"),
            UUID,
        )
        .build()
        .unwrap();
        let checkpoint = Arc::new(checkpoint);
        let world = world();
        let seen = Arc::new(Mutex::new(false));
        let mut backend = OperatorMount::with_transport_for_test(
            Path::new(DEVICE),
            &StaticAllowlist::new([SERIAL]),
            Box::new(Transport {
                inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
                fault: fault(),
                audit_cut: None,
                hide_serial: false,
                commands: None,
                cut_after_prevent: false,
                boundary: Some((
                    Arc::clone(&checkpoint),
                    Arc::clone(&world),
                    ua,
                    Arc::clone(&seen),
                )),
            }),
            &mut state,
            &root.path().join("locks"),
        )
        .unwrap();
        let mounted = backend.mount(target()).unwrap();
        let (mut actor, reopen_state) = own_mount!(backend, mounted, state);
        let receipt = actor
            .write_checkpointed(request(root.path(), "boundary"))
            .unwrap();
        assert!(!receipt.object.object_id.is_empty());
        let _ = actor.finish();
        assert!(
            *seen.lock().unwrap(),
            "post-reply readiness boundary exercised"
        );
        assert!(checkpoint
            .acquire_exclusive_for_terminal_recovery()
            .unwrap()
            .last_record_bounded()
            .unwrap()
            .is_some());
        assert!(
            checkpoint.terminal_finalization_intent().unwrap().is_some(),
            "decision precedes the object reply and media readiness"
        );
        if !ua {
            assert_eq!(
                world.lock().unwrap().tapes["SWAPPED"].written_bytes,
                u64::from(BLOCK)
            );
        }
        assert!(
            world.lock().unwrap().tapes["HAND"].written_bytes < 16 * 1024 * 1024,
            "no terminal extent written"
        );
        let _ = actor.finish();
        drop(reopen_state());
    }
}

#[test]
fn barcode_mount_preserves_the_legacy_read_command_stream_without_generation() {
    let root = tempfile::tempdir().unwrap();
    let state = configured_state(root.path(), true);
    state
        .write_handle()
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: UUID,
            voltag: "HAND".into(),
            block_size: BLOCK,
            parity: crate::ParityConfig::None,
            force: false,
        })
        .unwrap();
    let policy = StaticAllowlist::new(["UNUSED"]);
    let mut streams = Vec::new();
    for legacy in [true, false] {
        let world = world();
        let snapshot = world.lock().unwrap().library_snapshot();
        let shared = Arc::clone(&world);
        let mut library = snapshot
            .open_with(&policy, move |path| {
                let role = shared.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(ModelTransport::new(Arc::clone(&shared), role)))
            })
            .unwrap();
        world.lock().unwrap().command_log.clear();
        if legacy {
            let mut drive =
                crate::load_tape_by_uuid(state.catalog(), &mut library, &policy, &UUID).unwrap();
            drive.rewind().unwrap();
            crate::verify_tape_identity(&mut DriveHandleSource(&mut drive), &UUID).unwrap();
        } else {
            let mut backend = ChangerMount {
                library: &mut library,
                policy: &policy,
                index: state.catalog(),
            };
            let mounted = backend
                .mount(MountTarget::Tape { tape_uuid: UUID })
                .unwrap();
            assert!(
                mounted.generation.is_none(),
                "generation cannot be invented for reads"
            );
            backend.release(mounted);
        }
        streams.push(
            world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .map(|c| c.opcode)
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(streams[0], streams[1]);
    // INQUIRY/VPD from opening the loaded drive, then the existing identity
    // verification prefix: REWIND, LOCATE, READ POSITION, READ.
    assert_eq!(streams[1], [0x12, 0x12, 0x01, 0x92, 0x34, 0x08]);
    assert!(
        !streams[1].contains(&0x00) && !streams[1].contains(&0x1e),
        "no new readiness or removal commands"
    );
    assert_eq!(streams[1].iter().filter(|&&op| op == 0x01).count(), 1);
}

fn authority_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            }
            // SQLite read-lock slots are volatile shared-memory coordination,
            // not durable authority. Database and WAL bytes remain covered.
            else if !path.to_string_lossy().ends_with("-shm") {
                files.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}
#[test]
fn ephemeral_open_and_mount_leave_all_durable_authority_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let mut backend = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mounted = backend.mount(target()).unwrap();
    backend.release(mounted);
    seed_open_session(
        &mut state,
        remanence_library::DriveKey::new("LIB", 0x100),
        [0x44; 16],
    );
    let before = authority_snapshot(root.path());
    let policy = StaticAllowlist::new([DEVICE]).with_unserialized_device_allowed(Path::new(DEVICE));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy,
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
            fault: fault(),
            audit_cut: None,
            hide_serial: true,
            commands: None,
            cut_after_prevent: false,
            boundary: None,
        }),
        &mut state,
        &root.path().join("ephemeral-locks"),
    )
    .unwrap();
    let mounted = backend.mount(target()).unwrap();
    assert!(!mounted.writable());
    backend.release(mounted);
    let after = authority_snapshot(root.path());
    let changed: Vec<_> = after
        .iter()
        .filter(|(p, bytes)| before.get(*p) != Some(*bytes))
        .map(|(p, _)| p)
        .collect();
    assert!(changed.is_empty(), "authority files changed: {changed:?}");
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
}

#[test]
fn catalogless_manifest_named_members_use_a_disjoint_payload_namespace() {
    use remanence_format::{RemTarFileSpec, RemTarFileStream, RemTarObjectOptions};
    use sha2::{Digest, Sha256};
    for kind in ["file", "directory", "symlink"] {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        let world = world();
        let spec = match kind {
            "file" => RemTarFileSpec::new(
                "manifest.json",
                "member",
                7,
                Sha256::digest(b"payload").into(),
            ),
            "directory" => RemTarFileSpec::directory("manifest.json/", "member"),
            _ => RemTarFileSpec::symlink("manifest.json", "member", "target.txt"),
        };
        let mut data = std::io::Cursor::new(if kind == "file" {
            b"payload".as_slice()
        } else {
            b"".as_slice()
        });
        let mut target_bytes = std::io::Cursor::new(b"target".as_slice());
        let target_spec =
            RemTarFileSpec::new("target.txt", "target", 6, Sha256::digest(b"target").into());
        let mut inputs = [
            RemTarFileStream::new(spec, &mut data),
            RemTarFileStream::new(target_spec, &mut target_bytes),
        ];
        let mut options =
            RemTarObjectOptions::new("collision", "test", "2026-10-15T00:00:00Z", "manifest");
        options.chunk_size = BLOCK as usize;
        let mut sink = remanence_library::VecBlockSink::new();
        remanence_format::write_rem_tar_object_from_readers(&mut sink, &options, &mut inputs)
            .unwrap();
        {
            let mut w = world.lock().unwrap();
            let tape = w.tapes.get_mut("HAND").unwrap();
            tape.written_bytes += sink.blocks.len() as u64 * u64::from(BLOCK);
            tape.records
                .extend(sink.blocks.into_iter().map(Record::Block));
            tape.records.push(Record::Filemark);
        }
        let mut backend = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        let mut mounted = backend.mount(target()).unwrap();
        let out = root.path().join("out");
        let report = crate::scan_and_restore(&mut mounted.drive, &out, None).unwrap();
        assert_eq!(
            report["candidates"][0]["status"], "restored",
            "{kind}: {report}"
        );
        let candidate = out.join("candidate-1");
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(candidate.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(metadata["payload"], "payload");
        let member = candidate.join("payload/manifest.json");
        match kind {
            "file" => assert_eq!(std::fs::read(member).unwrap(), b"payload"),
            "directory" => assert!(member.is_dir()),
            _ => assert_eq!(std::fs::read_link(member).unwrap(), Path::new("target.txt")),
        }
        // The collision layout must reject links at either directory boundary,
        // before creating a report or payload outside the selected destination.
        for link_at in ["candidate-1", "candidate-1/payload"] {
            let hostile = root
                .path()
                .join(format!("hostile-{}", link_at.replace('/', "-")));
            let outside = root
                .path()
                .join(format!("outside-{}", link_at.replace('/', "-")));
            std::fs::create_dir(&outside).unwrap();
            let link = hostile.join(link_at);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let report = crate::scan_and_restore(&mut mounted.drive, &hostile, None).unwrap();
            assert_eq!(
                report["candidates"][0]["status"], "refused",
                "{kind}: {report}"
            );
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        }
        backend.release(mounted);
    }
}

fn seed_open_session(
    state: &mut StateHandle,
    endpoint: remanence_library::DriveKey,
    tape: TapeUuid,
) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    state.catch_up_authority_projection().unwrap();
    remanence_state::FileAuditLog::open(&state.paths().audit_dir, true)
        .unwrap()
        .append(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            event: AuditEvent::SessionOpened,
            operation_id: None,
            session_id: Some(id),
            idempotency_key: None,
            subject: AuditSubject {
                kind: "write".into(),
                id: Some(id.to_string()),
            },
            detail: BTreeMap::from([
                ("session_kind".into(), ciborium::Value::Text("write".into())),
                (
                    "endpoint_key".into(),
                    ciborium::Value::Text(endpoint.encode()),
                ),
                ("tape_uuid".into(), ciborium::Value::Bytes(tape.to_vec())),
            ]),
        })
        .unwrap();
    state.replay_audit_projection().unwrap();
    id
}
macro_rules! append_for_rollback {
    (&mut $state:ident, $world:expr, $root:expr, $name:expr) => {{
        let world = $world;
        let root = $root;
        let mut mount = backend(&mut $state, world, &root.join("locks"), fault()).unwrap();
        let mounted = mount.mount(target()).unwrap();
        let (mut actor, reopen_state) = own_mount!(mount, mounted, $state);
        let result = actor.write_checkpointed(request(root, $name));
        let _ = actor.finish();
        $state = reopen_state();
        let _ = &$state; // The caller regains state ownership even on refusal.
        result
    }};
}
/// Publish an endpoint operation through the producer used by pool_ops/write_tests.
fn seed_operation_session(
    state: &StateHandle,
    endpoint: remanence_library::DriveKey,
    tape: TapeUuid,
) -> uuid::Uuid {
    use remanence_state::library_log as lib;
    let id = uuid::Uuid::new_v4();
    state
        .library_runtime()
        .observe_session(
            *id.as_bytes(),
            lib::SessionKind::Operation,
            Some(&endpoint),
            Some(tape),
            lib::SessionEvent::Opened,
            None,
            AuditActor::System,
            (None, None),
        )
        .unwrap();
    id
}

#[test]
fn restart_recovery_is_shared_and_scoped_to_the_owned_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    // D-C3a-1c-1: prime a native media commit before opening the crash session.
    let world = world();
    set_rollback_bootstrap(&world, crate::ParityConfig::None);
    append_for_rollback!(&mut state, &world, root.path(), "A").unwrap();

    let library_tape = [0x44; 16];
    // Accepted source fact (D-C3a-1c-1): state_with_parity supplies a recorded medium.
    state
        .provision_tape_with_media(
            remanence_state::ProvisionTapeInput {
                tape_uuid: library_tape,
                voltag: "LIBRARY".into(),
                block_size: BLOCK,
                parity: crate::ParityConfig::None,
                force: false,
            },
            remanence_state::LabelProvenance::Operator,
            Some(remanence_state::MediaObservation {
                generation: LtoGen::Lto9,
                source: GenerationSource::Operator,
            }),
        )
        .unwrap();
    let library_session = seed_operation_session(
        &state,
        remanence_library::DriveKey::new("LIB", 0x100),
        library_tape,
    );
    let standalone_session = seed_operation_session(
        &state,
        remanence_library::DriveKey::Standalone {
            drive_serial: SERIAL.into(),
        },
        UUID,
    );
    open_crash_fixture(&state);
    // Like drive_adapter's passing automatic-terminal-completion fixture,
    // refresh the external native producer's prefix before runtime catch-up.
    {
        use remanence_state::authority::TapeAuthorityView as _;
        let authority = remanence_state::TapeAuthority::open(
            state.paths().journal_dir.join("tapes"),
            UUID,
            &state.paths().sqlite_path,
        )
        .unwrap();
        let snapshot = authority.freeze().unwrap();
        state.library_runtime().refresh_native(&snapshot).unwrap();
        state.library_runtime().catch_up().unwrap();
    }
    let before = state.calibration_control().row(library_tape).write_epoch;
    assert!(backend(&mut state, &world, &root.path().join("locks"), fault()).is_ok());
    let open = state.catalog().non_terminal_sessions().unwrap();
    assert!(open.iter().any(|s| s.session_id == library_session));
    assert!(!open.iter().any(|s| s.session_id == standalone_session));
    assert_eq!(
        state.calibration_control().row(library_tape).write_epoch,
        before
    );
    // C8-G2 / DEC-2: endpoint recovery ends only its library sessions and
    // raises no native-write restart fence; the tape's next open resolves that write.
    assert!(state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    let native_before_global = state.calibration_control().row(UUID).write_epoch;
    // The daemon uses exactly this boundary with global state ownership.
    assert_eq!(
        state
            .recover_sessions_after_restart(None, |_, _| Ok(()))
            .unwrap(),
        1
    );
    let open = state.catalog().non_terminal_sessions().unwrap();
    assert!(open.iter().all(|session| session.session_kind == "write"));
    assert_eq!(open.len(), 1);
    let native_session = uuid::Uuid::from_bytes([0xc7; 16]);
    assert_eq!(open[0].session_id, native_session);
    assert_eq!(
        state
            .catalog()
            .session_state(native_session)
            .unwrap()
            .as_deref(),
        Some("open")
    );
    assert!(state.calibration_control().row(UUID).write_epoch > native_before_global);
}

/// Ruling 32-L1: the refusal does not depend on the tape's fence or seal, and an import that is
/// refused leaves the quarantine and terminal restrictions exactly as they were.
#[test]
fn imported_authority_preserves_quarantine_and_terminal_restrictions() {
    let root = tempfile::tempdir().unwrap();
    let from = source_journal(&root.path().join("source"), crate::ParityConfig::None);
    let world = world();
    for restricted in ["fence", "sealed"] {
        let mut state = adopted_state(&root.path().join(restricted), crate::ParityConfig::None);
        let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        let mut mounted = mount.mount(target()).unwrap();
        if restricted == "fence" {
            mount
                .state
                .write_handle()
                .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                    tape_uuid: UUID,
                    barcode: Some("HAND".into()),
                    reason: "unrelated".into(),
                    evidence_json: None,
                })
                .unwrap();
        } else {
            mount.state.write_handle().seal_tape(UUID).unwrap();
        }
        assert_import_refused(&mut mount, &mut mounted, &from, &world);
        let tape = mount.state.catalog().get_tape(&UUID).unwrap().unwrap();
        assert_eq!(
            tape.state,
            if restricted == "fence" {
                "recovery_required"
            } else {
                "sealed"
            }
        );
        mount.release(mounted);
    }
}

/// The library CLI owns a plain drive: its removal guard must survive the
/// checkpointed call and release before the transport closes, on either exit.
#[test]
fn library_automatic_sealing_releases_owned_removal_guard() {
    for cut in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state_with_audit(root.path(), false);
        let paths = state.paths().clone();
        let mut config = state.config().clone();
        drop(state);
        config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
        config.tape_pools[0].watermark_low = 0.000001;
        config.tape_pools[0].watermark_high = 0.25;
        let state = StateHandle::open_with_config(paths, config).unwrap();
        let world = world();
        let snapshot = world.lock().unwrap().library_snapshot();
        let policy = StaticAllowlist::new(["UNUSED"]);
        let shared = Arc::clone(&world);
        let commands = Arc::new(Mutex::new(Vec::new()));
        let trace = Arc::clone(&commands);
        let mut library = snapshot
            .open_with(&policy, move |path| {
                let role = shared.lock().unwrap().role_for_path(path).unwrap();
                Ok(Box::new(Transport {
                    inner: ModelTransport::new(Arc::clone(&shared), role),
                    fault: fault(),
                    audit_cut: None,
                    hide_serial: false,
                    boundary: None,
                    commands: Some(Arc::clone(&trace)),
                    cut_after_prevent: cut,
                }))
            })
            .unwrap();
        let drive =
            crate::load_tape_by_uuid(state.catalog(), &mut library, &policy, &UUID).unwrap();
        let mut owner = crate::engine::drive_adapter::MountedDrive::from_placed_library(
            state,
            library,
            drive,
            policy,
            UUID,
            "seal",
            &root.path().join("locks"),
        );
        if let Ok(actor) = &mut owner {
            let result = actor.write_checkpointed(request(root.path(), "seal"));
            if cut {
                assert!(result.is_err());
            } else {
                result.unwrap();
                actor.close_session().unwrap();
                assert_eq!(
                    actor
                        .catalog_index()
                        .get_tape(&UUID)
                        .unwrap()
                        .unwrap()
                        .state,
                    "sealed"
                );
            }
            let prevents: Vec<_> = commands
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c[0] == 0x1e)
                .map(|c| c[4])
                .collect();
            assert!(!prevents.is_empty());
            assert!(
                prevents.iter().all(|value| *value == 1),
                "no premature ALLOW"
            );
            let _ = actor.finish();
        } else {
            assert!(cut);
        }
        let commands = commands.lock().unwrap();
        let removal: Vec<_> = commands
            .iter()
            .filter(|c| c[0] == 0x1e)
            .map(|c| c[4])
            .collect();
        assert_eq!(removal.iter().filter(|v| **v == 0).count(), 1);
        assert_eq!(removal.last(), Some(&0));
        assert!(removal[..removal.len() - 1].iter().all(|v| *v == 1));
        assert_eq!(
            commands
                .iter()
                .rev()
                .find(|c| c[0] == 0x1e)
                .unwrap()
                .as_slice(),
            &[0x1e, 0, 0, 0, 0, 0]
        );
    }
}

#[test]
fn mounted_positioning_reuses_only_current_bootstrap_evidence_and_releases_once() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
            fault: fault(),
            audit_cut: None,
            hide_serial: false,
            boundary: None,
            commands: Some(Arc::clone(&commands)),
            cut_after_prevent: false,
        }),
        &mut state,
        &root.path().join("locks"),
    )
    .unwrap();
    let mut mounted = backend.mount(target()).unwrap();
    commands.lock().unwrap().clear();
    crate::position_to_object(&mut mounted, crate::BotRelativeTapeFile(1)).unwrap();
    assert!(!commands.lock().unwrap().iter().any(|c| c[0] == 1));
    // The mount no longer sits just after Bootstrap: recover BOT before SPACE.
    commands.lock().unwrap().clear();
    crate::position_to_object(&mut mounted, crate::BotRelativeTapeFile(1)).unwrap();
    assert_eq!(commands.lock().unwrap().first().unwrap()[0], 1);
    mounted.drive.locate(1).unwrap();
    assert!(mounted.drive.known_position().is_some());
    mounted.drive.allow_medium_removal().unwrap();
    assert!(
        mounted.drive.known_position().is_none(),
        "release invalidates cached proof"
    );
    commands.lock().unwrap().clear();
    crate::position_to_object(&mut mounted, crate::BotRelativeTapeFile(1)).unwrap();
    assert_eq!(commands.lock().unwrap().first().unwrap()[0], 1);
    mounted.drive.prevent_medium_removal().unwrap();
    commands.lock().unwrap().clear();
    backend.release(mounted);
    assert_eq!(*commands.lock().unwrap(), vec![vec![0x1e, 0, 0, 0, 0, 0]]);
}

/// Physical release leaves endpoint sessions for startup recovery and cannot clear fences.
#[test]
fn release_context_does_not_run_write_owner_recovery() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let session = seed_operation_session(
        &state,
        remanence_library::DriveKey::Standalone {
            drive_serial: SERIAL.into(),
        },
        UUID,
    );
    let world = world();
    OperatorMount::release_with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
            fault: fault(),
            audit_cut: None,
            hide_serial: false,
            commands: None,
            cut_after_prevent: false,
            boundary: None,
        }),
        &mut state,
        &root.path().join("locks"),
    )
    .unwrap();
    assert!(state
        .catalog()
        .non_terminal_sessions()
        .unwrap()
        .iter()
        .any(|s| s.session_id == session));
    assert!(state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    {
        let mut reopened =
            backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        let mounted = reopened.mount(target()).unwrap();
        // C8-G2 / DEC-2: endpoint recovery ends library sessions without a
        // restart fence; native writes await their tape's next open.
        assert!(reopened.mutate_catalog(&mounted, |_| Ok(())).is_ok());
    }
    assert!(state
        .catalog()
        .non_terminal_sessions()
        .unwrap()
        .iter()
        .all(|s| s.session_id != session));
    assert!(state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
}

/// The release-only type is still a mutation boundary: ephemeral devices cannot
/// acquire it even with the read override used by diagnostic/read consumers.
#[test]
fn release_context_refuses_ephemeral_devices() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let commands = Arc::new(Mutex::new(Vec::new()));
    let policy = StaticAllowlist::new([DEVICE]).with_unserialized_device_allowed(Path::new(DEVICE));
    let result = OperatorMount::release_with_transport(
        Path::new(DEVICE),
        &policy,
        Box::new(Transport {
            inner: ModelTransport::new(world(), DeviceRole::Drive { bay: BAY }),
            fault: fault(),
            audit_cut: None,
            hide_serial: true,
            commands: Some(Arc::clone(&commands)),
            cut_after_prevent: false,
            boundary: None,
        }),
        &mut state,
        &root.path().join("locks"),
    );
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("ephemeral endpoint is read-only"));
    assert!(commands.lock().unwrap().iter().all(|c| c[0] != 0x1e));
}

/// Copy closed state, including journals, as an operator backup would.
fn copy_closed_state(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_closed_state(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).unwrap();
        }
    }
}
/// Restore an actual crash cut: the previous closed boundary plus a new unended owner.
fn open_crash_fixture(state: &StateHandle) {
    let authority = remanence_state::TapeAuthority::open(
        state.paths().journal_dir.join("tapes"),
        UUID,
        &state.paths().sqlite_path,
    )
    .unwrap();
    authority
        .acquire_exclusive()
        .unwrap()
        .open_session(
            [0xc7; 16],
            "2026-11-02T00:00:00Z".into(),
            remanence_state::tape_log::SessionDrive {
                serial: "DRV-TEST".into(),
                vendor: None,
                product: None,
                endpoint: remanence_state::library_log::DriveKey::Standalone(
                    remanence_state::library_log::Standalone {
                        drive_serial: "DRV-TEST".into(),
                    },
                ),
                drive_uuid: None,
            },
        )
        .unwrap();
}

fn dispatched_writes(world: &SharedVirtualWorld) -> usize {
    world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .filter(|c| matches!(c.opcode, 0x0a | 0x10))
        .count()
}
fn rollback_parity(enabled: bool) -> crate::ParityConfig {
    if enabled {
        crate::ParityConfig::Scheme(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("rollback"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        })
    } else {
        crate::ParityConfig::None
    }
}
fn set_rollback_bootstrap(world: &SharedVirtualWorld, parity: crate::ParityConfig) {
    let bootstrap =
        crate::build_tape_bootstrap(UUID, BLOCK, parity, "2026-10-23T00:00:00Z", "test");
    let mut bytes = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut bytes).unwrap();
    world.lock().unwrap().tapes.get_mut("HAND").unwrap().records[0] = Record::Block(bytes);
}

#[tokio::test]
async fn standalone_and_daemon_state_rollback_refuses_checkpoint_and_fresh_with_both_parity_modes()
{
    for parity_on in [false, true] {
        for fresh in [false, true] {
            for daemon in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let state_dir = root.path().join("state");
                let backup = root.path().join("backup");
                let parity = rollback_parity(parity_on);
                let world = world();
                set_rollback_bootstrap(&world, parity.clone());
                let mut state = state_with_parity(&state_dir, true, parity.clone());
                if !fresh {
                    append_for_rollback!(&mut state, &world, root.path(), "A").unwrap();
                }
                drop(state);
                copy_closed_state(&state_dir, &backup);
                let mut state = configured_state(&state_dir, true);
                let b = append_for_rollback!(&mut state, &world, root.path(), "B").unwrap();
                drop(state);
                let intact = world.lock().unwrap().tapes["HAND"].records.clone();
                std::fs::remove_dir_all(&state_dir).unwrap();
                copy_closed_state(&backup, &state_dir);
                let mut state = configured_state(&state_dir, true);
                let before = dispatched_writes(&world);
                let old = state
                    .write_handle()
                    .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                        tape_uuid: UUID,
                        barcode: Some("HAND".into()),
                        reason: "old_uncertain_session".into(),
                        evidence_json: None,
                    })
                    .unwrap();
                state
                    .write_handle()
                    .release_tape_io_fence(&old.quarantine_id, "old recovery completed")
                    .unwrap();
                let error = if daemon {
                    crate::write_owner::tests::daemon_rollback_refusal(
                        &state,
                        world.clone(),
                        crate::SelectedTape {
                            standalone_endpoint: None,
                            pool_id: "default".into(),
                            tape_uuid: UUID,
                            block_size: BLOCK,
                            parity_config: parity,
                        },
                    )
                    .await
                } else {
                    append_for_rollback!(&mut state, &world, root.path(), "C")
                        .unwrap_err()
                        .to_string()
                };

                assert!(
                    error.contains("TailBeyondKnownState"),
                    "parity={parity_on} fresh={fresh}: {error}"
                );
                assert!(
                    error.contains(&uuid::Uuid::from_bytes(b.object.object_id).to_string()),
                    "{error}"
                );
                assert_eq!(dispatched_writes(&world), before);
                assert_eq!(world.lock().unwrap().tapes["HAND"].records, intact);
            }
        }
    }
}

#[test]
fn complete_uncheckpointed_tail_requires_confirmation_and_records_discarded_ids() {
    for finalize_after in [false, true] {
        for parity_on in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("state");
            let backup = root.path().join("backup");
            let parity = rollback_parity(parity_on);
            let world = world();
            set_rollback_bootstrap(&world, parity.clone());
            let mut state = state_with_parity(&dir, false, parity);
            append_for_rollback!(&mut state, &world, root.path(), "A").unwrap();
            drop(state);
            copy_closed_state(&dir, &backup);
            let mut state = configured_state(&dir, false);
            let b = append_for_rollback!(&mut state, &world, root.path(), "B").unwrap();
            drop(state);
            // Crash after the complete object and closing delimiter, before checkpoint.
            let b_id = uuid::Uuid::from_bytes(b.object.object_id).to_string();
            {
                let mut locked = world.lock().unwrap();
                let records = &mut locked.tapes.get_mut("HAND").unwrap().records;
                let file = b.object.copies[0].tape_file_number as usize;
                let start = records
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| matches!(r, Record::Filemark))
                    .nth(file - 1)
                    .unwrap()
                    .0
                    + 1;
                let end = records[start..]
                    .iter()
                    .position(|r| matches!(r, Record::Filemark))
                    .unwrap()
                    + start;
                records.truncate(end + 1);
            }
            std::fs::remove_dir_all(&dir).unwrap();
            copy_closed_state(&backup, &dir);
            let mut state = configured_state(&dir, false);
            open_crash_fixture(&state);
            let paths = state.paths().clone();
            let library_uuid = state.library_runtime().manifest().library_uuid();
            let before = dispatched_writes(&world);
            assert!(append_for_rollback!(&mut state, &world, root.path(), "C")
                .unwrap_err()
                .to_string()
                .contains(&b_id));
            assert_eq!(dispatched_writes(&world), before);
            let mut mount =
                backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
            let mounted = mount.mount(target()).unwrap();
            let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
            assert!(actor
                .supersede_tail_confirmed("HAND", |_, _| Err("no confirmation".into()))
                .is_err());
            assert_eq!(dispatched_writes(&world), before);
            let discarded = actor
                .supersede_tail_confirmed("HAND", |expected, found| {
                    assert_eq!(expected, "SUPERSEDE HAND");
                    assert_eq!(found, std::slice::from_ref(&b_id));
                    Ok(())
                })
                .unwrap();
            assert_eq!(discarded, std::slice::from_ref(&b_id));
            use remanence_state::authority::TapeAuthorityView as _;
            let authority = remanence_state::TapeAuthority::open(
                paths.journal_dir.join("tapes"),
                UUID,
                &paths.sqlite_path,
            )
            .unwrap();
            let snapshot = authority.freeze().unwrap();
            let ending = snapshot.tape_summary().last_session_end.clone().unwrap();
            assert_eq!(ending.session_id, [0xc7; 16]);
            assert_eq!(
                ending.reason,
                remanence_state::tape_log::SessionEndReason::CrashRecovered
            );
            let crash_ack = ending
                .crash_ack_audit_sequence
                .expect("crash ending must cite the SupersedeTail acknowledgement");
            let mut tail = Vec::new();
            snapshot
                .visit_session_tail(&mut |file| {
                    tail.push(file.clone());
                    Ok(())
                })
                .unwrap();
            assert!(tail.iter().any(|file| file.identity
                == remanence_state::tape_log::TailIdentity::Object(b_id.clone())));
            drop(snapshot);
            drop(authority);
            if finalize_after {
                actor
                    .finalize_standalone(StandaloneFinalizeRequest {
                        tape_uuid: UUID,
                        acknowledge_tape: UUID,
                        expected_pool_id: Some("default".into()),
                        reason: "close recovered tape".into(),
                        idempotency_key: uuid::Uuid::new_v4(),
                    })
                    .unwrap();
            }
            let _ = actor.finish();
            let mut state = reopen_state();
            if !finalize_after {
                append_for_rollback!(&mut state, &world, root.path(), "C").unwrap();
            }
            drop(state);
            // Use the same validated replay as read_library_records; its Record callback
            // cannot borrow SupersedeTail, so inspect the public fold by source sequence.
            use remanence_state::library_log::{native_view::NativeTapeView, store, Tag};
            let view = NativeTapeView::probe(&paths.journal_dir.join("tapes"), &paths.sqlite_path)
                .unwrap();
            let mut sequences = Vec::new();
            let replay =
                store::replay_observed(&paths.audit_dir, library_uuid, &view, &mut |applied| {
                    if applied.record().tag() == Tag::SupersedeTail {
                        sequences.push(applied.sequence());
                    }
                    Ok(())
                })
                .unwrap();
            assert!(
                replay.refusal.is_none(),
                "typed library replay refused: {:?}",
                replay.refusal
            );
            assert!(replay.fold.pending.is_empty());
            let acknowledgments: Vec<_> = sequences
                .into_iter()
                .filter_map(|sequence| {
                    replay
                        .fold
                        .supersede(sequence)
                        .filter(|value| value.at.tape.uuid == UUID)
                        .map(|value| (sequence, value.clone()))
                })
                .collect();
            let (sequence, _) = acknowledgments
                .iter()
                .find(|(_, value)| value.session_id == Some([0xc7; 16]))
                .expect("library log must acknowledge this tape's crashed-session discard");
            assert_eq!(crash_ack, *sequence);
        }
    }
}

#[test]
fn stale_import_publishes_nothing_and_terminal_finalize_preserves_foreign_objects() {
    for parity_on in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let backup = root.path().join("backup");
        let parity = rollback_parity(parity_on);
        let world = world();
        set_rollback_bootstrap(&world, parity.clone());
        let mut state = state_with_parity(&dir, true, parity.clone());
        append_for_rollback!(&mut state, &world, root.path(), "A").unwrap();
        drop(state);
        copy_closed_state(&dir, &backup);
        let mut state = configured_state(&dir, true);
        let b = append_for_rollback!(&mut state, &world, root.path(), "B").unwrap();
        let id = uuid::Uuid::from_bytes(b.object.object_id).to_string();
        drop(state);
        let intact = world.lock().unwrap().tapes["HAND"].records.clone();
        let before = dispatched_writes(&world);
        let mut new_host = adopted_state(&root.path().join("new-host"), parity);
        let destination = new_host.paths().journal_dir.clone();
        let mut mount =
            backend(&mut new_host, &world, &root.path().join("locks"), fault()).unwrap();
        let mut mounted = mount.mount(target()).unwrap();
        let error = mount
            .import_journals(&mut mounted, &backup.join("journals"))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("JournalImportDeferred"), "{error}");
        mount.release(mounted);
        assert!(!remanence_state::TapeAuthority::exists(destination.join("tapes"), UUID).unwrap());
        drop(new_host);
        std::fs::remove_dir_all(&dir).unwrap();
        copy_closed_state(&backup, &dir);
        let mut state = configured_state(&dir, true);
        let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        let mounted = mount.mount(target()).unwrap();
        let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
        let error = actor
            .finalize_standalone(StandaloneFinalizeRequest {
                tape_uuid: UUID,
                acknowledge_tape: UUID,
                expected_pool_id: Some("default".into()),
                reason: "close tape".into(),
                idempotency_key: uuid::Uuid::new_v4(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains(&id), "{error}");
        let refusal = actor
            .supersede_tail_confirmed("HAND", |_, _| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("restore a newer copy of the state directory"),
            "{refusal}"
        );
        assert!(
            refusal.contains("cannot clear a stale terminal intent"),
            "{refusal}"
        );
        assert_eq!(dispatched_writes(&world), before);
        assert_eq!(world.lock().unwrap().tapes["HAND"].records, intact);
        let _ = actor.finish();
        drop(reopen_state());
    }
}

#[test]
fn torn_object_after_rollback_is_superseded_with_both_parity_modes() {
    for parity_on in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("state");
        let backup = root.path().join("backup");
        let parity = rollback_parity(parity_on);
        let world = world();
        set_rollback_bootstrap(&world, parity.clone());
        let mut state = state_with_parity(&dir, true, parity);
        append_for_rollback!(&mut state, &world, root.path(), "A").unwrap();
        drop(state);
        copy_closed_state(&dir, &backup);
        let mut state = configured_state(&dir, true);
        let b = append_for_rollback!(&mut state, &world, root.path(), "B").unwrap();
        drop(state);
        {
            let mut locked = world.lock().unwrap();
            let records = &mut locked.tapes.get_mut("HAND").unwrap().records;
            let start = records
                .iter()
                .enumerate()
                .filter(|(_, r)| matches!(r, Record::Filemark))
                .nth(b.object.copies[0].tape_file_number as usize - 1)
                .unwrap()
                .0
                + 1;
            records.truncate(start + 1);
        }
        std::fs::remove_dir_all(&dir).unwrap();
        copy_closed_state(&backup, &dir);
        let mut state = configured_state(&dir, true);
        open_crash_fixture(&state);
        append_for_rollback!(&mut state, &world, root.path(), "C").unwrap();
    }
}

#[test]
fn encrypted_complete_tail_is_preserved_without_a_private_key_and_past_empty_filemarks() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("state");
    let backup = root.path().join("backup");
    let world = world();
    let mut state = state(&dir);
    append_for_rollback!(&mut state, &world, root.path(), "A").unwrap();
    let boundary = world.lock().unwrap().tapes["HAND"].records.len();
    drop(state);
    copy_closed_state(&dir, &backup);
    let mut state = configured_state(&dir, true);
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    let mut input = request(root.path(), "encrypted-B");
    input.representation = crate::PoolWriteRepresentation::Encrypted {
        recipients: vec![
            remanence_aead::RecipientPrivateKey::new([9; 16], "primary", [7; 32])
                .unwrap()
                .public_key(0)
                .unwrap(),
            remanence_aead::RecipientPrivateKey::new([10; 16], "backup", [8; 32])
                .unwrap()
                .public_key(1)
                .unwrap(),
        ],
    };
    actor.write_checkpointed(input).unwrap();
    let _ = actor.finish();
    let state = reopen_state();
    drop(state);
    // A delimiter cannot hide later complete evidence from the suffix scan.
    world
        .lock()
        .unwrap()
        .tapes
        .get_mut("HAND")
        .unwrap()
        .records
        .insert(boundary, Record::Filemark);
    let intact = world.lock().unwrap().tapes["HAND"].records.clone();
    std::fs::remove_dir_all(&dir).unwrap();
    copy_closed_state(&backup, &dir);
    let mut state = configured_state(&dir, true);
    open_crash_fixture(&state);
    let before = dispatched_writes(&world);
    let error = append_for_rollback!(&mut state, &world, root.path(), "C")
        .unwrap_err()
        .to_string();
    assert!(error.contains("without a session-tail identity"), "{error}");
    assert_eq!(dispatched_writes(&world), before);
    assert_eq!(world.lock().unwrap().tapes["HAND"].records, intact);
}

#[tokio::test]
async fn daemon_abort_crash_supersede_and_healthy_append_both_parity_modes() {
    for parity_on in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let parity = rollback_parity(parity_on);
        let world = world();
        set_rollback_bootstrap(&world, parity.clone());
        let state = state_with_parity(&root.path().join("state"), true, parity.clone());
        crate::write_owner::tests::daemon_append_recovery_roundtrip(
            &state,
            world,
            crate::SelectedTape {
                standalone_endpoint: None,
                pool_id: "default".into(),
                tape_uuid: UUID,
                block_size: BLOCK,
                parity_config: parity,
            },
            root.path(),
        )
        .await;
    }
}

#[test]
fn bootstrap_without_filemark_can_be_explicitly_superseded() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    world
        .lock()
        .unwrap()
        .tapes
        .get_mut("HAND")
        .unwrap()
        .records
        .truncate(1);
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    assert!(actor
        .supersede_tail_confirmed("HAND", |expected, found| {
            assert_eq!(expected, "SUPERSEDE HAND");
            assert!(found.is_empty());
            Ok(())
        })
        .unwrap()
        .is_empty());
    actor
        .write_checkpointed(request(root.path(), "after-bootstrap-recovery"))
        .unwrap();
    let _ = actor.finish();
    drop(reopen_state());
}

#[test]
fn zero_dispatch_probe_failure_is_unavailable_and_not_durable() {
    let root = tempfile::tempdir().unwrap();
    let state = state(root.path());
    let cfg = crate::write_owner::TerminalWriteConfig {
        authority: crate::write_owner::TerminalAuthority::fixture(
            None,
            state.paths().audit_dir.clone(),
            true,
            &(Arc::new(Mutex::new(()))),
        ),
        io_memory: crate::io_memory::IoMemoryReservation::new(1024 * 1024 * 1024).unwrap(),
        mounted_endpoint: None,
        checkpoint_journal_dir: state.paths().journal_dir.join("tapes"),
    };
    let error = crate::append_admission::AdmissionError::new(
        crate::append_admission::AdmissionErrorKind::Io(remanence_library::TapeIoError::new(
            remanence_library::TapeIoErrorKind::OperationFailed("transient read failure".into()),
        )),
    );
    let outcome = crate::append_admission::admission_error_outcome(&error);
    let status = crate::append_admission::daemon_admission_error(
        remanence_state::AuditActor::System,
        &mut state.write_handle(),
        &cfg,
        UUID,
        error,
        &outcome,
    );
    assert_eq!(status.code(), tonic::Code::Unavailable);
    assert_eq!(
        state.catalog().get_tape(&UUID).unwrap().unwrap().state,
        "ready"
    );
    assert!(state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    assert!(remanence_state::FileAuditLog::replay(
        &cfg.authority
            .historical_fixture()
            .expect("historical audit fixture")
            .audit_dir
    )
    .unwrap()
    .iter()
    .all(|r| r.event != AuditEvent::RecoveryEvent));
}

/// Structural refusals quarantine only the tape, and retain the authenticated actor.
#[test]
fn structural_admission_refusals_project_recovery_with_authenticated_actor() {
    for error in [
        crate::append_admission::AdmissionError::new(
            crate::append_admission::AdmissionErrorKind::MissingBootstrapFilemark,
        ),
        crate::append_admission::AdmissionError::new(
            crate::append_admission::AdmissionErrorKind::Unproved(
                "control suffix position mismatch".into(),
            ),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let cfg = crate::write_owner::TerminalWriteConfig {
            authority: crate::write_owner::TerminalAuthority::Typed(state.library_runtime()),
            io_memory: crate::io_memory::IoMemoryReservation::new(1024 * 1024 * 1024).unwrap(),
            mounted_endpoint: None,
            checkpoint_journal_dir: state.paths().journal_dir.join("tapes"),
        };
        let actor = remanence_state::AuditActor::User("authenticated-test".into());
        let status = {
            let outcome = crate::append_admission::admission_error_outcome(&error);
            crate::append_admission::daemon_admission_error(
                actor.clone(),
                &mut state.write_handle(),
                &cfg,
                UUID,
                error,
                &outcome,
            )
        };
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        let fences = state.catalog().list_active_tape_io_fences().unwrap();
        assert_eq!(
            fences.len(),
            1,
            "structural refusal persists typed exclusion"
        );
        assert_eq!(fences[0].tape_uuid, UUID.to_vec());
        let paths = state.paths().clone();
        let config = state.config().clone();
        let runtime = state.library_runtime();
        let library_uuid = runtime.manifest().library_uuid();
        drop(runtime);
        drop(cfg);
        drop(state);
        let view = remanence_state::library_log::native_view::NativeTapeView::probe(
            &paths.journal_dir.join("tapes"),
            &paths.sqlite_path,
        )
        .unwrap();
        let mut raised = 0;
        remanence_state::library_log::store::replay_observed(
            &paths.audit_dir,
            library_uuid,
            &view,
            &mut |record| {
                if record.record().tag() == remanence_state::library_log::Tag::FenceRaised {
                    raised += 1;
                    assert_eq!(record.record().envelope.actor, actor);
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(raised, 1);
        let reopened = remanence_state::StateHandle::open_with_config(paths, config).unwrap();
        assert_eq!(
            reopened
                .catalog()
                .list_active_tape_io_fences()
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn tape_log_lease_contention_is_unavailable_and_other_lock_faults_are_not() {
    use remanence_state::tape_log::store::StoreError;
    let busy =
        remanence_state::StateError::from(StoreError::Locked(nix::errno::Errno::EWOULDBLOCK));
    assert_eq!(
        crate::status_from_state_error(busy).code(),
        tonic::Code::Unavailable
    );
    let fault = remanence_state::StateError::from(StoreError::Header);
    assert_eq!(
        crate::status_from_state_error(fault).code(),
        tonic::Code::FailedPrecondition
    );
}

/// A child forked by another thread shares the lock's open file description
/// until it execs, so release must unlock explicitly rather than rely on close.
#[test]
fn device_lock_release_frees_the_drive_while_a_duplicate_descriptor_lives() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().to_owned();
    let mut io = move |_: &str, logical: &Path| -> std::io::Result<PathBuf> {
        Ok(base.join(logical.strip_prefix("/run/lock/rem").unwrap()))
    };
    let held = lock_device(&mut io, "DUP-DRIVE").unwrap();
    // try_clone duplicates the descriptor onto the same open file description,
    // exactly what fork(2) hands a child that has not exec'd yet.
    let inherited = held.0.try_clone().unwrap();
    let error = lock_device(&mut io, "DUP-DRIVE")
        .err()
        .expect("held lock refuses a second owner")
        .to_string();
    assert!(error.contains("os error 11"), "{error}");
    drop(held);
    let reacquired = lock_device(&mut io, "DUP-DRIVE")
        .expect("released lock is free although a duplicate descriptor is still open");
    drop(reacquired);
    drop(inherited);
}

/// Omitted staging mode reaches the real ingress and writer on both tape layouts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_overlap_batches_small_objects_and_retains_mixed_receipts() {
    for parity_on in [false, true] {
        for serial in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let parity = rollback_parity(parity_on);
            let world = world();
            set_rollback_bootstrap(&world, parity.clone());
            let state = state_with_parity(root.path(), true, parity.clone());
            crate::write_owner::tests::check_overlap_batch_receipts(
                &state,
                world,
                crate::SelectedTape {
                    standalone_endpoint: None,
                    pool_id: "default".into(),
                    tape_uuid: UUID,
                    block_size: BLOCK,
                    parity_config: parity,
                },
                serial,
            )
            .await;
        }
    }
}

/// Durable receipts remain deliverable when the next append context cannot renew.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_barrier_context_failures_preserve_receipts() {
    for trigger in ["threshold", "explicit"] {
        let root = tempfile::tempdir().unwrap();
        let parity = rollback_parity(false);
        let world = world();
        set_rollback_bootstrap(&world, parity.clone());
        let mut state = state_with_parity(root.path(), true, parity.clone());
        crate::write_owner::tests::check_context_failure_receipts(
            &state,
            world,
            crate::SelectedTape {
                standalone_endpoint: None,
                pool_id: "default".into(),
                tape_uuid: UUID,
                block_size: BLOCK,
                parity_config: parity,
            },
            trigger,
        )
        .await;
        crate::write_owner::tests::assert_rebuild_safety_equivalent(&mut state);
    }
}

/// An uncertain bootstrap rewrite leaves replayable supersede failure evidence.
#[test]
fn supersede_tail_failure_fence_survives_empty_catalog_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    world
        .lock()
        .unwrap()
        .tapes
        .get_mut("HAND")
        .unwrap()
        .records
        .truncate(1);
    let cut = fault();
    let mut mount = backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        Arc::clone(&cut),
    )
    .unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    *cut.lock().unwrap() = Some(0x0a);
    assert!(actor
        .supersede_tail_confirmed("HAND", |_, _| Ok(()))
        .is_err());
    assert!(cut.lock().unwrap().is_none(), "the write fault must fire");
    let _ = actor.finish();
    let state = reopen_state();
    let paths = state.paths().clone();
    let config = state.config().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    let fences = state.catalog().list_active_tape_io_fences().unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "supersede_tail_failed");
    drop(state);
    drop(actor);
    crate::tests::assert_fences_survive_empty_rebuild(&paths, config, library_uuid, &fences);
}

/// Inspect the typed hold through L1-1's authoritative list, including initialization's planned tape.
fn assert_init_hold(
    runtime: &remanence_state::library_runtime::LibraryRuntime,
    error: &str,
) -> u64 {
    assert!(!runtime.motion_fenced());
    let holds = runtime.standalone_drive_holds().unwrap();
    assert_eq!(holds.len(), 1);
    let hold = &holds[0];
    assert_eq!(hold.drive_serial, SERIAL);
    assert_eq!(
        hold.cause,
        remanence_state::library_log::FenceCause::TransferError
    );
    let evidence = hold
        .detail
        .iter()
        .find(|(key, _)| key == "evidence_json")
        .unwrap();
    let evidence: serde_json::Value = serde_json::from_str(&evidence.1).unwrap();
    assert_eq!(
        evidence["tape_uuid"],
        uuid::Uuid::from_bytes(UUID).to_string()
    );
    assert_eq!(evidence["error"], error);
    hold.fence
}

/// A failed initialization holds its drive without latching; the next invocation refuses before RESERVE.
#[test]
fn b3b_standalone_init_failure_one_fence_by_cause() {
    use crate::tests::refusal_tests::{read_log, refusals, RecordedRefusal};
    use remanence_library::outcome::census;
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let paths = state.paths().clone();
    let runtime = state.library_runtime();
    let library_uuid = runtime.manifest().library_uuid();
    let world = world();
    let injected = fault();
    let mut mount = backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        injected.clone(),
    )
    .unwrap();
    let finished = std::cell::Cell::new((0, 0));
    let result: Result<(), _> = mount.initialize(
        false,
        |_, _| Ok((UUID, ())),
        |drive, _, ()| {
            *injected.lock().unwrap() = Some(10);
            let before = census::count();
            let error = drive.write_block_unpositioned(&[0; 512]).unwrap_err();
            assert_eq!(census::count() - before, 1);
            finished.set((census::total(), census::normalizations()));
            Err(error.to_string())
        },
    );
    let error = result.unwrap_err().to_string();
    assert!(
        !error.contains("cannot durably record standalone write uncertainty"),
        "{error}"
    );
    assert_eq!(
        census::total(),
        finished.get().0 + 1,
        "only ALLOW removal follows the failure"
    );
    assert_eq!(census::normalizations(), finished.get().1);
    assert!(mount.unusable);
    let hold = assert_init_hold(&runtime, &error);
    let fences = mount.state.catalog().list_active_tape_io_fences().unwrap();
    assert!(fences.is_empty(), "{fences:?}");
    let commands = world.lock().unwrap().command_log.len();
    let again: Result<(), _> = mount.initialize(
        false,
        |_, _| panic!("spent invocation cannot prepare"),
        |_, _, (): ()| panic!("spent invocation cannot write"),
    );
    assert!(again.is_err());
    assert_eq!(world.lock().unwrap().command_log.len(), commands);
    drop((mount, runtime));
    drop(state);

    assert!(refusals(&read_log(&paths, library_uuid)).is_empty());

    let mut state = configured_state(root.path(), false);
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    world.lock().unwrap().command_log.clear();
    let prepared = std::cell::Cell::new(false);
    let retry: Result<(), _> = mount.initialize(
        true,
        |_, _| {
            prepared.set(true);
            Err::<(TapeUuid, ()), _>("prepare ran".into())
        },
        |_, _, ()| panic!("held drive must not write"),
    );
    let error = retry.unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("DriveFenced: drive P4-MODEL library:{hold}")),
        "{error}"
    );
    assert!(!prepared.get());
    assert!(world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .all(|c| c.opcode != 0x16));
    drop(mount);
    drop(state);
    assert_eq!(
        refusals(&read_log(&paths, library_uuid)),
        [RecordedRefusal::expect(
            12,
            "DriveFenced",
            "tape",
            None,
            remanence_state::AuditActor::local_user(),
        )]
    );
}

/// Code 5, through `OperatorMount::initialize`, the standalone write path. A failed WRITE whose
/// uncertainty the library log cannot record latches the motion fence and refuses with its text,
/// and the raise is counted once in the library log, naming the tape. (C8-G2 / DEC-2:
/// `recover_sessions_after_restart` at a standalone mount does not reach `raise_standalone_fence`;
/// it recovers only library-log sessions, which have no write kind.)
#[test]
fn an_unrecordable_standalone_write_fence_is_counted_once_with_its_tape() {
    use crate::tests::refusal_tests::{read_log, refusals, RecordedRefusal};
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let paths = state.paths().clone();
    let runtime = state.library_runtime();
    let library_uuid = runtime.manifest().library_uuid();
    let world = world();
    let injected = fault();
    let mut mount = backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        injected.clone(),
    )
    .unwrap();
    assert!(!runtime.motion_fenced());
    runtime.fail_next_standalone_hold_for_test();
    let result: Result<(), _> = mount.initialize(
        false,
        |_, _| Ok((UUID, ())),
        |drive, _, ()| {
            *injected.lock().unwrap() = Some(10);
            let error = drive.write_block_unpositioned(&[0; 512]).unwrap_err();
            Err(error.to_string())
        },
    );
    // The closure's refusal ("cannot durably record standalone write uncertainty ...") is what
    // `initialize` returns: it flushes the audit first, then returns the closure's refusal.
    let error = result
        .expect_err("the fence cannot be recorded")
        .to_string();
    assert!(
        error.starts_with("cannot durably record standalone write uncertainty for "),
        "{error}"
    );
    assert!(
        error.ends_with("; motion is fenced and recovery is required"),
        "{error}"
    );
    assert!(
        error.contains("; the drive hold was not recorded: "),
        "{error}"
    );
    assert!(mount.unusable);
    assert!(runtime.motion_fenced());
    assert!(runtime.standalone_drive_holds().unwrap().is_empty());
    drop((mount, runtime));
    drop(state);

    let log = read_log(&paths, library_uuid);
    assert_eq!(
        refusals(&log),
        [RecordedRefusal::expect(
            5,
            "StandaloneFenceUnrecorded",
            "tape",
            Some(UUID),
            remanence_state::AuditActor::local_user(),
        )]
    );
}

/// Mounted origin conversion discards the bootstrap shortcut after a failed pipeline.
#[test]
fn b3b_mounted_object_origin_after_pipeline_failure() {
    b3b_pipeline_position_consumer("origin");
}

/// Catalogless candidate listing positions from BOT after a failed pipeline, then reads the correct object.
#[test]
fn b3b_catalogless_listing_after_pipeline_failure() {
    b3b_pipeline_position_consumer("listing");
}

/// A candidate cache miss LOCATEs after a failed pipeline before reading the saved block.
#[test]
fn b3b_tape_candidate_after_pipeline_failure() {
    b3b_pipeline_position_consumer("candidate");
}

/// Build real object bytes, inject a partial pipeline at the consumer's cached cursor, and inspect subsequent CDBs.
fn b3b_pipeline_position_consumer(consumer: &str) {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let lock = root.path().join("locks");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "position-object"))
        .unwrap();
    actor.finish().unwrap();
    let mut state = reopen_state();
    let armed = fault();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut mount = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(world.clone(), DeviceRole::Drive { bay: BAY }),
            fault: armed.clone(),
            audit_cut: None,
            hide_serial: false,
            boundary: None,
            commands: Some(commands.clone()),
            cut_after_prevent: false,
        }),
        &mut state,
        &lock,
    )
    .unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let fail = |drive: &mut DriveHandle| {
        *armed.lock().unwrap() = Some(0xff);
        let error = drive
            .write_block_batch_pipelined(
                &vec![0; (4 * BLOCK) as usize],
                BLOCK,
                &remanence_scsi::read_write::build_write_fixed_cdb(4),
            )
            .unwrap_err();
        assert!(matches!(
            error.kind,
            remanence_library::TapeIoErrorKind::PartialBatchUncommittable { .. }
        ));
        assert!(armed.lock().unwrap().is_none());
        assert!(drive.known_position().is_none());
        commands.lock().unwrap().clear();
    };
    match consumer {
        "origin" => {
            assert_eq!(mounted.drive.known_position().unwrap().lba, 1);
            fail(&mut mounted.drive);
            let position =
                crate::position_to_object(&mut mounted, crate::BotRelativeTapeFile(1)).unwrap();
            assert_eq!(position.lba, 2);
            let commands = commands.lock().unwrap().clone();
            assert_eq!(commands.first().unwrap()[0], 1, "must rewind before SPACE");
            assert!(commands.iter().any(|c| c[0] == 0x11));
        }
        "listing" => {
            fail(&mut mounted.drive);
            let report = crate::restore_catalogless_selected(
                &mut mounted.drive,
                None,
                None,
                true,
                true,
                &[],
            )
            .unwrap();
            let candidates = report["candidates"].as_array().unwrap();
            assert_eq!(candidates.len(), 1, "{report}");
            assert!(candidates[0]["manifest"]["rows"].is_array(), "{report}");
            assert!(report.to_string().contains("position-object"), "{report}");
            let commands = commands.lock().unwrap().clone();
            let rewind = commands.iter().position(|c| c[0] == 1).unwrap();
            let read = commands.iter().position(|c| c[0] == 8).unwrap();
            let positioning = commands
                .iter()
                .position(|c| matches!(c[0], 1 | 0x2b | 0x92))
                .unwrap();
            assert!(
                positioning < read,
                "bootstrap discovery establishes a fresh position before reading"
            );
            let space = commands.iter().rposition(|c| c[0] == 0x11).unwrap();
            let candidate_read = commands.iter().rposition(|c| c[0] == 8).unwrap();
            assert!(
                rewind < space && space < candidate_read,
                "candidate listing proves its own origin"
            );
            assert!(
                commands.iter().any(|c| c[0] == 0x11),
                "candidate positioning uses SPACE"
            );
        }
        "candidate" => {
            let expected = match &world.lock().unwrap().tapes["HAND"].records[2] {
                Record::Block(block) => block[..16].to_vec(),
                other => panic!("{other:?}"),
            };
            crate::catalogless_restore::b3b_check_candidate_after_failure(
                &mut mounted.drive,
                BLOCK,
                fail,
                &expected,
            );
            let commands = commands.lock().unwrap().clone();
            assert!(matches!(commands.first().unwrap()[0], 0x2b | 0x92));
            let locate = commands
                .iter()
                .position(|c| matches!(c[0], 0x2b | 0x92))
                .unwrap();
            let read = commands.iter().position(|c| c[0] == 8).unwrap();
            assert!(locate < read);
        }
        _ => unreachable!(),
    }
    mount.release(mounted);
}

/// A post-write adapter failure holds the drive while preserving the original failure and no tape fence.
fn fix2_init_adapter(scenario: &str) {
    use crate::tests::refusal_tests::{read_log, refusals};
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let paths = state.paths().clone();
    let runtime = state.library_runtime();
    let library_uuid = runtime.manifest().library_uuid();
    let world = world();
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let result: Result<(), _> = mount.initialize(
        false,
        |_, _| Ok((UUID, ())),
        |drive, _, ()| {
            let before = drive.media_write_dispatch_count();
            let error = crate::pool_write::post_write_adapter_error(drive, scenario);
            assert!(drive.media_write_dispatch_count() > before);
            assert!(error.outcome().write_context);
            assert_eq!(
                error.outcome().evidence.dispatch,
                remanence_library::outcome::Dispatch::NotIssued
            );
            Err(error.to_string())
        },
    );
    let error = result.unwrap_err().to_string();
    assert!(
        !error.contains("cannot durably record standalone write uncertainty"),
        "{error}"
    );
    assert!(mount.unusable, "{scenario}");
    assert_init_hold(&runtime, &error);
    let fences = mount.state.catalog().list_active_tape_io_fences().unwrap();
    assert!(fences.is_empty(), "{scenario}: {fences:?}");
    drop((mount, runtime));
    drop(state);

    let log = read_log(&paths, library_uuid);
    assert!(refusals(&log).is_empty(), "{scenario}");
}

/// An overlap completion failure holds the standalone drive.
#[test]
fn b3b_fix2_init_overlap_post_write() {
    fix2_init_adapter("overlap");
}

/// A post-write parity block-size refusal holds the standalone drive.
#[test]
fn b3b_fix2_init_parity_size_post_write() {
    fix2_init_adapter("parity-size");
}

/// A post-write poisoned parity sink refusal holds the standalone drive.
#[test]
fn b3b_fix2_init_parity_poisoned_post_write() {
    fix2_init_adapter("parity-poisoned");
}

/// R6 still removes the initialization blanket fence for a dispatched zero-byte definite rejection.
#[test]
fn b3b_fix2_init_r6_definite_rejection() {
    use remanence_library::outcome::{Dispatch, Effect};
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let world = world();
    let injected = fault();
    let mut mount = backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        injected.clone(),
    )
    .unwrap();
    let result: Result<(), _> = mount.initialize(
        false,
        |_, _| Ok((UUID, ())),
        |drive, _, ()| {
            drive
                .write_block_unpositioned(&vec![0; BLOCK as usize])
                .unwrap();
            *injected.lock().unwrap() = Some(0xfe);
            let error = drive
                .write_block_unpositioned(&vec![0; BLOCK as usize])
                .unwrap_err();
            assert_eq!(error.outcome().evidence.dispatch, Dispatch::Issued);
            assert!(error.outcome().write_context);
            assert_eq!(error.outcome().effect, Effect::NoFence);
            Err(error.to_string())
        },
    );
    assert!(result.is_err());
    assert!(injected.lock().unwrap().is_none());
    assert!(mount
        .state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    assert!(
        !remanence_state::FileAuditLog::replay(&mount.state.paths().audit_dir)
            .unwrap()
            .iter()
            .any(|e| e.event == AuditEvent::TapeIoFenceRaised)
    );
}

/// The real operator constructor installs the shared typed runtime on its actual
/// VPD endpoint. LOAD and UNLOAD confirm through TUR plus READ POSITION and replay.
#[test]
fn operator_motion_publishes_typed_intent_observation_and_done() {
    use remanence_state::library_log as lib;
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let paths = state.paths().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    let world = world();
    world.lock().unwrap().leave_cartridge_unthreaded(BAY);
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    mount.context.drive.as_mut().unwrap().load().unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mounted.drive.allow_medium_removal().unwrap();
    mounted.drive.unload().unwrap();
    mount.release(mounted);
    drop(state);
    let view = lib::native_view::NativeTapeView::probe(
        &paths.journal_dir.join("tapes"),
        &paths.sqlite_path,
    )
    .unwrap();
    let mut intents = Vec::new();
    let mut observations = Vec::new();
    let mut outcomes = Vec::new();
    lib::store::replay_observed(&paths.audit_dir, library_uuid, &view, &mut |applied| {
        if let Some(intent) = applied.record().motion_intent() {
            intents.push((applied.sequence(), intent.clone()));
        }
        if let Some(inventory) = applied.record().inventory_observation() {
            observations.push((applied.sequence(), inventory.clone()));
        }
        if let Some(outcome) = applied.record().motion_outcome() {
            outcomes.push((applied.sequence(), outcome.clone()));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(intents.len(), 2);
    assert_eq!(observations.len(), 2);
    assert_eq!(outcomes.len(), 2);
    for (
        (intent_sequence, intent),
        ((observation_sequence, inventory), (outcome_sequence, outcome)),
    ) in intents.iter().zip(observations.iter().zip(&outcomes))
    {
        let endpoint = match &intent.motion {
            lib::Motion::Load(load) => &load.endpoint,
            lib::Motion::Unload(unload) => &unload.endpoint,
            _ => panic!("standalone CDB"),
        };
        assert_eq!(
            endpoint,
            &lib::DriveKey::Standalone(lib::Standalone {
                drive_serial: SERIAL.into()
            })
        );
        assert!(inventory.resolves.0.is_empty());
        assert!(matches!(
            inventory.subject,
            lib::InventorySubject::Standalone(_)
        ));
        assert!(
            *intent_sequence < *observation_sequence && *observation_sequence < *outcome_sequence
        );
        assert_eq!(outcome.result, lib::MotionResult::Done);
        assert_eq!(outcome.intent.sequence, *intent_sequence);
        assert_eq!(
            outcome.confirmation,
            Some(lib::Confirmation::Inventory(*observation_sequence))
        );
    }
}

/// A process cut before/after dispatch leaves an open/Unknown intent. The real
/// operator constructor observes TUR and READ POSITION before resolving its own
/// endpoint; it never replays the old motion and leaves another endpoint dirty.
#[test]
fn operator_startup_resolves_only_observed_open_or_unknown_motion() {
    use remanence_library::motion::{MotionAuthority, MotionCommand, MotionContext};
    use remanence_state::library_log as lib;
    for unknown in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = state(root.path());
        let runtime = state.library_runtime();
        let command = MotionCommand::Load(remanence_library::DriveKey::Standalone {
            drive_serial: SERIAL.into(),
        });
        let other = MotionCommand::Load(remanence_library::DriveKey::Standalone {
            drive_serial: "OTHER".into(),
        });
        let ticket = runtime.intent(&command, &MotionContext::default()).unwrap();
        runtime.intent(&other, &MotionContext::default()).unwrap();
        if unknown {
            runtime
                .append(|a, v| {
                    a.motion_outcome(
                        lib::MotionOutcome {
                            intent: lib::DecisionRef {
                                sequence: ticket.sequence,
                                record_uuid: ticket.record_uuid,
                            },
                            result: lib::MotionResult::Unknown,
                            evidence: lib::CompletionEvidence {
                                dispatch: lib::Dispatch::Issued,
                                completion: lib::Completion::Unknown,
                                scsi_status: None,
                                host_status: None,
                                driver_status: None,
                                sense: None,
                                residual: None,
                            },
                            confirmation: None,
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer2,
                        v,
                    )
                })
                .unwrap();
        }
        let paths = state.paths().clone();
        let config = state.config().clone();
        let log_uuid = runtime.manifest().library_uuid();
        drop(runtime);
        drop(state);
        let mut state = StateHandle::open_with_config(paths.clone(), config).unwrap();
        let world = world();
        world.lock().unwrap().command_log.clear();
        let mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        drop(mount);
        let runtime = state.library_runtime();
        assert!(!runtime.requires_reconciliation(&command));
        assert!(runtime.requires_reconciliation(&other));
        let commands = world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .map(|v| v.opcode)
            .collect::<Vec<_>>();
        assert!(commands.contains(&0x00) && commands.contains(&0x34));
        assert!(
            !commands
                .iter()
                .any(|v| matches!(v, 0x1b | 0xa5 | 0x0a | 0x10)),
            "startup must only observe: {commands:?}"
        );
        drop(runtime);
        drop(state);
        let view = lib::native_view::NativeTapeView::probe(
            &paths.journal_dir.join("tapes"),
            &paths.sqlite_path,
        )
        .unwrap();
        let mut resolved = Vec::new();
        lib::store::replay_observed(&paths.audit_dir, log_uuid, &view, &mut |record| {
            if let Some(value) = record.record().inventory_observation() {
                resolved.extend(value.resolves.0.iter().copied());
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            resolved,
            vec![lib::DecisionRef {
                sequence: ticket.sequence,
                record_uuid: ticket.record_uuid
            }]
        );
    }
}

#[test]
fn operator_startup_failed_position_keeps_motion_unresolved() {
    use remanence_library::motion::{MotionAuthority, MotionCommand, MotionContext};
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let runtime = state.library_runtime();
    let command = MotionCommand::Unload(remanence_library::DriveKey::Standalone {
        drive_serial: SERIAL.into(),
    });
    runtime.intent(&command, &MotionContext::default()).unwrap();
    let world = world();
    world.lock().unwrap().command_log.clear();
    let failed = Arc::new(Mutex::new(Some(0x34)));
    let result = backend(&mut state, &world, &root.path().join("locks"), failed);
    assert!(matches!(result,Err(ref error) if error.to_string().contains("TUR/READ POSITION")));
    drop(result);
    assert!(runtime.requires_reconciliation(&command));
    assert!(!world
        .lock()
        .unwrap()
        .command_log
        .iter()
        .any(|v| matches!(v.opcode, 0x1b | 0xa5 | 0x0a | 0x10)));
}

/// Reproducible operator spool measurement, run explicitly on the pre-cutover
/// export and current tree with the same model, payload and fsync policy. No
/// device path is opened: OperatorMount receives only the injected transport.
#[test]
#[ignore = "explicit operator spool measurement"]
fn operator_spool_cutover_measurement() {
    use remanence_state::authority::TapeAuthorityView;
    const PAYLOAD: usize = 8 * 1024 * 1024;
    const OBJECTS: usize = 8;
    for sample in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let mut state = state(root.path());
        let world = world();
        {
            let records = &world.lock().unwrap().tapes["HAND"].records;
            assert_eq!(records.len(), 2);
            assert!(matches!(records[1], Record::Filemark));
            let Record::Block(bytes) = &records[0] else {
                panic!("bootstrap")
            };
            let bootstrap = remanence_parity::bootstrap::parse_bootstrap_block(bytes).unwrap();
            assert_eq!(bootstrap.tape_uuid, UUID);
            assert_eq!(bootstrap.block_size_bytes, BLOCK);
            assert!(bootstrap.scheme.is_none());
            let authority = remanence_state::TapeAuthority::open(
                state.paths().journal_dir.join("tapes"),
                UUID,
                &state.paths().sqlite_path,
            )
            .unwrap();
            let mut lease = authority.acquire_exclusive().unwrap();
            lease
                .publish_initialized(
                    BLOCK,
                    None,
                    remanence_parity::CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
            state
                .write_handle()
                .project_tape_log(&lease.freeze().unwrap())
                .unwrap();
        }
        let input = root.path().join("payload");
        let payload: Vec<u8> = (0..PAYLOAD)
            .map(|i| (i.wrapping_mul(31).wrapping_add(i / 251) & 255) as u8)
            .collect();
        std::fs::write(&input, payload).unwrap();
        let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        let mounted = mount.mount(target()).unwrap();
        let (mut actor, reopen) = own_mount!(mount, mounted, state);
        world.lock().unwrap().command_log.clear();
        let start = std::time::Instant::now();
        for object in 0..OBJECTS {
            actor
                .write_checkpointed(WriteObjectToPoolRequest {
                    pool_id: "default".into(),
                    source: crate::WriteObjectSource::Path(input.clone()),
                    archive_path: format!("sample-{sample}-{object}").into(),
                    caller_object_id: format!("sample-{sample}-{object}"),
                    expected_content_sha256: None,
                    expected_object_id: None,
                    input_kind: crate::WriteObjectInputKind::LogicalFile,
                    representation: crate::PoolWriteRepresentation::Plaintext,
                })
                .unwrap();
        }
        let seconds = start.elapsed().as_secs_f64();
        let writes = world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter(|c| c.opcode == 0x0a)
            .count();
        actor.finish().unwrap();
        let state = reopen();
        assert_eq!(
            state.catalog().list_native_objects().unwrap().len(),
            OBJECTS
        );
        let authority_bytes: u64 = std::fs::read_dir(&state.paths().audit_dir)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap())
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
            .sum();
        println!("CUTOVER_SPOOL {{\"sample\":{sample},\"input_bytes\":{},\"seconds\":{seconds:.6},\"mib_per_second\":{:.6},\"write_cdbs\":{writes},\"library_source_bytes\":{authority_bytes}}}",PAYLOAD*OBJECTS,(PAYLOAD*OBJECTS) as f64/1048576.0/seconds);
    }
}

/// L1-R3a fixtures publish the model's verified Bootstrap through the native owner.
/// Shared only by the new production-path tests; the ruling-11 reproduction stays intact.
fn r3a_initialized_state(root: &Path, world: &SharedVirtualWorld) -> StateHandle {
    use remanence_state::authority::TapeAuthorityView;
    let state = state(root);
    let records = world.lock().unwrap().tapes["HAND"].records.clone();
    assert_eq!(records.len(), 2);
    let Record::Block(bytes) = &records[0] else {
        panic!("Bootstrap")
    };
    let bootstrap = remanence_parity::bootstrap::parse_bootstrap_block(bytes).unwrap();
    assert_eq!(bootstrap.tape_uuid, UUID);
    assert_eq!(bootstrap.block_size_bytes, BLOCK);
    assert!(bootstrap.scheme.is_none());
    assert!(matches!(records[1], Record::Filemark));
    let authority = remanence_state::TapeAuthority::open(
        state.paths().journal_dir.join("tapes"),
        UUID,
        &state.paths().sqlite_path,
    )
    .unwrap();
    let mut lease = authority.acquire_exclusive().unwrap();
    lease
        .publish_initialized(
            BLOCK,
            None,
            remanence_parity::CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    state
        .write_handle()
        .project_tape_log(&lease.freeze().unwrap())
        .unwrap();
    state
}

/// DEC-1 and D-N-1-1: the coupled failure latches, then recovery admits a WRITE after its durable ending.
#[test]
fn r3a_recovery_admits_mounted_write_after_durable_ending() {
    use remanence_state::library_log as lib;
    let root = tempfile::tempdir().unwrap();
    let world = world();
    let mut state = r3a_initialized_state(root.path(), &world);
    let paths = state.paths().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    let audit_path = std::fs::read_dir(&paths.audit_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "remlib"))
        .unwrap();
    let fault = fault();
    let mut mount = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: BAY }),
            fault: fault.clone(),
            audit_cut: Some(audit_path.clone()),
            hide_serial: false,
            commands: None,
            cut_after_prevent: false,
            boundary: None,
        }),
        &mut state,
        &root.path().join("locks"),
    )
    .unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, reopen_state) = own_mount!(mount, mounted, state);
    actor.open_session("default").unwrap();
    *fault.lock().unwrap() = Some(0x0a);
    assert!(actor
        .write_checkpointed(request(root.path(), "r3a-cut"))
        .is_err());
    assert!(actor
        .catalog_index()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    let marker = paths.audit_dir.join("library-log.fence-reconcile");
    let names_tape = || match std::fs::read(&marker) {
        Ok(bytes) => bytes.chunks_exact(16).any(|entry| entry == UUID),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => panic!("read reconcile marker: {error}"),
    };
    assert!(
        !names_tape(),
        "a durable live ending owes no reconciled fence (DEC-1, D-N-1-1)"
    );
    std::fs::remove_dir(&audit_path).unwrap();
    std::fs::rename(audit_path.with_extension("saved"), &audit_path).unwrap();
    let count = dispatched_writes(&world);
    assert!(actor
        .write_checkpointed(request(root.path(), "r3a-latched"))
        .is_err());
    assert_eq!(dispatched_writes(&world), count);
    let _ = actor.finish();
    {
        use lib::{
            native_view::NativeTapeView,
            store::{Clock, Origin, UuidSource},
            LibraryLog,
        };
        struct Now;
        impl Clock for Now {
            fn now(&self) -> u64 {
                1
            }
        }
        struct Fresh;
        impl UuidSource for Fresh {
            fn next(&self, _: u64) -> [u8; 16] {
                *uuid::Uuid::new_v4().as_bytes()
            }
        }
        let _lease = remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap();
        let native = remanence_state::tape_log::store::Store::open(
            paths.journal_dir.join("tapes"),
            UUID,
            &paths.sqlite_path,
        )
        .unwrap();
        let native = native.snapshot().unwrap();
        let end = native.summary().last_session_end.as_ref().unwrap();
        assert_eq!(
            end.reason,
            remanence_state::tape_log::SessionEndReason::DriveError
        );
        let view =
            NativeTapeView::probe(&paths.journal_dir.join("tapes"), &paths.sqlite_path).unwrap();
        let mut log = LibraryLog::open(
            &paths.audit_dir,
            library_uuid,
            Origin {
                host: "r3a-recovery".into(),
                pid: std::process::id(),
                build: "test".into(),
            },
            Box::new(Now),
            Box::new(Fresh),
            true,
            &view,
        )
        .unwrap();
        log.clear_ambiguity(&view).unwrap();
        assert!(!names_tape());
    }
    let mut state = reopen_state();
    let fences = state.catalog().list_active_tape_io_fences().unwrap();
    assert!(
        fences.is_empty(),
        "a durable live ending owes no quarantine (DEC-1, D-N-1-1)"
    );
    assert!(!names_tape());
    let mut mount = backend(
        &mut state,
        &world,
        &root.path().join("locks"),
        self::fault(),
    )
    .unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, _reopen_state) = own_mount!(mount, mounted, state);
    let before = dispatched_writes(&world);
    actor
        .write_checkpointed(request(root.path(), "r3a-refused"))
        .unwrap();
    assert!(dispatched_writes(&world) > before);
    actor.finish().unwrap();
}

/// A3: a real mounted Object and clean close produce the zero-tail native shape used by S10.
#[test]
fn r3a_clean_mounted_close_has_no_native_tail() {
    let root = tempfile::tempdir().unwrap();
    let world = world();
    let mut state = r3a_initialized_state(root.path(), &world);
    let paths = state.paths().clone();
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    let (mut actor, _reopen_state) = own_mount!(mount, mounted, state);
    actor
        .write_checkpointed(request(root.path(), "r3a-clean"))
        .unwrap();
    actor.close_session().unwrap();
    actor.finish().unwrap();
    let native = remanence_state::tape_log::store::Store::open(
        paths.journal_dir.join("tapes"),
        UUID,
        &paths.sqlite_path,
    )
    .unwrap();
    let native = native.snapshot().unwrap();
    assert!(native.summary().open_session.is_none());
    let end = native.summary().last_session_end.as_ref().unwrap();
    eprintln!(
        "A3 observed: reason={:?}, tail_file_count={}, possible_tail_blocks={}",
        end.reason, end.tail_file_count, end.possible_tail_blocks
    );
    assert_eq!(
        end.reason,
        remanence_state::tape_log::SessionEndReason::CleanClose
    );
    assert_eq!(
        end.tail_file_count, 0,
        "A3 requires stop and report on nonzero tail"
    );
    assert_eq!(
        end.possible_tail_blocks, 0,
        "A3 requires stop and report on nonzero tail"
    );
}
