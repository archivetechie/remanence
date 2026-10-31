//! Virtual transport proofs for mounted ownership, durability and recovery.
use super::*;
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{transport::TransferOutcome, ScsiError, TimeoutClass};
use remanence_state::{AuditEvent, CatalogIndex, FileAuditLog};
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
}
impl Transport {
    fn observe(&mut self, cdb: &[u8]) -> std::result::Result<(), ScsiError> {
        self.commands.lock().unwrap().push(cdb.to_vec());
        // Check each write run and every delimiter. Replaying the entire audit
        // per padding block makes the fixed two-GiB terminal layout quadratic.
        let previous = self.last_opcode.replace(cdb[0]);
        if matches!(cdb[0], 0x0a | 0x10 | 0x19) && (previous != Some(0x0a) || cdb[0] != 0x0a) {
            if let Some(path) = &self.evidence {
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
                assert_eq!(last.event, AuditEvent::SessionOpened);
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
        self.observe(cdb)?;
        self.inner.execute_in(cdb, bytes)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        bytes: &[u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        self.observe(cdb)?;
        self.inner.execute_out(cdb, bytes)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> std::result::Result<(), ScsiError> {
        self.observe(cdb)?;
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
            .catalog_index()
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: UUID,
                voltag: "HAND".into(),
                block_size: BLOCK,
                parity: parity.clone(),
                force: false,
            })
            .unwrap();
        state
            .catalog_index()
            .set_tape_generation(
                UUID,
                remanence_state::MediaObservation {
                    generation: crate::LtoGen::Lto9,
                    source: remanence_state::GenerationSource::Operator,
                },
            )
            .unwrap();
        state
            .catalog_index()
            .project_tape_pool_membership(UUID, "default")
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
        MountedDrive::with_transport(
            state,
            Path::new(DEVICE),
            &StaticAllowlist::new([SERIAL]),
            Box::new(Transport {
                inner: ModelTransport::new(self.world.clone(), DeviceRole::Drive { bay: BAY }),
                commands: self.commands.clone(),
                fault: self.fault.clone(),
                evidence: Some(self.config.audit.dir.clone()),
                last_opcode: None,
            }),
            self.root.path(),
            Some(UUID),
        )
        .unwrap()
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
        CatalogIndex::open(&self.config.index.sqlite_path).unwrap()
    }
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
    assert_eq!(native.copies[0].representation, "encrypted");
    assert_eq!(
        native.copies[0].recipient_epoch_ids,
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
    let (fixture, mut state) = Fixture::new(false);
    state
        .catalog_index()
        .set_append_recovery_required(UUID, true)
        .unwrap();
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
        std::fs::create_dir_all(&fixture.config.journal.dir).unwrap();
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
            "recovery_required"
        );
        let preview = owner
            .supersede_tail(pb::SupersedeTailRequest {
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
    let records = FileAuditLog::replay(&config.audit.dir).unwrap();
    assert!(records.iter().any(|r| r.event == AuditEvent::SessionOpened));
    assert!(!records.iter().any(|r| r.event == AuditEvent::SessionClosed));
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
    assert_eq!(
        state.catalog_index().non_terminal_sessions().unwrap().len(),
        1
    );
    let mount = OperatorMount::with_transport(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(fixture.world.clone(), DeviceRole::Drive { bay: BAY }),
            commands: fixture.commands.clone(),
            fault: fixture.fault.clone(),
            evidence: None,
            last_opcode: None,
        }),
        &mut state,
        fixture.root.path(),
    )
    .unwrap();
    mount.finish().unwrap();
    let fences = state.catalog_index().list_active_tape_io_fences().unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(
        fences[0].endpoint,
        Some(remanence_library::DriveKey::Standalone {
            drive_serial: SERIAL.into()
        })
    );
    assert_eq!(fences[0].reason, "standalone_write_lost_by_restart");
    assert!(state
        .catalog_index()
        .non_terminal_sessions()
        .unwrap()
        .is_empty());
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
        fixture.config.journal.dir.join("checkpoints"),
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
    let (mut fixture, state) = Fixture::new(false);
    drop(state);
    fixture.config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
    fixture.config.tape_pools[0].watermark_low = 0.1;
    fixture.config.tape_pools[0].watermark_high = 0.25;
    let mut owner = fixture.owner(fixture.reopen().unwrap());
    owner.open_session("default").unwrap();
    let receipt = owner.append(fixture.request("seal-due")).unwrap();
    owner.close_session().unwrap();
    // Project the durable seal obligation at the recovery cut. The complete
    // object checkpoint remains the authority for the terminal suffix.
    let journal = remanence_state::authority::fixtures::AuthorityBuilder::new(
        fixture.config.journal.dir.join("checkpoints"),
        UUID,
    )
    .build()
    .unwrap();
    let mut checkpoint = journal.last().unwrap().unwrap();
    checkpoint.seal_due = Some(remanence_state::SealDueReason::ReachedLowWatermark);
    fixture
        .index()
        .project_checkpoint_record(&checkpoint)
        .unwrap();
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
    assert!(index
        .get_tape(&UUID)
        .unwrap()
        .unwrap()
        .terminal_finalization
        .is_some());
    let endpoint = remanence_library::DriveKey::Standalone {
        drive_serial: SERIAL.into(),
    };
    assert!(!index
        .tape_io_is_fenced(Some(&UUID), None, Some(&endpoint))
        .unwrap());
    let records = FileAuditLog::replay(&fixture.config.audit.dir).unwrap();
    assert!(!records
        .iter()
        .any(|r| r.event == AuditEvent::SessionOrphaned));
    let opened: Vec<_> = records
        .iter()
        .filter(|r| r.event == AuditEvent::SessionOpened)
        .collect();
    assert_eq!(opened.len(), 2);
    for open in opened {
        assert_eq!(
            records
                .iter()
                .filter(|r| r.event == AuditEvent::SessionClosed && r.session_id == open.session_id)
                .count(),
            1
        );
    }
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
            let before = FileAuditLog::replay(&fixture.config.audit.dir).unwrap();
            owner.finish().unwrap();
            let after = FileAuditLog::replay(&fixture.config.audit.dir).unwrap();
            let session_events = |records: &[remanence_state::AuditRecord]| {
                records
                    .iter()
                    .filter(|r| {
                        matches!(
                            r.event,
                            AuditEvent::SessionOpened
                                | AuditEvent::SessionClosed
                                | AuditEvent::SessionOrphaned
                        )
                    })
                    .map(|r| (r.session_id, r.event.clone()))
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
        if FileAuditLog::replay(&fixture.config.audit.dir)
            .unwrap()
            .iter()
            .any(|r| r.session_id == Some(session_id) && r.event == AuditEvent::SessionClosed)
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
        fixture.config.journal.dir.join("checkpoints"),
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
        FileAuditLog::replay(&fixture.config.audit.dir)
            .unwrap()
            .iter()
            .filter(|r| r.event == AuditEvent::SessionOpened)
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
    let error = owner
        .put_path_version_with_policy(fixture.request("committed-before-failure"), false)
        .unwrap_err();
    let crate::MountError::Committed {
        receipt,
        reason,
        failure,
    } = error
    else {
        panic!("missing durable receipt: {error}");
    };
    assert_eq!(reason, "new");
    assert_eq!(receipt.object.caller_object_id, "committed-before-failure");
    assert!(failure.to_string().contains("post-projection"), "{failure}");
    assert_eq!(owner.receipts().len(), 1);
    let _ = owner.finish();
    assert!(fixture.reopen().is_ok());
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
        .catalog_index()
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
        assert!(owner.finish().is_err());
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
