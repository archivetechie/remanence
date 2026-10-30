//! P1 C2–C5 oracles at the real drive, mount, write, and restore boundaries.
//! Proposed test constructors inject transport and lock IO into OperatorMount;
//! they must use the production mount and
//! write paths. No replacement writer, identity verifier, or extractor lives here.
//! Missing interfaces deliberately leave this integration target compile-red.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use remanence_api::engine::drive_adapter::MountedDrive;
use remanence_api::{
    build_tape_bootstrap, check_writability_preconditions, lto_generation_from_density,
    scan_and_restore, verify_tape_identity, LtoGen, MountBackend, MountError, MountTarget,
    OperatorMount, PoolWriteRepresentation, TapeUuid, WriteObjectInputKind, WriteObjectSource,
    WriteObjectToPoolRequest,
};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::transport::TransferOutcome;
use remanence_library::{
    DriveHandle, DriveHandleSource, FixtureTransport, MountEndpoint, RecordingLog,
    RecordingTransport, ScsiError, SgTransport, StaticAllowlist, TapeIoError, TimeoutClass,
    VecBlockSink,
};
use remanence_parity::{bootstrap::write_bootstrap_block, ParityConfig};
use remanence_state::{
    CatalogIndex, FileAuditLog, ProvisionTapeInput, StateHandle, StatePaths, TapeIoFenceInput,
};

const DEVICE: &str = "/dev/sg-chaos-drive-0100";
const SERIAL: &str = "ORACLE-STANDALONE-1";
const BAY: u16 = 0x0100; // Model addressing only; never an endpoint identity.
const UUID: TapeUuid = [0x31; 16];
const OTHER: TapeUuid = [0x42; 16];
const BLOCK: u32 = 262144;
const AT: &str = "2026-10-15T00:00:00Z";

fn tape(uuid: TapeUuid) -> VirtualTape {
    let payload = build_tape_bootstrap(uuid, BLOCK, ParityConfig::None, AT, "P1-oracle");
    let mut block = vec![0; BLOCK as usize];
    write_bootstrap_block(&payload, &mut block).unwrap();
    let mut tape = VirtualTape::empty(128 * 1024 * 1024, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK);
    tape
}

fn world(tape: VirtualTape) -> SharedVirtualWorld {
    let mut world = VirtualWorld::single_drive("UNUSED-CHANGER", BAY, SERIAL, 0x0400, 0);
    world.put_tape_in_drive(BAY, "ORACLEL9", None, tape);
    Arc::new(Mutex::new(world))
}

fn transport(world: &SharedVirtualWorld) -> FaultTransport {
    FaultTransport {
        inner: ModelTransport::new(Arc::clone(world), DeviceRole::Drive { bay: BAY }),
        armed: Arc::new(Mutex::new(None)),
        hide_serial: false,
        fence_observer: None,
        session_observer: None,
    }
}

fn policy() -> StaticAllowlist {
    StaticAllowlist::new([SERIAL, DEVICE])
}

fn target(uuid: Option<TapeUuid>) -> MountTarget {
    MountTarget::Device {
        path: DEVICE.into(),
        expected_uuid: uuid,
    }
}

fn provision(index: &mut CatalogIndex) {
    index
        .provision_tape(ProvisionTapeInput {
            tape_uuid: UUID,
            voltag: "ORACLEL9".into(),
            block_size: BLOCK,
            parity: ParityConfig::None,
            force: false,
        })
        .unwrap();
}

fn state(root: &Path) -> StateHandle {
    fs::create_dir_all(root).unwrap();
    let config = remanence_state::parse_config_toml(&format!(
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
        root.display()
    ))
    .unwrap();
    let paths = StatePaths::from_config(root.join("config.toml"), &config);
    let mut state = StateHandle::open_with_config(paths, config).unwrap();
    provision(state.catalog_index());
    // ModelTransport's INQUIRY is LTO-9. Record an operator observation so
    // mount/write tests do not depend on label inference or model density 0.
    rusqlite::Connection::open(&state.paths().sqlite_path)
        .unwrap()
        .execute(
            "UPDATE tapes SET cartridge_generation = 'L9', generation_source = 'operator'
         WHERE tape_uuid = ?1",
            [UUID.as_slice()],
        )
        .unwrap();
    state
        .catalog_index()
        .project_tape_pool_membership(UUID, "default")
        .unwrap();
    state
}

fn request(root: &Path) -> WriteObjectToPoolRequest {
    let path = root.join("payload.txt");
    fs::write(&path, b"standalone oracle payload\n").unwrap();
    WriteObjectToPoolRequest {
        pool_id: "default".into(),
        source: WriteObjectSource::Path(path),
        archive_path: "payload.txt".into(),
        caller_object_id: "P1-oracle".into(),
        expected_content_sha256: None,
        expected_object_id: None,
        input_kind: WriteObjectInputKind::LogicalFile,
        representation: PoolWriteRepresentation::Plaintext,
    }
}

