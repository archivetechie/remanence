//! Model coverage for invocation boundaries, durable crash cuts, and the shared
//! finalization/import funnels. Transport injection never substitutes a writer.
use super::*;
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{transport::TransferOutcome, ScsiError, TimeoutClass};
const UUID: TapeUuid = [
    0x73, 0x73, 0x73, 0x73, 0x73, 0x73, 0x43, 0x73, 0x83, 0x73, 0x73, 0x73, 0x73, 0x73, 0x73, 0x73,
];
const BLOCK: u32 = 262144;
const BAY: u16 = 0x100;
const DEVICE: &str = "/dev/sg-model-p4";
const SERIAL: &str = "P4-MODEL";
struct Transport {
    inner: ModelTransport,
    fault: Arc<Mutex<Option<u8>>>,
    audit_cut: Option<PathBuf>,
    hide_serial: bool,
    commands: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
    cut_after_prevent: bool,
    boundary: Option<(PathBuf, SharedVirtualWorld, bool, Arc<Mutex<bool>>)>,
}
impl Transport {
    fn fail(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if let Some(commands) = &self.commands {
            commands.lock().unwrap().push(cdb.to_vec());
        }
        if self
            .boundary
            .as_ref()
            .is_some_and(|(p, _, _, _)| std::fs::metadata(p).is_ok_and(|m| m.len() > 32))
        {
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
        .catalog_index()
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: UUID,
            voltag: "HAND".into(),
            block_size: BLOCK,
            parity,
            force: false,
        })
        .unwrap();
    state
        .catalog_index()
        .set_tape_generation(
            UUID,
            remanence_state::MediaObservation {
                generation: LtoGen::Lto9,
                source: GenerationSource::Operator,
            },
        )
        .unwrap();
    state
        .catalog_index()
        .project_tape_pool_membership(UUID, "default")
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
        state
            .catalog_index()
            .get_tape(&UUID)
            .unwrap()
            .unwrap()
            .state,
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
    let mut mounted = mount.mount(target()).unwrap();
    world.lock().unwrap().drive_bays.insert(BAY, None);
    assert!(mount
        .write_checkpointed(&mut mounted, request(root.path(), "removed"))
        .is_err());
    assert_eq!(
        world.lock().unwrap().tapes["HAND"].written_bytes,
        u64::from(BLOCK)
    );
    mount.release(mounted);
    assert!(
        state
            .catalog_index()
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
fn crash_after_session_open_refuses_restart_before_media_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let lock = root.path().join("locks");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mounted = mount.mount(target()).unwrap();
    mount
        .session(&mounted, uuid::Uuid::new_v4(), AuditEvent::SessionOpened)
        .unwrap();
    drop(mounted);
    drop(mount);
    let paths = state.paths().clone();
    let config = state.config().clone();
    drop(state);
    let mut state = StateHandle::open_with_config(paths, config).unwrap();
    let result = backend(&mut state, &world, &lock, fault());
    assert!(matches!(result, Err(MountError::Refused(ref message)) if message.contains("fence")));
    assert_eq!(
        world.lock().unwrap().tapes["HAND"].written_bytes,
        u64::from(BLOCK)
    );
}
#[test]
fn uncertain_write_cut_is_durably_fenced_even_after_an_earlier_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let lock = root.path().join("locks");
    let fault = fault();
    let mut mount = backend(&mut state, &world, &lock, Arc::clone(&fault)).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "first"))
        .unwrap();
    *fault.lock().unwrap() = Some(0x0a);
    assert!(mount
        .write_checkpointed(&mut mounted, request(root.path(), "second"))
        .is_err());
    assert!(fault.lock().unwrap().is_none(), "the write cut was reached");
    mount.release(mounted);
    assert!(!state
        .catalog_index()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    let records = remanence_state::FileAuditLog::replay(&state.paths().audit_dir).unwrap();
    let mut rebuilt = CatalogIndex::open(root.path().join("rebuilt.sqlite")).unwrap();
    rebuilt.replay_audit_records(&records).unwrap();
    let fences = rebuilt.list_active_tape_io_fences().unwrap();
    assert!(
        fences.iter().any(|f| f.endpoint
            == Some(remanence_library::DriveKey::Standalone {
                drive_serial: SERIAL.into(),
            })),
        "audit replay must preserve the standalone fence identity"
    );
    assert!(backend(&mut state, &world, &lock, self::fault()).is_err());
}
#[test]
fn checkpoint_finalize_and_terminal_restore_share_the_production_funnels() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state_with_audit(root.path(), false);
    let world = world();
    let lock = root.path().join("locks");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "first"))
        .unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "second"))
        .unwrap();
    let id = uuid::Uuid::new_v4();
    let finalize = || StandaloneFinalizeRequest {
        tape_uuid: UUID,
        acknowledge_tape: UUID,
        expected_pool_id: Some("default".into()),
        reason: "operator finished".into(),
        idempotency_key: id,
    };
    mount.finalize(&mut mounted, finalize()).unwrap();
    mount.finalize(&mut mounted, finalize()).unwrap();
    let before_replay = mounted.drive.media_write_dispatch_count();
    assert!(mount
        .write_checkpointed(&mut mounted, request(root.path(), "first"))
        .unwrap()
        .is_replay());
    assert_eq!(mounted.drive.media_write_dispatch_count(), before_replay);
    let output = root.path().join("restored");
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
#[test]
fn imported_checkpoint_authorizes_append_and_conflicting_import_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(&root.path().join("original"));
    let world = world();
    let lock = root.path().join("locks");
    let from = state.paths().journal_dir.clone();
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "first"))
        .unwrap();
    mount.release(mounted);
    drop(state);
    let mut state = adopted_state(&root.path().join("new-host"), crate::ParityConfig::None);
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount.import_journals(&mut mounted, &from).unwrap();
    mount.import_journals(&mut mounted, &from).unwrap();
    mount.release(mounted);
    let paths = state.paths().clone();
    let config = state.config().clone();
    drop(state);
    let mut state = StateHandle::open_with_config(paths, config).unwrap();
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "second"))
        .unwrap();
    assert!(mount.import_journals(&mut mounted, &from).is_err());
    let output = root.path().join("restored");
    let report = crate::scan_and_restore(&mut mounted.drive, &output, None).unwrap();
    assert_eq!(
        report["candidates"].as_array().unwrap().len(),
        2,
        "{report}"
    );
    assert_eq!(
        std::fs::read(output.join("candidate-2/second")).unwrap(),
        b"second"
    );
    mount.release(mounted);
}

