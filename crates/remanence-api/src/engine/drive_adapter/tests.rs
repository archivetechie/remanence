//! Virtual transport proofs for mounted ownership, durability and recovery.
use super::*;
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{transport::TransferOutcome, ScsiError, TimeoutClass};
use remanence_state::{authority::TapeAuthorityView, AuditEvent, CatalogIndex, FileAuditLog};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
const UUID: [u8; 16] = [0x73; 16];
const BLOCK: u32 = 262144;
const BAY: u16 = 0x100;
const SERIAL: &str = "STEP1-MOUNTED";
// This is only passed to the injected transport, never opened as a device.
const DEVICE: &str = "/virtual/step1-drive";

struct Transport {
    inner: ModelTransport,
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    fault: Arc<Mutex<Option<u8>>>,
    evidence: Option<PathBuf>,
    last_opcode: Option<u8>,
    bootstrap_step: u8,
    empty_prefix_recovery: Option<EmptyPrefixRecoveryProof>,
}
/// Test observer for exactly ruling19's recovery of a missing Bootstrap
/// delimiter. One matching header-only ack permits one verified Bootstrap block
/// and one delimiter. It cannot authorize initialization or a later write run.
struct EmptyPrefixRecoveryProof {
    runtime: remanence_state::library_runtime::LibraryRuntime,
    step: u8,
    fail_position_after_delimiter: bool,
}
impl EmptyPrefixRecoveryProof {
    fn authorize(&mut self, cdb: &[u8], bytes: Option<&[u8]>) -> bool {
        use sha2::{Digest, Sha256};
        if !matches!(cdb[0], 0x0a | 0x10 | 0x19) || self.step == 2 {
            return false;
        }
        if self.step == 0 {
            assert_eq!(
                cdb[0], 0x0a,
                "recovery begins with its exact Bootstrap rewrite"
            );
            let bytes = bytes.expect("Bootstrap output bytes");
            assert_eq!(bytes.len(), BLOCK as usize);
            assert_eq!(
                remanence_parity::bootstrap::parse_bootstrap_block(bytes)
                    .unwrap()
                    .tape_uuid,
                UUID
            );
            let path = self
                .runtime
                .manifest()
                .tape_root()
                .join(format!("{}.remtape", Uuid::from_bytes(UUID).simple()));
            let raw = std::fs::read(path).unwrap();
            let mut reader = std::io::Cursor::new(raw.as_slice());
            remanence_state::tape_log::store::Header::decode(&mut reader, UUID).unwrap();
            assert_eq!(
                reader.position(),
                raw.len() as u64,
                "ack must name the current header-only source"
            );
            let at = remanence_state::library_log::TapeAt {
                tape: remanence_state::library_log::TapeRef {
                    uuid: UUID,
                    incarnation: 1,
                },
                sequence: 0,
                record_digest: Sha256::digest(&raw).into(),
            };
            let ack = self
                .runtime
                .supersede_at(at)
                .unwrap()
                .expect("matching applied recovery acknowledgement before WRITE");
            assert_eq!(ack.session_id, None);
            assert_eq!(ack.tail_file_count, 0);
            assert_eq!(
                ack.preview_digest, None,
                "header-only recovery has no native session tail spool"
            );
            self.step = 1;
        } else {
            assert_eq!(cdb[0], 0x10);
            assert_eq!(&cdb[2..5], &[0, 0, 1], "exactly one recovery delimiter");
            self.step = 2;
        }
        true
    }
}

/// Permit only the ordered, identity-checked precommit bootstrap sequence.
fn authorize_bootstrap(step: &mut u8, cdb: &[u8], bytes: Option<&[u8]>) -> bool {
    let permitted = match *step {
        0 => {
            cdb[0] == 0x0a
                && bytes.is_some_and(|bytes| {
                    remanence_parity::bootstrap::parse_bootstrap_block(bytes)
                        .is_ok_and(|bootstrap| bootstrap.tape_uuid == UUID)
                })
        }
        1 => cdb[0] == 0x10 && cdb[2..5] == [0, 0, 1],
        2 => cdb[0] == 0x10 && cdb[1] & 1 == 0 && cdb[2..5] == [0, 0, 0],
        _ => false,
    };
    if permitted {
        *step += 1;
    }
    permitted
}