// A one-shot transport fault, injected before the inner command can mutate media.
// Arming it after proof avoids confusing an open/readiness failure with a stale proof.
#[derive(Clone, Copy)]
struct Fault {
    opcode: u8,
    key: u8,
    asc: u8,
    ascq: u8,
    completion_unknown: bool,
}
type ArmedFault = Arc<Mutex<Option<Fault>>>;
struct FaultTransport {
    inner: ModelTransport,
    armed: ArmedFault,
    hide_serial: bool,
    fence_observer: Option<(PathBuf, u64, Arc<Mutex<usize>>)>,
    session_observer: Option<PathBuf>,
}

fn modifies_media(opcode: u8) -> bool {
    matches!(opcode, 0x04 | 0x0a | 0x8a | 0x10 | 0x80 | 0x19 | 0x93)
}

fn assert_no_media_dispatch(log: &RecordingLog) {
    let log = log.borrow();
    assert!(log.iter().all(|cdb| !modifies_media(cdb[0])), "{log:?}");
}
impl FaultTransport {
    fn check(&self, cdb: &[u8]) -> Result<(), ScsiError> {
        if modifies_media(cdb[0]) {
            if let Some(db_path) = &self.session_observer {
                // A separate connection must see a committed, recoverable write
                // session for this tape and C2's decoded standalone DriveKey.
                // RestartSession.drive_key exposes the migrated endpoint without
                // prescribing its SQLite encoding in this oracle.
                let index = CatalogIndex::open(db_path).unwrap();
                assert!(
                    index
                        .non_terminal_sessions()
                        .unwrap()
                        .iter()
                        .any(|session| {
                            session.session_kind == "write"
                                && session.tape_uuid.as_deref() == Some(UUID.as_slice())
                                && matches!(
                                    &session.drive_key,
                                    Some(remanence_library::DriveKey::Standalone { drive_serial })
                                        if drive_serial == SERIAL
                                )
                        }),
                    "durable standalone write session must precede media dispatch opcode {:02x}",
                    cdb[0]
                );
            }
            if let Some((dir, before, observed)) = &self.fence_observer {
                // Reopen from disk, not a clone of the in-memory fold. This runs
                // before the model sees even the FIRST modifying command.
                let persisted = remanence_state::CalibrationControlStore::open(dir).unwrap();
                assert!(
                    persisted.row(UUID).write_epoch > *before,
                    "durable fence must precede media dispatch opcode {:02x}",
                    cdb[0]
                );
                *observed.lock().unwrap() += 1;
            }
        }
        let mut armed = self.armed.lock().unwrap();
        if armed.as_ref().is_some_and(|f| f.opcode == cdb[0]) {
            let fault = armed.take().unwrap();
            if fault.completion_unknown {
                return Err(ScsiError::TransportError {
                    status: 0,
                    host_status: 3,
                    driver_status: 0,
                    info: 1,
                    sense: Vec::new(),
                }); // DID_TIME_OUT: completion is unknown, not CHECK CONDITION.
            }
            let mut sense = vec![0; 18];
            sense[0] = 0x70;
            sense[2] = fault.key;
            sense[7] = 10;
            sense[12] = fault.asc;
            sense[13] = fault.ascq;
            return Err(ScsiError::CheckCondition {
                sense,
                bytes_transferred: 0,
            });
        }
        Ok(())
    }
}
impl SgTransport for FaultTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.check(cdb)?;
        if self.hide_serial && cdb[0] == 0x12 && cdb[1] & 1 != 0 && cdb[2] == 0x80 {
            return Ok(TransferOutcome::clean(0));
        }
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.check(cdb)?;
        if cdb.first() == Some(&0x1e) {
            // The existing model does not implement PREVENT/ALLOW. Use the
            // fixture's no-data completion; RecordingTransport still observes
            // the real CDB and its position relative to READ. No tape write is
            // synthesized here, and the swap fault deliberately bypasses a hand lock.
            assert_eq!(cdb.len(), 6);
            assert!(cdb[4] <= 1);
            return FixtureTransport::new().execute_none(cdb);
        }
        self.inner.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.check(cdb)?;
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

fn prove(drive: &mut DriveHandle) -> u64 {
    drive.test_unit_ready().unwrap();
    drive.prevent_medium_removal().unwrap();
    drive.rewind().unwrap();
    // Reset UA invalidates fixed-block mode as well as identity.
    drive.read_config().unwrap();
    verify_tape_identity(&mut DriveHandleSource(drive), &UUID).unwrap();
    drive.locate(2).unwrap();
    drive.identity_generation()
}