#[test]
fn encrypted_scan_uses_the_same_contained_restore_sink() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
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
    mount.write_checkpointed(&mut mounted, input).unwrap();
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
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let count = world.lock().unwrap().command_log.len();
    assert!(
        crate::position_to_object(&mut mounted.drive, crate::BotRelativeTapeFile(u64::MAX))
            .is_err()
    );
    assert_eq!(world.lock().unwrap().command_log.len(), count);
    assert!(!mounted.reserved);
    mount.reserve(&mut mounted).unwrap();
    assert!(mounted.reserved);
    mount.release(mounted);
    let records = remanence_state::FileAuditLog::replay(&state.paths().audit_dir).unwrap();
    assert!(records.iter().any(|record| record
        .detail
        .values()
        .any(|value| value == &ciborium::Value::Text("reserve_drive".into()))));
    assert!(records.iter().any(|record| record
        .detail
        .values()
        .any(|value| value == &ciborium::Value::Text("release_drive".into()))));
}

#[test]
fn parity_import_rejects_a_missing_partner_and_validates_the_complete_pair() {
    let root = tempfile::tempdir().unwrap();
    let parity = crate::ParityConfig::Scheme(remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_static("p4-import"),
        data_blocks_per_stripe: 2,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    });
    let mut state = state_with_parity(&root.path().join("original"), true, parity.clone());
    let world = world();
    let bootstrap =
        crate::build_tape_bootstrap(UUID, BLOCK, parity.clone(), "2026-10-15T00:00:00Z", "P4");
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    world.lock().unwrap().tapes.get_mut("HAND").unwrap().records[0] = Record::Block(block);
    let lock = root.path().join("locks");
    let from = state.paths().journal_dir.clone();
    let partner = state.journal_path(UUID);
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "first"))
        .unwrap();
    mount.release(mounted);
    drop(state);
    let mut state = adopted_state(&root.path().join("new-host"), parity);
    let destination = state.paths().journal_dir.join("checkpoints");
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    let hidden = partner.with_extension("saved");
    std::fs::rename(&partner, &hidden).unwrap();
    assert!(mount.import_journals(&mut mounted, &from).is_err());
    assert!(!destination
        .join(format!("{}.remcheckpoint", uuid::Uuid::from_bytes(UUID)))
        .exists());
    std::fs::rename(hidden, &partner).unwrap();
    mount.import_journals(&mut mounted, &from).unwrap();
    mount.release(mounted);
    let paths = state.paths().clone();
    let config = state.config().clone();
    drop(state);
    let mut state = StateHandle::open_with_config(paths, config).unwrap();
    let mut mount = backend(&mut state, &world, &lock, fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "second"))
        .unwrap();
    mount.release(mounted);
}