impl Transport {
    fn observe(&mut self, cdb: &[u8], bytes: Option<&[u8]>) -> std::result::Result<(), ScsiError> {
        self.commands.lock().unwrap().push(cdb.to_vec());
        // Check each write run and every delimiter. Replaying the entire audit
        // per padding block makes the fixed two-GiB terminal layout quadratic.
        let previous = self.last_opcode.replace(cdb[0]);
        let recovery = self
            .empty_prefix_recovery
            .as_mut()
            .is_some_and(|proof| proof.authorize(cdb, bytes));
        if cdb[0] == 0x10
            && self
                .empty_prefix_recovery
                .as_ref()
                .is_some_and(|proof| proof.step == 2 && proof.fail_position_after_delimiter)
        {
            *self.fault.lock().unwrap() = Some(0x34);
        }
        if !recovery
            && matches!(cdb[0], 0x0a | 0x10 | 0x19)
            && (previous != Some(0x0a) || cdb[0] != 0x0a || matches!(self.bootstrap_step, 1 | 2))
        {
            if let Some(path) = &self.evidence {
                let native_path = path
                    .parent()
                    .unwrap()
                    .join("journals/tapes")
                    .join(format!("{}.remtape", Uuid::from_bytes(UUID).simple()));
                // Observe the actual durable native source without acquiring a
                // second tape owner while execute_out holds the writer lease.
                let mut native_empty = true;
                let native_authorized = match std::fs::File::open(&native_path) {
                    Ok(mut file) => {
                        remanence_state::tape_log::store::Header::decode(&mut file, UUID).unwrap();
                        let offset = std::io::Seek::stream_position(&mut file).unwrap();
                        native_empty = file.metadata().unwrap().len() == offset;
                        let mut decoder =
                            remanence_state::tape_log::codec::Decoder::at(file, 1, offset);
                        let mut fold = remanence_state::tape_log::fold::Fold::new();
                        while let Some(frame) = decoder.next_frame().unwrap() {
                            native_empty = false;
                            fold.push(&frame).unwrap();
                        }
                        let (summary, _) = fold.finish(decoder.tail()).unwrap();
                        summary.open_session.is_some()
                            || summary.finalization.as_ref().is_some_and(|v| !v.prepared)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                    Err(error) => panic!("read native session authority: {error}"),
                };
                let bootstrap_authorized =
                    native_empty && authorize_bootstrap(&mut self.bootstrap_step, cdb, bytes);
                if !native_authorized && !bootstrap_authorized {
                    let records = FileAuditLog::replay(path).unwrap();
                    let last = records
                        .iter()
                        .rev()
                        .find(|r| {
                            matches!(
                                r.event,
                                AuditEvent::SessionOpened
                                    | AuditEvent::SessionClosed
                                    | AuditEvent::SessionOrphaned
                            )
                        })
                        .expect("durable session before modifying CDB");
                    if last.event != AuditEvent::SessionOpened {
                        // A sealing session ends before its accepted terminal plan runs.
                        assert_eq!(last.event, AuditEvent::SessionClosed);
                        let native = path
                            .parent()
                            .unwrap()
                            .join("journals/tapes")
                            .join(format!("{}.remtape", Uuid::from_bytes(UUID).simple()));
                        let mut file = std::fs::File::open(native).unwrap();
                        remanence_state::tape_log::store::Header::decode(&mut file, UUID).unwrap();
                        let offset = std::io::Seek::stream_position(&mut file).unwrap();
                        let mut decoder =
                            remanence_state::tape_log::codec::Decoder::at(file, 1, offset);
                        let mut fold = remanence_state::tape_log::fold::Fold::new();
                        while let Some(frame) = decoder.next_frame().unwrap() {
                            fold.push(&frame).unwrap();
                        }
                        let (summary, _) = fold.finish(decoder.tail()).unwrap();
                        let closeout = records
                            .iter()
                            .rev()
                            .find(|r| {
                                r.detail.get("operation_kind")
                                    == Some(&ciborium::Value::Text("tape_closeout".into()))
                            })
                            .is_some_and(|r| r.event == AuditEvent::OperationStarted);
                        assert!(
                        summary.finalization.is_some() || (summary.closeout_due() && closeout),
                        "post-session writes require an accepted plan or a due closeout operation"
                    );
                        assert!(summary.open_session.is_none());
                    }
                    assert_eq!(
                        last.detail.get("endpoint_key"),
                        Some(&ciborium::value::Value::Text(
                            remanence_library::DriveKey::Standalone {
                                drive_serial: SERIAL.into()
                            }
                            .encode()
                        ))
                    );
                    assert!(!last.detail.contains_key("library_serial"));
                    assert!(!last.detail.contains_key("drive_bay"));
                }
            }
        }
        let mut fault = self.fault.lock().unwrap();
        if *fault == Some(cdb[0]) {
            *fault = None;
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
    fn execute_in(
        &mut self,
        cdb: &[u8],
        bytes: &mut [u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        self.observe(cdb, None)?;
        self.inner.execute_in(cdb, bytes)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        bytes: &[u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        self.observe(cdb, Some(bytes))?;
        self.inner.execute_out(cdb, bytes)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> std::result::Result<(), ScsiError> {
        self.observe(cdb, None)?;
        if matches!(cdb[0], 0x1e | 0x16 | 0x17) {
            return Ok(());
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}
struct Fixture {
    root: tempfile::TempDir,
    world: SharedVirtualWorld,
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    fault: Arc<Mutex<Option<u8>>>,
    config: remanence_state::RemConfig,
}
impl Fixture {
    fn new(parity: bool) -> (Self, StateHandle) {
        let parity = if parity {
            crate::ParityConfig::Scheme(remanence_parity::ParityScheme {
                data_blocks_per_stripe: 4,
                parity_blocks_per_stripe: 2,
                stripes_per_neighborhood: 2,
                id: remanence_parity::SchemeId::new_static("mounted"),
            })
        } else {
            crate::ParityConfig::None
        };
        Self::with_parity(parity)
    }
    /// This fixture's physical Bootstrap and delimiter are already complete.
    /// Publish that actual fact for fault tests which begin after initialization.
    fn initialized(parity: bool) -> (Self, StateHandle) {
        let (fixture, state) = Self::new(parity);
        fixture.publish_initialized(&state, parity);
        (fixture, state)
    }
    fn publish_initialized(&self, state: &StateHandle, parity: bool) {
        use remanence_state::authority::TapeAuthorityView;
        let fixture = self;
        let scheme = if parity {
            Some(remanence_parity::ParityScheme {
                data_blocks_per_stripe: 4,
                parity_blocks_per_stripe: 2,
                stripes_per_neighborhood: 2,
                id: remanence_parity::SchemeId::new_static("mounted"),
            })
        } else {
            None
        };
        let records = fixture.world.lock().unwrap().tapes["HAND"].records.clone();
        assert_eq!(records.len(), 2);
        assert!(matches!(records[1], Record::Filemark));
        let Record::Block(ref bytes) = records[0] else {
            panic!("physical Bootstrap")
        };
        assert_eq!(
            remanence_parity::bootstrap::parse_bootstrap_block(bytes)
                .unwrap()
                .tape_uuid,
            UUID
        );
        let authority = remanence_state::TapeAuthority::open(
            fixture.config.journal.dir.join("tapes"),
            UUID,
            &fixture.config.index.sqlite_path,
        )
        .unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .publish_initialized(
                BLOCK,
                scheme,
                remanence_parity::CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        state
            .library_runtime()
            .refresh_native(&owner.freeze().unwrap())
            .unwrap();
        state.library_runtime().catch_up().unwrap();
        drop(owner);
    }

    fn with_parity(parity: crate::ParityConfig) -> (Self, StateHandle) {
        let root = tempfile::tempdir().unwrap();
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
fsync = false
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
            root.path().display()
        ))
        .unwrap();
        config.daemon.checkpoint_max_age_seconds = 1800;
        let mut state = StateHandle::open_with_config(
            remanence_state::StatePaths::from_config(root.path().join("config.toml"), &config),
            config.clone(),
        )
        .unwrap();
        state
            .provision_tape_with_media(
                remanence_state::ProvisionTapeInput {
                    tape_uuid: UUID,
                    voltag: "HAND".into(),
                    block_size: BLOCK,
                    parity: parity.clone(),
                    force: false,
                },
                remanence_state::LabelProvenance::Operator,
                Some(remanence_state::MediaObservation {
                    generation: crate::LtoGen::Lto9,
                    source: remanence_state::GenerationSource::Operator,
                }),
            )
            .unwrap();
        let payload = crate::build_tape_bootstrap(
            UUID,
            BLOCK,
            parity,
            "2026-10-15T00:00:00Z",
            "mounted-test",
        );
        let mut block = vec![0; BLOCK as usize];
        remanence_parity::bootstrap::write_bootstrap_block(&payload, &mut block).unwrap();
        let mut tape = VirtualTape::empty(3 * 1024 * 1024 * 1024, BLOCK);
        tape.records = vec![Record::Block(block), Record::Filemark];
        tape.written_bytes = u64::from(BLOCK);
        let mut world = VirtualWorld::single_drive("NOT-A-LIBRARY-ENDPOINT", BAY, SERIAL, 0x400, 0);
        world.put_tape_in_drive(BAY, "HAND", None, tape);
        (
            Self {
                root,
                world: Arc::new(Mutex::new(world)),
                commands: Default::default(),
                fault: Default::default(),
                config,
            },
            state,
        )
    }
    fn reopen(&self) -> std::result::Result<StateHandle, remanence_state::StateError> {
        StateHandle::open_with_config(
            remanence_state::StatePaths::from_config(
                self.root.path().join("config.toml"),
                &self.config,
            ),
            self.config.clone(),
        )
    }
    fn owner(&self, state: StateHandle) -> MountedDrive {
        self.owner_with_recovery_observer(state, false)
    }
    fn owner_with_recovery_observer(
        &self,
        state: StateHandle,
        empty_recovery: bool,
    ) -> MountedDrive {
        self.owner_with_recovery_fault(state, empty_recovery, None)
    }
    fn owner_with_recovery_fault(
        &self,
        state: StateHandle,
        empty_recovery: bool,
        fault: Option<u8>,
    ) -> MountedDrive {
        let proof = empty_recovery.then(|| EmptyPrefixRecoveryProof {
            runtime: state.library_runtime(),
            step: 0,
            fail_position_after_delimiter: fault == Some(0x34),
        });
        let owner = MountedDrive::with_transport(
            state,
            Path::new(DEVICE),
            &StaticAllowlist::new([SERIAL]),
            Box::new(Transport {
                inner: ModelTransport::new(self.world.clone(), DeviceRole::Drive { bay: BAY }),
                commands: self.commands.clone(),
                fault: self.fault.clone(),
                evidence: Some(self.config.audit.dir.clone()),
                last_opcode: None,
                bootstrap_step: 0,
                empty_prefix_recovery: proof,
            }),
            self.root.path(),
            Some(UUID),
        )
        .unwrap();
        if fault != Some(0x34) {
            *self.fault.lock().unwrap() = fault;
        }
        owner
    }
    fn request(&self, name: &str) -> crate::WriteObjectToPoolRequest {
        let path = self.root.path().join(name);
        std::fs::write(&path, name.as_bytes()).unwrap();
        crate::WriteObjectToPoolRequest {
            pool_id: "default".into(),
            source: crate::WriteObjectSource::Path(path),
            archive_path: name.into(),
            caller_object_id: name.into(),
            expected_content_sha256: None,
            expected_object_id: None,
            representation: crate::PoolWriteRepresentation::Plaintext,
            input_kind: crate::WriteObjectInputKind::LogicalFile,
        }
    }
    fn index(&self) -> CatalogIndex {
        remanence_state::projector::open(&self.config.index.sqlite_path)
            .map(|catalog| catalog.write_handle())
            .unwrap()
    }
}
/// Read native tape evidence without taking a second lease from the mounted owner.
/// A never-admitted session may have no native file yet.
fn native_records(config: &remanence_state::RemConfig) -> Vec<remanence_state::tape_log::Record> {
    let path = config
        .journal
        .dir
        .join("tapes")
        .join(format!("{}.remtape", Uuid::from_bytes(UUID).simple()));
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read native session evidence: {error}"),
    };
    remanence_state::tape_log::store::Header::decode(&mut file, UUID).unwrap();
    let offset = std::io::Seek::stream_position(&mut file).unwrap();
    let mut decoder = remanence_state::tape_log::codec::Decoder::at(file, 1, offset);
    let mut records = Vec::new();
    while let Some(frame) = decoder.next_frame().unwrap() {
        records.push(frame.record);
    }
    records
}

#[test]
fn pinned_endpoint_receipts_source_and_explicit_finish_release_both_locks() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    assert!(fixture.reopen().is_err(), "state.lock remains held");
    let session = owner.open_session("default").unwrap();
    assert_eq!(session.drive_element_address, None);
    let receipt = owner.append(fixture.request("one")).unwrap();
    assert_eq!(owner.receipts(), &[receipt]);
    assert!(fixture.root.path().join("one").exists());
    owner.close_session().unwrap();
    assert!(fixture.reopen().is_err());
    let start = Instant::now();
    owner.finish().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
    let count = fixture.commands.lock().unwrap().len();
    owner.finish().unwrap();
    drop(owner);
    assert_eq!(count, fixture.commands.lock().unwrap().len());
    let state = fixture.reopen().unwrap();
    let mut second = fixture.owner(state);
    second.finish().unwrap();
    let commands = fixture.commands.lock().unwrap();
    assert!(
        !commands.iter().any(|c| matches!(c[0], 0x1b | 0xa5)),
        "no LOAD/UNLOAD or changer move"
    );
    assert_eq!(
        commands
            .iter()
            .filter(|c| c[0] == 0x1e && c[4] == 0)
            .count(),
        2
    );
}
#[test]
fn owner_end_closes_partial_epoch_without_unloading_and_keeps_receipt() {
    let (fixture, state) = Fixture::new(true);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    owner.append(fixture.request("parity")).unwrap();
    let tape = fixture.index().get_tape(&UUID).unwrap().unwrap();
    assert!(tape.total_committed_ordinals > tape.highest_protected_ordinal);
    owner.finish().unwrap();
    assert_eq!(owner.receipts().len(), 1);
    let tape = fixture.index().get_tape(&UUID).unwrap().unwrap();
    assert_eq!(
        tape.total_committed_ordinals,
        tape.highest_protected_ordinal
    );
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| c[0] == 0x1b));
}
#[test]
fn abort_and_append_failure_never_remove_caller_file_or_prior_receipt() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    owner.append(fixture.request("good")).unwrap();
    *fixture.fault.lock().unwrap() = Some(0x0a);
    let error = owner.append(fixture.request("bad")).unwrap_err();
    assert!(error.pool_write_error.is_some(), "{error:?}");
    assert_eq!(owner.receipts().len(), 1);
    let _ = owner.abort_session(Some("test failure".into()));
    let _ = owner.finish();
    assert!(fixture.root.path().join("good").exists());
    assert!(fixture.root.path().join("bad").exists());
    assert!(fixture.reopen().is_ok());
}
#[test]
fn drop_joins_active_owner_and_cancels_long_timers() {
    let (fixture, state) = Fixture::new(true);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    owner.append(fixture.request("drop")).unwrap();
    let start = Instant::now();
    drop(owner);
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(fixture.reopen().is_ok());
    assert_eq!(
        fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x1e && c[4] == 0)
            .count(),
        1
    );
}

#[test]
fn encrypted_append_carries_recipients_and_replay_guard_without_deleting_source() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    let key = remanence_aead::RecipientPrivateKey::new([9; 16], "mounted", [7; 32]).unwrap();
    let backup = remanence_aead::RecipientPrivateKey::new([10; 16], "backup", [8; 32]).unwrap();
    let mut request = fixture.request("secret");
    request.representation = crate::PoolWriteRepresentation::Encrypted {
        recipients: vec![key.public_key(0).unwrap(), backup.public_key(1).unwrap()],
    };
    let receipt = owner.append(request).unwrap();
    let native = fixture
        .index()
        .get_native_object(&Uuid::from_slice(&receipt.object_id).unwrap().to_string())
        .unwrap()
        .unwrap();
    assert_eq!(native.instances[0].representation, "encrypted");
    assert_eq!(
        native.instances[0].recipient_epoch_ids,
        Some(vec!["09".repeat(16), "0a".repeat(16)])
    );
    let error = owner.append(fixture.request("secret")).unwrap_err();
    assert!(
        matches!(
            error.pool_write_error.as_deref(),
            Some(crate::PoolWriteError::CallerObjectIdRepresentationConflict { .. })
        ),
        "{error:?}"
    );
    assert_eq!(owner.receipts().len(), 1);
    owner.finish().unwrap();
    let mut state = fixture.reopen().unwrap();
    let mut mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: None,
            last_opcode: None,
            bootstrap_step: 0,
            empty_prefix_recovery: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    let mut mounted = mount
        .mount(MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    let out = fixture.root.path().join("restored");
    let report = crate::scan_and_restore(&mut mounted.drive, &out, Some(&key)).unwrap();
    assert_eq!(report["candidates"][0]["status"], "restored", "{report}");
    assert_eq!(
        std::fs::read(out.join("candidate-1/secret")).unwrap(),
        b"secret"
    );
    mount.release(mounted);
}

#[test]
fn prepared_source_mutation_is_refused_and_preserved() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    let mut request = fixture.request("changed");
    let path = fixture.root.path().join("changed");
    request.source = crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(
        remanence_stream::prepare_regular_file(
            &path,
            &request.archive_path,
            Uuid::new_v4().to_string(),
        )
        .unwrap(),
    ));
    std::fs::write(&path, b"different bytes").unwrap();
    assert!(owner.append(request).is_err());
    assert_eq!(std::fs::read(path).unwrap(), b"different bytes");
    assert!(owner.receipts().is_empty());
    let _ = owner.finish();
}