#[test]
fn c3_density_mode_sense_and_generation_mapping() {
    // IBM LTO SCSI Reference GA32-0928-05 (2022-02-28), p. 362,
    // block descriptor density table (PDF page 392):
    // https://www.ibm.com/support/pages/system/files/inline-files/LTO%20SCSI%20Reference_GA32-0928-05%20%28EXTERNAL%29_0.pdf
    for (density, expected) in [
        (0x58, Some(LtoGen::Lto5)),
        (0x5a, Some(LtoGen::Lto6)),
        (0x5c, Some(LtoGen::Lto7)),
        (0x5d, Some(LtoGen::M8)),
        (0x5e, Some(LtoGen::Lto8)),
        (0x60, Some(LtoGen::Lto9)),
        (0x00, None),
        (0x7f, None),
    ] {
        let mut mode = vec![0; 28];
        mode[0] = 27;
        mode[1] = 0x68; // Rewritable medium type, independent of density.
        mode[3] = 8;
        mode[4] = density;
        mode[9..12].copy_from_slice(&[4, 0, 0]);
        mode[12] = 0x0f;
        mode[13] = 14;
        let fixture = FixtureTransport::new().with_responses([
            include_bytes!("../../../fixtures/standalone/drive-inquiry.bin").to_vec(),
            include_bytes!("../../../fixtures/standalone/drive-vpd80.bin").to_vec(),
            vec![0, 0xff, 0xff, 0xff, 0, 1],
            mode,
        ]);
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new(DEVICE),
            Box::new(fixture),
            &StaticAllowlist::new([DEVICE]),
        )
        .unwrap();
        assert_eq!(drive.read_config().unwrap().density_code, Some(density));
        assert_eq!(lto_generation_from_density(density), expected);
    }
}

#[test]
fn c3_identity_generation_invalidates_old_proof_and_allows_reproof() {
    // A non-readiness command must also invalidate on *any* UA, not just reset.
    for fault in [
        Fault {
            opcode: 0x4d,
            key: 6,
            asc: 0x28,
            ascq: 0,
            completion_unknown: false,
        },
        Fault {
            opcode: 0x4d,
            key: 6,
            asc: 0x29,
            ascq: 0,
            completion_unknown: false,
        },
        Fault {
            opcode: 0x4d,
            key: 6,
            asc: 0x2a,
            ascq: 1,
            completion_unknown: false,
        },
        Fault {
            opcode: 0x00,
            key: 2,
            asc: 0x04,
            ascq: 1,
            completion_unknown: false,
        },
        Fault {
            opcode: 0x4d,
            key: 0,
            asc: 0,
            ascq: 0,
            completion_unknown: true,
        },
    ] {
        let world = world(tape(UUID));
        let armed = Arc::new(Mutex::new(None));
        let mut transport = transport(&world);
        transport.armed = Arc::clone(&armed);
        let mut drive = DriveHandle::open_standalone_with_transport(
            Path::new(DEVICE),
            Box::new(transport),
            &policy(),
        )
        .unwrap();
        drive.install_media_write_fence(Box::new(
            remanence_library::InMemoryCalibrationControl::new(),
        ));
        let old = prove(&mut drive);
        let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
        *armed.lock().unwrap() = Some(fault);
        if fault.opcode == 0x00 {
            assert!(drive.test_unit_ready().is_err());
        } else {
            assert!(drive.read_tape_alerts().is_err());
        }
        assert!(
            armed.lock().unwrap().is_none(),
            "fault was actually consumed"
        );
        assert!(drive.identity_generation() > old);
        let block = vec![0xa5; BLOCK as usize];
        let result = drive.write_block_with_identity_generation(&block, old);
        assert!(
            matches!(result, Err(TapeIoError::IdentityInvalidated { .. })),
            "{result:?}"
        );
        assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
        assert!(world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .all(|command| !modifies_media(command.opcode)));
        let current = prove(&mut drive);
        assert!(current > old);
        drive
            .write_block_with_identity_generation(&block, current)
            .unwrap();
        assert_eq!(
            world.lock().unwrap().tapes["ORACLEL9"].records.last(),
            Some(&Record::Block(block))
        );
    }
}

fn assert_prevent_before_read_allow_after(log: &RecordingLog) {
    let log = log.borrow();
    let prevent = log
        .iter()
        .position(|c| c[0] == 0x1e && c[4] & 1 == 1)
        .expect("PREVENT");
    let read = log
        .iter()
        .position(|c| matches!(c[0], 0x08 | 0x88))
        .expect("BOT READ");
    let allow = log
        .iter()
        .rposition(|c| c[0] == 0x1e && c[4] & 1 == 0)
        .expect("ALLOW");
    assert!(prevent < read && read < allow, "{log:?}");
}