#[test]
fn combined_transport_and_audit_failure_latches_and_persists_quarantine() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let world = world();
    let log = remanence_state::FileAuditLog::open(&state.paths().audit_dir, true).unwrap();
    let audit_path = log.segment_path().to_owned();
    drop(log);
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
    let mut mounted = backend.mount(target()).unwrap();
    *fault.lock().unwrap() = Some(0x0a);
    assert!(backend
        .write_checkpointed(&mut mounted, request(root.path(), "cut"))
        .is_err());
    assert!(backend.unusable);
    assert!(!backend
        .state
        .catalog_index()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
    std::fs::remove_dir(&audit_path).unwrap();
    std::fs::rename(audit_path.with_extension("saved"), &audit_path).unwrap();
    let count = mounted.drive.media_write_dispatch_count();
    for _ in 0..2 {
        let error = backend
            .write_checkpointed(&mut mounted, request(root.path(), "retry"))
            .unwrap_err();
        assert!(error.to_string().contains("unusable"), "{error}");
    }
    assert_eq!(mounted.drive.media_write_dispatch_count(), count);
    backend.release(mounted);
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
        let checkpoint = remanence_state::FileCheckpointJournal::open(
            state.paths().journal_dir.join("checkpoints"),
            UUID,
        )
        .unwrap();
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
                    checkpoint.path().into(),
                    Arc::clone(&world),
                    ua,
                    Arc::clone(&seen),
                )),
            }),
            &mut state,
            &root.path().join("locks"),
        )
        .unwrap();
        let mut mounted = backend.mount(target()).unwrap();
        let error = backend
            .write_checkpointed(&mut mounted, request(root.path(), "boundary"))
            .unwrap_err();
        assert!(*seen.lock().unwrap(), "boundary not exercised: {error}");
        assert!(checkpoint
            .acquire_exclusive()
            .unwrap()
            .last_record_bounded()
            .unwrap()
            .is_some());
        assert!(checkpoint.terminal_finalization_intent().unwrap().is_none());
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
        backend.release(mounted);
    }
}