#[test]
fn mounted_finalize_uses_prefix_plan_and_idempotent_recovery() {
    for parity in [false, true] {
        let (fixture, state) = Fixture::new(parity);
        let mut owner = fixture.owner(state);
        owner.open_session("default").unwrap();
        owner.append(fixture.request("final")).unwrap();
        owner.close_session().unwrap();
        let key = Uuid::new_v4();
        let request = || crate::StandaloneFinalizeRequest {
            tape_uuid: UUID,
            acknowledge_tape: UUID,
            expected_pool_id: Some("default".into()),
            reason: "operator requested".into(),
            idempotency_key: key,
        };
        let first = owner.finalize(request()).unwrap();
        let count = fixture.commands.lock().unwrap().len();
        let second = owner.finalize(request()).unwrap();
        assert_eq!(first, second);
        assert_eq!(count, fixture.commands.lock().unwrap().len());
        owner.finish().unwrap();
        assert!(fixture
            .index()
            .get_tape(&UUID)
            .unwrap()
            .unwrap()
            .terminal_finalization
            .is_some());
    }
}

#[test]
fn audit_failure_refuses_modifying_dispatch_even_when_config_fsync_is_false() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    let audit = &fixture.config.audit.dir;
    std::fs::rename(audit, audit.with_extension("saved")).unwrap();
    std::fs::write(audit, b"not a directory").unwrap();
    let error = owner.open_session("default").unwrap_err();
    assert!(!error.to_string().is_empty());
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
    assert!(owner.finish().is_err());
    std::fs::remove_file(audit).unwrap();
    std::fs::rename(audit.with_extension("saved"), audit).unwrap();
    assert!(fixture.reopen().is_ok());
}

#[test]
fn ready_only_gate_and_wrong_pool_refuse_before_write_motion() {
    let (fixture, state) = Fixture::new(false);
    use remanence_state::library_log as lib;
    let runtime = state.library_runtime();
    let tape = runtime.tape_state(UUID).unwrap().tape;
    runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    detail: lib::Bounded(vec![]),
                    kind: lib::FenceKind::Write,
                    scope: lib::FenceScope::Tape(tape),
                    cause: lib::FenceCause::TransferError,
                    evidence: None,
                    at: None,
                    operation: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    drop(runtime);
    let mut owner = fixture.owner(state);
    assert!(owner.open_session("default").is_err());
    assert!(owner.open_session("other").is_err());
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
    let _ = owner.finish();
}

#[test]
fn finish_reports_allow_failure_once_and_releases_device_and_state() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    *fixture.fault.lock().unwrap() = Some(0x1e);
    assert!(owner.finish().is_err());
    owner.finish().unwrap();
    drop(owner);
    assert_eq!(
        fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c[0] == 0x1e && c[4] == 0)
            .count(),
        1
    );
    let state = fixture.reopen().unwrap();
    let mut again = fixture.owner(state);
    again.finish().unwrap();
}

/// Ruling 32-L1: tail repair is refused before any effect, so a damaged log is left as found and
/// there is no repaired log, archive or reconciliation record for supersede-tail to confirm.
#[test]
fn tail_repair_is_refused_and_leaves_the_log_without_a_reconciliation_record() {
    use remanence_state::authority::TapeAuthorityView;
    use std::io::Write as _;
    for parity in [false, true] {
        let (fixture, state) = Fixture::initialized(parity);
        drop(state);
        let directory = fixture.config.journal.dir.join("tapes");
        let authority = remanence_state::TapeAuthority::open(
            &directory,
            UUID,
            &fixture.config.index.sqlite_path,
        )
        .unwrap();
        let through = authority.freeze().unwrap().log_reference().0;
        let path = directory.join(format!("{}.remtape", Uuid::from_bytes(UUID).simple()));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[0xff; 28])
            .unwrap();
        let damaged = std::fs::read(&path).unwrap();
        let mut state = fixture.reopen().unwrap();
        let names = |directory: &std::path::Path| {
            let mut names: Vec<_> = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            names.sort();
            names
        };
        let before = names(&directory);
        let error = state.repair_tape_log_tail(UUID, through, true).unwrap_err();
        assert!(error.to_string().contains("TailRepairDeferred"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        assert!(!path.with_extension("repair-required").exists());
        assert_eq!(
            names(&directory),
            before,
            "no archive or reconciliation record"
        );
    }
}

#[test]
fn fresh_state_rollback_refuses_complete_tail_then_explicit_supersede_repairs() {
    for parity in [false, true] {
        let (fixture, state) = Fixture::new(parity);
        drop(state);
        let backup = fixture.root.path().join("initial.sqlite");
        std::fs::copy(&fixture.config.index.sqlite_path, &backup).unwrap();
        let mut owner = fixture.owner(fixture.reopen().unwrap());
        owner.open_session("default").unwrap();
        owner.append(fixture.request("foreign")).unwrap();
        owner.finish().unwrap();
        drop(owner);
        std::fs::copy(&backup, &fixture.config.index.sqlite_path).unwrap();
        std::fs::remove_dir_all(&fixture.config.journal.dir).unwrap();
        std::fs::create_dir_all(fixture.config.journal.dir.join("tapes")).unwrap();
        let mut owner = fixture.owner(fixture.reopen().unwrap());
        fixture.commands.lock().unwrap().clear();
        let before = fixture.world.lock().unwrap().tapes["HAND"].records.clone();
        let error = owner.open_session("default").unwrap_err();
        assert!(
            error.to_string().contains("TailBeyondKnownState"),
            "{error:?}"
        );
        assert!(!fixture.commands.lock().unwrap().is_empty());
        assert!(!fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
        assert_eq!(fixture.world.lock().unwrap().tapes["HAND"].records, before);
        assert_eq!(
            fixture.index().get_tape(&UUID).unwrap().unwrap().state,
            "ready"
        );
        // Ruling 13: the suffix contradiction is a durable fence, not a tape state.
        let fences = fixture.index().list_active_tape_io_fences().unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].tape_uuid, UUID);
        assert_eq!(fences[0].reason, "session_suffix_contradiction");
        let preview = owner
            .supersede_tail(pb::SupersedeTailRequest {
                observed_tape_log_sequence: None,
                tape_uuid: UUID.to_vec(),
                ack_tape_uuid: UUID.to_vec(),
                confirm_supersede: None,
                expected_found: vec![],
            })
            .unwrap();
        assert!(!preview.completed);
        assert!(!preview.found.is_empty());
        let completed = owner
            .supersede_tail(pb::SupersedeTailRequest {
                observed_tape_log_sequence: Some(preview.tape_log_sequence),
                tape_uuid: UUID.to_vec(),
                ack_tape_uuid: UUID.to_vec(),
                confirm_supersede: Some(preview.required_confirmation),
                expected_found: preview.found,
            })
            .unwrap();
        assert!(completed.completed);
        owner.open_session("default").unwrap();
        owner.append(fixture.request("repaired")).unwrap();
        owner.finish().unwrap();
    }
}

#[test]
#[ignore = "subprocess helper; invoked by abrupt_process_restart_recovers_only_the_recorded_endpoint_with_fsync_disabled"]
fn abrupt_owner_child() {
    let manifest =
        std::env::var_os("REM_STEP1_CRASH_MANIFEST").expect("parent supplies crash manifest");
    let (fixture, state) = Fixture::new(true);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    owner.append(fixture.request("crash")).unwrap();
    std::fs::write(manifest, serde_json::to_vec(&fixture.config).unwrap()).unwrap();
    // Terminate the actual owner process without running Rust Drop or finish.
    std::process::exit(86);
}