#[test]
fn c4_operator_mount_identity_blank_release_and_swap_before_write() {
    let temp = tempfile::tempdir().unwrap();
    let lock_root = temp.path().join("locks");
    let mut state = state(&temp.path().join("state"));
    let world = world(tape(UUID));
    let original = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    let (transport, log) = RecordingTransport::new(transport(&world));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(transport),
        &mut state,
        &lock_root,
    )
    .unwrap();
    let mounted = backend.mount(target(Some(UUID))).unwrap();
    assert_eq!(mounted.tape_uuid, UUID);
    assert!(mounted.writable());
    assert!(
        matches!(&mounted.endpoint, MountEndpoint::Durable(remanence_library::DriveKey::Standalone { drive_serial }) if drive_serial == SERIAL)
    );
    assert_eq!(
        mounted.identity_generation,
        mounted.drive.identity_generation()
    );
    backend.release(mounted);
    assert_prevent_before_read_allow_after(&log);
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, original);

    let (transport, _) = RecordingTransport::new(self::transport(&world));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(transport),
        &mut state,
        &lock_root,
    )
    .unwrap();
    let error = match backend.mount(target(Some(OTHER))) {
        Ok(_) => panic!("wrong cartridge must be refused"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains(&uuid::Uuid::from_bytes(UUID).to_string()),
        "{error}"
    );
    assert!(
        error.contains(&uuid::Uuid::from_bytes(OTHER).to_string()),
        "{error}"
    );
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, original);
    drop(backend);

    let blank_world = self::world(VirtualTape::empty(128 * 1024 * 1024, BLOCK));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(self::transport(&blank_world)),
        &mut state,
        &lock_root,
    )
    .unwrap();
    assert!(matches!(
        backend.mount(target(None)),
        Err(MountError::Blank { .. })
    ));
    assert!(blank_world.lock().unwrap().tapes["ORACLEL9"]
        .records
        .is_empty());
    assert_eq!(
        blank_world.lock().unwrap().tapes["ORACLEL9"].written_bytes,
        0
    );
    drop(backend);

    let armed = Arc::new(Mutex::new(None));
    let mut injected = self::transport(&world);
    injected.armed = Arc::clone(&armed);
    let mut actor = MountedDrive::with_transport(
        state,
        Path::new(DEVICE),
        &policy(),
        Box::new(injected),
        &lock_root,
        Some(UUID),
    )
    .unwrap();
    let old_tape = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    world
        .lock()
        .unwrap()
        .put_tape_in_drive(BAY, "SWAPPEDL9", None, tape(OTHER));
    let other_before = world.lock().unwrap().tapes["SWAPPEDL9"].records.clone();
    // Report the medium change on the first command after the swap. The adapter
    // must encounter it before dispatching an Object WRITE, even if it re-proves.
    *armed.lock().unwrap() = Some(Fault {
        opcode: 0x01,
        key: 6,
        asc: 0x28,
        ascq: 0,
        completion_unknown: false,
    });
    assert!(actor.write_checkpointed(request(temp.path())).is_err());
    assert!(
        armed.lock().unwrap().is_none(),
        "the medium-change UA was delivered"
    );
    let w = world.lock().unwrap();
    assert_eq!(w.tapes["ORACLEL9"].records, old_tape);
    assert_eq!(w.tapes["SWAPPEDL9"].records, other_before);
    assert_eq!(w.tapes["SWAPPEDL9"].written_bytes, u64::from(BLOCK));
    drop(w);
    let _ = actor.finish();
}

#[test]
fn c4_device_lock_is_host_wide_and_held_until_release() {
    let temp = tempfile::tempdir().unwrap();
    let lock_root = temp.path().join("locks");
    assert_eq!(
        OperatorMount::device_lock_path(SERIAL),
        Path::new("/run/lock/rem").join(format!("{SERIAL}.lock"))
    );
    let mut first_state = state(&temp.path().join("one"));
    let mut second_state = state(&temp.path().join("two"));
    let world = world(tape(UUID));
    let mut first = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(transport(&world)),
        &mut first_state,
        &lock_root,
    )
    .unwrap();
    let held = first.mount(target(Some(UUID))).unwrap();
    assert!(lock_root.join(format!("{SERIAL}.lock")).exists());
    let second = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(transport(&world)),
        &mut second_state,
        &lock_root,
    );
    let error = match second {
        Err(error) => error.to_string(),
        Ok(mut backend) => match backend.mount(target(Some(UUID))) {
            Ok(_) => panic!("same serial in another state directory must be locked"),
            Err(error) => error.to_string(),
        },
    };
    assert!(
        error.contains(SERIAL) && error.to_lowercase().contains("lock"),
        "{error}"
    );
    first.release(held);
    let mut second = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(transport(&world)),
        &mut second_state,
        &lock_root,
    )
    .unwrap();
    let mounted = second
        .mount(target(Some(UUID)))
        .expect("release relinquishes host-wide lock");
    second.release(mounted);
}