#[test]
fn barcode_mount_preserves_the_legacy_read_command_stream_without_generation() {
    let root = tempfile::tempdir().unwrap();
    let mut state = configured_state(root.path(), true);
    state
        .catalog_index()
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
                crate::load_tape_by_uuid(state.catalog_index(), &mut library, &policy, &UUID)
                    .unwrap();
            drive.rewind().unwrap();
            crate::verify_tape_identity(&mut DriveHandleSource(&mut drive), &UUID).unwrap();
        } else {
            let mut backend = ChangerMount {
                library: &mut library,
                policy: &policy,
                index: state.catalog_index(),
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
    backend
        .session(&mounted, uuid::Uuid::new_v4(), AuditEvent::SessionOpened)
        .unwrap();
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
    state.refresh_audit_append_cursor().unwrap();
    state
        .audit()
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
#[test]
fn restart_recovery_is_shared_and_scoped_to_the_owned_endpoint() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(root.path());
    let library_tape = [0x44; 16];
    let library_session = seed_open_session(
        &mut state,
        remanence_library::DriveKey::new("LIB", 0x100),
        library_tape,
    );
    let standalone_session = seed_open_session(
        &mut state,
        remanence_library::DriveKey::Standalone {
            drive_serial: SERIAL.into(),
        },
        UUID,
    );
    let before = state.calibration_control().row(library_tape).write_epoch;
    assert!(backend(&mut state, &world(), &root.path().join("locks"), fault()).is_err());
    let open = state.catalog_index().non_terminal_sessions().unwrap();
    assert!(open.iter().any(|s| s.session_id == library_session));
    assert!(!open.iter().any(|s| s.session_id == standalone_session));
    assert_eq!(
        state.calibration_control().row(library_tape).write_epoch,
        before
    );
    assert!(state
        .catalog_index()
        .list_active_tape_io_fences()
        .unwrap()
        .iter()
        .all(|f| f.tape_uuid == UUID));
    // The daemon uses exactly this boundary with global state ownership.
    assert_eq!(
        state
            .recover_sessions_after_restart(None, |_, _| Ok(()))
            .unwrap(),
        1
    );
    assert!(state
        .catalog_index()
        .non_terminal_sessions()
        .unwrap()
        .is_empty());
    assert!(state.calibration_control().row(library_tape).write_epoch > before);
}

#[test]
fn imported_authority_preserves_quarantine_and_terminal_restrictions() {
    let root = tempfile::tempdir().unwrap();
    let mut state = state(&root.path().join("source"));
    let world = world();
    let from = state.paths().journal_dir.clone();
    let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
    let mut mounted = mount.mount(target()).unwrap();
    mount
        .write_checkpointed(&mut mounted, request(root.path(), "first"))
        .unwrap();
    mount.release(mounted);
    drop(state);
    for restricted in ["fence", "sealed"] {
        let mut state = adopted_state(&root.path().join(restricted), crate::ParityConfig::None);
        let mut mount = backend(&mut state, &world, &root.path().join("locks"), fault()).unwrap();
        let mut mounted = mount.mount(target()).unwrap();
        if restricted == "fence" {
            mount
                .state
                .catalog_index()
                .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                    tape_uuid: UUID,
                    barcode: Some("HAND".into()),
                    reason: "unrelated".into(),
                    evidence_json: None,
                })
                .unwrap();
        } else {
            mount.state.catalog_index().seal_tape(UUID).unwrap();
        }
        assert!(mount.import_journals(&mut mounted, &from).is_err());
        let tape = mount
            .state
            .catalog_index()
            .get_tape(&UUID)
            .unwrap()
            .unwrap();
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
        let mut state = StateHandle::open_with_config(paths, config).unwrap();
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
        let mut drive =
            crate::load_tape_by_uuid(state.catalog_index(), &mut library, &policy, &UUID).unwrap();
        let result = crate::write_to_selected_drive_checkpointed(
            &mut state,
            &mut drive,
            request(root.path(), "seal"),
            crate::pool_write::SelectedTape {
                pool_id: "default".into(),
                tape_uuid: UUID,
                block_size: BLOCK,
                parity_config: crate::ParityConfig::None,
            },
        );
        if cut {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().sealed_after_write());
        }
        assert_eq!(
            commands
                .lock()
                .unwrap()
                .iter()
                .filter(|c| c[0] == 0x1e)
                .map(|c| c[4])
                .collect::<Vec<_>>(),
            [1]
        );
        drop(drive);
        let commands = commands.lock().unwrap();
        assert_eq!(
            commands
                .iter()
                .filter(|c| c[0] == 0x1e)
                .map(|c| c[4])
                .collect::<Vec<_>>(),
            [1, 0]
        );
        assert_eq!(commands.last().unwrap().as_slice(), &[0x1e, 0, 0, 0, 0, 0]);
    }
}

#[test]
fn authority_hashing_keeps_no_follow_open_policy() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let link = root.path().join("link");
    std::fs::write(&a, b"authority").unwrap();
    std::fs::write(&b, b"authority").unwrap();
    assert!(same_authority(&a, &b).unwrap());
    std::os::unix::fs::symlink(&a, &link).unwrap();
    assert!(same_authority(&a, &link).is_err());
    std::fs::write(&b, b"changed").unwrap();
    assert!(!same_authority(&a, &b).unwrap());
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