#[test]
fn abrupt_process_restart_recovers_only_the_recorded_endpoint_with_fsync_disabled() {
    let (fixture, state) = Fixture::new(false);
    drop(state);
    let manifest = fixture.root.path().join("crash.json");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "engine::drive_adapter::tests::abrupt_owner_child",
            "--ignored",
            "--nocapture",
        ])
        .env("REM_STEP1_CRASH_MANIFEST", &manifest)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));
    let config: remanence_state::RemConfig =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert!(!config.audit.fsync);
    use remanence_state::tape_log::Record as Native;
    let records = native_records(&config);
    let crashed_session = records
        .iter()
        .find_map(|record| match record {
            Native::SessionOpened(open) => Some(open.session_id),
            _ => None,
        })
        .expect("crashed child recorded its native session");
    assert!(!records.iter().any(|record| matches!(
        record,
        Native::SessionEnded(end) if end.session_id == crashed_session
    )));
    let mut state = StateHandle::open_with_config(
        remanence_state::StatePaths::from_config(
            config.daemon.state_dir.join("config.toml"),
            &config,
        ),
        config.clone(),
    )
    .unwrap();
    let other = remanence_library::DriveKey::Standalone {
        drive_serial: "OTHER-DEVICE".into(),
    };
    state
        .recover_sessions_after_restart(Some(&other), |_, _| {
            panic!("unrelated endpoint recovered a mounted session")
        })
        .unwrap();
    assert_eq!(state.catalog().non_terminal_sessions().unwrap().len(), 1);
    let mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: None,
            last_opcode: None,
            bootstrap_step: 0,
            empty_prefix_recovery: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    mount.finish().unwrap();
    // C8-G2 and DEC-2: typed restart leaves native write sessions open;
    // the next open of their tape reconciles them, without a restart fence.
    let fences = state.catalog().list_active_tape_io_fences().unwrap();
    assert!(!fences.iter().any(|fence| fence.tape_uuid == UUID));
    let sessions = state.catalog().non_terminal_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, Uuid::from_bytes(crashed_session));
    assert_eq!(
        state
            .catalog()
            .session_state(sessions[0].session_id)
            .unwrap()
            .as_deref(),
        Some("open")
    );
    drop(state);
    std::fs::remove_dir_all(&config.daemon.state_dir).unwrap();
}

#[test]
fn swapped_bot_identity_refuses_without_modifying_media() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    let payload = crate::build_tape_bootstrap(
        [0x82; 16],
        BLOCK,
        crate::ParityConfig::None,
        "2026-10-15T00:00:00Z",
        "swapped",
    );
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&payload, &mut block).unwrap();
    fixture
        .world
        .lock()
        .unwrap()
        .tapes
        .get_mut("HAND")
        .unwrap()
        .records[0] = Record::Block(block);
    fixture.commands.lock().unwrap().clear();
    assert!(owner.open_session("default").is_err());
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
    let _ = owner.finish();
}

#[test]
fn provisional_representation_guard_and_session_lease_release() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    let append = |owner: &MountedDrive, representation| {
        let request = fixture.request("pending");
        let source = crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(
            remanence_stream::prepare_regular_file(
                fixture.root.path().join("pending"),
                &request.archive_path,
                Uuid::new_v4().to_string(),
            )
            .unwrap(),
        ));
        let (reply, rx) = oneshot::channel();
        owner
            .client
            .send(DriveCommand::AppendFinish {
                session_id: owner.client.session.as_ref().unwrap().0,
                source,
                archive_path: request.archive_path,
                caller_object_id: request.caller_object_id,
                expected_content_sha256: None,
                expected_object_id: None,
                input_kind: request.input_kind,
                representation,
                live_write_counter: None,
                reply,
            })
            .unwrap();
        receive(rx)
    };
    append(&owner, crate::PoolWriteRepresentation::Plaintext).unwrap();
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        fixture.config.journal.dir.join("tapes"),
        UUID,
    )
    .build()
    .unwrap();
    assert!(journal.acquire_exclusive().is_err());
    let key = remanence_aead::RecipientPrivateKey::new([9; 16], "key", [7; 32]).unwrap();
    let error = append(
        &owner,
        crate::PoolWriteRepresentation::Encrypted {
            recipients: vec![key.public_key(0).unwrap()],
        },
    )
    .unwrap_err();
    assert!(matches!(
        error.pool_write_error.as_deref(),
        Some(crate::PoolWriteError::CallerObjectIdRepresentationConflict { .. })
    ));
    owner.close_session().unwrap();
    drop(journal.acquire_exclusive().unwrap());
    owner.open_session("default").unwrap();
    owner.append(fixture.request("pending")).unwrap();
    assert_eq!(owner.receipts().len(), 1);
    owner.finish().unwrap();
    drop(journal.acquire_exclusive().unwrap());
    assert!(fixture.root.path().join("pending").exists());
}

#[test]
fn transferred_reservation_and_audit_latch_survive_into_owner_and_finish() {
    let (fixture, mut state) = Fixture::new(false);
    let mut mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: Some(fixture.config.audit.dir.clone()),
            last_opcode: None,
            bootstrap_step: 0,
            empty_prefix_recovery: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    let mut mounted = mount
        .mount(MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    mount.reserve(&mut mounted).unwrap();
    let latch = mount.context.audit_error.clone();
    mount.finish().unwrap();
    let mut owner = MountedDrive::start(
        state,
        mounted,
        StaticAllowlist::new([SERIAL]),
        latch.clone(),
    )
    .unwrap();
    owner.open_session("default").unwrap();
    let request = fixture.request("durable");
    let prepared = remanence_stream::prepare_regular_file(
        fixture.root.path().join("durable"),
        &request.archive_path,
        Uuid::new_v4().to_string(),
    )
    .unwrap();
    let (reply, rx) = oneshot::channel();
    owner
        .client
        .send(DriveCommand::AppendFinish {
            session_id: owner.client.session.as_ref().unwrap().0,
            source: crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(
                prepared,
            )),
            archive_path: request.archive_path,
            caller_object_id: request.caller_object_id,
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: request.input_kind,
            representation: request.representation,
            live_write_counter: None,
            reply,
        })
        .unwrap();
    receive(rx).unwrap();
    let (reply, rx) = oneshot::channel();
    owner
        .client
        .send(DriveCommand::Checkpoint {
            session_id: owner.client.session.as_ref().unwrap().0,
            trigger: write_owner::CheckpointTrigger::Append,
            expected_batch_id: None,
            reply: Some(reply),
        })
        .unwrap();
    assert_eq!(receive(rx).unwrap().committed_objects.len(), 1);
    assert!(owner.receipts().is_empty());

    *latch.lock().unwrap() = Some("injected drive audit append failure".into());
    // Claim a receipt retained by an automatic barrier despite the later audit
    // failure; this path must not start another checkpoint or issue a CDB.
    let before_claim = fixture.commands.lock().unwrap().len();
    owner.checkpoint().unwrap();
    assert_eq!(before_claim, fixture.commands.lock().unwrap().len());
    let count = fixture.commands.lock().unwrap().len();
    let error = owner.append(fixture.request("refused")).unwrap_err();
    assert!(error.to_string().contains("drive audit failed"));
    assert_eq!(count, fixture.commands.lock().unwrap().len());
    assert!(fixture.root.path().join("refused").exists());
    assert_eq!(owner.receipts().len(), 1);
    assert!(owner.finish().is_err());
    drop(owner);
    let commands = fixture.commands.lock().unwrap();
    for opcode in [0x16, 0x17] {
        assert_eq!(commands.iter().filter(|c| c[0] == opcode).count(), 1);
    }
    assert_eq!(
        commands
            .iter()
            .filter(|c| c[0] == 0x1e && c[4] == 0)
            .count(),
        1
    );
    assert!(fixture.reopen().is_ok());
}

#[tokio::test]
async fn drop_on_async_runtime_defers_join_without_blocking_worker() {
    let (fixture, state) = Fixture::new(false);
    let (fixture, owner) = tokio::task::spawn_blocking(move || {
        let owner = fixture.owner(state);
        (fixture, owner)
    })
    .await
    .unwrap();
    // Block the first cleanup CDB on a separate thread. Its timeout prevents the
    // old synchronous Drop from hanging this test; that implementation fails the
    // elapsed-time assertion and cannot run another task on this current-thread runtime.
    let commands = fixture.commands.clone();
    let (entered, ready) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        let _guard = commands.lock().unwrap();
        entered.send(()).unwrap();
        let _ = released.recv_timeout(Duration::from_secs(3));
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    drop(owner);
    tokio::task::yield_now().await;
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "Drop must not wait on drive I/O on a Tokio worker"
    );
    assert!(
        fixture.reopen().is_err(),
        "detached cleanup still owns state.lock"
    );
    release.send(()).unwrap();
    tokio::task::spawn_blocking(move || {
        blocker.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(state) = fixture.reopen() {
                drop(state);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "detached cleanup joins the drive and releases its locks"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    })
    .await
    .unwrap();
}

#[test]
fn automatic_terminal_receipt_and_host_only_reopen_use_existing_completion() {
    let (mut fixture, state) = Fixture::new(false);
    drop(state);
    fixture.config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
    fixture.config.tape_pools[0].watermark_low = 0.000001;
    fixture.config.tape_pools[0].watermark_high = 0.25;
    let mut owner = fixture.owner(fixture.reopen().unwrap());
    owner.open_session("default").unwrap();
    let receipt = owner.append(fixture.request("terminal")).unwrap();
    assert_eq!(owner.receipts(), &[receipt]);
    owner.close_session().unwrap();
    assert!(fixture
        .index()
        .get_tape(&UUID)
        .unwrap()
        .unwrap()
        .terminal_finalization
        .is_some());
    let count = fixture.commands.lock().unwrap().len();
    let error = owner.open_session("default").unwrap_err();
    assert!(
        matches!(
            error.pool_write_error.as_deref(),
            Some(crate::PoolWriteError::TerminalCloseRequired { .. })
        ),
        "{error:?}"
    );
    assert_eq!(
        count,
        fixture.commands.lock().unwrap().len(),
        "completed terminal retry is host-only"
    );
    assert_eq!(owner.receipts().len(), 1);
    owner.finish().unwrap();
}

#[test]
fn device_lock_excludes_another_state_directory_until_owner_finish() {
    let (fixture, state) = Fixture::new(false);
    let mut owner = fixture.owner(state);
    let (other, other_state) = Fixture::new(false);
    let attempt = |state| {
        MountedDrive::with_transport(
            state,
            Path::new(DEVICE),
            &StaticAllowlist::new([SERIAL]),
            Box::new(Transport {
                inner: ModelTransport::new(other.world.clone(), DeviceRole::Drive { bay: BAY }),
                commands: other.commands.clone(),
                fault: other.fault.clone(),
                evidence: None,
                last_opcode: None,
                bootstrap_step: 0,
                empty_prefix_recovery: None,
            }),
            fixture.root.path(),
            Some(UUID),
        )
    };
    assert!(attempt(other_state).is_err());
    owner.finish().unwrap();
    let mut next = attempt(other.reopen().unwrap()).unwrap();
    next.finish().unwrap();
}