#[test]
fn c4_lock_failures_refuse_at_production_root_without_fallback() {
    for failure in ["create_dir_all", "lock_exclusive"] {
        let temp = tempfile::tempdir().unwrap();
        let mut state = state(&temp.path().join("state"));
        let world = world(tape(UUID));
        let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
        let (transport, log) = RecordingTransport::new(transport(&world));
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&attempts);
        let sandbox = temp.path().join("lock-fs");
        // Proposed IO seam: the NORMAL production root-selection/acquisition
        // path calls this before EVERY filesystem operation (including retries
        // or fallback attempts). Ok maps only the physical IO path into /tmp;
        // Err is propagated exactly like a filesystem failure. It must not
        // replace locking policy or the acquisition result.
        let hook = move |operation: &str, logical: &Path| -> std::io::Result<PathBuf> {
            recorded
                .lock()
                .unwrap()
                .push((operation.to_owned(), logical.to_path_buf()));
            assert!(
                logical.starts_with("/run/lock/rem"),
                "fallback attempted: {logical:?}"
            );
            if operation == failure {
                return Err(std::io::Error::from_raw_os_error(
                    if failure == "create_dir_all" { 13 } else { 11 },
                ));
            }
            Ok(sandbox.join(logical.strip_prefix("/run/lock/rem").unwrap()))
        };
        let attempt = OperatorMount::with_transport_and_lock_io_for_test(
            Path::new(DEVICE),
            &policy(),
            Box::new(transport),
            &mut state,
            Box::new(hook),
        );
        let error = match attempt {
            Err(error) => error.to_string(),
            Ok(mut backend) => match backend.mount(target(Some(UUID))) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("lock failure must refuse, never acquire a fallback lock"),
            },
        };
        let attempts = attempts.lock().unwrap();
        assert!(
            attempts.iter().any(|(op, _)| op == failure),
            "fault not reached: {attempts:?}"
        );
        assert_eq!(
            attempts.last().unwrap().0,
            failure,
            "no acquisition after failure"
        );
        assert!(error.contains("/run/lock/rem"), "{error}");
        let error = error.to_lowercase();
        let fixes: &[&str] = if failure == "create_dir_all" {
            &["ensure", "grant", "check", "chmod", "chown", "mkdir", "fix"]
        } else {
            &[
                "release",
                "retry",
                "other process",
                "another process",
                "stop",
            ]
        };
        assert!(
            fixes.iter().any(|fix| error.contains(fix)),
            "must name remediation: {error}"
        );
        assert_no_media_dispatch(&log);
        assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
        assert_eq!(
            world.lock().unwrap().tapes["ORACLEL9"].written_bytes,
            u64::from(BLOCK)
        );
    }
}

#[test]
fn c4_ephemeral_mount_is_read_only_and_mints_no_write_authority() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp.path().join("state"));
    let db = rusqlite::Connection::open(&state.paths().sqlite_path).unwrap();
    let journal_dir = state.paths().journal_dir.clone();
    let journal_before = authority_files(&journal_dir);
    let sessions_before: i64 = db
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    let calibration_dir = state.paths().calibration_dir.clone();
    let epoch = state.calibration_control().row(UUID).write_epoch;
    let world = world(tape(UUID));
    let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    let mut injected = transport(&world);
    injected.hide_serial = true;
    let (injected, log) = RecordingTransport::new(injected);
    let policy = policy().with_unserialized_device_allowed(Path::new(DEVICE));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy,
        Box::new(injected),
        &mut state,
        &temp.path().join("locks"),
    )
    .unwrap();
    let mounted = backend
        .mount(target(Some(UUID)))
        .expect("otherwise valid read-only mount");
    assert!(matches!(mounted.endpoint, MountEndpoint::Ephemeral { .. }));
    assert!(!mounted.writable());
    backend.release(mounted);
    let mut injected = transport(&world);
    injected.hide_serial = true;
    let (injected, write_log) = RecordingTransport::new(injected);
    let error = MountedDrive::with_transport(
        state,
        Path::new(DEVICE),
        &policy,
        Box::new(injected),
        &temp.path().join("locks"),
        Some(UUID),
    )
    .err()
    .expect("ephemeral endpoint cannot own a writer")
    .to_string();
    assert_no_media_dispatch(&write_log);
    assert!(
        ["ephemeral", "read-only", "serial"]
            .iter()
            .any(|word| error.to_lowercase().contains(word)),
        "{error}"
    );
    assert_no_media_dispatch(&log);
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
    assert_eq!(
        world.lock().unwrap().tapes["ORACLEL9"].written_bytes,
        u64::from(BLOCK)
    );
    assert_eq!(
        remanence_state::CalibrationControlStore::open(calibration_dir)
            .unwrap()
            .row(UUID)
            .write_epoch,
        epoch
    );
    // A read session is permitted; a durable write session is not.
    let write_sessions: i64 = db
        .query_row(
            "SELECT count(*) FROM sessions WHERE session_kind = 'write'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(write_sessions, 0);
    assert_eq!(sessions_before, 0);
    let objects: i64 = db
        .query_row("SELECT count(*) FROM object_copies", [], |r| r.get(0))
        .unwrap();
    assert_eq!(objects, 0, "no durable object write authority");
    assert_eq!(
        authority_files(&journal_dir),
        journal_before,
        "no checkpoint/parity journal authority"
    );
}

#[test]
fn c5_calibration_fence_failure_prevents_media_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let mut state = state(&temp.path().join("state"));
    let dir = state.paths().calibration_dir.clone();
    let control = state.calibration_control().clone();
    let epoch = control.row(UUID).write_epoch;
    let world = world(tape(UUID));
    let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    let (injected, log) = RecordingTransport::new(transport(&world));
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &policy(),
        Box::new(injected),
        &mut state,
        &temp.path().join("locks"),
    )
    .unwrap();
    let mounted = backend.mount(target(Some(UUID))).unwrap();
    // EISDIR on journal append is deterministic even under root. Inject after
    // successful mount so readiness/identity cannot mask the fence refusal.
    let journal = dir.join(remanence_state::calibration::CALIBRATION_CONTROL_FILENAME);
    if journal.exists() {
        fs::rename(&journal, dir.join("before-fault")).unwrap();
    }
    fs::create_dir(&journal).unwrap();
    backend.release(mounted);
    let (injected, write_log) = RecordingTransport::new(transport(&world));
    let error = MountedDrive::with_transport(
        state,
        Path::new(DEVICE),
        &policy(),
        Box::new(injected),
        &temp.path().join("locks"),
        Some(UUID),
    )
    .err()
    .expect("failed calibration fence refuses actor construction")
    .to_string();
    assert_no_media_dispatch(&write_log);
    assert!(
        error.contains("calibration") || error.contains("fence"),
        "{error}"
    );
    assert_no_media_dispatch(&log);
    assert_eq!(control.row(UUID).write_epoch, epoch);
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
    assert_eq!(
        world.lock().unwrap().tapes["ORACLEL9"].written_bytes,
        u64::from(BLOCK)
    );
}

/// The installed fence must still refuse a write when its durable store fails
/// after the owned adapter has mounted successfully in the same invocation.
#[test]
fn c5_calibration_fence_failure_after_mount_prevents_media_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp.path().join("state"));
    let dir = state.paths().calibration_dir.clone();
    let control = state.calibration_control().clone();
    let epoch = control.row(UUID).write_epoch;
    let world = world(tape(UUID));
    let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    let (injected, log) = RecordingTransport::new(transport(&world));
    let mut actor = MountedDrive::with_transport(
        state,
        Path::new(DEVICE),
        &policy(),
        Box::new(injected),
        &temp.path().join("locks"),
        Some(UUID),
    )
    .expect("mount and install the calibration fence before injecting the fault");
    let journal = dir.join(remanence_state::calibration::CALIBRATION_CONTROL_FILENAME);
    fs::rename(&journal, dir.join("before-fault")).unwrap();
    fs::create_dir(&journal).unwrap();
    let error = actor
        .write_checkpointed(request(temp.path()))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("calibration") || error.contains("fence"),
        "{error}"
    );
    assert_no_media_dispatch(&log);
    assert_eq!(control.row(UUID).write_epoch, epoch);
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
    assert_eq!(
        world.lock().unwrap().tapes["ORACLEL9"].written_bytes,
        u64::from(BLOCK)
    );
    let _ = actor.finish();
    assert_no_media_dispatch(&log);
}

#[test]
fn c2_null_generation_refuses_writability_even_with_lto_label() {
    let mut index = CatalogIndex::open(":memory:").unwrap();
    provision(&mut index);
    let mut tape = index.get_tape(&UUID).unwrap().unwrap();
    tape.cartridge_generation = None;
    tape.generation_source = None;
    let error = check_writability_preconditions(&tape, 1)
        .expect_err("label cannot replace recorded generation");
    assert!(error.to_string().contains("generation"), "{error}");
    tape.cartridge_generation = Some("L7".into());
    tape.generation_source = Some("operator".into());
    check_writability_preconditions(&tape, 1).expect("setting generation removes this refusal");
}

#[test]
fn c5_write_adapter_sessions_fence_audit_and_stale_fence_refusal() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp.path().join("state"));
    let db_path = state.paths().sqlite_path.clone();
    let audit_dir = state.paths().audit_dir.clone();
    let world = world(tape(UUID));
    let before = state.calibration_control().row(UUID).write_epoch;
    let observed = Arc::new(Mutex::new(0usize));
    let mut observed_transport = transport(&world);
    observed_transport.session_observer = Some(db_path.clone());
    observed_transport.fence_observer = Some((
        state.paths().calibration_dir.clone(),
        before,
        Arc::clone(&observed),
    ));
    let config_before = state.config().clone();
    let paths_before = state.paths().clone();
    let mut actor = MountedDrive::with_transport(
        state,
        Path::new(DEVICE),
        &policy(),
        Box::new(observed_transport),
        &temp.path().join("locks"),
        Some(UUID),
    )
    .unwrap();
    actor.write_checkpointed(request(temp.path())).unwrap();
    actor.finish().unwrap();
    let mut state = StateHandle::open_with_config(paths_before, config_before).unwrap();
    assert!(*observed.lock().unwrap() > 0, "observer saw media dispatch");
    assert!(world.lock().unwrap().tapes["ORACLEL9"].written_bytes > u64::from(BLOCK));
    assert!(
        state.calibration_control().row(UUID).write_epoch > before,
        "durable calibration fence advanced"
    );
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let sessions: i64 = db
        .query_row(
            "SELECT count(*) FROM sessions WHERE tape_uuid = ?1 AND session_kind = 'write'",
            [UUID.as_slice()],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        sessions > 0,
        "adapter persists a write session for the mounted tape"
    );
    let records = FileAuditLog::replay(&audit_dir).unwrap();
    assert!(
        records
            .iter()
            .any(|record| record.subject.kind == "drive"
                && record.subject.id.as_deref() == Some(SERIAL)),
        "a drive audit subject must name the VPD serial: {records:?}"
    );
    state
        .catalog_index()
        .record_tape_io_fence(TapeIoFenceInput {
            tape_uuid: UUID,
            barcode: Some("ORACLEL9".into()),
            reason: "oracle-stale-active".into(),
            evidence_json: None,
        })
        .unwrap();
    let config = state.config().clone();
    let paths = state.paths().clone();
    drop(state);
    // Reopen without a startup helper that could silently clear the stale fence.
    let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    let error = match StateHandle::open_with_config(paths, config) {
        Err(error) => error.to_string(),
        Ok(mut reopened) => {
            let attempt = OperatorMount::with_transport_for_test(
                Path::new(DEVICE),
                &policy(),
                Box::new(transport(&world)),
                &mut reopened,
                &temp.path().join("locks"),
            );
            // Contract amendment 3 (C5, standalone fences): reads run under an
            // active fence so the operator can rescue data; writes to the fenced
            // tape are refused before any media dispatch.
            let mut backend = attempt.expect("a stale fence must not refuse the read-capable open");
            let mounted = backend
                .mount(target(Some(UUID)))
                .expect("a stale fence must not refuse a mount for reading");
            backend.release(mounted);
            let paths = reopened.paths().clone();
            let config = reopened.config().clone();
            let error = match MountedDrive::with_transport(
                reopened,
                Path::new(DEVICE),
                &policy(),
                Box::new(transport(&world)),
                &temp.path().join("locks"),
                Some(UUID),
            ) {
                Err(error) => error.to_string(),
                Ok(mut actor) => {
                    let error = actor
                        .put_path_version_with_policy(request(temp.path()), false)
                        .unwrap_err()
                        .to_string();
                    let _ = actor.finish();
                    error
                }
            };
            let mut reopened = StateHandle::open_with_config(paths, config).unwrap();
            assert!(
                !reopened
                    .catalog_index()
                    .list_active_tape_io_fences()
                    .unwrap()
                    .is_empty(),
                "the refused write must leave the stale fence active"
            );
            error
        }
    };
    assert!(error.to_lowercase().contains("fence"), "{error}");
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
}