/// A successful media-path seal still refuses Objects, but is not a lost write.
#[test]
fn automatic_terminal_media_completion_closes_audit_without_a_false_fence() {
    use remanence_state::TerminalFinalizationTrigger as Trigger;
    for trigger in [
        Trigger::ReachedLowWatermark,
        Trigger::HardwareEarlyWarning,
        Trigger::PoolCloseOut,
        Trigger::NoPendingObjectFits,
    ] {
        check_automatic_terminal_media_completion(trigger);
    }
}

/// Exercise recovery through the actor with a durable typed seal obligation.
fn check_automatic_terminal_media_completion(
    trigger: remanence_state::TerminalFinalizationTrigger,
) {
    use remanence_state::authority::{TapeAuthorityView, TapeAuthorityWriter};
    use remanence_state::tape_log::{Record as Native, SessionEndReason};

    let (mut fixture, state) = Fixture::new(false);
    drop(state);
    fixture.config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
    fixture.config.tape_pools[0].watermark_low = 0.1;
    fixture.config.tape_pools[0].watermark_high = 0.25;
    let state = fixture.reopen().unwrap();
    let runtime = state.library_runtime();
    let mut owner = fixture.owner(state);
    owner.open_session("default").unwrap();
    let receipt = owner.append(fixture.request("seal-due")).unwrap();
    owner.close_session().unwrap();
    // Persist the typed seal obligation at the recovery cut before projecting
    // its display cache. Recovery takes the reason from native authority.
    let index = fixture.index();
    let journal = remanence_state::TapeAuthority::open(
        fixture.config.journal.dir.join("tapes"),
        UUID,
        index.path(),
    )
    .unwrap();
    let mut writer = journal.acquire_exclusive().unwrap();
    let mut checkpoint = writer.latest_commit().unwrap().unwrap();
    checkpoint.ordinal += 1;
    checkpoint.operation_id = [0x43; 16];
    checkpoint.session_id = None;
    checkpoint.seal_due = Some(trigger);
    writer.publish_report(&checkpoint).unwrap();
    runtime.refresh_native(&writer.freeze().unwrap()).unwrap();
    runtime.catch_up().unwrap();
    drop(writer);
    drop(index);
    assert!(fixture
        .index()
        .get_tape(&UUID)
        .unwrap()
        .unwrap()
        .seal_due
        .is_some());
    let before = fixture.commands.lock().unwrap().len();
    let error = owner.open_session("default").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("completes terminal finalization"),
        "{error:?}"
    );
    assert!(
        fixture.commands.lock().unwrap()[before..]
            .iter()
            .any(|c| c[0] == 0x0a),
        "this is media completion, not host-only replay"
    );
    // A subsequent mailbox command waits for the open's audit settlement.
    owner.finish().unwrap();
    let index = fixture.index();
    assert_eq!(
        index
            .get_tape(&UUID)
            .unwrap()
            .unwrap()
            .terminal_finalization
            .unwrap()
            .trigger,
        trigger
    );
    let endpoint = remanence_library::DriveKey::Standalone {
        drive_serial: SERIAL.into(),
    };
    assert!(!index
        .tape_io_is_fenced(Some(&UUID), None, Some(&endpoint))
        .unwrap());
    let records = native_records(&fixture.config);
    let opened: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            Native::SessionOpened(open) => Some(open),
            _ => None,
        })
        .collect();
    let open = opened.first().expect("native write session opened");
    let endings: Vec<_> = records
        .iter()
        .enumerate()
        .filter_map(|(position, record)| match record {
            Native::SessionEnded(end) if end.session_id == open.session_id => Some((position, end)),
            _ => None,
        })
        .collect();
    assert_eq!(endings.len(), 1, "one native ending for the write session");
    let (end_position, end) = endings[0];
    assert_eq!(end.reason, SessionEndReason::CleanClose);
    // D-C3a-1b-1: recovery finalizes before native session admission. Its
    // durable evidence is the finalization itself, outside any write session.
    let after_end = &records[end_position + 1..];
    assert!(
        !after_end
            .iter()
            .any(|record| matches!(record, Native::SessionOpened(_))),
        "automatic finalization must not open another native session"
    );
    assert_eq!(opened.len(), 1);
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record, Native::SessionEnded(_)))
            .count(),
        1
    );
    assert!(!records.iter().any(|record| matches!(
        record,
        Native::SessionEnded(end) if end.reason == SessionEndReason::CrashRecovered
    )));
    let decisions: Vec<_> = after_end
        .iter()
        .enumerate()
        .filter_map(|(position, record)| {
            matches!(record, Native::FinalizationDecided(_)).then_some(position)
        })
        .collect();
    let seals: Vec<_> = after_end
        .iter()
        .enumerate()
        .filter_map(|(position, record)| {
            matches!(record, Native::Sealed { .. }).then_some(position)
        })
        .collect();
    assert_eq!(decisions.len(), 1);
    assert_eq!(seals.len(), 1);
    assert!(
        decisions[0] < seals[0],
        "finalization decision precedes its seal"
    );
    assert_eq!(owner.receipts(), &[receipt]);
}

/// No-op owner finish must not create evidence that restart could call an orphan.
#[test]
fn finish_without_an_open_epoch_adds_no_session_evidence() {
    for parity in [false, true] {
        for admitted in [false, true] {
            let (fixture, state) = Fixture::new(parity);
            let mut owner = fixture.owner(state);
            if admitted {
                owner.open_session("default").unwrap();
                owner.close_session().unwrap();
            }
            let before = native_records(&fixture.config);
            owner.finish().unwrap();
            let after = native_records(&fixture.config);
            let session_events = |records: &[remanence_state::tape_log::Record]| {
                use remanence_state::tape_log::Record as Native;
                records
                    .iter()
                    .filter_map(|record| match record {
                        Native::SessionOpened(open) => Some((open.session_id, record.tag())),
                        Native::SessionEnded(end) => Some((end.session_id, record.tag())),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(session_events(&before), session_events(&after));
            assert_eq!(session_events(&after).len(), if admitted { 2 } else { 0 });
        }
    }
}

/// Timer closure is actor-owned; the adapter must forget the stale ID and keep receipts.
#[test]
fn idle_close_allows_reopen_and_keeps_claimed_receipts() {
    let (mut fixture, state) = Fixture::new(false);
    drop(state);
    fixture.config.daemon.checkpoint_max_age_seconds = 1;
    fixture.config.daemon.default_idle_timeout_seconds = 1;
    let mut owner = fixture.owner(fixture.reopen().unwrap());
    owner.open_session("default").unwrap();
    let request = fixture.request("idle");
    let prepared = remanence_stream::prepare_regular_file(
        fixture.root.path().join("idle"),
        &request.archive_path,
        Uuid::new_v4().to_string(),
    )
    .unwrap();
    let (reply, rx) = oneshot::channel();
    owner
        .client
        .send(DriveCommand::AppendFinish {
            session_id: owner.client.session.as_ref().unwrap().0,
            source: crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(
                prepared,
            )),
            archive_path: request.archive_path,
            caller_object_id: request.caller_object_id,
            expected_content_sha256: None,
            expected_object_id: None,
            input_kind: request.input_kind,
            representation: request.representation,
            live_write_counter: None,
            reply,
        })
        .unwrap();
    receive(rx).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (reply, rx) = oneshot::channel();
        owner
            .client
            .send(DriveCommand::Get {
                session_id: owner.client.session.as_ref().unwrap().0,
                reply,
            })
            .unwrap();
        if receive(rx).unwrap().state
            == pb::write_session::State::WriteSessionStateCheckpointed as i32
        {
            break;
        }
        assert!(Instant::now() < deadline, "timer checkpoint did not fire");
        std::thread::sleep(Duration::from_millis(10));
    }
    owner.checkpoint().unwrap();
    let receipts = owner.receipts().to_vec();
    assert_eq!(receipts.len(), 1);
    let session_id = owner.client.session.as_ref().unwrap().0;
    loop {
        if native_records(&fixture.config)
            .iter()
            .any(|record| matches!(record, remanence_state::tape_log::Record::SessionEnded(end) if end.session_id == *session_id.as_bytes()))
        {
            break;
        }
        assert!(Instant::now() < deadline, "idle timer did not close");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        owner.close_session().unwrap().state,
        pb::write_session::State::WriteSessionStateClosed as i32
    );
    let authority = remanence_state::TapeAuthority::open(
        fixture.config.journal.dir.join("tapes"),
        UUID,
        &fixture.root.path().join("state.sqlite"),
    )
    .unwrap();
    let snapshot = authority.freeze().unwrap();
    let end = snapshot.tape_summary().last_session_end.as_ref().unwrap();
    assert_eq!(
        end.reason,
        remanence_state::tape_log::SessionEndReason::ClientStall
    );
    assert_eq!(end.tail_file_count, 0);
    assert!(snapshot.tape_summary().open_session.is_none());
    let next = owner.open_session("default").unwrap();
    assert_ne!(next.session_id, session_id.as_bytes());
    assert_eq!(owner.receipts(), receipts);
    owner.append(fixture.request("after-idle")).unwrap();
    assert_eq!(owner.receipts().len(), 2);
    owner.abort_session(None).unwrap();
    owner.finish().unwrap();
}

#[tokio::test]
async fn cleanup_spawn_failure_finishes_in_place_and_releases_every_obligation() {
    let (fixture, mut owner) = tokio::task::spawn_blocking(|| {
        let (fixture, mut state) = Fixture::new(false);
        let mut mount = OperatorMount::with_transport(
            Path::new(DEVICE),
            &StaticAllowlist::new([SERIAL]),
            Box::new(Transport {
                inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
                commands: fixture.commands.clone(),
                fault: fixture.fault.clone(),
                evidence: Some(fixture.config.audit.dir.clone()),
                last_opcode: None,
                bootstrap_step: 0,
                empty_prefix_recovery: None,
            }),
            &mut state,
            fixture.root.path(),
        )
        .unwrap();
        let mut mounted = mount
            .mount(MountTarget::Device {
                path: DEVICE.into(),
                expected_uuid: Some(UUID),
            })
            .unwrap();
        mount.reserve(&mut mounted).unwrap();
        let latch = mount.context.audit_error.clone();
        mount.finish().unwrap();
        let mut owner = MountedDrive::start(
            state,
            mounted,
            StaticAllowlist::new([SERIAL]),
            latch.clone(),
        )
        .unwrap();
        owner.open_session("default").unwrap();
        owner.append(fixture.request("spawn-failure")).unwrap();
        (fixture, owner)
    })
    .await
    .unwrap();
    // Simulate EAGAIN on an async worker. The exceptional synchronous fallback
    // must use the actor completion channel without Tokio blocking_recv panics.
    defer_cleanup(owner.owner.take().unwrap(), |_| {
        Err(std::io::Error::from_raw_os_error(11))
    });
    drop(owner);
    assert!(fixture.reopen().is_ok());
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        fixture.config.journal.dir.join("tapes"),
        UUID,
    )
    .build()
    .unwrap();
    drop(journal.acquire_exclusive().unwrap());
    let commands = fixture.commands.lock().unwrap();
    for opcode in [0x16, 0x17] {
        assert_eq!(commands.iter().filter(|c| c[0] == opcode).count(), 1);
    }
    assert_eq!(
        commands
            .iter()
            .filter(|c| c[0] == 0x1e && c[4] == 0)
            .count(),
        1
    );
}