// Build valid objects with production format code. Only hostile member fixtures
// need byte mutation: the public writer correctly rejects those paths at input.
fn object_blocks(object_id: &str, member: &str, hostile: bool) -> Vec<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let placeholder = "x".repeat(member.len());
    let path = if hostile {
        placeholder.as_str()
    } else {
        member
    };
    let mut options =
        remanence_format::RemTarObjectOptions::new(object_id, "oracle", AT, "manifest-1");
    options.chunk_size = BLOCK as usize;
    let mut sink = VecBlockSink::new();
    let layout = remanence_format::write_rem_tar_object(
        &mut sink,
        &options,
        &[remanence_format::RemTarFile {
            path,
            file_id: "file-1",
            data: b"recovered bytes",
            mtime: None,
            executable: None,
        }],
    )
    .unwrap();
    if !hostile {
        return sink.blocks;
    }
    // Equal lengths retain CBOR/PAX lengths and tape-block alignment. Update
    // every path occurrence, the manifest digest, and tar header checksums.
    fn replace(bytes: &mut [u8], from: &[u8], to: &[u8]) -> usize {
        assert_eq!(from.len(), to.len());
        let positions: Vec<_> = bytes
            .windows(from.len())
            .enumerate()
            .filter_map(|(i, chunk)| (chunk == from).then_some(i))
            .collect();
        for &i in &positions {
            bytes[i..i + to.len()].copy_from_slice(to);
        }
        positions.len()
    }
    let mut manifest = layout.manifest_cbor.clone();
    assert_eq!(
        replace(&mut manifest, path.as_bytes(), member.as_bytes()),
        1
    );
    let hex = |bytes: &[u8]| -> String {
        use std::fmt::Write;
        bytes.iter().fold(String::new(), |mut out, byte| {
            write!(&mut out, "{byte:02x}").unwrap();
            out
        })
    };
    let old_digest = hex(&Sha256::digest(&layout.manifest_cbor));
    let new_digest = hex(&Sha256::digest(&manifest));
    let mut bytes = sink.blocks.concat();
    assert!(replace(&mut bytes, path.as_bytes(), member.as_bytes()) >= 2);
    assert_eq!(
        replace(&mut bytes, old_digest.as_bytes(), new_digest.as_bytes()),
        1
    );
    for header in bytes.chunks_exact_mut(512) {
        if &header[257..263] == b"ustar\0" {
            header[148..156].fill(b' ');
            let checksum: u32 = header.iter().map(|b| u32::from(*b)).sum();
            header[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
        }
    }
    let blocks: Vec<_> = bytes
        .chunks_exact(BLOCK as usize)
        .map(<[u8]>::to_vec)
        .collect();
    // Reject on member syntax, not a broken fixture checksum or digest.
    let error = remanence_format::read_rem_tar_object(
        &mut remanence_library::VecBlockSource::new(blocks.clone()),
        BLOCK as usize,
        blocks.len() as u64,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("path") && (error.contains("relative") || error.contains("canonical")),
        "{error}"
    );
    blocks
}

fn restore_world(objects: &[(&str, &str, bool)]) -> SharedVirtualWorld {
    let mut tape = tape(UUID);
    for &(id, member, hostile) in objects {
        let blocks = object_blocks(id, member, hostile);
        tape.written_bytes += blocks.iter().map(|b| b.len() as u64).sum::<u64>();
        tape.records.extend(blocks.into_iter().map(Record::Block));
        tape.records.push(Record::Filemark);
    }
    world(tape)
}

fn restore_drive(world: &SharedVirtualWorld) -> DriveHandle {
    DriveHandle::open_standalone_with_transport(
        Path::new(DEVICE),
        Box::new(transport(world)),
        &policy(),
    )
    .unwrap()
}

fn authority_files(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(authority_files(&path));
        } else {
            files.push((path.clone(), fs::read(path).unwrap()));
        }
    }
    files.sort();
    files
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

// C5 permits a destination-level report alongside the ordinal candidates.
fn candidate_names(dest: &Path) -> Vec<String> {
    names(dest)
        .into_iter()
        .filter(|name| {
            if name == "report.json" {
                assert!(fs::symlink_metadata(dest.join(name)).unwrap().is_file());
                false
            } else {
                true
            }
        })
        .collect()
}

#[test]
fn c5_scan_restore_uses_ordinal_destinations_for_every_object_id() {
    let temp = tempfile::tempdir().unwrap();
    let dest = temp.path().join("restore");
    let sentinel = temp.path().join("outside-sentinel");
    fs::write(&sentinel, b"unchanged").unwrap();
    let absolute_id = temp.path().join("absolute-object");
    let ids = [
        "ordinary-object",
        "../escape",
        absolute_id.to_str().unwrap(),
        "ordinary-object",
    ];
    let objects: Vec<_> = ids.iter().map(|id| (*id, "payload.txt", false)).collect();
    let world = restore_world(&objects);
    let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    let report = scan_and_restore(&mut restore_drive(&world), &dest, None)
        .expect("all valid candidates restored");
    let report = serde_json::to_string(&report).unwrap();
    for (i, id) in ids.iter().enumerate() {
        let candidate = dest.join(format!("candidate-{}", i + 1));
        assert!(candidate
            .canonicalize()
            .unwrap()
            .starts_with(dest.canonicalize().unwrap()));
        assert_eq!(
            fs::read(candidate.join("payload.txt")).unwrap(),
            b"recovered bytes"
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(candidate.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["object_id"], *id);
        assert!(report.contains(id), "{report}");
    }
    assert_eq!(
        candidate_names(&dest),
        ["candidate-1", "candidate-2", "candidate-3", "candidate-4"]
    );
    assert_eq!(names(temp.path()), ["outside-sentinel", "restore"]);
    assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
}

#[test]
fn c5_scan_restore_contains_hostile_members_and_destination_symlinks() {
    for case in [
        "parent",
        "nested-parent",
        "absolute",
        "candidate-symlink",
        "member-symlink",
        "file-symlink",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("restore");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let sentinel = outside.join("payload.txt");
        fs::write(&sentinel, b"unchanged").unwrap();
        fs::create_dir_all(&dest).unwrap();
        let absolute = sentinel.to_str().unwrap();
        let member = match case {
            "parent" => "../../outside/payload.txt",
            "nested-parent" => "nested/../../../outside/payload.txt",
            "absolute" => absolute,
            "member-symlink" => "linked/payload.txt",
            _ => "payload.txt",
        };
        let candidate = dest.join("candidate-1");
        match case {
            "candidate-symlink" => std::os::unix::fs::symlink(&outside, &candidate).unwrap(),
            "member-symlink" => {
                fs::create_dir(&candidate).unwrap();
                std::os::unix::fs::symlink(&outside, candidate.join("linked")).unwrap();
            }
            "file-symlink" => {
                fs::create_dir(&candidate).unwrap();
                std::os::unix::fs::symlink(&sentinel, candidate.join("payload.txt")).unwrap();
            }
            _ => {}
        }
        let hostile = matches!(case, "parent" | "nested-parent" | "absolute");
        let world = restore_world(&[("ordinary-object", member, hostile)]);
        let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
        // Whole-scan or per-candidate refusal is acceptable. A symlink can
        // also be safely replaced, provided extraction stays inside dest.
        let outcome = match scan_and_restore(&mut restore_drive(&world), &dest, None) {
            Ok(report) => serde_json::to_string(&report).unwrap(),
            Err(error) => error.to_string(),
        }
        .to_lowercase();
        let output = candidate.join(member);
        let contained_restore = !hostile
            && output
                .canonicalize()
                .is_ok_and(|path| path.starts_with(dest.canonicalize().unwrap()))
            && fs::read(&output).unwrap() == b"recovered bytes";
        if !contained_restore {
            assert!(
                [
                    "path",
                    "contain",
                    "symlink",
                    "relative",
                    "canonical",
                    "exist"
                ]
                .iter()
                .any(|word| outcome.contains(word)),
                "{case}: {outcome}"
            );
        }
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged", "{case}");
        assert_eq!(names(&outside), ["payload.txt"], "{case}");
        assert_eq!(names(temp.path()), ["outside", "restore"], "{case}");
        assert!(
            candidate_names(&dest)
                .iter()
                .all(|name| name == "candidate-1"),
            "{case}"
        );
        assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
    }
}