#[test]
fn frontend_batches_one_invocation_and_all_skips_open_no_session() {
    let (fixture, state) = Fixture::new(true);
    let mut owner = fixture.owner(state);
    crate::startup_checkpoint::RECONCILIATION_COUNT.with(|count| count.set(0));
    for name in ["first", "second"] {
        assert!(owner
            .put_path_version_with_policy(fixture.request(name), false)
            .unwrap()
            .is_ok());
    }
    let opened = || {
        native_records(&fixture.config)
            .iter()
            .filter(|record| matches!(record, remanence_state::tape_log::Record::SessionOpened(_)))
            .count()
    };
    assert_eq!(opened(), 1, "one session spans both object checkpoints");
    assert_eq!(owner.receipts().len(), 2);
    crate::startup_checkpoint::RECONCILIATION_COUNT.with(|count| assert_eq!(count.get(), 1));
    owner.finish().unwrap();
    let sessions = opened();
    let modifying = || {
        fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c[0], 0x0a | 0x10 | 0x19))
            .count()
    };
    let before = modifying();
    let mut owner = fixture.owner(fixture.reopen().unwrap());
    for name in ["first", "second"] {
        assert!(owner
            .put_path_version_with_policy(fixture.request(name), false)
            .unwrap()
            .is_err());
    }
    owner.finish().unwrap();
    assert_eq!(opened(), sessions);
    assert_eq!(modifying(), before);
}

#[test]
fn frontend_returns_committed_receipt_beside_post_projection_failure() {
    let (fixture, mut state) = Fixture::new(false);
    let mut mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: Some(fixture.config.audit.dir.clone()),
            last_opcode: None,
            bootstrap_step: 0,
            empty_prefix_recovery: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    let mounted = mount
        .mount(MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    let audit = mount.context.audit_error.clone();
    mount.finish().unwrap();
    let cfg = mounted_config(&state, &mounted, StaticAllowlist::new([SERIAL]), audit).unwrap();
    cfg.io_memory
        .epoch_test_action
        .store(9, std::sync::atomic::Ordering::Relaxed);
    let endpoint = cfg.mounted().unwrap().endpoint.clone();
    let (tx, thread) = write_owner::spawn_mounted_actor(mounted, cfg).unwrap();
    let mut owner = MountedDrive {
        owner: Some(Owner {
            tx: tx.clone(),
            thread,
            state,
            _library_guard: None,
        }),
        client: MountedCommands::new(tx, UUID, endpoint),
    };
    let (receipt, reason) = owner
        .put_path_version_with_policy(fixture.request("committed-before-failure"), false)
        .expect("durable commit succeeds")
        .unwrap();
    assert_eq!(reason, "new");
    assert_eq!(receipt.object.caller_object_id, "committed-before-failure");
    assert_eq!(owner.receipts().len(), 1);
    let _ = owner.finish();
    assert!(fixture.reopen().is_ok());
}

/// A B6b projection failure returns the native receipt on replay after ending.
#[test]
fn projection_busy_returns_committed_receipt_on_native_replay() {
    let (fixture, mut state) = Fixture::new(false);
    let mut mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: Some(fixture.config.audit.dir.clone()),
            last_opcode: None,
            bootstrap_step: 0,
            empty_prefix_recovery: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    let mounted = mount
        .mount(MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    let audit = mount.context.audit_error.clone();
    mount.finish().unwrap();
    let cfg = mounted_config(&state, &mounted, StaticAllowlist::new([SERIAL]), audit).unwrap();
    cfg.io_memory
        .epoch_test_action
        .store(10, std::sync::atomic::Ordering::Relaxed);
    let endpoint = cfg.mounted().unwrap().endpoint.clone();
    let (tx, thread) = write_owner::spawn_mounted_actor(mounted, cfg).unwrap();
    let mut owner = MountedDrive {
        owner: Some(Owner {
            tx: tx.clone(),
            thread,
            state,
            _library_guard: None,
        }),
        client: MountedCommands::new(tx, UUID, endpoint),
    };
    let receipt = owner
        .put_path_version_with_policy(fixture.request("committed-before-failure"), false)
        .expect("durable native commit returns its receipt despite projection failure");
    assert_eq!(
        receipt.unwrap().0.object.caller_object_id,
        "committed-before-failure"
    );
    assert_eq!(owner.receipts().len(), 1);
    let _ = owner.finish();
    let state = fixture.reopen().unwrap();
    let mut owner = fixture.owner(state);
    let receipt = owner
        .write_checkpointed(fixture.request("committed-before-failure"))
        .unwrap();
    assert!(
        receipt.is_replay(),
        "native commit supplies the receipt on replay, without a second Object"
    );
    assert_eq!(receipt.object.caller_object_id, "committed-before-failure");
    owner.finish().unwrap();
}

/// A catalog generation cannot replace the generation proof captured by the mount.
#[test]
fn supersede_requires_mounted_generation_before_any_modifying_command() {
    let (fixture, mut state) = Fixture::new(false);
    let policy = StaticAllowlist::new([SERIAL]);
    let mut mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &policy,
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: Some(fixture.config.audit.dir.clone()),
            last_opcode: None,
            bootstrap_step: 0,
            empty_prefix_recovery: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    let mut mounted = mount
        .mount(MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    // Reads permit an unknown mount generation even when the catalog has one.
    // Exercise that exact handoff, independently of transport density inference.
    mounted.generation = None;
    let audit = mount.context.audit_error.clone();
    mount.finish().unwrap();
    assert!(state
        .catalog()
        .get_tape(&UUID)
        .unwrap()
        .unwrap()
        .cartridge_generation
        .is_some());
    let mut owner = MountedDrive::start(state, mounted, policy, audit).unwrap();
    let before = fixture.commands.lock().unwrap().len();
    for confirmation in [None, Some("SUPERSEDE HAND".into())] {
        let error = owner
            .supersede_tail(pb::SupersedeTailRequest {
                observed_tape_log_sequence: None,
                tape_uuid: UUID.to_vec(),
                ack_tape_uuid: UUID.to_vec(),
                confirm_supersede: confirmation,
                expected_found: vec![],
            })
            .unwrap_err();
        assert_eq!(error.status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            error.status.message(),
            "tape generation is required before superseding a tail"
        );
    }
    assert_eq!(fixture.commands.lock().unwrap().len(), before);
    owner.finish().unwrap();
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|cdb| matches!(cdb[0], 0x0a | 0x10 | 0x19)));
}

/// Terminal frontends retain the full standalone refusal, including release guidance.
#[test]
fn standalone_terminal_fence_refusals_keep_quarantine_id_and_release_command() {
    let here = remanence_library::DriveKey::Standalone {
        drive_serial: SERIAL.into(),
    };
    let elsewhere = remanence_library::DriveKey::Standalone {
        drive_serial: "OTHER".into(),
    };
    for (tape_uuid, endpoint, release_options) in [
        (UUID, Some(&elsewhere), "--ack TEXT"),
        ([9; 16], Some(&here), "--ack TEXT"),
        (UUID, None, "--after-settled-inventory --ack TEXT"),
    ] {
        let (fixture, state) = Fixture::new(false);
        let mut owner = fixture.owner(state);
        let fence = fixture
            .index()
            .record_tape_io_fence_at_endpoint(
                remanence_state::TapeIoFenceInput {
                    tape_uuid,
                    barcode: None,
                    reason: "review regression".into(),
                    evidence_json: None,
                },
                endpoint,
            )
            .unwrap();
        let expected = format!(
            "write blocked by active tape-I/O fence {}: review regression; inspect `rem tape quarantine list`, then release with `rem tape quarantine release {} {release_options}`",
            fence.quarantine_id, fence.quarantine_id
        );
        let before = fixture.commands.lock().unwrap().len();
        let error = owner
            .finalize_standalone(crate::StandaloneFinalizeRequest {
                tape_uuid: UUID,
                acknowledge_tape: UUID,
                expected_pool_id: Some("default".into()),
                reason: "operator requested".into(),
                idempotency_key: Uuid::new_v4(),
            })
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
        let error = owner
            .supersede_tail_confirmed("HAND", |_, _| {
                panic!("a fence must refuse before asking for confirmation")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
        assert_eq!(fixture.commands.lock().unwrap().len(), before);
        // Finish reports the existing fence but still joins and releases the locks.
        owner.finish().unwrap();
        assert!(fixture.reopen().is_ok());
    }
}

mod library;

/// A source returning to the newest bytes during preparation must never reach media.
#[test]
fn prepared_skip_and_command_reconciliation_are_independent_of_file_count() {
    let (fixture, state) = Fixture::new(false);
    let mut actor = fixture.owner(state);
    crate::startup_checkpoint::RECONCILIATION_COUNT.with(|count| count.set(0));
    for name in ["first", "second", "third"] {
        assert!(actor
            .put_path_version_with_policy(fixture.request(name), false)
            .unwrap()
            .is_ok());
    }
    let before = fixture.commands.lock().unwrap().len();
    for name in ["first", "second", "third"] {
        let input = fixture.request(name);
        let crate::WriteObjectSource::Path(path) = &input.source else {
            panic!("path source")
        };
        let path = path.clone();
        std::fs::write(&path, b"changed before preparation").unwrap();
        let decision = put_version(
            &mut actor.client,
            &mut actor.owner.as_mut().unwrap().state,
            input,
            false,
            || {
                std::fs::write(path, name.as_bytes()).unwrap();
            },
        )
        .unwrap();
        assert!(decision.is_err(), "prepared digest must skip");
    }
    assert_eq!(fixture.commands.lock().unwrap().len(), before);
    crate::startup_checkpoint::RECONCILIATION_COUNT.with(|count| assert_eq!(count.get(), 1));
    actor.finish().unwrap();
}

mod pool_sessions;

/// A real tail beyond restored host state must exclude the tape from production pool selection.
#[test]
fn gate2b_rejected_tail_remains_ineligible() {
    for parity in [false, true] {
        let (fixture, state) = Fixture::new(parity);
        drop(state);
        let backup = fixture.root.path().join("initial.sqlite");
        std::fs::copy(&fixture.config.index.sqlite_path, &backup).unwrap();
        let state = fixture.reopen().unwrap();
        fixture.publish_initialized(&state, parity);
        let mut owner = fixture.owner(state);
        owner.open_session("default").unwrap();
        owner.append(fixture.request("foreign")).unwrap();
        owner.finish().unwrap();
        drop(owner);
        std::fs::copy(&backup, &fixture.config.index.sqlite_path).unwrap();
        std::fs::remove_dir_all(&fixture.config.journal.dir).unwrap();
        std::fs::create_dir_all(fixture.config.journal.dir.join("tapes")).unwrap();
        let mut owner = fixture.owner(fixture.reopen().unwrap());
        fixture.commands.lock().unwrap().clear();
        let before = fixture.world.lock().unwrap().tapes["HAND"].records.clone();
        let error = owner.open_session("default").unwrap_err();
        assert!(
            error.to_string().contains("TailBeyondKnownState"),
            "{error:?}"
        );
        assert!(!fixture.commands.lock().unwrap().is_empty());
        assert!(!fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
        assert_eq!(fixture.world.lock().unwrap().tapes["HAND"].records, before);
        assert_eq!(
            fixture.index().get_tape(&UUID).unwrap().unwrap().state,
            "ready"
        );
        let fences = fixture.index().list_active_tape_io_fences().unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].tape_uuid, UUID);
        assert_eq!(fences[0].reason, "session_suffix_contradiction");
        owner.finish().unwrap();
        let selected = crate::pool_write::select_tape_in_pool_for_write_session(
            &fixture.index(),
            &fixture.config.tape_pools[0],
            1,
            &std::collections::HashSet::new(),
            &fixture.config.journal.dir.join("tapes"),
        );
        match selected {
            Err(crate::pool_write::SelectTapeError::NoWritableTapes { pool_id, reasons }) => {
                assert_eq!(pool_id, "default");
                assert_eq!(
                    reasons,
                    vec![crate::pool_write::WritabilityError::TapeIoFence {
                        quarantine_id: fences[0].quarantine_id.clone(),
                        reason: fences[0].reason.clone(),
                    }]
                );
            }
            other => {
                panic!("refused physical tail must remain excluded from pool selection: {other:?}")
            }
        }

        let state = fixture.reopen().unwrap();
        let runtime = state.library_runtime();
        let mut owner = fixture.owner_with_recovery_observer(state, true);
        // A real physical suffix remains after host rollback. The first typed
        // acknowledgement is durable, but the injected cut issues no write.
        crate::write_owner::cut_after_ack(runtime.manifest().library_uuid());
        fixture.commands.lock().unwrap().clear();
        let error = owner
            .supersede_tail_confirmed("HAND", |_, found| {
                assert!(!found.is_empty());
                Ok(())
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("injected cut after typed supersede"),
            "{error}"
        );
        assert!(!fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
        assert_eq!(fixture.world.lock().unwrap().tapes["HAND"].records, before);
        let found = owner
            .supersede_tail_confirmed("HAND", |_, found| {
                assert!(!found.is_empty());
                Ok(())
            })
            .unwrap();
        assert!(!found.is_empty());
        assert!(owner
            .catalog_index()
            .list_active_tape_io_fences()
            .unwrap()
            .is_empty());
        owner.open_session("default").unwrap();
        owner.close_session().unwrap();
        owner.finish().unwrap();
        let manifest = runtime.manifest().clone();
        drop(runtime);
        let view = remanence_state::library_log::native_view::NativeTapeView::probe(
            manifest.tape_root(),
            &fixture.config.index.sqlite_path,
        )
        .unwrap();
        let replay = remanence_state::library_log::store::replay(
            manifest.library_root(),
            manifest.library_uuid(),
            &view,
        )
        .unwrap();
        assert!(replay.refusal.is_none());
        assert_eq!(
            (1..=replay.last_sequence)
                .filter(|sequence| replay.fold.supersede(*sequence).is_some())
                .count(),
            1,
            "acknowledgment retry reuses the original source"
        );
    }
}

/// Ruling 19 reaches the physical missing-filemark producer, with a real
/// durable header-only dependency and no invented native record.
#[test]
fn fresh_missing_filemark_recovery_uses_typed_empty_prefix() {
    use remanence_state::library_log::{self as lib, native_view::NativeTapeView};
    for parity in [false, true] {
        let (fixture, state) = Fixture::new(parity);
        fixture
            .world
            .lock()
            .unwrap()
            .tapes
            .get_mut("HAND")
            .unwrap()
            .records
            .pop();
        let manifest = state.library_runtime().manifest().clone();
        let mut owner = fixture.owner_with_recovery_observer(state, true);
        let found = owner
            .supersede_tail_confirmed("HAND", |confirmation, found| {
                assert_eq!(confirmation, "SUPERSEDE HAND");
                assert!(found.is_empty());
                Ok(())
            })
            .unwrap();
        assert!(found.is_empty());
        owner.finish().unwrap();
        let view =
            NativeTapeView::probe(manifest.tape_root(), &fixture.config.index.sqlite_path).unwrap();
        let replay =
            lib::store::replay(manifest.library_root(), manifest.library_uuid(), &view).unwrap();
        assert!(replay.refusal.is_none());
        let supersede = (1..=replay.last_sequence)
            .find_map(|sequence| replay.fold.supersede(sequence))
            .expect("typed supersede acknowledgement");
        assert_eq!(supersede.at.sequence, 0);
        assert_eq!(supersede.session_id, None);
        assert_eq!(supersede.tail_file_count, 0);
        assert!(fixture.world.lock().unwrap().tapes["HAND"]
            .records
            .iter()
            .any(|r| matches!(r, Record::Filemark)));
    }
}

/// A real cut after the recovery delimiter must finish native publication on
/// reopen, with or without a suffix fence, and issue no second media write.
#[test]
fn recovery_effect_cut_completes_initialization_without_rewriting() {
    use remanence_state::library_log as lib;
    for parity in [false, true] {
        for suffix_fence in [false, true] {
            let (fixture, state) = Fixture::new(parity);
            fixture
                .world
                .lock()
                .unwrap()
                .tapes
                .get_mut("HAND")
                .unwrap()
                .records
                .pop();
            let runtime = state.library_runtime();
            if suffix_fence {
                let tape = runtime.tape_state(UUID).unwrap().tape;
                runtime
                    .append(|a, v| {
                        a.fence_raised(
                            lib::FenceRaised {
                                detail: lib::Bounded(vec![]),
                                kind: lib::FenceKind::Write,
                                scope: lib::FenceScope::Tape(tape),
                                cause: lib::FenceCause::SessionSuffixContradiction,
                                evidence: None,
                                at: None,
                                operation: None,
                            },
                            remanence_state::AuditActor::System,
                            remanence_state::SourceLayer::Layer4,
                            v,
                        )
                    })
                    .unwrap();
            }
            crate::write_owner::cut_after_recovery_effect(runtime.manifest().library_uuid());
            let mut owner = fixture.owner_with_recovery_observer(state, true);
            let error = owner
                .supersede_tail_confirmed("HAND", |_, _| Ok(()))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("injected cut after physical recovery"),
                "{error}"
            );
            owner.finish().unwrap();
            drop(runtime);
            let authority = remanence_state::TapeAuthority::open(
                fixture.config.journal.dir.join("tapes"),
                UUID,
                &fixture.config.index.sqlite_path,
            )
            .unwrap();
            assert_eq!(authority.freeze().unwrap().log_reference().0, 0);
            let records = fixture.world.lock().unwrap().tapes["HAND"].records.clone();
            assert_eq!(records.len(), 2);
            let state = fixture.reopen().unwrap();
            let runtime = state.library_runtime();
            let mut owner = fixture.owner(state);
            fixture.commands.lock().unwrap().clear();
            owner
                .supersede_tail_confirmed("HAND", |_, found| {
                    assert!(found.is_empty());
                    Ok(())
                })
                .unwrap();
            assert!(owner
                .catalog_index()
                .list_active_tape_io_fences()
                .unwrap()
                .is_empty());
            assert!(!fixture
                .commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
            assert_eq!(fixture.world.lock().unwrap().tapes["HAND"].records, records);
            owner.finish().unwrap();
            let snapshot = authority.freeze().unwrap();
            assert!(snapshot.tape_summary().last_media_commit.is_some());
            let report = authority.latest_commit().unwrap().unwrap();
            assert_eq!((report.cursor.partition, report.cursor.lba), (0, 2));
            drop(runtime);
            let state = fixture.reopen().unwrap();
            assert!(state
                .catalog()
                .list_active_tape_io_fences()
                .unwrap()
                .is_empty());
        }
    }
}

/// Failed recovery WRITE, delimiter, or post-effect position cannot publish a
/// native initialization. This uses actual model transport failure boundaries.
#[test]
fn recovery_initialization_waits_for_complete_physical_evidence() {
    for parity in [false, true] {
        for fault in [0x0a, 0x10, 0x34] {
            let (fixture, state) = Fixture::new(parity);
            fixture
                .world
                .lock()
                .unwrap()
                .tapes
                .get_mut("HAND")
                .unwrap()
                .records
                .pop();
            let mut owner = fixture.owner_with_recovery_fault(state, true, Some(fault));
            let error = owner
                .supersede_tail_confirmed("HAND", |_, _| Ok(()))
                .unwrap_err();
            assert!(!error.to_string().is_empty());
            owner.finish().unwrap();
            let authority = remanence_state::TapeAuthority::open(
                fixture.config.journal.dir.join("tapes"),
                UUID,
                &fixture.config.index.sqlite_path,
            )
            .unwrap();
            assert_eq!(
                authority.freeze().unwrap().log_reference().0,
                0,
                "fault {fault:x}"
            );
            assert!(authority.latest_commit().unwrap().is_none());
            assert!(fixture
                .commands
                .lock()
                .unwrap()
                .iter()
                .any(|c| c[0] == fault));
        }
    }
}

/// Matching UUID alone cannot authenticate a recovered initialization's geometry.
#[test]
fn recovery_effect_retry_refuses_changed_bootstrap_geometry() {
    let (fixture, state) = Fixture::new(false);
    fixture
        .world
        .lock()
        .unwrap()
        .tapes
        .get_mut("HAND")
        .unwrap()
        .records
        .pop();
    let runtime = state.library_runtime();
    let tape = runtime.tape_state(UUID).unwrap().tape;
    use remanence_state::library_log as lib;
    let fence = runtime
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    detail: lib::Bounded(vec![]),
                    kind: lib::FenceKind::Write,
                    scope: lib::FenceScope::Tape(tape),
                    cause: lib::FenceCause::SessionSuffixContradiction,
                    evidence: None,
                    at: None,
                    operation: None,
                },
                remanence_state::AuditActor::System,
                remanence_state::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    crate::write_owner::cut_after_recovery_effect(runtime.manifest().library_uuid());
    let mut owner = fixture.owner_with_recovery_observer(state, true);
    assert!(owner
        .supersede_tail_confirmed("HAND", |_, _| Ok(()))
        .unwrap_err()
        .to_string()
        .contains("injected cut"));
    owner.finish().unwrap();
    drop(runtime);
    {
        let mut world = fixture.world.lock().unwrap();
        let Record::Block(bytes) = &mut world.tapes.get_mut("HAND").unwrap().records[0] else {
            panic!("Bootstrap")
        };
        let mut payload = remanence_parity::bootstrap::parse_bootstrap_block(bytes).unwrap();
        payload.drive_compression = true;
        remanence_parity::bootstrap::write_bootstrap_block(&payload, bytes).unwrap();
    }
    let state = fixture.reopen().unwrap();
    let runtime = state.library_runtime();
    let mut owner = fixture.owner(state);
    fixture.commands.lock().unwrap().clear();
    let error = owner
        .supersede_tail_confirmed("HAND", |_, _| Ok(()))
        .unwrap_err();
    assert!(error.to_string().contains("geometry differs"), "{error}");
    assert!(runtime.active_fence(fence.sequence).is_some());
    assert!(!fixture
        .commands
        .lock()
        .unwrap()
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
    owner.finish().unwrap();
    let authority = remanence_state::TapeAuthority::open(
        fixture.config.journal.dir.join("tapes"),
        UUID,
        &fixture.config.index.sqlite_path,
    )
    .unwrap();
    assert_eq!(authority.freeze().unwrap().log_reference().0, 0);
}

/// The actual RPC preserves authenticated actor attribution, idempotent replies
/// and the suffix fence's physical-recovery restriction across source replay.
#[test]
fn typed_fence_release_rpc_preserves_actor_and_physical_recovery_policy() {
    use crate::pb::library_service_server::LibraryService;
    use remanence_state::library_log as lib;
    let (fixture, state) = Fixture::initialized(false);
    let runtime = state.library_runtime();
    let tape = runtime.tape_state(UUID).unwrap().tape;
    let raise = |cause| {
        runtime
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        detail: lib::Bounded(vec![]),
                        kind: lib::FenceKind::Write,
                        scope: lib::FenceScope::Tape(tape),
                        cause,
                        evidence: None,
                        at: None,
                        operation: None,
                    },
                    remanence_state::AuditActor::System,
                    remanence_state::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap()
    };
    let ordinary = raise(lib::FenceCause::TransferError);
    let suffix = raise(lib::FenceCause::SessionSuffixContradiction);
    let service = crate::library::LibraryServiceApi {
        state: crate::ApiState::new(state.projection_client()),
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for sequence in [ordinary.sequence, ordinary.sequence, suffix.sequence] {
        let mut request = tonic::Request::new(crate::pb::ReleaseTapeIoFenceRequest {
            force: false,
            quarantine_id: format!("library:{sequence}"),
            acknowledgment: "inspected original failure".into(),
        });
        request
            .metadata_mut()
            .insert("x-remanence-actor", "actual-operator".parse().unwrap());
        let result = executor.block_on(service.release_tape_io_fence(request));
        if sequence == suffix.sequence {
            assert!(result
                .unwrap_err()
                .message()
                .contains("physical tail recovery"));
        } else {
            assert_eq!(result.unwrap().into_inner().state, "released");
        }
    }
    assert!(runtime.active_fence(suffix.sequence).is_some());
    let manifest = runtime.manifest().clone();
    drop(service);
    drop(runtime);
    drop(state);
    let view = lib::native_view::NativeTapeView::probe(
        manifest.tape_root(),
        &fixture.config.index.sqlite_path,
    )
    .unwrap();
    let mut actors = Vec::new();
    lib::store::replay_observed(
        manifest.library_root(),
        manifest.library_uuid(),
        &view,
        &mut |record| {
            if record.record().tag() == lib::Tag::FenceReleased {
                actors.push(record.record().envelope.actor.clone());
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        actors,
        vec![remanence_state::AuditActor::Service(
            "actual-operator".into()
        )]
    );
    let state = fixture.reopen().unwrap();
    assert_eq!(
        state.catalog().list_active_tape_io_fences().unwrap().len(),
        1
    );
    assert!(fixture.commands.lock().unwrap().is_empty());
}

/// The closed fence schema cannot preserve the original coupled tape/endpoint
/// uncertainty and error text. Refuse without inventing a different source.
#[test]
fn standalone_coupled_fence_refuses_without_legacy_authority_append() {
    let (fixture, mut state) = Fixture::initialized(false);
    let source_bytes = || {
        let mut files: Vec<_> = std::fs::read_dir(&fixture.config.audit.dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "remlib"))
            .collect();
        files.sort();
        files
            .into_iter()
            .map(|path| std::fs::read(path).unwrap())
            .collect::<Vec<_>>()
    };
    let before = source_bytes();
    let paths = state.paths().clone();
    let library_uuid = state.library_runtime().manifest().library_uuid();
    let counted = crate::tests::refusal_tests::mark(&fixture.config.index.sqlite_path);
    let error = crate::mount_support::raise_standalone_fence(
        &mut state,
        UUID,
        Some(&remanence_library::DriveKey::Standalone {
            drive_serial: SERIAL.into(),
        }),
        "standalone_write_lost_by_restart",
        "unfinished durable write session requires recovery",
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot durably record standalone write uncertainty"));
    assert!(state.library_runtime().motion_fenced());
    // The library log only grew, by the one record that counts the refusal.
    let after = source_bytes();
    assert_eq!(after.len(), before.len());
    assert!(
        after
            .iter()
            .zip(&before)
            .all(|(now, was)| now.starts_with(was)),
        "the library log is append-only"
    );
    crate::tests::refusal_tests::assert_grew_by(&fixture.config.index.sqlite_path, &counted, 1);
    assert!(state
        .catalog()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    assert!(fixture.commands.lock().unwrap().is_empty());
    drop(state);
    let log = crate::tests::refusal_tests::read_log(&paths, library_uuid);
    assert_eq!(
        crate::tests::refusal_tests::refusals(&log),
        [crate::tests::refusal_tests::RecordedRefusal::expect(
            5,
            "StandaloneFenceUnrecorded",
            "tape",
            Some(UUID),
            remanence_state::AuditActor::local_user(),
        )]
    );
}

/// R13 operator recovery releases only the typed suffix fence after the real
/// physical proof; a repeated crash-after-effect proof performs no media write.
#[test]
fn suffix_fence_release_requires_explicit_physical_recovery_and_replays() {
    use remanence_state::library_log as lib;
    for parity in [false, true] {
        let (fixture, state) = Fixture::new(parity);
        let runtime = state.library_runtime();
        let tape = runtime.tape_state(UUID).unwrap().tape;
        let raise = runtime
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        detail: lib::Bounded(vec![]),
                        kind: lib::FenceKind::Write,
                        scope: lib::FenceScope::Tape(tape),
                        cause: lib::FenceCause::SessionSuffixContradiction,
                        evidence: None,
                        at: None,
                        operation: None,
                    },
                    remanence_state::AuditActor::System,
                    remanence_state::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        let mut owner = fixture.owner(state);
        fixture.commands.lock().unwrap().clear();
        let before = fixture.world.lock().unwrap().tapes["HAND"].records.clone();
        let error = owner
            .supersede_tail_confirmed("HAND", |_, _| Err("no acknowledgment".into()))
            .unwrap_err();
        assert!(error.to_string().contains("no acknowledgment"), "{error}");
        assert!(runtime.active_fence(raise.sequence).is_some());
        owner
            .supersede_tail_confirmed("HAND", |_, _| Ok(()))
            .unwrap();
        assert!(runtime.active_fence(raise.sequence).is_none());
        assert!(!fixture
            .commands
            .lock()
            .unwrap()
            .iter()
            .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
        assert_eq!(fixture.world.lock().unwrap().tapes["HAND"].records, before);
        owner.finish().unwrap();
        drop(runtime);
        for _ in 0..2 {
            let state = fixture.reopen().unwrap();
            assert!(state
                .catalog()
                .list_active_tape_io_fences()
                .unwrap()
                .is_empty());
        }
    }
}
